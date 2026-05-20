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
    let model = if cli.model.is_none() && cfg.model.is_none() {
        compact_str::CompactString::new(crate::provider::default_model_for(&provider))
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
            MessageRole::User => ("<you>   ", theme::user()),
            MessageRole::Assistant => ("<dirge> ", theme::agent()),
            MessageRole::System => ("<sys>   ", theme::system()),
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
            let mut styled = markdown::markdown_to_styled(&msg.content, max_width);
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
            match chars.next() {
                Some('[') | Some(']') => {
                    for next in &mut chars {
                        if next.is_ascii_alphabetic() || next == '~' {
                            break;
                        }
                    }
                }
                Some(_) => {}
                None => break,
            }
        } else if c.is_ascii_control() && c != '\n' && c != '\t' && c != '\r' {
            continue;
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
