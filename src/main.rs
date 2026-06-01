mod agent;
mod cli;
mod config;
mod context;
mod event;
mod extras;
mod fs_atomic;
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
mod text;
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
use crate::ui::ansi::{self, StripPolicy};

/// Per-session channels and shared state, threaded through the agent build
/// chain in place of a ten-position tuple. Cloneable senders + shared state
/// (`bg_store`, `permission`) survive being moved through
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
}

/// Resolve the session id passed to `build_agent` for the
/// session-search tool's current-session exclusion. dirge-sk3e:
/// `--no-session` (one-shot prompts that aren't persisted) yields
/// `None` so the tool doesn't try to exclude a row that will never
/// land in the session DB. Otherwise the live session id is
/// returned so the model can't recall its own in-progress
/// prompt-response pair.
fn session_id_for_agent(cli: &cli::Cli, session: &session::Session) -> Option<String> {
    if cli.no_session {
        None
    } else {
        Some(session.id.to_string())
    }
}

fn resolve_mode(cli: &cli::Cli, cfg: &config::Config) -> SecurityMode {
    // Warn on conflicting CLI flags. Previously `--yolo --restrictive`
    // silently picked yolo (the first-match in the if-else chain)
    // without surfacing the conflict — the user thought they had
    // restricted permissions and got the opposite. Emit a stderr
    // warning naming the active mode so the user can correct it.
    let cli_modes: &[(bool, &str)] = &[
        (cli.yolo, "--yolo"),
        (cli.accept_all, "--accept-all"),
        (cli.restrictive, "--restrictive"),
    ];
    let cli_picks: Vec<&str> = cli_modes
        .iter()
        .filter(|(v, _)| *v)
        .map(|(_, name)| *name)
        .collect();
    if cli_picks.len() > 1 {
        eprintln!(
            "warning: conflicting permission flags {:?}; using the most permissive ({}). \
             Pass only one of --yolo / --accept-all / --restrictive.",
            cli_picks, cli_picks[0],
        );
    }

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
            "standard" => SecurityMode::Standard,
            other => {
                // Unknown value silently mapped to Standard before
                // this — a typo like `restritctive` ended up as
                // Standard and the user never knew. Warn explicitly
                // and name the valid values.
                eprintln!(
                    "warning: unknown default_permission_mode {:?} in config; using standard. \
                     Valid values: yolo, accept, restrictive, standard.",
                    other,
                );
                SecurityMode::Standard
            }
        }
    } else {
        SecurityMode::Standard
    }
}

fn build_channels(cli: &cli::Cli, cfg: &config::Config) -> Channels {
    if cli.resolve_no_tools(cfg) {
        return Channels::default();
    }

    let perm_config: PermissionConfig = match cfg.permission.as_ref() {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(c) => c,
            // Surface the error loudly: falling back to defaults silently
            // would drop the user's intended rules (and harden nothing).
            Err(e) => {
                eprintln!(
                    "warning: invalid `permission` config ({e}); falling back to \
                     defaults (all actions Ask). Fix the config to restore your rules."
                );
                PermissionConfig::default()
            }
        },
        None => PermissionConfig::default(),
    };

    let mode = resolve_mode(cli, cfg);
    let checker = PermissionChecker::new(&perm_config, mode, None);
    // Audit H11: Yolo mode unconditionally returns Allowed BEFORE rule
    // lookup, so any explicit Deny rule the user configured (for
    // `rm -rf /`, `aws *`, an `external_directory` deny, etc.) is
    // silently inert. Warn once at startup so the user understands
    // the implication of their config; we don't change the behavior
    // (Yolo is documented as "all rules off") but the warning makes
    // the gap visible instead of hidden.
    if mode == SecurityMode::Yolo {
        let n = checker.deny_rule_count();
        if n > 0 {
            eprintln!(
                "warning: Yolo mode is active and your config has {} deny rule(s) — those rules will be IGNORED. Yolo allows every tool call unconditionally. Remove --yolo (or `yolo = true` in config) to honor deny rules.",
                n,
            );
        }
    }
    let perm: PermCheck = std::sync::Arc::new(std::sync::Mutex::new(checker));

    let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(64);
    let (question_tx, question_rx) = tokio::sync::mpsc::channel(64);
    let (plan_tx, plan_rx) = tokio::sync::mpsc::channel(64);
    let (lifecycle_tx, lifecycle_rx) = tokio::sync::mpsc::unbounded_channel();
    let bg_store = BackgroundStore::with_ui_sink(lifecycle_tx);

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
    }
}

