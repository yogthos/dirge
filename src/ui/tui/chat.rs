//! Chat region widget.
//!
//! Paints chat scrollback into the `Layout::chat` rect plus the two
//! vertical │ borders at `chat_v_left_col` / `chat_v_right_col`. The
//! widget owns the verticals because they extend the full chat
//! height — making the top frame paint the corners and this widget
//! the body keeps one source of truth for each row's content.
//!
//! ANSI escape parsing in `LineEntry.text` is handled via the
//! `ansi-to-tui` crate — markdown's inline bold/italic/color
//! emphasis renders as styled Spans rather than literal escape
//! bytes. Selection rendering + mouse coordinate mapping remain
//! TODO; the legacy buffer_pos_at logic in renderer.rs is kept
//! for the eventual port.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Modifier, Style};
use ratatui::widgets::Widget;

use super::layout::Layout;
use crate::ui::renderer::{LineEntry, SelectionRange};

/// Render the chat region from a slice of `LineEntry` lines.
///
/// `scroll_offset` is the number of lines from the END of the
/// buffer to skip (0 = show newest). Matches the legacy renderer's
/// `Renderer::scroll_offset` semantics so the migration can swap
/// paint paths without changing state.
#[derive(Clone, Copy)]
pub struct ChatPane<'a> {
    layout: &'a Layout,
    lines: &'a [LineEntry],
    scroll_offset: usize,
    /// Style for the chat │ verticals.
    border_style: Style,
    /// Active selection range in normalized `(line_idx, char_offset)`
    /// coordinates. Cells inside this range have REVERSED applied on
    /// top of their underlying style.
    selection: Option<SelectionRange>,
}

impl<'a> ChatPane<'a> {
    pub fn new(layout: &'a Layout, lines: &'a [LineEntry], scroll_offset: usize) -> Self {
        Self {
            layout,
            lines,
            scroll_offset,
            border_style: Style::default().fg(RColor::Green),
            selection: None,
        }
    }

    /// Override the │ border style. Default is `Color::Green`.
    pub fn border_style(mut self, style: Style) -> Self {
        self.border_style = style;
        self
    }

    /// Highlight cells inside the given selection range with REVERSED.
    pub fn selection(mut self, sel: SelectionRange) -> Self {
        self.selection = Some(sel);
        self
    }
}

impl<'a> Widget for ChatPane<'a> {
    fn render(self, _area: Rect, buf: &mut Buffer) {
        let l = self.layout;
        let visible = l.chat.height as usize;

        // ── chat │ verticals on every row of the chat band ──
        for dy in 0..l.chat.height {
            let y = l.chat.y + dy;
            if l.chat_v_left_col < buf.area.width {
                buf[(l.chat_v_left_col, y)]
                    .set_char('│')
                    .set_style(self.border_style);
            }
            if l.chat_v_right_col < buf.area.width {
                buf[(l.chat_v_right_col, y)]
                    .set_char('│')
                    .set_style(self.border_style);
            }
        }

        // ── chat body ──
        if visible == 0 || l.chat.width == 0 || self.lines.is_empty() {
            return;
        }
        let total = self.lines.len();
        let end = total.saturating_sub(self.scroll_offset);
        let start = end.saturating_sub(visible);
        let slice = &self.lines[start..end];
        // Reserve a one-cell right margin so content doesn't sit
        // flush against the chat │ border on the right.
        let text_w = l.chat.width.saturating_sub(1);
        for (i, entry) in slice.iter().enumerate() {
            let y = l.chat.y + i as u16;
            paint_line(buf, l.chat.x, y, text_w, entry);

            // Selection overlay: line indices are absolute buffer
            // indices, so add `start` (the first visible line's
            // buffer index) to translate the on-screen row into a
            // buffer-line index for range comparison.
            if let Some(sel) = self.selection {
                let line_idx = start + i;
                apply_selection_to_row(buf, l.chat.x, y, text_w, entry, line_idx, &sel);
            }
        }
    }
}

