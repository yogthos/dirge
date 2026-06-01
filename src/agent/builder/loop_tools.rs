//! LoopTool-registry construction for the agent builder. Split out of
//! `agent/builder.rs` (dirge-4y4l stage 11b): assembles the
//! `Vec<Arc<dyn LoopTool>>` the agent_loop dispatches against
//! (`build_loop_tools`), wraps background-injected MCP tools
//! (`wrap_mcp_tools`), and carries the dynamic-tool-search handles
//! (`DynamicToolSearch`).

use std::sync::Arc;

use crate::agent::tools;
use crate::agent::tools::ToolCache;
use crate::agent::tools::background::BackgroundStore;
use crate::agent::tools::plan::PlanSwitchSender;
use crate::agent::tools::question::QuestionSender;
use crate::cli::Cli;
use crate::config::Config;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::AnyModel;
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;

use crate::skill::{self, Skill};

use super::build_session_search_tool;

/// dirge-x949: wrap a batch of freshly-collected MCP tools into the
/// `LoopTool` adapters the agent loop dispatches against, applying the
/// same built-in-name collision filter `build_loop_tools` uses. Pulled
/// out so background MCP loading (see main.rs) can inject server tools
/// into an already-running agent *after* the UI is drawn, instead of
/// blocking startup on `connect_all` + `collect_tools`.
#[cfg(feature = "mcp")]
pub async fn wrap_mcp_tools(
    mcp_tools: Vec<crate::extras::mcp::tool::McpTool>,
) -> Vec<Arc<dyn crate::agent::agent_loop::LoopTool>> {
    use crate::agent::agent_loop::RigToolAdapter;
    let builtin_names: &[&str] = tools::BUILTIN_TOOL_NAMES;
    let mut out: Vec<Arc<dyn crate::agent::agent_loop::LoopTool>> = Vec::new();
    for mcp_tool in mcp_tools {
        let name = mcp_tool.definition.name.to_string();
        if builtin_names.contains(&name.as_str()) {
            eprintln!(
                "warning: MCP server '{}' exports tool '{}' which collides with a dirge built-in; skipping MCP version",
                mcp_tool.server_name, name,
            );
            continue;
        }
        let adapter = RigToolAdapter::new(Box::new(mcp_tool)).await;
        out.push(Arc::new(adapter));
    }
    out
}

// ============================================================
// Phase 4.5h-4 — parallel LoopTool registry builder
// ============================================================

/// dirge-tpx6: the dynamic_tool_search state `build_loop_tools` produces
/// for the agent to hold onto. Both Arcs are the SAME ones the
/// `ToolSearchTool` registered in `loop_tools` holds, so the agent can
/// mutate them after build:
/// - `filter` — the shared loaded-set (names whose full defs ship each
///   request); `tool_search` inserts into it as the model discovers tools.
/// - `registry` — the live searchable catalog; `extend_loop_tools` appends
///   background-injected MCP tools here so they stay search-gated.
pub struct DynamicToolSearch {
    pub filter: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    pub registry: std::sync::Arc<std::sync::Mutex<Vec<tools::ToolMeta>>>,
}

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
    Option<DynamicToolSearch>,
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
        let registry_vec: Vec<tools::ToolMeta> = tools
            .iter()
            .map(|t| tools::tool_search::meta_from_loop_tool(t.as_ref()))
            .collect();
        // dirge-tpx6: registry behind a Mutex so the background MCP
        // loader can append late-connected tools (keeping them
        // search-gated). The SAME Arc goes into the ToolSearchTool and
        // back to the agent via `DynamicToolSearch`.
        let registry = std::sync::Arc::new(std::sync::Mutex::new(registry_vec));
        let filter: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let search_tool = tools::ToolSearchTool::new(registry.clone(), filter.clone());
        // ToolSearchTool implements LoopTool directly (not via
        // RigToolAdapter — it needs to mutate session state and
        // doesn't fit the rig::ToolDyn shape). Push as Arc
        // straight away.
        tools.push(Arc::new(search_tool));
        Some(DynamicToolSearch { filter, registry })
    } else {
        None
    };

    (tools, tool_def_filter)
}
