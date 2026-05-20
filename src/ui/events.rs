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
    let total = session.messages.len();
    for (idx, msg) in session.messages.iter().enumerate() {
        let (badge, line_color) = match msg.role {
            MessageRole::User => ("▌▌ USR ▏", theme::user()),
            MessageRole::Assistant => ("▌▌ AI  ▏", theme::agent()),
            MessageRole::System => ("▌▌ SYS ▏", theme::system()),
        };
        if msg.role == MessageRole::Assistant {
            let max_width = renderer.line_width();
            let mut styled = markdown::markdown_to_styled(&msg.content, max_width);
            if !styled.is_empty() {
                styled[0].text = CompactString::from(format!("{} {}", badge, styled[0].text));
            }
            for entry in styled {
                renderer.write_line(&entry.text, entry.color)?;
            }
        } else {
            for line in msg.content.lines() {
                renderer.write_line(&format!("{} {}", badge, line), line_color)?;
            }
        }
        // Insert a thin gradient turn-divider between messages (but not
        // after the last one). Visual rhythm hint without dominating.
        if idx + 1 < total {
            renderer.write_line(&turn_divider(renderer.line_width()), theme::divider())?;
        } else {
            renderer.write_line("", Color::Reset)?;
        }
    }
    Ok(())
}

/// A thin half-width gradient divider used between chat turns. Subtle
/// enough not to noise up the scrollback but distinctive enough to
/// signal a turn boundary at a glance.
fn turn_divider(term_w: usize) -> String {
    let w = term_w.min(60).max(20);
    let mut out = String::with_capacity(w);
    // Use ░▒ alternation for a phosphor scanline feel.
    for i in 0..w {
        out.push(if i % 2 == 0 { '░' } else { '▒' });
    }
    out
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

/// Welcome banner for the phosphor/plain theme. Renders a chunky
/// block-letter "DIRGE" wordmark sandwiched between gradient bars,
/// followed by a status line. Falls back to a single-line text banner
/// when the terminal is too narrow (< 42 cols) for the ASCII art.
fn render_banner(renderer: &mut Renderer, provider: &str, model: &str) -> anyhow::Result<()> {
    let label = theme::current().label;
    let version = env!("CARGO_PKG_VERSION");
    let term_w = renderer.line_width().max(20);

    // The block art is 38 cols wide; we need ~42 for it to breathe.
    // Narrow terminals get a one-line text banner instead.
    if term_w < 42 {
        let line = format!("▓▒░ DIRGE · {} · v{} ░▒▓", label, version);
        renderer.write_line(&line, theme::banner_primary())?;
        renderer.write_line(
            &format!("    provider: {} · model: {}", provider, model),
            theme::banner_secondary(),
        )?;
        renderer.write_line("", Color::Reset)?;
        return Ok(());
    }

    // Gradient bar — runs the full visible width (capped at 78) so the
    // banner anchors a "screen header" feel without bleeding into the
    // right-hand panel.
    let bar_width = term_w.min(78);
    let gradient = build_gradient_bar(bar_width);

    renderer.write_line(&gradient, theme::banner_secondary())?;
    renderer.write_line("", Color::Reset)?;
    for art_line in DIRGE_BLOCK_ART {
        renderer.write_line(&format!("   {}", art_line), theme::banner_primary())?;
    }
    renderer.write_line("", Color::Reset)?;
    // Status line in the wordmark's right gutter; centered under the
    // art for symmetry.
    let status = format!(
        "▓▒░ {} · v{} · {} · {} ░▒▓",
        label, version, provider, model
    );
    let pad = bar_width.saturating_sub(status.chars().count()) / 2;
    renderer.write_line(
        &format!("{}{}", " ".repeat(pad), status),
        theme::banner_secondary(),
    )?;
    renderer.write_line("", Color::Reset)?;
    renderer.write_line(&gradient, theme::banner_secondary())?;
    renderer.write_line("", Color::Reset)?;
    Ok(())
}

/// Build a `▓▒░░░▒▓` gradient bar exactly `width` chars wide. The
/// pattern fades from solid to sparse across each end, suggesting CRT
/// scanline edge dither without doing anything that actually depends
/// on color depth.
fn build_gradient_bar(width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let glyphs = ['█', '▓', '▒', '░', ' ', '░', '▒', '▓', '█'];
    // We want a smooth left-edge gradient that reaches solid blocks in
    // the middle and fades back out on the right.
    let mut out = String::with_capacity(width * 3);
    // Build half-gradients of length min(8, width / 3) and a solid
    // middle filling the remainder.
    let half = (width / 3).min(8);
    let solid = width.saturating_sub(half * 2);
    for i in 0..half {
        // Map left edge: 0..half -> indices 3..0 of glyphs (░▒▓█)
        let glyph_idx = 3usize.saturating_sub((i * 4) / half.max(1));
        out.push(glyphs[glyph_idx]);
    }
    for _ in 0..solid {
        out.push('█');
    }
    for i in 0..half {
        // Right edge: mirror the left.
        let glyph_idx = (((i + 1) * 4) / half.max(1)).min(3);
        out.push(glyphs[5 + glyph_idx]); // skip the space; reach ▒▓█
    }
    out
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
