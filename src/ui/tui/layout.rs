//! Layout: single source of truth for UI region geometry.
//!
//! All widgets paint into a `ratatui::Rect` returned from this
//! module. Per-callsite column math in the legacy renderer was the
//! root cause of misaligned frames, ghost borders, and side-panel
//! flicker — concentrating geometry here makes those bugs
//! structurally impossible.

use ratatui::layout::Rect;

/// Maximum chat content width before centering kicks in. Above this
/// the chat band is capped at 120 cols and side panels grow to fill
/// the remaining space symmetrically.
pub const CHAT_CONTENT_MAX_W: u16 = 120;

/// Maximum input editor row count (matches the legacy
/// `MAX_INPUT_VISIBLE_LINES`). Layout consumers are expected to
/// clamp at this value before passing `input_rows`.
pub const MAX_INPUT_ROWS: u16 = 8;

/// All region rects for one frame.
///
/// Layout rows top → bottom:
///
/// ```text
/// row 0                       top_frame    ───[AGENT STATUS]───╭───[AGENT LOG STREAM]───╮───[SYSTEM]───
/// rows 1..=chat_bot_frame-1   chat / left_panel / right_panel  (chat │ on each side)
/// row chat_bot_frame          chat closes (╰───╯ inside chat band, blanks outside)
/// row bottom_strip_top        avatar/input top frame  (╭───╮ inside chat band)
/// rows ..                     avatar (centered face) | input editor | right margin (blank)
/// row bottom_strip_bot        avatar/input bot frame  (╰───╯ inside chat band)
/// row rows-1                  status                            (full-width status line)
/// ```
///
/// Horizontally:
///
/// ```text
/// cols [0, left_panel.right)        left panel content
/// col  left_panel.right - 1         (== chat.left - 1: paint chat's left │ at chat.x - 1)
/// cols [chat.x, chat.right)         chat content (between the │ borders)
/// col  chat.right                   chat's right │
/// cols [chat.right+1, cols)         right panel content
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Full terminal viewport — `Rect { x: 0, y: 0, width: cols, height: rows }`.
    pub full: Rect,
    /// Top frame row (single row, full width).
    pub top_frame: Rect,
    /// Chat content inner rect (between the │ borders, top frame, and chat bottom frame).
    pub chat: Rect,
    /// Left panel content (excluding chat's left │).
    pub left_panel: Rect,
    /// Right panel content (excluding chat's right │).
    pub right_panel: Rect,
    /// Column where the chat's left │ sits (row range = chat.y..chat_bot_frame.y).
    pub chat_v_left_col: u16,
    /// Column where the chat's right │ sits.
    pub chat_v_right_col: u16,
    /// Chat bottom frame row (single row, full width — ╰───╯ painted in chat band only).
    pub chat_bot_frame: Rect,
    /// Avatar box rect — bottom-left, mirrors the input box's vertical extent.
    pub avatar_box: Rect,
    /// Input box rect (light rounded frame, between chat verticals).
    pub input_box: Rect,
    /// Right margin in the bottom strip (mirrors avatar_box on the right). Blank.
    pub right_margin: Rect,
    /// Status line row (single row, full width).
    pub status: Rect,
}

impl Layout {
    /// Compute layout for `cols × rows` with `input_rows` editor
    /// lines, with both side panels visible. `input_rows` is clamped
    /// to `[1, MAX_INPUT_ROWS]`.
    ///
    /// On very small terminals some rects collapse to zero width or
    /// height (e.g. left/right panels disappear when the chat fills
    /// the available width). Callers must check `rect.is_empty()`
    /// before painting into a region.
    ///
    /// Production callers go through [`Layout::with_panels`] to honour
    /// per-side visibility; `new` is the both-visible convenience used
    /// throughout the widget tests.
    #[allow(dead_code)]
    pub fn new(cols: u16, rows: u16, input_rows: u16) -> Self {
        Self::with_panels(cols, rows, input_rows, true, true)
    }

