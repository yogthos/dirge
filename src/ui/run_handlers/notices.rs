//! Small status / notice `AgentEvent` arms extracted from
//! `run_interactive`. Each is render-only — a pure function of the
//! renderer plus the event payload — so they read and test far more
//! easily out here than buried in the multi-thousand-line `select!`
//! loop. Behavior is identical to the inline code; pure refactor.

use crossterm::style::Color;

use crate::agent::agent_loop::message::EscalationReason;
use crate::agent::agent_loop::tool_input_repair::RepairStatsSnapshot;
use crate::ui::renderer::Renderer;
use crate::ui::text_output::{
    strip_leading_system_reminder, write_critic_lines, write_system_lines, write_user_lines,
};
use crate::ui::theme;

/// `AgentEvent::UserMessage` — the literal prompt sent to the LLM. Strips
/// any leading `<system-reminder>` wrapper (added by
/// `prepend_pending_notifications` when background tasks just finished) so
/// the user sees only their own text; the clean copy is already persisted
/// to the session at submit time.
pub(crate) fn handle_user_message(renderer: &mut Renderer, content: &str) -> std::io::Result<()> {
    let visible = strip_leading_system_reminder(content);
    // dirge-vg9e: the in-loop critic re-enters as a user-role message so the
    // model acts on it; surface it under a distinct `<critic>` handle/color
    // rather than the user's `<you>`. The tag is stripped from the display.
    if let Some(body) = visible.strip_prefix(crate::agent::agent_loop::critic::CRITIC_TAG) {
        write_critic_lines(renderer, body.trim_start())?;
        return renderer.write_line("", Color::White);
    }
    write_user_lines(renderer, visible)?;
    renderer.write_line("", Color::White)
}

/// `AgentEvent::SystemNotice` — a dirge-originated `<system>` log line
/// (e.g. the max-agent-turns cap), rendered in the warning color so it
/// reads as runtime output rather than something the user typed.
pub(crate) fn handle_system_notice(renderer: &mut Renderer, content: &str) -> std::io::Result<()> {
    write_system_lines(renderer, content)?;
    renderer.write_line("", Color::White)
}

/// `AgentEvent::RetryNotice` — transient backoff banner (PROV-2) so the
/// user isn't staring at silence during retry delays.
pub(crate) fn handle_retry_notice(
    renderer: &mut Renderer,
    attempt: u32,
    delay_ms: u64,
) -> std::io::Result<()> {
    renderer.write_line(
        &format!("  ⟳ retry {attempt} ({delay_ms}ms)…"),
        theme::dim(),
    )
}

/// `AgentEvent::EscalationActivated` — Phase 4 dual-client tiering: the
/// next LLM call swapped to the escalation provider. Surface it so the
/// provider takeover isn't silent.
pub(crate) fn handle_escalation_activated(
    renderer: &mut Renderer,
    provider: &str,
    reason: &EscalationReason,
) -> std::io::Result<()> {
    let summary = reason.summary();
    renderer.write_line(
        &format!("  ↑ escalating to {provider} (next turn): {summary}"),
        theme::dim(),
    )
}

/// `AgentEvent::RepairStats` — per-run input-repair telemetry summary.
/// The caller guards the empty-snapshot case (it `continue`s the loop to
/// skip the trailing status redraw); this only renders the summary line.
pub(crate) fn handle_repair_stats(
    renderer: &mut Renderer,
    snapshot: &RepairStatsSnapshot,
) -> std::io::Result<()> {
    let mut parts: Vec<String> = Vec::new();
    if snapshot.md_link_unwrapped > 0 {
        parts.push(format!("{} md-link", snapshot.md_link_unwrapped));
    }
    if snapshot.null_stripped > 0 {
        parts.push(format!("{} null-strip", snapshot.null_stripped));
    }
    if snapshot.json_string_to_array > 0 {
        parts.push(format!("{} json-array", snapshot.json_string_to_array));
    }
    if snapshot.object_to_array > 0 {
        parts.push(format!("{} obj-to-array", snapshot.object_to_array));
    }
    if snapshot.bare_string_to_array > 0 {
        parts.push(format!("{} bare-to-array", snapshot.bare_string_to_array));
    }
    let total = snapshot.total_successful();
    let mut line = format!("  ⊕ repaired {total} input(s): {}", parts.join(", "));
    if snapshot.invalid > 0 {
        line.push_str(&format!("; {} invalid", snapshot.invalid));
    }
    renderer.write_line(&line, theme::dim())
}
