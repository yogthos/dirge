mod agent;
mod cli;
mod config;
mod context;
mod event;
mod extras;
mod image_util;
#[cfg(feature = "lsp")]
mod lsp;
mod permission;
mod plugin;
mod provider;
mod sandbox;
#[cfg(feature = "semantic")]
mod semantic;
mod session;
mod shell;
mod skill;
mod ui;

#[cfg(test)]
mod tests;

use clap::Parser;
use compact_str::CompactString;
use session::MessageRole;

use crate::agent::tools::background::{BackgroundStore, LifecycleReceiver};
use crate::agent::tools::plan::{PlanSwitchReceiver, PlanSwitchSender};
use crate::agent::tools::question::{QuestionReceiver, QuestionSender};
#[cfg(feature = "lsp")]
use crate::lsp::manager::LspManager;
#[cfg(feature = "lsp")]
use crate::lsp::spawn::{ProcessCommand, ProcessSpawner};
use crate::permission::ask::{AskReceiver, AskSender};
use crate::permission::checker::{PermCheck, PermissionChecker};
use crate::permission::{PermissionConfig, SecurityMode};

/// Per-session channels and shared state, threaded through the agent build
/// chain in place of a ten-position tuple. Cloneable senders + shared state
/// (`bg_store`, `lsp_manager`, `permission`) survive being moved through
/// `build_agent`; the receivers (`ask_rx`, `question_rx`, `plan_rx`,
/// `lifecycle_rx`) are unique-owner and end up consumed by the UI loop.
#[derive(Default)]
struct Channels {
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    ask_rx: Option<AskReceiver>,
    question_tx: Option<QuestionSender>,
    question_rx: Option<QuestionReceiver>,
    plan_tx: Option<PlanSwitchSender>,
    plan_rx: Option<PlanSwitchReceiver>,
    bg_store: Option<BackgroundStore>,
    lifecycle_rx: Option<LifecycleReceiver>,
    #[cfg(feature = "lsp")]
    lsp_manager: Option<std::sync::Arc<LspManager>>,
}

fn resolve_mode(cli: &cli::Cli, cfg: &config::Config) -> SecurityMode {
    if cli.yolo || cfg.yolo.unwrap_or(false) {
        SecurityMode::Yolo
    } else if cli.accept_all || cfg.accept_all.unwrap_or(false) {
        SecurityMode::Accept
    } else if cli.restrictive || cfg.restrictive.unwrap_or(false) {
        SecurityMode::Restrictive
    } else if let Some(m) = &cfg.default_permission_mode {
        match m.as_str() {
            "yolo" => SecurityMode::Yolo,
            "accept" => SecurityMode::Accept,
            "restrictive" => SecurityMode::Restrictive,
            _ => SecurityMode::Standard,
        }
    } else {
        SecurityMode::Standard
    }
}

fn build_channels(cli: &cli::Cli, cfg: &config::Config) -> Channels {
    if cli.resolve_no_tools(cfg) {
        return Channels::default();
    }

    let perm_config: PermissionConfig = cfg
        .permission
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let mode = resolve_mode(cli, cfg);
    let checker = PermissionChecker::new(&perm_config, mode, None);
    let perm: PermCheck = std::sync::Arc::new(std::sync::Mutex::new(checker));

    let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(64);
    let (question_tx, question_rx) = tokio::sync::mpsc::channel(64);
    let (plan_tx, plan_rx) = tokio::sync::mpsc::channel(64);
    let (lifecycle_tx, lifecycle_rx) = tokio::sync::mpsc::unbounded_channel();
    let bg_store = BackgroundStore::with_ui_sink(lifecycle_tx);

    #[cfg(feature = "lsp")]
    let lsp_manager = if cli.resolve_lsp_enabled(cfg) {
        let worktree = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let commands = compile_lsp_commands(cfg);
        let spawner = std::sync::Arc::new(ProcessSpawner::new(commands));
        Some(std::sync::Arc::new(LspManager::new(spawner, worktree)))
    } else {
        None
    };

    Channels {
        permission: Some(perm),
        ask_tx: Some(ask_tx),
        ask_rx: Some(ask_rx),
        question_tx: Some(question_tx),
        question_rx: Some(question_rx),
        plan_tx: Some(plan_tx),
        plan_rx: Some(plan_rx),
        bg_store: Some(bg_store),
        lifecycle_rx: Some(lifecycle_rx),
        #[cfg(feature = "lsp")]
        lsp_manager,
    }
}

