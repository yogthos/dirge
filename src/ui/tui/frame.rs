//! Top + chat-bottom frame widgets.
//!
//! The unified top frame paints
//! `───[AGENT STATUS]───╭───[AGENT LOG STREAM]───╮───[SYSTEM]───`
//! across the full terminal width. The chat-bottom frame paints
//! `╰───╯` inside the chat band only — side panel rows below the
//! frame are left blank so the bottom strip doesn't collide with
//! stray ─── horizontals.
//!
//! The chat's vertical │ borders at `chat_v_left_col` and
//! `chat_v_right_col` are painted separately by the chat region
//! widget (see beads dirge-a0q) — this module owns the horizontals
//! only.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

use super::layout::Layout;

/// Title text shown in the left panel section of the top frame.
pub const LEFT_TITLE: &str = "[AGENT STATUS]";
/// Title text shown in the chat section of the top frame.
pub const CHAT_TITLE: &str = "[AGENT LOG STREAM]";
/// Title text shown in the right panel section of the top frame.
pub const RIGHT_TITLE: &str = "[SYSTEM]";

/// Top frame widget — paints row 0 across the full width.
///
/// Glyph layout:
///
/// ```text
/// ───[AGENT STATUS]───╭───[AGENT LOG STREAM]───╮───[SYSTEM]───
/// └── left_panel ───┘└─ chat outer (incl. │) ─┘└─ right_panel ┘
/// ```
///
/// Each section's title is centered within its segment via
/// `─` padding. When a section is narrower than its title (e.g.
/// very narrow terminal) the title is dropped and the section
/// fills with bare `─`.
#[derive(Clone, Copy, Debug)]
pub struct TopFrame<'a> {
    layout: &'a Layout,
    style: Style,
}

impl<'a> TopFrame<'a> {
    pub fn new(layout: &'a Layout) -> Self {
        Self {
            layout,
            style: Style::default().fg(Color::Green),
        }
    }

    /// Override the foreground color (default is `Color::Green`,
    /// matching the phosphor theme's header tone).
    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

impl<'a> Widget for TopFrame<'a> {
    fn render(self, _area: Rect, buf: &mut Buffer) {
        let l = self.layout;
        if l.top_frame.height == 0 || l.top_frame.width == 0 {
            return;
        }
        let y = l.top_frame.y;

        // Left section: cols [0, chat_v_left_col).
        paint_titled_horizontal(buf, 0, y, l.chat_v_left_col, LEFT_TITLE, '─', self.style);
        // Chat corner ╭.
        if l.chat_v_left_col < l.top_frame.width {
            buf[(l.chat_v_left_col, y)]
                .set_char('╭')
                .set_style(self.style);
        }
        // Chat section: cols (chat_v_left_col, chat_v_right_col).
        let chat_inner_w = l
            .chat_v_right_col
            .saturating_sub(l.chat_v_left_col)
            .saturating_sub(1);
        paint_titled_horizontal(
            buf,
            l.chat_v_left_col.saturating_add(1),
            y,
            chat_inner_w,
            CHAT_TITLE,
            '─',
            self.style,
        );
        // Chat corner ╮.
        if l.chat_v_right_col < l.top_frame.width {
            buf[(l.chat_v_right_col, y)]
                .set_char('╮')
                .set_style(self.style);
        }
        // Right section: cols (chat_v_right_col, cols).
        // All three top-frame titles share the same style so the
        // header reads as one coherent strip. (Earlier the [SYSTEM]
        // title was amber to match the body content inside that
        // pane, but visual inconsistency between sibling titles
        // outweighed the section-coloring win.)
        let right_start = l.chat_v_right_col.saturating_add(1);
        let right_w = l.top_frame.width.saturating_sub(right_start);
        paint_titled_horizontal(buf, right_start, y, right_w, RIGHT_TITLE, '─', self.style);
    }
}

/// Bottom frame widget — paints `╰───╯` inside the chat band on the
/// chat_bot_frame row. Side panel cols on that row are left
/// untouched (callers are responsible for clearing them via a
/// `Clear` widget or by writing blanks).
#[derive(Clone, Copy, Debug)]
pub struct ChatBotFrame<'a> {
    layout: &'a Layout,
    style: Style,
}

