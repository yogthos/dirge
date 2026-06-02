use std::io::{self, Write};

use compact_str::CompactString;
use crossterm::ExecutableCommand;
use crossterm::cursor::MoveTo;
use crossterm::style::Color;
use crossterm::terminal::{Clear, ClearType};
// `MoveTo` / `Clear` / `ExecutableCommand` are still used by
// `clear_content` (resets the alt screen on `/clear`). The
// streaming + viewport paint no longer touches stdout directly —
// that's all routed through `tui_redraw` (ratatui).

/// Output sink for ratatui's CrosstermBackend. Prefers a fresh
/// `/dev/tty` handle (so painting is isolated from the process's
/// fd 1 — see `TerminalGuard`'s fd redirection); falls back to
/// stdout when there's no controlling terminal (CI tests).
pub enum BackendWriter {
    // In test builds the constructor is stubbed (cfg(test) at the
    // factory below returns None), so the variants are never
    // constructed — but the `impl Write` arms still need them.
    #[cfg_attr(test, allow(dead_code))]
    Tty(std::fs::File),
    #[cfg_attr(test, allow(dead_code))]
    Stdout(std::io::Stdout),
}

impl std::io::Write for BackendWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            BackendWriter::Tty(f) => f.write(b),
            BackendWriter::Stdout(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            BackendWriter::Tty(f) => f.flush(),
            BackendWriter::Stdout(s) => s.flush(),
        }
    }
}

fn build_tui_terminal()
-> Option<ratatui::Terminal<ratatui::backend::CrosstermBackend<BackendWriter>>> {
    // Never open /dev/tty or stdout for painting during tests.
    // cargo test captures stdout but /dev/tty still points at the
    // real terminal.  Multiple test threads calling tui_redraw
    // (via write_line / scroll_to_bottom / render_viewport) would
    // interleave ratatui escape sequences directly onto the user's
    // screen, corrupting the terminal and triggering spurious
    // behaviours (form-feed print dialogs, colour leaks, cursor
    // jumps).  Returning None makes tui_redraw a no-op.
    #[cfg(test)]
    {
        return None;
    }
    #[cfg(not(test))]
    {
        let writer = match crate::ui::terminal::open_tty_for_write() {
            Some(f) => BackendWriter::Tty(f),
            None => BackendWriter::Stdout(std::io::stdout()),
        };
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(writer)).ok()
    }
}

#[derive(Clone)]
pub struct LineEntry {
    pub text: CompactString,
    pub color: Color,
}

/// Cap on how many logical input lines we'll show stacked at the bottom of
/// the screen before the input box starts internally scrolling. Beyond this
/// the chat-history viewport would be unreasonably squashed.
pub const MAX_INPUT_VISIBLE_LINES: usize = 8;

/// ui-redesign: the bottom [ALERT] panel wraps the input area in a
/// double-line frame. Two reserved rows = top border (with title)
/// plus bottom border. Side borders (│ ... │) are painted on every
/// input row so the entire input area reads as one framed card,
/// matching the mockup's bottom strip.
///
/// The frame title is `[ALERT]` permanently — input text and
/// permission prompts both live INSIDE the frame.
pub const ALERT_FRAME_ROWS: u16 = 2;

/// ui-redesign: chat area is wrapped in a heavy double-line frame
/// titled `[AGENT LOG STREAM]`. Two reserved rows = top border
/// (row 0) + bottom border (row 1 + visible_lines). Side borders
/// (│ … │) are painted at the chat-band edges on every visible
/// chat row when there's room (content_indent >= 1).
pub const CHAT_FRAME_ROWS: u16 = 2;

/// Minimum terminal width at which `PanelMode::Auto` decides to show
/// the side panels. Below this the chat is too narrow to spare any
/// margin for the AGENT STATUS / SYSTEM gutters.
const PANEL_AUTO_MIN_COLS: u16 = 100;

#[cfg(feature = "experimental-ui-terminal-tab")]
fn format_terminal_title(state: crate::ui::avatar::AvatarState, tool_name: Option<&str>) -> String {
    use crate::ui::avatar::AvatarState;
    // PR #144 follow-up: strip control bytes from caller-supplied
    // tool names. Today the names come from the internal tool
    // registry (`bash`, `edit`, …) so this is purely defensive,
    // but a plugin or MCP server is one register-call away from
    // smuggling `\x07` (BEL) or `\x1b` (ESC) into a name — which
    // would prematurely close the OSC or inject further escape
    // sequences when concatenated below. Newlines also break the
    // title display.
    let sanitize = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_control() && *c != '\u{0007}' && *c != '\u{001b}' && *c != '\u{009c}')
            .take(64)
            .collect()
    };
    match state {
        AvatarState::Idle | AvatarState::Done => "● dirge".to_string(),
        AvatarState::Thinking => "● dirge: thinking".to_string(),
        AvatarState::Speaking => "● dirge: responding".to_string(),
        AvatarState::Reading | AvatarState::Writing | AvatarState::Bash => {
            if let Some(name) = tool_name {
                let clean = sanitize(name);
                if clean.is_empty() {
                    "◌ dirge: working".to_string()
                } else {
                    format!("◌ dirge: {}", clean)
                }
            } else {
                "◌ dirge: working".to_string()
            }
        }
        AvatarState::Alert => "✗ dirge: needs input".to_string(),
        AvatarState::Error => "✗ dirge: ERROR".to_string(),
    }
}

