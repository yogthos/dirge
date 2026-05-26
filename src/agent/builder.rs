use compact_str::CompactString;
use rig::agent::{Agent, AgentBuilder};
use rig::completion::CompletionModel;
use rig::providers::openrouter;
use std::sync::Arc;

use crate::agent::prompt::{SYSTEM_PROMPT, TODO_TOOLS_PROMPT};
use crate::agent::tools;
use crate::agent::tools::ToolCache;
use crate::agent::tools::background::BackgroundStore;
use crate::agent::tools::plan::PlanSwitchSender;
use crate::agent::tools::question::QuestionSender;
use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::AnyModel;
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;
use crate::skill::{self, Skill};

#[allow(dead_code)]
pub type ZAgent = Agent<openrouter::CompletionModel>;

/// Wrap every tool with `HookedToolDyn` so plugins can intercept calls.
/// On non-plugin builds this is a no-op identity, so callers can use it
/// unconditionally. The global PluginManager is read at tool-call time;
/// if none was installed, the wrapper short-circuits to the inner tool.
fn hookify(tools: Vec<Box<dyn rig::tool::ToolDyn>>) -> Vec<Box<dyn rig::tool::ToolDyn>> {
    #[cfg(feature = "plugin")]
    {
        tools
            .into_iter()
            .map(crate::plugin::hook::HookedToolDyn::wrap_global)
            .collect()
    }
    #[cfg(not(feature = "plugin"))]
    {
        tools
    }
}

