use chrono::Datelike;
use compact_str::CompactString;
use crossterm::style::Color;

use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
use crate::session::{MessageRole, Session};
use crate::ui::markdown;
use crate::ui::renderer::Renderer;
use crate::ui::theme;

/// dirge-jhky — derive a one-line preview describing what a
/// session was about, for the `/sessions` listing.
///
/// Order of preference:
/// 1. The Active Task / Goal line from the most recent
///    compaction summary, if any — that's the model's own one-
///    sentence answer to "what is the user trying to do here".
/// 2. The first user message (truncated to `max_chars`). User
///    prompts are typically a clear statement of intent; the
///    last assistant message often isn't.
/// 3. The last message content (truncated). Fall-back when
///    neither of the above applies — e.g., a fresh session with
///    only one assistant turn.
/// 4. Empty string for empty sessions.
///
/// The returned string is at most `max_chars` characters, with a
/// trailing ellipsis when truncation happened. Single line only —
/// newlines are replaced with spaces.
pub fn session_preview(session: &crate::session::Session, max_chars: usize) -> String {
    if session.messages.is_empty() && session.compactions.is_empty() {
        return String::new();
    }
    let raw = compaction_active_task(session)
        .or_else(|| first_user_message(session))
        .or_else(|| last_message_content(session))
        .unwrap_or_default();
    truncate_oneline(&raw, max_chars)
}

/// Extract the "## Active Task" or "## Goal" section's first
/// line from the most recent compaction summary. Returns `None`
/// when there's no compaction or no recognised section.
fn compaction_active_task(session: &crate::session::Session) -> Option<String> {
    let summary = session.compactions.last()?.summary.as_str();
    // Look for a section heading then take its first non-empty
    // content line. Active Task wins over Goal because it
    // identifies what the session is currently doing.
    for heading in ["## Active Task", "## Goal"] {
        if let Some(start) = summary.find(heading) {
            let rest = &summary[start + heading.len()..];
            // Skip blank lines after the heading, return the
            // first content line.
            for line in rest.lines() {
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                if t.starts_with('#') {
                    // Hit the next section without finding content.
                    break;
                }
                return Some(t.to_string());
            }
        }
    }
    None
}

fn first_user_message(session: &crate::session::Session) -> Option<String> {
    session
        .messages
        .iter()
        .find(|m| matches!(m.role, MessageRole::User))
        .map(|m| m.content.to_string())
        .filter(|s| !s.is_empty())
}

fn last_message_content(session: &crate::session::Session) -> Option<String> {
    session
        .messages
        .last()
        .map(|m| m.content.to_string())
        .filter(|s| !s.is_empty())
}

fn truncate_oneline(s: &str, max_chars: usize) -> String {
    // Collapse newlines + tabs to spaces so the preview stays
    // single-line. Trim runs of spaces so a wrapped prompt
    // doesn't render with double spaces in the middle.
    let mut collapsed = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        let mapped = if c == '\n' || c == '\r' || c == '\t' {
            ' '
        } else {
            c
        };
        if mapped == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        collapsed.push(mapped);
    }
    let trimmed = collapsed.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let prefix: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{prefix}…")
    }
}

pub fn format_time(rfc3339: &str) -> CompactString {
    let dt = chrono::DateTime::parse_from_rfc3339(rfc3339).ok();
    let dt = match dt {
        Some(dt) => dt,
        None => return CompactString::new(rfc3339),
    };
    let local = dt.with_timezone(&chrono::Local);
    let now = chrono::Local::now();
    if local.date_naive() == now.date_naive() {
        CompactString::new(local.format("%H:%M").to_string())
    } else if local.year() == now.year() {
        CompactString::new(local.format("%b %d %H:%M").to_string())
    } else {
        CompactString::new(local.format("%Y-%m-%d %H:%M").to_string())
    }
}