/// Build the OSC-0 byte sequence to set the terminal title. PR #144
/// follow-up: switch to ST (`\x1b\\`) terminator, which is the
/// RFC 1605 / xterm-canonical form and passes through tmux without
/// needing `set-option -g allow-passthrough on`. BEL works on most
/// terminals but tmux specifically prefers ST.
#[cfg(feature = "experimental-ui-terminal-tab")]
fn osc_set_title(title: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(title.len() + 5);
    out.extend_from_slice(b"\x1b]0;");
    out.extend_from_slice(title.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Emit an empty OSC-0 to release the terminal title back to the
/// shell's default. The TUI shutdown path in `terminal.rs` inlines
/// the same bytes alongside other reset escapes for efficiency;
/// this helper exists as a single source of truth for future
/// callers (signal handlers, panic-recovery, etc.) and to anchor
/// the unit test.
#[cfg(feature = "experimental-ui-terminal-tab")]
#[allow(dead_code)]
fn osc_reset_title() -> Vec<u8> {
    b"\x1b]0;\x1b\\".to_vec()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelMode {
    /// Show panel when terminal width >= PANEL_AUTO_MIN_COLS.
    Auto,
    /// Force panel on (still hidden if terminal is absurdly narrow).
    On,
    /// Force panel off regardless of width.
    Off,
}

/// Which side panels a `/display` spec (or the `display` config value)
/// asks for. The main conversation pane is always shown — the centered
/// chat band is the layout's anchor and can't be hidden — so only the
/// left and right gutters are toggled here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneVisibility {
    pub left: bool,
    pub right: bool,
}

/// Parse a `/display` / `display` spec into the set of side panels to show.
///
/// Tokens are the pane names `left`, `main`, `right`, separated by `|`,
/// `,`, or whitespace and matched case-insensitively — e.g.
/// `left|main|right`, `main`, `main right`, `MAIN, RIGHT`. `main` is
/// accepted but has no effect on layout (the conversation always shows);
/// listing it is how a user says "only the main pane" (`/display main`).
///
/// Returns `Err` with a user-facing message on an empty spec or an
/// unrecognized token, so the caller can surface it instead of silently
/// applying a wrong layout.
pub fn parse_display_spec(spec: &str) -> Result<PaneVisibility, String> {
    let mut vis = PaneVisibility {
        left: false,
        right: false,
    };
    let mut saw_token = false;
    for tok in spec.split(['|', ',', ' ', '\t']).filter(|t| !t.is_empty()) {
        saw_token = true;
        match tok.to_ascii_lowercase().as_str() {
            "left" => vis.left = true,
            "right" => vis.right = true,
            // `main` is always shown; accept it so the user can name the
            // full layout, but it doesn't toggle anything.
            "main" => {}
            other => {
                return Err(format!(
                    "unknown pane '{other}' (use left, main, and/or right, e.g. /display left|main|right)"
                ));
            }
        }
    }
    if !saw_token {
        return Err(
            "usage: /display <panes> where panes are left|main|right (e.g. /display main|right)"
                .to_string(),
        );
    }
    Ok(vis)
}

// Re-exported from submodules so existing imports don't break.
pub use crate::ui::panel_data::{LeftPanelInfo, PanelData, SubagentStatusRow};
/// Normalized selection range — `start <= end` in row-major order.
/// Coordinates are `(buffer_line_idx, char_offset_in_line)`. Used by
/// the chat pane to apply REVERSED styling to selected cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: (usize, usize),
    pub end: (usize, usize),
}

/// Order two selection endpoints into row-major (start, end) so the
/// renderer never has to handle the upward-drag case mid-paint.
/// Word-character test for double-click select. Matches the input
/// editor's definition (alphanumeric + underscore) so selecting a word
/// behaves consistently across the chat buffer and the input box.
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

pub fn normalize_selection_range(a: (usize, usize), b: (usize, usize)) -> SelectionRange {
    if (a.0, a.1) <= (b.0, b.1) {
        SelectionRange { start: a, end: b }
    } else {
        SelectionRange { start: b, end: a }
    }
}

/// Per-chat state saved while a chat is INACTIVE. Mirrors the fields
/// the active chat uses on the `Renderer` itself; switching chats
/// swaps state in/out via `save_active` / `load_active`. Keeps the
/// hot-path rendering code unchanged — only chat-switch boundaries
/// pay the snapshot cost.
///
/// dirge-ov2 Phase A: enables multiple subagent chat windows. The
/// main session is always at index 0; subagent chats start at index 1.
/// Selection state lives per-chat because a selection in chat A would
/// be meaningless when chat B is on screen.
pub struct ChatSnapshot {
    pub name: String,
    buffer: Vec<LineEntry>,
    partial: CompactString,
    partial_color: Color,
    scroll_offset: usize,
    lines: u16,
    col: u16,
    selection_active: bool,
    selection_start: Option<(usize, usize)>,
    selection_end: Option<(usize, usize)>,
}

pub struct Renderer {
    lines: u16,
    col: u16,
    spinner_tick: bool,
    buffer: Vec<LineEntry>,
    partial: CompactString,
    partial_color: Color,
    scroll_offset: usize,
    /// dirge-ov2: snapshots of the OTHER chats — the active chat's
    /// state lives in the fields above. `chats[active_chat]` is the
    /// "free slot" (its name/buffer match what's on screen but the
    /// fields haven't been written into it yet; switching chats
    /// flushes them).
    chats: Vec<ChatSnapshot>,
    active_chat: usize,
    /// Number of rows the input area currently occupies (1 by default, grows
    /// up to MAX_INPUT_VISIBLE_LINES as the user adds newlines or types past
    /// the wrap width). The chat viewport shrinks by the same amount.
    input_rows: u16,
    monochrome: bool,
    pub selection_active: bool,
    /// Selection anchor as `(buffer_line_index, char_offset_in_line)`.
    /// Char offset is in *chars* (not bytes) so multi-byte UTF-8 glyphs
    /// behave the same as ASCII. `(line, line_len)` is a valid past-the-
    /// end position used when dragging past the line's right edge.
    pub selection_start: Option<(usize, usize)>,
    pub selection_end: Option<(usize, usize)>,
    /// Time + cell of the last mouse-down, for double-click detection
    /// (select-word). `None` until the first click or after a
    /// double-click is consumed.
    pub last_click: Option<(std::time::Instant, u16, u16)>,
    /// Set when a double-click selected a word: the following mouse-up
    /// must NOT extend/clear that selection (it would collapse the word
    /// to the click point). Consumed on the next mouse-up.
    pub suppress_next_mouseup: bool,
    /// Visibility mode for the left / right side panels, controlled
    /// independently (`/display`, `/panel`, and the `display` config).
    /// The main conversation pane is always shown.
    left_panel_mode: PanelMode,
    right_panel_mode: PanelMode,
    /// Most-recently set panel snapshot. The UI rebuilds and pushes this
    /// before each redraw so render_viewport/draw_bottom can repaint the
    /// panel along with the rest of the screen.
    panel_data: PanelData,
    /// dirge-gek: subagent task summary rows for the LEFT gutter
    /// panel. Each entry surfaces one in-flight or recently-finished
    /// subagent so the user can glance at activity without switching
    /// chat windows. Set by the UI loop on each lifecycle event;
    /// rendered above the bottom-row avatar in `draw_left_panel`.
    subagent_status: Vec<SubagentStatusRow>,
    /// ui-redesign: idle-state info for the left panel. Painted when
    /// `subagent_status` is empty so the gutter never looks dead.
    left_panel_info: LeftPanelInfo,
    /// ui-redesign Phase 6: when set, `draw_bottom` paints these
    /// lines inside the bottom frame INSTEAD of the input editor.
    /// Used by permission prompts and questionnaire prompts so the
    /// user can see the prompt without the input box obscuring it.
    /// Cleared after the ask handler resolves. Each entry is
    /// (text, color); painter centers text horizontally within the
    /// frame's inner band.
    alert_overlay: Option<Vec<(String, Color)>>,
    /// ui-redesign: title shown in the bottom-frame's top border
    /// when the alert overlay is active. Empty when no overlay (the
    /// idle input has no title, per the mockup). Caller of
    /// `set_alert_overlay` is expected to push this via
    /// `set_alert_title` so the frame label matches the prompt
    /// type (`[ALERT]`, `[QUESTION]`, etc.).
    alert_title: String,
    /// What the agent is doing — drives the bottom-left ASCII avatar.
    avatar_state: crate::ui::avatar::AvatarState,
    /// Animation flip; toggled by `tick_avatar()` so the avatar's
    /// eyes / mouth alternate between two poses per state.
    avatar_tick: bool,

    // ── ratatui migration (Phase 6) ────────────────────────────────
    /// The ratatui Terminal driving the new paint pipeline. `Option`
    /// because tests construct Renderer without a real stdout and
    /// must skip the actual draw call (the legacy paint paths kept
    /// no terminal handle either — this preserves the same testable
    /// shape).
    tui_terminal: Option<ratatui::Terminal<ratatui::backend::CrosstermBackend<BackendWriter>>>,
    /// Cached input editor snapshot used when `write_line` / `write`
    /// trigger a redraw — they don't have the editor reference at
    /// hand, but the last `draw_bottom` did. Stored as pre-wrapped
    /// rows (one per visual line) so the widget can render multi-
    /// line input without re-wrapping each frame.
    cached_input_rows: Vec<String>,
    /// Cursor row within `cached_input_rows`.
    cached_input_cursor_row: u16,
    /// Cursor column on `cached_input_rows[cached_input_cursor_row]`,
    /// in display cells.
    cached_input_cursor_col: u16,
    /// Status string from the most recent `draw_bottom` call.
    cached_status: String,
    /// `is_running` from the most recent `draw_bottom` call.
    cached_is_running: bool,
    /// Completion preview string — formatted list of upcoming
    /// slash commands from the most recent `draw_bottom` call.
    /// Empty when no tab-completion is active.
    cached_completion_preview: String,
    /// Inline dark-gray ghost completion for an in-progress slash command
    /// (e.g. typing `/dis` shows `play`). Empty when not applicable;
    /// accepted with the Right arrow.
    cached_input_ghost: String,
    /// Chat content rect from the most recent `tui_redraw` call.
    /// Used by `buffer_pos_at` to map mouse `(row, col)` into the
    /// chat buffer using the actual ratatui layout, not the legacy
    /// row-1-is-chat-top assumption. `None` until the first paint
    /// (selection events before the first frame are dropped, which
    /// matches "no drag is possible because there's nothing on
    /// screen yet").
    cached_chat_rect: Option<ratatui::layout::Rect>,

    /// dirge-b11: user-driven scroll offset into the MODIFIED
    /// sub-panel. 0 = show the most recent entries (default). Walked
    /// by mouse-wheel events when the cursor hovers inside the
    /// modified region (see `panel_modified_scroll`); persisted
    /// across redraws so a stream of agent events doesn't reset
    /// the view. Resets to 0 when the underlying list grows (a new
    /// modification arrives) so the user always sees the newest
    /// entry without scrolling back.
    pub(crate) modified_offset: usize,
    /// dirge-b11: previous MODIFIED list length, used to detect
    /// growth so we can reset `modified_offset` to 0 on the next
    /// `set_panel_data` call. `None` before the first push.
    last_modified_len: Option<usize>,
    /// dirge-b11: MODIFIED sub-panel rect from the most recent
    /// paint, used by the mouse-event handler to decide whether a
    /// scroll wheel tick should walk the modified list or fall
    /// through to chat scrolling. `None` until the first paint or
    /// when the panel is hidden.
    pub(crate) cached_modified_rect: Option<ratatui::layout::Rect>,

    #[cfg(feature = "experimental-ui-terminal-tab")]
    cached_terminal_title: String,
    #[cfg(feature = "experimental-ui-terminal-tab")]
    last_tool_name: Option<String>,
}

impl Renderer {
    pub fn new() -> io::Result<Self> {
        Ok(Renderer {
            lines: 0,
            col: 0,
            spinner_tick: false,
            buffer: Vec::new(),
            partial: CompactString::new(""),
            partial_color: Color::White,
            scroll_offset: 0,
            // dirge-ov2: one default "main" chat. Subagent chats are
            // appended via `add_chat`. Index 0 is always the main
            // session.
            chats: vec![ChatSnapshot::empty("main")],
            active_chat: 0,
            input_rows: 1,
            monochrome: false,
            selection_active: false,
            selection_start: None,
            selection_end: None,
            last_click: None,
            suppress_next_mouseup: false,
            left_panel_mode: PanelMode::Auto,
            right_panel_mode: PanelMode::Auto,
            panel_data: PanelData::default(),
            subagent_status: Vec::new(),
            left_panel_info: LeftPanelInfo::default(),
            alert_overlay: None,
            alert_title: String::new(),
            avatar_state: crate::ui::avatar::AvatarState::Idle,
            avatar_tick: false,
            // ratatui's backend writes to /dev/tty (a fresh fd
            // pointing at the controlling terminal) rather than the
            // process's stdout. With stdout/stderr redirected to
            // the log file by TerminalGuard, this is the only path
            // that can paint the screen — no rogue (print …),
            // println!, panic, or child-process output can reach
            // the TTY anymore. Falls back to stdout when /dev/tty
            // isn't available (CI tests, headless).
            tui_terminal: build_tui_terminal(),
            cached_input_rows: vec![String::new()],
            cached_input_cursor_row: 0,
            cached_input_cursor_col: 0,
            cached_status: String::new(),
            cached_is_running: false,
            cached_completion_preview: String::new(),
            cached_input_ghost: String::new(),
            cached_chat_rect: None,
            modified_offset: 0,
            last_modified_len: None,
            cached_modified_rect: None,

            #[cfg(feature = "experimental-ui-terminal-tab")]
            cached_terminal_title: String::new(),
            #[cfg(feature = "experimental-ui-terminal-tab")]
            last_tool_name: None,
        })
    }

    /// Phase 6 paint entry point. Builds a `Scene` from current
    /// Renderer state and calls `render_frame` through the ratatui
    /// Terminal. Every legacy paint method funnels here.
    ///
    /// Returns `Ok(())` (no-op) when no ratatui Terminal was
    /// initialised — keeps tests that construct `Renderer::new()`
    /// against captured stdout from blowing up on `draw`.
    pub(crate) fn tui_redraw(&mut self) -> io::Result<()> {
        use crate::ui::avatar;
        use crate::ui::tui::bottom::{AvatarSpec, BottomBody};
        use crate::ui::tui::scene::{Scene, render_frame};

        #[cfg(feature = "experimental-ui-terminal-tab")]
        let new_title = {
            let tool = self.last_tool_name.as_deref();
            format_terminal_title(self.avatar_state, tool)
        };

        // panel-visibility borrows &self via terminal_size, so compute
        // it BEFORE we take the split mutable borrow on tui_terminal.
        let show_left_panel = self.left_panel_visible();
        let show_right_panel = self.right_panel_visible();
        let frame_color = crate::ui::theme::header();

        // Split borrows on Self so we can hold &mut tui_terminal
        // and immutable references to the data fields at the same
        // time. Rust's borrow checker requires we name each field
        // we intend to read here.
        let Self {
            buffer,
            scroll_offset,
            input_rows,
            panel_data,
            left_panel_info,
            subagent_status,
            alert_overlay,
            alert_title,
            avatar_state,
            avatar_tick,
            cached_input_rows,
            cached_input_cursor_row,
            cached_input_cursor_col,
            cached_status,
            cached_is_running,
            cached_completion_preview,
            cached_input_ghost,
            cached_chat_rect,
            modified_offset,
            cached_modified_rect,
            tui_terminal,
            selection_active,
            selection_start,
            selection_end,
            ..
        } = self;

        let Some(terminal) = tui_terminal.as_mut() else {
            return Ok(());
        };

        let face = avatar::art(*avatar_state, *avatar_tick);
        let avatar_color = crate::ui::tui::chat::crossterm_to_ratatui(avatar::color(*avatar_state));
        let avatar = Some(AvatarSpec {
            face,
            color: avatar_color,
        });

        let body = if let Some(lines) = alert_overlay.as_ref() {
            BottomBody::Overlay {
                title: alert_title.as_str(),
                lines: lines.as_slice(),
            }
        } else {
            // dirge-5w9v: scroll the editor so the cursor's wrapped row
            // stays visible once the content exceeds the capped box
            // height. The painter draws from row 0 and `.take()`s the
            // window, so without this the newest/cursor lines fell off
            // the bottom and the user's typing appeared to vanish.
            let completion_extra = if cached_completion_preview.is_empty() {
                0
            } else {
                1
            };
            let window = (*input_rows as usize)
                .saturating_sub(completion_extra)
                .max(1);
            let offset = editor_scroll_offset(
                cached_input_rows.len(),
                *cached_input_cursor_row as usize,
                window,
            );
            BottomBody::Editor {
                rows: &cached_input_rows[offset..],
                cursor_row: cached_input_cursor_row.saturating_sub(offset as u16),
                cursor_col: *cached_input_cursor_col,
                is_running: *cached_is_running,
                completion_preview: cached_completion_preview.as_str(),
                ghost: cached_input_ghost.as_str(),
            }
        };

        // Size the input box to fit the overlay (or, for the
        // editor, the wrapped editor row count). For overlays we
        // bypass MAX_INPUT_VISIBLE_LINES because the user
        // **must** see the action keys row regardless of how
        // long the alert body is — clipping at 8 was hiding
        // [y]/[a]/[n]/[ESC]. The chat shrinks to accommodate, with
        // a floor of 4 rows so the user still sees recent context
        // above the alert. The editor stays clamped at MAX so the
        // user can't accidentally crowd the chat by pasting a 50-
        // line block.
        let (cols_q, rows_q) = crate::ui::terminal::tty_size();
        let effective_input_rows = if let Some(lines) = alert_overlay.as_ref() {
            let probe = crate::ui::tui::layout::Layout::with_panels(
                cols_q,
                rows_q,
                1,
                show_left_panel,
                show_right_panel,
            );
            let wrapped =
                crate::ui::tui::bottom::overlay_wrapped_row_count(lines, probe.input_box.width);
            // Leave at least 4 rows for the chat (+ 5 fixed rows
            // of frames/status), so input_rows ≤ rows - 9.
            let ceiling = (rows_q as i32 - 9).max(1) as u16;
            (wrapped as u16).clamp(1, ceiling)
        } else {
            *input_rows
        };

        // Compute the layout once so we can stash the chat rect for
        // mouse-coordinate mapping (selection::handle reads
        // cached_chat_rect to translate row/col → buffer line/char).
        // render_frame computes its own from the frame's area, but
        // with the same `(cols, rows, effective_input_rows)` inputs
        // they're identical. The terminal::size() probe used here
        // matches what render_frame sees because both go through the
        // same /dev/tty winsize.
        let layout_now = crate::ui::tui::layout::Layout::with_panels(
            cols_q,
            rows_q,
            effective_input_rows,
            show_left_panel,
            show_right_panel,
        );
        let chat_rect_now = layout_now.chat;
        *cached_chat_rect = Some(chat_rect_now);

        // dirge-b11: compute the MODIFIED sub-panel rect from the
        // current layout + panel data so the mouse handler can do
        // hit-testing before the next paint. Also clamp the offset
        // here so a list that shrunk since the last redraw doesn't
        // leave the offset stranded past the visible window.
        // Mirrors the math in `RightPanel::render` — kept in sync
        // via the shared `compute_modified_rect` helper.
        let modified_rect_now = if show_right_panel && layout_now.right_panel.width >= 16 {
            crate::ui::tui::panels::compute_modified_rect(panel_data, layout_now.right_panel)
        } else {
            None
        };
        *cached_modified_rect = modified_rect_now;
        if let Some(r) = modified_rect_now {
            let inner_rows = (r.height as usize).saturating_sub(2);
            let head_rows = inner_rows.saturating_sub(1).max(1);
            let total = panel_data.modified.len();
            let max_off = total.saturating_sub(head_rows);
            if *modified_offset > max_off {
                *modified_offset = max_off;
            }
        } else {
            *modified_offset = 0;
        }

        let chat_selection = if *selection_active {
            match (*selection_start, *selection_end) {
                (Some(s), Some(e)) => Some(normalize_selection_range(s, e)),
                _ => None,
            }
        } else {
            None
        };

        let scene = Scene {
            chat_buffer: buffer,
            scroll_offset: *scroll_offset,
            input_rows: effective_input_rows,
            chat_selection,
            panel_data,
            modified_offset: *modified_offset,
            left_info: left_panel_info,
            subagents: subagent_status,
            avatar,
            body,
            status: cached_status.as_str(),
            show_left_panel,
            show_right_panel,
            frame_color,
        };

        // Wrap the draw in Begin/EndSynchronizedUpdate. Modern
        // terminals (iTerm2, kitty, foot, recent xterm, Windows
        // Terminal) buffer the bracketed escape sequences and
        // present the resulting frame atomically — eliminates the
        // flicker we'd otherwise see as ratatui emits one escape
        // per changed cell sequentially. Terminals that don't
        // implement the sequence ignore it (it's a private DECSET
        // ?2026), so the bracket is harmless backwards-compat.
        use crossterm::ExecutableCommand as _;
        use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
        let mut stdout = std::io::stdout();
        // Guard synchronization brackets so they never leak escape
        // codes into non-TTY stdout (e.g. `cargo test`, CI logs,
        // redirected output). Terminals ignore unsupported DECSET
        // sequences, but the raw bytes in test output / logs are
        // noise.
        let sync = std::io::IsTerminal::is_terminal(&stdout);
        if sync {
            let _ = stdout.execute(BeginSynchronizedUpdate);
        }
        let draw_result = terminal.draw(|f| render_frame(&scene, f));
        if sync {
            let _ = stdout.execute(EndSynchronizedUpdate);
        }
        draw_result?;

        #[cfg(feature = "experimental-ui-terminal-tab")]
        {
            if new_title != self.cached_terminal_title {
                self.cached_terminal_title.clone_from(&new_title);
                let osc = osc_set_title(&new_title);
                let _ = terminal.backend_mut().write_all(&osc);
            }
        }

        Ok(())
    }

    /// dirge-ov2: append a new chat (typically a subagent) with the
    /// supplied display name. Returns the new chat's index, which the
    /// caller stores so it can target events at this chat later via
    /// `switch_chat`.
    ///
    /// The new chat starts empty — no buffer entries, no selection,
    /// no scroll. Does NOT switch to it; the caller chooses when to
    /// surface the new chat in the UI.
    pub fn add_chat(&mut self, name: impl Into<String>) -> usize {
        self.chats.push(ChatSnapshot::empty(name.into()));
        self.chats.len() - 1
    }

    /// dirge-ov2: switch the active chat. Saves the current chat's
    /// state to its snapshot, loads the target chat's snapshot into
    /// the Renderer's hot fields, and triggers a viewport repaint via
    /// the next render call. No-op if `idx == active_chat`.
    pub fn switch_chat(&mut self, idx: usize) {
        if idx == self.active_chat || idx >= self.chats.len() {
            return;
        }
        self.save_active();
        self.active_chat = idx;
        self.load_active();
    }

    /// Cycle to the next chat (wraps from last → first).
    /// No-op when there's only one chat.
    #[allow(dead_code)]
    pub fn next_chat(&mut self) {
        if self.chats.len() <= 1 {
            return;
        }
        let next = if self.active_chat + 1 >= self.chats.len() {
            0
        } else {
            self.active_chat + 1
        };
        self.switch_chat(next);
    }

    /// Cycle to the previous chat (wraps from first → last).
    /// No-op when there's only one chat.
    #[allow(dead_code)]
    pub fn prev_chat(&mut self) {
        if self.chats.len() <= 1 {
            return;
        }
        let prev = if self.active_chat == 0 {
            self.chats.len() - 1
        } else {
            self.active_chat - 1
        };
        self.switch_chat(prev);
    }

    /// Remove a chat by index. The active chat is adjusted:
    /// - If `idx < active`, active shifts down by 1.
    /// - If `idx == active`, moves to idx (which becomes the next
    ///   chat after removal) or wraps to 0 if at the end.
    /// - If `idx > active`, active stays unchanged.
    ///
    /// Refuses to remove the last remaining chat.
    pub fn remove_chat(&mut self, idx: usize) {
        if self.chats.len() <= 1 || idx >= self.chats.len() {
            return;
        }
        self.chats.remove(idx);
        if idx < self.active_chat {
            self.active_chat -= 1;
        } else if idx == self.active_chat && self.active_chat >= self.chats.len() {
            self.active_chat = 0;
        }
    }

    pub fn active_chat(&self) -> usize {
        self.active_chat
    }

    pub fn chat_count(&self) -> usize {
        self.chats.len()
    }

    pub fn chat_names(&self) -> Vec<String> {
        // Active chat's name lives in `chats[active_chat]` too (kept
        // in sync at add-time; mutations of the active chat's name
        // would go through a dedicated setter if added later).
        self.chats.iter().map(|c| c.name.clone()).collect()
    }

    /// dirge-ov2: snapshot the current hot fields into the active
    /// chat's slot. Called before switching chats and when the
    /// caller wants a consistent persistent state (e.g. session
    /// save).
    fn save_active(&mut self) {
        let slot = &mut self.chats[self.active_chat];
        slot.buffer = std::mem::take(&mut self.buffer);
        slot.partial = std::mem::take(&mut self.partial);
        slot.partial_color = self.partial_color;
        slot.scroll_offset = self.scroll_offset;
        slot.lines = self.lines;
        slot.col = self.col;
        slot.selection_active = self.selection_active;
        slot.selection_start = self.selection_start;
        slot.selection_end = self.selection_end;
    }

    /// dirge-ov2: load the active chat's snapshot into the hot
    /// fields. Inverse of `save_active`. Called after `switch_chat`
    /// updates `active_chat`.
    fn load_active(&mut self) {
        let slot = &mut self.chats[self.active_chat];
        self.buffer = std::mem::take(&mut slot.buffer);
        self.partial = std::mem::take(&mut slot.partial);
        self.partial_color = slot.partial_color;
        self.scroll_offset = slot.scroll_offset;
        self.lines = slot.lines;
        self.col = slot.col;
        self.selection_active = slot.selection_active;
        self.selection_start = slot.selection_start;
        self.selection_end = slot.selection_end;
    }
}

impl ChatSnapshot {
    fn empty(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            buffer: Vec::new(),
            partial: CompactString::new(""),
            partial_color: Color::White,
            scroll_offset: 0,
            lines: 0,
            col: 0,
            selection_active: false,
            selection_start: None,
            selection_end: None,
        }
    }
}

