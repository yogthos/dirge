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

/// Print a (possibly multi-line) user-typed message to the chat log
/// as a single visual message: the first line gets the `<you> `
/// prefix, continuation lines are indented to align under it, and
/// blank lines stay blank (so an expanded paste doesn't produce a
/// column of empty `<you>` markers, as reported by users pasting
/// multi-paragraph text). `sanitize_output` is applied per line to
/// strip control bytes — the paste-placeholder SOH markers in
/// particular must not leak to the terminal.
pub(crate) fn write_user_lines(renderer: &mut Renderer, text: &str) -> std::io::Result<()> {
    const PREFIX: &str = "<you> ";
    // Visible width of `PREFIX` — 6 cells. Used as the continuation
    // indent so wrapped lines line up under the first character of
    // the message body.
    const CONT_INDENT: &str = "      ";
    let mut prefix_emitted = false;
    for line in text.lines() {
        let safe = sanitize_output(line);
        if safe.is_empty() {
            // Preserve blank lines as actual blank rows — no prefix,
            // no indent — so paragraphs stay paragraphs in the log.
            renderer.write_line("", theme::user())?;
            continue;
        }
        let formatted = if !prefix_emitted {
            prefix_emitted = true;
            format!("{}{}", PREFIX, safe)
        } else {
            format!("{}{}", CONT_INDENT, safe)
        };
        renderer.write_line(&formatted, theme::user())?;
    }
    // If `text` was entirely empty (no `lines()` iterations) emit a
    // single `<you>` line so the user still sees their (empty)
    // submission acknowledged.
    if !prefix_emitted {
        renderer.write_line(PREFIX, theme::user())?;
    }
    Ok(())
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
