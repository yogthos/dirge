//! Miscellaneous / smaller slash command handlers:
//! /mcp, /btw, /cd, /panel, /display, /quit, /help, /allow, /loop.

use crossterm::style::Color;

use super::{SlashCtx, c_agent, c_error, c_result};
use crate::ui::events::render_session;
use crate::ui::theme;

#[cfg(feature = "mcp")]
pub(super) async fn cmd_mcp(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let Some(mgr) = ctx.mcp_manager else {
        ctx.renderer
            .write_line("no MCP servers configured", c_agent())?;
        return Ok(());
    };
    let connections = mgr.connections_snapshot();
    if connections.is_empty() {
        ctx.renderer
            .write_line("no MCP servers connected", c_agent())?;
    } else if parts.len() == 1 {
        ctx.renderer.write_line("MCP servers:", c_agent())?;
        for (server_name, conn) in &connections {
            match crate::extras::mcp::client::list_tools(conn).await {
                Ok(tools) => {
                    ctx.renderer.write_line(
                        &format!("  {} ({} tools)", server_name, tools.len()),
                        c_result(),
                    )?;
                }
                Err(e) => {
                    ctx.renderer
                        .write_line(&format!("  {} (error: {})", server_name, e), c_error())?;
                }
            }
        }
    } else {
        let name = parts[1].trim();
        if let Some(conn) = connections.iter().find(|(n, _)| n == name).map(|(_, c)| c) {
            match crate::extras::mcp::client::list_tools(conn).await {
                Ok(tools) => {
                    if tools.is_empty() {
                        ctx.renderer
                            .write_line(&format!("server '{}' has no tools", name), c_agent())?;
                    } else {
                        ctx.renderer
                            .write_line(&format!("tools on '{}':", name), c_agent())?;
                        for tool in &tools {
                            let desc = tool.description.as_deref().unwrap_or("");
                            ctx.renderer
                                .write_line(&format!("  {}  {}", tool.name, desc), c_result())?;
                        }
                    }
                }
                Err(e) => {
                    ctx.renderer.write_line(
                        &format!("error listing tools on '{}': {}", name, e),
                        c_error(),
                    )?;
                }
            }
        } else {
            ctx.renderer
                .write_line(&format!("unknown MCP server: '{}'", name), c_error())?;
        }
    }
    Ok(())
}

/// dirge-781c: `/kill <id-prefix>` — abort a running subagent.
///
/// Resolves `id-prefix` against the in-flight subagent registry
/// (populated by `TaskTool::call` at spawn time). On a unique match
/// triggers the subagent's `AbortSignal`; the subagent's task tool
/// observes it within ~100ms and emits `SubagentChatEvent::Aborted`,
/// which the UI renders as `(aborted)` in the matching chat tab.
///
/// Usage echoes back one of:
///   - `killed <full-id>` — success.
///   - `no subagent matches <prefix>` — nothing in flight.
///   - `ambiguous prefix: <id1> <id2> ...` — supply more chars.
pub(super) async fn cmd_kill(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    use crate::agent::tools::task::{KillOutcome, kill_subagent};
    let prefix = parts.get(1).copied().unwrap_or("").trim();
    if prefix.is_empty() {
        ctx.renderer
            .write_line("usage: /kill <id-prefix>", c_error())?;
        return Ok(());
    }
    match kill_subagent(prefix) {
        KillOutcome::Killed(id) => {
            ctx.renderer
                .write_line(&format!("killed {}", id), c_agent())?;
        }
        KillOutcome::NotFound => {
            ctx.renderer
                .write_line(&format!("no subagent matches '{}'", prefix), c_error())?;
        }
        KillOutcome::Ambiguous(ids) => {
            ctx.renderer.write_line(
                &format!("ambiguous prefix '{}'; matches: {}", prefix, ids.join(" "),),
                c_error(),
            )?;
        }
    }
    Ok(())
}

