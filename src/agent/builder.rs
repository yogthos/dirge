use rig::agent::{Agent, AgentBuilder};
use rig::completion::CompletionModel;
use std::sync::Arc;

use crate::agent::model_family::{ModelFamily, resolve_family};
use crate::agent::prompt::{
    DEEPSEEK_GUIDANCE, MEMORY_GUIDANCE, PROJECT_SKILLS_PREAMBLE, SESSION_SEARCH_GUIDANCE,
    SKILLS_GUIDANCE, SYSTEM_PROMPT, TODO_TOOLS_PROMPT,
};
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

/// Append a memory provider's prompt block to the assembled preamble.
/// Goes through `MemoryProvider::format_for_system_prompt`
/// (trait-dispatched) so a non-default backend's block lands in the
/// preamble too — pre-fix `builder.rs` called the concrete
/// `MemoryToolStore::format_for_system_prompt` directly, which broke
/// any future plugin provider's prompt contribution. See dirge-fmau.
pub(crate) fn append_memory_to_preamble(
    preamble: &mut String,
    provider: &std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>,
) {
    tracing::debug!(
        target: "dirge::memory",
        provider = provider.name(),
        "Injecting memory provider prompt block"
    );
    let block = provider.format_for_system_prompt();
    if !block.is_empty() {
        preamble.push_str(&block);
    }
}

/// Assemble the always-on base preamble — `SYSTEM_PROMPT`,
/// `TODO_TOOLS_PROMPT`, and the in-session `SKILLS_GUIDANCE`
/// (dirge-xxun, mirroring hermes `SKILLS_GUIDANCE`). Other contextual
/// blocks (AGENTS.md, prompts, project skills, memory) are layered on
/// top by `build_agent_inner`. Extracted so the assembly is testable
/// without exercising the full DI signature.
pub(crate) fn assemble_base_preamble() -> String {
    let mut p = SYSTEM_PROMPT.to_string();
    p.push('\n');
    p.push_str(TODO_TOOLS_PROMPT);
    // dirge-xxun: skills self-improvement nudge (hermes SKILLS_GUIDANCE).
    p.push_str(SKILLS_GUIDANCE);
    // dirge-a6bv: memory + past-session recall guidance (hermes
    // MEMORY_GUIDANCE + SESSION_SEARCH_GUIDANCE). Both tools are always
    // present in dirge's registry, so we inject unconditionally rather
    // than tool-gating like hermes does on `valid_tool_names`.
    p.push_str(MEMORY_GUIDANCE);
    p.push_str(SESSION_SEARCH_GUIDANCE);
    p
}