/// Construct the `LspManager` (if LSP is enabled). Built standalone —
/// rather than inside `build_channels` — so the host can wire the plugin
/// LSP responder to it BEFORE plugins are loaded. A plugin that queries
/// `harness/lsp` at load time would otherwise deadlock against a
/// not-yet-spawned drainer. Returns `None` when tools are disabled
/// (`--no-tools`) or LSP is turned off in config/CLI.
#[cfg(feature = "lsp")]
fn build_lsp_manager(cli: &cli::Cli, cfg: &config::Config) -> Option<std::sync::Arc<LspManager>> {
    if cli.resolve_no_tools(cfg) || !cli.resolve_lsp_enabled(cfg) {
        return None;
    }
    let worktree = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let commands = compile_lsp_commands(cfg);
    let spawner = std::sync::Arc::new(ProcessSpawner::new(commands));
    // Apply per-server config overrides (extensions, disabled).
    // Without this, user config like
    //   "lsp": { "rust": { "extensions": ["rs", "rlib"] } }
    // was silently ignored — the manager always used the builtin list.
    let mut servers = crate::lsp::server::builtin_servers();
    if let Some(lsp_cfg) = &cfg.lsp {
        crate::lsp::server::apply_extension_overrides(&mut servers, lsp_cfg.server_overrides());
    }
    Some(std::sync::Arc::new(LspManager::with_servers(
        spawner, worktree, servers,
    )))
}

/// Compile the spawn commands by starting from `ProcessSpawner::default_commands`
/// and applying per-server overrides from user config. A `disabled = true`
/// override removes the entry; any non-empty `command` replaces the default.
///
/// Extensions overrides are applied separately in `build_channels` via
/// `lsp::server::apply_extension_overrides` since they live on the
/// per-session `ServerInfo` registry, not on the spawn-command map.
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

