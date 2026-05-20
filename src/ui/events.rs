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
    for msg in &session.messages {
        let (prefix, line_color) = match msg.role {
            MessageRole::User => (">", theme::user()),
            MessageRole::Assistant => ("<", theme::agent()),
            MessageRole::System => ("#", theme::system()),
        };
        if msg.role == MessageRole::Assistant {
            let max_width = renderer.line_width();
            let mut styled = markdown::markdown_to_styled(&msg.content, max_width);
            if !styled.is_empty() {
                styled[0].text = CompactString::from(format!("{} {}", prefix, styled[0].text));
            }
            for entry in styled {
                renderer.write_line(&entry.text, entry.color)?;
            }
        } else {
            for line in msg.content.lines() {
                renderer.write_line(&format!("{} {}", prefix, line), line_color)?;
            }
        }
        renderer.write_line("", Color::Reset)?;
    }
    Ok(())
}

/// 80s-CRT welcome banner. Four lines: top border, wordmark + theme +
/// version, provider/model summary, bottom border. Width clamps to the
/// terminal so narrow windows don't see overrunning box-drawing chars.
fn render_banner(renderer: &mut Renderer, provider: &str, model: &str) -> anyhow::Result<()> {
    let label = theme::current().label;
    let version = env!("CARGO_PKG_VERSION");
    let title = format!("░ DIRGE ░ {} ░ v{}", label, version);
    let subtitle = format!("provider: {} · model: {}", provider, model);
    // Inner width = max content length + 2 padding on each side.
    // Clamp to terminal width − 4 (margin) so we don't push the banner
    // past the visible region.
    let term_w = renderer.line_width().max(20);
    let max_inner = term_w.saturating_sub(4);
    let inner = title
        .chars()
        .count()
        .max(subtitle.chars().count())
        .min(max_inner);
    let inner_width = inner + 2; // single-space padding each side

    let border_top = format!("╔{}╗", "═".repeat(inner_width));
    let border_bot = format!("╚{}╝", "═".repeat(inner_width));
    let title_line = format!("║ {:width$} ║", truncate(&title, inner), width = inner);
    let sub_line = format!("║ {:width$} ║", truncate(&subtitle, inner), width = inner);

    renderer.write_line(&border_top, theme::banner_secondary())?;
    renderer.write_line(&title_line, theme::banner_primary())?;
    renderer.write_line(&sub_line, theme::banner_secondary())?;
    renderer.write_line(&border_bot, theme::banner_secondary())?;
    renderer.write_line("", Color::Reset)?;
    Ok(())
}

/// Truncate a string to `max` *characters* (not bytes), adding an
/// ellipsis when shortened. Used by the banner so wordmarks stay
/// inside the box drawing.
fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else if max <= 1 {
        s.chars().take(max).collect()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

pub fn sanitize_output(text: &str) -> CompactString {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars();
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