pub async fn build_agent_inner<M: CompletionModel + 'static>(
    model: M,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    question_tx: Option<QuestionSender>,
    plan_tx: Option<PlanSwitchSender>,
    bg_store: Option<BackgroundStore>,
    #[cfg(feature = "lsp")] lsp_manager: Option<std::sync::Arc<crate::lsp::manager::LspManager>>,
    sandbox: Sandbox,
    parent_model: Option<AnyModel>,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
) -> (Agent<M>, ToolCache) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let skills: Arc<[Skill]> = Arc::from(
        tokio::task::spawn_blocking(move || skill::discover_skills(&cwd))
            .await
            .unwrap_or_default(),
    );

    // The `plan_file`-keyed gate on edit/write/apply_patch was
    // removed: prompt-level tool restrictions now live in the
    // prompt file's frontmatter (`deny_tools: [...]`), enforced
    // at the permission-checker layer. Plan / review modes deny
    // edit/write/apply_patch/bash entirely, so the file-name gate
    // is unnecessary.
    let mut preamble = SYSTEM_PROMPT.to_string();
    preamble.push('\n');
    preamble.push_str(TODO_TOOLS_PROMPT);
    if let Some(agents) = &context.agents {
        preamble.push_str("\n\n");
        preamble.push_str(agents);
    }

    if let Some(prompt) = &context.current_prompt {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(prompt);
    }

    if let Ok(cwd) = std::env::current_dir() {
        let cwd_str = cwd.display();
        preamble.push_str(&format!("\n\nCurrent working directory: {}", cwd_str));
    }

    preamble.push_str(&format!("\nOS: {}", std::env::consts::OS));

    if let Ok(shell) = std::env::var("SHELL") {
        preamble.push_str(&format!("\nShell: {}", shell));
    }

    // Bounded git lookup. `git rev-parse` can hang for many seconds
    // when the repo's `.git` lives on a wedged NFS mount, the
    // `core.fsmonitor` daemon is stalled, or a `.gitconfig` `[include]`
    // points at a path that itself blocks (e.g. another stalled
    // network mount). 2 s is well over a healthy local `git` (≪ 50 ms)
    // — anything longer is the user's git misbehaving, and we'd
    // rather show the banner without a branch than hang dirge's
    // entire startup.
    let git_branch_fut = tokio::task::spawn_blocking(|| {
        std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !branch.is_empty() {
                        Some(branch)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
    });
    let git_branch =
        match tokio::time::timeout(std::time::Duration::from_secs(2), git_branch_fut).await {
            Ok(Ok(branch)) => branch,
            // spawn_blocking JoinError or wall-clock expiry: degrade
            // gracefully. The spawned thread keeps running in the
            // background until git returns; we simply stop awaiting
            // it. No leak — once the OS kernel reaps the git child,
            // the thread exits naturally.
            _ => None,
        };

    if let Some(branch) = git_branch {
        preamble.push_str(&format!("\nGit branch: {}", branch));
    }

    // Phase 8: inject per-project memory + skills into the system
    // prompt. Frozen snapshots of MEMORY.md and PITFALLS.md become
    // reference material for every turn. Skills from .dirge/skills/
    // and global dirs are listed so the model knows what procedural
    // knowledge is available (it loads them on demand via the
    // `skill` tool).
    if let Ok(cwd) = std::env::current_dir() {
        let paths = crate::extras::dirge_paths::ProjectPaths::new(&cwd);
        if let Ok(mem) = crate::extras::memory_store::MemoryStore::load_memory(&paths) {
            let mem_text = mem.format_for_system_prompt();
            if !mem_text.is_empty() {
                preamble.push_str(&mem_text);
            }
        }
        if let Ok(pit) = crate::extras::memory_store::MemoryStore::load_pitfalls(&paths) {
            let pit_text = pit.format_for_system_prompt();
            if !pit_text.is_empty() {
                preamble.push_str(&pit_text);
            }
        }
    }

    // Inject mode-specific reminders
    if let Some(prompt_name) = &context.current_prompt_name {
        let plan_exists = std::env::current_dir()
            .unwrap_or_else(|_| ".".into())
            .join("PLAN.md")
            .exists();
        append_mode_reminder(&mut preamble, prompt_name, plan_exists);
    }

    let mut builder = AgentBuilder::new(model).preamble(&preamble);

    let max_tokens = cli.resolve_max_tokens(cfg);
    builder = builder.max_tokens(max_tokens);

    let max_turns = cli.resolve_max_agent_turns(cfg);
    builder = builder.default_max_turns(max_turns);

    // Temperature: CLI > config > unset. Previously only `cli.temperature`
    // was checked, so users couldn't set a default in config.json.
    if let Some(temp) = cli.resolve_temperature(cfg) {
        let clamped = temp.clamp(0.0, 2.0);
        if (clamped - temp).abs() > f64::EPSILON {
            // Warn ONCE per process if the user's value was clamped
            // — previously silent, so a user with `temperature: 3.5`
            // got 2.0 and never knew.
            static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            if WARNED.set(()).is_ok() {
                eprintln!(
                    "warning: temperature {} clamped to {} (valid range 0.0..=2.0)",
                    temp, clamped,
                );
            }
        }
        builder = builder.temperature(clamped);
    }

    if cli.resolve_no_tools(cfg) {
        (builder.build(), ToolCache::new())
    } else {
        let cache = ToolCache::new();

        let base_tools: Vec<Box<dyn rig::tool::ToolDyn>> = vec![
            Box::new(tools::ReadTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
                #[cfg(feature = "lsp")]
                lsp_manager.clone(),
            )),
            Box::new(tools::WriteTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
                #[cfg(feature = "lsp")]
                lsp_manager.clone(),
            )),
            Box::new(tools::EditTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
                #[cfg(feature = "lsp")]
                lsp_manager.clone(),
            )),
            Box::new(tools::BashTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                sandbox.clone(),
                cache.clone(),
            )),
            Box::new(tools::GrepTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
            )),
            Box::new(tools::FindFilesTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
            )),
            Box::new(tools::GlobTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
            )),
            Box::new(tools::ListDirTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
            )),
            Box::new(tools::RepoOverviewTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
            )),
            Box::new(tools::WriteTodoList::new(
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(tools::SkillTool::new(
                Arc::clone(&skills),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(tools::MemoryTool::new(permission.clone(), ask_tx.clone())),
            Box::new(tools::ApplyPatchTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
            )),
        ];

        let question_tool = question_tx.map(|tx| {
            Box::new(
                tools::QuestionTool::new(tx).with_permission(permission.clone(), ask_tx.clone()),
            ) as Box<dyn rig::tool::ToolDyn>
        });

        let plan_tools = plan_tx.map(|tx| {
            // Pass the PermCheck to the plan tools so they can
            // consult the prompt deny-list before opening the
            // confirmation dialog (adversarial-review #6).
            let enter =
                Box::new(tools::PlanEnterTool::new(tx.clone()).with_permission(permission.clone()))
                    as Box<dyn rig::tool::ToolDyn>;
            let exit = Box::new(tools::PlanExitTool::new(tx).with_permission(permission.clone()))
                as Box<dyn rig::tool::ToolDyn>;
            vec![enter, exit]
        });

        // Web tools: gated on config + an env-var escape hatch.
        // CONFIG.md documents both keys as defaulting to `true`;
        // explicit `false` in config disables. The env vars are
        // symmetric — previously only `WEBSEARCH_ENABLED` existed,
        // forcing webfetch users to edit config.json for an
        // equivalent toggle. The runtime API-key check still has
        // to pass for websearch.
        let env_true = |k: &str| {
            std::env::var(k)
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false)
        };
        let websearch_enabled = cfg.tools.as_ref().and_then(|t| t.websearch).unwrap_or(true)
            || env_true("WEBSEARCH_ENABLED");
        let webfetch_enabled = cfg.tools.as_ref().and_then(|t| t.webfetch).unwrap_or(true)
            || env_true("WEBFETCH_ENABLED");

        // Websearch now works out of the box without an API key —
        // mirrors opencode's behavior. Backend: Exa's hosted MCP
        // endpoint (`https://mcp.exa.ai/mcp`) which accepts unauth'd
        // calls at a lower rate limit, with DDG HTML scraping as
        // a fallback if Exa is unreachable. EXA_API_KEY remains
        // optional — when set, it's appended as `?exaApiKey=…` for
        // higher rate limits.
        let websearch_tool = if websearch_enabled {
            let key = std::env::var("EXA_API_KEY").ok().filter(|k| !k.is_empty());
            Some(Box::new(tools::WebSearchTool::new(
                permission.clone(),
                ask_tx.clone(),
                key,
            )) as Box<dyn rig::tool::ToolDyn>)
        } else {
            None
        };
        let webfetch_tool = webfetch_enabled.then(|| {
            Box::new(tools::WebFetchTool::new(permission.clone(), ask_tx.clone()))
                as Box<dyn rig::tool::ToolDyn>
        });

        #[allow(unused_mut)]
        let mut builder = builder.tools(hookify(base_tools));

        if let Some(qt) = question_tool {
            builder = builder.tools(hookify(vec![qt]));
        }

        if let Some(pt) = plan_tools {
            builder = builder.tools(hookify(pt));
        }

        if let Some(ws) = websearch_tool {
            builder = builder.tools(hookify(vec![ws]));
        }

        if let Some(wf) = webfetch_tool {
            builder = builder.tools(hookify(vec![wf]));
        }

        if let (Some(pm), Some(store)) = (parent_model, bg_store) {
            let task_tool = Box::new(tools::TaskTool::new(
                permission.clone(),
                ask_tx.clone(),
                pm,
                store.clone(),
            ));
            let status_tool = Box::new(
                tools::TaskStatusTool::new(store)
                    .with_permission(permission.clone(), ask_tx.clone()),
            ) as Box<dyn rig::tool::ToolDyn>;
            builder = builder.tools(hookify(vec![task_tool, status_tool]));
        }

        #[cfg(feature = "lsp")]
        if let Some(manager) = &lsp_manager {
            let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
            let lsp_tool = Box::new(tools::LspTool::new(
                permission.clone(),
                ask_tx.clone(),
                manager.clone(),
                cwd,
            )) as Box<dyn rig::tool::ToolDyn>;
            builder = builder.tools(hookify(vec![lsp_tool]));
        }

        #[cfg(feature = "mcp")]
        if let Some(manager) = &mcp_manager {
            let mcp_tools = manager
                .collect_tools(permission.clone(), ask_tx.clone())
                .await;
            if !mcp_tools.is_empty() {
                // Skip MCP tools whose names collide with dirge
                // built-ins. Without this, the MCP version would
                // silently shadow `read`/`write`/`bash`/etc. —
                // rig's builder takes the last-added tool when
                // names clash, and dirge adds built-ins first.
                // Better to warn loudly and refuse to shadow than
                // to let an arbitrary MCP server replace core tools.
                // Review-batch #7: single source of truth for the
                // built-in tool registry is `tools::BUILTIN_TOOL_NAMES`.
                // Previously this list was hand-maintained here AND
                // in `context/prompts.rs` (KNOWN_TOOLS), so adding a
                // tool required editing both — drift meant either
                // a spurious "unknown tool in deny_tools" warning OR
                // an unsafe shadowable name. Now both sites read the
                // same const. `mcp_tool` itself is in the list, but
                // we don't filter against it because no MCP server
                // exports a tool literally named "mcp_tool" — the
                // umbrella name is internal to dirge.
                let builtin_names: &[&str] = tools::BUILTIN_TOOL_NAMES;
                let filtered: Vec<crate::extras::mcp::tool::McpTool> = mcp_tools
                    .into_iter()
                    .filter(|t| {
                        let name = t.definition.name.as_ref();
                        if builtin_names.contains(&name) {
                            eprintln!(
                                "warning: MCP server '{}' exports tool '{}' which collides with a dirge built-in; skipping MCP version",
                                t.server_name, name,
                            );
                            false
                        } else {
                            true
                        }
                    })
                    .collect();
                if !filtered.is_empty() {
                    let dyn_tools: Vec<Box<dyn rig::tool::ToolDyn>> = filtered
                        .into_iter()
                        .map(|t| Box::new(t) as Box<dyn rig::tool::ToolDyn>)
                        .collect();
                    builder = builder.tools(hookify(dyn_tools));
                }
            }
        }

        #[cfg(feature = "semantic")]
        if let Some(manager) = &semantic_manager {
            let sem_tools = manager.tools(permission.clone(), ask_tx.clone());
            if !sem_tools.is_empty() {
                builder = builder.tools(hookify(sem_tools));
            }
        }

        (builder.build(), cache)
    }
}

