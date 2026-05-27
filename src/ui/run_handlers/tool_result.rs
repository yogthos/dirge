//! `AgentEvent::ToolResult` handler extracted from `run_interactive`.
//!
//! Pairs the result with its in-flight tool call (id-match, with a
//! pending-Interrupted fallback for providers that don't emit ids),
//! then paints the result inside the open chamber — or as a single
//! `↳ first_line` trailer if the chamber was closed by a deny path,
//! or as a fresh chamber if a parallel-execution race displaced the
//! original. Edits get the colorized diff path when `show_edit_diff`
//! is enabled; everything else goes through `render_tool_output`.

use crossterm::style::Color;

use crate::ui::colors::c_tool;
use crate::ui::events::sanitize_output;
use crate::ui::run_handlers::RunCtx;
use crate::ui::theme;
use crate::ui::tool_display::{
    chamber_bottom, chamber_row, chamber_row_with_bg, chamber_widths,
    close_tool_chamber_passive, fit_banner_header, format_tool_banner_value,
    render_tool_output,
};

pub(crate) async fn handle_tool_result(
    ctx: &mut RunCtx<'_>,
    id: String,
    output: String,
) -> anyhow::Result<()> {
    // Phase 3: pair the result with its call.
    // Prefer id-match; fall back to the most-
    // recent Interrupted (pending) entry for
    // providers that don't emit ids.
    let target = if !id.is_empty() {
        ctx.tool_calls_buf
            .iter_mut()
            .rev()
            .find(|e| e.id == id.as_str())
    } else {
        ctx.tool_calls_buf
            .iter_mut()
            .rev()
            .find(|e| matches!(e.state, crate::session::ToolCallState::Interrupted))
    };
    if let Some(entry) = target {
        entry.state = crate::session::ToolCallState::Completed {
            result: output.to_string(),
        };
    }
    let show_details = ctx.cfg.show_tool_details.unwrap_or(true);
    let max_chars = ctx.cfg.resolve_tool_result_max_chars();
    let show_diff = ctx.cfg.resolve_show_edit_diff();

    // dirge-jzj: if the chamber on screen belongs to a
    // DIFFERENT tool call (parallel-execution race
    // where ToolResults arrive out of order, or a
    // newer ToolCall's TOP displaced this result's
    // chamber before the result arrived), paint a
    // fresh complete chamber for THIS id below the
    // current scroll position. Lets each result land
    // in its own correctly-labeled frame regardless
    // of completion order. The id-matches case (the
    // common sequential path) falls through to the
    // existing render paths below.
    if !id.is_empty()
        && ctx.last_tool_call_id.as_deref() != Some(id.as_str())
        && show_details
    {
        // Close whatever chamber is on screen first,
        // then paint a fresh TOP for this id. We
        // don't reuse the ToolCall handler's TOP-
        // paint code path because that fires from a
        // different event; the body of the new
        // chamber will land via path (a) below now
        // that tool_chamber_open=true.
        if *ctx.tool_chamber_open {
            close_tool_chamber_passive(
                ctx.renderer,
                ctx.last_tool_name,
                ctx.tool_chamber_open,
                ctx.chamber_top_start,
                ctx.chamber_top_end,
            )?;
        }
        let (resolved_name, resolved_args) = ctx
            .tool_calls_buf
            .iter()
            .rev()
            .find(|e| e.id == id.as_str())
            .map(|e| (e.name.to_string(), e.args.clone()))
            .unwrap_or_else(|| (String::new(), serde_json::Value::Null));
        if !resolved_name.is_empty() {
            let upper = resolved_name.to_ascii_uppercase();
            let raw_value = format_tool_banner_value(&resolved_name, &resolved_args);
            let raw_value = sanitize_output(&raw_value).into_string();
            let (frame_w, _) = chamber_widths(ctx.renderer);
            let header = fit_banner_header(&upper, &raw_value, frame_w);
            ctx.renderer.write_line("", Color::White)?;
            ctx.renderer.write_line(&header, c_tool())?;
            *ctx.tool_chamber_open = true;
            *ctx.last_tool_name = Some(resolved_name);
            *ctx.last_tool_call_id = Some(id.to_string());
        }
        // If the call wasn't in tool_calls_buf (id
        // unknown — shouldn't happen post-ToolCall
        // but defensive), fall through to path (b)
        // trailer; we have no banner to paint.
    }

    // on-tool-end is also fired by HookedToolDyn so the
    // host doesn't re-dispatch it here.

    // Three states at ToolResult time:
    //
    //  (a) chamber OPEN, show_details=true  → paint body
    //      inside the chamber + close with `╰─╯`.
    //  (b) chamber NOT OPEN (deny path closed it via
    //      the alert handler), show_details=true →
    //      emit a single dim `  ↳ {output}` trailer.
    //      The trailer is the only thing pinned to
    //      the original tool call now that its
    //      chamber is gone.
    //  (c) show_details=false → no body, but if the
    //      chamber is still open we MUST close it
    //      (a bare chamber_bottom) or the next
    //      output paints inside a dead chamber.
    //
    // Gating on `tool_chamber_open` (not
    // `last_tool_name`) is deliberate: the name
    // slot has unrelated clear sites and can drain
    // while the chamber TOP is still on screen —
    // that's the whole reason for the dedicated
    // chamber-state bool.
    if !*ctx.tool_chamber_open && show_details {
        // (b) chamber already closed by deny path.
        let trimmed = output.trim();
        if !trimmed.is_empty() {
            let first_line = trimmed.lines().next().unwrap_or("");
            ctx.renderer.write_line(
                &format!("  ↳ {}", sanitize_output(first_line)),
                theme::dim(),
            )?;
        }
    }
    if *ctx.tool_chamber_open && !show_details {
        // (c) chamber on-screen but body suppressed
        // — show a single dim "(body hidden)" row
        // so the chamber doesn't look like an
        // empty box with no content. Then close
        // with a bare bottom so a stale `╭─`
        // doesn't swallow the next paint.
        let (frame_w, inner) = chamber_widths(ctx.renderer);
        ctx.renderer.write_line(
            &chamber_row("(body hidden — show_tool_details=false)", inner),
            theme::dim(),
        )?;
        ctx.renderer
            .write_line(&chamber_bottom(frame_w), theme::dim())?;
        *ctx.tool_chamber_open = false;
    }
    if *ctx.tool_chamber_open && show_details {
        // Resolve the tool name + banner for the
        // collapse store. Prefer the just-stored
        // `last_tool_name`; fall back to looking
        // up the call by id in `tool_calls_buf`
        // (covers paths where `last_tool_name`
        // was drained out from under us — same
        // shape as the alert-bug fix).
        let resolved_name: String = ctx
            .last_tool_name
            .clone()
            .or_else(|| {
                ctx.tool_calls_buf
                    .iter()
                    .rev()
                    .find(|e| e.id == id.as_str())
                    .map(|e| e.name.to_string())
            })
            .unwrap_or_default();
        let resolved_args = ctx
            .tool_calls_buf
            .iter()
            .rev()
            .find(|e| e.id == id.as_str())
            .map(|e| e.args.clone())
            .unwrap_or(serde_json::Value::Null);
        let banner_value = format_tool_banner_value(&resolved_name, &resolved_args);
        let max_lines = ctx.cfg.resolve_tool_result_max_lines();

        // Review #7: gate the colorized diff path
        // on `resolved_name`, not `last_tool_name`
        // — if the name slot drained we'd lose
        // the green/red background coloring and
        // fall back to plain `render_tool_output`.
        let is_edit = resolved_name == "edit" && show_diff;
        // Review #6: empty name fallback would
        // paint an unnamed chamber AND collapse
        // it. Surface a single dim trailer and
        // emit the chamber bottom so the chamber
        // doesn't orphan. Skip the rest of branch
        // (a).
        if resolved_name.is_empty() {
            let (frame_w, inner) = chamber_widths(ctx.renderer);
            let trimmed = output.trim();
            let row_text = if trimmed.is_empty() {
                "(unresolved tool, no output)".to_string()
            } else {
                let first = trimmed.lines().next().unwrap_or("");
                format!("(unresolved tool) {}", first)
            };
            ctx.renderer
                .write_line(&chamber_row(&row_text, inner), theme::dim())?;
            ctx.renderer
                .write_line(&chamber_bottom(frame_w), theme::dim())?;
            *ctx.tool_chamber_open = false;
            *ctx.chamber_top_start = None;
            *ctx.chamber_top_end = None;
            *ctx.last_tool_name = None;
            // Early-exit equivalent of the inline arm's `continue`:
            // the inline `continue` skipped the trailing
            // `last_tool_name = None; last_tool_call_id = None;`
            // clears below, but `last_tool_name` was already cleared
            // here and `last_tool_call_id` is intentionally left
            // alone (matches inline behavior).
            return Ok(());
        }

        if is_edit {
            // Colorized diff rendering. The edit tool emits
            // its diff block starting with "--- a/<path>" —
            // match that exact sentinel to avoid false
            // positives on stray "--- " prefixes elsewhere
            // in the output.
            let lines: Vec<&str> = output.lines().collect();
            let diff_start = lines.iter().position(|l| l.starts_with("--- a/"));
            if let Some(pre) = diff_start {
                let (frame_w, inner) = chamber_widths(ctx.renderer);
                // Pre-diff prose (the edit tool's
                // header line, etc.) renders in
                // the chamber's standard tone.
                for l in &lines[..pre] {
                    if !l.is_empty() {
                        let txt = sanitize_output(l).into_string();
                        ctx.renderer
                            .write_line(&chamber_row(&txt, inner), theme::result())?;
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
                        ctx.renderer
                            .write_line(&chamber_row(&txt, inner), theme::accent())?;
                    } else if l.starts_with("@@") {
                        // Hunk position markers — use dim
                        // so they recede behind the +/-
                        // content lines below.
                        ctx.renderer
                            .write_line(&chamber_row(&txt, inner), theme::dim())?;
                    } else if l.starts_with('+') {
                        ctx.renderer
                            .write_line(&chamber_row_with_bg(&txt, inner, 22), Color::Green)?;
                    } else if l.starts_with('-') {
                        ctx.renderer
                            .write_line(&chamber_row_with_bg(&txt, inner, 52), Color::Red)?;
                    } else {
                        ctx.renderer
                            .write_line(&chamber_row(&txt, inner), theme::dim())?;
                    }
                }
                ctx.renderer
                    .write_line(&chamber_bottom(frame_w), theme::dim())?;
                *ctx.tool_chamber_open = false;
            } else {
                // No diff section found, show normally
                *ctx.last_collapsed = render_tool_output(
                    ctx.renderer,
                    &resolved_name,
                    &banner_value,
                    &output,
                    max_chars,
                    max_lines,
                )?;
                *ctx.tool_chamber_open = false;
            }
        } else {
            *ctx.last_collapsed = render_tool_output(
                ctx.renderer,
                &resolved_name,
                &banner_value,
                &output,
                max_chars,
                max_lines,
            )?;
            *ctx.tool_chamber_open = false;
        }
    }
    // Clear after consuming so a future stray ToolResult
    // can't be coloured with a stale tool name.
    *ctx.last_tool_name = None;
    *ctx.last_tool_call_id = None;
    Ok(())
}
