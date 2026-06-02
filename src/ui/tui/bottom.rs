//! Bottom strip widget — avatar box, input box (or overlay), and
//! the status row.
//!
//! The strip occupies three rects from `Layout`: `avatar_box` (the
//! face on the bottom-left), `input_box` (between the chat │
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
    /// User input editor. Pre-wrapped into one string per visual
    /// row — caller is responsible for splitting on newlines AND
    /// soft-wrapping long logical lines to the input box's inner
    /// width. `cursor_row` and `cursor_col` locate the cursor
    /// inside `rows` (rows[cursor_row], display col 0-based).
    Editor {
        rows: &'a [String],
        cursor_row: u16,
        cursor_col: u16,
        is_running: bool,
        /// Dim preview row shown below the input when slash-command
        /// tab completion is active (e.g. "/mode  /panel  /quit").
        /// Empty string = no preview.
        completion_preview: &'a str,
        /// Inline dark-gray ghost completion painted right after the
        /// typed text (e.g. typing "/dis" shows "play"). Empty = none.
        ghost: &'a str,
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
                rows,
                is_running,
                completion_preview,
                ghost,
                ..
            }) => paint_editor_box(
                buf,
                l.input_box,
                rows,
                *is_running,
                completion_preview,
                ghost,
                self.border_style,
            ),
            Some(BottomBody::Overlay { title, lines }) => {
                paint_overlay_box(buf, l.input_box, title, lines, self.border_style)
            }
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

#[allow(clippy::too_many_arguments)]
fn paint_editor_box(
    buf: &mut Buffer,
    area: Rect,
    rows: &[String],
    is_running: bool,
    completion_preview: &str,
    ghost: &str,
    style: Style,
) {
    paint_frame(buf, area, None, style);
    if area.width < 6 || area.height < 3 {
        return;
    }
    let inner_w = area.width as usize - 2;
    let prompt_main = if is_running { "░▌ " } else { "> " };
    // Continuation prompt — a single dim glyph + spaces — so wrapped
    // lines visually attach to the prompt above without taking
    // another bold spinner.
    let prompt_cont = "▏  ";
    // Reserve 3 cells for the prompt zone regardless of which glyph
    // fills it (running spinner `░▌ ` is 3 cells; idle `> ` paints 2
    // and leaves the 3rd blank). Keeping the width fixed keeps the
    // editor text column and `scene.rs`'s cursor math aligned.
    let prompt_w = 3_usize;
    let accent = Style::default().fg(RColor::Yellow);
    let user = Style::default().fg(RColor::White);
    let dim = Style::default().fg(RColor::DarkGray);
    let text_avail = inner_w.saturating_sub(prompt_w);
    let visible_rows = (area.height as usize).saturating_sub(2);
    let has_preview = !completion_preview.is_empty();
    let editor_rows = visible_rows.saturating_sub(if has_preview { 1 } else { 0 });
    // Track the last painted row so the inline ghost completion can be
    // drawn right after its text.
    let mut last_row: Option<(u16, usize)> = None;
    for (i, row_text) in rows.iter().take(editor_rows).enumerate() {
        let y = area.y + 1 + i as u16;
        let prompt = if i == 0 { prompt_main } else { prompt_cont };
        buf.set_stringn(area.x + 1, y, prompt, inner_w, accent);
        let text_x = area.x + 1 + prompt_w as u16;
        buf.set_stringn(text_x, y, row_text, text_avail, user);
        last_row = Some((y, row_text.chars().count()));
    }
    // Inline dark-gray ghost completion, painted right after the typed
    // text on the last row (only set when the cursor is at end-of-input).
    if !ghost.is_empty()
        && let Some((y, used)) = last_row
    {
        let remaining = text_avail.saturating_sub(used);
        if remaining > 0 {
            let ghost_x = area.x + 1 + prompt_w as u16 + used as u16;
            buf.set_stringn(ghost_x, y, ghost, remaining, dim);
        }
    }
    if has_preview {
        let preview_y = area.y + 1 + editor_rows as u16;
        buf.set_stringn(area.x + 1, preview_y, prompt_cont, inner_w, accent);
        let text_x = area.x + 1 + prompt_w as u16;
        buf.set_stringn(text_x, preview_y, completion_preview, text_avail, dim);
    }
    // Cursor positioning is owned by render_frame via
    // Frame::set_cursor_position.
}

/// Return the display width of a `label:` prefix at the start of
/// `text`, or 0 if there is no such prefix. The label must be all
/// ASCII alpha (`[A-Za-z]+`) followed by a literal `: ` — narrow
/// enough that arbitrary user content like "1:30 ratio" won't be
/// treated as a label. Used to compute hanging continuation
/// indents for wrapped overlay lines.
fn label_prefix_width(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i > 0 && bytes.get(i) == Some(&b':') && bytes.get(i + 1) == Some(&b' ') {
        i + 2
    } else {
        0
    }
}