impl Renderer {
    #[allow(dead_code)]
    fn _ov2_phase_a_anchor() {}

    /// dirge-ov2 Phase E: append a line to a SPECIFIC chat's buffer
    /// without disturbing the active chat's on-screen state. If
    /// `idx` is the active chat, falls through to the regular
    /// `write_line` so the line is also painted to stdout. For
    /// inactive chats the line is pushed to the snapshot's buffer
    /// only — visible the next time the user switches to that
    /// chat via Ctrl-N/P/X.
    pub fn write_line_to_chat(&mut self, idx: usize, text: &str, color: Color) -> io::Result<()> {
        if idx == self.active_chat {
            return self.write_line(text, color);
        }
        if let Some(slot) = self.chats.get_mut(idx) {
            for line in text.split('\n') {
                slot.buffer.push(LineEntry {
                    text: CompactString::from(line),
                    color,
                });
                slot.lines = slot.lines.saturating_add(1);
            }
        }
        Ok(())
    }

    /// Update the avatar state and trigger a repaint of the bottom-left
    /// pixels. Cheap when the state hasn't changed — only the existing
    /// 3-row × 5-col patch is re-drawn.
    pub fn set_avatar_state(&mut self, state: crate::ui::avatar::AvatarState) {
        if self.avatar_state != state {
            self.avatar_state = state;
        }
    }