pub(super) async fn cmd_btw(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let query = parts.get(1..).map(|p| p.join(" ")).unwrap_or_default();
    if query.is_empty() {
        ctx.renderer
            .write_line("usage: /btw <question>", c_error())?;
    } else {
        let model = ctx.client.completion_model(ctx.session.model.to_string());
        ctx.renderer
            .write_line(&format!("btw: {}", query), Color::DarkGrey)?;
        match model.btw_query(query).await {
            Ok(response) => {
                ctx.renderer.write_line("", Color::White)?;
                let max_width = ctx.renderer.line_width();
                let styled =
                    crate::ui::markdown::markdown_to_styled(&response, max_width, c_agent());
                for span in styled {
                    ctx.renderer.write(&span.text, span.color)?;
                }
                ctx.renderer.write_line("", Color::White)?;
            }
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("btw error: {}", e), c_error())?;
            }
        }
    }
    Ok(())
}

pub(super) async fn cmd_cd(ctx: &mut SlashCtx<'_>, text: &str) -> anyhow::Result<()> {
    let raw_args = text.trim().strip_prefix("/cd").unwrap_or("").trim();
    let target = raw_args;
    let path = if target.is_empty() {
        dirs::home_dir().unwrap_or_default()
    } else if let Some(rest) = target.strip_prefix('~') {
        let mut home = dirs::home_dir().unwrap_or_default();
        home.push(rest.trim_start_matches('/'));
        home
    } else {
        std::path::PathBuf::from(target)
    };
    match std::env::set_current_dir(&path) {
        Ok(()) => {
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            ctx.session.working_dir = compact_str::CompactString::new(canonical.to_string_lossy());
            if let Some(perm) = ctx.permission
                && let Ok(mut guard) = perm.lock()
            {
                guard.set_working_dir(&ctx.session.working_dir);
            }
            ctx.context.reload();
            let model = ctx.client.completion_model(ctx.session.model.to_string());
            *ctx.agent = crate::provider::build_agent(
                model,
                ctx.cli,
                ctx.cfg,
                ctx.context,
                ctx.permission.clone(),
                ctx.ask_tx.clone(),
                ctx.question_tx.clone(),
                ctx.plan_tx.clone(),
                ctx.bg_store.clone(),
                #[cfg(feature = "lsp")]
                ctx.lsp_manager.cloned(),
                ctx.sandbox.clone(),
                #[cfg(feature = "mcp")]
                ctx.mcp_manager,
                #[cfg(feature = "semantic")]
                ctx.semantic_manager,
                Some(ctx.session.id.to_string()),
            )
            .await;
            render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
            ctx.renderer.write_line(
                &format!("changed directory to {}", ctx.session.working_dir),
                c_agent(),
            )?;
        }
        Err(e) => {
            ctx.renderer.write_line(&format!("cd: {}", e), c_error())?;
        }
    }
    Ok(())
}

pub(super) async fn cmd_panel(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    use crate::ui::renderer::PanelMode;
    let arg = parts.get(1).copied().unwrap_or("").trim();
    let new_mode = match arg {
        "" => None,
        "on" => Some(PanelMode::On),
        "off" => Some(PanelMode::Off),
        "auto" => Some(PanelMode::Auto),
        "debug" => Some(PanelMode::Debug),
        other => {
            ctx.renderer.write_line(
                &format!("unknown /panel mode '{}' (use on|off|auto|debug)", other),
                c_error(),
            )?;
            return Ok(());
        }
    };
    if let Some(mode) = new_mode {
        if mode == PanelMode::Debug {
            ctx.renderer.set_right_panel_mode(mode);
        } else {
            ctx.renderer.set_panel_mode(mode);
        }
        ctx.renderer.render_viewport()?;
    }
    // Both sides share a mode after /panel; report the left as the
    // representative. For per-side control, see /display.
    let current = ctx.renderer.left_panel_mode();
    let left = ctx.renderer.left_panel_visible();
    let right = ctx.renderer.right_panel_visible();
    ctx.renderer.write_line(
        &format!(
            "panel mode: {:?} (left {}, right {}). Use /display for per-pane control.",
            current,
            if left { "shown" } else { "hidden" },
            if right { "shown" } else { "hidden" },
        ),
        c_agent(),
    )?;
    Ok(())
}

