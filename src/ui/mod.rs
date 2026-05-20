pub(crate) mod avatar;
mod events;
pub(crate) mod input;
mod markdown;
pub(crate) mod picker;
#[cfg(feature = "plugin")]
mod plugin_tree;
mod renderer;
mod slash;
mod status;
#[cfg(feature = "plugin")]
mod streaming;
mod terminal;
pub(crate) mod theme;
mod tree;

use std::collections::VecDeque;

use compact_str::CompactString;
use crossterm::event;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::style::Color;
use tokio::sync::mpsc;

use crate::agent::tools::plan::{
    PlanAction, PlanSwitchReceiver, PlanSwitchResponse, PlanSwitchSender,
};
use crate::agent::tools::question::{QuestionReceiver, QuestionResponse, QuestionSender};
use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
use crate::event::{AgentEvent, UserEvent};
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::{AskReceiver, AskSender, UserDecision};
use crate::permission::checker::PermCheck;
#[cfg(feature = "plugin")]
use crate::plugin::PluginManager;
use crate::provider::{AnyAgent, AnyClient};
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;
use crate::session::{MessageRole, PermissionAllowEntry, Session};
use crate::shell;
use crate::ui::events::{render_session, sanitize_output};
use crate::ui::input::InputEditor;
use crate::ui::picker::ListPicker;
use crate::ui::renderer::{LineEntry, Renderer, copy_to_clipboard};
use crate::ui::slash::{handle_compress, handle_slash};
use crate::ui::status::StatusLine;
use crate::ui::terminal::TerminalGuard;

// Themed color accessors. These wrap `theme::agent()` etc. so we can
// keep the existing call-site spelling (e.g. `c_agent()` is now a fn).
// Active palette is set at startup via `theme::init`.
#[inline]
fn c_agent() -> Color {
    theme::agent()
}
#[inline]
fn c_error() -> Color {
    theme::error()
}
#[inline]
fn c_tool() -> Color {
    theme::tool()
}
#[inline]
fn c_perm() -> Color {
    theme::perm()
}

/// Append a `q:N` queue-depth suffix to the status line when there are
/// interjections waiting to be sent to the agent. Hidden when the queue
/// is empty so the line doesn't gain noise during normal operation.
fn with_queue(s: String, n: usize) -> String {
    if n == 0 { s } else { format!("{} q:{}", s, n) }
}

/// Map a plugin-supplied color string ("cyan", "red", ...) to a
/// crossterm `Color`. Falls back to dim grey for anything unrecognized
/// so a typo in plugin code doesn't crash the UI.
#[cfg(feature = "plugin")]
fn parse_plugin_color(name: &str) -> Color {
    // Lowercase + strip a leading `:` so `:cyan`, `cyan`, `Cyan` all
    // map to the same crossterm color.
    let normalized = name.trim_start_matches(':').to_ascii_lowercase();
    match normalized.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "darkgrey" | "darkgray" | "grey" | "gray" => Color::DarkGrey,
        "darkred" => Color::DarkRed,
        "darkgreen" => Color::DarkGreen,
        "darkyellow" => Color::DarkYellow,
        "darkblue" => Color::DarkBlue,
        "darkmagenta" => Color::DarkMagenta,
        "darkcyan" => Color::DarkCyan,
        _ => Color::DarkGrey,
    }
}

/// Render one plugin entry to the chat. Looks up a registered
/// renderer for `entry.custom_type`; if found, invokes it and
/// renders the returned (color, text) lines. If not found (or the
/// renderer emitted nothing), falls back to a minimal default
/// rendering: a header line + the raw data string.
#[cfg(feature = "plugin")]
fn render_plugin_entry(
    pm_arc: &std::sync::Arc<std::sync::Mutex<PluginManager>>,
    renderer: &mut Renderer,
    entry: &crate::session::PluginEntry,
) -> std::io::Result<()> {
    let handler_name = {
        let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
        mgr.list_renderers()
            .into_iter()
            .find(|(t, _)| t == &entry.custom_type)
            .map(|(_, h)| h)
    };

    if let Some(handler) = handler_name {
        let lines = {
            let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
            mgr.invoke_renderer(&handler, &entry.data)
                .unwrap_or_default()
        };
        if !lines.is_empty() {
            for (color_name, text) in lines {
                let color = parse_plugin_color(&color_name);
                renderer.write_line(&sanitize_output(&text), color)?;
            }
            return Ok(());
        }
    }

    // Default rendering: identify the custom type and dump the data.
    // Keeps entries visible even when their plugin is uninstalled.
    renderer.write_line(&format!("[entry: {}]", entry.custom_type), theme::dim())?;
    if !entry.data.is_empty() {
        renderer.write_line(&format!("  {}", sanitize_output(&entry.data)), theme::dim())?;
    }
    Ok(())
}

/// Snapshot the various pieces of state the info panel surfaces (cwd, MCP,
/// LSP, todos, modified files) into a `PanelData` ready to hand to the
/// renderer. Reads global statics (TODO_LIST, MODIFIED_FILES) under their
/// own mutexes; safe to call from the UI loop tick.
fn build_panel_data(
    session: &Session,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "lsp")] lsp_manager: Option<&std::sync::Arc<crate::lsp::manager::LspManager>>,
) -> crate::ui::renderer::PanelData {
    use std::path::Path;

    let cwd_str = Path::new(session.working_dir.as_str())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(session.working_dir.as_str())
        .to_string();

    #[cfg(feature = "mcp")]
    let mcp: Vec<(String, bool)> = mcp_manager
        .map(|m| {
            m.handles
                .iter()
                .map(|h| (h.server_name.clone(), true))
                .collect()
        })
        .unwrap_or_default();
    #[cfg(not(feature = "mcp"))]
    let mcp: Vec<(String, bool)> = Vec::new();

    #[cfg(feature = "lsp")]
    let lsp: Vec<(String, String, bool)> = lsp_manager
        .map(|m| {
            let cwd_path = Path::new(session.working_dir.as_str());
            let shorten = |p: &Path| -> String {
                p.strip_prefix(cwd_path)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(String::from)
                            .unwrap_or_else(|| p.display().to_string())
                    })
            };
            let mut all = Vec::new();
            for (id, root) in m.active_servers() {
                all.push((id, shorten(&root), true));
            }
            for (id, root) in m.broken_servers() {
                all.push((id, shorten(&root), false));
            }
            all
        })
        .unwrap_or_default();
    #[cfg(not(feature = "lsp"))]
    let lsp: Vec<(String, String, bool)> = Vec::new();

    let todos: Vec<(String, String)> = {
        let list = crate::agent::tools::todo::TODO_LIST
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        list.iter()
            .take(8)
            .map(|t| {
                let status = match t.status.as_str() {
                    "in_progress" => "[~]",
                    "completed" => "[x]",
                    _ => "[ ]",
                };
                (status.to_string(), t.content.to_string())
            })
            .collect()
    };

    let cwd_path = Path::new(session.working_dir.as_str()).to_path_buf();
    let modified: Vec<String> = crate::agent::tools::modified::recent(8)
        .into_iter()
        .map(|p| {
            p.strip_prefix(&cwd_path)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(String::from)
                        .unwrap_or_else(|| p.display().to_string())
                })
        })
        .collect();

    crate::ui::renderer::PanelData {
        cwd: cwd_str,
        mcp,
        lsp,
        todos,
        modified,
    }
}

#[inline]
pub(crate) fn resolve_color(color: Color, monochrome: bool) -> Color {
    if monochrome {
        let _ = color;
        Color::Reset
    } else {
        color
    }
}

/// Flatten a multi-line / control-char-bearing string into one safe line
/// suitable for a single `write_line` call. Newlines, tabs, and ANSI escape
/// sequences would otherwise corrupt the renderer's per-line buffering — the
/// renderer splits on `\n` and writes raw bytes. Truncates to `max_chars`
/// characters and appends `…` when truncated.
fn sanitize_single_line(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars));
    let mut count = 0;
    for c in s.chars() {
        if count >= max_chars {
            out.push('…');
            return out;
        }
        let replacement = match c {
            '\n' | '\r' | '\t' => ' ',
            // ASCII control range; strip rather than render literally.
            c if c.is_control() => continue,
            // Skip ESC (0x1B) — start of ANSI sequences.
            '\u{001B}' => continue,
            c => c,
        };
        out.push(replacement);
        count += 1;
    }
    out
}

/// Formats a tool call showing only the primary file/command parameter.
/// - read/write/edit → path
/// - grep → pattern (and path if both present)
/// - find_files → pattern
/// - list_dir → path
/// - bash → command (truncated to 60 chars)
/// - others → first string arg or nothing
fn format_tool_call_summary(name: &str, args: &serde_json::Value) -> String {
    let obj = match args {
        serde_json::Value::Object(map) => map,
        _ => return name.to_string(),
    };

    // Determine which key(s) to show based on tool name
    let primary_keys: &[&str] = match name {
        "read" | "write" | "edit" | "list_dir" => &["path"],
        "grep" => &["pattern", "path"],
        "find_files" | "glob" => &["pattern"],
        "bash" => &["command"],
        "question" => &["questions"],
        "task" | "task_status" => &["prompt", "task_id"],
        "apply_patch" => &["operations"],
        _ => &[],
    };

    let mut shown = Vec::new();
    for key in primary_keys {
        if let Some(serde_json::Value::String(val)) = obj.get(*key) {
            let truncated = if val.len() > 60 {
                format!("\"{}...\"", &val[..57])
            } else {
                format!("\"{}\"", val)
            };
            shown.push(truncated);
        }
    }

    if shown.is_empty() {
        // fallback: show first string value if any
        if let Some((_, serde_json::Value::String(val))) = obj.iter().next() {
            let truncated = if val.len() > 60 {
                format!("\"{}...\"", &val[..57])
            } else {
                format!("\"{}\"", val)
            };
            format!("{} {}", name, truncated)
        } else {
            name.to_string()
        }
    } else {
        format!("{} {}", name, shown.join(" "))
    }
}