    #[cfg(feature = "experimental-ui-terminal-tab")]
    pub fn set_last_tool_name(&mut self, name: &str) {
        self.last_tool_name = if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        };
    }

    /// Set BOTH side panels to the same mode (the `/panel on|off|auto`
    /// command and any caller that toggles the sidebar as a unit).
    pub fn set_panel_mode(&mut self, mode: PanelMode) {
        self.left_panel_mode = mode;
        self.right_panel_mode = mode;
    }

    /// Apply a parsed `/display` selection (or the `display` config
    /// value): each listed side panel is forced on, each omitted one
    /// forced off — an explicit user choice, so `On`/`Off` rather than
    /// `Auto`.
    pub fn set_pane_visibility(&mut self, vis: PaneVisibility) {
        self.left_panel_mode = if vis.left {
            PanelMode::On
        } else {
            PanelMode::Off
        };
        self.right_panel_mode = if vis.right {
            PanelMode::On
        } else {
            PanelMode::Off
        };
    }

    pub fn left_panel_mode(&self) -> PanelMode {
        self.left_panel_mode
    }

    /// dirge-gek: replace the subagent panel data. UI loop calls this
    /// on each subagent lifecycle event (Spawn / Complete / Failed)
    /// and on Ctrl-N/P chat switch so the panel reflects current
    /// state. Cheap — just swaps the Vec; the next `render_viewport`
    /// repaints the gutter.
    pub fn set_subagent_status(&mut self, rows: Vec<SubagentStatusRow>) {
        self.subagent_status = rows;
    }

    /// ui-redesign: set the idle-state info shown in the left panel
    /// (DIRGE logo + agent metadata). The UI loop calls this at
    /// session start + on `/model` / `/prompt` switches so the
    /// gutter stays current.
    pub fn set_left_panel_info(&mut self, info: LeftPanelInfo) {
        self.left_panel_info = info;
    }

    /// ui-redesign Phase 6: set the alert overlay. While `Some`, the
    /// `[ALERT]` frame contains the supplied lines instead of the
    /// input editor. The ask handler builds the lines, pushes them
    /// here on prompt-open, and calls `clear_alert_overlay` on
    /// response.
    ///
    /// Lines are painted centered horizontally within the frame's
    /// inner band. Caller is responsible for keeping line count
    /// within `MAX_INPUT_VISIBLE_LINES` — taller overlays clip.
    pub fn set_alert_overlay(&mut self, rows: Vec<(String, Color)>) {
        self.alert_overlay = Some(rows);
        if self.alert_title.is_empty() {
            self.alert_title = "[ALERT]".to_string();
        }
    }

    pub fn clear_alert_overlay(&mut self) {
        self.alert_overlay = None;
        self.alert_title.clear();
    }

    pub fn set_panel_data(&mut self, data: PanelData) {
        // dirge-b11: when the MODIFIED list GROWS (a new file
        // modification just entered the tracker) reset the user's
        // scroll offset so they immediately see the newest entry.
        // Shrinkage (entries pruned out the back at 256-cap) leaves
        // the offset alone; the render-time clamp handles the case
        // where the offset would otherwise point past the end of
        // the list. First push (last_modified_len is None) is not a
        // growth event.
        let new_len = data.modified.len();
        if let Some(prev) = self.last_modified_len
            && new_len > prev
        {
            self.modified_offset = 0;
        }
        self.last_modified_len = Some(new_len);
        self.panel_data = data;
    }

    /// dirge-b11: walk the MODIFIED sub-panel scroll offset by
    /// `delta` lines. Positive = older (offset increases), negative
    /// = newer. No-op when the list is shorter than `visible_rows`.
    /// Clamps so the offset can't strand past the end of the list —
    /// `offset.clamp(0, list_len.saturating_sub(visible_rows))`.
    /// Returns true when the offset actually changed so the caller
    /// can decide whether to repaint.
    pub fn panel_modified_scroll(&mut self, delta: isize, visible_rows: usize) -> bool {
        let total = self.panel_data.modified.len();
        if total <= visible_rows {
            // List fits — nothing to scroll. Reset just in case the
            // user had scrolled the list when it was longer.
            let was = self.modified_offset;
            self.modified_offset = 0;
            return was != 0;
        }
        let max_off = total.saturating_sub(visible_rows);
        let prev = self.modified_offset as isize;
        let next = (prev + delta).clamp(0, max_off as isize);
        let next = next as usize;
        let changed = next != self.modified_offset;
        self.modified_offset = next;
        changed
    }

    /// Resolve a single side panel's mode against the current terminal
    /// size. Hidden when `Off`, or when the terminal is too narrow to fit
    /// both the panel and a usable content area (content_indent reflects
    /// each side's width in the centered layout, so require ~15 cols min).
    fn side_panel_visible(&self, mode: PanelMode) -> bool {
        let (cols, _) = self.terminal_size();
        match mode {
            PanelMode::Off => false,
            PanelMode::On => self.content_indent() >= 15,
            PanelMode::Auto => cols >= PANEL_AUTO_MIN_COLS && self.content_indent() >= 15,
        }
    }

    pub fn left_panel_visible(&self) -> bool {
        self.side_panel_visible(self.left_panel_mode)
    }

    pub fn right_panel_visible(&self) -> bool {
        self.side_panel_visible(self.right_panel_mode)
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.monochrome = monochrome;
    }

    fn terminal_size(&self) -> (u16, u16) {
        crate::ui::terminal::tty_size()
    }

    /// Width chat text wraps to before pushing into the buffer. Uses
    /// the *capped* `content_width()` (120 cols max) so wide terminals
    /// don't grow scrollback past the centered band into the
    /// divider/panel margin. Previously aliased `line_width()` which
    /// returns the raw band width and ignored the 120-col cap —
    /// chat overflowed the documented content area on wide terminals.
    fn max_line_width(&self) -> usize {
        self.content_width()
    }

    /// The display width the compose buffer is soft-wrapped to in the
    /// input box (content width minus the 3-col prompt prefix). Mirrors
    /// the `wrap_w` computed in `draw_bottom`; pushed into the editor so
    /// Up/Down can move by wrapped display rows (dirge-5w9v).
    pub fn input_wrap_w(&self) -> usize {
        self.content_width().saturating_sub(3).max(1)
    }

    /// Raw width of the chat band (terminal width minus 2 cols for
    /// the chat frame's left + right │). Used for *positioning*
    /// math (`content_indent`, panel widths) — chat text wrapping
    /// should go through `max_line_width` / `content_width` so it
    /// honors the 120-col cap.
    pub fn line_width(&self) -> usize {
        let (cols, _) = self.terminal_size();
        cols.saturating_sub(2) as usize
    }

    /// Target width for chat content. Caps at 120 cols so wide
    /// terminals don't stretch chambers + chat lines into sprawling
    /// rivers of text. Matches the cap used by tool chambers.
    pub fn content_width(&self) -> usize {
        self.line_width().min(120)
    }

    /// Left padding in columns to horizontally center the chat
    /// content area (`content_width`) within the visible chat band
    /// (`line_width`). Zero when content already fills the band.
    pub fn content_indent(&self) -> usize {
        let band = self.line_width();
        let target = self.content_width();
        band.saturating_sub(target) / 2
    }

    pub fn buffer_len(&self) -> usize {
        self.buffer.len()
    }

    #[allow(dead_code)]
    pub fn buffer_lines(&self) -> Vec<&str> {
        self.buffer.iter().map(|e| e.text.as_str()).collect()
    }

    pub fn replace_from(&mut self, start: usize, lines: Vec<LineEntry>) {
        self.commit_partial();
        let old_len = self.buffer.len();
        self.buffer.truncate(start);
        self.buffer.extend(lines);
        let new_len = self.buffer.len();
        self.lines = new_len as u16;
        self.col = 0;
        self.partial.clear();
        let visible = self.visible_lines();
        let max_offset = new_len.saturating_sub(visible);
        // When the user is scrolled up, keep the view anchored to the same
        // absolute content by shifting scroll_offset to match the size delta.
        if self.scroll_offset > 0 {
            let delta = new_len as isize - old_len as isize;
            let new_offset = (self.scroll_offset as isize + delta).max(0) as usize;
            self.scroll_offset = new_offset.min(max_offset);
        } else if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
    }

    /// Number of rows reserved for chat history above the input area.
    /// Subtracts the input box (`input_rows`) and the status line (1 row).
    pub fn visible_lines(&self) -> usize {
        let (_, rows) = self.terminal_size();
        rows.saturating_sub(self.input_rows + 1 + ALERT_FRAME_ROWS + CHAT_FRAME_ROWS) as usize
    }

    /// The screen row index where the input box starts. Overlays that need
    /// to anchor *above* the input box (e.g. the file picker) should treat
    /// this as their bottom limit.
    pub fn input_top_row(&self) -> u16 {
        let (_, rows) = self.terminal_size();
        // ui-redesign: input_top sits BELOW the [ALERT] top border
        // and ABOVE the bottom border + status row. Reserve
        // input_rows for input + 1 for status + (ALERT_FRAME_ROWS - 1)
        // for the bottom border; subtracting (input_rows +
        // ALERT_FRAME_ROWS) puts us right after the top border row.
        rows.saturating_sub(self.input_rows + ALERT_FRAME_ROWS)
    }

    /// Map a screen `(row, col)` to a `(line_idx, char_col)` anchor for
    /// granular selection. Uses the ratatui chat rect cached by
    /// `tui_redraw` so the mapping matches the actual on-screen
    /// layout (including side-panel gutters on wide terminals).
    /// Falls back to legacy math when no rect has been cached yet —
    /// pre-paint events and tests that bypass `tui_redraw`.
    pub fn buffer_pos_at(&self, row: u16, col: u16) -> Option<(usize, usize)> {
        let line_idx = self.buffer_line_at_row(row)?;
        let entry = self.buffer.get(line_idx)?;
        let clean = crate::ui::ansi::strip_ansi(&entry.text);
        let chat_x = self
            .cached_chat_rect
            .map(|r| r.x)
            .unwrap_or(self.content_indent() as u16);
        let display_col = if col < chat_x {
            0
        } else {
            (col - chat_x) as usize
        };
        let char_col = display_col_to_char_index(&clean, display_col);
        Some((line_idx, char_col))
    }

    pub fn buffer_line_at_row(&self, row: u16) -> Option<usize> {
        let total = self.buffer.len();
        if total == 0 {
            return None;
        }

        // Prefer the cached chat rect (ratatui layout); fall back to
        // legacy math only when the renderer hasn't painted yet.
        let (chat_y, visible) = if let Some(rect) = self.cached_chat_rect {
            (rect.y, rect.height as usize)
        } else {
            let (_, rows) = self.terminal_size();
            let v = rows.saturating_sub(self.input_rows + 1 + ALERT_FRAME_ROWS + CHAT_FRAME_ROWS)
                as usize;
            (1, v)
        };
        if visible == 0 {
            return None;
        }

        let chat_row = row.checked_sub(chat_y)? as usize;
        if chat_row >= visible {
            return None;
        }
        let start = if self.scroll_offset == 0 {
            total.saturating_sub(visible)
        } else {
            total.saturating_sub(self.scroll_offset + visible)
        };
        let start = start.min(total.saturating_sub(visible));
        let idx = start + chat_row;
        if idx < total { Some(idx) } else { None }
    }

    /// Cached chat rect from the most recent `tui_redraw` call.
    /// `None` until the first paint.
    #[allow(dead_code)]
    pub fn chat_rect(&self) -> Option<ratatui::layout::Rect> {
        self.cached_chat_rect
    }

    /// Test-only setter for the cached chat rect. Lets unit tests
    /// (selection::handle, buffer_pos_at across rect shapes) drive
    /// the coordinate mapping without going through a full paint.
    #[cfg(test)]
    pub fn set_chat_rect_for_test(&mut self, rect: ratatui::layout::Rect) {
        self.cached_chat_rect = Some(rect);
    }

    /// Word-selection bounds (start inclusive, end exclusive, both as
    /// `(line, char)`) around a buffer position, for double-click select.
    /// Returns `None` when the position isn't on a word character (e.g.
    /// whitespace / punctuation), so a double-click on a gap selects
    /// nothing rather than a stray glyph.
    pub fn word_bounds_at(&self, pos: (usize, usize)) -> Option<((usize, usize), (usize, usize))> {
        let (line, ch) = pos;
        let entry = self.buffer.get(line)?;
        let chars: Vec<char> = crate::ui::ansi::strip_ansi(&entry.text).chars().collect();
        if chars.is_empty() {
            return None;
        }
        let i = ch.min(chars.len() - 1);
        if !is_word_char(chars[i]) {
            return None;
        }
        let mut start = i;
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        let mut end = i;
        while end + 1 < chars.len() && is_word_char(chars[end + 1]) {
            end += 1;
        }
        Some(((line, start), (line, end + 1)))
    }

    pub fn clear_selection(&mut self) {
        self.selection_active = false;
        self.selection_start = None;
        self.selection_end = None;
    }

    pub fn selected_text(&self) -> Option<String> {
        // Normalize (start, end) so start <= end in row-major order:
        // earlier row wins; same row → earlier column wins.
        let (start, end) = match (self.selection_start, self.selection_end) {
            (Some(s), Some(e)) if (s.0, s.1) <= (e.0, e.1) => (s, e),
            (Some(s), Some(e)) => (e, s),
            _ => return None,
        };
        // Markdown rendering bakes SGR escapes into `LineEntry::text`
        // (see markdown.rs:291 — inline emphasis / code spans embed
        // `\x1b[…m` directly in the line text). The selection
        // columns are user-perceived character offsets, NOT byte
        // offsets into the escape-laden source — slicing the raw
        // text would either land mid-escape or include the escape
        // in the clipboard. Strip per-row first, then index into
        // the cleaned form.
        let row_clean = |i: usize| -> Option<Vec<char>> {
            self.buffer
                .get(i)
                .map(|e| crate::ui::ansi::strip_ansi(&e.text).chars().collect())
        };
        let mut result = String::new();
        if start.0 == end.0 {
            if let Some(chars) = row_clean(start.0) {
                let lo = start.1.min(chars.len());
                let hi = end.1.min(chars.len());
                if lo < hi {
                    result.extend(&chars[lo..hi]);
                }
            }
        } else {
            if let Some(chars) = row_clean(start.0) {
                let lo = start.1.min(chars.len());
                result.extend(&chars[lo..]);
            }
            for i in (start.0 + 1)..end.0 {
                result.push('\n');
                if let Some(chars) = row_clean(i) {
                    let s: String = chars.into_iter().collect();
                    result.push_str(&s);
                }
            }
            result.push('\n');
            if let Some(chars) = row_clean(end.0) {
                let hi = end.1.min(chars.len());
                result.extend(&chars[..hi]);
            }
        }
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    fn wrap_line(&self, line: &str, max_width: usize) -> Vec<CompactString> {
        // Every plain `write_line` ultimately routes through here.
        // Centralise on `wrap::soft_wrap` so the whole UI shares one
        // wrap policy: word-aware where possible, hard-break for
        // unbreakable runs, display-width-aware (CJK/emoji),
        // preserving hard newlines. Was previously a char-chunk
        // hard wrap that broke mid-word.
        crate::ui::wrap::soft_wrap(line, max_width, "")
            .into_iter()
            .map(CompactString::new)
            .collect()
    }

    fn commit_partial(&mut self) {
        if !self.partial.is_empty() {
            let max_width = self.max_line_width();
            let c = self.partial_color;
            for chunk in self.wrap_line(&self.partial, max_width) {
                self.push_buffer_line(LineEntry {
                    text: chunk,
                    color: c,
                });
            }
            self.partial.clear();
        }
    }

    /// Append a line to the scrollback buffer. If the user is currently
    /// scrolled up (scroll_offset > 0), bumps the offset by one so the
    /// view stays anchored to the same absolute content rather than drifting
    /// forward as new lines arrive. The selection (which uses absolute
    /// indices) is unaffected.
    fn push_buffer_line(&mut self, entry: LineEntry) {
        self.buffer.push(entry);
        // Audit M10: scrollback was unbounded. A long session with
        // verbose tool output (large `grep`, repeated test runs,
        // streaming logs) could grow `buffer` until it OOM'd the
        // process. Cap at MAX_SCROLLBACK lines; when exceeded, drop
        // the oldest in a single drain (cheap relative to the
        // per-line push cost, and only fires once per overflow
        // batch). Drain in chunks of MAX/8 so we don't shift on
        // every push once at-cap. Selection indices use absolute
        // line positions; adjust selection_start / selection_end /
        // scroll_offset by the eviction count so the user's
        // visible state remains anchored to the same content.
        const MAX_SCROLLBACK: usize = 20_000;
        const DRAIN_CHUNK: usize = MAX_SCROLLBACK / 8;
        if self.buffer.len() > MAX_SCROLLBACK {
            let drop_n = DRAIN_CHUNK;
            self.buffer.drain(..drop_n);
            // Adjust absolute line indices used by selection +
            // scrolling. `lines` field tracks the same counter
            // used by selection_indices_stay_absolute_under_streaming_appends
            // — leave it as a count rather than rebasing, but DO
            // rebase selection so it points at the same surviving
            // content.
            let shift = drop_n;
            if let Some(s) = self.selection_start.as_mut() {
                s.0 = s.0.saturating_sub(shift);
            }
            if let Some(e) = self.selection_end.as_mut() {
                e.0 = e.0.saturating_sub(shift);
            }
            // scroll_offset is measured from the BOTTOM, so eviction
            // from the front doesn't change it. But if the user was
            // scrolled into the now-evicted region, clamp.
            let visible = self.visible_lines();
            let max_offset = self.buffer.len().saturating_sub(visible);
            if self.scroll_offset > max_offset {
                self.scroll_offset = max_offset;
            }
        }
        if self.scroll_offset > 0 {
            let visible = self.visible_lines();
            let max_offset = self.buffer.len().saturating_sub(visible);
            self.scroll_offset = (self.scroll_offset + 1).min(max_offset);
        }
    }

    pub fn is_scrolling(&self) -> bool {
        self.scroll_offset > 0
    }

    pub fn scroll_line_up(&mut self) {
        let visible = self.visible_lines();
        let max_offset = self.buffer.len().saturating_sub(visible);
        if self.scroll_offset < max_offset {
            self.scroll_offset += 1;
        }
    }

    pub fn scroll_line_down(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
        }
    }

    pub fn scroll_page_up(&mut self) {
        let visible = self.visible_lines();
        let page = visible.saturating_sub(2).max(1);
        let max_offset = self.buffer.len().saturating_sub(visible);
        self.scroll_offset = (self.scroll_offset + page).min(max_offset);
    }

    pub fn scroll_page_down(&mut self) {
        let visible = self.visible_lines();
        let page = visible.saturating_sub(2).max(1);
        if self.scroll_offset <= page {
            self.scroll_offset = 0;
        } else {
            self.scroll_offset = self.scroll_offset.saturating_sub(page);
        }
    }

    pub fn scroll_to_top(&mut self) {
        let visible = self.visible_lines();
        self.scroll_offset = self.buffer.len().saturating_sub(visible);
    }

    pub fn scroll_to_bottom(&mut self) -> io::Result<()> {
        self.scroll_offset = 0;
        self.sync_to_buffer()
    }

    fn sync_to_buffer(&mut self) -> io::Result<()> {
        self.commit_partial();
        self.col = 0;
        self.lines = self.buffer.len() as u16;
        self.render_viewport()
    }

    pub fn render_viewport(&mut self) -> io::Result<()> {
        self.tui_redraw()
    }

    pub fn write_line(&mut self, text: &str, color: Color) -> io::Result<()> {
        self.commit_partial();
        let max_width = self.max_line_width();
        for segment in text.split('\n') {
            let wrapped = self.wrap_line(segment, max_width);
            for chunk in &wrapped {
                self.push_buffer_line(LineEntry {
                    text: chunk.clone(),
                    color,
                });
            }
        }
        // ratatui path: state is mutated above; the redraw repaints
        // the full chat region (no per-line direct stdout writes,
        // no Clear(CurrentLine) wiping side-panel cols).
        if self.scroll_offset == 0 {
            self.tui_redraw()?;
        }
        Ok(())
    }

    pub fn write(&mut self, text: &str, color: Color) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let max_width = self.max_line_width();
        if max_width == 0 {
            return Ok(());
        }
        // ratatui path: token-by-token streaming just appends to the
        // partial line buffer + commits on newlines / wrap. The
        // ratatui Buffer diff handles which cells actually changed;
        // no direct stdout writes, no per-token MoveTo, no manual
        // CRLF handling, no Clear(CurrentLine) collateral on side
        // panels. Soft-wrap math stays here so wrapped-line counts
        // remain consistent with render math.
        let parts: Vec<&str> = text.split('\n').collect();
        let last = parts.len() - 1;
        for (i, segment) in parts.iter().enumerate() {
            if i < last {
                let len_before = self.buffer.len();
                self.commit_partial();
                let had_content = len_before < self.buffer.len();
                if !segment.is_empty() {
                    self.partial_color = color;
                    self.partial.push_str(segment);
                    self.commit_partial();
                } else if !had_content {
                    self.push_buffer_line(LineEntry {
                        text: CompactString::new(""),
                        color,
                    });
                }
                self.col = 0;
            } else if !segment.is_empty() {
                let chars: Vec<char> = segment.chars().collect();
                let mut idx = 0;
                while idx < chars.len() {
                    let avail = max_width.saturating_sub(self.col as usize);
                    if avail == 0 {
                        self.commit_partial();
                        self.col = 0;
                        continue;
                    }
                    let end = (idx + avail).min(chars.len());
                    let chunk: String = chars[idx..end].iter().collect();
                    self.partial_color = color;
                    self.partial.push_str(&chunk);
                    self.col = self.col.saturating_add(chunk.chars().count() as u16);
                    idx = end;
                    if idx < chars.len() {
                        self.commit_partial();
                        self.col = 0;
                    }
                }
            }
        }
        // Single redraw at the end of the streamed batch — repeated
        // tokens within the batch land in the buffer + partial, and
        // the diff engine in ratatui only emits cells that changed.
        if self.scroll_offset == 0 {
            self.tui_redraw()?;
        }
        Ok(())
    }

    pub fn clear_content(&mut self) -> io::Result<()> {
        self.buffer.clear();
        self.partial.clear();
        self.scroll_offset = 0;
        self.clear_selection();
        let mut stdout = io::stdout();
        stdout.execute(Clear(ClearType::All))?;
        stdout.execute(MoveTo(0, 0))?;
        stdout.flush()?;
        self.lines = 0;
        self.col = 0;
        Ok(())
    }

    pub fn draw_bottom(
        &mut self,
        editor: &crate::ui::input::InputEditor,
        status: &str,
        is_running: bool,
    ) -> io::Result<()> {
        // Use the editor's display projection so paste markers
        // (`\x01<idx>\x01` blocks) appear as `[N lines pasted]`
        // placeholders rather than bare digits between invisible
        // SOH bytes. `display()` also maps the cursor byte into
        // the projected string.
        // When Ctrl+R reverse-i-search is active, show the search
        // mini-buffer instead of the normal editor buffer.
        let (display_buf, cursor_byte) = if editor.is_in_search() {
            editor.search_display()
        } else {
            editor.display()
        };
        let full = display_buf.as_str();
        let cursor_byte = cursor_byte.min(full.len());
        // Wrap to chat-content width minus 3 cols of prompt prefix.
        let wrap_w = self.content_width().saturating_sub(3).max(1);
        let (rows, cursor_row, cursor_col) = wrap_editor(full, cursor_byte, wrap_w);
        let total_rows = rows.len() as u16;
        self.cached_input_rows = rows;
        self.cached_input_cursor_row = cursor_row;
        self.cached_input_cursor_col = cursor_col;
        // Inline ghost completion: only when the cursor is at the very end
        // of an in-progress slash command (so the ghost paints right after
        // the typed text and Right-to-accept is unambiguous).
        self.cached_input_ghost = if cursor_byte == full.len() {
            crate::ui::slash::ghost_suffix(full).unwrap_or_default()
        } else {
            String::new()
        };
        self.cached_status = status.to_string();
        self.cached_is_running = is_running;
        self.input_rows = total_rows.clamp(1, MAX_INPUT_VISIBLE_LINES as u16);

        // Build slash-command completion preview if active.
        #[cfg(feature = "experimental-ui-tab-slash")]
        {
            self.cached_completion_preview =
                crate::ui::slash::format_completion_preview(editor.completion.as_ref(), wrap_w);
        }
        #[cfg(not(feature = "experimental-ui-tab-slash"))]
        {
            self.cached_completion_preview = String::new();
        }
        let completion_extra: u16 = if self.cached_completion_preview.is_empty() {
            0
        } else {
            1
        };
        self.input_rows = (total_rows + completion_extra).clamp(1, MAX_INPUT_VISIBLE_LINES as u16);

        if is_running {
            self.spinner_tick = !self.spinner_tick;
            self.avatar_tick = !self.avatar_tick;
        }

        self.tui_redraw()
    }
}