/// `/display <panes>` — choose which panes are visible. `panes` is a
/// `|`/`,`/space-separated subset of `left`, `main`, `right` (e.g.
/// `/display left|main|right`, `/display main`, `/display main|right`).
/// The main conversation pane is always shown; this toggles the left and
/// right side panels independently. With no argument, reports the current
/// layout.
pub(super) async fn cmd_display(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    use crate::ui::renderer::parse_display_spec;

    // Rejoin everything after the command so both `/display main|right`
    // (one token) and `/display main right` (several) parse the same way.
    let spec = parts[1..].join(" ");
    if spec.trim().is_empty() {
        let left = ctx.renderer.left_panel_visible();
        let right = ctx.renderer.right_panel_visible();
        let mut shown = vec!["main"];
        if left {
            shown.insert(0, "left");
        }
        if right {
            shown.push("right");
        }
        ctx.renderer.write_line(
            &format!(
                "display: {} (usage: /display left|main|right)",
                shown.join("|")
            ),
            c_agent(),
        )?;
        return Ok(());
    }

    match parse_display_spec(&spec) {
        Ok(vis) => {
            ctx.renderer.set_pane_visibility(vis);
            ctx.renderer.render_viewport()?;
            let mut shown = vec!["main"];
            if vis.left {
                shown.insert(0, "left");
            }
            if vis.right {
                shown.push("right");
            }
            ctx.renderer
                .write_line(&format!("display: {}", shown.join("|")), c_agent())?;
        }
        Err(msg) => {
            ctx.renderer.write_line(&msg, c_error())?;
        }
    }
    Ok(())
}

pub(super) async fn cmd_quit(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    *ctx.is_running = false;
    Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "quit").into())
}

/// `/why <tool> [input]` — explain how the permission engine would
/// decide a tool call: the final effect, the deciding policy, and
/// every applicable policy's vote. Dry-run (no side effects).
pub(super) async fn cmd_why(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let perm = match ctx.permission {
        Some(p) => p,
        None => {
            ctx.renderer.write_line(
                "permission system unavailable (--no-tools mode?)",
                c_error(),
            )?;
            return Ok(());
        }
    };
    let Some(tool) = parts.get(1).copied() else {
        ctx.renderer.write_line(
            "usage: /why <tool> [input]   e.g. /why bash cargo test   ·   /why write src/main.rs",
            c_error(),
        )?;
        return Ok(());
    };
    let input = parts.get(2..).map(|s| s.join(" ")).unwrap_or_default();
    let is_path = crate::permission::engine::is_path_tool_name(tool);
    let report = {
        let guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        guard.explain(tool, &input, is_path)
    };
    for line in report.lines() {
        ctx.renderer.write_line(line, c_result())?;
    }
    Ok(())
}