pub fn render_session(
    renderer: &mut Renderer,
    session: &Session,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
) -> anyhow::Result<()> {
    renderer.clear_content()?;
    let provider = cli.resolve_provider(cfg);
    let config_model = cfg
        .resolve_role(crate::config::ConfigRole::Default)
        .and_then(|(_, e)| e.model);
    let model = if cli.model.is_none() && config_model.is_none() {
        // dirge-j3jd: resolve the alias's provider TYPE so a custom alias
        // doesn't fall back to the OpenRouter default model id.
        compact_str::CompactString::new(crate::provider::default_model_for_alias(
            &provider,
            &cfg.providers_map(),
        ))
    } else {
        cli.resolve_model(cfg)
    };
    // Top padding rows. Without this, when the user scrolls all the
    // way up, the banner's top border `╭───╮` sits pressed against
    // the terminal's top edge, which reads as "cut off." Two blank
    // rows give the eye breathing room above the banner.
    renderer.write_line("", Color::Reset)?;
    renderer.write_line("", Color::Reset)?;
    render_banner(renderer, &provider, &model)?;
    if context.agents.is_some() {
        renderer.write_line("░ loaded AGENTS.md", theme::dim())?;
        renderer.write_line("", Color::Reset)?;
    }
    if !session.compactions.is_empty() {
        renderer.write_line(
            &format!(
                "░ compacted {} times (saved ~{} tokens)",
                session.compactions.len(),
                session
                    .compactions
                    .last()
                    .map(|c| c.token_savings)
                    .unwrap_or(0),
            ),
            theme::dim(),
        )?;
        renderer.write_line("", Color::Reset)?;
    }
    let total = session.messages.len();
    for (idx, msg) in session.messages.iter().enumerate() {
        // IRC-style angle-bracketed handle. All three handles padded
        // to 8 columns so multi-role chats stay visually aligned.
        // Continuation lines are indented to that same width so the
        // handle isn't repeated on every wrap.
        let (handle, line_color) = match msg.role {
            MessageRole::User => ("<you> ", theme::user()),
            MessageRole::Assistant => ("<dirge> ", theme::agent()),
            MessageRole::System => ("<sys> ", theme::system()),
        };
        let cont_indent = " ".repeat(handle.chars().count());

        if msg.role == MessageRole::Assistant {
            // Wrap chat to the same width tool chambers use so chat
            // and chamber blocks line up visually. The 8-col handle
            // prefix is subtracted so wrapped continuation text fits
            // beneath the handle position.
            let max_width = renderer
                .content_width()
                .saturating_sub(handle.chars().count() + 1);
            let mut styled = markdown::markdown_to_styled(&msg.content, max_width, line_color);
            for (i, entry) in styled.iter_mut().enumerate() {
                if i == 0 {
                    entry.text = CompactString::from(format!("{} {}", handle, entry.text));
                } else {
                    entry.text = CompactString::from(format!("{}{}", cont_indent, entry.text));
                }
            }
            for entry in styled {
                renderer.write_line(&entry.text, entry.color)?;
            }
        } else {
            for (i, line) in msg.content.lines().enumerate() {
                let prefix = if i == 0 {
                    handle.to_string()
                } else {
                    cont_indent.clone()
                };
                renderer.write_line(&format!("{} {}", prefix, line), line_color)?;
            }
        }
        // Thin chamber-bar divider between turns. Single character,
        // not a full-width gradient — the bar runs flush against the
        // left margin like an IRC log's timeline.
        if idx + 1 < total {
            renderer.write_line("·", theme::divider())?;
        } else {
            renderer.write_line("", Color::Reset)?;
        }
    }
    Ok(())
}

/// Block-letter "DIRGE" in the ANSI Shadow figlet style. Period-correct
/// 80s BBS aesthetic. Six lines tall, 38 chars wide.
const DIRGE_BLOCK_ART: &[&str] = &[
    "██████╗ ██╗██████╗  ██████╗ ███████╗",
    "██╔══██╗██║██╔══██╗██╔════╝ ██╔════╝",
    "██║  ██║██║██████╔╝██║  ███╗█████╗  ",
    "██║  ██║██║██╔══██╗██║   ██║██╔══╝  ",
    "██████╔╝██║██║  ██║╚██████╔╝███████╗",
    "╚═════╝ ╚═╝╚═╝  ╚═╝ ╚═════╝ ╚══════╝",
];

