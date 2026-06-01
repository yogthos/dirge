//! Small text-rendering helpers used by the chat log: status-line
//! decoration, system-reminder stripping, multi-line user echo, and a
//! single-line sanitizer for tool-output strings.
//!
//! Extracted from `ui/mod.rs` so each helper is unit-testable in
//! isolation without dragging in the full `run_interactive` deps.

use crate::ui::events::sanitize_output;
use crate::ui::renderer::Renderer;
use crate::ui::theme;

/// Append a `q:N` queue-depth suffix to the status line when there are
/// interjections waiting to be sent to the agent. Hidden when the queue
/// is empty so the line doesn't gain noise during normal operation.
pub(crate) fn with_queue(s: String, n: usize) -> String {
    if n == 0 { s } else { format!("{} q:{}", s, n) }
}

/// Strip a leading `<system-reminder>…</system-reminder>` block (and
/// any trailing blank lines) so the user's visible echo shows only
/// what they typed. The agent loop currently sees the full prepended
/// string for LLM context; the UI's visible log should not. Used by
/// the `AgentEvent::UserMessage` handler. Returns the input slice
/// unchanged if no reminder block is present.
pub(crate) fn strip_leading_system_reminder(content: &str) -> &str {
    let trimmed = content.trim_start();
    let Some(rest) = trimmed.strip_prefix("<system-reminder>") else {
        return content;
    };
    let Some(end) = rest.find("</system-reminder>") else {
        return content;
    };
    let after = &rest[end + "</system-reminder>".len()..];
    after.trim_start_matches(['\n', '\r', ' ', '\t'])
}

#[cfg(test)]
mod strip_system_reminder_tests {
    use super::strip_leading_system_reminder;

    #[test]
    fn passes_plain_text_through() {
        assert_eq!(strip_leading_system_reminder("hello"), "hello");
    }

    #[test]
    fn strips_block_and_trailing_blank_lines() {
        let input = "<system-reminder>\nTask 1 done\n</system-reminder>\n\nwhat's next?";
        assert_eq!(strip_leading_system_reminder(input), "what's next?");
    }

    #[test]
    fn does_not_strip_mid_message_reminder() {
        let input = "see <system-reminder>nope</system-reminder>";
        assert_eq!(strip_leading_system_reminder(input), input);
    }

    #[test]
    fn handles_leading_whitespace_before_reminder() {
        let input = "  \n<system-reminder>x</system-reminder>\nhi";
        assert_eq!(strip_leading_system_reminder(input), "hi");
    }

    #[test]
    fn missing_close_tag_leaves_input_alone() {
        let input = "<system-reminder>oops";
        assert_eq!(strip_leading_system_reminder(input), input);
    }
}

/// Print a (possibly multi-line) prefixed message to the chat log as a
/// single visual block: the first line gets `prefix`, continuation lines
/// are indented to align under it, blank lines stay blank (so an expanded
/// paste / multi-line notice doesn't produce a column of empty prefix
/// markers), and an entirely-empty `text` still emits one prefix line so
/// the submission is acknowledged. Every line (including blanks and the
/// fallback) renders in `color`; `sanitize_output` strips control bytes
/// per line so paste-placeholder SOH markers and ANSI escapes can't leak
/// to the terminal.
///
/// Shared by `write_user_lines` (`<you>`) and `write_system_lines`
/// (`<system>`) so their wrap/blank/empty handling can't drift.
fn write_prefixed_lines(
    renderer: &mut Renderer,
    prefix: &str,
    color: crossterm::style::Color,
    text: &str,
) -> std::io::Result<()> {
    // Continuation indent = the prefix's visible width, so wrapped lines
    // line up under the first character of the body.
    let cont_indent = " ".repeat(prefix.chars().count());
    let mut prefix_emitted = false;
    for line in text.lines() {
        let safe = sanitize_output(line);
        if safe.is_empty() {
            renderer.write_line("", color)?;
            continue;
        }
        let formatted = if !prefix_emitted {
            prefix_emitted = true;
            format!("{}{}", prefix, safe)
        } else {
            format!("{}{}", cont_indent, safe)
        };
        renderer.write_line(&formatted, color)?;
    }
    if !prefix_emitted {
        renderer.write_line(prefix, color)?;
    }
    Ok(())
}

/// Print a (possibly multi-line) user-typed message to the chat log: the
/// first line gets the `<you> ` prefix in the user color, continuation
/// lines align under it. See `write_prefixed_lines`.
pub(crate) fn write_user_lines(renderer: &mut Renderer, text: &str) -> std::io::Result<()> {
    write_prefixed_lines(renderer, "<you> ", theme::user(), text)
}

/// Print the in-loop critic's review under a distinct `<critic> ` prefix in
/// the critic color, so it reads as a separate reviewing voice rather than
/// the user's own message (dirge-vg9e). See `write_prefixed_lines`.
pub(crate) fn write_critic_lines(renderer: &mut Renderer, text: &str) -> std::io::Result<()> {
    write_prefixed_lines(renderer, "<critic> ", theme::critic(), text)
}

/// Print a dirge-originated log/notice (e.g. the max-agent-turns cap) to
/// the chat log with a `<system> ` prefix in the *warning* color so it
/// reads as a transient runtime notice that stands out from the user's
/// own messages and agent output.
///
/// Note the deliberate divergence from persisted session-history system
/// messages (`MessageRole::System`), which render as `<sys>` in
/// `theme::system()` — see `render_session` in `ui/events.rs`. Those are
/// durable context (summaries, bootstrap); this is an attention-grabbing
/// live log line, hence the louder warning color and distinct label.
pub(crate) fn write_system_lines(renderer: &mut Renderer, text: &str) -> std::io::Result<()> {
    write_prefixed_lines(renderer, "<system> ", theme::warn(), text)
}

/// Flatten a multi-line / control-char-bearing string into one safe line
/// suitable for a single `write_line` call. Newlines, tabs, and ANSI escape
/// sequences would otherwise corrupt the renderer's per-line buffering — the
/// renderer splits on `\n` and writes raw bytes. Truncates to `max_chars`
/// characters and appends `…` when truncated.
pub(crate) fn sanitize_single_line(s: &str, max_chars: usize) -> String {
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
