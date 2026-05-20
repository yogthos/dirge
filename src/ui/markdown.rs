use compact_str::CompactString;
use crossterm::style::Color;
use pulldown_cmark::{Event, Tag, TagEnd};

use super::renderer::LineEntry;

fn word_wrap(text: &str, max_width: usize) -> Vec<CompactString> {
    if text.is_empty() {
        return vec![CompactString::new("")];
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_width {
        return vec![CompactString::from(text)];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + max_width).min(chars.len());
        if end < chars.len() {
            let mut break_at = end;
            for i in (start..end).rev() {
                if chars[i] == ' ' {
                    break_at = i + 1;
                    break;
                }
            }
            if break_at == start {
                break_at = end;
            }
            lines.push(CompactString::from(
                chars[start..break_at].iter().collect::<String>(),
            ));
            start = break_at;
        } else {
            lines.push(CompactString::from(
                chars[start..].iter().collect::<String>(),
            ));
            break;
        }
    }
    lines
}

fn flush_acc(acc: &str, color: Color, max_width: usize, out: &mut Vec<LineEntry>) {
    if acc.is_empty() {
        return;
    }
    for line in acc.split('\n') {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            out.push(LineEntry {
                text: CompactString::new(""),
                color,
            });
        } else {
            for chunk in word_wrap(trimmed, max_width) {
                out.push(LineEntry { text: chunk, color });
            }
        }
    }
}

fn bullet_prefix(in_blockquote: bool) -> &'static str {
    if in_blockquote { "  ┊ " } else { "  • " }
}

/// Render a markdown table as `| col | col |` rows with a separator
/// line below the header. Columns are padded so the right borders
/// align. Caps each cell's display at the available width so a
/// long cell doesn't break alignment. No-ops when both header and
/// rows are empty.
fn render_table(
    header: &[String],
    rows: &[Vec<String>],
    max_width: usize,
    out: &mut Vec<LineEntry>,
) {
    if header.is_empty() && rows.is_empty() {
        return;
    }
    // Compute per-column max char width.
    let ncols = header
        .len()
        .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if ncols == 0 {
        return;
    }
    let mut widths = vec![0usize; ncols];
    for (i, cell) in header.iter().enumerate() {
        widths[i] = widths[i].max(cell.chars().count());
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    // Cap any single column to avoid one runaway cell blowing the
    // line width. Distribute available width: target inner width =
    // max_width - 4 (for outer `| ` + ` |`), minus 3*(ncols-1) for
    // ` | ` separators. Cells get clipped to fit.
    let inner = max_width.saturating_sub(2 * 2);
    let sep_overhead = if ncols > 1 { 3 * (ncols - 1) } else { 0 };
    let cell_budget = inner.saturating_sub(sep_overhead);
    let per_col = if ncols > 0 { cell_budget / ncols } else { 0 };
    for w in widths.iter_mut() {
        if per_col > 0 && *w > per_col {
            *w = per_col;
        }
    }

    let fit = |cell: &str, w: usize| -> String {
        let chars: Vec<char> = cell.chars().collect();
        if chars.len() <= w {
            let mut s: String = chars.iter().collect();
            for _ in chars.len()..w {
                s.push(' ');
            }
            s
        } else if w <= 1 {
            chars.iter().take(w).collect()
        } else {
            let mut s: String = chars.iter().take(w - 1).collect();
            s.push('…');
            s
        }
    };

    let render_row = |row: &[String], widths: &[usize]| -> String {
        let mut s = String::with_capacity(max_width);
        s.push_str("│ ");
        for i in 0..widths.len() {
            if i > 0 {
                s.push_str(" │ ");
            }
            let cell = row.get(i).map(String::as_str).unwrap_or("");
            s.push_str(&fit(cell, widths[i]));
        }
        s.push_str(" │");
        s
    };

    let sep = {
        let mut s = String::with_capacity(max_width);
        s.push('├');
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                s.push('┼');
            }
            for _ in 0..(w + 2) {
                s.push('─');
            }
        }
        s.push('┤');
        s
    };

    if !header.is_empty() {
        out.push(LineEntry {
            text: CompactString::new(&render_row(header, &widths)),
            color: crate::ui::theme::header(),
        });
        out.push(LineEntry {
            text: CompactString::new(&sep),
            color: crate::ui::theme::dim(),
        });
    }
    for row in rows {
        out.push(LineEntry {
            text: CompactString::new(&render_row(row, &widths)),
            color: crate::ui::theme::agent(),
        });
    }
    out.push(LineEntry {
        text: CompactString::new(""),
        color: crate::ui::theme::agent(),
    });
}