/// SESS-8: print a stderr warning when resuming a session whose
/// working_dir differs from the current cwd or whose `updated_at`
/// is more than 24h old. Tool results captured during the original
/// session may no longer match reality; without a visible signal
/// the agent confidently acts on stale `git status` / `read`
/// content. Warnings only — never refuse to load.
fn warn_on_stale_resume(session: &session::Session) {
    let cwd = std::env::current_dir().ok();
    let session_wd = session.working_dir.as_str();
    if !session_wd.is_empty()
        && let Some(cwd) = &cwd
        && cwd.to_string_lossy() != session_wd
    {
        eprintln!(
            "warning: resumed session was created in {:?}, current cwd is {:?}. Tool results captured against the old tree may be stale.",
            session_wd,
            cwd.display().to_string(),
        );
    }
    if let Ok(updated) = chrono::DateTime::parse_from_rfc3339(session.updated_at.as_str()) {
        let age = chrono::Utc::now().signed_duration_since(updated.with_timezone(&chrono::Utc));
        if age.num_hours() >= 24 {
            eprintln!(
                "warning: resumed session is {} hours old. Captured tool results (read/git/bash) may no longer reflect the current state of the working tree.",
                age.num_hours(),
            );
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    // Install the off-stream notification channel EARLY so MCP
    // stderr forwarders spawning during `connect_all` (later in
    // main, before run_interactive) have a live sender to push
    // into. Without this, lines fired during MCP-server handshake
    // hit `sender() = None` and were silently dropped — exactly
    // the regression review #1 caught.
    ui::notifications::install();

    // Tracing filter precedence: RUST_LOG (always wins) > --verbose
    // (debug for dirge + warn for plugin hooks) > default
    // (warn, rig silenced). `--verbose` exists primarily so plugin
    // authors can see hook-error logs without having to know the
    // RUST_LOG syntax.
    //
    // Log destination: opt-in only. By default, tracing output is
    // dropped (`io::sink`) and the fd-isolation redirect in
    // `TerminalGuard` sends stdout/stderr to `/dev/null` — no file
    // is created on disk. When the user opts in via `--verbose`,
    // `RUST_LOG=...`, or `DIRGE_LOG=path`, the file at
    // `$XDG_STATE_HOME/dirge/dirge.log` (or `$DIRGE_LOG` if set
    // explicitly) becomes the target for BOTH tracing events AND
    // the stdout/stderr redirect, so plugin authors can see
    // everything in one place.
    let default_filter = if cli.verbose {
        "dirge=debug,dirge::plugin=warn,rig=off"
    } else {
        "warn,rig=off"
    };
    let want_log_file = cli.verbose
        || std::env::var_os("RUST_LOG").is_some()
        || std::env::var_os("DIRGE_LOG").is_some();
    let log_path_opt: Option<std::path::PathBuf> = if want_log_file {
        let path = std::env::var_os("DIRGE_LOG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("XDG_STATE_HOME")
                    .map(std::path::PathBuf::from)
                    .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
                    .map(|base| base.join("dirge"))
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                    .join("dirge.log")
            });
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new("/tmp")));
        Some(path)
    } else {
        None
    };
    // Publish the chosen path (or absence) for TerminalGuard's
    // stdout/stderr fd redirect — both sinks need to agree.
    if let Some(ref p) = log_path_opt {
        ui::terminal::set_log_path(Some(p.clone()));
    } else {
        ui::terminal::set_log_path(None);
    }
    let log_writer: Box<dyn std::io::Write + Send + Sync> = match log_path_opt.as_ref() {
        Some(path) => match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(f) => Box::new(f),
            // File couldn't be opened (read-only fs, etc.) — drop
            // trace output rather than spam the TUI.
            Err(_) => Box::new(std::io::sink()),
        },
        // No log file requested — discard tracing events.
        None => Box::new(std::io::sink()),
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        .with_writer(std::sync::Mutex::new(log_writer))
        .with_ansi(false)
        .init();
    let cfg = config::load();
    // Initialize the global UI theme before any rendering happens. The
    // theme is global state; setting it once at boot keeps every
    // render site from having to thread it explicitly.
    ui::theme::init(cfg.theme.as_deref().unwrap_or("phosphor"));
    let mut context = context::load(cli.resolve_no_context_files(&cfg));

    let default_prompt = cli
        .prompt
        .as_deref()
        .unwrap_or(cfg.default_prompt.as_deref().unwrap_or("code"));
    if let Some(p) = context.prompts.get(default_prompt) {
        context.current_prompt = Some(p.body.clone());
        context.current_prompt_name = Some(default_prompt.to_string());
        context.current_prompt_deny_tools = p.deny_tools.clone();
    }

    let provider = cli.resolve_provider(&cfg);
    let config_model = cfg
        .resolve_role(config::ConfigRole::Default)
        .and_then(|(_, e)| e.model);
    let model = if cli.model.is_none() && config_model.is_none() {
        // dirge-j3jd: resolve the alias's provider TYPE so a custom alias
        // doesn't fall back to the OpenRouter default model id.
        CompactString::new(provider::default_model_for_alias(
            &provider,
            &cfg.providers_map(),
        ))
    } else {
        cli.resolve_model(&cfg)
    };

    let mut session = session::Session::new(
        &provider,
        &model,
        cfg.resolve_context_window(model.as_str()),
    );

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
        // Try exact id first; fall back to prefix match so the CLI
        // is as forgiving as the interactive `/sessions <prefix>`
        // command. Ambiguous prefix surfaces a list of matching ids.
        match session::storage::load_session(session_id) {
            Ok(s) => session = s,
            Err(_) => {
                let matches = session::storage::find_sessions_by_prefix(session_id)?;
                match matches.len() {
                    0 => anyhow::bail!("no session matching id or prefix {:?}", session_id),
                    1 => session = matches.into_iter().next().expect("len == 1"),
                    n => {
                        let ids: Vec<String> =
                            matches.iter().take(5).map(|s| s.id.to_string()).collect();
                        anyhow::bail!(
                            "session prefix {:?} matches {} sessions: {} — pass a longer prefix",
                            session_id,
                            n,
                            ids.join(", "),
                        );
                    }
                }
            }
        }
        // SESS-8: warn when resuming a session whose working_dir
        // differs from the current cwd or whose updated_at is
        // stale (>24h). Tool results stored in the session were
        // captured against that earlier state; resuming silently
        // can lead the agent to act on outdated `git status` /
        // `read` output that no longer matches reality.
        warn_on_stale_resume(&session);
    }

    // Restore the active prompt from the loaded session so resumed
    // sessions don't silently snap back to the default `code` prompt.
    // Default-prompt initialization above set context.current_prompt
    // to `code`; we override it here if the session carries a name.
    // If the persisted name no longer resolves (user uninstalled the
    // prompt), warn so the silent fallback doesn't surprise them.
    if let Some(name) = session.current_prompt_name.clone() {
        match context.prompts.get(&name) {
            Some(p) => {
                context.current_prompt = Some(p.body.clone());
                context.current_prompt_name = Some(name);
                context.current_prompt_deny_tools = p.deny_tools.clone();
            }
            None => {
                eprintln!(
                    "warning: session was using prompt {:?} but it's no longer available — falling back to default ({:?}).",
                    name,
                    context.current_prompt_name.as_deref().unwrap_or("none"),
                );
            }
        }
    }

    // Plugin loading must happen BEFORE `create_client` so plugin-
    // registered providers (via `harness/register-provider`) are
    // installed into `PLUGIN_PROVIDERS` before
    // `resolve_provider_info` runs. Previously plugins loaded later,
    // so `--provider <plugin-name>` failed with "Unknown provider"
    // even though the plugin defined it.
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
    let mut dialog_rx = plugin_manager.as_ref().and_then(|pm| {
        pm.lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_dialog_rx()
    });
    #[cfg(not(feature = "plugin"))]
    let dialog_rx: Option<tokio::sync::mpsc::UnboundedReceiver<plugin::DialogRequest>> = None;
    // Headless modes (--print, --loop) have no UI to render plugin
    // dialogs. Without a drain, `harness/confirm` / `harness/select`
    // calls in plugin code block the worker thread forever waiting
    // on a reply. When `--auto-confirm` is set, spawn a background
    // task that consumes `dialog_rx` and answers synthetically. The
    // task lives until `dialog_rx` closes (worker shutdown). If
    // `--auto-confirm` is omitted, dialog_rx stays attached to the
    // UI path (interactive) or is intentionally left undrained
    // (headless without --auto-confirm — same behaviour as before).
    #[cfg(feature = "plugin")]
    let _dialog_responder: Option<tokio::task::JoinHandle<()>> = {
        let headless = cli.print || {
            #[cfg(feature = "loop")]
            {
                cli.loop_mode
            }
            #[cfg(not(feature = "loop"))]
            {
                false
            }
        };
        match (headless, cli.auto_confirm, dialog_rx.take()) {
            (true, Some(mode), Some(rx)) => Some(plugin::spawn_headless_dialog_responder(rx, mode)),
            (_, _, taken) => {
                // Put it back — interactive path still needs it.
                dialog_rx = taken;
                None
            }
        }
    };

    // Build the LSP manager and wire the plugin LSP responder to it BEFORE
    // loading plugins. The responder drains `harness/lsp` requests from the
    // worker thread; if a plugin queries LSP at load time, the drainer must
    // already be running or the worker would block forever on a reply that
    // never comes (and main couldn't reach a later spawn point). When LSP is
    // disabled (`lsp_manager` is None) we deliberately do NOT spawn it and
    // drop the receiver, so `harness/lsp?` reports the bridge as not live
    // and queries return nil instead of hanging.
    #[cfg(feature = "lsp")]
    let lsp_manager = build_lsp_manager(&cli, &cfg);
    #[cfg(all(feature = "plugin", feature = "lsp"))]
    let _lsp_responder: Option<tokio::task::JoinHandle<()>> = {
        let lsp_rx = plugin_manager
            .as_ref()
            .and_then(|pm| pm.lock().unwrap_or_else(|e| e.into_inner()).take_lsp_rx());
        match (lsp_rx, lsp_manager.clone()) {
            (Some(rx), Some(mgr)) => Some(plugin::spawn_lsp_responder(rx, mgr)),
            // No manager (LSP off) or no receiver: drop `lsp_rx` here so the
            // worker's sender sees a closed channel.
            _ => None,
        }
    };

    #[cfg(feature = "plugin")]
    if let Some(pm_arc) = plugin_manager.as_ref() {
        use std::path::PathBuf;
        // Honor DIRGE_CONFIG_DIR via the shared base, like config.json
        // (dirge-f8oe) — previously this hard-coded ~/.config/dirge, so
        // an override moved config but left plugins behind.
        let candidate_dirs: Vec<PathBuf> = vec![
            crate::session::storage::config_path().join("plugins"),
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
                    path.is_file() && path.extension().is_some_and(|e| e == "janet");
                let is_plugin_dir = path.is_dir();
                if !is_janet_file && !is_plugin_dir {
                    continue;
                }
                // dirge-99ic: a plugin's config-key is its directory name
                // (dir plugin) or `.janet` file stem (single-file). Honor
                // `plugins.<name>.enabled` (default true) and pass
                // `auto_start` to the plugin via harness-plugin-config.
                let plugin_name = if is_plugin_dir {
                    path.file_name()
                } else {
                    path.file_stem()
                }
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
                if !cfg.plugin_enabled(&plugin_name) {
                    if cli.verbose {
                        eprintln!("skipping disabled plugin: {plugin_name}");
                    }
                    continue;
                }
                if cli.verbose {
                    eprintln!("loading plugin: {}", path.display());
                }
                let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
                mgr.set_loading_plugin_config(true, cfg.plugin_auto_start(&plugin_name));
                match plugin::load_plugin(&mut mgr, &path) {
                    Ok(loaded) => {
                        if cli.verbose && loaded.files.len() > 1 {
                            eprintln!(
                                "  loaded {} files from plugin '{}'",
                                loaded.files.len(),
                                loaded.stem,
                            );
                        }
                        if cli.verbose {
                            for hook in &loaded.hooks_registered {
                                eprintln!(
                                    "  registered hook: {} -> {}-{}",
                                    hook, loaded.stem, hook
                                );
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("warning: failed to load plugin {}: {}", path.display(), e);
                    }
                }
                // Don't let this plugin's config leak into the next one.
                mgr.clear_loading_plugin_config();
            }
        }

        // After all plugins have loaded, harvest the providers each
        // registered via `harness/register-provider` and install them
        // into the global provider resolver. Config-declared
        // custom_providers still take precedence on name collision.
        let plugin_providers: std::collections::HashMap<String, config::ProviderEntry> = {
            let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
            mgr.list_providers()
                .into_iter()
                .map(|(name, ptype, base_url, api_key_env)| {
                    (
                        name,
                        config::ProviderEntry {
                            provider_type: Some(ptype),
                            base_url: Some(base_url),
                            model: None,
                            api_key_env,
                            // Plugin-registered providers don't expose
                            // a chunk-timeout knob via the
                            // `harness/register-provider` API; they
                            // inherit the top-level default
                            // (`stream_chunk_timeout_secs` or 300s).
                            stream_chunk_timeout_secs: None,
                            // PROV-1: plugin-registered providers
                            // can't opt into HTTP. If a plugin
                            // declares a non-https base_url the
                            // validator in `install_plugin_providers`
                            // will reject it.
                            allow_insecure: false,
                            // `harness/register-provider` doesn't expose
                            // a literal api_key or options map — plugins
                            // declare the env var name and the request
                            // builder reads options from cfg/CLI.
                            api_key: None,
                            options: None,
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

    // Audit C2: resolve `--api-key-file` / `--api-key-stdin` before
    // falling back to `--api-key`. The flag-based key is still
    // accepted for backward compat but the explicit warning in
    // `resolve_api_key` fires when it's used. Mutually-exclusive
    // checks: stdin OR file, never both.
    if cli.api_key_stdin && cli.api_key_file.is_some() {
        anyhow::bail!("--api-key-stdin and --api-key-file are mutually exclusive");
    }
    if cli.api_key.is_some() && !cli.api_key.as_deref().unwrap_or("").is_empty() {
        eprintln!(
            "warning: --api-key value is visible in process listings (/proc/*/cmdline, `ps`). Prefer --api-key-file <path>, --api-key-stdin, or the provider's env var."
        );
    }
    let resolved_key: Option<String> = if let Some(path) = &cli.api_key_file {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("--api-key-file: read {:?}: {}", path, e))?;
        let key = raw.trim().to_string();
        if key.is_empty() {
            anyhow::bail!("--api-key-file: file {:?} is empty after trimming", path);
        }
        Some(key)
    } else if cli.api_key_stdin {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| anyhow::anyhow!("--api-key-stdin: read: {}", e))?;
        let key = buf.trim().to_string();
        if key.is_empty() {
            anyhow::bail!("--api-key-stdin: received empty input");
        }
        Some(key)
    } else {
        cli.api_key.clone()
    };

    let client = provider::create_client(&provider, resolved_key.as_deref(), &cfg.providers_map())?;

    // MCP connection. `connect_all` can take seconds — an `npx -y <pkg>`
    // cold start, ×N servers — so blocking on it before the UI draws was
    // the dominant time-to-first-frame cost. The non-interactive paths
    // (--print / --loop) build their agent up front and genuinely need
    // the tools synchronously, so they still connect here. The
    // interactive TUI instead DEFERS: it builds the agent WITHOUT MCP
    // tools, draws immediately, and a background task spawned just before
    // `run_interactive` connects + injects the tools once ready
    // (dirge-x949). ACP / no-tools don't use MCP on this path.
    #[cfg(feature = "mcp")]
    let mcp_manager = if let Some(servers) = &cfg.mcp_servers {
        if !cli.resolve_no_tools(&cfg) && (cli.print || cli.loop_mode) {
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
    // Push the active prompt's `deny_tools` into the freshly-built
    // checker. `context.current_prompt_deny_tools` was populated by
    // the default-prompt + session-restore blocks above; this is the
    // first opportunity to wire it into the now-existing checker
    // (the checker is built inside `build_channels`).
    crate::permission::apply_prompt_deny(&permission, &context.current_prompt_deny_tools);

    // dirge-0g6i: wire optional LLM auto-approval. When `approval_provider`
    // is set, a permission prompt is judged by that model instead of the
    // human (the evaluator is global, read by the `enforce` chokepoint).
    // Opt-in — `resolve_role(Approval)` has no default fallback. A build
    // failure or unmatched alias leaves the human-prompt path intact.
    if cfg.approval_provider.is_some() {
        match cfg.resolve_role(config::ConfigRole::Approval) {
            Some((alias, entry)) => {
                match provider::build_approval_fn(&alias, &entry, &cfg.providers_map()) {
                    Ok(f) => {
                        if let Some(perm) = &permission {
                            perm.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .set_approval_fn(f);
                            eprintln!(
                                "info: approval_provider '{alias}' enabled — permission prompts will be auto-evaluated by the LLM"
                            );
                        }
                    }
                    Err(e) => eprintln!(
                        "warning: approval_provider '{alias}' configured but client build failed: {e}; falling back to human prompts"
                    ),
                }
            }
            None => eprintln!(
                "error: approval_provider is configured but does not match any entry in `providers` or any built-in; falling back to human prompts"
            ),
        }
    }

    let completion_model = client.completion_model(model.to_string());

    if cli.print {
        let session_id_for_print = session_id_for_agent(&cli, &session);
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
            session_id_for_print,
        )
        .await;
        let msg = cli.message.join(" ");
        let response = agent
            .run_print(&msg, cli.resolve_max_agent_turns(&cfg), cli.output_format)
            .await?;
        // A plugin may have called `harness/set-next-model` during
        // `prepare-next-run`. `--print` is single-shot so we can't
        // honor it — surface a warning to stderr so the plugin
        // author can see why their model swap didn't take effect.
        #[cfg(feature = "plugin")]
        if let Some(pm_arc) = plugin_manager.as_ref()
            && let Some(m) = pm_arc
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take_pending_next_model()
        {
            let t = m.trim();
            if !t.is_empty() {
                eprintln!(
                    "[plugin] prepare-next-run requested model={} — \
                        --print is single-shot; ignored. Use --loop or \
                        interactive mode for model swap.",
                    t
                );
            }
        }
        // dirge-bx4g + dirge-4tuq: --print is a one-shot. Fire
        // on_session_end on the persisted-or-not transcript so
        // plugin providers always see the boundary, then attempt
        // the save. Pre-fix the order was reversed and `?` on
        // save_session short-circuited past the hook — exactly
        // the scenario (disk failure / permission error) where
        // the provider's own backend may be the only durable
        // record. The hook itself is fire-and-forget; saves can
        // fail without losing the lifecycle signal.
        if !cli.no_session {
            session.add_message(MessageRole::User, &msg);
            session.add_message(MessageRole::Assistant, &response);
            crate::agent::review::maybe_fire_session_end(&agent, &session);
            if let Err(e) = session::storage::save_session(&session) {
                eprintln!("warning: failed to save session: {}", e);
            }
        }
        // Kill any detached background shells the run started so they don't
        // outlive this headless process (they're in their own process group).
        crate::agent::tools::bg_shell::global().kill_all();
        // dirge-x949: --print connects MCP synchronously (above), so shut
        // it down here. The interactive path instead hands its manager to
        // run_interactive, which owns the shutdown; the old shared
        // post-run shutdown is gone because `mcp_manager` is conditionally
        // moved into run_interactive and can't be named afterward.
        #[cfg(feature = "mcp")]
        if let Some(mgr) = mcp_manager {
            mgr.shutdown().await;
        }
    } else {
        #[cfg(feature = "loop")]
        if cli.loop_mode {
            use std::path::PathBuf;
            use uuid::Uuid;

            use crate::extras::r#loop as loop_mod;

            let loop_prompt = cli
                .loop_prompt
                .clone()
                .or_else(|| {
                    let msg = cli.message.join(" ");
                    if msg.is_empty() { None } else { Some(msg) }
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("No loop prompt. Use --loop-prompt or pass a message.")
                })?;

            let plan_file = cli
                .loop_plan
                .clone()
                .unwrap_or_else(|| PathBuf::from("LOOP_PLAN.md"));
            let _use_existing = loop_mod::plan::handle_startup(&plan_file)?;

            let mut loop_state = loop_mod::LoopState::new(
                loop_prompt,
                plan_file,
                cli.loop_max,
                cli.loop_run.clone(),
            );
            let session_id = Uuid::new_v4().to_string();

            // Build the initial agent; on plugin-requested model swap
            // we rebuild here and re-enter the inner iteration loop
            // with the same `loop_state` so iteration numbering and
            // transcript continuity are preserved across the swap.
            // `mut` is only needed when the plugin feature is enabled.
            #[cfg_attr(not(feature = "plugin"), allow(unused_mut))]
            let mut current_agent = {
                let m = client.completion_model(model.to_string());
                provider::build_agent(
                    m,
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
                    Some(session.id.to_string()),
                )
                .await
            };

            loop {
                let exit = run_headless_loop(
                    &current_agent,
                    &mut loop_state,
                    &session_id,
                    &cli,
                    &cfg,
                    #[cfg(feature = "plugin")]
                    plugin_manager.as_ref(),
                )
                .await?;
                match exit {
                    HeadlessLoopExit::MaxIterations => {
                        // dirge-jmc9: fire on_session_end before
                        // returning from --loop mode. session.messages
                        // is typically empty here (run_print doesn't
                        // populate it) but the hook still serves as a
                        // "flush buffered state" signal for plugin
                        // providers.
                        crate::agent::review::maybe_fire_session_end(&current_agent, &session);
                        crate::agent::tools::bg_shell::global().kill_all();
                        return Ok(());
                    }
                    #[cfg(feature = "plugin")]
                    HeadlessLoopExit::ModelSwap(new_model) => {
                        let m = client.completion_model(new_model);
                        current_agent = provider::build_agent(
                            m,
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
                            Some(session.id.to_string()),
                        )
                        .await;
                    }
                }
            }
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
            Some(session.id.to_string()),
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
        // Clone the LSP manager Arc so we can call didClose
        // cleanup after `run_interactive` has consumed its handle.
        // The Arc is cheap to clone; both copies point at the same
        // manager state, which is what we need for shutdown.
        #[cfg(feature = "lsp")]
        let lsp_manager_for_shutdown = lsp_manager.clone();

        // dirge-ov2 Phase D: subagent chat event channel. The
        // `task` tool emits Spawn / Complete / Failed events here
        // when it dispatches subagents; the UI loop receives them
        // and creates / writes to chat windows. Process-global
        // sink so the TaskTool doesn't need plumbing through the
        // 13-site builder pipeline.
        // dirge-02tn: bounded — display-only events, producers use
        // try_send (drop on overflow) so a runaway subagent can't grow
        // this channel without bound if the UI stalls.
        let (subagent_chat_tx, subagent_chat_rx) =
            tokio::sync::mpsc::channel::<crate::agent::tools::task::SubagentChatEvent>(
                crate::agent::tools::task::SUBAGENT_CHAT_CAP,
            );
        crate::agent::tools::task::set_subagent_chat_sink(subagent_chat_tx);

        // ui-redesign: spawn the system-load poller. The handle is
        // a cheap Arc; cloning into run_interactive lets the panel
        // painter read snapshots without crossing the channel.
        let sysload = crate::ui::sysload::spawn_poller(std::time::Duration::from_secs(2));

        // dirge-x949: background MCP loading. Connect the servers and
        // collect their tools OFF the critical path so the UI draws
        // immediately; the wrapped tools + the connected manager are sent
        // back to the select loop, which injects the tools into the live
        // agent (the next prompt's clone picks them up) and lights up the
        // MCP panel. `permission` / `ask_tx` are cloned here because
        // `run_interactive` consumes the originals just below.
        // The loader delivers the connected manager + wrapped tools on
        // `ready`, then pings the untyped `wake` channel — a
        // `tokio::select!` arm can't be `#[cfg]`-gated on the mcp-only
        // payload type, so the select loop wakes on `()` and drains the
        // payload in a cfg'd block.
        #[cfg(feature = "mcp")]
        let (mcp_ready_rx, mcp_wake_rx) = if !cli.resolve_no_tools(&cfg)
            && let Some(servers) = cfg.mcp_servers.clone()
        {
            let (ready_tx, ready_rx) = tokio::sync::mpsc::unbounded_channel();
            let (wake_tx, wake_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let perm = permission.clone();
            let ask = ask_tx.clone();
            tokio::spawn(async move {
                let mgr = extras::mcp::McpClientManager::connect_all(&servers).await;
                let mcp_tools = mgr.collect_tools(perm, ask).await;
                let wrapped = crate::agent::builder::wrap_mcp_tools(mcp_tools).await;
                // Deliver the payload, then nudge the UI loop to drain it.
                // If the receiver is gone (user quit before connect
                // finished) the send fails harmlessly and the manager
                // drops, killing the child processes.
                if ready_tx.send((mgr, wrapped)).is_ok() {
                    let _ = wake_tx.send(());
                }
            });
            (Some(ready_rx), Some(wake_rx))
        } else {
            (None, None)
        };
        #[cfg(not(feature = "mcp"))]
        let mcp_wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>> = None;

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
            mcp_manager,
            #[cfg(feature = "mcp")]
            mcp_ready_rx,
            mcp_wake_rx,
            #[cfg(feature = "semantic")]
            semantic_manager.as_ref(),
            #[cfg(feature = "plugin")]
            plugin_manager.as_ref(),
            dialog_rx,
            subagent_chat_rx,
            sysload,
        )
        .await?;

        // Best-effort `textDocument/didClose` for every file each
        // LSP server saw this session. Servers retain parse trees
        // + diagnostic caches keyed on open files; without this
        // cleanup a long session leaves them holding all that
        // state for the lifetime of their process. The notify is
        // fire-and-forget; individual failures are swallowed
        // inside `close_all_files`.
        #[cfg(feature = "lsp")]
        if let Some(mgr) = lsp_manager_for_shutdown.as_ref() {
            mgr.close_all_files().await;
        }
        // dirge-x949: MCP shutdown moved INTO run_interactive — for the
        // interactive path the connected manager is now owned there
        // (delivered by the background loader), so it shuts the servers
        // down before returning. The --print / --loop paths return earlier
        // and let their MCP children die on process exit, as before.
    }

    Ok(())
}

/// How a single run of [`run_headless_loop`] ended.
///
/// `MaxIterations` is the normal terminal state (or non-recoverable
/// iteration error). `ModelSwap` is only returned when a plugin
/// called `harness/set-next-model` from `prepare-next-run` — the
/// caller is expected to rebuild the agent with the requested model
/// and re-invoke `run_headless_loop` with the same mutable state /
/// session id so iteration counting and the transcript continue
/// seamlessly across the swap.
#[cfg(feature = "loop")]
enum HeadlessLoopExit {
    MaxIterations,
    #[cfg(feature = "plugin")]
    ModelSwap(String),
}

#[cfg(feature = "loop")]
async fn run_headless_loop(
    agent: &crate::provider::AnyAgent,
    state: &mut crate::extras::r#loop::LoopState,
    session_id: &str,
    cli: &cli::Cli,
    cfg: &config::Config,
    #[cfg(feature = "plugin")] plugin_manager: Option<
        &std::sync::Arc<std::sync::Mutex<plugin::PluginManager>>,
    >,
) -> anyhow::Result<HeadlessLoopExit> {
    use crate::extras::r#loop as loop_mod;

    loop {
        state.iteration += 1;

        if state.should_stop() {
            eprintln!(
                "[loop] max iterations ({}) reached, stopping",
                state.iteration
            );
            return Ok(HeadlessLoopExit::MaxIterations);
        }

        let iteration_prompt = state.build_prompt();

        eprintln!("=== {} ===", state.iteration_label());
        eprintln!();

        let response = match agent
            .run_print(
                &iteration_prompt,
                cli.resolve_max_agent_turns(cfg),
                cli.output_format,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[loop] error in iteration {}: {}", state.iteration, e);
                return Ok(HeadlessLoopExit::MaxIterations);
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
                    let safe = ansi::strip_controls(&combined, StripPolicy::KEEP_NEWLINE);
                    eprintln!("{safe}");
                    Some(safe)
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
            session_id,
            state.iteration,
            &iteration_prompt,
            &response,
            validation_output.as_deref(),
            &summary,
        ) {
            eprintln!("[loop] warning: failed to save transcript: {}", e);
        }

        // `prepare-next-run` hooks fired inside `run_print` may have
        // set a `next_model` slot on the PluginManager. Drain it
        // BEFORE eprintln'ing "iteration complete" so the swap log
        // line lands in the right narrative slot.
        #[cfg(feature = "plugin")]
        if let Some(pm_arc) = plugin_manager {
            let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(raw) = mgr.take_pending_next_model() {
                let trimmed = raw.trim();
                if !trimmed.is_empty() {
                    let next_model = trimmed.to_string();
                    drop(mgr);
                    eprintln!(
                        "--- iteration {} complete, plugin requested model swap to '{}' ---\n",
                        state.iteration, next_model
                    );
                    return Ok(HeadlessLoopExit::ModelSwap(next_model));
                }
            }
        }

        eprintln!("--- iteration {} complete, looping ---\n", state.iteration);
    }
}

#[cfg(test)]
mod session_id_tests {
    use super::*;
    use clap::Parser;

    fn fresh_session() -> session::Session {
        session::Session::new("openrouter", "anthropic/claude-sonnet-4.5", 200_000)
    }

    /// dirge-sk3e — `--no-session` yields `None` so the
    /// session-search tool doesn't try to exclude an id that will
    /// never land in the DB.
    #[test]
    fn no_session_yields_none() {
        let cli = cli::Cli::parse_from(["dirge", "--no-session", "--print"]);
        let session = fresh_session();
        assert_eq!(
            session_id_for_agent(&cli, &session),
            None,
            "--no-session must yield None"
        );
    }

    /// Sessioned runs still exclude the live session so the model
    /// can't see its own in-progress turn in `session_search`.
    #[test]
    fn sessioned_print_yields_some() {
        let cli = cli::Cli::parse_from(["dirge", "--print"]);
        let session = fresh_session();
        let got = session_id_for_agent(&cli, &session);
        assert_eq!(
            got.as_deref(),
            Some(session.id.as_str()),
            "sessioned --print must propagate the live session id"
        );
    }

    /// Interactive (no --print, no --no-session) also gets Some.
    #[test]
    fn interactive_yields_some() {
        let cli = cli::Cli::parse_from(["dirge"]);
        let session = fresh_session();
        let got = session_id_for_agent(&cli, &session);
        assert_eq!(got.as_deref(), Some(session.id.as_str()));
    }
}