impl<'a> ChatBotFrame<'a> {
    pub fn new(layout: &'a Layout) -> Self {
        Self {
            layout,
            style: Style::default().fg(Color::Green),
        }
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

impl<'a> Widget for ChatBotFrame<'a> {
    fn render(self, _area: Rect, buf: &mut Buffer) {
        let l = self.layout;
        if l.chat_bot_frame.height == 0 {
            return;
        }
        let y = l.chat_bot_frame.y;
        // Left ╰, inner ───, right ╯. Only inside the chat band.
        if l.chat_v_left_col < l.chat_bot_frame.width {
            buf[(l.chat_v_left_col, y)]
                .set_char('╰')
                .set_style(self.style);
        }
        let inner_w = l
            .chat_v_right_col
            .saturating_sub(l.chat_v_left_col)
            .saturating_sub(1);
        for i in 0..inner_w {
            let x = l.chat_v_left_col.saturating_add(1).saturating_add(i);
            if x >= l.chat_bot_frame.width {
                break;
            }
            buf[(x, y)].set_char('─').set_style(self.style);
        }
        if l.chat_v_right_col < l.chat_bot_frame.width {
            buf[(l.chat_v_right_col, y)]
                .set_char('╯')
                .set_style(self.style);
        }
    }
}

/// Paint `───[title]───` of exactly `width` cells starting at
/// `(x, y)`, centered. When `width < title.chars().count()` the
/// title is dropped (all `─`).
fn paint_titled_horizontal(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    title: &str,
    fill: char,
    style: Style,
) {
    if width == 0 {
        return;
    }
    let tw = title.chars().count() as u16;
    if tw >= width {
        for i in 0..width {
            buf[(x + i, y)].set_char(fill).set_style(style);
        }
        return;
    }
    let pad = width - tw;
    let left = pad / 2;
    // Left fill.
    for i in 0..left {
        buf[(x + i, y)].set_char(fill).set_style(style);
    }
    // Title.
    for (i, ch) in title.chars().enumerate() {
        buf[(x + left + i as u16, y)].set_char(ch).set_style(style);
    }
    // Right fill.
    let right_start = left + tw;
    for i in right_start..width {
        buf[(x + i, y)].set_char(fill).set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Render the top frame on a wide terminal and assert the exact
    /// content of row 0. Verifies section widths, corner placement,
    /// and title centering.
    #[test]
    fn top_frame_wide_terminal_layout() {
        let layout = Layout::new(60, 10, 1);
        // cols=60, line_w=58, chat=min(58,120)=58 → no gutter, side
        // panels collapse to 0. So this test reflects the
        // narrow-side-panel branch; use a wider terminal for the
        // gutter case.
        let _ = layout;

        let layout = Layout::new(160, 30, 1);
        // line_w=158, chat=120, gutter=(158-120)/2=19.
        // Left section width = 19, right section width = 19.
        // Chat corner ╭ at col 19, ╮ at col 19+121=140.
        assert_eq!(layout.chat_v_left_col, 19);
        assert_eq!(layout.chat_v_right_col, 140);

        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                f.render_widget(TopFrame::new(&layout), area);
            })
            .unwrap();
        backend = terminal.backend().clone();

        // Read row 0 from the buffer.
        let row0: String = (0..160)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, 0))
                    .unwrap()
                    .symbol()
                    .chars()
                    .next()
                    .unwrap()
            })
            .collect();

        // Expected:
        // cols [0,18]   → ───[AGENT STATUS]─── centered in 19 cells
        //   pad=19-14=5, left=2, right=3, so: ──[AGENT STATUS]───
        // col 19        → ╭
        // cols [20,139] → ───[AGENT LOG STREAM]─── in 120 cells
        //   pad=120-18=102, left=51, right=51 ─*51 + title + ─*51
        // col 140       → ╮
        // cols [141,159] → ───[SYSTEM]─── in 19 cells
        //   pad=19-8=11, left=5, right=6
        let expected_left = format!("{}{}{}", "─".repeat(2), LEFT_TITLE, "─".repeat(3));
        let expected_chat = format!("{}{}{}", "─".repeat(51), CHAT_TITLE, "─".repeat(51));
        let expected_right = format!("{}{}{}", "─".repeat(5), RIGHT_TITLE, "─".repeat(6));
        let expected = format!("{}╭{}╮{}", expected_left, expected_chat, expected_right);
        assert_eq!(row0, expected, "top frame row 0 mismatch");
    }

    /// Narrow terminal: side panels collapse, top frame is just
    /// ╭───[AGENT LOG STREAM]───╮ filling cols [0, cols-1].
    #[test]
    fn top_frame_narrow_terminal() {
        let layout = Layout::new(40, 10, 1);
        // line_w=38 <= 120 → chat=38, gutter=0, side panels empty.
        assert_eq!(layout.chat_v_left_col, 0);
        assert_eq!(layout.chat_v_right_col, 39);

        let mut backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                f.render_widget(TopFrame::new(&layout), area);
            })
            .unwrap();
        backend = terminal.backend().clone();

        let row0: String = (0..40)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, 0))
                    .unwrap()
                    .symbol()
                    .chars()
                    .next()
                    .unwrap()
            })
            .collect();
        // Chat inner width = 38, title = 18 chars, pad = 20, left = 10.
        let expected = format!("╭{}{}{}╮", "─".repeat(10), CHAT_TITLE, "─".repeat(10));
        assert_eq!(row0, expected);
    }

    /// Bottom chat frame: ╰───╯ within chat band, blanks (or buffer
    /// default — i.e. space) in side regions.
    #[test]
    fn chat_bot_frame_only_paints_chat_band() {
        let layout = Layout::new(160, 30, 1);
        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                f.render_widget(ChatBotFrame::new(&layout), area);
            })
            .unwrap();
        backend = terminal.backend().clone();

        let row = layout.chat_bot_frame.y;

        // Side regions (cols [0, 18] and [141, 159]) must remain at
        // the buffer's default (space). The widget MUST NOT touch
        // those cols.
        for x in 0..layout.chat_v_left_col {
            let c = backend
                .buffer()
                .cell((x, row))
                .unwrap()
                .symbol()
                .chars()
                .next()
                .unwrap();
            assert_eq!(c, ' ', "left side at col {x} should be blank, got {c:?}");
        }
        for x in (layout.chat_v_right_col + 1)..160 {
            let c = backend
                .buffer()
                .cell((x, row))
                .unwrap()
                .symbol()
                .chars()
                .next()
                .unwrap();
            assert_eq!(c, ' ', "right side at col {x} should be blank, got {c:?}");
        }
        // ╰ at chat_v_left_col, ─ inside, ╯ at chat_v_right_col.
        assert_eq!(
            backend
                .buffer()
                .cell((layout.chat_v_left_col, row))
                .unwrap()
                .symbol(),
            "╰"
        );
        assert_eq!(
            backend
                .buffer()
                .cell((layout.chat_v_right_col, row))
                .unwrap()
                .symbol(),
            "╯"
        );
        for x in (layout.chat_v_left_col + 1)..layout.chat_v_right_col {
            assert_eq!(
                backend.buffer().cell((x, row)).unwrap().symbol(),
                "─",
                "expected ─ at col {x}"
            );
        }
    }

    /// Titles drop cleanly when section is too narrow to fit them.
    #[test]
    fn titled_horizontal_drops_title_when_too_narrow() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 5, 1));
        paint_titled_horizontal(&mut buf, 0, 0, 5, "[AGENT STATUS]", '─', Style::default());
        let row: String = (0..5)
            .map(|x| buf.cell((x, 0)).unwrap().symbol().chars().next().unwrap())
            .collect();
        assert_eq!(row, "─────");
    }
}