pub fn markdown_to_styled(text: &str, max_width: usize) -> Vec<LineEntry> {
    if text.is_empty() {
        return Vec::new();
    }

    // Enable GFM tables so `Tag::Table*` events actually fire.
    // Without this, table syntax falls back to plain paragraphs and
    // the table never reaches `render_table`.
    let mut opts = pulldown_cmark::Options::empty();
    opts.insert(pulldown_cmark::Options::ENABLE_TABLES);
    let parser = pulldown_cmark::Parser::new_ext(text, opts);
    let mut result = Vec::new();
    let mut acc = String::new();

    let mut in_heading = false;
    let mut in_code_block = false;
    let mut in_blockquote = false;
    let mut ordered_list = false;
    let mut list_item_count: u64 = 0;
    // Table accumulation: pulldown_cmark emits TableHead → (Row × N
    // cells) for the header row, then more TableRow blocks for body.
    // We collect cells into `current_cell`, rows into `current_row`,
    // and the whole table into `table_header` + `table_rows`, then
    // render with column-aligned padding when the table ends.
    let mut in_table = false;
    let mut in_table_head = false;
    let mut current_cell = String::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut table_header: Vec<String> = Vec::new();
    let mut table_rows: Vec<Vec<String>> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level: _, .. } => {
                    flush_acc(&acc, crate::ui::theme::agent(), max_width, &mut result);
                    acc.clear();
                    in_heading = true;
                }
                Tag::CodeBlock(_kind) => {
                    flush_acc(&acc, crate::ui::theme::agent(), max_width, &mut result);
                    acc.clear();
                    in_code_block = true;
                }
                Tag::BlockQuote(_) => {
                    flush_acc(&acc, crate::ui::theme::agent(), max_width, &mut result);
                    acc.clear();
                    in_blockquote = true;
                }
                Tag::List(t) => {
                    ordered_list = t.is_some();
                    list_item_count = 0;
                }
                Tag::Item => {
                    flush_acc(&acc, crate::ui::theme::agent(), max_width, &mut result);
                    acc.clear();
                    list_item_count += 1;
                }
                Tag::FootnoteDefinition(_) => {}
                Tag::Table(_) => {
                    flush_acc(&acc, crate::ui::theme::agent(), max_width, &mut result);
                    acc.clear();
                    in_table = true;
                    table_header.clear();
                    table_rows.clear();
                }
                Tag::TableHead => {
                    in_table_head = true;
                    current_row.clear();
                }
                Tag::TableRow => {
                    current_row.clear();
                }
                Tag::TableCell => {
                    current_cell.clear();
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    let color = if in_blockquote {
                        crate::ui::theme::dim()
                    } else {
                        crate::ui::theme::agent()
                    };
                    flush_acc(&acc, color, max_width, &mut result);
                    acc.clear();
                }
                TagEnd::Heading(_) => {
                    flush_acc(&acc, crate::ui::theme::header(), max_width, &mut result);
                    acc.clear();
                    in_heading = false;
                    result.push(LineEntry {
                        text: CompactString::new(""),
                        color: crate::ui::theme::agent(),
                    });
                }
                TagEnd::CodeBlock => {
                    for line in acc.split('\n') {
                        let trimmed = line.trim_end_matches('\r');
                        if trimmed.is_empty() {
                            result.push(LineEntry {
                                text: CompactString::new(""),
                                color: crate::ui::theme::tool(),
                            });
                        } else {
                            result.push(LineEntry {
                                text: CompactString::from(trimmed),
                                color: crate::ui::theme::tool(),
                            });
                        }
                    }
                    acc.clear();
                    in_code_block = false;
                    result.push(LineEntry {
                        text: CompactString::new(""),
                        color: crate::ui::theme::agent(),
                    });
                }
                TagEnd::BlockQuote(_) => {
                    let mut quoted = Vec::new();
                    for line in acc.split('\n') {
                        let trimmed = line.trim_end_matches('\r');
                        if trimmed.is_empty() {
                            quoted.push(LineEntry {
                                text: CompactString::new(""),
                                color: crate::ui::theme::dim(),
                            });
                        } else {
                            let prefixed = format!("│ {}", trimmed);
                            for chunk in word_wrap(&prefixed, max_width) {
                                quoted.push(LineEntry {
                                    text: chunk,
                                    color: crate::ui::theme::dim(),
                                });
                            }
                        }
                    }
                    result.extend(quoted);
                    acc.clear();
                    in_blockquote = false;
                    result.push(LineEntry {
                        text: CompactString::new(""),
                        color: crate::ui::theme::agent(),
                    });
                }
                TagEnd::Item => {
                    let color = if in_blockquote {
                        crate::ui::theme::dim()
                    } else {
                        crate::ui::theme::agent()
                    };
                    let bullet = if ordered_list {
                        format!(" {}. ", list_item_count)
                    } else {
                        bullet_prefix(in_blockquote).to_string()
                    };
                    let mut item_lines = Vec::new();
                    let mut first = true;
                    for line in acc.split('\n') {
                        let trimmed = line.trim_end_matches('\r');
                        if trimmed.is_empty() {
                            item_lines.push(LineEntry {
                                text: CompactString::new(""),
                                color,
                            });
                        } else if first {
                            let prefixed = format!("{}{}", bullet, trimmed);
                            for chunk in word_wrap(&prefixed, max_width) {
                                item_lines.push(LineEntry { text: chunk, color });
                            }
                            first = false;
                        } else {
                            for chunk in word_wrap(trimmed, max_width) {
                                item_lines.push(LineEntry { text: chunk, color });
                            }
                        }
                    }
                    result.extend(item_lines);
                    acc.clear();
                }
                TagEnd::List(_) => {
                    ordered_list = false;
                    list_item_count = 0;
                    result.push(LineEntry {
                        text: CompactString::new(""),
                        color: crate::ui::theme::agent(),
                    });
                }
                TagEnd::FootnoteDefinition => {}
                TagEnd::Table => {
                    render_table(&table_header, &table_rows, max_width, &mut result);
                    in_table = false;
                }
                TagEnd::TableHead => {
                    table_header = std::mem::take(&mut current_row);
                    in_table_head = false;
                }
                TagEnd::TableRow => {
                    if !in_table_head {
                        table_rows.push(std::mem::take(&mut current_row));
                    }
                }
                TagEnd::TableCell => {
                    current_row.push(std::mem::take(&mut current_cell));
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_table {
                    current_cell.push_str(&t);
                } else if in_code_block {
                    acc.push_str(&t);
                } else {
                    acc.push_str(&t);
                }
            }
            Event::Code(t) => {
                if in_table {
                    current_cell.push_str(&t);
                } else if in_code_block {
                    acc.push_str(&t);
                } else {
                    acc.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_code_block {
                    acc.push('\n');
                } else {
                    acc.push('\n');
                }
            }
            Event::Rule => {
                flush_acc(&acc, crate::ui::theme::agent(), max_width, &mut result);
                acc.clear();
                let rule: String = std::iter::repeat('─').take(max_width.min(40)).collect();
                result.push(LineEntry {
                    text: CompactString::from(rule),
                    color: crate::ui::theme::dim(),
                });
                result.push(LineEntry {
                    text: CompactString::new(""),
                    color: crate::ui::theme::agent(),
                });
            }
            Event::Html(t) => {
                acc.push_str(&t);
            }
            Event::InlineHtml(t) => {
                acc.push_str(&t);
            }
            Event::FootnoteReference(t) => {
                acc.push_str(&t);
            }
            Event::TaskListMarker(checked) => {
                if checked {
                    acc.push_str("[x]");
                } else {
                    acc.push_str("[ ]");
                }
            }
            _ => {}
        }
    }

    if !acc.is_empty() {
        let color = if in_blockquote {
            crate::ui::theme::dim()
        } else if in_code_block {
            crate::ui::theme::tool()
        } else if in_heading {
            crate::ui::theme::header()
        } else {
            crate::ui::theme::agent()
        };
        flush_acc(&acc, color, max_width, &mut result);
    }

    result
}
