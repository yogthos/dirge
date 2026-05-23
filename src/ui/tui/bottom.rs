//! Bottom strip widget — avatar box, input box (or overlay), and
//! the status row.
//!
//! The strip occupies three rects from `Layout`: `avatar_box` (the
//! face on the bottom-left), `input_box` (between the chat ║
//! columns), and `right_margin` (mirror gutter, always blank). The
//! `status` rect at the very bottom is a single row holding the
//! BBS-style status chips.
//!
//! When an overlay is active (permission alert / questionnaire) the
//! input editor is REPLACED inside the same frame — the alert's
//! title goes in the top border and the body lines fill the inner
//! band. No second box appears anywhere.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Style};
use ratatui::widgets::Widget;

use super::layout::Layout;

/// What the input frame currently shows. `Copy` so it can be
/// captured in a `Scene` without explicit cloning.
#[derive(Clone, Copy)]
pub enum BottomBody<'a> {
    /// User input editor. `cursor_col` is the cursor's offset from
    /// the start of the inner band (after the prompt prefix) — the
    /// widget paints the cursor at `input_box.x + cursor_col`.
    Editor {
        text: &'a str,
        cursor_col: u16,
        is_running: bool,
    },
    /// Modal overlay replacing the editor. `title` shows in the
    /// frame's top border (e.g. `[ALERT]`); `lines` paint inside
    /// the inner band, one per row. Colors are crossterm-flavored
    /// (matches the rest of the renderer state) — converted to
    /// ratatui at paint time.
    Overlay {
        title: &'a str,
        lines: &'a [(String, crossterm::style::Color)],
    },
}

/// Avatar face spec — a single char and its color. The legacy
/// `( -_- )` art is multi-char; for ratatui we accept any short
/// string and clip to fit the avatar box's inner width.
pub struct AvatarSpec<'a> {
    pub face: &'a str,
    pub color: RColor,
}

/// Top-level bottom strip composer. Use the builder methods to
/// supply the avatar, body, and status text, then render against
/// the full terminal area (the widget reads rects from `layout`).
pub struct BottomStrip<'a> {
    layout: &'a Layout,
    avatar: Option<AvatarSpec<'a>>,
    body: Option<BottomBody<'a>>,
    status: &'a str,
    border_style: Style,
}

impl<'a> BottomStrip<'a> {
    pub fn new(layout: &'a Layout) -> Self {
        Self {
            layout,
            avatar: None,
            body: None,
            status: "",
            border_style: Style::default().fg(RColor::Green),
        }
    }

    pub fn avatar(mut self, spec: AvatarSpec<'a>) -> Self {
        self.avatar = Some(spec);
        self
    }
    pub fn body(mut self, body: BottomBody<'a>) -> Self {
        self.body = Some(body);
        self
    }
    pub fn status(mut self, status: &'a str) -> Self {
        self.status = status;
        self
    }
    pub fn border_style(mut self, style: Style) -> Self {
        self.border_style = style;
        self
    }
}

impl<'a> Widget for BottomStrip<'a> {
    fn render(self, _area: Rect, buf: &mut Buffer) {
        let l = self.layout;
        paint_avatar_box(buf, l.avatar_box, self.avatar.as_ref(), self.border_style);
        match self.body.as_ref() {
            Some(BottomBody::Editor {
                text,
                cursor_col,
                is_running,
            }) => paint_editor_box(
                buf,
                l.input_box,
                text,
                *cursor_col,
                *is_running,
                self.border_style,
            ),
            Some(BottomBody::Overlay { title, lines }) => paint_overlay_box(
                buf,
                l.input_box,
                title,
                lines,
                self.border_style,
            ),
            None => paint_empty_box(buf, l.input_box, self.border_style),
        }
        // right_margin: blank — explicitly clear so stale right
        // panel pixels can't ghost in when the strip grows.
        for dy in 0..l.right_margin.height {
            for dx in 0..l.right_margin.width {
                let x = l.right_margin.x + dx;
                let y = l.right_margin.y + dy;
                buf[(x, y)].set_char(' ');
            }
        }
        paint_status(buf, l.status, self.status);
    }
}