/// Welcome banner — block-letter "DIRGE" wordmark inside a rounded
/// frame with the theme/version/provider/model on the bottom border.
/// Mirrors the btop / cool-retro-term reference: every UI region is a
/// rounded panel with its label sitting on the border, no heavy
/// gradient stripes. Falls back to a single-line text banner on
/// terminals narrower than 50 cols.
fn render_banner(renderer: &mut Renderer, provider: &str, model: &str) -> anyhow::Result<()> {
    let label = theme::current().label;
    let version = env!("CARGO_PKG_VERSION");
    let term_w = renderer.line_width().max(20);

    if term_w < 50 {
        renderer.write_line(
            &format!("╭─ DIRGE · {} · v{} ", label, version),
            theme::banner_primary(),
        )?;
        renderer.write_line(
            &format!("│ provider: {} · model: {}", provider, model),
            theme::banner_secondary(),
        )?;
        renderer.write_line("╰─", theme::banner_secondary())?;
        renderer.write_line("", Color::Reset)?;
        return Ok(());
    }

    // Frame width: cap at 78 so the banner doesn't sprawl on wide
    // monitors, and so it sits visually proportionate to the chat.
    let frame_w = term_w.min(78);
    let inner_w = frame_w.saturating_sub(2);

    // Top border with the wordmark label sitting on it.
    let top_label = format!(" DIRGE · {} ", label);
    let top_label_len = top_label.chars().count();
    let top_filler = inner_w.saturating_sub(top_label_len + 2);
    let top_left = "─".repeat(2);
    let top_right = "─".repeat(top_filler);
    let top_border = format!("╭{}{}{}╮", top_left, top_label, top_right);

    // Bottom border with the status label.
    let bot_label = format!(" v{} · {} · {} ", version, provider, model);
    let bot_label_len = bot_label.chars().count();
    let bot_left = "─".repeat(inner_w.saturating_sub(bot_label_len + 2));
    let bot_right = "─".repeat(2);
    let bot_border = format!("╰{}{}{}╯", bot_left, bot_label, bot_right);

    renderer.write_line(&top_border, theme::banner_secondary())?;
    // Padding row above the art.
    renderer.write_line(
        &format!("│{}│", " ".repeat(inner_w)),
        theme::banner_secondary(),
    )?;
    // Block-letter art, padded on both sides to fill the frame.
    // The whole line renders in the bright banner_primary tone so the
    // wordmark glows; the surrounding empty padding rows + borders
    // stay dim, giving the eye a clear focal point.
    for art_line in DIRGE_BLOCK_ART {
        let art_len = art_line.chars().count();
        let total_pad = inner_w.saturating_sub(art_len);
        let left = total_pad / 2;
        let right = total_pad - left;
        let line = format!("│{}{}{}│", " ".repeat(left), art_line, " ".repeat(right),);
        renderer.write_line(&line, theme::banner_primary())?;
    }
    // Padding row below the art.
    renderer.write_line(
        &format!("│{}│", " ".repeat(inner_w)),
        theme::banner_secondary(),
    )?;
    renderer.write_line(&bot_border, theme::banner_secondary())?;
    renderer.write_line("", Color::Reset)?;
    Ok(())
}