/// Walk the visible cells on row `y` and OR `Modifier::REVERSED` onto
/// each cell that falls inside the selection char-range for this
/// buffer line. Char→display-column mapping mirrors
/// `display_col_to_char_index` in renderer.rs: count `width_cjk` per
/// char until we've consumed the selection's start/end char counts.
///
/// The function is a no-op when:
/// - line_idx is outside [sel.start.0, sel.end.0]
/// - the row's selected char range collapses to empty
/// - the chat row has zero usable width
fn apply_selection_to_row(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    entry: &LineEntry,
    line_idx: usize,
    sel: &SelectionRange,
) {
    use unicode_width::UnicodeWidthChar;

    if width == 0 {
        return;
    }
    if line_idx < sel.start.0 || line_idx > sel.end.0 {
        return;
    }
    let clean: Vec<char> = crate::ui::ansi::strip_ansi(&entry.text).chars().collect();
    let line_char_len = clean.len();

    // Per-row char range: first row clips to [start.1, end]; last row
    // clips to [0, end.1]; intermediate rows highlight the whole line.
    // A 1-row selection clips to [start.1, end.1].
    let (lo, hi) = if line_idx == sel.start.0 && line_idx == sel.end.0 {
        (sel.start.1.min(line_char_len), sel.end.1.min(line_char_len))
    } else if line_idx == sel.start.0 {
        (sel.start.1.min(line_char_len), line_char_len)
    } else if line_idx == sel.end.0 {
        (0, sel.end.1.min(line_char_len))
    } else {
        (0, line_char_len)
    };
    if lo >= hi {
        return;
    }

    // Translate the char range to a display-column range by summing
    // glyph widths up to lo, then continuing to hi.
    let col_start_off: u16 = clean[..lo]
        .iter()
        .map(|c| UnicodeWidthChar::width(*c).unwrap_or(0) as u16)
        .sum();
    let col_end_off: u16 = col_start_off
        + clean[lo..hi]
            .iter()
            .map(|c| UnicodeWidthChar::width(*c).unwrap_or(0) as u16)
            .sum::<u16>();

    let cell_x_lo = x.saturating_add(col_start_off);
    let cell_x_hi = x.saturating_add(col_end_off).min(x.saturating_add(width));
    if cell_x_lo >= cell_x_hi {
        return;
    }
    for cx in cell_x_lo..cell_x_hi {
        if cx >= buf.area.x.saturating_add(buf.area.width) {
            break;
        }
        let cell = &mut buf[(cx, y)];
        cell.modifier.insert(Modifier::REVERSED);
    }
}

/// Write `entry.text` into the chat row at `(x, y)`, clipped to
/// `width` cells, styled with the entry's color as a base. SGR
/// escape sequences embedded in the text (bold / italic / inline
/// colors emitted by markdown.rs) are parsed into ratatui Spans
/// via the `ansi-to-tui` crate so they render with the right
/// styling instead of appearing as literal `\x1b[1m...` text.
fn paint_line(buf: &mut Buffer, x: u16, y: u16, width: u16, entry: &LineEntry) {
    use ansi_to_tui::IntoText;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    if width == 0 {
        return;
    }
    let base_style = Style::default().fg(crossterm_to_ratatui(entry.color));

    // Try to parse SGR escapes. On parse error (malformed input)
    // fall back to plain set_stringn — better to show raw text
    // than to drop the line silently.
    match entry.text.as_str().into_text() {
        Ok(text) => {
            if let Some(line) = text.lines.into_iter().next() {
                // Apply base style — Spans without their own fg
                // inherit it; spans with fg keep theirs (patch is
                // a merge, not an override).
                let line = line.style(base_style);
                let area = Rect::new(x, y, width, 1);
                line.render(area, buf);
            }
        }
        Err(_) => {
            buf.set_stringn(x, y, entry.text.as_str(), width as usize, base_style);
        }
    }
}