fn paint_avatar_box(buf: &mut Buffer, area: Rect, spec: Option<&AvatarSpec>, style: Style) {
    if area.width < 4 || area.height < 2 {
        return;
    }
    let inner_w = area.width as usize - 2;

    // Top border.
    buf[(area.x, area.y)].set_char('╭').set_style(style);
    for i in 0..inner_w {
        buf[(area.x + 1 + i as u16, area.y)]
            .set_char('─')
            .set_style(style);
    }
    buf[(area.x + area.width - 1, area.y)]
        .set_char('╮')
        .set_style(style);

    // Sides + blank interior.
    for dy in 1..(area.height - 1) {
        let y = area.y + dy;
        buf[(area.x, y)].set_char('│').set_style(style);
        for dx in 1..(area.width - 1) {
            buf[(area.x + dx, y)].set_char(' ');
        }
        buf[(area.x + area.width - 1, y)]
            .set_char('│')
            .set_style(style);
    }

    // Bottom border.
    let by = area.y + area.height - 1;
    buf[(area.x, by)].set_char('╰').set_style(style);
    for i in 0..inner_w {
        buf[(area.x + 1 + i as u16, by)]
            .set_char('─')
            .set_style(style);
    }
    buf[(area.x + area.width - 1, by)]
        .set_char('╯')
        .set_style(style);

    // Centered face.
    if let Some(spec) = spec {
        let face_w = spec.face.chars().count();
        if face_w <= inner_w {
            let content_h = area.height as usize - 2;
            if content_h == 0 {
                return;
            }
            let mid_dy = (content_h / 2) as u16;
            let y = area.y + 1 + mid_dy;
            let pad = (inner_w - face_w) / 2;
            let face_style = Style::default().fg(spec.color);
            buf.set_stringn(
                area.x + 1 + pad as u16,
                y,
                spec.face,
                inner_w - pad,
                face_style,
            );
        }
    }
}

fn paint_frame(buf: &mut Buffer, area: Rect, title: Option<&str>, style: Style) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let inner_w = area.width as usize - 2;
    let title_chars: Vec<char> = title.map(|t| t.chars().collect()).unwrap_or_default();
    let tw = title_chars.len();

    // Top border.
    buf[(area.x, area.y)].set_char('╭').set_style(style);
    buf[(area.x + area.width - 1, area.y)]
        .set_char('╮')
        .set_style(style);
    if tw > 0 && tw <= inner_w {
        let pad = inner_w - tw;
        let lpad = pad / 2;
        for i in 0..lpad as u16 {
            buf[(area.x + 1 + i, area.y)].set_char('─').set_style(style);
        }
        for (i, ch) in title_chars.iter().enumerate() {
            buf[(area.x + 1 + lpad as u16 + i as u16, area.y)]
                .set_char(*ch)
                .set_style(style);
        }
        for i in (1 + lpad + tw)..(1 + inner_w) {
            buf[(area.x + i as u16, area.y)]
                .set_char('─')
                .set_style(style);
        }
    } else {
        for i in 0..inner_w {
            buf[(area.x + 1 + i as u16, area.y)]
                .set_char('─')
                .set_style(style);
        }
    }

    // Sides + blank interior.
    for dy in 1..(area.height - 1) {
        let y = area.y + dy;
        buf[(area.x, y)].set_char('│').set_style(style);
        for dx in 1..(area.width - 1) {
            buf[(area.x + dx, y)].set_char(' ');
        }
        buf[(area.x + area.width - 1, y)]
            .set_char('│')
            .set_style(style);
    }

    // Bottom border.
    let by = area.y + area.height - 1;
    buf[(area.x, by)].set_char('╰').set_style(style);
    for i in 0..inner_w {
        buf[(area.x + 1 + i as u16, by)]
            .set_char('─')
            .set_style(style);
    }
    buf[(area.x + area.width - 1, by)]
        .set_char('╯')
        .set_style(style);
}

fn paint_empty_box(buf: &mut Buffer, area: Rect, style: Style) {
    paint_frame(buf, area, None, style);
}