pub fn sanitize_output(text: &str) -> CompactString {
    // Two-pass: first strip orphan SGR mouse reports of the form
    // `[<digits;digits;digits(M|m)` (no leading escape). These can
    // leak into tool output when a shell command captures terminal
    // input bytes, and without this guard they smear `[<65;79;32M…`
    // through the chamber. Then run the regular ANSI/control-char
    // sanitizer over the cleaned text.
    let stripped = strip_orphan_mouse_reports(text);

    let mut result = String::with_capacity(stripped.len());
    let mut chars = stripped.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Strip ALL escape sequences, not just CSI/OSC.
            // CSI: ESC [ ... final-byte (0x40..=0x7E)
            // OSC: ESC ] ... BEL or ESC \
            // DCS/APC/PM/SOS: ESC P/X/^/_ ... ESC \
            // Single-byte: ESC + any other char (reset, etc.)
            match chars.next() {
                Some('[') => {
                    let mut n = 0;
                    for next in &mut chars {
                        let cp = next as u32;
                        if (0x40..=0x7e).contains(&cp) {
                            break;
                        }
                        n += 1;
                        if n >= 256 {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let mut n = 0;
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' {
                            let mut peek = chars.clone();
                            if peek.next() == Some('\\') {
                                chars = peek;
                                break;
                            }
                        }
                        n += 1;
                        if n >= 256 {
                            break;
                        }
                    }
                }
                // DCS/APC/PM/SOS — consume until ST (ESC \). Cap at 4 KB.
                Some('P') | Some('X') | Some('^') | Some('_') => {
                    let mut prev = '\0';
                    let mut n = 0;
                    for next in &mut chars {
                        if prev == '\x1b' && next == '\\' {
                            break;
                        }
                        prev = next;
                        n += 1;
                        if n >= 4096 {
                            break;
                        }
                    }
                }
                Some(_) => {} // Single-byte esc sequence — skip the second byte.
                None => break,
            }
        } else if c.is_ascii_control() || (0x80..=0x9F).contains(&(c as u32)) {
            if c != '\n' && c != '\t' {
                continue;
            }
            result.push(c);
        } else {
            result.push(c);
        }
    }
    CompactString::from(result)
}