    /// Compute layout for `cols × rows` with `input_rows` editor lines,
    /// reserving gutter space only for the side panels that are shown.
    ///
    /// A hidden panel's gutter is reclaimed by the chat band rather
    /// than left blank — so `/display main` widens the conversation to
    /// the full content width, and hiding one side shifts the chat to
    /// absorb that side's gutter while the other panel keeps its width.
    /// When both panels are visible this is identical to [`Layout::new`].
    pub fn with_panels(
        cols: u16,
        rows: u16,
        input_rows: u16,
        show_left: bool,
        show_right: bool,
    ) -> Self {
        let input_rows = input_rows.clamp(1, MAX_INPUT_ROWS);
        let full = Rect::new(0, 0, cols, rows);

        // Vertical layout. From bottom up:
        //   status (1) + bot_strip_bot (1) + input_rows + bot_strip_top (1) +
        //   chat_bot_frame (1) + chat_content (N) + top_frame (1) = rows
        // So chat_content_h = rows - input_rows - 5 (saturating).
        let fixed_v = 5_u16; // top_frame + chat_bot_frame + 2 bot-strip frames + status
        let chat_h = rows.saturating_sub(input_rows).saturating_sub(fixed_v);
        let top_frame = Rect::new(0, 0, cols, if rows >= 1 { 1 } else { 0 });
        // Chat content rows 1..1+chat_h
        let chat_content_top = 1_u16.min(rows);
        let chat_bot_frame_y = chat_content_top.saturating_add(chat_h);
        let bottom_strip_top_y = chat_bot_frame_y.saturating_add(1);
        let input_top_y = bottom_strip_top_y.saturating_add(1);
        let bottom_strip_bot_y = input_top_y.saturating_add(input_rows);
        let status_y = rows.saturating_sub(1);

        // Horizontal layout: chat band centered in (cols - 2) so the
        // left/right gutters are symmetric. Each VISIBLE side panel
        // takes `base_gutter` cols; the gutter of a hidden panel is
        // reclaimed by the chat band instead of left blank, so the
        // conversation expands to use the freed space.
        let line_w = cols.saturating_sub(2);
        let base_chat_w = line_w.min(CHAT_CONTENT_MAX_W);
        let base_gutter = line_w.saturating_sub(base_chat_w) / 2;
        let left_gutter = if show_left { base_gutter } else { 0 };
        // Chat keeps its capped width plus whichever gutters are freed.
        let freed =
            if show_left { 0 } else { base_gutter } + if show_right { 0 } else { base_gutter };
        let chat_content_w = base_chat_w.saturating_add(freed);

        let chat_v_left_col = left_gutter; // col of chat's left │
        let chat_x = chat_v_left_col.saturating_add(1);
        let chat_v_right_col = chat_x.saturating_add(chat_content_w);
        // Right panel starts after the right │. Compute its width
        // as remainder so any rounding from the symmetric gutter
        // math doesn't leak (right side absorbs the +1).
        let right_panel_x = chat_v_right_col.saturating_add(1);
        let right_panel_w = cols.saturating_sub(right_panel_x);

        // Chat content rect — between │ borders, rows 1..chat_bot_frame_y.
        let chat = Rect::new(chat_x, chat_content_top, chat_content_w, chat_h);

        let left_panel = Rect::new(0, chat_content_top, left_gutter, chat_h);
        let right_panel = Rect::new(right_panel_x, chat_content_top, right_panel_w, chat_h);

        let chat_bot_frame = Rect::new(0, chat_bot_frame_y, cols, 1);

        // Bottom strip vertical extent = top_frame + input_rows + bot_frame.
        let strip_h = input_rows.saturating_add(2);
        // Avatar box mirrors the left panel cols; input box spans
        // chat verticals inclusive; right margin mirrors right panel cols.
        let avatar_box = Rect::new(0, bottom_strip_top_y, left_gutter, strip_h);
        // Input box includes both chat ║ cols so its rounded ╭...╮
        // sits exactly at the chat verticals — visual continuation.
        let input_box_x = chat_v_left_col;
        let input_box_w = chat_v_right_col
            .saturating_sub(chat_v_left_col)
            .saturating_add(1);
        let input_box = Rect::new(input_box_x, bottom_strip_top_y, input_box_w, strip_h);
        let right_margin_w = cols.saturating_sub(right_panel_x);
        let right_margin = Rect::new(right_panel_x, bottom_strip_top_y, right_margin_w, strip_h);

        // Status: single row at the very bottom.
        let status = Rect::new(0, status_y, cols, if rows >= 1 { 1 } else { 0 });

        let _ = bottom_strip_bot_y; // accounted for via strip_h

        Self {
            full,
            top_frame,
            chat,
            left_panel,
            right_panel,
            chat_v_left_col,
            chat_v_right_col,
            chat_bot_frame,
            avatar_box,
            input_box,
            right_margin,
            status,
        }
    }