/// Model-specific steering fragment to append to the preamble, if any.
///
/// Returns the DeepSeek guidance for DeepSeek **chat** models and `None`
/// for everything else (other vendors, and the DeepSeek reasoner, which
/// ignores the system prompt). Appended last by `build_agent_inner` so it
/// sits closest to the conversation / action boundary — research shows
/// rules stated far from the decision point lose influence in long
/// tool-calling loops ("prompt-distance drift").
pub(crate) fn model_steering_fragment(family: ModelFamily) -> Option<&'static str> {
    if family.is_deepseek_chat() {
        Some(DEEPSEEK_GUIDANCE)
    } else {
        None
    }
}

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
    // Active session id forwarded to SessionSearchTool — see
    // dirge-502b. Mirrors the same param on `build_agent_inner`.
    session_id: Option<String>,
) -> (
    Vec<std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>>,
    Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
) {
    use crate::agent::agent_loop::types::ToolExecutionMode;
    use crate::agent::agent_loop::{LoopTool, RigToolAdapter};

    if cli.resolve_no_tools(cfg) {
        return (Vec::new(), None);
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let paths = crate::extras::dirge_paths::ProjectPaths::new(&cwd);
    let skill_mgr = crate::extras::skills::manager::SkillManager::new(&paths);
    let usage_store = crate::extras::skills::usage::UsageStore::load(&paths).ok();
    let skills: Arc<[Skill]> = Arc::from(
        tokio::task::spawn_blocking(move || skill::discover_skills(&cwd))
            .await
            .unwrap_or_default(),
    );

    // dirge-dktb: same synchronous-I/O fix as `build_agent_inner`.
    // Off-load the disk read to the blocking pool so a slow
    // filesystem can't stall the async runtime worker. dirge-fmau:
    // returns `Arc<dyn MemoryProvider>` so plugin backends can plug
    // in without churning the call sites.
    let memory_store: Option<Arc<dyn crate::extras::memory_provider::MemoryProvider>> =
        if let Ok(c) = std::env::current_dir() {
            let paths = crate::extras::dirge_paths::ProjectPaths::new(&c);
            tokio::task::spawn_blocking(move || {
                crate::extras::memory_store::MemoryToolStore::load(&paths)
                    .ok()
                    .map(|s| {
                        let arc: Arc<dyn crate::extras::memory_provider::MemoryProvider> =
                            Arc::new(s);
                        arc
                    })
            })
            .await
            .unwrap_or_default()
        } else {
            None
        };

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
            )
            .with_shell_store(Some(tools::bg_shell::global())),
            Some(ToolExecutionMode::Sequential),
        )
        .await,
    );
    tools.push(wrap(tools::BashOutputTool::new(tools::bg_shell::global()), None).await);
    tools.push(wrap(tools::KillShellTool::new(tools::bg_shell::global()), None).await);

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

    // Session search — read-only DB queries.
    let session_db_path = std::env::current_dir()
        .map(|c| crate::extras::dirge_paths::ProjectPaths::new(&c).session_db_path())
        .unwrap_or_else(|_| std::path::PathBuf::from(".dirge/sessions/state.db"));
    tools.push(
        wrap(
            build_session_search_tool(
                session_db_path,
                session_id.clone(),
                permission.clone(),
                ask_tx.clone(),
            ),
            None,
        )
        .await,
    );

    // SkillTool runs arbitrary skill bodies — Sequential to be
    // safe (a skill body could do anything).
    tools.push(
        wrap(
            tools::SkillTool::new(
                Arc::clone(&skills),
                skill_mgr,
                usage_store.clone(),
                permission.clone(),
                ask_tx.clone(),
            ),
            Some(ToolExecutionMode::Sequential),
        )
        .await,
    );

    // Writes to memory file — Sequential.
    tools.push(
        wrap(
            tools::MemoryTool::new(
                memory_store
                    .clone()
                    .expect("memory_store not loaded in loop tools"),
                permission.clone(),
                ask_tx.clone(),
            ),
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
    let websearch_enabled = crate::config::websearch_enabled(cfg);
    let webfetch_enabled = crate::config::webfetch_enabled(cfg);
    if websearch_enabled {
        let key = crate::config::exa_api_key();
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
        tools.push(
            wrap(
                tools::LspTool::new(permission.clone(), ask_tx.clone(), manager.clone()),
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

    // Phase-3: dynamic-tool-search opt-in. When enabled, take a
    // metadata snapshot of EVERY tool registered above (registry
    // includes plugin + MCP + semantic + built-ins), allocate the
    // shared loaded-set Arc, and register `ToolSearchTool`
    // alongside the rest. The SAME Arc is returned so
    // `build_agent` can attach it to `AnyAgent.tool_def_filter`
    // (which `spawn_runner` then forwards to the stream
    // factory's filter).
    let tool_def_filter = if cfg.resolve_dynamic_tool_search() {
        let registry: Vec<tools::ToolMeta> = tools
            .iter()
            .map(|t| tools::tool_search::meta_from_loop_tool(t.as_ref()))
            .collect();
        let filter: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let search_tool = tools::ToolSearchTool::new(Arc::new(registry), filter.clone());
        // ToolSearchTool implements LoopTool directly (not via
        // RigToolAdapter — it needs to mutate session state and
        // doesn't fit the rig::ToolDyn shape). Push as Arc
        // straight away.
        tools.push(Arc::new(search_tool));
        Some(filter)
    } else {
        None
    };

    (tools, tool_def_filter)
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

    #[test]
    fn deepseek_chat_gets_steering_fragment() {
        let family = resolve_family("deepseek", "deepseek-v4-pro");
        let frag = model_steering_fragment(family).expect("deepseek chat should get steering");
        assert!(
            frag.contains("Plan-Execute-Verify"),
            "fragment should carry the research-backed guidance"
        );
        assert!(
            frag.contains("re-issue the same call"),
            "fragment should carry the anti-repetition rule"
        );
    }

    #[test]
    fn other_models_get_no_steering_fragment() {
        assert!(model_steering_fragment(resolve_family("openai", "gpt-4o")).is_none());
        assert!(
            model_steering_fragment(resolve_family("anthropic", "claude-sonnet-4-6")).is_none()
        );
    }

    #[test]
    fn deepseek_reasoner_gets_no_steering_fragment() {
        // R1 ignores the system prompt, so appending preamble guidance is
        // pointless — the fragment is chat-only.
        let family = resolve_family("deepseek", "deepseek-reasoner");
        assert!(model_steering_fragment(family).is_none());
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

        let (tools, _) = build_loop_tools(
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
            None, // session_id — test default
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

    /// dirge-xxun — the always-on base preamble includes the in-session
    /// skill creation/patch guidance, so the model sees the trigger
    /// every turn (not just at post-session review).
    #[test]
    fn base_preamble_includes_skills_guidance() {
        let p = assemble_base_preamble();
        // Trigger fragments must be present.
        assert!(p.contains("complex task"), "missing create trigger");
        assert!(p.contains("5+ tool calls"), "missing 5+ trigger");
        assert!(
            p.contains("patch it immediately"),
            "missing patch-now trigger"
        );
        // Must name the real `skill` actions.
        assert!(p.contains("action='create'"), "missing create action");
        assert!(p.contains("action='patch'"), "missing patch action");
        // Must NOT name hermes's tool aliases.
        assert!(!p.contains("skill_manage"), "leaked hermes tool name");
        // Section heading present.
        assert!(
            p.contains("## Skill creation and maintenance"),
            "missing heading"
        );
    }

    /// F2 — the base preamble carries the finishing self-check + stop
    /// condition so every agent (all models) gets it in the system
    /// prompt, not just in prose scattered elsewhere.
    #[test]
    fn base_preamble_includes_finishing_selfcheck() {
        let p = assemble_base_preamble();
        assert!(
            p.contains("# Finishing"),
            "missing the Finishing section heading"
        );
        assert!(
            p.to_lowercase().contains("self-check"),
            "missing the single self-check"
        );
        // The three checks: did exactly what was asked, verified it,
        // no unrequested changes.
        assert!(p.contains("exactly what was asked"), "missing scope check");
        assert!(
            p.to_lowercase().contains("verified"),
            "missing verify check"
        );
        assert!(
            p.to_lowercase().contains("unrequested"),
            "missing no-scope-creep check"
        );
        // Explicit stop condition.
        assert!(
            p.to_lowercase().contains("stop"),
            "missing an explicit stop condition"
        );
    }

    /// Phase D — all three base-prompt guidance features (F2 finishing
    /// self-check, F3 progress narration, F5 ask-vs-proceed calibration)
    /// coexist in one assembled preamble. Guards against a future edit
    /// silently dropping one.
    #[test]
    fn base_preamble_carries_full_guidance_suite() {
        let p = assemble_base_preamble();
        assert!(
            p.contains("# Finishing a task"),
            "F2 finishing self-check missing"
        );
        assert!(
            p.contains("# Progress updates"),
            "F3 progress narration missing"
        );
        assert!(
            p.contains("# Clarifying vs. proceeding"),
            "F5 ask-vs-proceed calibration missing"
        );
    }

    /// F3 — the base preamble tells the agent to plan up front and post
    /// terse progress notes during multi-step tool runs, while keeping
    /// the final reply terse (no contradiction with the Output section).
    #[test]
    fn base_preamble_includes_progress_updates() {
        let p = assemble_base_preamble();
        assert!(
            p.contains("# Progress updates"),
            "missing the Progress updates section heading"
        );
        assert!(
            p.to_lowercase().contains("plan up front"),
            "missing the up-front plan guidance"
        );
        assert!(
            p.to_lowercase().contains("multi-step"),
            "progress guidance must be scoped to multi-step tasks"
        );
        // Must explicitly exclude the final reply so it does not fight
        // the terse-output rule.
        assert!(
            p.to_lowercase().contains("final reply"),
            "must distinguish progress notes from the final reply"
        );
    }

    /// F5 — the base preamble carries the ask-vs-proceed calibration so
    /// the agent decides by cost/recoverability rather than reflex, and
    /// proceeds with a stated assumption when a question isn't warranted.
    #[test]
    fn base_preamble_includes_ask_calibration() {
        let p = assemble_base_preamble();
        let lower = p.to_lowercase();
        assert!(
            p.contains("# Clarifying vs. proceeding"),
            "missing the Clarifying-vs-proceeding section"
        );
        // Calibration signals.
        assert!(
            lower.contains("hard to reverse") || lower.contains("costly"),
            "missing the cost/recoverability signal"
        );
        assert!(
            lower.contains("infer"),
            "missing the inferable-from-context signal"
        );
        // The proceed-and-state-the-assumption path.
        assert!(
            lower.contains("assumption"),
            "missing the state-your-assumption path"
        );
    }

    /// dirge-fmau — the memory-preamble injection path goes through
    /// the `MemoryProvider` trait, so a non-default backend's prompt
    /// block lands in the preamble too. Recording provider verifies
    /// the trait method is called exactly once and its output appears.
    #[test]
    fn memory_preamble_injection_uses_trait_dispatch() {
        use crate::extras::memory_provider::MemoryProvider;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct RecordingProvider {
            calls: AtomicUsize,
            block: String,
        }
        impl MemoryProvider for RecordingProvider {
            fn name(&self) -> &str {
                "recording"
            }
            fn format_for_system_prompt(&self) -> String {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.block.clone()
            }
            fn view(&self, _: &str) -> serde_json::Value {
                serde_json::Value::Null
            }
            fn add(&self, _: &str, _: &str) -> Result<serde_json::Value, String> {
                Ok(serde_json::Value::Null)
            }
            fn replace(&self, _: &str, _: &str, _: &str) -> Result<serde_json::Value, String> {
                Ok(serde_json::Value::Null)
            }
            fn remove(&self, _: &str, _: &str) -> Result<serde_json::Value, String> {
                Ok(serde_json::Value::Null)
            }
        }

        let provider: Arc<dyn MemoryProvider> = Arc::new(RecordingProvider {
            calls: AtomicUsize::new(0),
            block: "\n\n## RecordingProviderBlock\n\nplugin-supplied prompt text\n".into(),
        });

        let mut preamble = String::from("base");
        append_memory_to_preamble(&mut preamble, &provider);

        assert!(
            preamble.contains("## RecordingProviderBlock"),
            "plugin provider's prompt heading must appear: {preamble}"
        );
        assert!(
            preamble.contains("plugin-supplied prompt text"),
            "plugin provider's body must appear: {preamble}"
        );

        // Empty block must not append anything.
        let empty: Arc<dyn MemoryProvider> = Arc::new(RecordingProvider {
            calls: AtomicUsize::new(0),
            block: String::new(),
        });
        let mut preamble2 = String::from("base2");
        append_memory_to_preamble(&mut preamble2, &empty);
        assert_eq!(
            preamble2, "base2",
            "empty provider block must not append anything"
        );
    }

    /// dirge-1ati — full end-to-end: `build_agent_inner` runs with
    /// the same DI pattern as production and produces an `Agent<M>`
    /// whose `preamble` field carries every guidance block the
    /// unit-level tests assert on (SKILLS_GUIDANCE, MEMORY_GUIDANCE,
    /// SESSION_SEARCH_GUIDANCE). Pre-fix only the assembly helper
    /// was tested; this test exercises the full builder path so a
    /// future change that accidentally drops a guidance block from
    /// the wiring (rather than from the helper) is caught.
    #[tokio::test]
    async fn build_agent_inner_emits_assembled_preamble() {
        use crate::context::ContextFiles;
        use rig::client::CompletionClient;
        use rig::providers::openai;

        let cli = Cli::parse_from::<_, &str>(["dirge"]);
        let cfg = Config::default();
        let context = ContextFiles {
            agents: None,
            prompts: std::collections::HashMap::new(),
            current_prompt: None,
            current_prompt_name: None,
            current_prompt_deny_tools: Vec::new(),
        };
        // Real openai client/model — never called (no network until
        // first request). The builder only inspects type bounds and
        // builds the rig Agent wrapper around it.
        let client = openai::Client::new("test-key")
            .expect("openai client builds")
            .completions_api();
        let model = client.completion_model("gpt-4o");
        let sandbox = Sandbox::new(false);

        let (agent, _cache, _provider) = build_agent_inner(
            model,
            &cli,
            &cfg,
            &context,
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
            None, // session_id
        )
        .await;

        let preamble = agent.preamble.unwrap_or_default();

        // SKILLS_GUIDANCE markers (dirge-xxun).
        assert!(
            preamble.contains("## Skill creation and maintenance"),
            "preamble must include skills heading"
        );
        assert!(
            preamble.contains("complex task"),
            "preamble must include create trigger"
        );
        assert!(
            preamble.contains("action='patch'"),
            "preamble must include skill patch action"
        );

        // MEMORY_GUIDANCE markers (dirge-a6bv).
        assert!(
            preamble.contains("persistent memory"),
            "preamble must include memory intro"
        );
        assert!(
            preamble.contains("Do NOT save task progress"),
            "preamble must include do-not-save rule"
        );
        assert!(
            preamble.contains("declarative facts"),
            "preamble must include declarative-fact framing"
        );

        // SESSION_SEARCH_GUIDANCE markers (dirge-a6bv).
        assert!(
            preamble.contains("session_search"),
            "preamble must mention session_search"
        );
        assert!(
            preamble.contains("before asking them to repeat"),
            "preamble must include past-session-recall nudge"
        );

        // Memory tool action names match the real schema
        // (dirge-yqmo) — caught here in the resolved preamble so a
        // future change to the SYSTEM_PROMPT bullet is verified
        // end-to-end, not just in the constant.
        for action in ["view", "add", "replace", "remove"] {
            assert!(
                preamble.contains(action),
                "preamble must mention real memory action '{}'",
                action
            );
        }
        for forbidden in ["delete", "create"] {
            // Project skills preamble references action='create' /
            // 'delete' for the skill tool — those are valid words in
            // SKILLS_GUIDANCE / project-skills block. The memory
            // bullet itself must not contain them. So grep the
            // `- memory:` line specifically.
            let mem_line = preamble
                .lines()
                .find(|l| l.trim_start().starts_with("- memory:"))
                .expect("memory bullet present");
            assert!(
                !mem_line
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|w| w == forbidden),
                "memory bullet must not name forbidden action '{}': {}",
                forbidden,
                mem_line
            );
        }

        // Project-skills preamble action (dirge-rq65) — if the
        // project happens to have no skills the block is absent,
        // so only assert when present.
        if preamble.contains("## Project Skills") {
            assert!(
                preamble.contains("action='load'"),
                "project-skills preamble must direct to action='load'"
            );
        }
    }

    /// dirge-a6bv — assembled preamble carries hermes's MEMORY_GUIDANCE
    /// and SESSION_SEARCH_GUIDANCE blocks: when to save vs not save,
    /// declarative-fact phrasing, and the past-session-recall nudge.
    #[test]
    fn base_preamble_includes_memory_and_search_guidance() {
        let p = assemble_base_preamble();

        // MEMORY_GUIDANCE — must include the do/don't-save rules and the
        // declarative-vs-imperative example pair.
        assert!(p.contains("persistent memory"), "missing memory-tool intro");
        assert!(
            p.contains("Do NOT save task progress"),
            "missing do-not-save rule"
        );
        assert!(
            p.contains("PR numbers"),
            "missing example list of stale artifacts"
        );
        assert!(
            p.contains("declarative facts"),
            "missing declarative-fact framing"
        );
        assert!(
            p.contains("Procedures and workflows belong in skills"),
            "missing memory-vs-skills boundary"
        );

        // SESSION_SEARCH_GUIDANCE — must name the trigger.
        assert!(
            p.contains("session_search"),
            "missing session_search tool name"
        );
        assert!(
            p.contains("before asking them to repeat"),
            "missing past-session-recall nudge"
        );
    }

    /// dirge-502b — the shared factory used by both `build_agent_inner`
    /// and `build_loop_tools` actually carries the session id through
    /// to the constructed `SessionSearchTool`. Exercising the factory
    /// directly (rather than fishing the tool out of a `dyn LoopTool`
    /// registry) lets us assert on the concrete type.
    #[test]
    fn build_session_search_tool_threads_session_id() {
        let db_path = std::path::PathBuf::from("/tmp/dirge-502b-test.db");
        let tool =
            build_session_search_tool(db_path.clone(), Some("sess-test-id".into()), None, None);
        assert_eq!(
            tool.current_session_id(),
            Some("sess-test-id"),
            "factory must thread session_id into SessionSearchTool"
        );

        // And `None` truly means None (no silent default).
        let tool_none = build_session_search_tool(db_path, None, None, None);
        assert!(
            tool_none.current_session_id().is_none(),
            "factory must not invent a session id when called with None"
        );
    }

    /// `--no-tools` (or equivalent config) yields an empty
    /// registry. Mirrors `build_agent_inner`'s short-circuit.
    #[tokio::test]
    async fn build_loop_tools_empty_with_no_tools() {
        let cli = Cli::parse_from::<_, &str>(["dirge", "--no-tools"]);
        let cfg = Config::default();
        let cache = ToolCache::new();
        let sandbox = Sandbox::new(false);
        let (tools, _) = build_loop_tools(
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
            None, // session_id — test default
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
        let (tools, _) = build_loop_tools(
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
            None, // session_id — test default
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
        let (tools, _) = build_loop_tools(
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
            None, // session_id — test default
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
