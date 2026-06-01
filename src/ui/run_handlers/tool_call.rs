//! `AgentEvent::ToolCall` handler extracted from `run_interactive`.
//!
//! Records the structured tool-call entry (Interrupted until its
//! `ToolResult` lands), feeds the left-panel activity ring, closes any
//! stale chamber (passively — chamber turnover, not a denial), flushes the
//! coalesced response tokens, and paints the rounded chamber TOP border.
//! Behavior is identical to the inline code; pure refactor (dirge-4y4l).

use std::collections::VecDeque;
use std::time::Instant;

use crossterm::style::Color;
use serde_json::Value;

use crate::ui::agent_io::render_agent_stream;
use crate::ui::avatar;
use crate::ui::colors::{c_agent, c_tool};
use crate::ui::events::sanitize_output;
use crate::ui::panel_data::tool_call_label;
use crate::ui::run_handlers::RunCtx;
use crate::ui::tool_display::{
    chamber_widths, close_tool_chamber_passive, fit_banner_header, format_tool_banner_value,
};

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_tool_call(
    ctx: &mut RunCtx<'_>,
    id: &str,
    name: &str,
    args: &Value,
    was_reasoning: &mut bool,
    last_token_render: &mut Option<Instant>,
    tool_activity: &mut VecDeque<String>,
    activity_cap: usize,
) -> anyhow::Result<()> {
    // Feed the left-panel [ACTIVITY] ticker (newest last, bounded ring).
    tool_activity.push_back(tool_call_label(name, args));
    while tool_activity.len() > activity_cap {
        tool_activity.pop_front();
    }
    // dirge-5h5: log entry state so the parallel-read race can be
    // reconstructed offline.
    tracing::trace!(
        target: "dirge::ui::chamber",
        event = "tool_call_in",
        id = %id,
        name = %name,
        last_tool_call_id_before = ?ctx.last_tool_call_id,
        tool_chamber_open_before = *ctx.tool_chamber_open,
        chamber_top_start_before = ?ctx.chamber_top_start,
        chamber_top_end_before = ?ctx.chamber_top_end,
        buffer_len = ctx.renderer.buffer_len(),
        "ToolCall handler entry"
    );
    *was_reasoning = false;
    // Phase 3: persist as structured entry. Start in Interrupted state so
    // that if the user aborts before the result arrives, the saved session
    // captures the right state. The matching `ToolResult` flips it to
    // Completed.
    ctx.tool_calls_buf.push(crate::session::ToolCallEntry {
        id: id.to_string(),
        name: name.to_string(),
        args: args.clone(),
        state: crate::session::ToolCallState::Interrupted,
    });
    // Track for the abort-trailer warning: when the user later hits Ctrl+C /
    // Esc, the saved partial reply notes how many tool calls ran (and didn't
    // have their results preserved in the message text).
    *ctx.tool_calls_this_run = ctx.tool_calls_this_run.saturating_add(1);
    ctx.renderer
        .set_avatar_state(avatar::AvatarState::from_tool_name(name));
    #[cfg(feature = "experimental-ui-terminal-tab")]
    ctx.renderer.set_last_tool_name(name);
    // If a previous tool's chamber never closed (errored without a
    // ToolResult, etc.), close it before opening the new one. Use PASSIVE
    // close, not abort: a new ToolCall arriving over a stale chamber is
    // chamber turnover, not a denial event — painting "⚠ tool denied" would
    // falsely brand a healthy tool call as refused.
    close_tool_chamber_passive(
        ctx.renderer,
        ctx.last_tool_name,
        ctx.tool_chamber_open,
        ctx.chamber_top_start,
        ctx.chamber_top_end,
    )?;
    *ctx.last_tool_name = Some(name.to_string());
    *ctx.last_tool_call_id = Some(id.to_string());
    // dirge-ufe0: flush any trailing token the render coalescer skipped (a
    // ToolCall queued behind the final tokens leaves them
    // caught-up-but-unpainted) before response_buf is cleared, so the
    // streamed text is on-screen above the tool chamber.
    if !ctx.response_buf.is_empty() {
        render_agent_stream(
            ctx.response_buf,
            ctx.response_start_line,
            c_agent(),
            ctx.renderer,
        )?;
    }
    *last_token_render = None;
    if *ctx.agent_line_started {
        ctx.renderer.write_line("", Color::White)?;
        *ctx.agent_line_started = false;
    }
    ctx.response_buf.clear();
    *ctx.response_start_line = None;
    ctx.reasoning_buf.clear();
    *ctx.reasoning_start_line = None;
    // Tool-call line: rounded chamber TOP border with the tool name on it.
    // Output lines below get `│ ` chamber rows; closed by `╰────╯` after the
    // ToolResult.
    let upper = name.to_ascii_uppercase();
    // Record the buffer position BEFORE the spacer + header — used by
    // passive close to drop the chamber entirely if no body content follows
    // (parallel tool calls).
    *ctx.chamber_top_start = Some(ctx.renderer.buffer_len());
    // Blank line BEFORE the chamber top so the eye has an anchor between
    // dense prior output and the new tool chamber.
    ctx.renderer.write_line("", Color::White)?;
    let raw_value = format_tool_banner_value(name, args);
    let raw_value = sanitize_output(&raw_value).into_string();
    let (frame_w, _) = chamber_widths(&*ctx.renderer);
    let header = fit_banner_header(&upper, &raw_value, frame_w);
    ctx.renderer.write_line(&header, c_tool())?;
    *ctx.chamber_top_end = Some(ctx.renderer.buffer_len());
    *ctx.tool_chamber_open = true;
    tracing::trace!(
        target: "dirge::ui::chamber",
        event = "tool_call_painted",
        id = %id,
        name = %name,
        chamber_top_start_after = ?ctx.chamber_top_start,
        chamber_top_end_after = ?ctx.chamber_top_end,
        buffer_len = ctx.renderer.buffer_len(),
        "ToolCall TOP painted"
    );
    // Note: on-tool-start fires from HookedToolDyn now, around the actual
    // tool invocation — the UI no longer dispatches it here.
    Ok(())
}