    /// Convenience: chat content height in rows.
    #[allow(dead_code)]
    pub fn chat_height(&self) -> u16 {
        self.chat.height
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wide terminal: chat is capped at CHAT_CONTENT_MAX_W and side
    /// panels split the remaining gutter symmetrically.
    #[test]
    fn wide_terminal_centers_chat() {
        let l = Layout::new(200, 30, 1);
        // cols=200, line_w=198, chat=120, gutter=(198-120)/2=39
        assert_eq!(l.chat_v_left_col, 39);
        assert_eq!(l.chat.x, 40);
        assert_eq!(l.chat.width, CHAT_CONTENT_MAX_W);
        assert_eq!(l.chat_v_right_col, 160);
        // Right panel absorbs the +1 from rounding.
        assert_eq!(l.right_panel.x, 161);
        assert_eq!(l.right_panel.width, 39);
        assert_eq!(l.left_panel.width, 39);
    }

    /// Hiding the right panel reclaims its gutter for the chat band:
    /// the chat grows by the freed gutter width, the left panel keeps
    /// its width, and the right panel collapses to zero.
    #[test]
    fn hiding_right_panel_expands_chat() {
        let both = Layout::new(200, 30, 1);
        let l = Layout::with_panels(200, 30, 1, true, false);
        // Left panel unchanged; right panel gone.
        assert_eq!(l.left_panel.width, both.left_panel.width);
        assert_eq!(l.right_panel.width, 0);
        // Chat absorbs the freed right gutter (no blank reserved space).
        assert_eq!(l.chat.width, both.chat.width + both.right_panel.width);
        assert_eq!(l.chat.x, both.chat.x); // chat start unchanged (left kept)
        // Horizontal tiling still covers the full viewport.
        assert_eq!(
            l.left_panel.width + 1 + l.chat.width + 1 + l.right_panel.width,
            200
        );
    }

    /// Hiding the left panel reclaims its gutter for the chat band and
    /// shifts the chat flush-left.
    #[test]
    fn hiding_left_panel_expands_chat() {
        let both = Layout::new(200, 30, 1);
        let l = Layout::with_panels(200, 30, 1, false, true);
        assert_eq!(l.left_panel.width, 0);
        assert_eq!(l.right_panel.width, both.right_panel.width);
        assert_eq!(l.chat.width, both.chat.width + both.left_panel.width);
        assert_eq!(l.chat_v_left_col, 0);
        assert_eq!(l.chat.x, 1);
        assert_eq!(
            l.left_panel.width + 1 + l.chat.width + 1 + l.right_panel.width,
            200
        );
    }

    /// Hiding both panels gives the entire content band to the chat.
    #[test]
    fn hiding_both_panels_fills_chat() {
        let l = Layout::with_panels(200, 30, 1, false, false);
        assert_eq!(l.left_panel.width, 0);
        assert_eq!(l.right_panel.width, 0);
        assert_eq!(l.chat.width, 198); // cols - 2 (the two │ borders)
        assert_eq!(l.chat.x, 1);
    }

    /// The avatar box (bottom-left) tracks the left gutter, so hiding
    /// the left panel collapses it and the input box expands left.
    #[test]
    fn avatar_box_tracks_left_gutter() {
        let both = Layout::new(200, 30, 1);
        assert_eq!(both.avatar_box.width, both.left_panel.width);
        let left_hidden = Layout::with_panels(200, 30, 1, false, true);
        assert_eq!(left_hidden.avatar_box.width, 0);
        // Input box now starts at col 0 (flush left, no avatar gutter).
        assert_eq!(left_hidden.input_box.x, 0);
    }

    /// `Layout::new` is the both-panels-visible case of `with_panels`.
    #[test]
    fn new_matches_with_panels_both_visible() {
        assert_eq!(
            Layout::new(200, 30, 1),
            Layout::with_panels(200, 30, 1, true, true)
        );
        assert_eq!(
            Layout::new(80, 24, 3),
            Layout::with_panels(80, 24, 3, true, true)
        );
    }

    /// Narrow terminal: line_w <= CHAT_CONTENT_MAX_W → no gutter,
    /// side panels collapse to zero width, chat fills.
    #[test]
    fn narrow_terminal_drops_side_panels() {
        let l = Layout::new(80, 24, 1);
        assert_eq!(l.left_panel.width, 0);
        assert_eq!(l.right_panel.width, 0);
        assert_eq!(l.chat.width, 78); // cols - 2
        assert_eq!(l.chat_v_left_col, 0);
        assert_eq!(l.chat_v_right_col, 79);
    }

    /// Vertical rects tile without overlap and cover the whole
    /// viewport. Sum of region heights == rows (when accounting for
    /// the side-panel cols that the bottom strip overlays).
    #[test]
    fn vertical_tiling_covers_viewport() {
        let l = Layout::new(200, 30, 1);
        let rows = 30_u16;
        // 1 (top) + chat_h + 1 (chat bot) + (2 + input_rows) (strip) + 1 (status) == rows
        assert_eq!(
            1 + l.chat.height + 1 + l.input_box.height + 1,
            rows,
            "vertical tiling: {:?}",
            l
        );
        // Adjacent rects must abut, not overlap.
        assert_eq!(l.top_frame.y + l.top_frame.height, l.chat.y);
        assert_eq!(l.chat.y + l.chat.height, l.chat_bot_frame.y);
        assert_eq!(l.chat_bot_frame.y + l.chat_bot_frame.height, l.input_box.y);
        assert_eq!(l.input_box.y + l.input_box.height, l.status.y);
    }

    /// Horizontally the four regions on a chat row tile: left panel
    /// + chat-│ + chat content + chat-│ + right panel == cols.
    #[test]
    fn horizontal_tiling_on_chat_row() {
        let l = Layout::new(200, 30, 1);
        let cols = 200_u16;
        // left_panel + 1 (left │) + chat + 1 (right │) + right_panel == cols
        let sum = l.left_panel.width + 1 + l.chat.width + 1 + l.right_panel.width;
        assert_eq!(sum, cols, "horizontal tiling: {:?}", l);
        // chat_v_left_col immediately after left_panel.
        assert_eq!(l.chat_v_left_col, l.left_panel.width);
        // chat_v_right_col immediately before right_panel.
        assert_eq!(l.chat_v_right_col, cols - l.right_panel.width - 1);
    }

    /// Input box overlays the chat │ columns so the input frame's
    /// ╭ ╮ ╰ ╯ corners sit exactly at the chat's vertical lines.
    #[test]
    fn input_box_aligns_with_chat_verticals() {
        let l = Layout::new(200, 30, 1);
        assert_eq!(l.input_box.x, l.chat_v_left_col);
        assert_eq!(l.input_box.x + l.input_box.width - 1, l.chat_v_right_col);
    }

    /// Avatar box ends exactly where input box begins; right margin
    /// begins where input box ends. No 1-col gaps anywhere.
    #[test]
    fn bottom_strip_horizontal_tiles() {
        let l = Layout::new(200, 30, 1);
        // avatar.right == input.left.
        assert_eq!(l.avatar_box.x + l.avatar_box.width, l.input_box.x);
        // input.right == right_margin.left.
        assert_eq!(l.input_box.x + l.input_box.width, l.right_margin.x);
        // right_margin.right == cols.
        assert_eq!(l.right_margin.x + l.right_margin.width, 200);
    }

    /// Tall input shrinks chat height by the same amount.
    #[test]
    fn growing_input_shrinks_chat() {
        let one = Layout::new(200, 30, 1);
        let eight = Layout::new(200, 30, 8);
        assert_eq!(one.chat.height - 7, eight.chat.height);
        // Bottom strip starts higher up.
        assert!(eight.input_box.y < one.input_box.y);
        // Status stays at the same row.
        assert_eq!(one.status.y, eight.status.y);
    }

    /// Clamping: input_rows > MAX_INPUT_ROWS clamps to MAX.
    #[test]
    fn input_rows_clamps_to_max() {
        let big = Layout::new(200, 40, 99);
        let max = Layout::new(200, 40, MAX_INPUT_ROWS);
        assert_eq!(big.input_box.height, max.input_box.height);
        assert_eq!(big.chat.height, max.chat.height);
    }

    /// Degenerate viewport: very small rows shouldn't panic or
    /// produce negative widths.
    #[test]
    fn degenerate_small_viewport_is_safe() {
        let l = Layout::new(20, 6, 1);
        // chat_h = 6 - 1 - 5 = 0; everything else gracefully
        // collapses but stays in-bounds.
        assert!(l.chat.x + l.chat.width <= 20);
        assert!(l.status.y < 6);
    }
}