/// Strip orphan SGR mouse-report sequences (e.g. `[<65;79;32M`) that
/// arrive without their leading `\x1b`. Walks the input scanning for
/// the literal pattern `[<` followed by digits and semicolons ending
/// in `M` or `m`; matched runs are dropped. Anything else passes
/// through unchanged.
fn strip_orphan_mouse_reports(text: &str) -> String {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '[' && i + 1 < bytes.len() && bytes[i + 1] == '<' {
            // Try to match `[<digits;digits;digits(M|m)`.
            let mut j = i + 2;
            let mut saw_digit_or_semi = false;
            while j < bytes.len() {
                let c = bytes[j];
                if c.is_ascii_digit() || c == ';' {
                    saw_digit_or_semi = true;
                    j += 1;
                } else if (c == 'M' || c == 'm') && saw_digit_or_semi {
                    i = j + 1;
                    break;
                } else {
                    // Not a mouse report — pass `[` through and resume
                    // scanning at the next position.
                    out.push(bytes[i]);
                    i += 1;
                    break;
                }
            }
            if j >= bytes.len() {
                // Truncated input — pass through what we have.
                out.push(bytes[i]);
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{MessageRole, Session};

    fn make_session() -> Session {
        Session::new("openrouter", "gpt-5", 200_000)
    }

    #[test]
    fn session_preview_empty_session_is_empty() {
        let s = make_session();
        assert_eq!(session_preview(&s, 60), "");
    }

    #[test]
    fn session_preview_uses_first_user_message_when_no_compaction() {
        let mut s = make_session();
        s.add_message(
            MessageRole::User,
            "Implement session_search exclusion for current session",
        );
        s.add_message(MessageRole::Assistant, "ok done");
        let p = session_preview(&s, 60);
        assert!(
            p.starts_with("Implement session_search"),
            "preview should lead with the user prompt: {p:?}"
        );
        assert!(!p.contains("ok done"));
    }

    #[test]
    fn session_preview_truncates_long_prompts_with_ellipsis() {
        let mut s = make_session();
        let long = "x".repeat(200);
        s.add_message(MessageRole::User, &long);
        let p = session_preview(&s, 30);
        assert_eq!(p.chars().count(), 30, "preview must respect max_chars");
        assert!(p.ends_with('…'));
    }

    #[test]
    fn session_preview_collapses_newlines_to_single_line() {
        let mut s = make_session();
        s.add_message(MessageRole::User, "first line\n\nsecond line\nthird\ttab");
        let p = session_preview(&s, 80);
        assert!(!p.contains('\n'), "preview must be single-line: {p:?}");
        assert!(!p.contains('\t'));
        // Multiple consecutive whitespace collapses to one.
        assert!(!p.contains("  "), "double-space remained: {p:?}");
        assert!(p.contains("first line second line third tab"));
    }

    #[test]
    fn session_preview_skips_system_messages_for_first_user() {
        let mut s = make_session();
        s.add_message(MessageRole::System, "system bootstrap");
        s.add_message(MessageRole::User, "user intent");
        s.add_message(MessageRole::Assistant, "reply");
        let p = session_preview(&s, 60);
        assert!(p.contains("user intent"));
        assert!(!p.contains("system bootstrap"));
        assert!(!p.contains("reply"));
    }

    #[test]
    fn session_preview_falls_back_to_last_when_no_user_message() {
        // Session containing only assistant/system messages — no
        // user prompt to anchor on. Fall back to the last
        // message content.
        let mut s = make_session();
        s.add_message(MessageRole::Assistant, "spontaneous greeting");
        s.add_message(MessageRole::Assistant, "follow-up reply");
        let p = session_preview(&s, 60);
        assert!(
            p.contains("follow-up reply"),
            "fallback should be last message: {p:?}"
        );
    }

    #[test]
    fn session_preview_prefers_compaction_active_task() {
        let mut s = make_session();
        s.add_message(MessageRole::User, "old prompt that got compacted");
        s.compactions.push(crate::session::Compaction {
            summary: compact_str::CompactString::new(
                "## Active Task\nWire session_search current-session exclusion\n\n## Goal\nFix the bug",
            ),
            first_kept_index: 1,
            summarized_count: 1,
            token_savings: 100,
            created_at: compact_str::CompactString::new("2026-05-28T00:00:00Z"),
        });
        let p = session_preview(&s, 80);
        assert!(
            p.contains("Wire session_search current-session exclusion"),
            "preview must use compaction Active Task: {p:?}"
        );
        assert!(
            !p.contains("old prompt"),
            "compaction overrides first-user-message: {p:?}"
        );
    }

    #[test]
    fn session_preview_falls_back_to_goal_when_active_task_missing() {
        let mut s = make_session();
        s.add_message(MessageRole::User, "u");
        s.compactions.push(crate::session::Compaction {
            summary: compact_str::CompactString::new(
                "## Goal\nrefactor the curator into umbrella skills\n\n## Completed\n- nothing",
            ),
            first_kept_index: 1,
            summarized_count: 1,
            token_savings: 50,
            created_at: compact_str::CompactString::new("2026-05-28T00:00:00Z"),
        });
        let p = session_preview(&s, 80);
        assert!(
            p.contains("refactor the curator into umbrella skills"),
            "Goal section should be used when Active Task absent: {p:?}"
        );
    }

    #[test]
    fn session_preview_ignores_empty_active_task_section() {
        let mut s = make_session();
        s.add_message(MessageRole::User, "the user's actual intent");
        s.compactions.push(crate::session::Compaction {
            // Active Task heading present but immediately followed
            // by the next section — no content. Should fall through
            // to the first user message.
            summary: compact_str::CompactString::new("## Active Task\n\n## Goal\n\n## Notes\n"),
            first_kept_index: 1,
            summarized_count: 1,
            token_savings: 0,
            created_at: compact_str::CompactString::new("2026-05-28T00:00:00Z"),
        });
        let p = session_preview(&s, 80);
        assert!(
            p.contains("the user's actual intent"),
            "should fall through past empty sections to user message: {p:?}"
        );
    }
}