fn paint_editor_box(
    buf: &mut Buffer,
    area: Rect,
    text: &str,
    cursor_col: u16,
    is_running: bool,
    style: Style,
) {
    paint_frame(buf, area, None, style);
    if area.width < 6 || area.height < 3 {
        return;
    }
    let inner_w = area.width as usize - 2;
    // First inner row (just below the top border).
    let y = area.y + 1;
    // Prompt prefix.
    let prompt = if is_running { "░▌ " } else { "▌▌ " };
    let accent = Style::default().fg(RColor::Yellow);
    let user = Style::default().fg(RColor::White);
    buf.set_stringn(area.x + 1, y, prompt, inner_w, accent);
    // Editor text after the prompt.
    let prompt_w = prompt.chars().count() as u16;
    let text_x = area.x + 1 + prompt_w;
    let text_avail = inner_w.saturating_sub(prompt_w as usize);
    buf.set_stringn(text_x, y, text, text_avail, user);

    // Cursor is shown via `Frame::set_cursor_position` at the
    // render-frame layer (so it blinks naturally). The widget only
    // owns text + style; it doesn't try to fake the cursor with an
    // inverted-bg cell anymore.
    let _ = cursor_col;
}

fn paint_overlay_box(
    buf: &mut Buffer,
    area: Rect,
    title: &str,
    lines: &[(String, crossterm::style::Color)],
    style: Style,
) {
    paint_frame(buf, area, Some(title), style);
    let inner_w = area.width as usize - 2;
    let inner_h = area.height as usize - 2;
    for (i, slot) in (0..inner_h).enumerate() {
        if let Some((text, color)) = lines.get(i) {
            let y = area.y + 1 + slot as u16;
            // Center each line horizontally within the inner band.
            let tw = text.chars().count();
            let pad = inner_w.saturating_sub(tw) / 2;
            let st = Style::default().fg(super::chat::crossterm_to_ratatui(*color));
            buf.set_stringn(area.x + 1 + pad as u16, y, text, inner_w - pad, st);
        }
    }
}