pub(super) async fn cmd_allow(
    ctx: &mut SlashCtx<'_>,
    parts: &[&str],
    text: &str,
) -> anyhow::Result<()> {
    let sub = parts.get(1).copied().unwrap_or("list");
    let perm = match ctx.permission {
        Some(p) => p,
        None => {
            ctx.renderer.write_line(
                "permission system unavailable (--no-tools mode?)",
                c_error(),
            )?;
            return Ok(());
        }
    };
    match sub {
        "list" => {
            let entries = {
                let guard = perm.lock().unwrap_or_else(|e| e.into_inner());
                guard.allowlist_entries()
            };
            if entries.is_empty() {
                ctx.renderer.write_line(
                    "session allowlist is empty (use '(a) allow always' in a permission prompt to add entries)",
                    c_agent(),
                )?;
            } else {
                ctx.renderer.write_line(
                    &format!("session allowlist ({} entries):", entries.len()),
                    c_agent(),
                )?;
                for (i, (tool, pat)) in entries.iter().enumerate() {
                    ctx.renderer
                        .write_line(&format!("  [{}] {} {}", i, tool, pat), c_result())?;
                }
                ctx.renderer.write_line(
                    "use '/allow remove <idx>' to drop a single entry; '/allow clear' to drop all",
                    theme::dim(),
                )?;
            }
        }
        "add" => {
            let raw_args = text.trim().strip_prefix("/allow").unwrap_or("").trim();
            let rest = raw_args.strip_prefix("add").unwrap_or("").trim();
            let mut it = rest.splitn(2, char::is_whitespace);
            let tool = it.next().unwrap_or("");
            let pattern = it.next().unwrap_or("").trim();
            const KNOWN_PERM_TOOLS: &[&str] = &[
                "bash",
                "read",
                "write",
                "edit",
                "grep",
                "find_files",
                "glob",
                "list_dir",
                "write_todo_list",
                "apply_patch",
                "lsp",
                "question",
                "webfetch",
                "websearch",
                "task",
                "task_status",
                "memory",
                "skill",
                "list_symbols",
                "get_symbol_body",
                "find_definition",
                "find_callers",
                "find_callees",
                "mcp_tool",
            ];
            if tool.is_empty() || pattern.is_empty() {
                ctx.renderer.write_line(
                    "usage: /allow add <tool> <pattern>  (e.g. /allow add bash 'cargo *')",
                    c_error(),
                )?;
            } else if !KNOWN_PERM_TOOLS.contains(&tool) {
                ctx.renderer.write_line(
                    &format!(
                        "unknown tool {:?}. Valid: {}",
                        tool,
                        KNOWN_PERM_TOOLS.join(", "),
                    ),
                    c_error(),
                )?;
            } else {
                {
                    let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
                    guard.add_session_allowlist(tool.to_string(), pattern);
                }
                let entry = crate::session::PermissionAllowEntry {
                    tool: tool.to_string(),
                    pattern: pattern.to_string(),
                };
                if !ctx
                    .session
                    .permission_allowlist
                    .iter()
                    .any(|e| e.tool == entry.tool && e.pattern == entry.pattern)
                {
                    ctx.session.permission_allowlist.push(entry);
                }
                ctx.renderer
                    .write_line(&format!("added: {} {}", tool, pattern), c_agent())?;
            }
        }
        "remove" => {
            let idx_str = parts.get(2).copied().unwrap_or("");
            let idx: usize = match idx_str.parse() {
                Ok(n) => n,
                Err(_) => {
                    ctx.renderer.write_line(
                        "usage: /allow remove <idx>  (run /allow list to see indices)",
                        c_error(),
                    )?;
                    return Ok(());
                }
            };
            let removed = {
                let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
                guard.remove_session_allowlist_at(idx)
            };
            match removed {
                Some((tool, pat)) => {
                    ctx.session
                        .permission_allowlist
                        .retain(|e| !(e.tool == tool && e.pattern == pat));
                    ctx.renderer
                        .write_line(&format!("removed [{}]: {} {}", idx, tool, pat), c_agent())?;
                }
                None => {
                    ctx.renderer
                        .write_line(&format!("no allowlist entry at index {}", idx), c_error())?;
                }
            }
        }
        "clear" => {
            {
                let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
                guard.clear_session_allowlist();
            }
            ctx.session.permission_allowlist.clear();
            ctx.renderer
                .write_line("session allowlist cleared", c_agent())?;
        }
        other => {
            ctx.renderer.write_line(
                &format!(
                    "unknown /allow subcommand {:?}; try: list, add, remove, clear",
                    other,
                ),
                c_error(),
            )?;
        }
    }
    Ok(())
}

