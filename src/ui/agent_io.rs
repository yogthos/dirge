//! Agent-stream rendering, subagent-panel mutation, partial-on-abort
//! capture, turn persistence, and plugin-entry rendering.
//!
//! Extracted from `ui/mod.rs`. These helpers all sit on the boundary
//! between agent-side events (`AgentEvent::Token`, subagent lifecycle
//! events, `AgentEvent::PluginEntry`) and the renderer / session
//! state. They're grouped here because they're the I/O surface the
//! event loop reaches for on every turn.

use compact_str::CompactString;
use crossterm::style::Color;

use crate::agent::tools::task::SubagentChatEvent;
#[cfg(feature = "plugin")]
use crate::plugin::PluginManager;
use crate::session::{self, MessageRole, Session, ToolCallEntry, ToolCallState};
use crate::ui::events::sanitize_output;
use crate::ui::markdown;
use crate::ui::panel_data;
use crate::ui::renderer::Renderer;
#[cfg(feature = "plugin")]
use crate::ui::theme;

#[cfg(feature = "plugin")]
use crate::ui::colors::parse_plugin_color;

/// Drive the left-panel subagent map from a chat-event:
///   - `Spawn`            → insert a `"running"` row (oldest at top).
///   - `Complete`/`Failed` → REMOVE the row.
///
/// The panel is for in-flight tracking only; the full result for a
/// finished subagent lives in its per-subagent chat (Ctrl-N/P/X to
/// reach it), so the row would just be visual noise. Earlier code
/// mutated `state` in place and never removed entries, causing the
/// panel to accumulate stale `✓`/`✗` rows for every subagent that
/// ever ran in the session.
pub(crate) fn apply_subagent_panel_event(
    rows: &mut indexmap::IndexMap<String, (String, String, Vec<String>)>,
    event: &SubagentChatEvent,
) {
    use SubagentChatEvent as E;
    match event {
        E::Spawn { id, prompt } => {
            let files = panel_data::extract_file_paths_from_prompt(prompt);
            rows.insert(id.clone(), ("running".to_string(), prompt.clone(), files));
        }
        E::Complete { id, .. } | E::Failed { id, .. } => {
            rows.shift_remove(id);
        }
    }
}

/// Single rendering pipeline for the agent chat — Reasoning AND Token
/// streams BOTH route through this helper. Markdown is parsed every
/// chunk so bold / italics / inline code / headings / code blocks /
/// blockquotes stay styled as text accumulates. The `base_color`
/// parameter sets the body / paragraph color so each stream picks
/// its own register (e.g. DarkMagenta for reasoning, theme::agent()
/// for content tokens) while highlights (headings, code, accent,
/// dim) follow the active theme.
///
/// `buf` is the accumulated stream text; `start_line` anchors the
/// region of the renderer's buffer that this stream owns so each
/// new chunk replaces-in-place. First call (when `*start_line ==
/// None`) captures the current buffer length as the anchor.
pub(crate) fn render_agent_stream(
    buf: &str,
    start_line: &mut Option<usize>,
    base_color: Color,
    renderer: &mut Renderer,
) -> anyhow::Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    // 8-col "<dirge> " handle + 1-col space — the per-line prefix the
    // first styled entry will carry.
    let max_width = renderer.content_width().saturating_sub(9);
    let mut styled = markdown::markdown_to_styled(buf, max_width, base_color);
    if !styled.is_empty() {
        styled[0].text = CompactString::from(format!("<dirge> {}", styled[0].text));
    }
    if let Some(start) = *start_line {
        renderer.replace_from(start, styled);
    } else {
        let start = renderer.buffer_len();
        *start_line = Some(start);
        renderer.replace_from(start, styled);
    }
    renderer.render_viewport()?;
    Ok(())
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
pub(crate) fn capture_partial_on_abort(
    response_buf: &mut String,
    session: &mut Session,
    why: &str,
    tool_calls_in_turn: u32,
    tool_calls_buf: &mut Vec<ToolCallEntry>,
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
    let est = session::Session::estimate_tokens(&stashed);
    session.add_message_with_tool_calls(MessageRole::Assistant, &stashed, calls);
    session.total_tokens = session.total_tokens.saturating_add(est);
    response_buf.clear();
    true
}

/// Persist the current turn (user prompt + assistant response + tool
/// calls) to the SQLite session DB for FTS5 search. Called at every
/// run boundary — Done, Interjected, ContextOverflow, and Error.
///
/// Best-effort: failures are silent (DB open/write errors shouldn't
/// break the session). Session insert is idempotent via INSERT OR IGNORE.
pub(crate) fn persist_turn_to_db(
    session: &Session,
    user_prompt: &str,
    assistant_text: &str,
    tool_calls: &[ToolCallEntry],
) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let paths = crate::extras::dirge_paths::ProjectPaths::new(&cwd);
    let db = match crate::extras::session_db::SessionDb::open(&paths.session_db_path()) {
        Ok(db) => db,
        Err(e) => {
            tracing::debug!(
                target: "dirge::ui",
                error = %e,
                "Session DB unavailable — turn not persisted"
            );
            return;
        }
    };
    let now = chrono::Utc::now().to_rfc3339();
    let sid = format!(
        "dirge-{}",
        session.id.as_str().chars().take(8).collect::<String>()
    );
    let _ = db.insert_session(&sid, "cli", &session.model, &session.provider, &now);

    if !user_prompt.is_empty() {
        let _ = db.insert_message(&sid, "user", user_prompt, None, None, None, &now);
    }

    if !assistant_text.is_empty() {
        // Collect tool names + serialized tool calls for the
        // assistant message so FTS5 can find them.
        let tool_names: Vec<&str> = tool_calls.iter().map(|tc| tc.name.as_str()).collect();
        let tool_name_str = if tool_names.is_empty() {
            None
        } else {
            Some(tool_names.join(" "))
        };
        let tool_calls_str = if tool_calls.is_empty() {
            None
        } else {
            serde_json::to_string(tool_calls).ok()
        };
        let _ = db.insert_message(
            &sid,
            "assistant",
            assistant_text,
            tool_name_str.as_deref(),
            tool_calls_str.as_deref(),
            None,
            &now,
        );
    }

    // Also insert each tool result as a separate message so
    // searching for a tool name finds concrete results.
    for tc in tool_calls {
        let result_text = match &tc.state {
            ToolCallState::Completed { result } => result.clone(),
            ToolCallState::Interrupted => "[interrupted]".to_string(),
            ToolCallState::Failed { error } => format!("[failed: {}]", error),
        };
        let _ = db.insert_message(
            &sid,
            "tool",
            &result_text,
            Some(&tc.name),
            None,
            Some(&tc.id),
            &now,
        );
    }

    // NOTE: end_session intentionally NOT called here.
    // Marking the session "done" after every turn was found to
    // cause previous session content to leak into the chat.
    // end_session() is reserved for true session termination
    // (compression splits, explicit user exit).
}

/// Render one plugin entry to the chat. Looks up a registered
/// renderer for `entry.custom_type`; if found, invokes it and
/// renders the returned (color, text) lines. If not found (or the
/// renderer emitted nothing), falls back to a minimal default
/// rendering: a header line + the raw data string.
#[cfg(feature = "plugin")]
pub(crate) fn render_plugin_entry(
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