/// Compile the spawn commands by starting from `ProcessSpawner::default_commands`
/// and applying per-server overrides from user config. A `disabled = true`
/// override removes the entry; any non-empty `command` replaces the default.
///
/// Known limitation: `extensions` on the override is currently ignored. The
/// claimed-extensions list lives in the static `builtin_servers()` registry
/// (`lsp/server.rs`) — making it instance-overridable requires plumbing a
/// per-session server set down through `LspManager`. Follow-up; users who
/// need new extensions today must edit `server.rs`.
#[cfg(feature = "lsp")]
fn compile_lsp_commands(cfg: &config::Config) -> std::collections::HashMap<String, ProcessCommand> {
    let mut commands = ProcessSpawner::default_commands();
    let Some(lsp_cfg) = &cfg.lsp else {
        return commands;
    };
    for (id, override_cfg) in lsp_cfg.server_overrides() {
        if override_cfg.disabled.unwrap_or(false) {
            commands.remove(id);
            continue;
        }
        let existing = commands.remove(id);
        let (program, args) = if let Some(cmd) = &override_cfg.command {
            if cmd.is_empty() {
                // User passed an empty command — fall back to the default.
                match &existing {
                    Some(e) => (e.program.clone(), e.args.clone()),
                    None => continue,
                }
            } else {
                (
                    std::path::PathBuf::from(&cmd[0]),
                    cmd.iter().skip(1).cloned().collect(),
                )
            }
        } else {
            match &existing {
                Some(e) => (e.program.clone(), e.args.clone()),
                None => continue, // unknown server, no default, no command
            }
        };
        let env = override_cfg
            .env
            .as_ref()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let init_options = override_cfg
            .initialization
            .clone()
            .unwrap_or(serde_json::Value::Null);
        commands.insert(
            id.clone(),
            ProcessCommand {
                program,
                args,
                env,
                init_options,
            },
        );
    }
    commands
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,rig=off")),
        )
        .init();

    let cli = cli::Cli::parse();
    let cfg = config::load();
    // Initialize the global UI theme before any rendering happens. The
    // theme is global state; setting it once at boot keeps every
    // render site from having to thread it explicitly.
    ui::theme::init(cfg.theme.as_deref().unwrap_or("phosphor"));
    let mut context = context::load(cli.resolve_no_context_files(&cfg));

    let default_prompt = cfg.default_prompt.as_deref().unwrap_or("code");
    if let Some(content) = context.prompts.get(default_prompt) {
        context.current_prompt = Some(content.clone());
        context.current_prompt_name = Some(default_prompt.to_string());
    }

    let provider = cli.resolve_provider(&cfg);
    let model = if cli.model.is_none() && cfg.model.is_none() {
        CompactString::new(provider::default_model_for(&provider))
    } else {
        cli.resolve_model(&cfg)
    };

    let mut session = session::Session::new(&provider, &model, cfg.resolve_context_window());

    if cli.resume && cli.session.is_none() && !cli.continue_session {
        let sessions = session::storage::find_recent_sessions(10)?;
        if sessions.is_empty() {
            eprintln!("No recent sessions found.");
        } else {
            eprintln!("Recent sessions:");
            for (i, s) in sessions.iter().enumerate() {
                let preview = s
                    .messages
                    .last()
                    .map(|m| {
                        let truncated: String = m.content.chars().take(60).collect();
                        truncated
                    })
                    .unwrap_or_default();
                eprintln!(
                    "  {}. {}  [{} msgs] {}",
                    i + 1,
                    &s.id[..8],
                    s.messages.len(),
                    preview
                );
            }
            if let Some(s) = sessions.into_iter().next() {
                session = s;
            }
        }
    }

    if cli.continue_session
        && cli.session.is_none()
        && let Ok(sessions) = session::storage::find_recent_sessions(1)
        && let Some(s) = sessions.into_iter().next()
    {
        session = s;
    }

    if let Some(session_id) = &cli.session {
        session = session::storage::load_session(session_id)?;
    }

    let client = provider::create_client(
        &provider,
        cli.api_key.as_deref(),
        &cfg.custom_providers_map(),
    )?;

    #[cfg(feature = "mcp")]
    let mcp_manager = if let Some(servers) = &cfg.mcp_servers {
        if !cli.resolve_no_tools(&cfg) {
            Some(extras::mcp::McpClientManager::connect_all(servers).await)
        } else {
            None
        }
    } else {
        None
    };

    #[cfg(feature = "semantic")]
    let semantic_manager = if !cli.resolve_no_tools(&cfg) {
        Some(semantic::SemanticManager::new())
    } else {
        None
    };

    #[cfg(feature = "plugin")]
    let plugin_manager = match plugin::PluginManager::try_new() {
        Ok(pm) => Some(std::sync::Arc::new(std::sync::Mutex::new(pm))),
        Err(e) => {
            eprintln!("warning: plugin support disabled ({e})");
            None
        }
    };

    // Make the PluginManager visible to HookedToolDyn (which runs inside
    // rig's tool dispatch, where we can't easily plumb the Arc through).
    // Set once, before any tool is built or called.
    #[cfg(feature = "plugin")]
    if let Some(pm_arc) = plugin_manager.as_ref() {
        plugin::hook::init_global(pm_arc.clone());
    }

    // Pull the dialog-request receiver out of the PluginManager once,
    // here, so we can hand it to the UI loop. After this point, calling
    // take_dialog_rx again returns None — single owner by design. Always
    // an Option so the UI signature is uniform across feature flags.
    #[cfg(feature = "plugin")]
    let dialog_rx = plugin_manager.as_ref().and_then(|pm| {
        pm.lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_dialog_rx()
    });
    #[cfg(not(feature = "plugin"))]
    let dialog_rx: Option<tokio::sync::mpsc::UnboundedReceiver<plugin::DialogRequest>> = None;

    #[cfg(feature = "plugin")]
    if let Some(pm_arc) = plugin_manager.as_ref() {
        use std::path::PathBuf;
        let candidate_dirs: Vec<PathBuf> = vec![
            dirs::home_dir()
                .unwrap_or_default()
                .join(".config")
                .join("dirge")
                .join("plugins"),
            PathBuf::from(".dirge").join("plugins"),
        ];
        // Silently drop missing default dirs; only surface real errors below.
        let search_dirs = plugin::filter_existing_dirs(&candidate_dirs);

        for dir in &search_dirs {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("warning: cannot read plugin dir {}: {}", dir.display(), e);
                    continue;
                }
            };

            for entry in entries.flatten() {
                let path = entry.path();
                // A plugin is either:
                //   - a single `.janet` file (legacy)
                //   - a directory whose name is the plugin id and whose
                //     `*.janet` contents are concatenated into one Janet
                //     env (multi-file plugins)
                let is_janet_file =
                    path.is_file() && path.extension().map_or(false, |e| e == "janet");
                let is_plugin_dir = path.is_dir();
                if !is_janet_file && !is_plugin_dir {
                    continue;
                }
                eprintln!("loading plugin: {}", path.display());
                let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
                match plugin::load_plugin(&mut mgr, &path) {
                    Ok(loaded) => {
                        if loaded.files.len() > 1 {
                            eprintln!(
                                "  loaded {} files from plugin '{}'",
                                loaded.files.len(),
                                loaded.stem,
                            );
                        }
                        for hook in &loaded.hooks_registered {
                            eprintln!("  registered hook: {} -> {}-{}", hook, loaded.stem, hook);
                        }
                    }
                    Err(e) => {
                        eprintln!("warning: failed to load plugin {}: {}", path.display(), e);
                    }
                }
            }
        }

        // After all plugins have loaded, harvest the providers each
        // registered via `harness/register-provider` and install them
        // into the global provider resolver. Config-declared
        // custom_providers still take precedence on name collision.
        let plugin_providers: std::collections::HashMap<String, config::CustomProviderConfig> = {
            let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
            mgr.list_providers()
                .into_iter()
                .map(|(name, ptype, base_url, api_key_env)| {
                    (
                        name,
                        config::CustomProviderConfig {
                            provider_type: ptype,
                            base_url,
                            api_key_env,
                        },
                    )
                })
                .collect()
        };
        if !plugin_providers.is_empty() {
            let n = provider::install_plugin_providers(plugin_providers);
            eprintln!("  registered {} plugin provider(s)", n);
        }
    }

    #[cfg(feature = "acp")]
    if cli.acp_enabled {
        return extras::acp::serve(cli, cfg, context).await;
    }

    let sandbox = sandbox::Sandbox::new(cli.resolve_sandbox(&cfg));
    let Channels {
        permission,
        ask_tx,
        ask_rx,
        question_tx,
        question_rx,
        plan_tx,
        plan_rx,
        bg_store,
        lifecycle_rx,
        #[cfg(feature = "lsp")]
        lsp_manager,
    } = build_channels(&cli, &cfg);

    if let Some(perm) = &permission {
        let allowlist: Vec<(String, String)> = session
            .permission_allowlist
            .iter()
            .map(|e| (e.tool.clone(), e.pattern.clone()))
            .collect();
        perm.lock()
            .unwrap_or_else(|e| e.into_inner())
            .load_session_allowlist(&allowlist);
    }

    let completion_model = client.completion_model(model.to_string());

    if cli.print {
        let agent = provider::build_agent(
            completion_model,
            &cli,
            &cfg,
            &context,
            permission,
            ask_tx,
            question_tx.clone(),
            plan_tx.clone(),
            bg_store.clone(),
            #[cfg(feature = "lsp")]
            lsp_manager.clone(),
            sandbox.clone(),
            #[cfg(feature = "mcp")]
            mcp_manager.as_ref(),
            #[cfg(feature = "semantic")]
            semantic_manager.as_ref(),
        )
        .await;
        let msg = cli.message.join(" ");
        let response = agent
            .run_print(&msg, cli.resolve_max_agent_turns(&cfg))
            .await?;
        if !cli.no_session {
            session.add_message(MessageRole::User, &msg);
            session.add_message(MessageRole::Assistant, &response);
            session::storage::save_session(&session)?;
        }
    } else {
        #[cfg(feature = "loop")]
        if cli.loop_mode {
            let model = client.completion_model(model.to_string());
            let agent = provider::build_agent(
                model,
                &cli,
                &cfg,
                &context,
                permission,
                ask_tx,
                question_tx.clone(),
                plan_tx.clone(),
                bg_store.clone(),
                #[cfg(feature = "lsp")]
                lsp_manager.clone(),
                sandbox.clone(),
                #[cfg(feature = "mcp")]
                mcp_manager.as_ref(),
                #[cfg(feature = "semantic")]
                semantic_manager.as_ref(),
            )
            .await;
            return run_headless_loop(agent, &cli, &cfg, &context).await;
        }

        let agent = provider::build_agent(
            completion_model,
            &cli,
            &cfg,
            &context,
            permission.clone(),
            ask_tx.clone(),
            question_tx.clone(),
            plan_tx.clone(),
            bg_store.clone(),
            #[cfg(feature = "lsp")]
            lsp_manager.clone(),
            sandbox.clone(),
            #[cfg(feature = "mcp")]
            mcp_manager.as_ref(),
            #[cfg(feature = "semantic")]
            semantic_manager.as_ref(),
        )
        .await;

        #[cfg(feature = "plugin")]
        if let Some(pm_arc) = plugin_manager.as_ref() {
            use crate::plugin::escape_janet_string;
            let cwd = std::env::current_dir()
                .unwrap_or_else(|_| ".".into())
                .display()
                .to_string();
            let mut pm = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = pm.dispatch(
                "on-init",
                &format!(
                    "@{{:model \"{}\" :cwd \"{}\" :provider \"{}\"}}",
                    escape_janet_string(&model),
                    escape_janet_string(&cwd),
                    escape_janet_string(&provider),
                ),
            ) {
                eprintln!("warning: plugin on-init dispatch failed: {e}");
            }
        }

        if !cli.resolve_no_tools(&cfg)
            && let Some(perm) = &permission
        {
            let mode = resolve_mode(&cli, &cfg);
            perm.lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_mode(mode);
        }

        let initial_msg = cli.message.join(" ");
        if !initial_msg.is_empty() {
            session.add_message(MessageRole::User, &initial_msg);
        }
        ui::run_interactive(
            client,
            agent,
            &cli,
            &cfg,
            &mut session,
            &mut context,
            permission,
            ask_tx,
            ask_rx,
            question_rx,
            plan_rx,
            question_tx,
            plan_tx,
            bg_store,
            lifecycle_rx,
            #[cfg(feature = "lsp")]
            lsp_manager,
            sandbox,
            #[cfg(feature = "mcp")]
            mcp_manager.as_ref(),
            #[cfg(feature = "semantic")]
            semantic_manager.as_ref(),
            #[cfg(feature = "plugin")]
            plugin_manager.as_ref(),
            dialog_rx,
        )
        .await?;
    }

    #[cfg(feature = "mcp")]
    if let Some(mgr) = mcp_manager {
        mgr.shutdown().await;
    }

    Ok(())
}