pub async fn run_interactive(
    client: AnyClient,
    mut agent: AnyAgent,
    cli: &Cli,
    cfg: &Config,
    session: &mut Session,
    context: &mut ContextFiles,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    mut ask_rx: Option<AskReceiver>,
    mut question_rx: Option<QuestionReceiver>,
    mut plan_rx: Option<PlanSwitchReceiver>,
    question_tx: Option<QuestionSender>,
    plan_tx: Option<PlanSwitchSender>,
    bg_store: Option<crate::agent::tools::background::BackgroundStore>,
    mut lifecycle_rx: Option<crate::agent::tools::background::LifecycleReceiver>,
    #[cfg(feature = "lsp")] lsp_manager: Option<std::sync::Arc<crate::lsp::manager::LspManager>>,
    sandbox: Sandbox,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    #[cfg(feature = "plugin")] plugin_manager: Option<
        &std::sync::Arc<std::sync::Mutex<PluginManager>>,
    >,
    // Consumer end of the Janet worker's dialog channel. None for
    // non-plugin builds (no worker, no channel). Always present as an
    // Option so the `tokio::select!` arm can be unconditional —
    // `tokio::select!` doesn't accept `cfg` attributes on its arms.
    mut dialog_rx: Option<tokio::sync::mpsc::UnboundedReceiver<crate::plugin::DialogRequest>>,
) -> anyhow::Result<()> {
    let _guard = TerminalGuard::new()?;

    let mut renderer = Renderer::new()?;
    renderer.set_monochrome(cli.no_color);
    let mut input = InputEditor::new();
    input.set_monochrome(cli.no_color);
    let mut is_running = false;
    // Plain-text messages typed while the agent is running are pushed here
    // instead of being rejected. When the current run finishes (and no plugin
    // or loop follow-up has claimed the next turn) the queue is drained as a
    // single concatenated user message — claude-code-style "type ahead".
    let mut interjection_queue: VecDeque<String> = VecDeque::new();
    let mut agent_rx: Option<mpsc::Receiver<AgentEvent>> = None;
    // Handle to the background agent task. Held alongside `agent_rx` so the
    // UI can abort in-flight work on Ctrl+C/D/Esc — otherwise tools keep
    // running and permission prompts arrive after the user has interrupted.
    let mut agent_abort: Option<tokio::task::JoinHandle<()>> = None;
    // Sender into the running agent's interjection channel. The UI signals
    // (unit-only payload) when a user-typed interjection is queued; the
    // runner honors it at the next tool-result boundary.
    let mut agent_interject: Option<mpsc::UnboundedSender<()>> = None;
    let mut agent_line_started = false;
    let mut response_buf = String::new();
    // Per-turn streaming state for the plugin hooks. The batcher
    // collects tokens since the last `on-message-update` dispatch so
    // we don't round-trip into Janet for every single token; the
    // turn-text buffer accumulates the entire turn for the closing
    // `on-turn-end` event. Reset at each TurnStart.
    #[cfg(feature = "plugin")]
    let mut token_batcher = crate::ui::streaming::TokenBatcher::default();
    #[cfg(feature = "plugin")]
    let mut current_turn_text = String::new();
    #[cfg(feature = "plugin")]
    let mut current_turn_index: u32 = 0;
    let mut response_start_line: Option<usize> = None;
    let mut show_reasoning = true;
    let mut was_reasoning = false;
    let mut todo_tools_enabled = false;
    let mut last_tool_name: Option<String> = None;
    #[allow(unused_mut)]
    let mut loop_label: Option<String> = None;
    #[cfg(feature = "loop")]
    let mut loop_state: Option<crate::extras::r#loop::LoopState> = None;
    #[cfg(feature = "git-worktree")]
    let mut wt_return_path: Option<String> = None;
    let mut rewind_picker = ListPicker::new();
    rewind_picker.set_monochrome(cli.no_color);
    let mut last_esc: Option<std::time::Instant> = None;
    let mut search_active = false;
    let mut search_query = String::new();
    let mut search_matches: Vec<usize> = Vec::new();
    let mut search_selected = 0usize;

    let perm_mode = || -> Option<String> {
        permission.as_ref().map(|p| {
            p.lock()
                .unwrap_or_else(|e| e.into_inner())
                .mode()
                .to_string()
        })
    };

    render_session(&mut renderer, session, cli, cfg, context)?;
    renderer.draw_bottom(
        &input,
        &with_queue(
            StatusLine::render(
                session,
                false,
                0,
                None,
                context.current_prompt_name.as_deref(),
                perm_mode().as_deref(),
            ),
            interjection_queue.len(),
        ),
        false,
    )?;

    let (user_tx, mut user_rx) = mpsc::channel::<UserEvent>(64);
    let user_tx_clone = user_tx.clone();
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(event::Event::Key(key)) => {
                    if user_tx_clone.blocking_send(UserEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(event::Event::Mouse(m)) => match m.kind {
                    MouseEventKind::ScrollUp => {
                        if user_tx_clone.blocking_send(UserEvent::ScrollUp).is_err() {
                            break;
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if user_tx_clone.blocking_send(UserEvent::ScrollDown).is_err() {
                            break;
                        }
                    }
                    MouseEventKind::Down(btn) if btn == MouseButton::Left => {
                        let _ = user_tx_clone.blocking_send(UserEvent::MouseDown {
                            row: m.row,
                            col: m.column,
                        });
                    }
                    MouseEventKind::Drag(btn) if btn == MouseButton::Left => {
                        let _ = user_tx_clone.blocking_send(UserEvent::MouseDrag {
                            row: m.row,
                            col: m.column,
                        });
                    }
                    MouseEventKind::Up(btn) if btn == MouseButton::Left => {
                        let _ = user_tx_clone.blocking_send(UserEvent::MouseUp {
                            row: m.row,
                            col: m.column,
                        });
                    }
                    _ => {}
                },
                Ok(event::Event::Paste(text)) => {
                    if user_tx_clone.blocking_send(UserEvent::Paste(text)).is_err() {
                        break;
                    }
                }
                Ok(event::Event::Resize(_, _)) => {
                    if user_tx_clone.blocking_send(UserEvent::Resize).is_err() {
                        break;
                    }
                }
                Err(_) => break,
                _ => {}
            }
        }
    });

    loop {
        // Refresh the info panel snapshot once per iteration so it stays
        // close to current as the agent edits files, runs MCP tools, etc.
        // Done at loop top (not after each redraw) to avoid touching the
        // 40-odd individual draw sites; the data shown lags one event in
        // the worst case, which is fine for ambient status.
        renderer.set_panel_data(build_panel_data(
            session,
            #[cfg(feature = "mcp")]
            mcp_manager,
            #[cfg(feature = "lsp")]
            lsp_manager.as_ref(),
        ));

        // Drain any pending plugin notifications and surface each as a
        // colored chat line. Done at loop top so notifications posted
        // during a tool hook or slash command appear on the next event,
        // not several events later.
        #[cfg(feature = "plugin")]
        if let Some(pm_arc) = crate::plugin::hook::global() {
            let pending = {
                let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
                mgr.drain_notifications()
            };
            for (level, msg) in pending {
                let color = match level.as_str() {
                    "warn" => Color::Yellow,
                    "error" => c_error(),
                    _ => theme::dim(),
                };
                renderer.write_line(&format!("[plugin] {}", msg), color)?;
            }
        }

        // Drain plugin-appended session entries. Each entry is
        // committed to `session.extra_entries` (so it survives
        // save/load) and displayed via the registered renderer for
        // its custom_type, or via the default JSON-dump renderer when
        // no renderer is registered.
        #[cfg(feature = "plugin")]
        if let Some(pm_arc) = crate::plugin::hook::global() {
            let drained = {
                let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
                mgr.drain_entries()
            };
            for (custom_type, data, display) in drained {
                // Record into session unconditionally (display=false
                // entries still persist; they're for plugin state that
                // shouldn't visually appear).
                let entry = session
                    .append_plugin_entry(custom_type.clone(), data.clone(), display)
                    .clone();
                if !entry.display {
                    continue;
                }
                render_plugin_entry(&pm_arc, &mut renderer, &entry)?;
            }
        }

        // Drain plugin-issued session-tree mutation ops (P4d). Applied
        // here so any /tree, /fork, /clone, navigate, set-label, or
        // session-replacement queued by a hook during the previous
        // event takes effect before the next user input is shown.
        #[cfg(feature = "plugin")]
        if let Some(pm_arc) = crate::plugin::hook::global() {
            let ops = {
                let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
                mgr.drain_tree_ops()
            };
            let mut any_session_replaced = false;
            for op in ops {
                let effect = plugin_tree::apply_tree_op(op, session, &mut input);
                match effect {
                    plugin_tree::TreeOpEffect::Applied(msg) => {
                        renderer.write_line(&msg, theme::dim())?;
                    }
                    plugin_tree::TreeOpEffect::Failed(msg) => {
                        renderer.write_line(&msg, c_error())?;
                    }
                    plugin_tree::TreeOpEffect::SessionReplaced(msg) => {
                        renderer.write_line(&msg, c_agent())?;
                        any_session_replaced = true;
                    }
                }
            }
            if any_session_replaced {
                // Repaint chat from the (possibly fresh) session so
                // the user sees the new state. The agent runtime
                // keeps the same model — reset_to_new / switch_session
                // preserve it — so no agent rebuild is needed here.
                render_session(&mut renderer, session, cli, cfg, context)?;
            }
        }

        tokio::select! {
            Some(ev) = user_rx.recv() => {
                match ev {
                    UserEvent::ScrollUp => {
                        renderer.scroll_line_up();
                        renderer.render_viewport()?;
                        renderer.draw_bottom(
                            &input,
                            &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                            is_running,
                        )?;
                        continue;
                    }
                    UserEvent::ScrollDown => {
                        renderer.scroll_line_down();
                        renderer.render_viewport()?;
                        renderer.draw_bottom(
                            &input,
                            &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                            is_running,
                        )?;
                        continue;
                    }
                    UserEvent::MouseDown { row, col } => {
                        if row < renderer.visible_lines() as u16
                            && let Some(pos) = renderer.buffer_pos_at(row, col)
                        {
                            renderer.selection_active = true;
                            renderer.selection_start = Some(pos);
                            renderer.selection_end = Some(pos);
                            renderer.render_viewport()?;
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                        }
                        continue;
                    }
                    UserEvent::MouseDrag { row, col } => {
                        if renderer.selection_active
                            && let Some(pos) = renderer.buffer_pos_at(row, col)
                        {
                            renderer.selection_end = Some(pos);
                            renderer.render_viewport()?;
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                        }
                        continue;
                    }
                    UserEvent::Paste(text) => {
                        input.handle_paste(&text);
                        renderer.draw_bottom(
                            &input,
                            &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                            is_running,
                        )?;
                        continue;
                    }
                    UserEvent::Resize => {
                        // Terminal dimensions changed — repaint everything so
                        // wrap, panel clipping, and input box rows recompute
                        // at the new size instead of waiting for the next
                        // unrelated event to trigger a redraw.
                        renderer.render_viewport()?;
                        renderer.draw_bottom(
                            &input,
                            &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                            is_running,
                        )?;
                        continue;
                    }
                    UserEvent::MouseUp { row, col } => {
                        if renderer.selection_active {
                            if let Some(pos) = renderer.buffer_pos_at(row, col) {
                                renderer.selection_end = Some(pos);
                            }
                            if let Some(text) = renderer.selected_text() {
                                copy_to_clipboard(&text);
                            }
                            renderer.clear_selection();
                            renderer.render_viewport()?;
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                        }
                        continue;
                    }
                    UserEvent::Key(key) => {
                        let is_ctrl_c = key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        let is_ctrl_d = key.code == KeyCode::Char('d')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        if is_ctrl_c || is_ctrl_d {
                            if rewind_picker.active {
                                rewind_picker.deactivate();
                                renderer.render_viewport()?;
                                renderer.draw_bottom(
                                    &input,
                                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                    is_running,
                                )?;
                                continue;
                            }
                            if search_active {
                                search_active = false;
                                renderer.render_viewport()?;
                                renderer.draw_bottom(
                                    &input,
                                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                    is_running,
                                )?;
                                continue;
                            }
                            if is_running {
                                is_running = false;
                                if let Some(h) = agent_abort.take() { h.abort(); }
                                agent_rx = None;
                                agent_interject = None;
                                #[cfg(feature = "loop")]
                                if let Some(ref mut ls) = loop_state {
                                    ls.active = false;
                                    loop_label = None;
                                }
                                let dropped = interjection_queue.len();
                                interjection_queue.clear();
                                if dropped > 0 {
                                    renderer.write_line(
                                        &format!("interrupted ({} queued message{} dropped)", dropped, if dropped == 1 { "" } else { "s" }),
                                        c_error(),
                                    )?;
                                } else {
                                    renderer.write_line("interrupted", c_error())?;
                                }
                                renderer.draw_bottom(
                                    &input,
                                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                    is_running,
                                )?;
                            } else {
                                break;
                            }
                            continue;
                        }

                        if renderer.selection_active && key.code == KeyCode::Char('y') {
                            if let Some(text) = renderer.selected_text() {
                                copy_to_clipboard(&text);
                                renderer.write_line("copied selection", Color::Green)?;
                            }
                            renderer.clear_selection();
                            renderer.render_viewport()?;
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                            continue;
                        }
                        // Ctrl+X drops the most-recently-queued interjection
                        // without affecting the running agent. No-op when the
                        // queue is empty so it doesn't shadow other behaviors.
                        let ctrl_x = key.code == KeyCode::Char('x')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        if ctrl_x && !interjection_queue.is_empty() {
                            interjection_queue.pop_back();
                            renderer.write_line(
                                &format!(
                                    "dropped 1 queued message ({} remaining)",
                                    interjection_queue.len()
                                ),
                                theme::dim(),
                            )?;
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                            continue;
                        }

                        let ctrl_f = key.code == KeyCode::Char('f')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        if ctrl_f && !search_active && !rewind_picker.active {
                            search_active = true;
                            search_query.clear();
                            search_matches.clear();
                            search_selected = 0;
                            update_search(&renderer, &search_query, &mut search_matches, &mut search_selected);
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                            draw_search_bar(&search_query, &search_matches, search_selected)?;
                            continue;
                        }

                        if search_active {
                            match key.code {
                                KeyCode::Esc => {
                                    search_active = false;
                                    last_esc = None;
                                    renderer.render_viewport()?;
                                    renderer.draw_bottom(
                                        &input,
                                        &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                        is_running,
                                    )?;
                                    continue;
                                }
                                KeyCode::Enter => {
                                    if let Some(&line) = search_matches.get(search_selected) {
                                        renderer.scroll_to_line(line);
                                    }
                                    search_active = false;
                                    renderer.render_viewport()?;
                                    renderer.draw_bottom(
                                        &input,
                                        &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                        is_running,
                                    )?;
                                    continue;
                                }
                                KeyCode::Up => {
                                    if search_selected > 0 {
                                        search_selected -= 1;
                                    }
                                }
                                KeyCode::Down => {
                                    if search_selected + 1 < search_matches.len() {
                                        search_selected += 1;
                                    }
                                }
                                KeyCode::Backspace => {
                                    search_query.pop();
                                    update_search(&renderer, &search_query, &mut search_matches, &mut search_selected);
                                }
                                KeyCode::Char(c) => {
                                    search_query.push(c);
                                    update_search(&renderer, &search_query, &mut search_matches, &mut search_selected);
                                }
                                _ => {}
                            }
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                            draw_search_bar(&search_query, &search_matches, search_selected)?;
                            continue;
                        }

                        if key.code == KeyCode::Esc && rewind_picker.active {
                            rewind_picker.deactivate();
                            renderer.render_viewport()?;
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                            continue;
                        }

                        if renderer.selection_active && key.code == KeyCode::Esc {
                            renderer.clear_selection();
                            renderer.render_viewport()?;
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                            continue;
                        }

                        if key.code == KeyCode::Esc && is_running {
                            is_running = false;
                            if let Some(h) = agent_abort.take() { h.abort(); }
                            agent_rx = None;
                            agent_interject = None;
                            #[cfg(feature = "loop")]
                            if let Some(ref mut ls) = loop_state {
                                ls.active = false;
                                loop_label = None;
                            }
                            renderer.write_line("interrupted (Esc)", c_error())?;
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                            continue;
                        }

                        if rewind_picker.active {
                            if let Some(idx) = rewind_picker.handle_key(key) {
                                rewind_session(session, idx, &mut renderer)?;
                                rewind_picker.deactivate();
                                renderer.render_viewport()?;
                            }
                            if rewind_picker.active {
                                renderer.render_viewport()?;
                            }
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                            if rewind_picker.active {
                                rewind_picker.draw()?;
                            }
                            continue;
                        }

                        if key.code == KeyCode::Esc && !is_running && !renderer.selection_active {
                            let now = std::time::Instant::now();
                            if let Some(prev) = last_esc {
                                if now.duration_since(prev) < std::time::Duration::from_millis(1500) {
                                    last_esc = None;
                                    open_rewind_picker(session, &mut rewind_picker);
                                    rewind_picker.draw()?;
                                    renderer.draw_bottom(
                                        &input,
                                        &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                        is_running,
                                    )?;
                                    continue;
                                }
                            }
                            last_esc = Some(now);
                            renderer.write_line("Press Esc again to rewind...", theme::dim())?;
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                            continue;
                        }

                        if key.code != KeyCode::Esc {
                            last_esc = None;
                        }

                        let ctrl_r = key.code == KeyCode::Char('r')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        if ctrl_r {
                            show_reasoning = !show_reasoning;
                            renderer.write_line(
                                &format!("reasoning visibility: {}", if show_reasoning { "on" } else { "off" }),
                                Color::White,
                            )?;
                            renderer.draw_bottom(
                                &input,
                                &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                is_running,
                            )?;
                            continue;
                        }

                        match key.code {
                            KeyCode::PageUp => {
                                renderer.scroll_page_up();
                                renderer.render_viewport()?;
                                renderer.draw_bottom(
                                    &input,
                                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                    is_running,
                                )?;
                                continue;
                            }
                            KeyCode::PageDown => {
                                renderer.scroll_page_down();
                                renderer.render_viewport()?;
                                renderer.draw_bottom(
                                    &input,
                                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                    is_running,
                                )?;
                                continue;
                            }
                            KeyCode::Home => {
                                renderer.scroll_to_top();
                                renderer.render_viewport()?;
                                renderer.draw_bottom(
                                    &input,
                                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                    is_running,
                                )?;
                                continue;
                            }
                            KeyCode::End => {
                                renderer.scroll_to_bottom()?;
                                renderer.draw_bottom(
                                    &input,
                                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                    is_running,
                                )?;
                                continue;
                            }
                            _ => {}
                        }

                        if input.picker.as_ref().is_some_and(|p| p.active)
                            && input.handle_picker_key(key) {
                                renderer.render_viewport()?;
                                renderer.draw_bottom(
                                    &input,
                                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                    is_running,
                                )?;
                                if let Some(ref picker) = input.picker {
                                    picker.draw(renderer.input_top_row())?;
                                }
                                continue;
                            }

                        if let Some(text) = input.handle_key(key) {
                            #[cfg(feature = "loop")]
                            if loop_state.as_ref().is_some_and(|ls| ls.active) && !text.starts_with('/') {
                                renderer.write_line("loop active: /loop stop to cancel", c_error())?;
                                renderer.draw_bottom(
                                    &input,
                                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                    is_running,
                                )?;
                                continue;
                            }
                            if renderer.is_scrolling() {
                                renderer.scroll_to_bottom()?;
                            }
                            if let Some(prefix) = shell::parse_shell_prefix(&text) {
                                if is_running {
                                    renderer.write_line("agent is busy, wait or interrupt first", c_error())?;
                                    renderer.draw_bottom(
                                        &input,
                                        &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                        is_running,
                                    )?;
                                    continue;
                                }
                                for line in text.lines() {
                                    let safe_line = sanitize_output(line);
                                    renderer.write_line(&format!("<you>   {}", safe_line), theme::user())?;
                                }
                                renderer.write_line("", Color::White)?;
                                match prefix {
                                    shell::ShellPrefix::Visible(cmd) => {
                                        match run_shell_command(&cmd, &sandbox).await {
                                            Ok(output) => {
                                                renderer.write_line(&output, theme::dim())?;
                                                let msg = format!("I ran: $ {}\n\nOutput:\n{}", cmd, output);
                                                let history = crate::agent::runner::convert_history(session);
                                                session.add_message(MessageRole::User, &msg);
                                renderer.set_avatar_state(avatar::AvatarState::Idle);
                                                let runner = agent.clone().spawn_runner(
                                                    crate::agent::tools::background::prepend_pending_notifications(&msg, bg_store.as_ref()),
                                                    history,
                                                );
                                                agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
                                                agent_interject = Some(runner.interject_tx);
                                                is_running = true;
                                            }
                                            Err(e) => {
                                                renderer.write_line(&format!("shell error: {}", e), c_error())?;
                                            }
                                        }
                                    }
                                    shell::ShellPrefix::Invisible(cmd) => {
                                        match run_shell_command(&cmd, &sandbox).await {
                                            Ok(output) => {
                                                renderer.write_line(&output, theme::dim())?;
                                            }
                                            Err(e) => {
                                                renderer.write_line(&format!("shell error: {}", e), c_error())?;
                                            }
                                        }
                                    }
                                }
                                renderer.draw_bottom(
                                    &input,
                                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                    is_running,
                                )?;
                                continue;
                            }
                            if text.starts_with('/') {
                                if is_running && !matches!(
                                    text.split_whitespace().next().unwrap_or(""),
                                    "/quit" | "/help" | "/reasoning"
                                ) {
                                    renderer.write_line("agent is busy — wait, interrupt (Ctrl+C), or use /quit", c_error())?;
                                    renderer.draw_bottom(
                                        &input,
                                        &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                        is_running,
                                    )?;
                                    continue;
                                }
                                for line in text.lines() {
                                    let safe_line = sanitize_output(line);
                                    renderer.write_line(&format!("<you>   {}", safe_line), theme::user())?;
                                }
                                renderer.write_line("", Color::White)?;
                                let result = handle_slash(&text, &mut agent, &client, &mut renderer, session, cli, cfg, context, &mut show_reasoning, &mut is_running, &mut input, &permission, &ask_tx, &mut todo_tools_enabled, &bg_store, &sandbox, #[cfg(feature = "loop")] &mut loop_state, #[cfg(feature = "mcp")] mcp_manager, #[cfg(feature = "semantic")] semantic_manager).await;
                                match result {
                                Err(e) if e.to_string().starts_with("DEFER_COMPRESS:") => {
                                    let err_msg = e.to_string();
                                    let instructions = err_msg.strip_prefix("DEFER_COMPRESS:").and_then(|s| {
                                        let s = s.trim();
                                        if s.is_empty() || s == "(none)" { None } else { Some(s.to_string()) }
                                    });
                                        let compress_result = handle_compress(
                                            instructions.as_deref(),
                                            &mut agent, &client, &mut renderer, session, cli, cfg, context,
                                            &permission, &ask_tx, &bg_store, &sandbox,
                                            #[cfg(feature = "mcp")] mcp_manager,
                                            #[cfg(feature = "semantic")] semantic_manager,
                                        ).await;
                                        if let Err(e) = compress_result {
                                            renderer.write_line(&format!("compress error: {}", e), c_error())?;
                                        }
                                        if let Err(e) = crate::session::storage::save_session(session) {
                                            renderer.write_line(
                                                &format!("warning: failed to save session: {}", e),
                                                c_error(),
                                            )?;
                                        }
                                    }
                                    #[cfg(feature = "git-worktree")]
                                    Err(e) if e.to_string().starts_with("DEFER_WT_MERGE:") => {
                                        let err_msg = e.to_string();
                                        let parts: Vec<&str> = err_msg.strip_prefix("DEFER_WT_MERGE:").unwrap_or("").splitn(5, ':').collect();
                                        if parts.len() == 5 {
                                            let branch = parts[0];
                                            let target = parts[1];
                                            let main_path = parts[2].to_string();
                                            let wt_path = parts[3];
                                            let _repo_name = parts[4];
                                            let prompt = format!(
                                                "I'm in a git worktree on branch '{}' at '{}'. \
                                                 Please merge branch '{}' into '{}' in the main repo at '{}', \
                                                 push the changes, and delete the worktree at '{}'. \
                                                 After merging, go back to the main repo at '{}'.",
                                                branch, wt_path, branch, target, main_path, wt_path, main_path
                                            );
                                            let history = crate::agent::runner::convert_history(session);
                                            session.add_message(MessageRole::User, &prompt);
                                            renderer.set_avatar_state(avatar::AvatarState::Idle);
                                            let runner = agent.clone().spawn_runner(
                                                crate::agent::tools::background::prepend_pending_notifications(&prompt, bg_store.as_ref()),
                                                history,
                                            );
                                            agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
                                                agent_interject = Some(runner.interject_tx);
                                            is_running = true;
                                            wt_return_path = Some(main_path);
                                        }
                                    }
                                    #[cfg(feature = "git-worktree")]
                                    Err(e) if e.to_string().starts_with("DEFER_WT_EXIT:") => {
                                        let err_msg = e.to_string();
                                        let parts: Vec<&str> = err_msg.strip_prefix("DEFER_WT_EXIT:").unwrap_or("").splitn(2, ':').collect();
                                        if parts.len() == 2 {
                                            let main_path = parts[0];
                                            std::env::set_current_dir(main_path)
                                                .map_err(|e| anyhow::anyhow!("failed to change directory: {}", e))?;
                                            session.working_dir = compact_str::CompactString::new(main_path);
                                            context.reload();
                                            let model = client.completion_model(session.model.to_string());
                                            agent = crate::provider::build_agent(
                                                model,
                                                cli,
                                                cfg,
                                                context,
                                                permission.clone(),
                                                ask_tx.clone(),
                                                None,
                                                None,
                                                bg_store.clone(),
                                                                                                #[cfg(feature = "lsp")]
                                                                                                None,
                                                sandbox.clone(),
                                                #[cfg(feature = "mcp")] mcp_manager,
                                                #[cfg(feature = "semantic")] semantic_manager,
                                            ).await;
                                            render_session(&mut renderer, session, cli, cfg, context)?;
                                            renderer.write_line(
                                                &format!("returned to main repo at {}", main_path),
                                                c_agent(),
                                            )?;
                                        }
                                    }
                                    Err(e) => {
                                        if e.downcast_ref::<std::io::Error>().is_some_and(|e: &std::io::Error| e.kind() == std::io::ErrorKind::Interrupted) {
                                            break;
                                        }
                                        renderer.write_line(&format!("error: {}", e), c_error())?;
                                    }
                                    Ok(_) => {
                                        if !cli.no_session
                                            && let Err(e) = crate::session::storage::save_session(session)
                                        {
                                            renderer.write_line(
                                                &format!("warning: failed to save session: {}", e),
                                                c_error(),
                                            )?;
                                        }
                                        #[cfg(feature = "loop")]
                                        if let Some(ref mut ls) = loop_state
                                            && ls.active && ls.iteration == 0 && !is_running
                                        {
                                            ls.iteration = 1;
                                            let prompt = ls.build_prompt();
                                            let runner = agent.clone().spawn_runner(
                                                crate::agent::tools::background::prepend_pending_notifications(&prompt, bg_store.as_ref()),
                                                Vec::new(),
                                            );
                                            agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
                                                agent_interject = Some(runner.interject_tx);
                                            is_running = true;
                                            loop_label = Some(ls.iteration_label());
                                        }
                                    }
                                }
                                if !cli.no_session
                                    && let Err(e) = crate::session::storage::save_session(session)
                                {
                                    renderer.write_line(
                                        &format!("warning: failed to save session: {}", e),
                                        c_error(),
                                    )?;
                                }
                            } else if is_running {
                                // Agent busy — queue the message for after the
                                // current run finishes. Echo it dimmed so the
                                // user sees it landed in the queue, not lost.
                                interjection_queue.push_back(text.to_string());
                                // Ask the runner to stop at its next tool-result
                                // boundary so the queued message is picked up
                                // promptly instead of waiting for the whole
                                // multi-turn run to finish. send() returning
                                // Err just means the runner already exited
                                // (race with Done) — harmless, queue still
                                // drains on the Done handler.
                                if let Some(tx) = agent_interject.as_ref() {
                                    let _ = tx.send(());
                                }
                                for line in text.lines() {
                                    let safe_line = sanitize_output(line);
                                    renderer.write_line(
                                        &format!("» {}", safe_line),
                                        theme::dim(),
                                    )?;
                                }
                                renderer.write_line(
                                    &format!(
                                        "(queued; runner will stop at next safe boundary — Ctrl+X drops, Ctrl+C cancels)"
                                    ),
                                    theme::dim(),
                                )?;
                            } else {
                                for line in text.lines() {
                                    let safe_line = sanitize_output(line);
                                    renderer.write_line(&format!("<you>   {}", safe_line), theme::user())?;
                                }
                                renderer.write_line("", Color::White)?;

                                let history = crate::agent::runner::convert_history(session);

                                #[allow(unused_mut)]
                                let mut plugin_hint: Option<String> = None;
                                #[allow(unused_mut)]
                                let mut plugin_replace: Option<String> = None;
                                #[cfg(feature = "plugin")]
                                if let Some(pm) = plugin_manager {
                                    let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
                                    match mgr.dispatch(
                                        "on-prompt",
                                        &format!(
                                            "@{{:prompt \"{}\"}}",
                                            crate::plugin::escape_janet_string(&text)
                                        ),
                                    ) {
                                        Ok(results) if !results.is_empty() => {
                                            for line in &results {
                                                renderer.write_line(
                                                    &format!("[plugin] {}", line),
                                                    theme::dim(),
                                                )?;
                                            }
                                            plugin_hint = Some(results.join("\n"));
                                        }
                                        Ok(_) => {}
                                        Err(e) => {
                                            renderer.write_line(
                                                &format!("[plugin] on-prompt error: {e}"),
                                                c_error(),
                                            )?;
                                        }
                                    }
                                    // A plugin hook may queue a follow-up prompt via
                                    // harness/request-prompt; pick it up here.
                                    if let Some(pending) = mgr.take_pending_prompt() {
                                        plugin_hint = Some(pending);
                                    }
                                    // harness/replace-prompt rewrites the current
                                    // turn entirely (distinct from request-prompt
                                    // which queues a follow-up turn). Takes
                                    // precedence over hint prepending below.
                                    plugin_replace = mgr.take_pending_prompt_replace();
                                }

                                let prompt = if let Some(replacement) = plugin_replace {
                                    // Echo the rewrite so the user can see what
                                    // the LLM is actually receiving — otherwise
                                    // it looks like their message vanished.
                                    renderer.write_line(
                                        "[plugin] prompt rewritten:",
                                        theme::dim(),
                                    )?;
                                    for line in replacement.lines() {
                                        renderer.write_line(
                                            &format!("  {}", sanitize_output(line)),
                                            theme::dim(),
                                        )?;
                                    }
                                    replacement
                                } else if let Some(hint) = plugin_hint {
                                    format!("{}\n\n{}", hint, text)
                                } else {
                                    text.to_string()
                                };

                                let runner = agent.clone().spawn_runner(
                                    crate::agent::tools::background::prepend_pending_notifications(&prompt, bg_store.as_ref()),
                                    history,
                                );
                                agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
                                                agent_interject = Some(runner.interject_tx);
                                is_running = true;

                                session.add_message(MessageRole::User, &text);
                                renderer.set_avatar_state(avatar::AvatarState::Idle);
                            }
                        }
                        renderer.draw_bottom(
                            &input,
                            &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                            is_running,
                        )?;
                        if let Some(ref picker) = input.picker {
                            picker.draw(renderer.input_top_row())?;
                        }
                    }
                }
            }
            Some(event) = async {
                if let Some(rx) = &mut agent_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                match event {
                    AgentEvent::Reasoning(text) => {
                        renderer.set_avatar_state(avatar::AvatarState::Thinking);
                        if !show_reasoning {
                            continue;
                        }
                        if !agent_line_started {
                            renderer.write("<dirge> ", Color::DarkMagenta)?;
                            agent_line_started = true;
                        }
                        let safe = sanitize_output(&text);
                        renderer.write(&safe, Color::DarkMagenta)?;
                        was_reasoning = true;
                    }
                    AgentEvent::Token(text) => {
                        renderer.set_avatar_state(avatar::AvatarState::Speaking);
                        if was_reasoning {
                            renderer.write_line("", Color::White)?;
                            agent_line_started = false;
                            was_reasoning = false;
                            response_buf.clear();
                            response_start_line = None;
                        }
                        let safe = sanitize_output(&text);
                        response_buf.push_str(&safe);

                        // Stream this token into the per-turn batcher
                        // and accumulator. When the batcher crosses its
                        // threshold, dispatch `on-message-update` with
                        // the cumulative text so far. The batcher's
                        // batch covers only the *new* tokens since the
                        // last update; current_turn_text is the *full*
                        // turn text for the closing on-turn-end event.
                        #[cfg(feature = "plugin")]
                        if let Some(pm) = plugin_manager {
                            current_turn_text.push_str(&text);
                            if token_batcher.push(&text).is_some() {
                                let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
                                let _ = mgr.dispatch(
                                    "on-message-update",
                                    &format!(
                                        "@{{:index {} :partial \"{}\"}}",
                                        current_turn_index,
                                        crate::plugin::escape_janet_string(&current_turn_text),
                                    ),
                                );
                            }
                        }

                        if response_buf.is_empty() {
                            continue;
                        }

                        let max_width = renderer.content_width().saturating_sub(9); // 8-col handle + space
                        let mut styled =
                            crate::ui::markdown::markdown_to_styled(&response_buf, max_width);

                        if !styled.is_empty() {
                            styled[0].text =
                                CompactString::from(format!("<dirge> {}", styled[0].text));
                        }

                        if let Some(start) = response_start_line {
                            renderer.replace_from(start, styled);
                        } else {
                            let start = renderer.buffer_len();
                            response_start_line = Some(start);
                            renderer.replace_from(start, styled);
                        }
                        renderer.render_viewport()?;
                        agent_line_started = true;
                    }
                    AgentEvent::ToolCall { name, args } => {
                        was_reasoning = false;
                        renderer.set_avatar_state(avatar::AvatarState::from_tool_name(&name));
                        // If a previous tool's chamber never closed
                        // (errored without a ToolResult, etc.), close
                        // it before opening the new one. Without this
                        // the new `╭─ NAME ─ args` lands inside the
                        // stale chamber.
                        close_tool_chamber_if_open(&mut renderer, &mut last_tool_name)?;
                        last_tool_name = Some(name.to_string());
                        if agent_line_started {
                            renderer.write_line("", Color::White)?;
                            agent_line_started = false;
                        }
                        response_buf.clear();
                        response_start_line = None;
                        // Tool-call line: rounded chamber TOP border
                        // with the tool name on it. Output lines below
                        // get `│ ` chamber rows; the chamber is closed
                        // by `╰────╯` after the ToolResult. Header
                        // border pads with dashes out to the frame
                        // width so it visually mates with the closing
                        // bottom border (matching btop's framed cards).
                        let upper = name.to_ascii_uppercase();
                        let summary = format_tool_call_summary(&name, &args);
                        let trimmed = summary
                            .strip_prefix(&format!("{} ", name))
                            .unwrap_or(&summary);
                        let pre = format!("╭─ {} ─ {} ", upper, trimmed);
                        let pre_clean = sanitize_output(&pre).into_string();
                        let (frame_w, _) = chamber_widths(&renderer);
                        let pre_len = pre_clean.chars().count();
                        let dashes = frame_w
                            .saturating_sub(pre_len + 1) // 1 for closing ╮
                            .max(0);
                        let header = format!("{}{}╮", pre_clean, "─".repeat(dashes));
                        renderer.write_line(&header, c_tool())?;

                        // Note: on-tool-start fires from HookedToolDyn now,
                        // around the actual tool invocation. The UI no
                        // longer dispatches it here — that would double-
                        // fire the hook per tool call.
                    }
                    AgentEvent::ToolResult { output } => {
                        let show_details = cfg.show_tool_details.unwrap_or(true);
                        let max_chars = cfg.resolve_tool_result_max_chars();
                        let show_diff = cfg.resolve_show_edit_diff();

                        // on-tool-end is also fired by HookedToolDyn so the
                        // host doesn't re-dispatch it here.

                        if show_details {
                            let is_edit =
                                last_tool_name.as_deref() == Some("edit") && show_diff;

                            if is_edit {
                                // Colorized diff rendering. The edit tool emits
                                // its diff block starting with "--- a/<path>" —
                                // match that exact sentinel to avoid false
                                // positives on stray "--- " prefixes elsewhere
                                // in the output.
                                let lines: Vec<&str> = output.lines().collect();
                                let diff_start = lines
                                    .iter()
                                    .position(|l| l.starts_with("--- a/"));
                                if let Some(pre) = diff_start {
                                    let (frame_w, inner) = chamber_widths(&renderer);
                                    // Pre-diff prose (the edit tool's
                                    // header line, etc.) renders in
                                    // the chamber's standard tone.
                                    for l in &lines[..pre] {
                                        if !l.is_empty() {
                                            let txt = sanitize_output(l).into_string();
                                            renderer.write_line(
                                                &chamber_row(&txt, inner),
                                                theme::result(),
                                            )?;
                                        }
                                    }
                                    // Colorized diff with opencode-style
                                    // tinted backgrounds: + lines get a
                                    // dim-green bg (palette 22), - lines
                                    // get a dim-red bg (palette 52).
                                    // Header (`--- ` / `+++ ` / `@@`) and
                                    // context lines have no bg.
                                    for l in &lines[pre..] {
                                        let txt = sanitize_output(l).into_string();
                                        if l.starts_with("--- ") || l.starts_with("+++ ") {
                                            renderer.write_line(
                                                &chamber_row(&txt, inner),
                                                Color::Cyan,
                                            )?;
                                        } else if l.starts_with("@@") {
                                            renderer.write_line(
                                                &chamber_row(&txt, inner),
                                                Color::DarkCyan,
                                            )?;
                                        } else if l.starts_with('+') {
                                            renderer.write_line(
                                                &chamber_row_with_bg(&txt, inner, 22),
                                                Color::Green,
                                            )?;
                                        } else if l.starts_with('-') {
                                            renderer.write_line(
                                                &chamber_row_with_bg(&txt, inner, 52),
                                                Color::Red,
                                            )?;
                                        } else {
                                            renderer.write_line(
                                                &chamber_row(&txt, inner),
                                                theme::dim(),
                                            )?;
                                        }
                                    }
                                    renderer.write_line(
                                        &chamber_bottom(frame_w),
                                        theme::dim(),
                                    )?;
                                } else {
                                    // No diff section found, show normally
                                    render_tool_output(
                                        &mut renderer, &output, max_chars,
                                    )?;
                                }
                            } else {
                                render_tool_output(&mut renderer, &output, max_chars)?;
                            }
                        }
                        // Clear after consuming so a future stray ToolResult
                        // can't be coloured with a stale tool name.
                        last_tool_name = None;
                    }
                    AgentEvent::Done { response, tokens, cost } => {
                        was_reasoning = false;
                        last_tool_name = None;
                        renderer.set_avatar_state(avatar::AvatarState::Done);

                        #[allow(unused_mut, unused_variables)]
                        let mut plugin_followup: Option<String> = None;
                        #[cfg(feature = "plugin")]
                        if let Some(pm) = plugin_manager {
                            let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
                            match mgr.dispatch(
                                "on-response",
                                &format!(
                                    "@{{:response \"{}\"}}",
                                    crate::plugin::escape_janet_string(&response)
                                ),
                            ) {
                                Ok(results) if !results.is_empty() => {
                                    for line in &results {
                                        renderer.write_line(
                                            &format!("[plugin] {}", line),
                                            theme::dim(),
                                        )?;
                                    }
                                    plugin_followup = Some(results.join("\n"));
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    renderer.write_line(
                                        &format!("[plugin] on-response error: {e}"),
                                        c_error(),
                                    )?;
                                }
                            }
                            // Check for pending prompts queued by on-response
                            if let Some(pending) = mgr.take_pending_prompt() {
                                plugin_followup = Some(pending);
                            }
                            mgr.store_response(&response);
                            // Fire on-complete after on-response so
                            // plugins can react to "turn fully done."
                            // Previously this hook was in HOOK_NAMES
                            // (so plugins defining it got auto-aliased)
                            // but no host site dispatched — silent fail.
                            match mgr.dispatch("on-complete", "@{}") {
                                Ok(_) => {}
                                Err(e) => {
                                    renderer.write_line(
                                        &format!("[plugin] on-complete error: {e}"),
                                        c_error(),
                                    )?;
                                }
                            }
                            // Clear `harness-response` so the next hook
                            // doesn't see stale text from this turn.
                            let _ = mgr.eval("(set harness-response nil)");
                        }

                        if !response_buf.is_empty() {
                            let max_width = renderer.content_width().saturating_sub(9); // 8-col handle + space
                            let mut styled = crate::ui::markdown::markdown_to_styled(
                                &response_buf,
                                max_width,
                            );
                            if !styled.is_empty() {
                                styled[0].text =
                                    CompactString::from(format!("<dirge> {}", styled[0].text));
                            }
                            if let Some(start) = response_start_line {
                                renderer.replace_from(start, styled);
                                renderer.render_viewport()?;
                            }
                        } else if !agent_line_started {
                            renderer.write("<dirge> ", c_agent())?;
                        }

                        renderer.write_line("", Color::White)?;
                        renderer.write_line("", Color::White)?;
                        session.add_message(MessageRole::Assistant, &response);
                        session.total_tokens = session.total_tokens.saturating_add(tokens);
                        session.total_cost += cost;
                        agent_line_started = false;
                        response_buf.clear();
                        response_start_line = None;

                        #[cfg(feature = "loop")]
                        let loop_running = loop_state.as_ref().is_some_and(|ls| ls.active);
                        #[cfg(not(feature = "loop"))]
                        let loop_running = false;

                        if !loop_running
                            && cfg.resolve_compact_enabled()
                            && session.needs_compaction(cfg.resolve_reserve_tokens())
                            && !cli.no_session
                        {
                            renderer.write_line("auto-compacting...", theme::dim())?;
                            let compress_result = handle_compress(
                                None,
                                &mut agent, &client, &mut renderer, session, cli, cfg, context,
                                &permission, &ask_tx, &bg_store, &sandbox,
                                #[cfg(feature = "mcp")] mcp_manager,
                                #[cfg(feature = "semantic")] semantic_manager,
                            ).await;
                            if let Err(e) = compress_result {
                                renderer.write_line(&format!("auto-compact error: {}", e), c_error())?;
                            }
                        }

                        if !cli.no_session
                            && let Err(e) = crate::session::storage::save_session(session)
                        {
                            renderer.write_line(
                                &format!("warning: failed to save session: {}", e),
                                c_error(),
                            )?;
                        }
                        is_running = false;
                        if let Some(h) = agent_abort.take() { h.abort(); }
                        agent_rx = None;
                        agent_interject = None;

                        #[cfg(feature = "plugin")]
                        let followup_for_decision = plugin_followup.clone();
                        #[cfg(not(feature = "plugin"))]
                        let followup_for_decision: Option<String> = None;

                        #[cfg(feature = "loop")]
                        let (loop_active, loop_should_stop) = loop_state
                            .as_ref()
                            .map(|ls| (ls.active, ls.active && ls.should_stop()))
                            .unwrap_or((false, false));
                        #[cfg(not(feature = "loop"))]
                        let (loop_active, loop_should_stop) = (false, false);

                        let action = crate::plugin::decide_post_done_action(
                            followup_for_decision,
                            loop_active,
                            loop_should_stop,
                        );

                        match action {
                            crate::plugin::PostDoneAction::Followup(text) => {
                                let followup_prompt = text + "\n\nContinue.";
                                let runner = agent.clone().spawn_runner(
                                    crate::agent::tools::background::prepend_pending_notifications(&followup_prompt, bg_store.as_ref()),
                                    crate::agent::runner::convert_history(session),
                                );
                                agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
                                                agent_interject = Some(runner.interject_tx);
                                is_running = true;
                            }
                            crate::plugin::PostDoneAction::LoopStop => {
                                #[cfg(feature = "loop")]
                                if let Some(ref mut ls) = loop_state {
                                    renderer.write_line(
                                        &format!(
                                            "[loop] max iterations ({}) reached, stopping",
                                            ls.iteration
                                        ),
                                        c_agent(),
                                    )?;
                                    ls.active = false;
                                    loop_label = None;
                                }
                            }
                            crate::plugin::PostDoneAction::LoopIter => {
                                #[cfg(feature = "loop")]
                                if let Some(ref mut ls) = loop_state {
                                    let summary: String = response.chars().take(200).collect();
                                    ls.last_summary = Some(summary);
                                    ls.iteration += 1;
                                    let prompt = ls.build_prompt();
                                    let runner = agent.clone().spawn_runner(
                                        crate::agent::tools::background::prepend_pending_notifications(&prompt, bg_store.as_ref()),
                                        Vec::new(),
                                    );
                                    agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
                                                agent_interject = Some(runner.interject_tx);
                                    is_running = true;
                                    loop_label = Some(ls.iteration_label());
                                    renderer.write_line(
                                        &format!("[loop] launching {}", ls.iteration_label()),
                                        c_agent(),
                                    )?;
                                }
                            }
                            crate::plugin::PostDoneAction::Idle => {}
                        }

                        #[cfg(feature = "git-worktree")]
                        if let Some(main_path) = wt_return_path.take() {
                            match std::env::set_current_dir(&main_path) {
                                Ok(()) => {
                                    session.working_dir = compact_str::CompactString::new(&main_path);
                                    context.reload();
                                    let model = client.completion_model(session.model.to_string());
                                    agent = crate::provider::build_agent(
                                        model,
                                        cli,
                                        cfg,
                                        context,
                                        permission.clone(),
                                        ask_tx.clone(),
                                        None,
                                        None,
                                        bg_store.clone(),
                                                                                #[cfg(feature = "lsp")]
                                                                                None,
                                        sandbox.clone(),
                                        #[cfg(feature = "mcp")] mcp_manager,
                                        #[cfg(feature = "semantic")] semantic_manager,
                                    ).await;
                                    render_session(&mut renderer, session, cli, cfg, context)?;
                                    renderer.write_line(
                                        &format!("merged and returned to main repo at {}", main_path),
                                        c_agent(),
                                    )?;
                                }
                                Err(e) => {
                                    renderer.write_line(
                                        &format!("warning: failed to change back to main repo: {}", e),
                                        c_error(),
                                    )?;
                                }
                            }
                        }

                        // Drain the interjection queue once the run is fully
                        // idle (no plugin follow-up, loop iteration, or worktree
                        // cleanup grabbed the next turn). Concatenate all
                        // queued messages as a single new user turn and kick
                        // off another run against the now-stable agent/cwd.
                        if !is_running && !interjection_queue.is_empty() {
                            let queued: Vec<String> = interjection_queue.drain(..).collect();
                            let combined = queued.join("\n\n");
                            for line in combined.lines() {
                                let safe_line = sanitize_output(line);
                                renderer
                                    .write_line(&format!("<you>   {}", safe_line), theme::user())?;
                            }
                            renderer.write_line("", Color::White)?;

                            let history = crate::agent::runner::convert_history(session);
                            session.add_message(MessageRole::User, &combined);

                            let runner = agent.clone().spawn_runner(
                                crate::agent::tools::background::prepend_pending_notifications(
                                    &combined,
                                    bg_store.as_ref(),
                                ),
                                history,
                            );
                            agent_rx = Some(runner.event_rx);
                            agent_abort = Some(runner.task);
                            agent_interject = Some(runner.interject_tx);
                            is_running = true;
                        }
                    }
                    AgentEvent::Interjected { partial_response, tokens } => {
                        was_reasoning = false;
                        close_tool_chamber_if_open(&mut renderer, &mut last_tool_name)?;

                        // Finalize whatever assistant text streamed so far so
                        // the conversation history reflects what the user saw,
                        // not a phantom turn that "never happened".
                        if !response_buf.is_empty() {
                            let max_width = renderer.content_width().saturating_sub(9); // 8-col handle + space
                            let mut styled = crate::ui::markdown::markdown_to_styled(
                                &response_buf,
                                max_width,
                            );
                            if !styled.is_empty() {
                                styled[0].text =
                                    CompactString::from(format!("<dirge> {}", styled[0].text));
                            }
                            if let Some(start) = response_start_line {
                                renderer.replace_from(start, styled);
                                renderer.render_viewport()?;
                            }
                        }
                        renderer.write_line("", Color::White)?;
                        renderer.write_line(
                            "(interjected — stopped at last tool-result boundary)",
                            theme::dim(),
                        )?;
                        renderer.write_line("", Color::White)?;

                        // Record the (partial) assistant response in session
                        // history. Even truncated, it lets the LLM see what
                        // it had said when the user spoke up.
                        if !partial_response.is_empty() {
                            session.add_message(MessageRole::Assistant, &partial_response);
                            session.total_tokens = session.total_tokens.saturating_add(tokens);
                        }
                        agent_line_started = false;
                        response_buf.clear();
                        response_start_line = None;

                        if !cli.no_session
                            && let Err(e) = crate::session::storage::save_session(session)
                        {
                            renderer.write_line(
                                &format!("warning: failed to save session: {}", e),
                                c_error(),
                            )?;
                        }
                        is_running = false;
                        if let Some(h) = agent_abort.take() { h.abort(); }
                        agent_rx = None;
                        agent_interject = None;

                        // Drain the queue immediately — it's guaranteed to be
                        // non-empty here since the runner only emits this
                        // event when the UI signaled an interjection, and the
                        // signal is only sent from the queue-push code path.
                        if !interjection_queue.is_empty() {
                            let queued: Vec<String> = interjection_queue.drain(..).collect();
                            let combined = queued.join("\n\n");
                            for line in combined.lines() {
                                let safe_line = sanitize_output(line);
                                renderer.write_line(&format!("<you>   {}", safe_line), theme::user())?;
                            }
                            renderer.write_line("", Color::White)?;

                            let history = crate::agent::runner::convert_history(session);
                            session.add_message(MessageRole::User, &combined);

                            let runner = agent.clone().spawn_runner(
                                crate::agent::tools::background::prepend_pending_notifications(
                                    &combined,
                                    bg_store.as_ref(),
                                ),
                                history,
                            );
                            agent_rx = Some(runner.event_rx);
                            agent_abort = Some(runner.task);
                            agent_interject = Some(runner.interject_tx);
                            is_running = true;
                        }
                    }
                    AgentEvent::Error(e) => {
                        was_reasoning = false;
                        renderer.set_avatar_state(avatar::AvatarState::Error);
                        close_tool_chamber_if_open(&mut renderer, &mut last_tool_name)?;
                        let safe = sanitize_output(&e);
                        renderer.write_line(&format!("error: {}", safe), c_error())?;

                        #[cfg(feature = "plugin")]
                        if let Some(pm) = plugin_manager {
                            let mut mgr = pm.lock().unwrap_or_else(|err| err.into_inner());
                            if let Err(dispatch_err) = mgr.dispatch(
                                "on-error",
                                &format!(
                                    "@{{:error \"{}\"}}",
                                    crate::plugin::escape_janet_string(&e)
                                ),
                            ) {
                                renderer.write_line(
                                    &format!("[plugin] on-error error: {dispatch_err}"),
                                    c_error(),
                                )?;
                            }
                        }

                        is_running = false;
                        if let Some(h) = agent_abort.take() { h.abort(); }
                        agent_rx = None;
                        agent_interject = None;
                        agent_line_started = false;
                        response_buf.clear();
                        response_start_line = None;

                        // Drop queued interjections — they were typed expecting
                        // the running turn to succeed; replaying them blindly
                        // after an error (e.g. context-length) would just
                        // re-trigger it.
                        let dropped = interjection_queue.len();
                        interjection_queue.clear();
                        if dropped > 0 {
                            renderer.write_line(
                                &format!(
                                    "{} queued message{} dropped due to error",
                                    dropped,
                                    if dropped == 1 { "" } else { "s" }
                                ),
                                c_error(),
                            )?;
                        }
                    }
                    AgentEvent::TurnStart { index } => {
                        #[cfg(feature = "plugin")]
                        {
                            // New turn — reset per-turn streaming state.
                            // Without the reset, current_turn_text would
                            // accumulate across all turns and the index
                            // tracked here would drift from the runner's.
                            token_batcher.reset();
                            current_turn_text.clear();
                            current_turn_index = index;
                            if let Some(pm) = plugin_manager {
                                let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
                                let _ = mgr.dispatch(
                                    "on-turn-start",
                                    &format!("@{{:index {}}}", index),
                                );
                                // Clear tool-hook slots after the turn
                                // hook runs so a `(harness/block ...)`
                                // call inside on-turn-start can't bleed
                                // into the *first* tool of the next
                                // turn. `dispatch_tool_hook` clears
                                // slots before tool hooks, but turn
                                // hooks bypass that path.
                                let _ = mgr.eval(
                                    "(do (set harness-block nil) \
                                         (set harness-mutate-input nil) \
                                         (set harness-replace-result nil))",
                                );
                            }
                        }
                        #[cfg(not(feature = "plugin"))]
                        let _ = index;
                    }
                    AgentEvent::TurnEnd { index } => {
                        #[cfg(feature = "plugin")]
                        {
                            if let Some(pm) = plugin_manager {
                                // Flush any tokens that didn't reach the
                                // batcher threshold so the final partial
                                // update gets delivered.
                                if let Some(tail) = token_batcher.flush_remaining() {
                                    // tail is the *new* tokens since the
                                    // last update; current_turn_text now
                                    // covers them since we pushed at the
                                    // same time as the batcher.
                                    let _ = tail;
                                    let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
                                    let _ = mgr.dispatch(
                                        "on-message-update",
                                        &format!(
                                            "@{{:index {} :partial \"{}\"}}",
                                            index,
                                            crate::plugin::escape_janet_string(
                                                &current_turn_text
                                            ),
                                        ),
                                    );
                                }
                                let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
                                let _ = mgr.dispatch(
                                    "on-turn-end",
                                    &format!(
                                        "@{{:index {} :message \"{}\"}}",
                                        index,
                                        crate::plugin::escape_janet_string(&current_turn_text),
                                    ),
                                );
                                // Same defense as on-turn-start: clear
                                // tool-hook slots so turn-end can't
                                // leak block/mutate/replace into the
                                // next tool call.
                                let _ = mgr.eval(
                                    "(do (set harness-block nil) \
                                         (set harness-mutate-input nil) \
                                         (set harness-replace-result nil))",
                                );
                            }
                        }
                        #[cfg(not(feature = "plugin"))]
                        let _ = index;
                    }
                }
                renderer.draw_bottom(
                    &input,
                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                    is_running,
                )?;
                if let Some(ref picker) = input.picker {
                    picker.draw(renderer.input_top_row())?;
                }
            }
            Some(ask_req) = async {
                if let Some(rx) = &mut ask_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                was_reasoning = false;
                if agent_line_started {
                    renderer.write_line("", Color::White)?;
                    agent_line_started = false;
                }

                // If a tool chamber is open (the in-flight tool that
                // triggered this permission check), close it first so
                // the alert renders outside the chamber rather than
                // nested inside it.
                close_tool_chamber_if_open(&mut renderer, &mut last_tool_name)?;
                renderer.set_avatar_state(avatar::AvatarState::Alert);

                // Framed permission prompt. The double-bar border +
                // ALERT wordmark visually arrests the eye — this is
                // the single most important UX moment and the user
                // must not miss it. Box width = 64 for a stable look
                // independent of terminal width; the chat area
                // requires at least 60 cols anyway.
                const BOX_W: usize = 64;
                let inner = BOX_W.saturating_sub(2);
                let pre = "╭─ ⚠ ALERT · PERMISSION ";
                let pre_len = pre.chars().count();
                let top_fill = BOX_W.saturating_sub(pre_len + 1);
                let bot_bar = "─".repeat(inner);
                // Helper: format one row as `│ content (padded) │`
                // so every row of the alert closes on the right edge.
                let row = |content: &str| -> String {
                    let chars: Vec<char> = content.chars().collect();
                    let trimmed: String = if chars.len() <= inner.saturating_sub(2) {
                        chars.iter().collect()
                    } else if inner <= 2 {
                        String::new()
                    } else {
                        let cap = inner.saturating_sub(3);
                        let mut out: String = chars[..cap].iter().collect();
                        out.push('…');
                        out
                    };
                    let pad = inner.saturating_sub(trimmed.chars().count() + 1);
                    format!("│ {}{}│", trimmed, " ".repeat(pad))
                };
                renderer.write_line(
                    &format!("{}{}╮", pre, "─".repeat(top_fill)),
                    c_perm(),
                )?;
                renderer.write_line(&row(&format!("tool : {}", ask_req.tool)), c_perm())?;
                renderer.write_line(&row(&format!("args : {}", ask_req.input)), c_perm())?;
                renderer.write_line(&format!("├{}┤", bot_bar), c_perm())?;
                renderer.write_line(
                    &row("[y] allow once  [a] allow always  [n] deny  [ESC] abort"),
                    c_perm(),
                )?;
                renderer.write_line(&format!("╰{}╯", bot_bar), c_perm())?;

                let decision = loop {
                    tokio::select! {
                        Some(ev) = user_rx.recv() => {
                            match ev {
                                UserEvent::Key(key) => match key.code {
                                    KeyCode::Char('y') => break UserDecision::AllowOnce,
                                    KeyCode::Char('a') => {
                                        let pattern = suggest_pattern(&ask_req.tool, &ask_req.input);
                                        renderer.write_line(
                                            &format!("  -> will allow: {}", pattern),
                                            Color::Green,
                                        )?;
                                        break UserDecision::AllowAlways(pattern);
                                    }
                                    KeyCode::Char('n') | KeyCode::Esc => break UserDecision::Deny,
                                    _ => {}
                                },
                                // Keep scroll responsive while the
                                // alert is up — previously these
                                // events were dropped on the floor
                                // inside this loop, locking the chat
                                // viewport.
                                UserEvent::ScrollUp => {
                                    renderer.scroll_line_up();
                                    renderer.render_viewport()?;
                                }
                                UserEvent::ScrollDown => {
                                    renderer.scroll_line_down();
                                    renderer.render_viewport()?;
                                }
                                _ => {}
                            }
                        }
                    }
                };

                let allow_pattern = match &decision {
                    UserDecision::AllowAlways(p) => Some(p.clone()),
                    _ => None,
                };
                let _ = ask_req.reply.send(decision);

                if let Some(pattern) = allow_pattern {
                    session.permission_allowlist.push(PermissionAllowEntry {
                        tool: ask_req.tool.clone(),
                        pattern: pattern.clone(),
                    });
                    if !cli.no_session {
                        if let Err(e) = crate::session::storage::save_session(session) {
                            renderer.write_line(
                                &format!("warning: failed to save session: {}", e),
                                c_error(),
                            )?;
                        }
                    }
                    renderer.write_line(
                        &format!("  allowed {} {} (saved to session)", ask_req.tool, pattern),
                        Color::Green,
                    )?;
                }

                renderer.render_viewport()?;
                renderer.draw_bottom(
                    &input,
                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                    is_running,
                )?;
                if let Some(ref picker) = input.picker {
                    picker.draw(renderer.input_top_row())?;
                }
            }
            Some(lifecycle_evt) = async {
                if let Some(rx) = &mut lifecycle_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                // Human-visible lifecycle line for a background task. The
                // LLM-side notification (Finished only) is still queued
                // separately for prepend_pending_notifications at the next
                // turn boundary.
                use crate::agent::tools::background::{
                    LifecycleEvent, TaskState as TS,
                };
                let (label, color) = match &lifecycle_evt {
                    LifecycleEvent::Started { id } => {
                        let short: String = id.chars().take(8).collect();
                        (format!("[task {} started]", short), c_tool())
                    }
                    LifecycleEvent::Finished(notif) => {
                        let short: String = notif.id.chars().take(8).collect();
                        match &notif.state {
                            TS::Completed(_) => {
                                (format!("[task {} completed]", short), Color::Green)
                            }
                            TS::Failed(err) => {
                                let head = sanitize_single_line(err, 80);
                                (format!("[task {} failed: {}]", short, head), c_error())
                            }
                            // Running is never queued for notification.
                            TS::Running => continue,
                        }
                    }
                };
                // Make sure we land on a fresh line if a streamed response was in progress.
                if agent_line_started {
                    renderer.write_line("", Color::White)?;
                    agent_line_started = false;
                }
                renderer.write_line(&label, resolve_color(color, cli.no_color))?;
                renderer.render_viewport()?;
                renderer.draw_bottom(
                    &input,
                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                    is_running,
                )?;
            }
            Some(question_req) = async {
                if let Some(rx) = &mut question_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                was_reasoning = false;
                if agent_line_started {
                    renderer.write_line("", Color::White)?;
                    agent_line_started = false;
                }

                let mut answers: Vec<Vec<String>> = Vec::new();
                let mut rejected = false;

                for (qi, question) in question_req.questions.iter().enumerate() {
                    if let Some(header) = &question.header {
                        renderer.write_line(
                            &format!("\n--- {} ---", header),
                            c_perm(),
                        )?;
                    }
                    renderer.write_line(
                        &format!("\n[question {}] {}", qi + 1, question.question),
                        c_perm(),
                    )?;

                    let multi = question.multi_select.unwrap_or(false);
                    let custom = question.custom;
                    let num_options = question.options.len();
                    let mut cursor: usize = 0;
                    let mut selected: Vec<bool> = vec![false; num_options];
                    let mut custom_text: Option<String> = None;

                    // Anchor point — options rendered below will be replaced on each keystroke
                    let anchor = renderer.buffer_len();

                    loop {
                        // Build option lines as Vec<LineEntry>
                        let mut lines: Vec<LineEntry> = Vec::with_capacity(
                            num_options + if custom { 2 } else { 1 },
                        );
                        for (i, opt) in question.options.iter().enumerate() {
                            let marker = if i == cursor {
                                if multi {
                                    if selected[i] { "▶ [x]" } else { "▶ [ ]" }
                                } else {
                                    "▶"
                                }
                            } else {
                                if multi {
                                    if selected[i] { "  [x]" } else { "  [ ]" }
                                } else {
                                    "  "
                                }
                            };
                            lines.push(LineEntry {
                                text: compact_str::CompactString::new(
                                    &format!("  {} {} — {}", marker, opt.label, opt.description),
                                ),
                                color: c_perm(),
                            });
                        }
                        if custom {
                            let custom_marker = if cursor == num_options { "▶" } else { "  " };
                            let custom_label = if let Some(ref t) = custom_text {
                                format!("{} (custom) \"{}\"", custom_marker, t)
                            } else {
                                format!("{} (custom) type your own answer...", custom_marker)
                            };
                            lines.push(LineEntry {
                                text: compact_str::CompactString::new(&custom_label),
                                color: c_perm(),
                            });
                        }
                        lines.push(LineEntry {
                            text: compact_str::CompactString::new(if multi {
                                "  ↑↓ navigate  Space toggle  Enter confirm  Esc reject all"
                            } else {
                                "  ↑↓ navigate  Enter select  Esc reject all"
                            }),
                            color: c_perm(),
                        });

                        // Replace previous render with updated options
                        renderer.replace_from(anchor, lines);
                        renderer.render_viewport()?;
                        renderer.draw_bottom(
                            &input,
                            &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                            is_running,
                        )?;

                        // Wait for user input
                        let user_ev = user_rx.recv().await;
                        let Some(UserEvent::Key(key)) = user_ev else {
                            continue;
                        };

                        match key.code {
                            KeyCode::Up | KeyCode::Char('k') => {
                                if cursor > 0 { cursor -= 1; }
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                let max = if custom { num_options } else { num_options.saturating_sub(1) };
                                if cursor < max { cursor += 1; }
                            }
                            KeyCode::Enter => {
                                if custom && cursor == num_options {
                                    // Custom text input (works for both single and multi)
                                    let mut buf = String::new();
                                    renderer.write_line("  enter your answer:", c_perm())?;
                                    let input_anchor = renderer.buffer_len();
                                    loop {
                                        renderer.replace_from(
                                            input_anchor,
                                            vec![LineEntry {
                                                text: compact_str::CompactString::new(
                                                    &format!("  > {}", buf),
                                                ),
                                                color: c_perm(),
                                            }],
                                        );
                                        renderer.render_viewport()?;
                                        renderer.draw_bottom(
                                            &input,
                                            &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                                            is_running,
                                        )?;
                                        let ev = user_rx.recv().await;
                                        if let Some(UserEvent::Key(k)) = ev {
                                            match k.code {
                                                KeyCode::Enter => break,
                                                KeyCode::Esc => {
                                                    buf = String::new();
                                                    break;
                                                }
                                                KeyCode::Backspace => { buf.pop(); }
                                                KeyCode::Char(c) => { buf.push(c); }
                                                _ => {}
                                            }
                                        }
                                    }
                                    if buf.is_empty() {
                                        custom_text = None;
                                    } else {
                                        custom_text = Some(buf);
                                    }
                                    if !multi {
                                        // Single select: confirm immediately
                                        if let Some(ct) = custom_text.take() {
                                            answers.push(vec![ct]);
                                        }
                                        break;
                                    }
                                    // Multi select: continue, user presses Enter again to confirm
                                } else if multi {
                                    // Confirm multi-select
                                    let mut picked: Vec<String> = question
                                        .options
                                        .iter()
                                        .enumerate()
                                        .filter(|(i, _)| selected[*i])
                                        .map(|(_, o)| o.label.clone())
                                        .collect();
                                    if let Some(ct) = custom_text.take() {
                                        picked.push(ct);
                                    }
                                    if picked.is_empty() {
                                        renderer.write_line(
                                            "  select at least one option",
                                            c_perm(),
                                        )?;
                                    } else {
                                        answers.push(picked);
                                        break;
                                    }
                                } else {
                                    // Single select
                                    let opt = &question.options[cursor];
                                    answers.push(vec![opt.label.clone()]);
                                    break;
                                }
                            }
                            KeyCode::Char(' ') => {
                                if multi && cursor < num_options {
                                    selected[cursor] = !selected[cursor];
                                } else if !multi && cursor < num_options {
                                    // Space acts like Enter for single-select
                                    let opt = &question.options[cursor];
                                    answers.push(vec![opt.label.clone()]);
                                    break;
                                }
                            }
                            KeyCode::Esc => {
                                rejected = true;
                                break;
                            }
                            _ => {}
                        }
                    };
                    if rejected {
                        break;
                    }
                }

                if rejected {
                    let _ = question_req.reply.send(QuestionResponse::Rejected);
                } else {
                    let _ = question_req.reply.send(QuestionResponse::Answered(answers));
                }

                renderer.write_line("", Color::White)?;
                renderer.render_viewport()?;
                renderer.draw_bottom(
                    &input,
                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                    is_running,
                )?;
                if let Some(ref picker) = input.picker {
                    picker.draw(renderer.input_top_row())?;
                }
            }
            Some(dialog_req) = async {
                if let Some(rx) = dialog_rx.as_mut() {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                // Plugin asked the user a question via harness/confirm or
                // harness/select. The Janet worker thread is blocked on
                // the reply channel; render the dialog, drive a synchronous
                // key-read loop, then send the answer back. Other agent
                // events keep queuing in their channels — they'll process
                // after this arm returns.
                use crate::plugin::{DialogReply, DialogRequest};
                // Events that arrived during the dialog but didn't match
                // its accepted keys are stashed here, then pushed back into
                // user_rx after the dialog ends. Without this, a paste or
                // unrelated key during a confirm dialog would be lost.
                let mut deferred: Vec<UserEvent> = Vec::new();
                match dialog_req {
                    DialogRequest::Confirm { title, question, reply } => {
                        renderer.write_line(
                            &format!("[plugin {}] {}", title, question),
                            c_perm(),
                        )?;
                        renderer.write_line(
                            "  (y) yes  (n) no  (ESC) cancel = no",
                            c_perm(),
                        )?;
                        let answer = loop {
                            tokio::select! {
                                Some(ev) = user_rx.recv() => {
                                    if let UserEvent::Key(key) = ev {
                                        match key.code {
                                            KeyCode::Char('y') | KeyCode::Char('Y') => break true,
                                            KeyCode::Char('n')
                                            | KeyCode::Char('N')
                                            | KeyCode::Esc => break false,
                                            // Treat Ctrl+C as cancel (same
                                            // as Esc / no), not as
                                            // "interrupt the agent" — the
                                            // agent isn't running this code
                                            // path, the dialog is.
                                            KeyCode::Char('c')
                                                if key.modifiers
                                                    .contains(KeyModifiers::CONTROL) =>
                                            {
                                                break false;
                                            }
                                            _ => deferred.push(UserEvent::Key(key)),
                                        }
                                    } else {
                                        // Paste, Mouse*, ScrollUp/Down,
                                        // Resize, etc. Hand them back after
                                        // the dialog so the main loop arms
                                        // can handle them as usual.
                                        deferred.push(ev);
                                    }
                                }
                            }
                        };
                        let _ = reply.send(DialogReply::Confirm(answer));
                        renderer.write_line(
                            &format!("  -> {}", if answer { "yes" } else { "no" }),
                            theme::dim(),
                        )?;
                    }
                    DialogRequest::Select { title, options, reply } => {
                        renderer.write_line(
                            &format!("[plugin {}] pick one:", title),
                            c_perm(),
                        )?;
                        for (i, opt) in options.iter().enumerate() {
                            renderer.write_line(
                                &format!("  {}: {}", i + 1, opt),
                                c_perm(),
                            )?;
                        }
                        renderer.write_line(
                            "  (1-9) select  (ESC) cancel",
                            c_perm(),
                        )?;
                        let answer: Option<String> = loop {
                            tokio::select! {
                                Some(ev) = user_rx.recv() => {
                                    if let UserEvent::Key(key) = ev {
                                        match key.code {
                                            KeyCode::Char(c) if c.is_ascii_digit() => {
                                                let idx = (c as u8 - b'0') as usize;
                                                if idx >= 1 && idx <= options.len() {
                                                    break Some(options[idx - 1].clone());
                                                }
                                            }
                                            KeyCode::Esc => break None,
                                            KeyCode::Char('c')
                                                if key.modifiers
                                                    .contains(KeyModifiers::CONTROL) =>
                                            {
                                                break None;
                                            }
                                            _ => deferred.push(UserEvent::Key(key)),
                                        }
                                    } else {
                                        deferred.push(ev);
                                    }
                                }
                            }
                        };
                        let label = answer.as_deref().unwrap_or("(cancelled)").to_string();
                        let _ = reply.send(DialogReply::Select(answer));
                        renderer.write_line(
                            &format!("  -> {}", label),
                            theme::dim(),
                        )?;
                    }
                }
                // Replay deferred events into user_rx so the outer select!
                // arms see them next iteration. Best-effort: a full channel
                // (very unlikely, capacity 64) silently drops the tail.
                for ev in deferred {
                    let _ = user_tx.send(ev).await;
                }
                renderer.draw_bottom(
                    &input,
                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                    is_running,
                )?;
            }
            Some(plan_req) = async {
                if let Some(rx) = &mut plan_rx {
                    rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                was_reasoning = false;
                if agent_line_started {
                    renderer.write_line("", Color::White)?;
                    agent_line_started = false;
                }

                let (label, prompt_name) = match plan_req.action {
                    PlanAction::Enter => ("plan mode", "plan"),
                    PlanAction::Exit => ("implementation mode", "code"),
                };

                renderer.write_line(
                    &format!("[plan] switch to {}? (y/n)", label),
                    c_perm(),
                )?;

                let accepted = loop {
                    let Some(UserEvent::Key(key)) = user_rx.recv().await else {
                        continue;
                    };
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Enter => break true,
                        KeyCode::Char('n') | KeyCode::Esc => break false,
                        _ => {}
                    }
                };

                if accepted {
                    // Update context with the new prompt
                    if let Some(content) = context.prompts.get(prompt_name) {
                        context.current_prompt = Some(content.clone());
                        context.current_prompt_name = Some(prompt_name.to_string());
                    }

                    // Rebuild agent with new prompt mode
                    let model = client.completion_model(session.model.to_string());
                    agent = crate::provider::build_agent(
                        model,
                        cli,
                        cfg,
                        context,
                        permission.clone(),
                        ask_tx.clone(),
                        question_tx.clone(),
                        plan_tx.clone(),
                        bg_store.clone(),
                        #[cfg(feature = "lsp")]
                        lsp_manager.clone(),
                        sandbox.clone(),
                        #[cfg(feature = "mcp")]
                        mcp_manager,
                        #[cfg(feature = "semantic")]
                        semantic_manager,
                    )
                    .await;

                    let _ = plan_req.reply.send(PlanSwitchResponse::Accepted);
                    renderer.write_line(
                        &format!("  switched to {}", label),
                        Color::Green,
                    )?;

                    // Re-render the session to show new prompt mode
                    if !cli.print {
                        if let Err(e) = render_session(&mut renderer, session, cli, cfg, context) {
                            renderer.write_line(
                                &format!("render error: {}", e),
                                resolve_color(c_error(), cli.no_color),
                            )?;
                        }
                    }
                } else {
                    let _ = plan_req.reply.send(PlanSwitchResponse::Rejected);
                }

                renderer.render_viewport()?;
                renderer.draw_bottom(
                    &input,
                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                    is_running,
                )?;
                if let Some(ref picker) = input.picker {
                    picker.draw(renderer.input_top_row())?;
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(200)), if is_running => {
                renderer.draw_bottom(
                    &input,
                    &with_queue(StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()), interjection_queue.len()),
                    is_running,
                )?;
                if let Some(ref picker) = input.picker {
                    picker.draw(renderer.input_top_row())?;
                }
            }
            else => {
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            }
        }
    }

    Ok(())
}

fn suggest_pattern(tool: &str, input: &str) -> String {
    match tool {
        "bash" => {
            let first = input.split_whitespace().next().unwrap_or("*");
            format!("{} *", first)
        }
        "read" | "write" | "edit" | "list_dir" => {
            let path = std::path::Path::new(input);
            let parent = path
                .parent()
                .map(|p| p.to_string_lossy())
                .unwrap_or(std::borrow::Cow::Borrowed("*"));
            if parent.is_empty() {
                "**".to_string()
            } else {
                format!("{}/*", parent)
            }
        }
        "grep" | "find_files" => {
            let first = input.split_whitespace().next().unwrap_or("*");
            format!("{}*", first)
        }
        _ => "*".to_string(),
    }
}

/// Close the in-flight tool chamber if one is open. Used at every
/// site that fires without going through `render_tool_output` (which
/// closes the chamber itself): permission alerts, agent errors,
/// interjections, fresh `ToolCall` events when the previous chamber
/// wasn't terminated by a `ToolResult`. Idempotent — calling twice
/// emits one bottom border at most.
fn close_tool_chamber_if_open(
    renderer: &mut Renderer,
    last_tool_name: &mut Option<String>,
) -> anyhow::Result<()> {
    if last_tool_name.is_some() {
        let (frame_w, inner) = chamber_widths(renderer);
        // Abnormal close: this helper is only called when the tool's
        // chamber is closing without a `ToolResult` (permission
        // denied, interjected mid-execution, agent error, fresh tool
        // call before the previous one finished). Surface that with
        // a CRT-static "no signal" row so the empty chamber isn't a
        // mute box. Two textured rows + one labelled row inside the
        // chamber give it a distinct shape from a normal output
        // chamber.
        renderer.write_line(&static_row(inner, 0), theme::dim())?;
        renderer.write_line(
            &chamber_row_centered("░▒▓  NO OUTPUT  ▓▒░", inner),
            theme::dim(),
        )?;
        renderer.write_line(
            &chamber_row_centered("tool denied · aborted · no result", inner),
            theme::dim(),
        )?;
        renderer.write_line(&static_row(inner, 1), theme::dim())?;
        renderer.write_line(&chamber_bottom(frame_w), theme::dim())?;
        *last_tool_name = None;
    }
    Ok(())
}

/// Produce a "CRT signal noise" row inside a chamber: a deterministic
/// `░▒▓` glyph mix padded to inner width. The `seed` selects between
/// two pre-baked patterns so top vs bottom static rows differ slightly
/// — the eye reads it as continuous noise rather than a duplicated row.
fn static_row(inner: usize, seed: usize) -> String {
    let glyphs = [
        ['░', '▒', '░', '▓', '░', '▒', '▒', '░', '▓', '▒'],
        ['▒', '░', '▓', '░', '▒', '▓', '░', '▒', '░', '▓'],
    ];
    let pattern = &glyphs[seed % 2];
    // Body fills `inner` chars (the chamber inner is already the
    // padded content width); the `│ ` and ` │` borders sit outside.
    let body: String = (0..inner).map(|i| pattern[i % pattern.len()]).collect();
    format!("│{}│", body)
}

/// `│   <content centered to inner>   │` — pad text on both sides so
/// it sits horizontally centered within the chamber inner width.
fn chamber_row_centered(content: &str, inner: usize) -> String {
    let len = content.chars().count();
    if len + 2 >= inner {
        return chamber_row(content, inner);
    }
    let pad = inner.saturating_sub(len + 2);
    let left = pad / 2;
    let right = pad - left;
    format!("│ {}{}{} │", " ".repeat(left), content, " ".repeat(right))
}

fn render_tool_output(
    renderer: &mut Renderer,
    output: &str,
    max_chars: usize,
) -> anyhow::Result<()> {
    let sanitized = sanitize_output(output);
    let char_count = sanitized.chars().count();
    let body: String = if char_count <= max_chars {
        sanitized.into_string()
    } else {
        sanitized.chars().take(max_chars).collect()
    };
    // Tool output renders inside a closed rounded chamber:
    //   ╭─ READ ─ /path
    //   │ contents ...                    │
    //   ╰─────────────────────────────────╯
    // Lines are padded/truncated to a fixed inner width so the right
    // border stays aligned across the chamber.
    let (frame_w, inner) = chamber_widths(renderer);
    for line in body.lines() {
        renderer.write_line(&chamber_row(line, inner), theme::result())?;
    }
    if char_count > max_chars {
        let remaining = char_count - max_chars;
        let note = format!("░ +{} chars truncated", remaining);
        renderer.write_line(&chamber_row(&note, inner), theme::dim())?;
    }
    renderer.write_line(&chamber_bottom(frame_w), theme::dim())?;
    Ok(())
}

/// Standard tool-chamber widths derived from the renderer's content
/// area. Capped at 120 so very wide terminals don't produce sprawling
/// chambers that overwhelm the content.
fn chamber_widths(renderer: &Renderer) -> (usize, usize) {
    let term_w = renderer.line_width().max(20);
    let frame_w = term_w.min(120);
    let inner = frame_w.saturating_sub(4); // `│ ` + ` │`
    (frame_w, inner)
}

/// `╰────────────╯` footer of a tool chamber, sized to `frame_w`.
fn chamber_bottom(frame_w: usize) -> String {
    format!("╰{}╯", "─".repeat(frame_w.saturating_sub(2)))
}

/// `│ content (truncated/padded to inner) │` row of a tool chamber.
fn chamber_row(content: &str, inner: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    let trimmed: String = if chars.len() <= inner {
        chars.iter().collect()
    } else if inner == 0 {
        String::new()
    } else {
        let mut out: String = chars[..inner.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    };
    let pad = inner.saturating_sub(trimmed.chars().count());
    format!("│ {}{} │", trimmed, " ".repeat(pad))
}

/// Background-tinted chamber row for diff `+`/`-` lines. Emits raw
/// SGR `48;5;{bg}` background sequence inside the row so the diff
/// tint fills the inner width; the left + right border glyphs sit
/// outside the bg span so they keep the chamber color.
///
/// Opencode uses subtly-tinted backgrounds (`tint(bg, green, 0.15)`
/// etc.) to mark added/removed lines without overwhelming the
/// scanability. We approximate that with the 256-color palette:
/// dim green (22) for adds, dim red (52) for removes.
fn chamber_row_with_bg(content: &str, inner: usize, bg_idx: u8) -> String {
    let chars: Vec<char> = content.chars().collect();
    let trimmed: String = if chars.len() <= inner {
        chars.iter().collect()
    } else if inner == 0 {
        String::new()
    } else {
        let mut out: String = chars[..inner.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    };
    let pad = inner.saturating_sub(trimmed.chars().count());
    format!(
        "│ \x1b[48;5;{}m{}{}\x1b[49m │",
        bg_idx,
        trimmed,
        " ".repeat(pad),
    )
}

fn update_search(renderer: &Renderer, query: &str, matches: &mut Vec<usize>, selected: &mut usize) {
    if query.is_empty() {
        matches.clear();
        return;
    }
    let query_lower = query.to_lowercase();
    let lines = renderer.buffer_lines();
    *matches = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.to_lowercase().contains(&query_lower))
        .map(|(i, _)| i)
        .collect();
    *selected = 0;
}

fn draw_search_bar(query: &str, matches: &[usize], selected: usize) -> std::io::Result<()> {
    use crossterm::style::{ResetColor, SetForegroundColor};
    use crossterm::terminal::{Clear, ClearType};
    use std::io::Write;

    let mut stdout = std::io::stdout();
    let count = matches.len();
    let indicator = if count > 0 {
        format!("{}/{}", selected.saturating_add(1).min(count), count)
    } else {
        "0/0".to_string()
    };
    let bar = format!("Search: {} [{}]", query, indicator);
    crossterm::execute!(stdout, Clear(ClearType::CurrentLine))?;
    crossterm::execute!(stdout, SetForegroundColor(theme::accent()))?;
    write!(stdout, "\r\n")?;
    write!(stdout, "{}", bar)?;
    crossterm::execute!(stdout, ResetColor)?;
    Ok(())
}

fn open_rewind_picker(session: &Session, picker: &mut ListPicker) {
    let prompts: Vec<String> = session
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .rev()
        .take(20)
        .map(|m| {
            let truncated: String = m.content.chars().take(80).collect();
            if truncated.chars().count() >= 80 {
                format!("{}...", truncated)
            } else {
                truncated
            }
        })
        .collect();
    picker.activate("Rewind to:", prompts);
}

fn rewind_session(
    session: &mut Session,
    idx: usize,
    renderer: &mut Renderer,
) -> anyhow::Result<()> {
    let user_indices: Vec<usize> = session
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == MessageRole::User)
        .map(|(i, _)| i)
        .collect();

    let target = user_indices.len().saturating_sub(idx + 1);
    if let Some(&msg_idx) = user_indices.get(target) {
        let removed = session.messages.len() - msg_idx;
        session.messages.truncate(msg_idx);
        session.total_estimated_tokens = session.messages.iter().map(|m| m.estimated_tokens).sum();
        renderer.write_line(&format!("rewound {} message(s)", removed), theme::accent())?;
    }
    Ok(())
}

async fn run_shell_command(cmd: &str, sandbox: &Sandbox) -> anyhow::Result<String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        sandbox.wrap_command(cmd).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Command timed out after 120s"))??;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let mut result = stdout;
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&stderr);
    }
    let exit_code = output.status.code().unwrap_or(-1);
    if exit_code != 0 {
        result.push_str(&format!("\nExit code: {}", exit_code));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression H1: lifecycle line for a failed task previously embedded the
    // raw error string. Renderer.write_line splits on '\n', so a multi-line
    // error broke the line layout (color reset, closing ']' on its own row).
    // sanitize_single_line must collapse newlines into spaces.
    #[test]
    fn sanitize_replaces_newlines_with_space() {
        let s = sanitize_single_line("line one\nline two\nline three", 100);
        assert_eq!(s, "line one line two line three");
        assert!(!s.contains('\n'));
    }

    #[test]
    fn sanitize_replaces_carriage_return_and_tab() {
        let s = sanitize_single_line("a\rb\tc", 100);
        assert_eq!(s, "a b c");
    }

    // Regression: ANSI escape sequences (ESC = 0x1B) would otherwise be
    // emitted verbatim and corrupt terminal state.
    #[test]
    fn sanitize_strips_ansi_escape() {
        let s = sanitize_single_line("hello \x1b[31mred\x1b[0m world", 100);
        assert!(!s.contains('\x1b'));
        assert!(s.contains("hello"));
        assert!(s.contains("world"));
    }

    // Other ASCII control chars (bell, backspace, etc.) are also stripped.
    #[test]
    fn sanitize_strips_other_controls() {
        let s = sanitize_single_line("a\x07b\x08c\x00d", 100);
        // Each control disappears; visible chars remain in order.
        assert_eq!(s, "abcd");
    }

    #[test]
    fn sanitize_truncates_at_char_limit() {
        let s = sanitize_single_line(&"x".repeat(200), 50);
        // 50 x's + ellipsis.
        assert_eq!(s.chars().count(), 51);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn sanitize_does_not_truncate_when_within_limit() {
        let s = sanitize_single_line("hello", 100);
        assert_eq!(s, "hello");
        assert!(!s.ends_with('…'));
    }

    // Multibyte content counts by chars, not bytes, and remains intact.
    #[test]
    fn sanitize_handles_utf8_correctly() {
        let s = sanitize_single_line("🦀🦀🦀\n🦀🦀", 100);
        assert_eq!(s, "🦀🦀🦀 🦀🦀");
    }

    // Truncation at a multibyte boundary must produce valid UTF-8.
    #[test]
    fn sanitize_truncation_does_not_split_multibyte() {
        let s = sanitize_single_line("🦀🦀🦀🦀🦀", 3);
        // 3 emojis + ellipsis. No broken bytes.
        assert_eq!(s.chars().count(), 4);
        assert!(s.ends_with('…'));
        // Round-trip as &str succeeds.
        let _ = s.as_str();
    }

    #[test]
    fn with_queue_hides_zero_count() {
        // No interjections waiting → status line unchanged so the user
        // doesn't see ambient "q:0" noise during normal operation.
        let s = with_queue("ready".to_string(), 0);
        assert_eq!(s, "ready");
    }

    #[test]
    fn with_queue_appends_count() {
        let s = with_queue("running".to_string(), 3);
        assert!(s.ends_with("q:3"));
        assert!(s.starts_with("running"));
    }
}