fn paint_overlay_box(
    buf: &mut Buffer,
    area: Rect,
    title: &str,
    lines: &[(String, crossterm::style::Color)],
    _style: Style,
) {
    // Alert box border is yellow regardless of the supplied frame
    // style — the user must see at a glance that this is a
    // cautionary modal, not a casual chrome border.
    let yellow = Style::default().fg(RColor::Yellow);
    paint_frame(buf, area, Some(title), yellow);
    let inner_w = area.width as usize - 2;
    let inner_h = area.height as usize - 2;

    // Soft-wrap each input line, but use a single SENTINEL value for
    // the action-keys row (passed as the LAST line in `lines`) so
    // we can guarantee it stays visible even if the body overflows.
    // Lines shaped like `label: value` get a hanging continuation
    // indent equal to the label width — wrapped rows align under
    // the value column instead of bleeding back to col 0.
    let wrap_w = inner_w.saturating_sub(2).max(1);
    use crate::ui::wrap::soft_wrap;
    let last_idx = lines.len().saturating_sub(1);
    let mut head_visual: Vec<(String, crossterm::style::Color)> = Vec::new();
    let mut sticky_last: Vec<(String, crossterm::style::Color)> = Vec::new();
    for (i, (text, color)) in lines.iter().enumerate() {
        // Detect `label: ` prefix to derive the hanging indent for
        // continuation rows. e.g. `args: very long content` wraps to:
        //   args: very long
        //         content
        // rather than:
        //   args: very long
        //   content
        // We restrict this to ASCII alpha labels followed by ": " so
        // arbitrary user content like "1:30 ratio" doesn't trigger.
        let hang = label_prefix_width(text);
        let cont_indent: String = " ".repeat(hang);
        let chunks = soft_wrap(text, wrap_w, &cont_indent);
        for chunk in chunks {
            if i == last_idx {
                sticky_last.push((chunk, *color));
            } else {
                head_visual.push((chunk, *color));
            }
        }
    }

    // Layout: head visual rows fill from the top. If the combined
    // head + sticky doesn't fit `inner_h`, truncate the END of head
    // with a "…" indicator so the sticky (action keys) row is
    // always shown.
    let sticky_len = sticky_last.len();
    let head_budget = inner_h.saturating_sub(sticky_len);

    let need_ellipsis = head_visual.len() > head_budget;
    let head_keep = if need_ellipsis {
        head_budget.saturating_sub(1)
    } else {
        head_budget
    };
    let head_to_show: Vec<&(String, crossterm::style::Color)> =
        head_visual.iter().take(head_keep).collect();

    // Paint head rows LEFT-aligned with 1 leading space of padding.
    let mut row_idx = 0usize;
    for (text, color) in head_to_show {
        let y = area.y + 1 + row_idx as u16;
        let st = Style::default().fg(super::chat::crossterm_to_ratatui(*color));
        buf.set_stringn(area.x + 2, y, text, inner_w.saturating_sub(2), st);
        row_idx += 1;
    }
    if need_ellipsis && row_idx < inner_h {
        let y = area.y + 1 + row_idx as u16;
        let dim = Style::default().fg(RColor::DarkGray);
        buf.set_stringn(area.x + 2, y, "…", inner_w.saturating_sub(2), dim);
        row_idx += 1;
    }
    // Paint the sticky tail rows (action keys) at the BOTTOM of the
    // inner band so they're always visible.
    let tail_start = inner_h.saturating_sub(sticky_len);
    for (i, (text, color)) in sticky_last.iter().enumerate() {
        let slot = tail_start + i;
        if slot >= inner_h {
            break;
        }
        let y = area.y + 1 + slot as u16;
        let st = Style::default().fg(super::chat::crossterm_to_ratatui(*color));
        buf.set_stringn(area.x + 2, y, text, inner_w.saturating_sub(2), st);
    }
    let _ = row_idx; // silence linter; head rows fill from top, tail anchors to bottom
}

/// Soft-wrap the overlay `lines` against `outer_width` (the input
/// box's outer width — inner cells = outer - 2) and return the
/// total number of visual rows the body needs. Called by the
/// renderer to size `input_box.height` so wrapped content fits
/// instead of getting clipped at the box edge.
pub fn overlay_wrapped_row_count(
    lines: &[(String, crossterm::style::Color)],
    outer_width: u16,
) -> usize {
    let inner_w = (outer_width as usize).saturating_sub(2);
    let wrap_w = inner_w.saturating_sub(4).max(1);
    use crate::ui::wrap::soft_wrap;
    let mut total = 0;
    for (text, _) in lines {
        total += soft_wrap(text, wrap_w, "  ").len();
    }
    total
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
        let rows: Vec<String> = vec!["hi".to_string()];
        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                let widget = BottomStrip::new(&layout).body(BottomBody::Editor {
                    rows: &rows,
                    cursor_row: 0,
                    cursor_col: 2,
                    is_running: false,
                    completion_preview: "",
                    ghost: "",
                });
                f.render_widget(widget, area);
            })
            .unwrap();
        backend = terminal.backend().clone();
        let area = layout.input_box;
        // Top row: ╭───────╮ (no title).
        let top = row_chars(&backend, area.y, area.x, area.width);
        assert_eq!(top[0], '╭');
        assert_eq!(top[area.width as usize - 1], '╮');
        // First inner row contains the prompt + "hi". The idle
        // prompt `> ` fills 2 of the 3 reserved prompt cells (3rd
        // stays blank), so the text starts after two spaces.
        let body: String = row_chars(&backend, area.y + 1, area.x, area.width)
            .into_iter()
            .collect();
        assert!(body.contains(">  hi"), "got body {:?}", body);
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
        // input_rows=4 so the overlay has 4 inner rows. With our
        // sticky-tail logic, last line (action keys) anchors at the
        // bottom; head lines fill from the top. With 2 lines and
        // last-is-sticky, "⚠ PERMISSION REQUIRED" is at row 1 (top
        // inner), action-keys-equivalent "tool: read_file" at row 4
        // (bottom inner).
        let layout = Layout::new(160, 30, 4);
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
