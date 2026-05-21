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

/// Capture whatever assistant text had streamed in before an abort,
/// store it on the session as the assistant's reply (with a
/// `[interrupted by user]` trailer so the LLM sees on next turn
/// that it was cut off), and clear `response_buf`. Returns `true`
/// when a partial was actually stashed; `false` when nothing had
/// streamed yet (no-op).
///
/// `tool_calls_in_turn` is the count of `AgentEvent::ToolCall` events
/// the UI saw during the aborted turn. When non-zero, the trailer
/// notes that tool calls ran but their results are NOT in the
/// preserved text (since only Token events accumulate into
/// `response_buf`). Without this hint, the next turn's LLM context
/// would treat the partial as a complete reply and could re-run
/// side-effecting tools.
///
/// Mirrors opencode's `finalizeInterruptedAssistant` in
/// `packages/opencode/src/session/prompt.ts` — the streamed parts
/// are already on-screen, so the partial is preserved by virtue of
/// being saved into the session rather than discarded. opencode
/// uses `MessageV2.fromError(..., aborted: true)` to annotate the
/// message; dirge appends the trailer as plain text since
/// `SessionMessage` is content-only.
fn capture_partial_on_abort(
    response_buf: &mut String,
    session: &mut crate::session::Session,
    why: &str,
    tool_calls_in_turn: u32,
    tool_calls_buf: &mut Vec<crate::session::ToolCallEntry>,
) -> bool {
    let trimmed = response_buf.trim_end();
    if trimmed.is_empty() && tool_calls_buf.is_empty() {
        response_buf.clear();
        return false;
    }
    let trailer = if tool_calls_in_turn > 0 {
        let noun = if tool_calls_in_turn == 1 {
            "tool call ran"
        } else {
            "tool calls ran"
        };
        format!(
            "[interrupted by user ({}); {} {} in this turn — results not preserved]",
            why, tool_calls_in_turn, noun,
        )
    } else {
        format!("[interrupted by user ({})]", why)
    };
    let stashed = if trimmed.is_empty() {
        trailer
    } else {
        format!("{}\n\n{}", trimmed, trailer)
    };
    // Phase 3: persist the tool-call entries too. Any entry still
    // in Interrupted state at abort time stays Interrupted (the
    // matching ToolResult never arrived). Completed entries keep
    // their state — they ran fully before the user cancelled.
    // `convert_history` will emit tool_result blocks for both
    // states on resume so the LLM never sees orphan tool_use.
    let calls = std::mem::take(tool_calls_buf);
    // Capture the message's token estimate before add_message so we
    // can also bump `total_tokens` in lockstep with
    // `total_estimated_tokens` — matches the Done / Interjected
    // branches which both update total_tokens (a TODO(cost-tracking)
    // placeholder; kept consistent so the abort case doesn't look
    // like a zero-token turn).
    let est = crate::session::Session::estimate_tokens(&stashed);
    session.add_message_with_tool_calls(crate::session::MessageRole::Assistant, &stashed, calls);
    session.total_tokens = session.total_tokens.saturating_add(est);
    response_buf.clear();
    true
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
/// Extract the unquoted, untruncated value for the chamber banner.
/// Picks the most informative single argument for each tool — the
/// path for file ops, the command for bash, etc. Returns `""` for
/// tools without a meaningful single-value summary; the chamber
/// header falls back to the tool name alone.
///
/// Used by the chamber builder, which then left-truncates the
/// value to fill the available banner width (right side carries
/// the meaningful info for paths — filename — so we cut from the
/// left, not the right).
fn format_tool_banner_value(name: &str, args: &serde_json::Value) -> String {
    let obj = match args {
        serde_json::Value::Object(map) => map,
        _ => return String::new(),
    };
    // `apply_patch` is structurally different: its arg is
    // `operations: Vec<PatchOp>` (an array of ops, each with its
    // own path), not a single string. Render "N ops" so the
    // banner has content — degrading to bare "APPLY_PATCH" with
    // dashes was uninformative.
    if name == "apply_patch" {
        let n = obj
            .get("operations")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        return match n {
            0 => String::new(),
            1 => "1 op".to_string(),
            _ => format!("{n} ops"),
        };
    }
    let key = match name {
        "read" | "write" | "edit" | "list_dir" => "path",
        "grep" => "pattern",
        "find_files" | "glob" => "pattern",
        "bash" => "command",
        "question" => "questions",
        "task" => "prompt",
        "task_status" => "task_id",
        _ => return String::new(),
    };
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Build the rounded-chamber top border, left-truncating `value`
/// to fill the available width up to `─╮`. Layout:
///
///   `╭─ TOOL ─ "value"─…─╮`
///
/// - When `value` fits, pad with extra `─` between the closing
///   quote and `─╮` so the border is flush right.
/// - When too long, take the LAST `N` chars and prefix with `…`
///   (so filenames stay readable: paths put the filename on the
///   right). Original PR used right-truncation, which was the
///   wrong direction for paths.
/// - The suffix is a tight `─╮` (no leading space) to match the
///   chamber bottom's `╰────╯` solid-dash style. A previous
///   version used ` ─╮` (leading space) which produced a visible
///   gap `── ─╮` at the right edge that looked like a defect.
fn fit_banner_header(name_upper: &str, value: &str, frame_w: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;

    // Cap the tool name so a pathological long name
    // (e.g. `mcp_tool:long_server:long_function`) doesn't push
    // the prefix past `frame_w` and overflow the chamber. Reserve
    // enough room for the surrounding `╭─ ` + ` ─ ` + suffix +
    // 2 quote chars + at least 1 cell of value. If the name itself
    // is too long, left-truncate with `…` so the part closest to
    // the colon-separated suffix (typically the function name)
    // survives — same rationale as path truncation.
    const FRAME_OVERHEAD: usize = 8; // "╭─ " (3) + " ─ " (3) + "─╮" (2)
    let name_budget = frame_w.saturating_sub(FRAME_OVERHEAD + 3); // 3 = quotes + 1 value cell
    let name_w = name_upper.width();
    let displayed_name: String = if name_w <= name_budget || name_budget == 0 {
        name_upper.to_string()
    } else {
        let tail_budget = name_budget.saturating_sub(1); // for `…`
        let mut tail: Vec<char> = Vec::new();
        let mut used = 0;
        for ch in name_upper.chars().rev() {
            let w = ch.width().unwrap_or(0);
            if used + w > tail_budget {
                break;
            }
            tail.push(ch);
            used += w;
        }
        tail.reverse();
        format!("…{}", tail.into_iter().collect::<String>())
    };

    let prefix = format!("╭─ {} ─ ", displayed_name);
    let suffix = "─╮";
    let prefix_w = prefix.as_str().width();
    let suffix_w = suffix.width();
    // Reserve at least 1 cell for padding-or-truncation marker.
    // If frame_w is so small that even the prefix+suffix don't
    // fit, just emit the prefix + truncated tool name with no
    // value (degraded but doesn't panic).
    if value.is_empty() {
        let used = prefix_w + suffix_w;
        let pad = frame_w.saturating_sub(used);
        return format!("{}{}{}", prefix, "─".repeat(pad), suffix);
    }
    // Budget for `"value"` (the value + 2 quote chars). The
    // chamber needs room for at least the closing suffix; if
    // even that doesn't fit, fall back to no-value.
    let quote_w = 2;
    let value_budget = frame_w.saturating_sub(prefix_w + suffix_w + quote_w);
    if value_budget == 0 {
        let used = prefix_w + suffix_w;
        let pad = frame_w.saturating_sub(used);
        return format!("{}{}{}", prefix, "─".repeat(pad), suffix);
    }

    let value_w = value.width();
    let shown_value = if value_w <= value_budget {
        value.to_string()
    } else {
        // Left-truncate: take the LAST chars that fit, prefixed
        // with `…` (1 cell). Count by display width so emoji /
        // CJK don't break the budget.
        use unicode_width::UnicodeWidthChar;
        let tail_budget = value_budget.saturating_sub(1); // 1 for `…`
        // Walk the value from the END, accumulating chars until
        // we've used `tail_budget` cells.
        let mut tail: Vec<char> = Vec::new();
        let mut used = 0;
        for ch in value.chars().rev() {
            let w = ch.width().unwrap_or(0);
            if used + w > tail_budget {
                break;
            }
            tail.push(ch);
            used += w;
        }
        tail.reverse();
        let tail_str: String = tail.into_iter().collect();
        format!("…{}", tail_str)
    };

    let shown_w = shown_value.as_str().width() + quote_w;
    let total_used = prefix_w + shown_w + suffix_w;
    let pad = frame_w.saturating_sub(total_used);
    format!("{}\"{}\"{}{}", prefix, shown_value, "─".repeat(pad), suffix)
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
    // F20: bounded mpsc::Sender. Multiple interject signals while
    // the runner is mid-call get coalesced — only the first wakeup
    // matters since the runner drains via try_recv() after waking.
    let mut agent_interject: Option<mpsc::Sender<()>> = None;
    let mut agent_line_started = false;
    let mut response_buf = String::new();
    // Count of `AgentEvent::ToolCall` events observed during the
    // current run. Used by `capture_partial_on_abort` so the
    // saved partial's trailer can warn the LLM that tool calls
    // ran but their results aren't in the preserved text. Reset
    // when a new agent run starts (alongside response_buf clear).
    let mut tool_calls_this_run: u32 = 0;
    // Structured tool-call records for the current agent run.
    // Populated from `AgentEvent::ToolCall` (state: Interrupted) and
    // updated to `Completed{result}` on the matching `ToolResult`.
    // Attached to the assistant message on `Done` / `Interjected`,
    // or all remaining pending entries marked Interrupted on abort
    // (Ctrl+C / Esc). Persists to the session JSON; on resume,
    // `convert_history` re-emits each as a structured tool_use +
    // tool_result block so the LLM doesn't re-call the same tools.
    // Mirrors opencode's `ToolPart` lifecycle.
    let mut tool_calls_buf: Vec<crate::session::ToolCallEntry> = Vec::new();
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
                // Sanitize plugin-supplied strings: a misbehaving
                // or malicious plugin could emit ANSI escape codes
                // through `harness/notify`, painting the terminal
                // or moving the cursor. All other LLM/tool output
                // paths go through `sanitize_output`; plugin
                // notifications were the only path bypassing it.
                let safe = sanitize_output(&msg);
                renderer.write_line(&format!("[plugin] {}", safe), color)?;
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
                                // Persist whatever response had streamed in
                                // before the abort. Matches opencode's
                                // `finalizeInterruptedAssistant` pattern in
                                // `packages/opencode/src/session/prompt.ts`:
                                // the partial is already on-screen, so save
                                // it to the session with a `[interrupted by
                                // user]` marker so the next turn's LLM
                                // context shows what was happening. Without
                                // this, the user's next prompt referenced
                                // an invisible reply.
                                let stashed = capture_partial_on_abort(
                                    &mut response_buf,
                                    session,
                                    "Ctrl+C",
                                    tool_calls_this_run,
                                    &mut tool_calls_buf,
                                );
                                // Whether or not we stashed, the run
                                // is over — reset the counter so a
                                // subsequent run starts at zero.
                                tool_calls_this_run = 0;
                                let dropped = interjection_queue.len();
                                interjection_queue.clear();
                                let mut msg = String::from("interrupted");
                                if stashed {
                                    msg.push_str(" — partial reply preserved in session");
                                }
                                if dropped > 0 {
                                    msg.push_str(&format!(
                                        " ({} queued message{} dropped)",
                                        dropped,
                                        if dropped == 1 { "" } else { "s" },
                                    ));
                                }
                                renderer.write_line(&msg, c_error())?;
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
                            // Same partial-capture as Ctrl+C above —
                            // see comment there for the opencode parallel.
                            let stashed = capture_partial_on_abort(
                                &mut response_buf,
                                session,
                                "Esc",
                                tool_calls_this_run,
                                &mut tool_calls_buf,
                            );
                            tool_calls_this_run = 0;
                            let msg = if stashed {
                                "interrupted (Esc) — partial reply preserved in session"
                            } else {
                                "interrupted (Esc)"
                            };
                            renderer.write_line(msg, c_error())?;
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
                                // Queue the message instead of dropping it.
                                // The next loop iteration's prompt-build path
                                // already drains `interjection_queue` and
                                // prepends queued messages, so this is the
                                // natural place to land mid-loop user input.
                                interjection_queue.push_back(text.to_string());
                                renderer.write_line(
                                    "loop active — message queued (will send after current iteration; /loop stop to cancel)",
                                    c_agent(),
                                )?;
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
                                    renderer.write_line(&format!("<you> {}", safe_line), theme::user())?;
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
                                    renderer.write_line(&format!("<you> {}", safe_line), theme::user())?;
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
                                    // F20: try_send so a full channel
                                    // (already-queued wakeup) is a
                                    // no-op rather than blocking the
                                    // UI thread. We only need ONE
                                    // wakeup queued at a time.
                                    let _ = tx.try_send(());
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
                                    renderer.write_line(&format!("<you> {}", safe_line), theme::user())?;
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
                                                // Sanitize plugin output (ANSI injection defense).
                                                let safe = sanitize_output(line);
                                                renderer.write_line(
                                                    &format!("[plugin] {}", safe),
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
                    AgentEvent::ToolCall { id, name, args } => {
                        was_reasoning = false;
                        // Phase 3: persist as structured entry. Start
                        // in Interrupted state so that if the user
                        // aborts before the result arrives, the saved
                        // session captures the right state. The
                        // matching `ToolResult` flips it to Completed.
                        tool_calls_buf.push(crate::session::ToolCallEntry {
                            id: id.to_string(),
                            name: name.to_string(),
                            args: args.clone(),
                            state: crate::session::ToolCallState::Interrupted,
                        });
                        // Track for the abort-trailer warning: when
                        // the user later hits Ctrl+C / Esc, the
                        // saved partial reply notes how many tool
                        // calls ran (and didn't have their results
                        // preserved in the message text).
                        tool_calls_this_run = tool_calls_this_run.saturating_add(1);
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
                        // Blank line BEFORE the chamber top so the eye
                        // has an anchor between dense prior output (a
                        // permission alert + "allowed ..." lines) and
                        // the new tool chamber. Without it, the
                        // chamber's ╭─ tended to sit pressed against
                        // the previous line and on small terminals
                        // the ╭ row could scroll off-screen while the
                        // chamber's content rows stayed visible —
                        // looking like a "cut off at top" chamber.
                        renderer.write_line("", Color::White)?;
                        let raw_value = format_tool_banner_value(&name, &args);
                        let raw_value = sanitize_output(&raw_value).into_string();
                        let (frame_w, _) = chamber_widths(&renderer);
                        let header = fit_banner_header(&upper, &raw_value, frame_w);
                        renderer.write_line(&header, c_tool())?;

                        // Note: on-tool-start fires from HookedToolDyn now,
                        // around the actual tool invocation. The UI no
                        // longer dispatches it here — that would double-
                        // fire the hook per tool call.
                    }
                    AgentEvent::ToolResult { id, output } => {
                        // Phase 3: pair the result with its call.
                        // Prefer id-match; fall back to the most-
                        // recent Interrupted (pending) entry for
                        // providers that don't emit ids.
                        let target = if !id.is_empty() {
                            tool_calls_buf.iter_mut().rev().find(|e| e.id == id.as_str())
                        } else {
                            tool_calls_buf
                                .iter_mut()
                                .rev()
                                .find(|e| matches!(e.state, crate::session::ToolCallState::Interrupted))
                        };
                        if let Some(entry) = target {
                            entry.state = crate::session::ToolCallState::Completed {
                                result: output.to_string(),
                            };
                        }
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
                                            // Filenames in the diff header get
                                            // the same accent as section
                                            // markers elsewhere in chat. Was
                                            // hardcoded `Color::Cyan` which is
                                            // invisible on phosphor (same hue
                                            // as agent text).
                                            renderer.write_line(
                                                &chamber_row(&txt, inner),
                                                theme::accent(),
                                            )?;
                                        } else if l.starts_with("@@") {
                                            // Hunk position markers — use dim
                                            // so they recede behind the +/-
                                            // content lines below.
                                            renderer.write_line(
                                                &chamber_row(&txt, inner),
                                                theme::dim(),
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
                                        // Sanitize plugin output (ANSI injection defense).
                                        let safe = sanitize_output(line);
                                        renderer.write_line(
                                            &format!("[plugin] {}", safe),
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
                        // Phase 3: persist structured tool calls
                        // alongside the assistant text so the next
                        // resume sees the full tool_use/tool_result
                        // pairs in convert_history.
                        session.add_message_with_tool_calls(
                            MessageRole::Assistant,
                            &response,
                            std::mem::take(&mut tool_calls_buf),
                        );
                        // TODO(cost-tracking): `tokens` here is the heuristic
                        // estimate (text.len()/4) and `cost` is always 0.0 —
                        // these accumulate into placeholder fields and won't
                        // reflect actual provider usage / billing until we
                        // pipe rig's `FinalResponse.usage()` through into
                        // `AgentEvent::Done`. Kept as no-op-ish additions so
                        // the wiring is in place when real values arrive.
                        session.total_tokens = session.total_tokens.saturating_add(tokens);
                        session.total_cost += cost;
                        // Run ended cleanly — reset the per-run tool-
                        // call counter so the next user submission
                        // starts at zero. Mirrored in the Interjected
                        // branch + both abort paths below.
                        tool_calls_this_run = 0;
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
                            // Auto-compact failure used to render as a
                            // single dim red line that scrolled past
                            // unnoticed — users kept typing into an
                            // over-full context and saw mysterious
                            // context-length errors next turn. Frame
                            // the warning so it visibly stops the eye
                            // and tells the user what to do next.
                            renderer.write_line("▒░ auto-compacting context ░▒", theme::accent())?;
                            let compress_result = handle_compress(
                                None,
                                &mut agent, &client, &mut renderer, session, cli, cfg, context,
                                &permission, &ask_tx, &bg_store, &sandbox,
                                #[cfg(feature = "mcp")] mcp_manager,
                                #[cfg(feature = "semantic")] semantic_manager,
                            ).await;
                            if let Err(e) = compress_result {
                                renderer.write_line(
                                    "╭─ ⚠ AUTO-COMPACT FAILED ─────────────────────────────╮",
                                    c_error(),
                                )?;
                                // Cap the cause length so a sprawling
                                // multi-line error doesn't blow out the
                                // box's visual rhythm. The full error
                                // is still in the agent's recovery
                                // path; this is for the user-facing
                                // hint only.
                                let cause = {
                                    let s = e.to_string().replace('\n', " ");
                                    if s.chars().count() > 64 {
                                        let mut out: String = s.chars().take(63).collect();
                                        out.push('…');
                                        out
                                    } else {
                                        s
                                    }
                                };
                                renderer.write_line(
                                    &format!("│ cause: {}", cause),
                                    c_error(),
                                )?;
                                renderer.write_line(
                                    "│ context is over the threshold — replies may start",
                                    c_error(),
                                )?;
                                renderer.write_line(
                                    "│ hitting context-length errors. Try /compress",
                                    c_error(),
                                )?;
                                renderer.write_line(
                                    "│ manually, /clear to start fresh, or restart with",
                                    c_error(),
                                )?;
                                renderer.write_line(
                                    "│ a larger context_window in config.",
                                    c_error(),
                                )?;
                                renderer.write_line(
                                    "╰─────────────────────────────────────────────────────╯",
                                    c_error(),
                                )?;
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
                                    .write_line(&format!("<you> {}", safe_line), theme::user())?;
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
                            // Phase 3: same structured persistence
                            // as the Done branch. Any pending entries
                            // (tool calls without a result yet) keep
                            // their Interrupted state — the LLM
                            // sees [Tool execution was interrupted]
                            // tool_result on resume.
                            session.add_message_with_tool_calls(
                                MessageRole::Assistant,
                                &partial_response,
                                std::mem::take(&mut tool_calls_buf),
                            );
                            // TODO(cost-tracking): same caveat as the Done
                            // branch — `tokens` is an estimate, not actual
                            // provider usage. Wire after rig usage plumbing.
                            session.total_tokens = session.total_tokens.saturating_add(tokens);
                        } else {
                            // No partial text but maybe pending tool
                            // calls — drop them; the session already
                            // captured them via prior turns or they
                            // were a single-call abort with no text.
                            tool_calls_buf.clear();
                        }
                        // Run ended (interjection-style) — reset the
                        // per-run tool-call counter alongside the
                        // other per-run state.
                        tool_calls_this_run = 0;
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
                                renderer.write_line(&format!("<you> {}", safe_line), theme::user())?;
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

                // Chamber-vs-alert interleaving:
                //
                // The in-flight tool's chamber TOP was already drawn
                // by the ToolCall handler when the LLM emitted the
                // call. Drawing the alert box directly below would
                // visually orphan that top — the chamber would have
                // no body and no bottom, looking like a broken card.
                //
                // Old behavior (PR #100): leave the chamber open and
                // hope the body lands inside it. In practice the
                // alert renders BETWEEN the chamber top and the
                // chamber body, so the top is visually disconnected
                // from the body that arrives later.
                //
                // New behavior: close the in-flight chamber with a
                // "awaiting permission" footer BEFORE the alert
                // displays. If the user allows, reopen a fresh
                // chamber (matching banner) below the alert so the
                // ToolResult body lands inside it as usual. If the
                // user denies, the chamber is already closed and
                // we add a brief "(denied)" line below.
                let pending_chamber_tool =
                    if let Some(open_name) = last_tool_name.clone() {
                        let (frame_w, inner) = chamber_widths(&renderer);
                        renderer.write_line(
                            &chamber_row("awaiting permission…", inner),
                            theme::dim(),
                        )?;
                        renderer.write_line(&chamber_bottom(frame_w), c_tool())?;
                        // Blank line between the closed chamber and
                        // the alert box so the two boxes don't sit
                        // flush against each other — `╰─╯` directly
                        // followed by `╭─ ⚠ ALERT` reads as one
                        // continuous border.
                        renderer.write_line("", Color::White)?;
                        last_tool_name = None;
                        Some(open_name)
                    } else {
                        None
                    };

                renderer.set_avatar_state(avatar::AvatarState::Alert);
                // Force a bottom-row repaint so the avatar updates to
                // the Alert face immediately, before the user reads
                // the prompt and reaches for a key. Without this, the
                // avatar still showed the in-flight tool's face
                // (Reading/Writing/Bash) until the next keystroke.
                renderer.draw_bottom(
                    &input,
                    &with_queue(
                        StatusLine::render(
                            session,
                            is_running,
                            0,
                            loop_label.as_deref(),
                            context.current_prompt_name.as_deref(),
                            perm_mode().as_deref(),
                        ),
                        interjection_queue.len(),
                    ),
                    is_running,
                )?;

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
                                UserEvent::Key(key) => {
                                    // Ctrl+C / Ctrl+D in the alert
                                    // = "I want out" → treat as
                                    // Deny. Without this the loop
                                    // fell through to `_ => {}` and
                                    // the tool hung waiting for an
                                    // answer that never came; the
                                    // user had to keyboard-mash to
                                    // discover that only y/a/n/Esc
                                    // worked.
                                    let is_ctrl_c = key.code == KeyCode::Char('c')
                                        && key.modifiers.contains(KeyModifiers::CONTROL);
                                    let is_ctrl_d = key.code == KeyCode::Char('d')
                                        && key.modifiers.contains(KeyModifiers::CONTROL);
                                    if is_ctrl_c || is_ctrl_d {
                                        break UserDecision::Deny;
                                    }
                                    match key.code {
                                    KeyCode::Char('y') => break UserDecision::AllowOnce,
                                    KeyCode::Char('a') => {
                                        let pattern = suggest_pattern(&ask_req.tool, &ask_req.input);
                                        // Refuse to store the empty-
                                        // input placeholder as a real
                                        // pattern. Without this, an "a"
                                        // press on a tool call with
                                        // empty/whitespace args would
                                        // pin "<edit this pattern>" as
                                        // a literal allowlist entry —
                                        // useless and confusing.
                                        // Fall back to AllowOnce so the
                                        // tool still runs, but no
                                        // permanent rule is added.
                                        if is_placeholder_pattern(&pattern) {
                                            renderer.write_line(
                                                "  -> can't derive a useful pattern from empty input; allowing once only",
                                                theme::dim(),
                                            )?;
                                            break UserDecision::AllowOnce;
                                        }
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
                let was_denied = matches!(decision, UserDecision::Deny);
                let _ = ask_req.reply.send(decision);

                // Reopen / mark the chamber depending on outcome:
                //
                // - **Allow**: write a fresh chamber TOP banner so
                //   the about-to-arrive ToolResult body has a
                //   chamber to land inside. This gives the user a
                //   clear "permission granted, tool running" visual
                //   pair (closed chamber for the pause, fresh
                //   chamber for the result).
                //
                // - **Deny**: chamber stayed closed; render a
                //   single dim "(denied)" trailer line so it's
                //   clear no result is coming.
                if let Some(reopen_name) = pending_chamber_tool {
                    // Visual breathing room between the alert box's
                    // bottom border and whatever follows (reopened
                    // chamber OR denied trailer). Without this, the
                    // alert's `╰─╯` sits flush against the next
                    // line which reads as continuous output.
                    renderer.write_line("", Color::White)?;
                    if was_denied {
                        renderer.write_line(
                            &format!("  ↳ denied: {} {}", ask_req.tool, ask_req.input),
                            theme::dim(),
                        )?;
                    } else {
                        // Reopen with the same banner shape the
                        // ToolCall handler uses. `ask_req.input` is
                        // the value that the original banner would
                        // have rendered (path for read/write/edit,
                        // command for bash, etc.) so we can pass it
                        // directly without re-parsing the JSON args.
                        //
                        // Note for `apply_patch`: the initial chamber
                        // showed "N ops" (overview); the reopened
                        // chamber here shows the specific path the
                        // user just permitted. Intentional — the
                        // user is approving per-op, so per-op
                        // identification is more useful at the
                        // reopen point.
                        let upper = reopen_name.to_ascii_uppercase();
                        let raw_value = sanitize_output(&ask_req.input).into_string();
                        let (frame_w, _) = chamber_widths(&renderer);
                        let header = fit_banner_header(&upper, &raw_value, frame_w);
                        renderer.write_line(&header, c_tool())?;
                        last_tool_name = Some(reopen_name);
                    }
                }

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

const ALLOW_PLACEHOLDER: &str = "<edit this pattern>";

/// Whether a pattern was returned by `suggest_pattern` as the
/// "empty input — please type a real pattern" placeholder rather
/// than a real glob. Used by the ask-dialog to detect when the
/// user pressed "allow always" on a degenerate input and refuse
/// to store the placeholder as an actual allowlist entry.
fn is_placeholder_pattern(p: &str) -> bool {
    p == ALLOW_PLACEHOLDER
}

fn suggest_pattern(tool: &str, input: &str) -> String {
    // Refuse to suggest a catch-all wildcard for empty / whitespace-
    // only input. A user mis-clicking "(a) allow always" on an empty
    // invocation would otherwise pin an "allow everything for this
    // tool forever" rule into their session. The placeholder string
    // is intentionally not a valid glob — the UI shows it as the
    // suggested pattern, the user edits it before confirming.
    const PLACEHOLDER: &str = ALLOW_PLACEHOLDER;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return PLACEHOLDER.to_string();
    }
    match tool {
        "bash" => {
            let first = trimmed.split_whitespace().next().unwrap_or(PLACEHOLDER);
            format!("{} *", first)
        }
        "read" | "write" | "edit" | "list_dir" => {
            let path = std::path::Path::new(trimmed);
            let parent = path
                .parent()
                .map(|p| p.to_string_lossy())
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            if parent.is_empty() {
                "**".to_string()
            } else {
                format!("{}/*", parent)
            }
        }
        "grep" | "find_files" => {
            let first = trimmed.split_whitespace().next().unwrap_or(PLACEHOLDER);
            format!("{}*", first)
        }
        // Unknown tools (MCP, semantic, etc.) — return placeholder
        // so the user explicitly edits before allowing.
        _ => PLACEHOLDER.to_string(),
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
        // call before the previous one finished). Previously we
        // painted CRT-static dither rows here, but those used
        // `static_row()` which was 2 chars narrower than the chamber
        // frame so the right border never lined up, and the dithered
        // noise was visually loud for a state the user just needs to
        // SEE quickly. Now we emit a single centered alert row in
        // the perm/error color (same orange tone as the permission
        // ask), which lines up with the chamber border and reads at
        // a glance.
        renderer.write_line(
            &chamber_row_centered("⚠ tool denied · aborted · no result", inner),
            theme::perm(),
        )?;
        renderer.write_line(&chamber_bottom(frame_w), theme::dim())?;
        *last_tool_name = None;
    }
    Ok(())
}

/// `│   <content centered to inner>   │` — pad text on both sides so
/// it sits horizontally centered within the chamber inner width.
fn chamber_row_centered(content: &str, inner: usize) -> String {
    // Total row width matches `chamber_row`: exactly `inner + 4`
    // terminal cells (`│ ` (2) + inner-cell middle + ` │` (2)).
    // The middle is `inner` cells: left_pad + content + right_pad.
    //
    // TWO bugs were stacked here:
    // (1) Used `chars().count()` instead of display width — the
    //     NO-OUTPUT chamber starts with `⚠` (2 cells / 1 char) so
    //     centering was off by 1 cell.
    // (2) Subtracted `len + 2` from `inner` for the pad, leaving
    //     the row 2 cells short of `inner + 4` total — the right
    //     `│` didn't line up under the chamber's top `╮` /
    //     bottom `╯`. Correct: pad = inner - len (the middle is
    //     `inner` cells wide; subtracting only `len` reserves the
    //     rest for padding around it).
    //
    // Anything wider than `inner` falls back to `chamber_row`
    // which truncates with `…` and pads to exactly `inner` cells.
    use unicode_width::UnicodeWidthStr;
    let len = UnicodeWidthStr::width(content);
    if len >= inner {
        return chamber_row(content, inner);
    }
    let pad = inner - len;
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
    use crossterm::style::{Attribute, ResetColor, SetAttribute, SetForegroundColor};
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
    // Bold-glow on accent so the search bar reads consistently with
    // the rest of the chat. Without Bold it was visibly duller than
    // surrounding content.
    let bloom = theme::is_bright(theme::accent());
    if bloom {
        crossterm::execute!(stdout, SetAttribute(Attribute::Bold))?;
    }
    crossterm::execute!(stdout, SetForegroundColor(theme::accent()))?;
    write!(stdout, "\r\n")?;
    write!(stdout, "{}", bar)?;
    if bloom {
        crossterm::execute!(stdout, SetAttribute(Attribute::NormalIntensity))?;
    }
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
        // Collect ids of the messages we're dropping BEFORE truncate
        // so we can also prune them from `tree.entries` and
        // `message_store`. Without this, the tree references
        // orphaned ids (no content in store), and subsequent
        // fork/clone/switch-to-leaf operations silently fail or
        // corrupt the session.
        let dropped_ids: Vec<_> = session.messages[msg_idx..]
            .iter()
            .map(|m| m.id.clone())
            .collect();
        session.messages.truncate(msg_idx);

        // Sibling-branch prune (Phase 2). Same logic as compress —
        // walk descendants of dropped ids and remove any forked
        // subtrees rooted on them. Active-path messages (still in
        // `session.messages` after truncate) are excluded.
        let dropped_set: std::collections::HashSet<_> = dropped_ids.iter().cloned().collect();
        let active_ids: std::collections::HashSet<_> =
            session.messages.iter().map(|m| m.id.clone()).collect();
        let mut to_prune = dropped_set.clone();
        loop {
            let new_ids: Vec<_> = session
                .tree
                .entries
                .iter()
                .filter(|(id, node)| {
                    !to_prune.contains(*id)
                        && !active_ids.contains(*id)
                        && node
                            .parent
                            .as_ref()
                            .map(|p| to_prune.contains(p))
                            .unwrap_or(false)
                })
                .map(|(id, _)| id.clone())
                .collect();
            if new_ids.is_empty() {
                break;
            }
            for id in new_ids {
                to_prune.insert(id);
            }
        }
        let pruned_siblings = to_prune.len().saturating_sub(dropped_set.len());

        // Phase 4: capture BranchSummary entries for each pruned
        // sibling subtree BEFORE removing nodes. Same algorithm as
        // `Session::compress_reporting` — root of a subtree is a
        // node in `to_prune` whose direct parent was in
        // `dropped_set` (the closest dropped-path ancestor). One
        // summary per subtree root, walking descendants for the
        // count.
        let now_rfc = chrono::Utc::now().to_rfc3339();
        let mut subtree_summaries: Vec<crate::session::BranchSummary> = Vec::new();
        for id in &to_prune {
            if dropped_set.contains(id) {
                continue;
            }
            let node = match session.tree.entries.get(id) {
                Some(n) => n,
                None => continue,
            };
            let parent = match &node.parent {
                Some(p) => p,
                None => continue,
            };
            if !dropped_set.contains(parent) {
                continue;
            }
            let mut count = 0usize;
            let mut stack = vec![id.clone()];
            while let Some(cur) = stack.pop() {
                if !to_prune.contains(&cur) {
                    continue;
                }
                count += 1;
                for (child_id, child_node) in session.tree.entries.iter() {
                    if child_node.parent.as_ref() == Some(&cur) {
                        stack.push(child_id.clone());
                    }
                }
            }
            let label_prefix = node
                .label
                .as_deref()
                .map(|l| format!("[{}] ", l))
                .unwrap_or_default();
            let body_preview = session
                .message_store
                .get(id)
                .map(|m| {
                    let s: String = m.content.chars().take(80).collect();
                    if m.content.chars().count() > 80 {
                        format!("{}…", s)
                    } else {
                        s
                    }
                })
                .unwrap_or_default();
            subtree_summaries.push(crate::session::BranchSummary {
                root_id: id.clone(),
                parent_id: parent.clone(),
                message_count: count,
                preview: format!("{}{}", label_prefix, body_preview),
                created_at: now_rfc.clone(),
            });
        }
        session.branch_summaries.extend(subtree_summaries);

        for id in &to_prune {
            session.tree.entries.remove(id);
            session.message_store.remove(id);
        }

        // Re-anchor `leaf_id` to the new tail (or None if everything
        // was dropped). Previously the leaf was left pointing at a
        // dropped id, which made `/tree` show a phantom branch.
        session.tree.leaf_id = session.messages.last().map(|m| m.id.clone());
        session.total_estimated_tokens = session.messages.iter().map(|m| m.estimated_tokens).sum();
        renderer.write_line(&format!("rewound {} message(s)", removed), theme::accent())?;
        if pruned_siblings > 0 {
            renderer.write_line(
                &format!(
                    "discarded {} forked branch node{} rooted in the rewound region",
                    pruned_siblings,
                    if pruned_siblings == 1 { "" } else { "s" },
                ),
                c_error(),
            )?;
        }
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
    use unicode_width::UnicodeWidthStr;

    /// Chamber banner fills the full frame width when the value fits.
    /// Right border is flush at frame_w; no extra trailing whitespace.
    #[test]
    fn banner_short_value_pads_with_dashes_to_full_width() {
        let header = fit_banner_header("READ", "/tmp/x", 60);
        assert_eq!(
            header.as_str().width(),
            60,
            "header should fill frame_w exactly: {:?}",
            header,
        );
        assert!(header.starts_with("╭─ READ ─ \"/tmp/x\""));
        assert!(header.ends_with("─╮"));
    }

    /// Regression: the suffix must be a tight `─╮` (no leading
    /// space). The previous ` ─╮` form left a visible gap
    /// `── ─╮` at the right edge that looked like the dash run
    /// was broken. Solid dashes match the chamber bottom
    /// (`╰────╯`) style.
    #[test]
    fn banner_has_no_internal_space_before_corner() {
        let header = fit_banner_header("READ", "/short", 50);
        // Strip the closing ╮ and check the char just before it
        // is `─`, not ` `.
        let mut chars: Vec<char> = header.chars().collect();
        let last = chars.pop();
        assert_eq!(last, Some('╮'));
        let second_last = chars.pop();
        assert_eq!(
            second_last,
            Some('─'),
            "char before closing ╮ must be `─`, not space; got {:?}",
            second_last,
        );
    }

    /// Banner left-truncates long paths so the filename (right side
    /// of a path) stays visible. The prefix `…` signals truncation.
    #[test]
    fn banner_long_path_left_truncates_to_preserve_filename() {
        let path = "/very/very/very/long/nested/path/to/some/file/named/important.clj";
        let header = fit_banner_header("READ", path, 60);
        assert_eq!(
            header.as_str().width(),
            60,
            "header must still be exactly frame_w wide",
        );
        // Filename must be visible (right side of path was preserved).
        assert!(
            header.contains("important.clj"),
            "filename should survive truncation: {:?}",
            header,
        );
        // Truncation marker is present.
        assert!(header.contains('…'), "expected `…` marker: {:?}", header);
        // The `/very/very` head SHOULD be gone — that's the whole
        // point of left-truncation.
        assert!(
            !header.contains("/very/very/very/long"),
            "leading head should be truncated: {:?}",
            header,
        );
    }

    /// Empty value (e.g. tool with no banner-worthy arg) renders
    /// just the prefix with dash fill — no empty quote pair.
    #[test]
    fn banner_empty_value_renders_just_prefix_and_dashes() {
        let header = fit_banner_header("DONE", "", 50);
        assert_eq!(header.as_str().width(), 50);
        assert!(
            !header.contains("\"\""),
            "no empty quote pair: {:?}",
            header
        );
        assert!(header.starts_with("╭─ DONE ─"));
        assert!(header.ends_with("─╮"));
    }

    /// Regression: `chamber_row_centered` must use DISPLAY width
    /// not char count. The NO-OUTPUT chamber message starts with
    /// `⚠` (2 cells wide, 1 char). Before this, centering was off
    /// by 1 cell and the right `│` border misaligned with the
    /// chamber's top/bottom borders.
    #[test]
    fn chamber_row_centered_handles_wide_emoji() {
        let row = chamber_row_centered("⚠ tool denied", 40);
        // Row must occupy exactly `inner + 4` display cells
        // (`│ ` + content + padding + ` │` = inner + 4 = 44).
        let row_width = UnicodeWidthStr::width(row.as_str());
        assert_eq!(
            row_width, 44,
            "row must be exactly inner+4 cells wide; got {row_width} for {row:?}",
        );
        // Right border `│` MUST be at the very end (no trailing
        // pad mismatch).
        assert!(
            row.ends_with(" │"),
            "right border missing or padded wrong: {row:?}"
        );
    }

    /// Self-review bug 1: `apply_patch` arg is `operations`
    /// (array), not a single string. Previously fell through to
    /// "path" lookup which returned empty, degrading the banner
    /// to bare "APPLY_PATCH" with dashes. Now shows op count.
    #[test]
    fn banner_value_apply_patch_shows_op_count() {
        let args = serde_json::json!({"operations": [{"action": "create", "path": "/a"}]});
        assert_eq!(format_tool_banner_value("apply_patch", &args), "1 op");

        let args = serde_json::json!({
            "operations": [
                {"action": "create", "path": "/a"},
                {"action": "update", "path": "/b"},
                {"action": "delete", "path": "/c"},
            ],
        });
        assert_eq!(format_tool_banner_value("apply_patch", &args), "3 ops");

        // Empty operations array → empty value (banner degrades
        // gracefully to prefix + dashes + suffix).
        let args = serde_json::json!({"operations": []});
        assert_eq!(format_tool_banner_value("apply_patch", &args), "");

        // Missing operations key → empty.
        let args = serde_json::json!({});
        assert_eq!(format_tool_banner_value("apply_patch", &args), "");
    }

    /// `format_tool_banner_value` picks the right key per tool.
    #[test]
    fn banner_value_picks_correct_key_per_tool() {
        let args =
            serde_json::json!({"path": "/p", "command": "ls", "pattern": "*.rs", "task_id": "t1"});
        assert_eq!(format_tool_banner_value("read", &args), "/p");
        assert_eq!(format_tool_banner_value("write", &args), "/p");
        assert_eq!(format_tool_banner_value("edit", &args), "/p");
        assert_eq!(format_tool_banner_value("bash", &args), "ls");
        assert_eq!(format_tool_banner_value("grep", &args), "*.rs");
        assert_eq!(format_tool_banner_value("glob", &args), "*.rs");
        assert_eq!(format_tool_banner_value("task_status", &args), "t1");
        // Unknown tool → empty (header degrades to prefix-only).
        assert_eq!(format_tool_banner_value("mystery", &args), "");
    }

    /// Edge: very narrow frame_w that can't fit even one value
    /// char. Must degrade gracefully without panicking; just emits
    /// prefix + dashes + suffix.
    #[test]
    fn banner_handles_pathologically_narrow_frame() {
        // Just enough for "╭─ X ─ " + " ─╮" + maybe nothing else.
        let header = fit_banner_header("READ", "/some/path", 12);
        // Don't pin the exact string — just make sure no panic and
        // we got SOMETHING with the borders intact.
        assert!(header.starts_with("╭"));
        assert!(header.ends_with("╮"));
    }

    /// Regression: a very long tool name (e.g. an MCP-registered
    /// tool like `MCP_TOOL:LONG_SERVER:LONG_FUNCTION`) used to
    /// overflow frame_w because the prefix `╭─ NAME ─ ` was wider
    /// than the entire chamber. Now the name itself gets
    /// left-truncated to keep the header at most frame_w wide.
    #[test]
    fn banner_truncates_pathological_long_tool_name() {
        let very_long = "MCP_TOOL:VERY_LONG_SERVER_NAME:VERY_LONG_FUNCTION_NAME";
        let header = fit_banner_header(very_long, "/some/path", 40);
        assert!(
            header.as_str().width() <= 40,
            "header must not exceed frame_w; got width {} for {:?}",
            header.as_str().width(),
            header,
        );
        assert!(header.starts_with("╭"));
        assert!(header.ends_with("╮"));
    }

    // Partial assistant reply at abort time is preserved into the
    // session with a trailer marking the interruption, so the LLM
    // sees on the next turn what it had been saying. Mirrors
    // opencode's `finalizeInterruptedAssistant` in
    // `packages/opencode/src/session/prompt.ts`.
    /// Phase 3 — abort with pending tool calls preserves them as
    /// structured entries on the stashed message. Pending entries
    /// stay Interrupted (no matching result arrived); on resume,
    /// `convert_history` will emit a [Tool execution was
    /// interrupted] tool_result so the LLM sees paired blocks.
    #[test]
    fn capture_partial_on_abort_preserves_pending_tool_calls_as_interrupted() {
        let mut session = crate::session::Session::new("p", "m", 100_000);
        let mut buf = String::from("Running bash...");
        let mut calls = vec![
            crate::session::ToolCallEntry {
                id: "tc_abc".to_string(),
                name: "bash".to_string(),
                args: serde_json::json!({"cmd": "sleep 99"}),
                state: crate::session::ToolCallState::Interrupted,
            },
            crate::session::ToolCallEntry {
                id: "tc_xyz".to_string(),
                name: "read".to_string(),
                args: serde_json::json!({"path": "/etc/hostname"}),
                state: crate::session::ToolCallState::Completed {
                    result: "myhost".to_string(),
                },
            },
        ];
        let stashed = capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 2, &mut calls);
        assert!(stashed);
        assert!(calls.is_empty(), "tool_calls_buf must be drained on stash");

        let last = session.messages.last().unwrap();
        assert_eq!(last.tool_calls.len(), 2);
        let interrupted = last
            .tool_calls
            .iter()
            .find(|e| e.id == "tc_abc")
            .expect("missing interrupted entry");
        assert!(matches!(
            interrupted.state,
            crate::session::ToolCallState::Interrupted,
        ));
        let completed = last
            .tool_calls
            .iter()
            .find(|e| e.id == "tc_xyz")
            .expect("missing completed entry");
        match &completed.state {
            crate::session::ToolCallState::Completed { result } => {
                assert_eq!(result, "myhost");
            }
            other => panic!("expected Completed; got {other:?}"),
        }
    }

    #[test]
    fn capture_partial_on_abort_stashes_partial_with_trailer() {
        let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
        let baseline = session.messages.len();
        let mut buf = String::from("I was about to explain that");
        let stashed =
            capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 0, &mut Vec::new());
        assert!(stashed);
        assert_eq!(session.messages.len(), baseline + 1);
        let last = session.messages.last().unwrap();
        assert_eq!(last.role, crate::session::MessageRole::Assistant);
        assert!(
            last.content.contains("I was about to explain that"),
            "must keep the original partial: {:?}",
            last.content,
        );
        assert!(
            last.content.contains("[interrupted by user (Ctrl+C)]"),
            "must include the interruption trailer: {:?}",
            last.content,
        );
        assert!(buf.is_empty(), "buf must be cleared after stash");
    }

    // Aborting when nothing has streamed yet is a no-op — we don't
    // want a session full of empty "[interrupted]" messages from
    // mistaken Ctrl+C presses.
    #[test]
    fn capture_partial_on_abort_noop_on_empty_buf() {
        let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
        let baseline = session.messages.len();
        let mut buf = String::new();
        let stashed =
            capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 0, &mut Vec::new());
        assert!(!stashed);
        assert_eq!(session.messages.len(), baseline);
    }

    // Whitespace-only partial (e.g. agent had only emitted some
    // leading newlines) is also a no-op — no useful text to save.
    #[test]
    fn capture_partial_on_abort_noop_on_whitespace_only() {
        let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
        let baseline = session.messages.len();
        let mut buf = String::from("   \n\n\t  ");
        let stashed = capture_partial_on_abort(&mut buf, &mut session, "Esc", 0, &mut Vec::new());
        assert!(!stashed);
        assert_eq!(session.messages.len(), baseline);
    }

    // When tool calls ran in the same turn as the abort, the trailer
    // must say so. The agent's preserved text only covers what was
    // streamed via `AgentEvent::Token`; tool calls + results emitted
    // separately are NOT in `response_buf`. Without this hint the
    // next turn's LLM would see the partial as a definitive "this
    // was the assistant's response" and could re-run side-effecting
    // tool calls.
    #[test]
    fn capture_partial_on_abort_trailer_notes_tool_calls() {
        let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
        let mut buf = String::from("I deleted the file");
        let stashed =
            capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 2, &mut Vec::new());
        assert!(stashed);
        let content = &session.messages.last().unwrap().content;
        assert!(
            content.contains("I deleted the file"),
            "partial text dropped: {content:?}",
        );
        assert!(
            content.contains("[interrupted by user (Ctrl+C);"),
            "trailer prefix changed: {content:?}",
        );
        assert!(
            content.contains("2 tool call"),
            "trailer must mention tool call count: {content:?}",
        );
        assert!(
            content.contains("not preserved"),
            "trailer must warn that tool calls were not preserved: {content:?}",
        );
    }

    // Single tool call uses singular phrasing — "1 tool call ran" not
    // "1 tool calls ran". Tiny but the LLM is reading this verbatim.
    #[test]
    fn capture_partial_on_abort_trailer_handles_singular_tool_call() {
        let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
        let mut buf = String::from("Running tests now");
        capture_partial_on_abort(&mut buf, &mut session, "Esc", 1, &mut Vec::new());
        let content = &session.messages.last().unwrap().content;
        assert!(
            content.contains("1 tool call ran"),
            "expected singular phrasing for 1 tool call: {content:?}",
        );
        assert!(
            !content.contains("1 tool calls ran"),
            "leaked plural for singular case: {content:?}",
        );
    }

    /// `suggest_pattern` returns a literal placeholder for empty
    /// input. The ask-dialog path that consumes it must detect the
    /// placeholder and refuse to add it as an allowlist entry —
    /// otherwise pressing "a" (allow always) on an empty invocation
    /// would silently store `<edit this pattern>` as a real pattern.
    /// The detection is exposed via `is_placeholder_pattern` so the
    /// dialog code is unit-testable.
    #[test]
    fn placeholder_pattern_is_detectable() {
        let p = suggest_pattern("bash", "");
        assert!(
            is_placeholder_pattern(&p),
            "empty input should yield a detectable placeholder; got {p:?}",
        );
        let p = suggest_pattern("grep", "  \t  ");
        assert!(is_placeholder_pattern(&p));
        // A legit suggestion is NOT flagged as a placeholder.
        let p = suggest_pattern("bash", "cargo test");
        assert!(!is_placeholder_pattern(&p), "real pattern flagged: {p:?}");
    }

    // Whitespace-only or empty input must NOT collapse to a "* *"
    // / "*" wildcard pattern that matches every subsequent call.
    // The audit flagged this as a footgun: a user accidentally
    // hitting "(a) allow always" on an empty bash invocation would
    // permanently auto-allow ALL bash. Now we return a literal
    // placeholder + the user has to type the pattern themselves.
    #[test]
    fn suggest_pattern_refuses_wildcard_on_empty_input() {
        // Bash: empty / whitespace input should NOT yield "* *".
        let p = suggest_pattern("bash", "");
        assert_ne!(p, "* *", "empty bash input must not yield catch-all");
        assert!(
            !p.contains('*'),
            "empty input should not contain wildcards: {p:?}"
        );

        let p = suggest_pattern("bash", "   \t  ");
        assert_ne!(
            p, "* *",
            "whitespace-only bash input must not yield catch-all"
        );
        assert!(
            !p.contains('*'),
            "ws-only input should not contain wildcards: {p:?}"
        );

        // grep / find_files: same — empty must not yield "*"
        let p = suggest_pattern("grep", "");
        assert!(
            !p.contains('*'),
            "empty grep input must not yield wildcard: {p:?}"
        );

        // Unknown tool with empty input shouldn't yield catch-all.
        let p = suggest_pattern("mcp_tool:foo", "");
        assert!(!p.contains('*'), "unknown tool empty input: {p:?}");
    }

    // Non-empty inputs still produce the expected suggestion.
    #[test]
    fn suggest_pattern_works_for_non_empty_inputs() {
        assert_eq!(suggest_pattern("bash", "cargo test --all"), "cargo *");
        assert_eq!(suggest_pattern("grep", "fn foo bar"), "fn*");
    }

    // Rewind must sync tree.entries + message_store + leaf_id with
    // the truncated `messages` slice. Without this, the tree
    // references orphaned ids that no longer have content, and the
    // leaf_id can point past the truncation. Subsequent fork /
    // clone / save-load operations either fail or carry stale ids.
    #[test]
    fn rewind_truncates_tree_and_store_in_sync_with_messages() {
        let mut session = crate::session::Session::new("p", "m", 100_000);
        session.add_message(crate::session::MessageRole::User, "u1");
        session.add_message(crate::session::MessageRole::Assistant, "a1");
        session.add_message(crate::session::MessageRole::User, "u2");
        session.add_message(crate::session::MessageRole::Assistant, "a2");
        let baseline_tree = session.tree.entries.len();
        assert_eq!(baseline_tree, 4, "fixture: 4 entries");

        // Rewind back to the first user message (idx=1 in the
        // reverse-order user list means the *first* user).
        let mut renderer = crate::ui::renderer::Renderer::new().unwrap();
        // idx=0 = "rewind through the most recent user prompt" → cut
        // at the position of u2 → messages become [u1, a1].
        let _ = rewind_session(&mut session, 0, &mut renderer);

        // After rewind, messages has [u1, a1]; tree must agree.
        assert_eq!(session.messages.len(), 2);
        assert_eq!(
            session.tree.entries.len(),
            session.messages.len(),
            "tree entries must match messages count; got tree={}, msgs={}",
            session.tree.entries.len(),
            session.messages.len(),
        );
        assert_eq!(
            session.message_store.len(),
            session.messages.len(),
            "store must match messages count",
        );
        // Leaf points to the last remaining message.
        let last_id = session.messages.last().unwrap().id.clone();
        assert_eq!(
            session.tree.leaf_id,
            Some(last_id.clone()),
            "leaf_id must anchor to the new tail",
        );
        // Every remaining message id has a tree entry + store entry.
        for m in &session.messages {
            assert!(
                session.tree.entries.contains_key(&m.id),
                "missing tree entry for {}",
                m.id,
            );
            assert!(
                session.message_store.contains_key(&m.id),
                "missing store entry for {}",
                m.id,
            );
        }
    }

    // The token accumulator on the abort path keeps `total_tokens`
    // in sync with `total_estimated_tokens`. Both fields are
    // TODO(cost-tracking) placeholders today but the inconsistency
    // between Done/Interjected (which both update total_tokens) and
    // abort (which didn't) made the abort case look like the agent
    // produced zero tokens that turn.
    #[test]
    fn capture_partial_on_abort_keeps_total_tokens_in_sync() {
        let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
        let baseline_total = session.total_tokens;
        let baseline_est = session.total_estimated_tokens;
        let mut buf = String::from(
            "A reasonably long partial response that should produce a non-zero token estimate.",
        );
        capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 0, &mut Vec::new());
        // Both fields advanced by the same amount (the stashed
        // message's estimated_tokens). Without the parity fix, only
        // total_estimated_tokens moved.
        assert!(
            session.total_estimated_tokens > baseline_est,
            "total_estimated_tokens should advance on stash",
        );
        assert_eq!(
            session.total_tokens.saturating_sub(baseline_total),
            session.total_estimated_tokens.saturating_sub(baseline_est),
            "total_tokens must advance in lockstep with total_estimated_tokens",
        );
    }

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