/// Translate a crossterm color into ratatui's equivalent.
///
/// Brightness convention (ANSI 30-37 vs 90-97):
/// - crossterm: `Dark*` variants are the DIM tones (codes 31..37);
///   the unprefixed variants (e.g. `Color::Red`) are the BRIGHT
///   tones (codes 91..97). `DarkGrey` (90) is the exception.
/// - ratatui: `Red`/`Green`/… are DIM (31..37); `LightRed`/… are
///   BRIGHT (91..97). `DarkGray` (90) matches crossterm.
///
/// So the bright-named crossterm variants map to ratatui's
/// `Light*`, and the `Dark*` ones map to ratatui's unprefixed.
/// Earlier versions of this fn had the mapping inverted, which
/// made the phosphor theme render dim across the board.
pub fn crossterm_to_ratatui(c: crossterm::style::Color) -> RColor {
    use crossterm::style::Color as CC;
    match c {
        CC::Reset => RColor::Reset,
        CC::Black => RColor::Black,
        CC::DarkGrey => RColor::DarkGray,
        CC::Red => RColor::LightRed,
        CC::DarkRed => RColor::Red,
        CC::Green => RColor::LightGreen,
        CC::DarkGreen => RColor::Green,
        CC::Yellow => RColor::LightYellow,
        CC::DarkYellow => RColor::Yellow,
        CC::Blue => RColor::LightBlue,
        CC::DarkBlue => RColor::Blue,
        CC::Magenta => RColor::LightMagenta,
        CC::DarkMagenta => RColor::Magenta,
        CC::Cyan => RColor::LightCyan,
        CC::DarkCyan => RColor::Cyan,
        CC::White => RColor::White,
        CC::Grey => RColor::Gray,
        CC::Rgb { r, g, b } => RColor::Rgb(r, g, b),
        CC::AnsiValue(v) => RColor::Indexed(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::style::Color as CC;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn line(text: &str, color: CC) -> LineEntry {
        LineEntry {
            text: text.into(),
            color,
        }
    }

    /// │ borders appear on every chat row even when the buffer
    /// is empty.
    #[test]
    fn renders_borders_on_empty_buffer() {
        let layout = Layout::new(160, 30, 1);
        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        let lines: Vec<LineEntry> = vec![];
        terminal
            .draw(|f| {
                let area = f.area();
                f.render_widget(ChatPane::new(&layout, &lines, 0), area);
            })
            .unwrap();
        backend = terminal.backend().clone();

        for dy in 0..layout.chat.height {
            let y = layout.chat.y + dy;
            assert_eq!(
                backend
                    .buffer()
                    .cell((layout.chat_v_left_col, y))
                    .unwrap()
                    .symbol(),
                "│",
                "missing left │ at row {y}"
            );
            assert_eq!(
                backend
                    .buffer()
                    .cell((layout.chat_v_right_col, y))
                    .unwrap()
                    .symbol(),
                "│",
                "missing right │ at row {y}"
            );
        }
    }

    /// Lines paint into the chat rect, starting at chat.y. Text is
    /// clipped to chat.width so it cannot overwrite the right │.
    #[test]
    fn paints_buffer_lines_into_chat_rect() {
        let layout = Layout::new(160, 30, 1);
        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        let lines = vec![line("hello", CC::Green), line("world", CC::Cyan)];
        terminal
            .draw(|f| {
                let area = f.area();
                f.render_widget(ChatPane::new(&layout, &lines, 0), area);
            })
            .unwrap();
        backend = terminal.backend().clone();

        // Lines paint TOP-anchored at chat.y, chat.y + 1, ...
        // (matches the legacy renderer's render_viewport loop —
        // when total_lines < visible, content fills the top rows
        // and the bottom rows stay blank).
        let row_hello = layout.chat.y;
        let row_world = row_hello + 1;
        // Read the first 5 cells of each row.
        let read = |y: u16, w: u16| -> String {
            (0..w)
                .map(|i| {
                    backend
                        .buffer()
                        .cell((layout.chat.x + i, y))
                        .unwrap()
                        .symbol()
                        .to_string()
                })
                .collect()
        };
        assert_eq!(read(row_hello, 5), "hello");
        assert_eq!(read(row_world, 5), "world");
    }

    /// Long text is clipped at chat.width and never touches the
    /// right │ column.
    #[test]
    fn long_line_clips_at_chat_width() {
        let layout = Layout::new(40, 10, 1);
        // chat.width = 38 (narrow terminal, full chat band).
        let mut backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        let long = "x".repeat(200);
        let lines = vec![line(&long, CC::Green)];
        terminal
            .draw(|f| {
                let area = f.area();
                f.render_widget(ChatPane::new(&layout, &lines, 0), area);
            })
            .unwrap();
        backend = terminal.backend().clone();

        // Single line lands on the top row of the chat band.
        let row = layout.chat.y;
        // Content fills cols [chat.x, chat.x + chat.width - 1).
        // The last cell is reserved as a 1-cell right margin so
        // content doesn't run into the │ border.
        let text_w = layout.chat.width - 1;
        for i in 0..text_w {
            assert_eq!(
                backend
                    .buffer()
                    .cell((layout.chat.x + i, row))
                    .unwrap()
                    .symbol(),
                "x",
                "expected 'x' at col {} (chat content)",
                layout.chat.x + i
            );
        }
        // The reserved margin cell (chat.x + text_w) is blank.
        assert_eq!(
            backend
                .buffer()
                .cell((layout.chat.x + text_w, row))
                .unwrap()
                .symbol(),
            " ",
            "expected the 1-cell right margin to be blank"
        );
        // Right │ must NOT be overwritten.
        assert_eq!(
            backend
                .buffer()
                .cell((layout.chat_v_right_col, row))
                .unwrap()
                .symbol(),
            "│"
        );
    }

    /// scroll_offset shifts which lines are visible.
    #[test]
    fn scroll_offset_windows_older_lines() {
        let layout = Layout::new(160, 30, 1); // chat.height = 24
        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        // 30 lines named "L0".."L29"; with scroll_offset = 5 the
        // window is lines[30-5-24 .. 30-5] = lines[1..25]. Painted
        // top-anchored: row chat.y → L1, row chat.y + 23 → L24.
        let lines: Vec<LineEntry> = (0..30).map(|i| line(&format!("L{i}"), CC::Green)).collect();
        terminal
            .draw(|f| {
                let area = f.area();
                f.render_widget(ChatPane::new(&layout, &lines, 5), area);
            })
            .unwrap();
        backend = terminal.backend().clone();

        let row_top = layout.chat.y;
        let row_bot = layout.chat.y + layout.chat.height - 1;
        let read = |y: u16, w: u16| -> String {
            (0..w)
                .map(|i| {
                    backend
                        .buffer()
                        .cell((layout.chat.x + i, y))
                        .unwrap()
                        .symbol()
                        .to_string()
                })
                .collect()
        };
        assert_eq!(read(row_top, 3), "L1 ", "top visible row should be L1");
        assert_eq!(read(row_bot, 3), "L24", "bottom visible row should be L24");
    }

    /// Lines containing SGR escapes (markdown's bold/italic/inline
    /// colors) render as styled spans — not as raw `\x1b[1m...` chars.
    #[test]
    fn ansi_escapes_render_as_styled_spans() {
        let layout = Layout::new(160, 30, 1);
        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        // markdown.rs emits this shape for inline emphasis.
        let lines = vec![LineEntry {
            text: "hello \x1b[1mbold\x1b[22m world".into(),
            color: CC::White,
        }];
        terminal
            .draw(|f| {
                let area = f.area();
                f.render_widget(ChatPane::new(&layout, &lines, 0), area);
            })
            .unwrap();
        backend = terminal.backend().clone();

        // First chat row: read 17 chars starting at chat.x. Expect
        // "hello bold world" — escape bytes stripped, NO raw \x1b
        // appearing.
        let row: String = (0..17)
            .map(|i| {
                backend
                    .buffer()
                    .cell((layout.chat.x + i, layout.chat.y))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        assert_eq!(row, "hello bold world ", "got {:?}", row);
        // No literal escape chars survived.
        assert!(!row.contains('\x1b'), "raw ESC bytes leaked into buffer");
    }

    /// crossterm → ratatui color translation preserves brightness.
    /// crossterm's `Color::Green` is ANSI 92 (bright); ratatui's
    /// equivalent is `LightGreen`. crossterm's `DarkGreen` is ANSI
    /// 32 (dim); ratatui's equivalent is `Green`.
    #[test]
    fn color_translation_preserves_brightness() {
        // Bright crossterm → Light* in ratatui.
        assert_eq!(crossterm_to_ratatui(CC::Green), RColor::LightGreen);
        assert_eq!(crossterm_to_ratatui(CC::Red), RColor::LightRed);
        assert_eq!(crossterm_to_ratatui(CC::Yellow), RColor::LightYellow);
        assert_eq!(crossterm_to_ratatui(CC::Magenta), RColor::LightMagenta);
        assert_eq!(crossterm_to_ratatui(CC::Cyan), RColor::LightCyan);
        // Dim Dark* crossterm → unprefixed in ratatui.
        assert_eq!(crossterm_to_ratatui(CC::DarkGreen), RColor::Green);
        assert_eq!(crossterm_to_ratatui(CC::DarkRed), RColor::Red);
        assert_eq!(crossterm_to_ratatui(CC::DarkMagenta), RColor::Magenta);
        // DarkGrey (90) is the only "Dark*" that maps to DarkGray.
        assert_eq!(crossterm_to_ratatui(CC::DarkGrey), RColor::DarkGray);
        assert_eq!(crossterm_to_ratatui(CC::Grey), RColor::Gray);
        // RGB + indexed passthrough.
        assert_eq!(
            crossterm_to_ratatui(CC::Rgb { r: 1, g: 2, b: 3 }),
            RColor::Rgb(1, 2, 3)
        );
        assert_eq!(crossterm_to_ratatui(CC::AnsiValue(42)), RColor::Indexed(42));
    }

    /// Selection range painted with REVERSED modifier on the right
    /// cells, not on others. Single line, char-range maps to
    /// display-col range (ASCII so 1:1).
    #[test]
    fn selection_paints_reversed_on_selected_cells() {
        use crate::ui::renderer::SelectionRange;
        let layout = Layout::new(160, 30, 1);
        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        let lines = vec![line("hello world", CC::Green)];
        // Select "world" — chars 6..11 on line 0.
        let sel = SelectionRange {
            start: (0, 6),
            end: (0, 11),
        };
        terminal
            .draw(|f| {
                let area = f.area();
                f.render_widget(ChatPane::new(&layout, &lines, 0).selection(sel), area);
            })
            .unwrap();
        backend = terminal.backend().clone();

        let row = layout.chat.y;
        // chars 0..6 ("hello ") — not selected → not REVERSED.
        for i in 0..6 {
            let cell = backend.buffer().cell((layout.chat.x + i, row)).unwrap();
            assert!(
                !cell.modifier.contains(Modifier::REVERSED),
                "col {} should not be REVERSED",
                layout.chat.x + i
            );
        }
        // chars 6..11 ("world") — selected → REVERSED.
        for i in 6..11 {
            let cell = backend.buffer().cell((layout.chat.x + i, row)).unwrap();
            assert!(
                cell.modifier.contains(Modifier::REVERSED),
                "col {} should be REVERSED",
                layout.chat.x + i
            );
        }
        // Past the selection — not REVERSED.
        let cell = backend.buffer().cell((layout.chat.x + 11, row)).unwrap();
        assert!(!cell.modifier.contains(Modifier::REVERSED));
    }
}