#[cfg(feature = "loop")]
async fn run_headless_loop(
    agent: crate::provider::AnyAgent,
    cli: &cli::Cli,
    cfg: &config::Config,
    _context: &context::ContextFiles,
) -> anyhow::Result<()> {
    use std::path::PathBuf;
    use uuid::Uuid;

    use crate::extras::r#loop as loop_mod;

    let prompt = cli
        .loop_prompt
        .clone()
        .or_else(|| {
            let msg = cli.message.join(" ");
            if msg.is_empty() { None } else { Some(msg) }
        })
        .ok_or_else(|| anyhow::anyhow!("No loop prompt. Use --loop-prompt or pass a message."))?;

    let plan_file = cli
        .loop_plan
        .clone()
        .unwrap_or_else(|| PathBuf::from("LOOP_PLAN.md"));
    let max_iterations = cli.loop_max;
    let run_cmd = cli.loop_run.clone();
    let session_id = Uuid::new_v4().to_string();

    let use_existing = loop_mod::plan::handle_startup(&plan_file)?;
    if !use_existing {
        // No plan exists — agent will generate one on first iteration
    }

    let mut state = loop_mod::LoopState::new(prompt, plan_file, max_iterations, run_cmd);

    loop {
        state.iteration += 1;

        if state.should_stop() {
            eprintln!(
                "[loop] max iterations ({}) reached, stopping",
                state.iteration
            );
            break;
        }

        let iteration_prompt = state.build_prompt();

        eprintln!("=== {} ===", state.iteration_label());
        eprintln!();

        let response = match agent
            .run_print(&iteration_prompt, cli.resolve_max_agent_turns(cfg))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[loop] error in iteration {}: {}", state.iteration, e);
                break;
            }
        };

        let summary: String = response.chars().take(300).collect();
        state.last_summary = Some(summary.clone());

        let validation_output = if let Some(cmd) = &state.run_cmd {
            eprintln!("--- Validation: {} ---", cmd);
            let shell = if cfg!(windows) { "powershell" } else { "sh" };
            let shell_arg = if cfg!(windows) { "-Command" } else { "-c" };
            match tokio::process::Command::new(shell)
                .arg(shell_arg)
                .arg(cmd)
                .output()
                .await
            {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    let combined = if stderr.is_empty() {
                        stdout
                    } else {
                        format!("{}\n{}", stdout, stderr)
                    };
                    eprintln!("{}", combined);
                    Some(combined)
                }
                Err(e) => {
                    let msg = format!("error: {}", e);
                    eprintln!("{}", msg);
                    Some(msg)
                }
            }
        } else {
            None
        };
        state.last_run_output = validation_output.clone();

        if let Err(e) = loop_mod::transcript::save_iteration(
            &session_id,
            state.iteration,
            &iteration_prompt,
            &response,
            validation_output.as_deref(),
            &summary,
        ) {
            eprintln!("[loop] warning: failed to save transcript: {}", e);
        }

        eprintln!("--- iteration {} complete, looping ---\n", state.iteration);
    }

    Ok(())
}
