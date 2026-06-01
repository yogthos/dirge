use rig::agent::{Agent, AgentBuilder};
use rig::completion::CompletionModel;
use std::sync::Arc;

use crate::agent::model_family::resolve_family;
use crate::agent::prompt::PROJECT_SKILLS_PREAMBLE;
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

mod loop_tools;
mod preamble;
pub use loop_tools::*;
pub(crate) use preamble::*;

/// Factory for the `SessionSearchTool` instance plumbed into both the
/// rig-side tool registry and the new agent_loop registry. Lives here
/// (rather than inline at each construction site) so the threading of
/// `session_id` is testable without downcasting through `dyn LoopTool`.
/// See dirge-502b.
pub(crate) fn build_session_search_tool(
    db_path: std::path::PathBuf,
    session_id: Option<String>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
) -> tools::SessionSearchTool {
    tools::SessionSearchTool::new(db_path, session_id, permission, ask_tx)
}

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

// Arity reflects the wide dependency-injection signature the agent
// builder uses — every collaborator (model, CLI, config, permission,
// channels, plugin manager, semantic index, hooks, …) is passed
// explicitly so wiring stays grep-able. Refactoring into a builder
// struct is tracked separately; silence the lint here.
#[allow(clippy::too_many_arguments)]
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
    // Active session id. Passed through to `SessionSearchTool` so the
    // model's `session_search` calls exclude the live session's own
    // turns. `None` is only correct for one-shot non-session runs.
    // See dirge-502b.
    session_id: Option<String>,
) -> (
    Agent<M>,
    ToolCache,
    // dirge-7tvq: surface the constructed MemoryProvider so the
    // caller (provider::build_agent) can attach it to AnyAgent for
    // session-lifecycle hook dispatch. `None` when load failed.
    Option<Arc<dyn crate::extras::memory_provider::MemoryProvider>>,
) {
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
    let mut preamble = assemble_base_preamble();
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
    let paths = std::env::current_dir()
        .map(|c| crate::extras::dirge_paths::ProjectPaths::new(&c))
        .unwrap_or_else(|_| {
            crate::extras::dirge_paths::ProjectPaths::new(std::path::Path::new("."))
        });
    // dirge-dktb: `MemoryToolStore::load` performs synchronous file
    // I/O for both memory + pitfalls. On slow filesystems (NFS,
    // network mounts) this blocks the async runtime worker thread
    // during agent construction. Move the synchronous load onto
    // the blocking pool, mirroring the `skill::discover_skills`
    // shape above. `unwrap_or_default()` collapses both a
    // `spawn_blocking` JoinError and a load error into `None`,
    // which matches the previous `Err(_) => None` branch.
    let paths_for_mem = paths.clone();
    let memory_load_result: Result<crate::extras::memory_store::MemoryToolStore, String> =
        tokio::task::spawn_blocking(move || {
            crate::extras::memory_store::MemoryToolStore::load(&paths_for_mem)
        })
        .await
        .unwrap_or_else(|_| Err("spawn_blocking join failed".to_string()));
    // dirge-fmau: route the preamble snapshot through the
    // `MemoryProvider` trait so a non-default backend's prompt block
    // appears too. The unsizing coercion from `Arc<MemoryToolStore>`
    // to `Arc<dyn MemoryProvider>` is the only call-site change.
    let memory_store: Option<Arc<dyn crate::extras::memory_provider::MemoryProvider>> =
        match memory_load_result {
            Ok(store) => {
                let provider: Arc<dyn crate::extras::memory_provider::MemoryProvider> =
                    Arc::new(store);
                append_memory_to_preamble(&mut preamble, &provider);
                Some(provider)
            }
            Err(_) => None,
        };
    let skill_manager = crate::extras::skills::manager::SkillManager::new(&paths);
    let mut usage_store = crate::extras::skills::usage::UsageStore::load(&paths).ok();

    // Inject available project skills into the preamble so the
    // model knows what procedural knowledge exists for this project.
    // Skills are listed with name + description; the model loads
    // full content on demand via the `skill` tool.
    // Bumps view counters for each listed skill (best-effort).
    match skill_manager.list() {
        Ok(names) if !names.is_empty() => {
            let mut skill_lines = Vec::new();
            for name in &names {
                if let Ok(content) = skill_manager.read_content(name)
                    && let Some(spec) =
                        crate::extras::skills::format::parse_skill_spec(&content, name)
                {
                    let desc = if spec.description.is_empty() {
                        "(no description)".to_string()
                    } else {
                        spec.description.clone()
                    };
                    skill_lines.push(format!("  - **{name}**: {desc}"));
                }
            }
            if !skill_lines.is_empty() {
                preamble.push_str(PROJECT_SKILLS_PREAMBLE);
                for line in &skill_lines {
                    preamble.push_str(line);
                    preamble.push('\n');
                }
                // Bump view counters for each skill listed in preamble (best-effort).
                if let Some(ref mut u) = usage_store {
                    for name in &names {
                        u.record_view(name);
                    }
                }
            }
        }
        _ => {}
    }

    // Inject mode-specific reminders
    if let Some(prompt_name) = &context.current_prompt_name {
        let plan_exists = std::env::current_dir()
            .unwrap_or_else(|_| ".".into())
            .join("PLAN.md")
            .exists();
        append_mode_reminder(&mut preamble, prompt_name, plan_exists);
    }

    // Model-aware steering. DeepSeek chat models get a research-backed
    // guidance fragment; appended last so it's nearest the action
    // boundary, resisting prompt-distance drift. No-op for other models.
    let family = resolve_family(&cli.resolve_provider(cfg), &cli.resolve_model(cfg));
    if let Some(fragment) = model_steering_fragment(family) {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(fragment);
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

    // Phase 3 / part 2: install configured inline-output budgets
    // for the disk-backed-output relay. `set_thresholds` writes
    // process-wide statics read by `relay_if_large` on every
    // bash/webfetch call. Done once at builder time — re-calling
    // with the same values is a cheap atomic store.
    crate::agent::tools::output_relay::set_thresholds(
        cfg.tools
            .as_ref()
            .and_then(|t| t.bash_output_inline_max_bytes),
        cfg.tools
            .as_ref()
            .and_then(|t| t.webfetch_output_inline_max_bytes),
        cfg.tools
            .as_ref()
            .and_then(|t| t.task_output_inline_max_bytes),
    );

    if cli.resolve_no_tools(cfg) {
        (builder.build(), ToolCache::new(), memory_store)
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
            Box::new(
                tools::BashTool::with_cache(
                    permission.clone(),
                    ask_tx.clone(),
                    sandbox.clone(),
                    cache.clone(),
                )
                .with_shell_store(Some(tools::bg_shell::global())),
            ),
            Box::new(tools::BashOutputTool::new(tools::bg_shell::global())),
            Box::new(tools::KillShellTool::new(tools::bg_shell::global())),
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
            Box::new(build_session_search_tool(
                paths.session_db_path(),
                session_id.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(tools::SkillTool::new(
                Arc::clone(&skills),
                skill_manager,
                usage_store.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(tools::MemoryTool::new(
                memory_store.clone().expect("memory_store not loaded"),
                permission.clone(),
                ask_tx.clone(),
            )),
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
        // docs/config.md documents both keys as defaulting to `true`;
        // explicit `false` in config disables. The env vars are
        // symmetric — previously only `WEBSEARCH_ENABLED` existed,
        // forcing webfetch users to edit config.json for an
        // equivalent toggle. The runtime API-key check still has
        // to pass for websearch.
        let websearch_enabled = crate::config::websearch_enabled(cfg);
        let webfetch_enabled = crate::config::webfetch_enabled(cfg);

        // Websearch now works out of the box without an API key —
        // mirrors opencode's behavior. Backend: Exa's hosted MCP
        // endpoint (`https://mcp.exa.ai/mcp`) which accepts unauth'd
        // calls at a lower rate limit, with DDG HTML scraping as
        // a fallback if Exa is unreachable. EXA_API_KEY remains
        // optional — when set, it's appended as `?exaApiKey=…` for
        // higher rate limits.
        let websearch_tool = if websearch_enabled {
            let key = crate::config::exa_api_key();
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
            let lsp_tool = Box::new(tools::LspTool::new(
                permission.clone(),
                ask_tx.clone(),
                manager.clone(),
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
                            // EXT-11: emit BOTH a tracing warn (for
                            // structured-log consumers and the in-UI
                            // notification pipeline) AND an eprintln
                            // (for users running without a tracing
                            // subscriber that surfaces warns). The
                            // MCP version is dropped rather than
                            // shadowing the built-in — rig's builder
                            // would otherwise prefer the last-added
                            // tool, letting an arbitrary MCP server
                            // override core dirge tools.
                            tracing::warn!(
                                target: "dirge::mcp",
                                server = %t.server_name,
                                tool = %name,
                                "MCP tool name collides with a dirge built-in; skipping MCP version",
                            );
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

        (builder.build(), cache, memory_store)
    }
}

/// dirge-x949: wrap a batch of freshly-collected MCP tools into the
/// `LoopTool` adapters the agent loop dispatches against, applying the
/// same built-in-name collision filter `build_loop_tools` uses. Pulled
/// out so background MCP loading (see main.rs) can inject server tools
/// into an already-running agent *after* the UI is drawn, instead of
/// blocking startup on `connect_all` + `collect_tools`.
#[cfg(feature = "mcp")]
#[cfg(test)]
mod reminder_tests;