/// One visible row of the input box after soft-wrapping. A logical line
/// (between newlines in the buffer) may produce multiple visual rows when
/// it exceeds the terminal's wrap width.
///
/// Currently unused by production code (the ratatui BottomStrip renders
/// one input row only). Kept because multi-row input is the next likely
/// feature to land — re-using this `wrap_input` + tests means we don't
/// have to re-derive the cursor-placement-at-wrap-boundary logic.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisualRow {
    pub logical_line: usize,
    pub char_start: usize,
    pub char_end: usize,
}

/// Wrap pre-rendered display lines to `wrap_width` columns and locate the
/// cursor in the resulting visual grid. Returns `(rows, cursor_row, cursor_col)`.
///
/// Cursor placement at exact wrap boundaries (cursor sits at end-of-line
/// where chars exactly fill the row) keeps the cursor at the right edge of
/// the filled row rather than jumping to an empty phantom row beneath it,
/// matching what most line editors do.
#[allow(dead_code)]
pub(crate) fn wrap_input(
    display_lines: &[String],
    cursor_line_idx: usize,
    cursor_display_col: usize,
    wrap_width: usize,
) -> (Vec<VisualRow>, usize, usize) {
    let wrap_width = wrap_width.max(1);
    let mut rows: Vec<VisualRow> = Vec::new();
    let mut cursor_visual_row = 0usize;
    let mut cursor_visual_col = 0usize;

    for (li, line) in display_lines.iter().enumerate() {
        // B3-8 (audit fix): the cursor end-of-line detection
        // previously compared `cursor_display_col == char_count`,
        // misfiring on lines containing wide chars (CJK / emoji)
        // because col is a DISPLAY column and char_count is a
        // CHAR count. For a line like "日本" with cursor at the
        // end, col=4 (display cells) but char_count=2 — the
        // comparison failed and the cursor wrapped to row 1.
        // Compare against the line's display WIDTH instead.
        //
        // Row count and char_start/char_end slicing remain in
        // CHAR units (callers slice the chars vector). For pure
        // ASCII this is equivalent. Lines with wide chars + soft-
        // wrap can still split mid-double-width but the cursor
        // position math is correct.
        use unicode_width::UnicodeWidthStr;
        let char_count = line.chars().count();
        let display_width = UnicodeWidthStr::width(line.as_str());
        let row_count = if char_count == 0 {
            1
        } else {
            char_count.div_ceil(wrap_width)
        };

        let base = rows.len();
        let mut emitted = row_count;

        if li == cursor_line_idx {
            let col = cursor_display_col;
            let (vr, vc) = if col > 0 && col == display_width && col.is_multiple_of(wrap_width) {
                // End of a line that exactly fills the last row — stay on
                // the filled row, position cursor past its last char.
                (col / wrap_width - 1, wrap_width)
            } else {
                (col / wrap_width, col % wrap_width)
            };
            cursor_visual_row = base + vr;
            cursor_visual_col = vc;
            // Empty or short logical line still needs a row for the cursor.
            if vr + 1 > emitted {
                emitted = vr + 1;
            }
        }

        for r in 0..emitted {
            let cs = (r * wrap_width).min(char_count);
            let ce = ((r + 1) * wrap_width).min(char_count);
            rows.push(VisualRow {
                logical_line: li,
                char_start: cs,
                char_end: ce,
            });
        }
    }

    (rows, cursor_visual_row, cursor_visual_col)
}