pub(super) async fn cmd_loop(
    ctx: &mut SlashCtx<'_>,
    parts: &[&str],
    text: &str,
) -> anyhow::Result<()> {
    #[cfg(feature = "loop")]
    {
        if parts.len() < 2 || (parts.len() >= 2 && parts[1] == "status") {
            if let Some(ls) = ctx.loop_state.as_ref() {
                let status = if ls.active { "active" } else { "stopped" };
                ctx.renderer.write_line(
                    &format!(
                        "loop {}: {} ({})",
                        status,
                        ls.iteration_label(),
                        ls.plan_file.display()
                    ),
                    c_agent(),
                )?;
            } else {
                ctx.renderer.write_line("no active loop", c_agent())?;
                ctx.renderer
                    .write_line("usage: /loop <prompt>  |  /loop stop", c_result())?;
            }
        } else if parts[1] == "stop" {
            if let Some(ls) = ctx.loop_state.as_mut() {
                ls.active = false;
                ctx.renderer.write_line("loop stopped", c_agent())?;
            } else {
                ctx.renderer.write_line("no active loop", c_agent())?;
            }
        } else {
            let after = text.trim().strip_prefix("/loop").unwrap_or("").trim_start();
            let tokens: Vec<&str> = after.split_whitespace().collect();
            let mut max_iterations: Option<u32> = Some(20); // default cap
            let mut prompt_tokens: Vec<&str> = Vec::new();
            let mut i = 0;
            while i < tokens.len() {
                if tokens[i] == "--max" && i + 1 < tokens.len() {
                    match tokens[i + 1].parse::<u32>() {
                        Ok(0) => max_iterations = None,
                        Ok(n) => max_iterations = Some(n),
                        Err(_) => {
                            ctx.renderer.write_line(
                                &format!(
                                    "invalid --max value: {} (use a positive integer, or 0 for unbounded)",
                                    tokens[i + 1]
                                ),
                                c_error(),
                            )?;
                            return Ok(());
                        }
                    }
                    i += 2;
                } else {
                    prompt_tokens.push(tokens[i]);
                    i += 1;
                }
            }
            let prompt = prompt_tokens.join(" ");
            if prompt.is_empty() {
                ctx.renderer.write_line(
                    "usage: /loop [--max N] <prompt>  (default cap: 20 iterations; --max 0 = unbounded)",
                    c_error(),
                )?;
                return Ok(());
            }
            let plan_file = std::path::PathBuf::from("LOOP_PLAN.md");
            let ls = crate::extras::r#loop::LoopState::new(prompt, plan_file, max_iterations, None);
            *ctx.loop_state = Some(ls);
            let cap_msg = match max_iterations {
                Some(n) => format!("loop started (max {n} iterations) — iteration 1 will run after this message"),
                None => "loop started (unbounded — use /loop stop to cancel) — iteration 1 will run after this message".to_string(),
            };
            ctx.renderer.write_line(&cap_msg, c_agent())?;
        }
    }
    #[cfg(not(feature = "loop"))]
    {
        let _ = (parts, text);
        ctx.renderer.write_line(
            "/loop requires the 'loop' feature: cargo build --features loop",
            c_error(),
        )?;
    }
    Ok(())
}