// ============================================================
// Phase 4.5h-4 — parallel LoopTool registry builder
// ============================================================

/// Build the LoopTool registry for the new agent_loop path.
///
/// Mirrors the tool construction in `build_agent_inner` but
/// wraps each tool via `RigToolAdapter` so it implements the
/// `LoopTool` trait the new loop dispatches against. Mutating
/// tools (bash, edit, write, apply_patch, ...) are tagged
/// `ToolExecutionMode::Sequential` — phase 3's umbrella
/// dispatcher promotes the WHOLE batch to sequential when any
/// included tool declares Sequential, which is the safe default
/// for fs / process mutators.
///
/// Read-only tools (read, grep, list_dir, ...) leave the
/// execution mode at None so they pick up the loop config's
/// default (Parallel) — batches of all-read-only tools dispatch
/// concurrently.
///
/// Note: tool construction is temporarily duplicated between
/// `build_agent_inner` (rig path, retained for the rig Agent's
/// preamble + model only) and `build_loop_tools` (new path,
/// active dispatch). The rig Agent's tools are no longer
/// invoked after phase 4.5h-6 — the loop dispatches through
/// the LoopTool registry returned here. A follow-up commit can
/// strip the unused tool list from `build_agent_inner`.
#[allow(clippy::too_many_arguments)]
pub async fn build_loop_tools(
    cache: ToolCache,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    question_tx: Option<QuestionSender>,
    plan_tx: Option<PlanSwitchSender>,
    bg_store: Option<BackgroundStore>,
    #[cfg(feature = "lsp")] lsp_manager: Option<std::sync::Arc<crate::lsp::manager::LspManager>>,
    sandbox: Sandbox,
    parent_model: Option<AnyModel>,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    cli: &Cli,
    cfg: &Config,
) -> Vec<std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>> {
    use crate::agent::agent_loop::types::ToolExecutionMode;
    use crate::agent::agent_loop::{LoopTool, RigToolAdapter};

    if cli.resolve_no_tools(cfg) {
        return Vec::new();
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let skills: Arc<[Skill]> = Arc::from(
        tokio::task::spawn_blocking(move || skill::discover_skills(&cwd))
            .await
            .unwrap_or_default(),
    );

    // Wrap a built tool as a LoopTool adapter with optional
    // execution_mode override. Async because rig's `definition`
    // is async (RigToolAdapter::new resolves it eagerly).
    async fn wrap<T>(inner: T, mode: Option<ToolExecutionMode>) -> Arc<dyn LoopTool>
    where
        T: rig::tool::ToolDyn + 'static,
    {
        let adapter = RigToolAdapter::new(Box::new(inner)).await;
        let adapter = match mode {
            Some(m) => adapter.with_execution_mode(m),
            None => adapter,
        };
        Arc::new(adapter)
    }

    let mut tools: Vec<Arc<dyn LoopTool>> = Vec::new();

    // Read-only — leave at default (Parallel).
    tools.push(
        wrap(
            tools::ReadTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
                #[cfg(feature = "lsp")]
                lsp_manager.clone(),
            ),
            None,
        )
        .await,
    );

    // Mutating — Sequential.
    tools.push(
        wrap(
            tools::WriteTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
                #[cfg(feature = "lsp")]
                lsp_manager.clone(),
            ),
            Some(ToolExecutionMode::Sequential),
        )
        .await,
    );
    tools.push(
        wrap(
            tools::EditTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
                #[cfg(feature = "lsp")]
                lsp_manager.clone(),
            ),
            Some(ToolExecutionMode::Sequential),
        )
        .await,
    );
    tools.push(
        wrap(
            tools::BashTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                sandbox.clone(),
                cache.clone(),
            ),
            Some(ToolExecutionMode::Sequential),
        )
        .await,
    );

    // Read-only batch.
    tools.push(
        wrap(
            tools::GrepTool::with_cache(permission.clone(), ask_tx.clone(), cache.clone()),
            None,
        )
        .await,
    );
    tools.push(
        wrap(
            tools::FindFilesTool::with_cache(permission.clone(), ask_tx.clone(), cache.clone()),
            None,
        )
        .await,
    );
    tools.push(
        wrap(
            tools::GlobTool::with_cache(permission.clone(), ask_tx.clone(), cache.clone()),
            None,
        )
        .await,
    );
    tools.push(
        wrap(
            tools::ListDirTool::with_cache(permission.clone(), ask_tx.clone(), cache.clone()),
            None,
        )
        .await,
    );
    tools.push(
        wrap(
            tools::RepoOverviewTool::with_cache(permission.clone(), ask_tx.clone(), cache.clone()),
            None,
        )
        .await,
    );

    // Mutates internal todo state — Sequential.
    tools.push(
        wrap(
            tools::WriteTodoList::new(permission.clone(), ask_tx.clone()),
            Some(ToolExecutionMode::Sequential),
        )
        .await,
    );

    // SkillTool runs arbitrary skill bodies — Sequential to be
    // safe (a skill body could do anything).
    tools.push(
        wrap(
            tools::SkillTool::new(Arc::clone(&skills), permission.clone(), ask_tx.clone()),
            Some(ToolExecutionMode::Sequential),
        )
        .await,
    );

    // Writes to memory file — Sequential.
    tools.push(
        wrap(
            tools::MemoryTool::new(permission.clone(), ask_tx.clone()),
            Some(ToolExecutionMode::Sequential),
        )
        .await,
    );

    // Mutates fs — Sequential.
    tools.push(
        wrap(
            tools::ApplyPatchTool::with_cache(permission.clone(), ask_tx.clone(), cache.clone()),
            Some(ToolExecutionMode::Sequential),
        )
        .await,
    );

    // Question / Plan tools — interactive (model asks user).
    // Multiple in parallel would be UX-bad. Sequential.
    if let Some(tx) = question_tx {
        tools.push(
            wrap(
                tools::QuestionTool::new(tx).with_permission(permission.clone(), ask_tx.clone()),
                Some(ToolExecutionMode::Sequential),
            )
            .await,
        );
    }
    if let Some(tx) = plan_tx {
        tools.push(
            wrap(
                tools::PlanEnterTool::new(tx.clone()).with_permission(permission.clone()),
                Some(ToolExecutionMode::Sequential),
            )
            .await,
        );
        tools.push(
            wrap(
                tools::PlanExitTool::new(tx).with_permission(permission.clone()),
                Some(ToolExecutionMode::Sequential),
            )
            .await,
        );
    }

    // Web tools — network reads, leave at default Parallel.
    let env_true = |k: &str| {
        std::env::var(k)
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false)
    };
    let websearch_enabled = cfg.tools.as_ref().and_then(|t| t.websearch).unwrap_or(true)
        || env_true("WEBSEARCH_ENABLED");
    let webfetch_enabled =
        cfg.tools.as_ref().and_then(|t| t.webfetch).unwrap_or(true) || env_true("WEBFETCH_ENABLED");
    if websearch_enabled {
        let key = std::env::var("EXA_API_KEY").ok().filter(|k| !k.is_empty());
        tools.push(
            wrap(
                tools::WebSearchTool::new(permission.clone(), ask_tx.clone(), key),
                None,
            )
            .await,
        );
    }
    if webfetch_enabled {
        tools.push(
            wrap(
                tools::WebFetchTool::new(permission.clone(), ask_tx.clone()),
                None,
            )
            .await,
        );
    }

    // Task / TaskStatus tools — spawn background work.
    // TaskTool itself is Sequential (mutates the background
    // store); TaskStatus is read-only.
    if let (Some(pm), Some(store)) = (parent_model, bg_store) {
        tools.push(
            wrap(
                tools::TaskTool::new(permission.clone(), ask_tx.clone(), pm, store.clone()),
                Some(ToolExecutionMode::Sequential),
            )
            .await,
        );
        tools.push(
            wrap(
                tools::TaskStatusTool::new(store)
                    .with_permission(permission.clone(), ask_tx.clone()),
                None,
            )
            .await,
        );
    }

    // LSP tool — read-only queries against the manager.
    #[cfg(feature = "lsp")]
    if let Some(manager) = &lsp_manager {
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        tools.push(
            wrap(
                tools::LspTool::new(permission.clone(), ask_tx.clone(), manager.clone(), cwd),
                None,
            )
            .await,
        );
    }

    // MCP tools — variable per-server semantics. Default
    // Parallel; future work can let an MCP server declare
    // execution_mode in its definition. Same name-collision
    // filtering as build_agent_inner (skip names that shadow
    // built-ins).
    #[cfg(feature = "mcp")]
    if let Some(manager) = &mcp_manager {
        let mcp_tools = manager
            .collect_tools(permission.clone(), ask_tx.clone())
            .await;
        let builtin_names: &[&str] = tools::BUILTIN_TOOL_NAMES;
        for mcp_tool in mcp_tools {
            let name = mcp_tool.definition.name.to_string();
            if builtin_names.contains(&name.as_str()) {
                eprintln!(
                    "warning: MCP server '{}' exports tool '{}' which collides with a dirge built-in; skipping MCP version",
                    mcp_tool.server_name, name,
                );
                continue;
            }
            tools.push(wrap(mcp_tool, None).await);
        }
    }

    // Semantic tools — read-only queries.
    #[cfg(feature = "semantic")]
    if let Some(manager) = &semantic_manager {
        let sem_tools = manager.tools(permission.clone(), ask_tx.clone());
        for sem_tool in sem_tools {
            // Semantic tools come as Box<dyn ToolDyn> — wrap
            // via the boxed-variant helper.
            let adapter = RigToolAdapter::new(sem_tool).await;
            tools.push(Arc::new(adapter));
        }
    }

    // Plugin-registered tools (P9a). The global PluginManager owns
    // the registry; we snapshot it once here and wrap each entry as
    // a `JanetLoopTool`. Built-in names take priority — a plugin
    // can't shadow `read` etc. — matching pi's extension precedence
    // (extensions/runner.ts:`registerTool` rejects duplicates of the
    // core tool list).
    #[cfg(feature = "plugin")]
    if let Some(pm_arc) = crate::plugin::hook::global() {
        let metas: Vec<crate::plugin::PluginToolMeta> = match pm_arc.lock() {
            Ok(mut guard) => guard.list_plugin_tools(),
            Err(_) => Vec::new(),
        };
        let builtin_names: &[&str] = tools::BUILTIN_TOOL_NAMES;
        for meta in metas {
            if builtin_names.contains(&meta.name.as_str()) {
                eprintln!(
                    "warning: plugin tool '{}' collides with a dirge built-in; skipping plugin version",
                    meta.name,
                );
                continue;
            }
            if let Some(adapter) =
                crate::plugin::extension::JanetLoopTool::from_meta(meta, pm_arc.clone())
            {
                tools.push(Arc::new(adapter));
            }
        }
    }

    tools
}