fn paint_status(buf: &mut Buffer, area: Rect, status: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::default().fg(RColor::DarkGray);
    // Wipe + paint left-aligned (the legacy renderer centered it
    // under the chat band, but with the centered layout the status
    // bar reads cleaner left-flush).
    for dx in 0..area.width {
        buf[(area.x + dx, area.y)].set_char(' ');
    }
    buf.set_stringn(area.x, area.y, status, area.width as usize, style);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render<F: FnOnce(&Layout) -> BottomStrip<'_>>(
        cols: u16,
        rows: u16,
        input_rows: u16,
        build: F,
    ) -> TestBackend {
        let layout = Layout::new(cols, rows, input_rows);
        let mut backend = TestBackend::new(cols, rows);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                let widget = build(&layout);
                f.render_widget(widget, area);
            })
            .unwrap();
        backend = terminal.backend().clone();
        backend
    }

    fn row_chars(backend: &TestBackend, y: u16, x0: u16, w: u16) -> Vec<char> {
        (0..w)
            .map(|i| {
                backend
                    .buffer()
                    .cell((x0 + i, y))
                    .unwrap()
                    .symbol()
                    .chars()
                    .next()
                    .unwrap_or(' ')
            })
            .collect()
    }

    /// Avatar box: rounded frame around the avatar rect, face
    /// centered vertically + horizontally.
    #[test]
    fn avatar_box_centers_face() {
        let layout = Layout::new(160, 30, 1);
        let backend = render(160, 30, 1, |l| {
            BottomStrip::new(l).avatar(AvatarSpec {
                face: "(-_-)",
                color: RColor::Green,
            })
        });
        let area = layout.avatar_box;
        // Top + bottom borders.
        let top = row_chars(&backend, area.y, area.x, area.width);
        assert_eq!(top[0], '╭');
        assert_eq!(top[area.width as usize - 1], '╮');
        let bot = row_chars(&backend, area.y + area.height - 1, area.x, area.width);
        assert_eq!(bot[0], '╰');
        assert_eq!(bot[area.width as usize - 1], '╯');
        // Face on the middle row.
        let mid_y = area.y + 1 + (area.height - 2) / 2;
        let mid = row_chars(&backend, mid_y, area.x, area.width);
        let face_str: String = mid.iter().collect();
        assert!(face_str.contains("(-_-)"), "got mid row {:?}", face_str);
        // Edges of mid row should still be │.
        assert_eq!(mid[0], '│');
        assert_eq!(mid[area.width as usize - 1], '│');
    }

    /// Input box (editor mode): rounded frame + prompt + text.
    #[test]
    fn input_box_paints_editor() {
        let layout = Layout::new(160, 30, 1);
        let backend = render(160, 30, 1, |l| {
            BottomStrip::new(l).body(BottomBody::Editor {
                text: "hi",
                cursor_col: 2,
                is_running: false,
            })
        });
        let area = layout.input_box;
        // Top row: ╭───────╮ (no title).
        let top = row_chars(&backend, area.y, area.x, area.width);
        assert_eq!(top[0], '╭');
        assert_eq!(top[area.width as usize - 1], '╮');
        // First inner row contains the prompt + "hi".
        let body: String = row_chars(&backend, area.y + 1, area.x, area.width)
            .into_iter()
            .collect();
        assert!(body.contains("▌▌ hi"), "got body {:?}", body);
        // First and last cells are the side borders.
        let chars: Vec<char> = body.chars().collect();
        assert_eq!(chars[0], '│');
        assert_eq!(chars[area.width as usize - 1], '│');
    }

    /// Overlay mode: rounded frame with title in top border, lines
    /// painted inside. Replaces the editor — no second box.
    #[test]
    fn input_box_overlay_replaces_editor() {
        use crossterm::style::Color as CC;
        let layout = Layout::new(160, 30, 1);
        let lines = vec![
            ("⚠ PERMISSION REQUIRED".to_string(), CC::Yellow),
            ("tool: read_file".to_string(), CC::Yellow),
        ];
        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                let widget = BottomStrip::new(&layout).body(BottomBody::Overlay {
                    title: "[ALERT]",
                    lines: &lines,
                });
                f.render_widget(widget, area);
            })
            .unwrap();
        backend = terminal.backend().clone();
        let area = layout.input_box;
        let top: String = row_chars(&backend, area.y, area.x, area.width)
            .into_iter()
            .collect();
        // Top should contain [ALERT] centered.
        assert!(top.contains("[ALERT]"), "got top {:?}", top);
        assert!(top.starts_with('╭'));
        // Body row 0 contains the warning text.
        let body0: String = row_chars(&backend, area.y + 1, area.x, area.width)
            .into_iter()
            .collect();
        assert!(
            body0.contains("PERMISSION REQUIRED"),
            "got body0 {:?}",
            body0
        );
    }

    /// right_margin gets blanked even when the strip has body
    /// content — so right-panel pixels from earlier renders can't
    /// ghost into the bottom strip.
    #[test]
    fn right_margin_is_blanked() {
        let layout = Layout::new(160, 30, 1);
        // Pre-fill the buffer's right margin with garbage by
        // rendering once, then overwriting cells manually, then
        // re-rendering BottomStrip and asserting the cells are
        // back to ' '.
        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        // First draw: write 'X' into right_margin cols.
        terminal
            .draw(|f| {
                let buf = f.buffer_mut();
                for dy in 0..layout.right_margin.height {
                    for dx in 0..layout.right_margin.width {
                        let x = layout.right_margin.x + dx;
                        let y = layout.right_margin.y + dy;
                        buf[(x, y)].set_char('X');
                    }
                }
            })
            .unwrap();
        // Second draw: BottomStrip should blank right_margin.
        terminal
            .draw(|f| {
                let area = f.area();
                f.render_widget(BottomStrip::new(&layout), area);
            })
            .unwrap();
        backend = terminal.backend().clone();
        for dy in 0..layout.right_margin.height {
            for dx in 0..layout.right_margin.width {
                let x = layout.right_margin.x + dx;
                let y = layout.right_margin.y + dy;
                let c = backend.buffer().cell((x, y)).unwrap().symbol();
                assert_eq!(c, " ", "right_margin not blanked at ({x},{y}) = {c:?}");
            }
        }
    }

    /// Status row paints the supplied status text on the bottom row.
    #[test]
    fn status_row_paints_text() {
        let layout = Layout::new(160, 30, 1);
        let backend = render(160, 30, 1, |l| BottomStrip::new(l).status("ready [code]"));
        let row: String = row_chars(&backend, layout.status.y, 0, 160)
            .into_iter()
            .collect();
        assert!(row.starts_with("ready [code]"), "got status {:?}", row);
    }
}