pub(super) async fn cmd_help(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let renderer = &mut *ctx.renderer;
    renderer.write_line("commands:", c_agent())?;
    renderer.write_line("  /model [name]          show or switch model", c_result())?;
    renderer.write_line("  /sessions              list recent sessions", c_result())?;
    renderer.write_line(
        "  /sessions <id>         load a session (by ID prefix)",
        c_result(),
    )?;
    renderer.write_line("  /sessions delete <id>  delete a session", c_result())?;
    renderer.write_line(
        "  /reasoning             toggle reasoning visibility",
        c_result(),
    )?;
    renderer.write_line(
        "  /mode                  show/change security mode",
        c_result(),
    )?;
    renderer.write_line(
        "  /mode <mode>           set mode (standard|restrictive|accept|yolo)",
        c_result(),
    )?;
    #[cfg(feature = "mcp")]
    {
        let _ = renderer.write_line(
            "  /mcp                   list MCP servers and tools",
            c_result(),
        );
        let _ = renderer.write_line(
            "  /mcp <server>          list tools of an MCP server",
            c_result(),
        );
    }
    renderer.write_line(
        "  /clear                 clear screen + reset tree",
        c_result(),
    )?;
    renderer.write_line(
        "  /tree                  show the session tree (use /tree <id-prefix> to switch branches)",
        c_result(),
    )?;
    renderer.write_line(
        "  /fork [id-prefix]      branch off at the chosen message (default: last user message)",
        c_result(),
    )?;
    renderer.write_line(
        "  /clone <id-prefix>     switch to the branch ending at the chosen entry",
        c_result(),
    )?;
    renderer.write_line(
        "  /panel [on|off|auto|debug]   toggle right-hand info panel",
        c_result(),
    )?;
    renderer.write_line(
        "  /display <panes>       choose panes: left|main|right (e.g. main|right)",
        c_result(),
    )?;
    renderer.write_line(
        "  /cd [path]             change working directory",
        c_result(),
    )?;
    renderer.write_line(
        "  /btw <question>        ask a quick question (no tools, doesn't affect session)",
        c_result(),
    )?;
    renderer.write_line("  /undo                  undo last exchange", c_result())?;
    renderer.write_line("  /retry                 retry last prompt", c_result())?;
    renderer.write_line(
        "  /compress [/compact]   compress conversation history",
        c_result(),
    )?;
    renderer.write_line(
        "  /compress [focus]      compress; focus text guides what to preserve",
        c_result(),
    )?;
    renderer.write_line(
        "  /toggle <feat> [on|off] toggle a feature (e.g. /toggle todo)",
        c_result(),
    )?;
    renderer.write_line(
        "  /allow list            list session allowlist entries",
        c_result(),
    )?;
    renderer.write_line(
        "  /allow add <tool> <pat> add an allowlist entry",
        c_result(),
    )?;
    renderer.write_line(
        "  /allow remove <idx>    drop one allowlist entry",
        c_result(),
    )?;
    renderer.write_line(
        "  /allow clear           drop all allowlist entries",
        c_result(),
    )?;
    #[cfg(feature = "loop")]
    {
        let _ = renderer.write_line(
            "  /loop [prompt]         start iterative coding loop",
            c_result(),
        );
        let _ = renderer.write_line("  /loop stop             stop the loop", c_result());
    }
    #[cfg(not(feature = "loop"))]
    {
        let _ = renderer.write_line(
            "  /loop [prompt]         start iterative coding loop (req. 'loop' feature)",
            c_result(),
        );
    }
    renderer.write_line(
        "  /prompt                list available prompts (plan|code)",
        c_result(),
    )?;
    renderer.write_line(
        "  /prompt <name>         activate a prompt by name",
        c_result(),
    )?;
    renderer.write_line(
        "  /prompt default        activate the 'default' prompt if installed,",
        c_result(),
    )?;
    renderer.write_line(
        "                         otherwise clears the active prompt",
        c_result(),
    )?;
    renderer.write_line(
        "  /regen-prompts        restore built-in prompts to global dir",
        c_result(),
    )?;
    #[cfg(feature = "git-worktree")]
    {
        let _ = renderer.write_line(
            "  /worktree <name>       create a git worktree on <name> branch and cd into it",
            c_result(),
        );
        let _ = renderer.write_line(
            "  /wt-merge [branch]     merge worktree branch into [branch] (default: main/master)",
            c_result(),
        );
        let _ = renderer.write_line(
            "  /wt-exit               exit worktree and return to main repo",
            c_result(),
        );
    }
    renderer.write_line("  /quit                  exit dirge", c_result())?;
    renderer.write_line("  /help                  show this message", c_result())?;
    renderer.write_line("", c_agent())?;
    renderer.write_line("keys:", c_agent())?;
    renderer.write_line("  PgUp/PgDn / wheel      scroll chat history", c_result())?;
    renderer.write_line("  Home/End               jump to top/bottom", c_result())?;
    renderer.write_line(
        "  @<query>               file picker (Tab/Enter select, Esc cancel)",
        c_result(),
    )?;
    renderer.write_line(
        "  drag                  select text; mouse-up copies to clipboard",
        c_result(),
    )?;
    renderer.write_line("  Ctrl+R                 toggle reasoning", c_result())?;
    renderer.write_line("  Ctrl+C / Ctrl+D / Esc  interrupt/quit", c_result())?;
    renderer.write_line(
        "  Ctrl+N / Ctrl+P        next / previous chat (subagent windows)",
        c_result(),
    )?;
    renderer.write_line("  Ctrl+X                 close chat window", c_result())?;
    renderer.write_line(
        "  Ctrl+K                 kill subagent on focused tab",
        c_result(),
    )?;
    renderer.write_line(
        "  Ctrl+O                 expand collapsed tool result",
        c_result(),
    )?;
    renderer.write_line(
        "  Esc-Esc (idle)         open rewind picker (truncate history)",
        c_result(),
    )?;
    renderer.write_line(
        "  ! / !! cmd             run shell command (visible / invisible)",
        c_result(),
    )?;
    renderer.write_line(
        "  (type while agent runs to queue a follow-up message)",
        c_result(),
    )?;

    #[cfg(feature = "plugin")]
    if let Some(pm_arc) = crate::plugin::hook::global() {
        let cmds = {
            let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
            mgr.list_commands()
        };
        if !cmds.is_empty() {
            renderer.write_line("", c_agent())?;
            renderer.write_line("plugin commands:", c_agent())?;
            for (cmd, handler) in cmds {
                renderer.write_line(&format!("  /{:<20} -> {}", cmd, handler), c_result())?;
            }
        }
    }
    Ok(())
}