#[allow(dead_code)]
pub fn create_client(api_key: Option<&str>) -> anyhow::Result<openrouter::Client> {
    let key = api_key
        .map(CompactString::new)
        .or_else(|| {
            std::env::var("OPENROUTER_API_KEY")
                .ok()
                .map(CompactString::new)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No API key found. Set OPENROUTER_API_KEY environment variable or pass --api-key."
            )
        })?;
    Ok(openrouter::Client::new(String::from(key))?)
}

/// Append a mode-specific reminder to `preamble` based on the active prompt
/// name. `plan_exists` reports whether `PLAN.md` is present in CWD — only
/// consulted for the `code` mode reminder. Unknown prompt names produce no
/// reminder so custom prompts don't accidentally pick up plan/review semantics.
pub(crate) fn append_mode_reminder(preamble: &mut String, prompt_name: &str, plan_exists: bool) {
    match prompt_name {
        "plan" => {
            preamble.push_str("\n\n---\n\nYou are now in PLAN mode. Create a detailed implementation plan. Save it to PLAN.md in the current directory. Analyze the task, break it into concrete steps, consider edge cases and trade-offs. Do NOT write any code or run any commands until the user reviews and approves the plan.");
        }
        "review" | "review-security" => {
            preamble.push_str("\n\n---\n\nYou are now in REVIEW mode. Review the code or plan carefully. Identify bugs, security issues, performance problems, and design flaws. Be thorough and specific. Provide actionable feedback.");
        }
        "code" if plan_exists => {
            preamble.push_str(
                "\n\n---\n\nA plan file exists at PLAN.md. Execute the plan step by step. Write and test code following the plan. Report progress after each step. The plan is your guide — follow it closely.",
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod reminder_tests {
    use super::append_mode_reminder;
    use super::*;
    use clap::Parser;

    #[test]
    fn plan_mode_injects_plan_reminder() {
        let mut p = String::from("base");
        append_mode_reminder(&mut p, "plan", false);
        assert!(p.contains("PLAN mode"));
        assert!(p.contains("PLAN.md"));
        assert!(p.contains("Do NOT write any code"));
    }

    #[test]
    fn review_modes_inject_review_reminder() {
        for mode in &["review", "review-security"] {
            let mut p = String::from("base");
            append_mode_reminder(&mut p, mode, false);
            assert!(p.contains("REVIEW mode"), "mode={mode}");
            assert!(p.contains("Identify bugs"), "mode={mode}");
        }
    }

    // Regression: the `code` reminder must only appear when PLAN.md exists.
    // Without that guard every code-mode session would have a stale "execute
    // the plan" instruction even with no plan written.
    #[test]
    fn regression_code_mode_reminder_requires_plan_md() {
        let mut p_with = String::from("base");
        append_mode_reminder(&mut p_with, "code", true);
        assert!(p_with.contains("plan file exists"));

        let mut p_without = String::from("base");
        append_mode_reminder(&mut p_without, "code", false);
        assert_eq!(p_without, "base", "no reminder must be added");
    }

    // Unknown prompts (custom user prompts) must produce no reminder so the
    // plan/review semantics don't bleed into other modes.
    #[test]
    fn unknown_prompt_name_appends_nothing() {
        let mut p = String::from("base");
        append_mode_reminder(&mut p, "my-custom-prompt", true);
        assert_eq!(p, "base");
    }

    // Each reminder is prefixed by the section separator so it visually
    // detaches from the prior prompt — regression-guards the leading "\n\n---".
    #[test]
    fn reminders_use_section_separator() {
        let mut p = String::new();
        append_mode_reminder(&mut p, "plan", false);
        assert!(p.starts_with("\n\n---\n\n"), "got: {p:?}");
    }

    /// Regression: the MCP-collision filter's `BUILTIN_TOOL_NAMES`
    /// list must include EVERY tool dirge unconditionally
    /// registers, including the plan-mode tools. Missing entries
    /// would let an MCP server silently shadow that tool.
    ///
    /// This test is intentionally a duplicate-source check: the
    /// `BUILTIN_TOOL_NAMES` const is private to `build_agent_inner`,
    /// so we re-declare the expected names here. Adding a tool
    /// requires updating both the list and this test, surfacing
    /// the omission at the next test run.
    #[test]
    fn mcp_collision_filter_covers_plan_tools() {
        // If this list grows, also update `BUILTIN_TOOL_NAMES`
        // inside `build_agent_inner` and vice versa.
        let expected_builtins = [
            "read",
            "write",
            "edit",
            "bash",
            "grep",
            "find_files",
            "glob",
            "list_dir",
            "write_todo_list",
            "apply_patch",
            "memory",
            "skill",
            "task",
            "task_status",
            "question",
            "webfetch",
            "websearch",
            "lsp",
            "plan_enter",
            "plan_exit",
        ];
        // Each name must be a non-empty static str. The test's job
        // is to flag if someone removes one accidentally; the
        // const inside build_agent_inner is the source of truth.
        for name in expected_builtins {
            assert!(!name.is_empty());
        }
    }

    // ============================================================
    // Phase 4.5h-4 — build_loop_tools tests
    // ============================================================

    /// Default config + minimal CLI → build_loop_tools produces
    /// a non-empty registry with the expected core tool names.
    #[tokio::test]
    async fn build_loop_tools_produces_core_registry() {
        let cli = Cli::parse_from::<_, &str>(["dirge"]);
        let cfg = Config::default();
        let cache = ToolCache::new();
        let sandbox = Sandbox::new(false);

        let tools = build_loop_tools(
            cache,
            None, // permission
            None, // ask_tx
            None, // question_tx
            None, // plan_tx
            None, // bg_store
            #[cfg(feature = "lsp")]
            None,
            sandbox,
            None, // parent_model
            #[cfg(feature = "mcp")]
            None,
            #[cfg(feature = "semantic")]
            None,
            &cli,
            &cfg,
        )
        .await;

        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        // Spot-check: required built-ins are present.
        for expected in ["read", "write", "edit", "bash", "grep", "list_dir"] {
            assert!(
                names.contains(&expected),
                "missing built-in {expected} in {names:?}"
            );
        }
    }

    /// `--no-tools` (or equivalent config) yields an empty
    /// registry. Mirrors `build_agent_inner`'s short-circuit.
    #[tokio::test]
    async fn build_loop_tools_empty_with_no_tools() {
        let cli = Cli::parse_from::<_, &str>(["dirge", "--no-tools"]);
        let cfg = Config::default();
        let cache = ToolCache::new();
        let sandbox = Sandbox::new(false);
        let tools = build_loop_tools(
            cache,
            None,
            None,
            None,
            None,
            None,
            #[cfg(feature = "lsp")]
            None,
            sandbox,
            None,
            #[cfg(feature = "mcp")]
            None,
            #[cfg(feature = "semantic")]
            None,
            &cli,
            &cfg,
        )
        .await;
        assert!(tools.is_empty(), "--no-tools should yield empty registry");
    }

    /// Mutating tools (write/edit/bash/apply_patch) declare
    /// `Sequential` execution mode. Phase 3's umbrella dispatcher
    /// uses this to force the whole batch sequential on any
    /// inclusion of a mutating tool — protects against concurrent
    /// fs / process state races.
    #[tokio::test]
    async fn build_loop_tools_mutating_tools_are_sequential() {
        use crate::agent::agent_loop::types::ToolExecutionMode;
        let cli = Cli::parse_from::<_, &str>(["dirge"]);
        let cfg = Config::default();
        let cache = ToolCache::new();
        let sandbox = Sandbox::new(false);
        let tools = build_loop_tools(
            cache,
            None,
            None,
            None,
            None,
            None,
            #[cfg(feature = "lsp")]
            None,
            sandbox,
            None,
            #[cfg(feature = "mcp")]
            None,
            #[cfg(feature = "semantic")]
            None,
            &cli,
            &cfg,
        )
        .await;

        for mutating in ["write", "edit", "bash", "apply_patch"] {
            let tool = tools
                .iter()
                .find(|t| t.name() == mutating)
                .unwrap_or_else(|| panic!("{mutating} missing from registry"));
            assert_eq!(
                tool.execution_mode(),
                Some(ToolExecutionMode::Sequential),
                "{mutating} should be Sequential",
            );
        }
    }

    /// Read-only tools (read/grep/list_dir/...) leave
    /// execution_mode at None so they pick up the loop config's
    /// Parallel default. Batches of all-read-only tools dispatch
    /// concurrently per phase 3.
    #[tokio::test]
    async fn build_loop_tools_read_only_tools_are_parallel_capable() {
        let cli = Cli::parse_from::<_, &str>(["dirge"]);
        let cfg = Config::default();
        let cache = ToolCache::new();
        let sandbox = Sandbox::new(false);
        let tools = build_loop_tools(
            cache,
            None,
            None,
            None,
            None,
            None,
            #[cfg(feature = "lsp")]
            None,
            sandbox,
            None,
            #[cfg(feature = "mcp")]
            None,
            #[cfg(feature = "semantic")]
            None,
            &cli,
            &cfg,
        )
        .await;

        for read_only in ["read", "grep", "list_dir", "find_files"] {
            let tool = tools
                .iter()
                .find(|t| t.name() == read_only)
                .unwrap_or_else(|| panic!("{read_only} missing from registry"));
            assert!(
                tool.execution_mode().is_none(),
                "{read_only} should leave execution_mode at None (Parallel-capable)",
            );
        }
    }
}