/// B3-8: map a DISPLAY column on `s` to its CHAR index. ASCII-only
/// strings return `display_col` verbatim; lines containing CJK /
/// emoji compress to half the char count for full-width glyphs.
/// Clamps to the line's char count when `display_col` overshoots.
///
/// Used by `Renderer::buffer_pos_at` so mouse drag → clipboard
/// selection lines up with the visible characters on screen,
/// not the raw char positions which would mis-land in the middle
/// of double-width glyphs.
pub(crate) fn display_col_to_char_index(s: &str, display_col: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut acc = 0usize;
    for (char_idx, ch) in s.chars().enumerate() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc >= display_col {
            return char_idx;
        }
        // If adding this char's width would cross the target,
        // anchor on the boundary BEFORE the char (so a click in
        // the middle of a 2-cell glyph lands at the glyph's start,
        // not after it).
        if acc + w > display_col {
            return char_idx;
        }
        acc += w;
    }
    s.chars().count()
}

/// Truncate a string from the LEFT so the tail survives when content
/// overflows. Useful for paths where the filename matters more than
/// the prefix: `…clj/yourname/foo.rs` reads better than `src/clj/…`.
/// Returns the input verbatim when `s` fits in `max` chars.
/// Wrap the input editor's buffer into visual rows + locate the
/// cursor. Splits on `\n` (logical lines), then soft-wraps each
/// logical line to `wrap_w` display cells. Returns the wrapped
/// rows and the cursor's (row, col) position within them.
///
/// `cursor_byte` is the byte offset into `full`; conversion to
/// display cells handles multi-byte UTF-8 (the cursor column is
/// the display width of the row prefix up to the byte).
pub(crate) fn wrap_editor(
    full: &str,
    cursor_byte: usize,
    wrap_w: usize,
) -> (Vec<String>, u16, u16) {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let wrap_w = wrap_w.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut cursor_row: u16 = 0;
    let mut cursor_col: u16 = 0;
    let cursor_byte = cursor_byte.min(full.len());

    let mut byte_idx: usize = 0;
    for logical in full.split('\n') {
        let logical_start = byte_idx;
        let _logical_end = logical_start + logical.len();

        // Word-aware soft wrapping for this logical line.
        let mut cur = String::new();
        let mut cur_w: usize = 0;
        let mut local_byte: usize = 0;

        for ch in logical.chars() {
            let w = ch.width().unwrap_or(0);
            if cur_w + w > wrap_w && !cur.is_empty() {
                // Find last whitespace to break at a word boundary.
                let break_at = cur.rfind([' ', '\t']);
                match break_at {
                    Some(ws_idx) => {
                        // Word-boundary break.  Split at the whitespace:
                        // prefix stays on this row, suffix (whitespace +
                        // trailing text) moves to the continuation row.
                        let prefix: String = cur[..ws_idx].to_string();
                        let suffix: String = cur[ws_idx..].trim_start().to_string();

                        let row_start = logical_start + local_byte - cur.len();
                        let row_end = row_start + prefix.len();
                        rows.push(prefix);
                        if cursor_byte >= row_start && cursor_byte <= row_end {
                            cursor_row = rows.len() as u16 - 1;
                            cursor_col =
                                UnicodeWidthStr::width(&full[row_start..cursor_byte.min(row_end)])
                                    as u16;
                        }
                        // Start continuation row with the dangling suffix.
                        cur = suffix;
                        cur_w = UnicodeWidthStr::width(cur.as_str());
                    }
                    None => {
                        // No whitespace — a single token is wider than the
                        // row budget.  Fall back to character-level break.
                        let row_start = logical_start + local_byte - cur.len();
                        let row_end = row_start + cur.len();
                        rows.push(std::mem::take(&mut cur));
                        if cursor_byte >= row_start && cursor_byte <= row_end {
                            cursor_row = rows.len() as u16 - 1;
                            cursor_col =
                                UnicodeWidthStr::width(&full[row_start..cursor_byte.min(row_end)])
                                    as u16;
                        }
                        cur_w = 0;
                    }
                }
            }
            cur.push(ch);
            cur_w += w;
            local_byte += ch.len_utf8();
        }

        // Remaining characters on this logical line form the last row.
        let row_start = logical_start + local_byte - cur.len();
        let row_end = logical_start + local_byte;
        rows.push(cur);
        if cursor_byte >= row_start && cursor_byte <= row_end {
            cursor_row = rows.len() as u16 - 1;
            cursor_col = UnicodeWidthStr::width(&full[row_start..cursor_byte.min(row_end)]) as u16;
        }

        // Advance past this logical line + the '\n'.
        byte_idx += logical.len() + 1;
    }

    if rows.is_empty() {
        rows.push(String::new());
    }
    (rows, cursor_row, cursor_col)
}

