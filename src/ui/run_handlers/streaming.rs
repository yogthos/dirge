//! Streaming `AgentEvent` arms (`Reasoning`, `Token`) extracted from
//! `run_interactive`. Both feed the shared `render_agent_stream` pipeline;
//! `Token` additionally drives the plugin per-turn batcher and the
//! dirge-ufe0 render coalescer. The caller keeps the loop-control guards
//! (`Reasoning`'s `show_reasoning` skip, the avatar-state set) inline.
//! Behavior is identical to the inline code; pure refactor (dirge-4y4l).

use std::time::Instant;

use crossterm::style::Color;

use crate::ui::agent_io::{RENDER_FRAME, render_agent_stream, should_render_token};
use crate::ui::avatar;
use crate::ui::colors::c_agent;
use crate::ui::events::sanitize_output;
use crate::ui::run_handlers::RunCtx;

#[cfg(feature = "plugin")]
use crate::plugin::PluginManager;
#[cfg(feature = "plugin")]
use crate::ui::streaming::TokenBatcher;
#[cfg(feature = "plugin")]
use std::sync::{Arc, Mutex};

/// `AgentEvent::Reasoning` body (after the caller's avatar-state set +
/// `show_reasoning` guard): accumulate the thinking text and repaint it in
/// the DarkMagenta "thinking" register.
pub(crate) fn handle_reasoning(
    ctx: &mut RunCtx<'_>,
    text: &str,
    was_reasoning: &mut bool,
) -> anyhow::Result<()> {
    let safe = sanitize_output(text);
    ctx.reasoning_buf.push_str(&safe);
    // Shared pipeline with Token. The soft, recessive `thinking` register
    // signals the reasoning voice without competing with the agent's prose
    // (replaces the hard DarkMagenta); markdown highlights still ride the
    // theme accessors.
    render_agent_stream(
        ctx.reasoning_buf,
        ctx.reasoning_start_line,
        crate::ui::theme::thinking(),
        ctx.renderer,
    )?;
    *ctx.agent_line_started = true;
    *was_reasoning = true;
    Ok(())
}

/// `AgentEvent::Token` body: accumulate the assistant token, feed the
/// plugin per-turn batcher (`on-message-update`), and coalesce repaints
/// (dirge-ufe0) so a burst paints at most once per frame. `pending` is the
/// caller's `agent_rx.len()` — when 0 we're caught up to the last queued
/// event, so the final token of a burst always lands.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_token(
    ctx: &mut RunCtx<'_>,
    text: &str,
    was_reasoning: &mut bool,
    last_token_render: &mut Option<Instant>,
    pending: usize,
    #[cfg(feature = "plugin")] plugin_manager: Option<&Arc<Mutex<PluginManager>>>,
    #[cfg(feature = "plugin")] token_batcher: &mut TokenBatcher,
    #[cfg(feature = "plugin")] current_turn_text: &mut String,
    #[cfg(feature = "plugin")] current_turn_index: u32,
) -> anyhow::Result<()> {
    ctx.renderer.set_avatar_state(avatar::AvatarState::Speaking);
    if *was_reasoning {
        ctx.renderer.write_line("", Color::White)?;
        *was_reasoning = false;
        ctx.response_buf.clear();
        *ctx.response_start_line = None;
        // End-of-reasoning marker. The reasoning stays rendered in the
        // scroll; we just stop tracking it so the next reasoning burst
        // anchors at a fresh buffer position below the streamed content.
        ctx.reasoning_buf.clear();
        *ctx.reasoning_start_line = None;
    }
    let safe = sanitize_output(text);
    ctx.response_buf.push_str(&safe);

    // Stream the token into the per-turn batcher + accumulator. When the
    // batcher crosses its threshold, dispatch `on-message-update` with the
    // cumulative text so far. `current_turn_text` is the full turn text for
    // the closing `on-turn-end` event.
    #[cfg(feature = "plugin")]
    if let Some(pm) = plugin_manager {
        current_turn_text.push_str(text);
        if token_batcher.push(text).is_some() {
            let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
            let _ = mgr.dispatch(
                "on-message-update",
                &format!(
                    "@{{:index {} :partial \"{}\"}}",
                    current_turn_index,
                    crate::plugin::escape_janet_string(current_turn_text),
                ),
            );
        }
    }

    // dirge-ufe0: coalesce repaints. Paint only when caught up to the last
    // queued event (pending == 0, so the final token of a burst lands) or a
    // frame interval elapsed (so a long burst still streams visibly). The
    // ToolCall/Done/Error arms flush response_buf, so a coalesced trailing
    // token still renders before the buffer clears.
    let since = last_token_render.map_or(RENDER_FRAME, |t| t.elapsed());
    if should_render_token(pending, since, RENDER_FRAME) {
        render_agent_stream(
            ctx.response_buf,
            ctx.response_start_line,
            c_agent(),
            ctx.renderer,
        )?;
        *last_token_render = Some(Instant::now());
    }
    *ctx.agent_line_started = true;
    Ok(())
}
