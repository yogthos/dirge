mod events;
pub(crate) mod input;
mod markdown;
pub(crate) mod picker;
mod renderer;
mod slash;
mod status;
mod terminal;

use compact_str::CompactString;
use crossterm::event;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::style::Color;
use tokio::sync::mpsc;

use crate::agent::tools::question::{QuestionReceiver, QuestionResponse};
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

const C_AGENT: Color = Color::White;
const C_ERROR: Color = Color::Red;
const C_TOOL: Color = Color::Yellow;
const C_PERM: Color = Color::Magenta;

#[inline]
pub(crate) fn resolve_color(color: Color, monochrome: bool) -> Color {
    if monochrome {
        let _ = color;
        Color::Reset
    } else {
        color
    }
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
        "find_files" => &["pattern"],
        "bash" => &["command"],
        "question" => &["questions"],
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
    sandbox: Sandbox,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    #[cfg(feature = "plugin")] plugin_manager: Option<
        &std::sync::Arc<std::sync::Mutex<PluginManager>>,
    >,
) -> anyhow::Result<()> {
    let _guard = TerminalGuard::new()?;

    let mut renderer = Renderer::new()?;
    renderer.set_monochrome(cli.no_color);
    let mut input = InputEditor::new();
    input.set_monochrome(cli.no_color);
    let mut is_running = false;
    let mut agent_rx: Option<mpsc::Receiver<AgentEvent>> = None;
    // Handle to the background agent task. Held alongside `agent_rx` so the
    // UI can abort in-flight work on Ctrl+C/D/Esc — otherwise tools keep
    // running and permission prompts arrive after the user has interrupted.
    let mut agent_abort: Option<tokio::task::JoinHandle<()>> = None;
    let mut agent_line_started = false;
    let mut response_buf = String::new();
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
        &StatusLine::render(
            session,
            false,
            0,
            None,
            context.current_prompt_name.as_deref(),
            perm_mode().as_deref(),
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
                Ok(event::Event::Resize(_, _)) => {}
                Err(_) => break,
                _ => {}
            }
        }
    });

    loop {
        tokio::select! {
            Some(ev) = user_rx.recv() => {
                match ev {
                    UserEvent::ScrollUp => {
                        renderer.scroll_line_up();
                        renderer.render_viewport()?;
                        renderer.draw_bottom(
                            &input,
                            &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                            is_running,
                        )?;
                        continue;
                    }
                    UserEvent::ScrollDown => {
                        renderer.scroll_line_down();
                        renderer.render_viewport()?;
                        renderer.draw_bottom(
                            &input,
                            &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                            is_running,
                        )?;
                        continue;
                    }
                    UserEvent::MouseDown { row, col: _ } => {
                        if row < renderer.visible_lines() as u16
                            && let Some(idx) = renderer.buffer_line_at_row(row) {
                                renderer.selection_active = true;
                                renderer.selection_start = Some(idx);
                                renderer.selection_end = Some(idx);
                                renderer.render_viewport()?;
                                renderer.draw_bottom(
                                    &input,
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                    is_running,
                                )?;
                            }
                        continue;
                    }
                    UserEvent::MouseDrag { row, col: _ } => {
                        if renderer.selection_active
                            && let Some(idx) = renderer.buffer_line_at_row(row) {
                                renderer.selection_end = Some(idx);
                                renderer.render_viewport()?;
                                renderer.draw_bottom(
                                    &input,
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                    is_running,
                                )?;
                            }
                        continue;
                    }
                    UserEvent::Paste(text) => {
                        input.handle_paste(&text);
                        renderer.draw_bottom(
                            &input,
                            &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                            is_running,
                        )?;
                        continue;
                    }
                    UserEvent::MouseUp { row, col: _ } => {
                        if renderer.selection_active {
                            if let Some(idx) = renderer.buffer_line_at_row(row) {
                                renderer.selection_end = Some(idx);
                            }
                            if let Some(text) = renderer.selected_text() {
                                copy_to_clipboard(&text);
                            }
                            renderer.clear_selection();
                            renderer.render_viewport()?;
                            renderer.draw_bottom(
                                &input,
                                &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                    is_running,
                                )?;
                                continue;
                            }
                            if search_active {
                                search_active = false;
                                renderer.render_viewport()?;
                                renderer.draw_bottom(
                                    &input,
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                    is_running,
                                )?;
                                continue;
                            }
                            if is_running {
                                is_running = false;
                                if let Some(h) = agent_abort.take() { h.abort(); }
                                agent_rx = None;
                                #[cfg(feature = "loop")]
                                if let Some(ref mut ls) = loop_state {
                                    ls.active = false;
                                    loop_label = None;
                                }
                                renderer.write_line("interrupted", C_ERROR)?;
                                renderer.draw_bottom(
                                    &input,
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                        &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                        &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                is_running,
                            )?;
                            continue;
                        }

                        if renderer.selection_active && key.code == KeyCode::Esc {
                            renderer.clear_selection();
                            renderer.render_viewport()?;
                            renderer.draw_bottom(
                                &input,
                                &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                is_running,
                            )?;
                            continue;
                        }

                        if key.code == KeyCode::Esc && is_running {
                            is_running = false;
                            if let Some(h) = agent_abort.take() { h.abort(); }
                            agent_rx = None;
                            #[cfg(feature = "loop")]
                            if let Some(ref mut ls) = loop_state {
                                ls.active = false;
                                loop_label = None;
                            }
                            renderer.write_line("interrupted (Esc)", C_ERROR)?;
                            renderer.draw_bottom(
                                &input,
                                &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                        &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                        is_running,
                                    )?;
                                    continue;
                                }
                            }
                            last_esc = Some(now);
                            renderer.write_line("Press Esc again to rewind...", Color::DarkGrey)?;
                            renderer.draw_bottom(
                                &input,
                                &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                    is_running,
                                )?;
                                continue;
                            }
                            KeyCode::PageDown => {
                                renderer.scroll_page_down();
                                renderer.render_viewport()?;
                                renderer.draw_bottom(
                                    &input,
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                    is_running,
                                )?;
                                continue;
                            }
                            KeyCode::Home => {
                                renderer.scroll_to_top();
                                renderer.render_viewport()?;
                                renderer.draw_bottom(
                                    &input,
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                    is_running,
                                )?;
                                continue;
                            }
                            KeyCode::End => {
                                renderer.scroll_to_bottom()?;
                                renderer.draw_bottom(
                                    &input,
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                renderer.write_line("loop active: /loop stop to cancel", C_ERROR)?;
                                renderer.draw_bottom(
                                    &input,
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                    is_running,
                                )?;
                                continue;
                            }
                            if renderer.is_scrolling() {
                                renderer.scroll_to_bottom()?;
                            }
                            if let Some(prefix) = shell::parse_shell_prefix(&text) {
                                if is_running {
                                    renderer.write_line("agent is busy, wait or interrupt first", C_ERROR)?;
                                    renderer.draw_bottom(
                                        &input,
                                        &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                        is_running,
                                    )?;
                                    continue;
                                }
                                for line in text.lines() {
                                    let safe_line = sanitize_output(line);
                                    renderer.write_line(&format!("> {}", safe_line), Color::Green)?;
                                }
                                renderer.write_line("", Color::White)?;
                                match prefix {
                                    shell::ShellPrefix::Visible(cmd) => {
                                        match run_shell_command(&cmd, &sandbox).await {
                                            Ok(output) => {
                                                renderer.write_line(&output, Color::DarkGrey)?;
                                                let msg = format!("I ran: $ {}\n\nOutput:\n{}", cmd, output);
                                                let history = crate::agent::runner::convert_history(session);
                                                session.add_message(MessageRole::User, &msg);
                                                let runner = agent.clone().spawn_runner(msg, history);
                                                agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
                                                is_running = true;
                                            }
                                            Err(e) => {
                                                renderer.write_line(&format!("shell error: {}", e), C_ERROR)?;
                                            }
                                        }
                                    }
                                    shell::ShellPrefix::Invisible(cmd) => {
                                        match run_shell_command(&cmd, &sandbox).await {
                                            Ok(output) => {
                                                renderer.write_line(&output, Color::DarkGrey)?;
                                            }
                                            Err(e) => {
                                                renderer.write_line(&format!("shell error: {}", e), C_ERROR)?;
                                            }
                                        }
                                    }
                                }
                                renderer.draw_bottom(
                                    &input,
                                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                    is_running,
                                )?;
                                continue;
                            }
                            if text.starts_with('/') {
                                if is_running && !matches!(
                                    text.split_whitespace().next().unwrap_or(""),
                                    "/quit" | "/help" | "/reasoning"
                                ) {
                                    renderer.write_line("agent is busy — wait, interrupt (Ctrl+C), or use /quit", C_ERROR)?;
                                    renderer.draw_bottom(
                                        &input,
                                        &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                                        is_running,
                                    )?;
                                    continue;
                                }
                                for line in text.lines() {
                                    let safe_line = sanitize_output(line);
                                    renderer.write_line(&format!("> {}", safe_line), Color::Green)?;
                                }
                                renderer.write_line("", Color::White)?;
                                let result = handle_slash(&text, &mut agent, &client, &mut renderer, session, cli, cfg, context, &mut show_reasoning, &mut is_running, &mut input, &permission, &ask_tx, &mut todo_tools_enabled, &sandbox, #[cfg(feature = "loop")] &mut loop_state, #[cfg(feature = "mcp")] mcp_manager, #[cfg(feature = "semantic")] semantic_manager).await;
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
                                            &permission, &ask_tx, &sandbox,
                                            #[cfg(feature = "mcp")] mcp_manager,
                                            #[cfg(feature = "semantic")] semantic_manager,
                                        ).await;
                                        if let Err(e) = compress_result {
                                            renderer.write_line(&format!("compress error: {}", e), C_ERROR)?;
                                        }
                                        if let Err(e) = crate::session::storage::save_session(session) {
                                            renderer.write_line(
                                                &format!("warning: failed to save session: {}", e),
                                                C_ERROR,
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
                                            let runner = agent.clone().spawn_runner(prompt, history);
                                            agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
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
                                                sandbox.clone(),
                                                #[cfg(feature = "mcp")] mcp_manager,
                                                #[cfg(feature = "semantic")] semantic_manager,
                                            ).await;
                                            render_session(&mut renderer, session, cli, cfg, context)?;
                                            renderer.write_line(
                                                &format!("returned to main repo at {}", main_path),
                                                C_AGENT,
                                            )?;
                                        }
                                    }
                                    Err(e) => {
                                        if e.downcast_ref::<std::io::Error>().is_some_and(|e: &std::io::Error| e.kind() == std::io::ErrorKind::Interrupted) {
                                            break;
                                        }
                                        renderer.write_line(&format!("error: {}", e), C_ERROR)?;
                                    }
                                    Ok(_) => {
                                        if !cli.no_session
                                            && let Err(e) = crate::session::storage::save_session(session)
                                        {
                                            renderer.write_line(
                                                &format!("warning: failed to save session: {}", e),
                                                C_ERROR,
                                            )?;
                                        }
                                        #[cfg(feature = "loop")]
                                        if let Some(ref mut ls) = loop_state
                                            && ls.active && ls.iteration == 0 && !is_running
                                        {
                                            ls.iteration = 1;
                                            let prompt = ls.build_prompt();
                                            let runner = agent.clone().spawn_runner(prompt, Vec::new());
                                            agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
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
                                        C_ERROR,
                                    )?;
                                }
                            } else {
                                for line in text.lines() {
                                    let safe_line = sanitize_output(line);
                                    renderer.write_line(&format!("> {}", safe_line), Color::Green)?;
                                }
                                renderer.write_line("", Color::White)?;

                                let history = crate::agent::runner::convert_history(session);

                                #[allow(unused_mut)]
                                let mut plugin_hint: Option<String> = None;
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
                                                    Color::DarkGrey,
                                                )?;
                                            }
                                            plugin_hint = Some(results.join("\n"));
                                        }
                                        Ok(_) => {}
                                        Err(e) => {
                                            renderer.write_line(
                                                &format!("[plugin] on-prompt error: {e}"),
                                                C_ERROR,
                                            )?;
                                        }
                                    }
                                    // A plugin hook may queue a follow-up prompt via
                                    // harness/request-prompt; pick it up here.
                                    if let Some(pending) = mgr.take_pending_prompt() {
                                        plugin_hint = Some(pending);
                                    }
                                }

                                let prompt = if let Some(hint) = plugin_hint {
                                    format!("{}\n\n{}", hint, text)
                                } else {
                                    text.to_string()
                                };

                                let runner = agent.clone().spawn_runner(prompt, history);
                                agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
                                is_running = true;

                                session.add_message(MessageRole::User, &text);
                            }
                        }
                        renderer.draw_bottom(
                            &input,
                            &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                        if !show_reasoning {
                            continue;
                        }
                        if !agent_line_started {
                            renderer.write("< ", Color::DarkMagenta)?;
                            agent_line_started = true;
                        }
                        let safe = sanitize_output(&text);
                        renderer.write(&safe, Color::DarkMagenta)?;
                        was_reasoning = true;
                    }
                    AgentEvent::Token(text) => {
                        if was_reasoning {
                            renderer.write_line("", Color::White)?;
                            agent_line_started = false;
                            was_reasoning = false;
                            response_buf.clear();
                            response_start_line = None;
                        }
                        let safe = sanitize_output(&text);
                        response_buf.push_str(&safe);

                        if response_buf.is_empty() {
                            continue;
                        }

                        let max_width = renderer.line_width();
                        let mut styled =
                            crate::ui::markdown::markdown_to_styled(&response_buf, max_width);

                        if !styled.is_empty() {
                            styled[0].text =
                                CompactString::from(format!("< {}", styled[0].text));
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
                        last_tool_name = Some(name.to_string());
                        if agent_line_started {
                            renderer.write_line("", Color::White)?;
                            agent_line_started = false;
                        }
                        response_buf.clear();
                        response_start_line = None;
                        let line = format!("◈ {}", format_tool_call_summary(&name, &args));
                        renderer.write_line(&sanitize_output(&line), C_TOOL)?;

                        #[cfg(feature = "plugin")]
                        if let Some(pm) = plugin_manager {
                            // Pass args as a JSON string so the Janet
                            // parser never has to interpret arbitrary
                            // JSON tokens (`:`, `,`, `null`).
                            let args_json = args.to_string();
                            let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
                            if let Err(e) = mgr.dispatch(
                                "on-tool-start",
                                &format!(
                                    "@{{:tool \"{}\" :args \"{}\"}}",
                                    crate::plugin::escape_janet_string(&name),
                                    crate::plugin::escape_janet_string(&args_json),
                                ),
                            ) {
                                renderer.write_line(
                                    &format!("[plugin] on-tool-start error: {e}"),
                                    C_ERROR,
                                )?;
                            }
                        }
                    }
                    AgentEvent::ToolResult { output } => {
                        let show_details = cfg.show_tool_details.unwrap_or(true);
                        let max_chars = cfg.resolve_tool_result_max_chars();
                        let show_diff = cfg.resolve_show_edit_diff();

                        #[cfg(feature = "plugin")]
                        if let Some(pm) = plugin_manager {
                            let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
                            if let Err(e) = mgr.dispatch(
                                "on-tool-end",
                                &format!(
                                    "@{{:output \"{}\"}}",
                                    crate::plugin::escape_janet_string(&output)
                                ),
                            ) {
                                renderer.write_line(
                                    &format!("[plugin] on-tool-end error: {e}"),
                                    C_ERROR,
                                )?;
                            }
                        }

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
                                    // Show non-diff prefix
                                    for l in &lines[..pre] {
                                        if !l.is_empty() {
                                            renderer.write_line(
                                                &format!("◈ {}", sanitize_output(l)),
                                                Color::DarkGrey,
                                            )?;
                                        }
                                    }
                                    // Show colorized diff
                                    for l in &lines[pre..] {
                                        if l.starts_with("--- ") || l.starts_with("+++ ") {
                                            renderer.write_line(l, Color::Cyan)?;
                                        } else if l.starts_with("@@") {
                                            renderer.write_line(l, Color::DarkCyan)?;
                                        } else if l.starts_with('+') {
                                            renderer.write_line(l, Color::Green)?;
                                        } else if l.starts_with('-') {
                                            renderer.write_line(l, Color::Red)?;
                                        } else {
                                            renderer.write_line(
                                                &sanitize_output(l),
                                                Color::DarkGrey,
                                            )?;
                                        }
                                    }
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
                                            Color::DarkGrey,
                                        )?;
                                    }
                                    plugin_followup = Some(results.join("\n"));
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    renderer.write_line(
                                        &format!("[plugin] on-response error: {e}"),
                                        C_ERROR,
                                    )?;
                                }
                            }
                            // Check for pending prompts queued by on-response
                            if let Some(pending) = mgr.take_pending_prompt() {
                                plugin_followup = Some(pending);
                            }
                            mgr.store_response(&response);
                        }

                        if !response_buf.is_empty() {
                            let max_width = renderer.line_width();
                            let mut styled = crate::ui::markdown::markdown_to_styled(
                                &response_buf,
                                max_width,
                            );
                            if !styled.is_empty() {
                                styled[0].text =
                                    CompactString::from(format!("< {}", styled[0].text));
                            }
                            if let Some(start) = response_start_line {
                                renderer.replace_from(start, styled);
                                renderer.render_viewport()?;
                            }
                        } else if !agent_line_started {
                            renderer.write("< ", C_AGENT)?;
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
                            renderer.write_line("auto-compacting...", Color::DarkGrey)?;
                            let compress_result = handle_compress(
                                None,
                                &mut agent, &client, &mut renderer, session, cli, cfg, context,
                                &permission, &ask_tx, &sandbox,
                                #[cfg(feature = "mcp")] mcp_manager,
                                #[cfg(feature = "semantic")] semantic_manager,
                            ).await;
                            if let Err(e) = compress_result {
                                renderer.write_line(&format!("auto-compact error: {}", e), C_ERROR)?;
                            }
                        }

                        if !cli.no_session
                            && let Err(e) = crate::session::storage::save_session(session)
                        {
                            renderer.write_line(
                                &format!("warning: failed to save session: {}", e),
                                C_ERROR,
                            )?;
                        }
                        is_running = false;
                        if let Some(h) = agent_abort.take() { h.abort(); }
                        agent_rx = None;

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
                                    followup_prompt,
                                    crate::agent::runner::convert_history(session),
                                );
                                agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
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
                                        C_AGENT,
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
                                    let runner = agent.clone().spawn_runner(prompt, Vec::new());
                                    agent_rx = Some(runner.event_rx);
                                                agent_abort = Some(runner.task);
                                    is_running = true;
                                    loop_label = Some(ls.iteration_label());
                                    renderer.write_line(
                                        &format!("[loop] launching {}", ls.iteration_label()),
                                        C_AGENT,
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
                                        sandbox.clone(),
                                        #[cfg(feature = "mcp")] mcp_manager,
                                        #[cfg(feature = "semantic")] semantic_manager,
                                    ).await;
                                    render_session(&mut renderer, session, cli, cfg, context)?;
                                    renderer.write_line(
                                        &format!("merged and returned to main repo at {}", main_path),
                                        C_AGENT,
                                    )?;
                                }
                                Err(e) => {
                                    renderer.write_line(
                                        &format!("warning: failed to change back to main repo: {}", e),
                                        C_ERROR,
                                    )?;
                                }
                            }
                        }
                    }
                    AgentEvent::Error(e) => {
                        was_reasoning = false;
                        last_tool_name = None;
                        let safe = sanitize_output(&e);
                        renderer.write_line(&format!("error: {}", safe), C_ERROR)?;

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
                                    C_ERROR,
                                )?;
                            }
                        }

                        is_running = false;
                        if let Some(h) = agent_abort.take() { h.abort(); }
                        agent_rx = None;
                        agent_line_started = false;
                        response_buf.clear();
                        response_start_line = None;
                    }
                }
                renderer.draw_bottom(
                    &input,
                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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

                renderer.write_line(
                    &format!("[permission] {}: {}", ask_req.tool, ask_req.input),
                    C_PERM,
                )?;
                renderer.write_line(
                    "  (y) allow once  (a) allow always  (n) deny  (ESC) abort",
                    C_PERM,
                )?;

                let decision = loop {
                    tokio::select! {
                        Some(ev) = user_rx.recv() => {
                            if let UserEvent::Key(key) = ev {
                                match key.code {
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
                                }
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
                                C_ERROR,
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
                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                    is_running,
                )?;
                if let Some(ref picker) = input.picker {
                    picker.draw(renderer.input_top_row())?;
                }
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
                            C_PERM,
                        )?;
                    }
                    renderer.write_line(
                        &format!("\n[question {}] {}", qi + 1, question.question),
                        C_PERM,
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
                                color: C_PERM,
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
                                color: C_PERM,
                            });
                        }
                        lines.push(LineEntry {
                            text: compact_str::CompactString::new(if multi {
                                "  ↑↓ navigate  Space toggle  Enter confirm  Esc reject all"
                            } else {
                                "  ↑↓ navigate  Enter select  Esc reject all"
                            }),
                            color: C_PERM,
                        });

                        // Replace previous render with updated options
                        renderer.replace_from(anchor, lines);
                        renderer.render_viewport()?;
                        renderer.draw_bottom(
                            &input,
                            &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                    renderer.write_line("  enter your answer:", C_PERM)?;
                                    let input_anchor = renderer.buffer_len();
                                    loop {
                                        renderer.replace_from(
                                            input_anchor,
                                            vec![LineEntry {
                                                text: compact_str::CompactString::new(
                                                    &format!("  > {}", buf),
                                                ),
                                                color: C_PERM,
                                            }],
                                        );
                                        renderer.render_viewport()?;
                                        renderer.draw_bottom(
                                            &input,
                                            &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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
                                            C_PERM,
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
                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
                    is_running,
                )?;
                if let Some(ref picker) = input.picker {
                    picker.draw(renderer.input_top_row())?;
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(200)), if is_running => {
                renderer.draw_bottom(
                    &input,
                    &StatusLine::render(session, is_running, 0, loop_label.as_deref(), context.current_prompt_name.as_deref(), perm_mode().as_deref()),
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

fn render_tool_output(
    renderer: &mut Renderer,
    output: &str,
    max_chars: usize,
) -> anyhow::Result<()> {
    let sanitized = sanitize_output(output);
    let char_count = sanitized.chars().count();
    if char_count <= max_chars {
        renderer.write_line(&sanitized, Color::DarkGrey)?;
    } else {
        let preview: String = sanitized.chars().take(max_chars).collect();
        let remaining = char_count - max_chars;
        renderer.write_line(&preview, Color::DarkGrey)?;
        renderer.write_line(
            &format!("  [truncated: {} more chars]", remaining),
            Color::DarkCyan,
        )?;
    }
    Ok(())
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
    use crossterm::style::{Color, ResetColor, SetForegroundColor};
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
    crossterm::execute!(stdout, SetForegroundColor(Color::Cyan))?;
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
        renderer.write_line(&format!("rewound {} message(s)", removed), Color::Cyan)?;
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