/// Top scroll offset for the editor box so the cursor's wrapped row
/// stays visible within a `window`-row viewport (dirge-5w9v). Returns
/// the index of the first row to paint. `0` when everything fits.
///
/// Pre-fix the painter always drew from row 0 and `.take(window)`'d, so
/// once the wrapped content exceeded the capped box height the newest /
/// cursor lines fell off the bottom and the user's typing "vanished".
pub(crate) fn editor_scroll_offset(total_rows: usize, cursor_row: usize, window: usize) -> usize {
    if window == 0 || total_rows <= window {
        return 0;
    }
    let max_offset = total_rows - window;
    // Scroll just enough to land the cursor on the last visible row when
    // it's past the window; clamp so we never scroll past the end.
    cursor_row.saturating_sub(window - 1).min(max_offset)
}

// Used by the legacy modified-files panel; the new SubPanel widget
// doesn't truncate paths the same way (set_stringn clips at width).
// Kept because multi-line input wrap will likely need a similar
// shortening helper once it lands.
#[allow(dead_code)]
fn left_truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    // Reserve 1 char for the leading `…`; keep the last `max-1` chars.
    let start = chars.len() - (max - 1);
    let mut out = String::with_capacity(max);
    out.push('…');
    out.extend(&chars[start..]);
    out
}

pub fn copy_to_clipboard(text: &str) {
    let cmds: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("pbcopy", &[]),
        ("clip.exe", &[]),
    ];
    for &(cmd, args) in cmds {
        if let Ok(mut child) = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
                let _ = stdin.flush();
            }
            // Bounded wait so a wedged helper (broken XWayland,
            // frozen compositor, missing $DISPLAY for xclip) can't
            // freeze the TUI on a copy keystroke. ~2s is generous —
            // a healthy `pbcopy`/`wl-copy`/`xclip` returns in ms.
            // On expiry we SIGKILL the child and move on; the user
            // sees no immediate feedback but the editor stays
            // responsive.
            const CLIP_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_millis(2000);
            let poll_interval = std::time::Duration::from_millis(25);
            let deadline = std::time::Instant::now() + CLIP_WAIT_LIMIT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            // Reap the now-killed child so we don't
                            // leave a zombie behind. Ignore errors —
                            // best-effort cleanup.
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(poll_interval);
                    }
                    Err(_) => break,
                }
            }
            return;
        }
    }
}

#[cfg(test)]
#[path = "renderer_tests.rs"]
mod tests;
