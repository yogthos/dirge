use crossterm::style::Color;

use crate::ui::events::sanitize_output;
use crate::ui::renderer::Renderer;

use super::box_render;
use super::theme;

#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct CollapsedToolResult {
    pub tool_name: String,
    pub banner_value: String,
    pub full_output: String,
}

pub(crate) fn format_tool_banner_value(name: &str, args: &serde_json::Value) -> String {
    let obj = match args {
        serde_json::Value::Object(map) => map,
        _ => return String::new(),
    };
    if name == "apply_patch" {
        let n = obj
            .get("operations")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        return match n {
            0 => String::new(),
            1 => "1 op".to_string(),
            _ => format!("{n} ops"),
        };
    }
    let key = match name {
        "read" | "write" | "edit" | "list_dir" => "path",
        "grep" => "pattern",
        "find_files" | "glob" => "pattern",
        "bash" => "command",
        "question" => "questions",
        "task" => "prompt",
        "task_status" => "task_id",
        _ => return String::new(),
    };
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Build the rounded-chamber top border, left-truncating `value`
/// to fill the available width up to `─╮`. Layout:
///
///   `╭─ TOOL ─ "value"─…─╮`
pub(crate) fn fit_banner_header(name_upper: &str, value: &str, frame_w: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;

    let value_owned: String;
    let value: &str = if value.contains(['\n', '\r', '\t']) {
        value_owned = value
            .chars()
            .map(|c| {
                if c == '\n' || c == '\r' || c == '\t' {
                    ' '
                } else {
                    c
                }
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        value_owned.as_str()
    } else {
        value
    };

    const FRAME_OVERHEAD: usize = 8; // "╭─ " (3) + " ─ " (3) + "─╮" (2)
    let name_budget = frame_w.saturating_sub(FRAME_OVERHEAD + 3);
    let name_w = name_upper.width();
    let displayed_name: String = if name_w <= name_budget || name_budget == 0 {
        name_upper.to_string()
    } else {
        let tail_budget = name_budget.saturating_sub(1);
        let mut tail: Vec<char> = Vec::new();
        let mut used = 0;
        for ch in name_upper.chars().rev() {
            let w = ch.width().unwrap_or(0);
            if used + w > tail_budget {
                break;
            }
            tail.push(ch);
            used += w;
        }
        tail.reverse();
        format!("…{}", tail.into_iter().collect::<String>())
    };

    let prefix = format!("╭─ {} ─ ", displayed_name);
    let suffix = "─╮";
    let prefix_w = prefix.as_str().width();
    let suffix_w = suffix.width();
    if value.is_empty() {
        let used = prefix_w + suffix_w;
        let pad = frame_w.saturating_sub(used);
        return format!("{}{}{}", prefix, "─".repeat(pad), suffix);
    }
    let quote_w = 2;
    let value_budget = frame_w.saturating_sub(prefix_w + suffix_w + quote_w);
    if value_budget == 0 {
        let used = prefix_w + suffix_w;
        let pad = frame_w.saturating_sub(used);
        return format!("{}{}{}", prefix, "─".repeat(pad), suffix);
    }

    let value_w = value.width();
    let shown_value = if value_w <= value_budget {
        value.to_string()
    } else {
        use unicode_width::UnicodeWidthChar;
        let tail_budget = value_budget.saturating_sub(1);
        let mut tail: Vec<char> = Vec::new();
        let mut used = 0;
        for ch in value.chars().rev() {
            let w = ch.width().unwrap_or(0);
            if used + w > tail_budget {
                break;
            }
            tail.push(ch);
            used += w;
        }
        tail.reverse();
        let tail_str: String = tail.into_iter().collect();
        format!("…{}", tail_str)
    };

    let shown_w = shown_value.as_str().width() + quote_w;
    let total_used = prefix_w + shown_w + suffix_w;
    let pad = frame_w.saturating_sub(total_used);
    format!("{}\"{}\"{}{}", prefix, shown_value, "─".repeat(pad), suffix)
}

/// Write a line of text that must NOT land inside an open tool
/// chamber. Closes the chamber first if any signal indicates one
/// is open.
pub(crate) fn write_outside_chamber(
    renderer: &mut Renderer,
    last_tool_name: &mut Option<String>,
    tool_chamber_open: &mut bool,
    chamber_top_start: &mut Option<usize>,
    chamber_top_end: &mut Option<usize>,
    text: &str,
    color: Color,
) -> anyhow::Result<()> {
    close_tool_chamber_passive(
        renderer,
        last_tool_name,
        tool_chamber_open,
        chamber_top_start,
        chamber_top_end,
    )?;
    let safe = crate::ui::ansi::strip_controls(text, crate::ui::ansi::StripPolicy::KEEP_NEWLINE);
    renderer.write_line(&safe, color)?;
    Ok(())
}

/// Close an in-flight chamber WITH an abort/denied row painted inside.
pub(crate) fn close_tool_chamber_abort(
    renderer: &mut Renderer,
    last_tool_name: &mut Option<String>,
    tool_chamber_open: &mut bool,
) -> anyhow::Result<()> {
    if last_tool_name.is_some() || *tool_chamber_open {
        let (frame_w, inner) = chamber_widths(renderer);
        renderer.write_line(
            &chamber_row_centered("⚠ tool denied · aborted · no result", inner),
            theme::perm(),
        )?;
        renderer.write_line(&chamber_bottom(frame_w), theme::dim())?;
        *last_tool_name = None;
        *tool_chamber_open = false;
    }
    Ok(())
}

/// Close an in-flight chamber WITHOUT painting an abort row.
pub(crate) fn close_tool_chamber_passive(
    renderer: &mut Renderer,
    last_tool_name: &mut Option<String>,
    tool_chamber_open: &mut bool,
    chamber_top_start: &mut Option<usize>,
    chamber_top_end: &mut Option<usize>,
) -> anyhow::Result<()> {
    if last_tool_name.is_some() || *tool_chamber_open {
        let drop_chamber = match (*chamber_top_start, *chamber_top_end) {
            (Some(_start), Some(end)) => renderer.buffer_len() == end,
            _ => false,
        };
        if drop_chamber {
            if let Some(start) = *chamber_top_start {
                renderer.replace_from(start, Vec::new());
            }
        } else {
            let (frame_w, _inner) = chamber_widths(renderer);
            renderer.write_line(&chamber_bottom(frame_w), theme::dim())?;
        }
        *last_tool_name = None;
        *tool_chamber_open = false;
        *chamber_top_start = None;
        *chamber_top_end = None;
    }
    Ok(())
}

/// Back-compat alias for the abort variant.
pub(crate) fn close_tool_chamber_if_open(
    renderer: &mut Renderer,
    last_tool_name: &mut Option<String>,
    tool_chamber_open: &mut bool,
) -> anyhow::Result<()> {
    close_tool_chamber_abort(renderer, last_tool_name, tool_chamber_open)
}

/// `│   <content centered to inner>   │`
pub(crate) fn chamber_row_centered(content: &str, inner: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let len = UnicodeWidthStr::width(content);
    if len >= inner {
        return chamber_row(content, inner);
    }
    let pad = inner - len;
    let left = pad / 2;
    let right = pad - left;
    format!("│ {}{}{} │", " ".repeat(left), content, " ".repeat(right))
}

pub(crate) fn tool_skips_collapse(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "edit" | "read" | "question" | "task" | "task_status"
    )
}

/// Render a tool result chamber. Returns `Some(CollapsedToolResult)` if
/// truncated.
pub(crate) fn render_tool_output(
    renderer: &mut Renderer,
    tool_name: &str,
    banner_value: &str,
    output: &str,
    max_chars: usize,
    max_lines: usize,
) -> anyhow::Result<Option<CollapsedToolResult>> {
    let sanitized = sanitize_output(output);
    let total_chars = sanitized.chars().count();
    let char_sliced: String = if total_chars <= max_chars {
        sanitized.into_string()
    } else {
        sanitized.chars().take(max_chars).collect()
    };
    let chars_truncated = total_chars.saturating_sub(char_sliced.chars().count());

    let lines: Vec<&str> = char_sliced.lines().collect();
    let total_lines = lines.len();
    let line_cap = if tool_skips_collapse(tool_name) {
        usize::MAX
    } else {
        max_lines
    };
    let shown_lines = total_lines.min(line_cap);
    let hidden_lines = total_lines.saturating_sub(shown_lines);

    let (frame_w, inner) = chamber_widths(renderer);
    let body_is_empty = char_sliced.trim().is_empty();
    if body_is_empty {
        let placeholder = match tool_name {
            "glob" | "grep" | "find" | "semantic" => "(no matches)",
            "read" => "(empty file)",
            "bash" => "(no output)",
            _ => "(no output)",
        };
        renderer.write_line(&chamber_row(placeholder, inner), theme::dim())?;
    }
    // dirge-hukk: syntax-highlight `read` boxes (file content) by the file's
    // extension. Unknown/extensionless files fall through to the plain path.
    if let Some(lang) = read_highlight_lang(tool_name, banner_value) {
        let highlighted = crate::ui::highlight::highlight_code(&char_sliced, &lang);
        for spans in highlighted.iter().take(shown_lines) {
            let ansi = spans_to_ansi(spans);
            renderer.write_line(
                &chamber_row_styled(&ansi, inner, theme::result()),
                theme::result(),
            )?;
        }
    } else {
        for line in &lines[..shown_lines] {
            renderer.write_line(&chamber_row(line, inner), theme::result())?;
        }
    }
    if hidden_lines > 0 {
        let note = format!(
            "↓ {} more line{} (Ctrl+O to expand)",
            hidden_lines,
            if hidden_lines == 1 { "" } else { "s" }
        );
        renderer.write_line(&chamber_row(&note, inner), theme::dim())?;
    }
    if chars_truncated > 0 {
        let note = format!("░ +{} chars truncated (output too large)", chars_truncated);
        renderer.write_line(&chamber_row(&note, inner), theme::dim())?;
    }
    renderer.write_line(&chamber_bottom(frame_w), theme::dim())?;

    if hidden_lines > 0 || chars_truncated > 0 {
        Ok(Some(CollapsedToolResult {
            tool_name: tool_name.to_string(),
            banner_value: sanitize_output(banner_value).into_string(),
            full_output: output.to_string(),
        }))
    } else {
        Ok(None)
    }
}

/// Re-render a previously-collapsed result with NO line cap.
pub(crate) fn render_collapsed_in_full(
    renderer: &mut Renderer,
    collapsed: &CollapsedToolResult,
    max_chars: usize,
) -> anyhow::Result<()> {
    let upper = collapsed.tool_name.to_ascii_uppercase();
    let (frame_w, _) = chamber_widths(renderer);
    let header = fit_banner_header(&upper, &collapsed.banner_value, frame_w);
    renderer.write_line("", Color::White)?;
    renderer.write_line(&header, theme::tool())?;
    let _ = render_tool_output(
        renderer,
        &collapsed.tool_name,
        &collapsed.banner_value,
        &collapsed.full_output,
        max_chars,
        usize::MAX,
    )?;
    Ok(())
}

pub(crate) fn chamber_widths(renderer: &Renderer) -> (usize, usize) {
    let frame_w = renderer.content_width().saturating_sub(1).max(20);
    let inner = frame_w.saturating_sub(4); // `│ ` + ` │`
    (frame_w, inner)
}

pub(crate) fn chamber_bottom(frame_w: usize) -> String {
    box_render::bottom(box_render::BoxStyle::Rounded, frame_w)
}

pub(crate) fn chamber_row(content: &str, inner: usize) -> String {
    box_render::row(box_render::BoxStyle::Rounded, content, inner + 4)
}

/// Chamber row for ANSI-styled content (syntax-highlighted code). Unlike
/// [`chamber_row`], which is byte-width based and expects plain text, this
/// measures with the ANSI-aware [`crate::ui::wrap::visible_width`], truncates
/// without cutting an SGR sequence, and restores `base` (the row's themed
/// color) for the trailing padding + border so highlight colors don't bleed
/// into the chamber frame. dirge-hukk.
pub(crate) fn chamber_row_styled(content: &str, inner: usize, base: Color) -> String {
    let base_ansi = crate::ui::markdown::ansi_fg(base);
    let vis = crate::ui::wrap::visible_width(content);
    let body = if vis <= inner {
        format!("{content}{base_ansi}{}", " ".repeat(inner - vis))
    } else {
        // Reserve one cell for the `…` ellipsis.
        let truncated = truncate_ansi(content, inner.saturating_sub(1));
        format!("{truncated}{base_ansi}…")
    };
    format!("│ {body} │")
}

/// Truncate an ANSI-styled string to at most `max` visible cells, copying
/// SGR escape sequences through verbatim (0 width) so a color code is never
/// cut in half.
fn truncate_ansi(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::with_capacity(s.len());
    let mut width = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Copy the whole escape sequence: ESC, then up to the final
            // letter (e.g. `m` for SGR). Zero display width.
            out.push(c);
            for ec in chars.by_ref() {
                out.push(ec);
                if ec.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > max {
            break;
        }
        out.push(c);
        width += w;
    }
    out
}

/// Flatten one highlighted line's spans into an ANSI-embedded string
/// (`<color-sgr><text>` per span). Consumed by [`chamber_row_styled`].
fn spans_to_ansi(spans: &[crate::ui::highlight::Span]) -> String {
    let mut out = String::new();
    for span in spans {
        out.push_str(&crate::ui::markdown::ansi_fg(span.color));
        out.push_str(&span.text);
    }
    out
}

/// The fenced-code language id for a `read` box, derived from the file
/// path's extension (`banner_value`). `None` disables highlighting (plain
/// `theme::result()` rendering) — for extensionless files and non-`read`
/// tools whose output isn't a single source file.
fn read_highlight_lang(tool_name: &str, banner_value: &str) -> Option<String> {
    if tool_name != "read" {
        return None;
    }
    let ext = std::path::Path::new(banner_value.trim())
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    // Only highlight languages we actually have rules for; otherwise leave
    // the content in the plain themed color (don't paint it the uniform
    // fallback tone).
    crate::ui::highlight::supports(&ext).then_some(ext)
}

pub(crate) fn chamber_row_with_bg(content: &str, inner: usize, bg_idx: u8) -> String {
    box_render::row_with_bg(box_render::BoxStyle::Rounded, content, inner + 4, bg_idx)
}

/// Index of the line where the `write`/`edit` tools' appended LSP
/// diagnostics block begins (the `LSP errors detected …` heading), or
/// `None` if the output has no such section. Used to keep the diff
/// visible in full while collapsing the (often huge, often
/// language-server-noise) diagnostics tail into one line.
pub(crate) fn lsp_block_start(lines: &[&str]) -> Option<usize> {
    lines
        .iter()
        .position(|l| l.trim_start().starts_with("LSP errors detected"))
}

/// Parse a flood headline like `258 errors reported — too many …` into
/// its leading count.
fn parse_errors_reported(line: &str) -> Option<usize> {
    let rest = line.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = rest[digits.len()..].trim_start();
    after
        .starts_with("errors reported")
        .then(|| digits.parse().ok())
        .flatten()
}

/// Parse an overflow footer like `... and 15 more` into its count.
fn parse_and_more(line: &str) -> Option<usize> {
    let rest = line.trim_start().strip_prefix("... and ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    rest[digits.len()..]
        .trim_start()
        .starts_with("more")
        .then(|| digits.parse().ok())
        .flatten()
}

/// Collapse an appended LSP diagnostics tail (the slice from
/// [`lsp_block_start`] onward) into a single compact summary line.
/// Counts `<diagnostics file=…>` blocks and best-effort totals their
/// errors (flood headlines, enumerated `ERROR` lines, `… and N more`
/// footers). Falls back gracefully when the count can't be derived.
pub(crate) fn summarize_lsp_tail(tail: &[&str]) -> String {
    let mut files = 0usize;
    let mut errors = 0usize;
    let mut i = 0;
    while i < tail.len() {
        if tail[i].trim_start().starts_with("<diagnostics file=") {
            files += 1;
            let mut j = i + 1;
            let mut enumerated = 0usize;
            let mut flood: Option<usize> = None;
            while j < tail.len() && !tail[j].trim_start().starts_with("</diagnostics>") {
                let bl = tail[j].trim_start();
                if let Some(n) = parse_errors_reported(bl) {
                    flood = Some(n);
                } else if bl.starts_with("ERROR ") {
                    enumerated += 1;
                } else if let Some(n) = parse_and_more(bl) {
                    enumerated += n;
                }
                j += 1;
            }
            errors += flood.unwrap_or(enumerated);
            i = j + 1;
        } else {
            i += 1;
        }
    }

    let suffix = "Ctrl+O to expand";
    if files == 0 {
        return format!("⚠ LSP errors reported — {suffix}");
    }
    let fpl = if files == 1 { "" } else { "s" };
    if errors == 0 {
        format!("⚠ LSP errors in {files} file{fpl} — {suffix}")
    } else {
        let epl = if errors == 1 { "" } else { "s" };
        format!("⚠ LSP: {errors} error{epl} in {files} file{fpl} — {suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn lsp_block_start_finds_heading() {
        let lines = vec![
            "--- a/x.rs",
            "+++ b/x.rs",
            "+added",
            "",
            "LSP errors detected in this file, please fix:",
            "<diagnostics file=\"x.rs\">",
        ];
        assert_eq!(lsp_block_start(&lines), Some(4));
    }

    #[test]
    fn lsp_block_start_none_without_diagnostics() {
        let lines = vec!["--- a/x.rs", "+++ b/x.rs", "+added"];
        assert_eq!(lsp_block_start(&lines), None);
    }

    #[test]
    fn summarize_counts_enumerated_and_overflow() {
        let lines = vec![
            "LSP errors detected in this file, please fix:",
            "<diagnostics file=\"x.rs\">",
            "ERROR [1:1] a",
            "ERROR [2:1] b",
            "... and 4 more",
            "</diagnostics>",
        ];
        // 2 enumerated + 4 overflow = 6 errors across 1 file.
        assert_eq!(
            summarize_lsp_tail(&lines),
            "⚠ LSP: 6 errors in 1 file — Ctrl+O to expand"
        );
    }

    #[test]
    fn summarize_uses_flood_total_not_preview_lines() {
        // A flood block reports its own total and previews 3 lines; the
        // preview ERROR lines must NOT be double-counted on top of 258.
        let lines = vec![
            "LSP errors detected in this file, please fix:",
            "<diagnostics file=\"core.janet\">",
            "258 errors reported — too many to be from a single edit. …",
            "ERROR [1:13] Unresolved symbol: Library",
            "ERROR [2:27] Unresolved symbol: functions",
            "ERROR [2:41] Unresolved symbol: the",
            "</diagnostics>",
            "",
            "LSP errors detected in other files:",
            "<diagnostics file=\"api.janet\">",
            "ERROR [1:15] Unresolved symbol: API",
            "... and 4 more",
            "</diagnostics>",
        ];
        // 258 (flood) + (1 + 4) = 263 across 2 files.
        assert_eq!(
            summarize_lsp_tail(&lines),
            "⚠ LSP: 263 errors in 2 files — Ctrl+O to expand"
        );
    }

    #[test]
    fn summarize_singular_file_and_error() {
        let lines = vec![
            "LSP errors detected in this file, please fix:",
            "<diagnostics file=\"x.rs\">",
            "ERROR [1:1] only one",
            "</diagnostics>",
        ];
        assert_eq!(
            summarize_lsp_tail(&lines),
            "⚠ LSP: 1 error in 1 file — Ctrl+O to expand"
        );
    }

    #[test]
    fn banner_short_value_pads_with_dashes_to_full_width() {
        let header = fit_banner_header("READ", "/tmp/x", 60);
        assert_eq!(
            header.as_str().width(),
            60,
            "header should fill frame_w exactly: {:?}",
            header,
        );
        assert!(header.starts_with("╭─ READ ─ \"/tmp/x\""));
        assert!(header.ends_with("─╮"));
    }

    #[test]
    fn banner_has_no_internal_space_before_corner() {
        let header = fit_banner_header("READ", "/short", 50);
        let mut chars: Vec<char> = header.chars().collect();
        let last = chars.pop();
        assert_eq!(last, Some('╮'));
        let second_last = chars.pop();
        assert_eq!(
            second_last,
            Some('─'),
            "char before closing ╮ must be `─`, not space; got {:?}",
            second_last,
        );
    }

    #[test]
    fn banner_long_path_left_truncates_to_preserve_filename() {
        let path = "/very/very/very/long/nested/path/to/some/file/named/important.clj";
        let header = fit_banner_header("READ", path, 60);
        assert_eq!(header.as_str().width(), 60);
        assert!(header.contains("important.clj"));
        assert!(header.contains('…'));
        assert!(!header.contains("/very/very/very/long"));
    }

    #[test]
    fn banner_collapses_embedded_newlines_to_single_line() {
        let multi = "clang++ predecessor.cpp \\\n  nikon_he_precinct_decode.cpp 2>&1";
        let header = fit_banner_header("BASH", multi, 80);
        assert!(!header.contains('\n'));
        assert!(!header.contains('\t'));
        assert!(!header.contains('\r'));
        assert_eq!(header.as_str().width(), 80);
    }

    #[test]
    fn banner_collapses_embedded_tabs() {
        let header = fit_banner_header("READ", "path\twith\ttabs", 60);
        assert!(!header.contains('\t'));
        assert_eq!(header.as_str().width(), 60);
    }

    #[test]
    fn banner_empty_value_renders_just_prefix_and_dashes() {
        let header = fit_banner_header("DONE", "", 50);
        assert_eq!(header.as_str().width(), 50);
        assert!(!header.contains("\"\""));
        assert!(header.starts_with("╭─ DONE ─"));
        assert!(header.ends_with("─╮"));
    }

    #[test]
    fn chamber_row_centered_handles_wide_emoji() {
        let row = chamber_row_centered("⚠ tool denied", 40);
        let row_width = UnicodeWidthStr::width(row.as_str());
        assert_eq!(row_width, 44);
        assert!(row.ends_with(" │"));
    }

    #[test]
    fn chamber_row_handles_wide_emoji() {
        let row = chamber_row("ok ✅", 40);
        let row_width = UnicodeWidthStr::width(row.as_str());
        assert_eq!(row_width, 44);
        assert!(row.ends_with(" │"));

        let long = "日本語日本語日本語日本語日本語日本語日本語日本語日本語日本語";
        let row = chamber_row(long, 20);
        let row_width = UnicodeWidthStr::width(row.as_str());
        assert_eq!(row_width, 24);
        assert!(row.ends_with(" │"));
    }

    #[test]
    fn close_tool_chamber_fires_when_only_flag_is_open() {
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let mut name: Option<String> = None;
        let mut open = true;
        close_tool_chamber_if_open(&mut renderer, &mut name, &mut open).unwrap();
        assert!(!open);
        assert!(name.is_none());

        let mut name: Option<String> = Some("read".to_string());
        let mut open = false;
        close_tool_chamber_if_open(&mut renderer, &mut name, &mut open).unwrap();
        assert!(name.is_none());
        assert!(!open);

        let mut name: Option<String> = None;
        let mut open = false;
        close_tool_chamber_if_open(&mut renderer, &mut name, &mut open).unwrap();
        assert!(name.is_none());
        assert!(!open);
    }

    #[test]
    fn write_outside_chamber_closes_chamber_first() {
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let mut name: Option<String> = None;
        let mut open = true;
        let mut start: Option<usize> = None;
        let mut end: Option<usize> = None;
        write_outside_chamber(
            &mut renderer,
            &mut name,
            &mut open,
            &mut start,
            &mut end,
            "hello",
            Color::White,
        )
        .unwrap();
        assert!(!open);
        assert!(name.is_none());

        let mut name: Option<String> = Some("read".to_string());
        let mut open = false;
        let mut start: Option<usize> = None;
        let mut end: Option<usize> = None;
        write_outside_chamber(
            &mut renderer,
            &mut name,
            &mut open,
            &mut start,
            &mut end,
            "hi",
            Color::White,
        )
        .unwrap();
        assert!(name.is_none());
        assert!(!open);

        let mut name: Option<String> = None;
        let mut open = false;
        let mut start: Option<usize> = None;
        let mut end: Option<usize> = None;
        write_outside_chamber(
            &mut renderer,
            &mut name,
            &mut open,
            &mut start,
            &mut end,
            "plain",
            Color::White,
        )
        .unwrap();
    }

    #[test]
    fn close_passive_drops_empty_chamber() {
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let chamber_start = renderer.buffer_len();
        renderer.write_line("", Color::White).unwrap();
        renderer
            .write_line("╭─ MOCK_TOOL ─────╮", Color::White)
            .unwrap();
        let chamber_end = renderer.buffer_len();
        assert_eq!(chamber_end - chamber_start, 2);

        let mut name: Option<String> = Some("mock".into());
        let mut open = true;
        let mut start: Option<usize> = Some(chamber_start);
        let mut end: Option<usize> = Some(chamber_end);
        close_tool_chamber_passive(&mut renderer, &mut name, &mut open, &mut start, &mut end)
            .unwrap();
        assert_eq!(renderer.buffer_len(), chamber_start);
        assert!(!open);
        assert!(name.is_none());
        assert!(start.is_none());
        assert!(end.is_none());
    }

    #[test]
    fn close_passive_with_body_writes_bottom() {
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let chamber_start = renderer.buffer_len();
        renderer.write_line("", Color::White).unwrap();
        renderer
            .write_line("╭─ MOCK_TOOL ─────╮", Color::White)
            .unwrap();
        let chamber_end = renderer.buffer_len();
        renderer.write_line("│ body row 1 │", Color::White).unwrap();
        let after_body = renderer.buffer_len();
        assert!(after_body > chamber_end);

        let mut name: Option<String> = Some("mock".into());
        let mut open = true;
        let mut start: Option<usize> = Some(chamber_start);
        let mut end: Option<usize> = Some(chamber_end);
        close_tool_chamber_passive(&mut renderer, &mut name, &mut open, &mut start, &mut end)
            .unwrap();
        assert_eq!(renderer.buffer_len(), after_body + 1);
        let body_lines = renderer.buffer_lines();
        assert!(body_lines.last().unwrap().contains('╰'));
        assert!(!open);
        assert!(name.is_none());
    }

    #[test]
    fn close_abort_paints_warning_and_bottom() {
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let initial_buffer_len = renderer.buffer_len();
        let mut name: Option<String> = None;
        let mut open = true;
        close_tool_chamber_abort(&mut renderer, &mut name, &mut open).unwrap();
        let after = renderer.buffer_len();
        assert_eq!(after - initial_buffer_len, 2);
        assert!(!open);
        assert!(name.is_none());
    }

    #[test]
    fn render_tool_output_collapses_past_max_lines() {
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let output = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let collapsed = render_tool_output(&mut renderer, "grep", "pattern", &output, 10_000, 4)
            .expect("render ok");
        let c = collapsed.expect("grep should collapse past 4 lines");
        assert_eq!(c.tool_name, "grep");
        assert_eq!(c.banner_value, "pattern");
        assert!(c.full_output.contains("line 19"));
    }

    #[test]
    fn render_tool_output_does_not_collapse_exempt_tools() {
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let output = (0..20)
            .map(|i| format!("+ added line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for tool in ["edit", "read", "question", "task", "task_status"] {
            let collapsed = render_tool_output(&mut renderer, tool, "arg", &output, 10_000, 4)
                .expect("render ok");
            assert!(
                collapsed.is_none(),
                "exempt tool `{}` must not collapse",
                tool
            );
        }
    }

    #[test]
    fn render_tool_output_apply_patch_collapses() {
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let output = (0..20)
            .map(|i| format!("op {i} applied"))
            .collect::<Vec<_>>()
            .join("\n");
        let collapsed =
            render_tool_output(&mut renderer, "apply_patch", "20 ops", &output, 10_000, 4)
                .expect("render ok");
        assert!(
            collapsed.is_some(),
            "apply_patch must collapse past max_lines"
        );
    }

    #[test]
    fn render_tool_output_stashes_on_char_truncation_alone() {
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let long_single_line = "a".repeat(50_000);
        let collapsed =
            render_tool_output(&mut renderer, "grep", "pattern", &long_single_line, 500, 4)
                .expect("render ok");
        let c = collapsed.expect("char-truncation alone must still stash for Ctrl+O");
        assert_eq!(c.full_output.len(), 50_000);
    }

    #[test]
    fn render_tool_output_empty_body_gets_placeholder() {
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        render_tool_output(&mut renderer, "glob", "**/*.nonexistent", "", 10_000, 100)
            .expect("render ok");
        let body_text: Vec<&str> = renderer.buffer_lines();
        assert!(body_text.iter().any(|l| l.contains("(no matches)")));

        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        render_tool_output(&mut renderer, "read", "empty.txt", "   \n\n  ", 10_000, 100)
            .expect("render ok");
        let body_text: Vec<&str> = renderer.buffer_lines();
        assert!(body_text.iter().any(|l| l.contains("(empty file)")));

        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        render_tool_output(&mut renderer, "weird_tool", "x", "", 10_000, 100).expect("render ok");
        let body_text: Vec<&str> = renderer.buffer_lines();
        assert!(body_text.iter().any(|l| l.contains("(no output)")));
    }

    #[test]
    fn render_tool_output_returns_none_when_no_truncation() {
        let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
        let collapsed = render_tool_output(
            &mut renderer,
            "list_dir",
            ".",
            "1 entries (1 files):\n  [file]  foo.txt",
            10_000,
            4,
        )
        .expect("render ok");
        assert!(collapsed.is_none());
    }

    #[test]
    fn banner_value_apply_patch_shows_op_count() {
        let args = serde_json::json!({"operations": [{"action": "create", "path": "/a"}]});
        assert_eq!(format_tool_banner_value("apply_patch", &args), "1 op");

        let args = serde_json::json!({
            "operations": [
                {"action": "create", "path": "/a"},
                {"action": "update", "path": "/b"},
                {"action": "delete", "path": "/c"},
            ],
        });
        assert_eq!(format_tool_banner_value("apply_patch", &args), "3 ops");

        let args = serde_json::json!({"operations": []});
        assert_eq!(format_tool_banner_value("apply_patch", &args), "");

        let args = serde_json::json!({});
        assert_eq!(format_tool_banner_value("apply_patch", &args), "");
    }

    #[test]
    fn banner_value_picks_correct_key_per_tool() {
        let args =
            serde_json::json!({"path": "/p", "command": "ls", "pattern": "*.rs", "task_id": "t1"});
        assert_eq!(format_tool_banner_value("read", &args), "/p");
        assert_eq!(format_tool_banner_value("write", &args), "/p");
        assert_eq!(format_tool_banner_value("edit", &args), "/p");
        assert_eq!(format_tool_banner_value("bash", &args), "ls");
        assert_eq!(format_tool_banner_value("grep", &args), "*.rs");
        assert_eq!(format_tool_banner_value("glob", &args), "*.rs");
        assert_eq!(format_tool_banner_value("task_status", &args), "t1");
        assert_eq!(format_tool_banner_value("mystery", &args), "");
    }

    #[test]
    fn banner_handles_pathologically_narrow_frame() {
        let header = fit_banner_header("READ", "/some/path", 12);
        assert!(header.starts_with("╭"));
        assert!(header.ends_with("╮"));
    }

    #[test]
    fn banner_truncates_pathological_long_tool_name() {
        let very_long = "MCP_TOOL:VERY_LONG_SERVER_NAME:VERY_LONG_FUNCTION_NAME";
        let header = fit_banner_header(very_long, "/some/path", 40);
        assert!(header.as_str().width() <= 40);
        assert!(header.starts_with("╭"));
        assert!(header.ends_with("╮"));
    }

    #[test]
    fn chamber_row_right_border_aligns_with_tabs() {
        let inner = 60;
        let rows = [
            chamber_row("plain text", inner),
            chamber_row("\tindented", inner),
            chamber_row("2:\t(cd ..; make library)", inner),
        ];
        let expected = inner + 4;
        for (r, w) in rows
            .iter()
            .zip(rows.iter().map(|r| UnicodeWidthStr::width(r.as_str())))
        {
            assert_eq!(
                w, expected,
                "width mismatch for {r:?}: got {w}, want {expected}"
            );
            assert!(r.ends_with('│'));
        }
    }

    #[test]
    fn chamber_row_with_bg_right_border_aligns_with_tabs() {
        let inner = 60;
        let row = chamber_row_with_bg("+\tadded line", inner, 22);
        let visible = crate::ui::wrap::visible_width(&row);
        assert_eq!(visible, inner + 4);
        assert!(row.ends_with('│'));
    }

    // dirge-hukk: a styled (ANSI-colored) chamber row pads to the same
    // visible width as a plain one — the embedded SGR codes are zero-width,
    // so the right border still aligns.
    #[test]
    fn chamber_row_styled_aligns_despite_ansi() {
        let inner = 30;
        let content = format!("\x1b[32mfn\x1b[0m main() {{}}");
        let row = chamber_row_styled(&content, inner, Color::Green);
        assert_eq!(crate::ui::wrap::visible_width(&row), inner + 4);
        assert!(row.starts_with("│ ") && row.ends_with(" │"));
    }

    // Long styled content truncates with `…`, still aligns, and never cuts
    // an SGR escape in half (every ESC is followed by its terminator).
    #[test]
    fn chamber_row_styled_truncates_without_orphaning_escapes() {
        let inner = 8;
        let content = "\x1b[32mverylongidentifier_here\x1b[0m".to_string();
        let row = chamber_row_styled(&content, inner, Color::Green);
        assert_eq!(crate::ui::wrap::visible_width(&row), inner + 4);
        assert!(row.contains('…'));
        // No ESC left dangling without an alphabetic terminator before EOL.
        let bytes: Vec<char> = row.chars().collect();
        for (i, c) in bytes.iter().enumerate() {
            if *c == '\x1b' {
                assert!(
                    bytes[i + 1..].iter().any(|d| d.is_ascii_alphabetic()),
                    "dangling ESC at {i} in {row:?}"
                );
            }
        }
    }
}
