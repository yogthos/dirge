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

use super::resolve_color;

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
/// + bottom border. Side borders (║ ... ║) are painted on every
/// input row so the entire input area reads as one framed card,
/// matching the mockup's bottom strip.
///
/// The frame title is `[ALERT]` permanently — input text and
/// permission prompts both live INSIDE the frame.
pub const ALERT_FRAME_ROWS: u16 = 2;

/// ui-redesign: chat area is wrapped in a heavy double-line frame
/// titled `[AGENT LOG STREAM]`. Two reserved rows = top border
/// (row 0) + bottom border (row 1 + visible_lines). Side borders
/// (║ … ║) are painted at the chat-band edges on every visible
/// chat row when there's room (content_indent >= 1).
pub const CHAT_FRAME_ROWS: u16 = 2;

/// Minimum terminal width at which `PanelMode::Auto` decides to show
/// the side panels. Below this the chat is too narrow to spare any
/// margin for the AGENT STATUS / SYSTEM gutters.
const PANEL_AUTO_MIN_COLS: u16 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelMode {
    /// Show panel when terminal width >= PANEL_AUTO_MIN_COLS.
    Auto,
    /// Force panel on (still hidden if terminal is absurdly narrow).
    On,
    /// Force panel off regardless of width.
    Off,
}

/// Snapshot of the data the info panel displays. Built fresh by the UI loop
/// at each redraw because the underlying state (todos, modified files, etc.)
/// is mutated by the agent and we don't want stale reads.
#[derive(Default, Clone)]
pub struct PanelData {
    /// (server name, connected) — connected currently always true because the
    /// MCP manager drops failed connections at connect time; future health
    /// tracking can flip this to false.
    pub mcp: Vec<(String, bool)>,
    /// (server_id, short root path, ok) — ok=false for broken servers.
    pub lsp: Vec<(String, String, bool)>,
    /// (status glyph, todo text). Status is single-char shorthand
    /// like "[ ]", "[~]", "[x]" depending on the todo state.
    pub todos: Vec<(String, String)>,
    /// Recent modified file paths, shortened relative to cwd when possible.
    pub modified: Vec<String>,
    /// ui-redesign: latest system load snapshot for the
    /// [SYSTEM LOAD] sub-panel. `None` when the polling task hasn't
    /// produced a reading yet (very early startup) — painter skips
    /// the section in that case.
    pub sysload: Option<crate::ui::sysload::SysLoadSnapshot>,
}

/// dirge-gek: one row in the left-gutter subagent panel. Rendered as
/// `<status-glyph> <short-id> <truncated-prompt>` so a quick glance
/// shows what's running. The UI loop rebuilds these from
/// `bg_store.list()` on each lifecycle event and pushes via
/// `Renderer::set_subagent_status`.
#[derive(Debug, Clone, Default)]
pub struct SubagentStatusRow {
    pub id_short: String,
    pub state: String,
    pub prompt_short: String,
}

/// ui-redesign: idle-state info for the left panel. When no
/// subagents are active, the left gutter paints this card: ASCII
/// DIRGE logo + agent metadata. Updated at session-start (and on
/// `/model` switch / `/prompt` switch) by the UI loop.
#[derive(Debug, Clone, Default)]
pub struct LeftPanelInfo {
    pub agent_id: String,
    pub model: String,
    pub focus: String,
}

/// Per-chat state saved while a chat is INACTIVE. Mirrors the fields
/// the active chat uses on the `Renderer` itself; switching chats
/// swaps state in/out via `save_active` / `load_active`. Keeps the
/// hot-path rendering code unchanged — only chat-switch boundaries
/// pay the snapshot cost.
///
/// dirge-ov2 Phase A: enables multiple subagent chat windows. The
/// main session is always at index 0; subagent chats start at index
/// 1. Selection state lives per-chat because a selection in chat A
/// would be meaningless when chat B is on screen.
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
    panel_mode: PanelMode,
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
    tui_terminal:
        Option<ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>>,
    /// Cached input editor snapshot used when `write_line` / `write`
    /// trigger a redraw — they don't have the editor reference at
    /// hand, but the last `draw_bottom` did. Empty until the first
    /// `draw_bottom` call.
    cached_input_text: String,
    /// Cursor column within the input editor's first visual row.
    cached_input_cursor: u16,
    /// Status string from the most recent `draw_bottom` call.
    cached_status: String,
    /// `is_running` from the most recent `draw_bottom` call.
    cached_is_running: bool,
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
            panel_mode: PanelMode::Auto,
            panel_data: PanelData::default(),
            subagent_status: Vec::new(),
            left_panel_info: LeftPanelInfo::default(),
            alert_overlay: None,
            alert_title: String::new(),
            avatar_state: crate::ui::avatar::AvatarState::Idle,
            avatar_tick: false,
            // ratatui migration (Phase 6). Lazily initialised — if
            // stdout isn't a terminal the backend still constructs
            // successfully; `draw` is the call that would fail.
            tui_terminal: ratatui::Terminal::new(
                ratatui::backend::CrosstermBackend::new(std::io::stdout()),
            )
            .ok(),
            cached_input_text: String::new(),
            cached_input_cursor: 0,
            cached_status: String::new(),
            cached_is_running: false,
        })
    }

    /// Build a `Scene` snapshot from current renderer state.
    /// Extracted from `tui_redraw` so tests can inspect the scene
    /// without driving the actual ratatui Terminal.
    #[cfg(test)]
    pub(crate) fn build_test_scene(
        &self,
    ) -> (
        String,
        u16,
        bool,
        bool,
    ) {
        (
            self.cached_input_text.clone(),
            self.cached_input_cursor,
            self.cached_is_running,
            self.alert_overlay.is_some(),
        )
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

        // TEMP DEBUG: log every redraw + the text we'd paint into the
        // input box. Tail /tmp/dirge-debug.log to verify the runtime
        // is calling this path and the cached_input_text is updating.
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/dirge-debug.log")
        {
            use std::io::Write as _;
            let _ = writeln!(
                f,
                "tui_redraw: text={:?} cursor={} overlay={} running={}",
                self.cached_input_text,
                self.cached_input_cursor,
                self.alert_overlay.is_some(),
                self.cached_is_running
            );
        }

        // panel_visible() borrows &self via terminal_size, so compute
        // it BEFORE we take the split mutable borrow on tui_terminal.
        let show_side_panels = self.panel_visible();
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
            cached_input_text,
            cached_input_cursor,
            cached_status,
            cached_is_running,
            tui_terminal,
            ..
        } = self;

        let Some(terminal) = tui_terminal.as_mut() else {
            return Ok(());
        };

        let face = avatar::art(*avatar_state, *avatar_tick);
        let avatar_color =
            crate::ui::tui::chat::crossterm_to_ratatui(avatar::color(*avatar_state));
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
            BottomBody::Editor {
                text: cached_input_text.as_str(),
                cursor_col: *cached_input_cursor,
                is_running: *cached_is_running,
            }
        };

        let scene = Scene {
            chat_buffer: buffer,
            scroll_offset: *scroll_offset,
            input_rows: *input_rows,
            panel_data,
            left_info: left_panel_info,
            subagents: subagent_status,
            avatar,
            body,
            status: cached_status.as_str(),
            show_side_panels,
            frame_color,
        };

        let result = terminal.draw(|f| render_frame(&scene, f));
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/dirge-debug.log")
        {
            use std::io::Write as _;
            let _ = writeln!(f, "  terminal.draw result: {:?}", result.is_ok());
        }
        result?;
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

#[allow(dead_code)]
impl Renderer {
    // (continuation marker — methods below this block remain unchanged)
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

    pub fn set_panel_mode(&mut self, mode: PanelMode) {
        self.panel_mode = mode;
    }

    pub fn panel_mode(&self) -> PanelMode {
        self.panel_mode
    }

    /// dirge-gek: replace the subagent panel data. UI loop calls this
    /// on each subagent lifecycle event (Spawn / Complete / Failed)
    /// + on Ctrl-N/P chat switch so the panel reflects current
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

    /// Set both overlay content and the title shown in the frame's
    /// top border. Use this instead of `set_alert_overlay` when the
    /// prompt is something other than a generic alert (e.g.
    /// `[QUESTION]`).
    pub fn set_alert_overlay_with_title(
        &mut self,
        title: impl Into<String>,
        rows: Vec<(String, Color)>,
    ) {
        self.alert_title = title.into();
        self.alert_overlay = Some(rows);
    }

    pub fn clear_alert_overlay(&mut self) {
        self.alert_overlay = None;
        self.alert_title.clear();
    }

    pub fn alert_overlay_active(&self) -> bool {
        self.alert_overlay.is_some()
    }

    pub fn set_panel_data(&mut self, data: PanelData) {
        self.panel_data = data;
    }

    /// Whether the panel will actually be drawn given current mode and
    /// terminal size. Hidden when `Off`, or when the terminal is too narrow
    /// to fit both the panel and a usable content area.
    pub fn panel_visible(&self) -> bool {
        let (cols, _) = self.terminal_size();
        match self.panel_mode {
            PanelMode::Off => false,
            // Show side panels only when there's enough margin to
            // host them — content_indent reflects each side's width
            // in the centered layout, so require ~15 cols min.
            PanelMode::On => self.content_indent() >= 15,
            PanelMode::Auto => cols >= PANEL_AUTO_MIN_COLS && self.content_indent() >= 15,
        }
    }

    /// Content-area width in columns. With the centered chat layout,
    /// the chat fills the middle and the side panels split the
    /// remainder symmetrically — so chat width math uses the full
    /// terminal width (minus 2 for the chat ║ borders).
    fn content_cols(&self) -> u16 {
        let (cols, _) = self.terminal_size();
        cols
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.monochrome = monochrome;
    }

    fn color(&self, color: Color) -> Color {
        resolve_color(color, self.monochrome)
    }

    fn terminal_size(&self) -> (u16, u16) {
        crossterm::terminal::size().unwrap_or((80, 24))
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

    /// Raw width of the chat band (terminal width minus 2 cols for
    /// the chat frame's left + right ║). Used for *positioning*
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

    pub fn buffer_lines(&self) -> Vec<&str> {
        self.buffer.iter().map(|e| e.text.as_str()).collect()
    }

    pub fn scroll_to_line(&mut self, idx: usize) {
        let visible = self.visible_lines();
        let total = self.buffer.len();
        self.scroll_offset = total
            .saturating_sub(idx + visible)
            .min(total.saturating_sub(visible));
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
    /// granular selection. `col` is the absolute terminal column; we
    /// subtract `content_indent()` to get the char offset within the
    /// rendered line, clamped to the line's char count so dragging
    /// past the right edge anchors at the end-of-line.
    pub fn buffer_pos_at(&self, row: u16, col: u16) -> Option<(usize, usize)> {
        let line_idx = self.buffer_line_at_row(row)?;
        let entry = self.buffer.get(line_idx)?;
        // L-R3 + B3-8: the column coming in is a DISPLAY offset
        // (terminal col − indent), but `selected_text` indexes
        // the strip_ansi-ed line by CHAR. For ASCII the two are
        // equivalent; for CJK / emoji a display column maps to
        // half as many chars. Walk the visible string accumulating
        // display widths until we reach the target column; return
        // the char index at that point.
        let clean = crate::ui::ansi::strip_ansi(&entry.text);
        let indent = self.content_indent() as u16;
        let display_col = if col < indent {
            0
        } else {
            (col - indent) as usize
        };
        let char_col = display_col_to_char_index(&clean, display_col);
        Some((line_idx, char_col))
    }

    pub fn buffer_line_at_row(&self, row: u16) -> Option<usize> {
        let (_, rows) = self.terminal_size();
        let visible = rows
            .saturating_sub(self.input_rows + 1 + ALERT_FRAME_ROWS + CHAT_FRAME_ROWS)
            as usize;
        let total = self.buffer.len();
        if total == 0 {
            return None;
        }
        // ui-redesign: row 0 is the chat top frame; chat rows start
        // at row 1. Map screen row → chat-content row by subtracting
        // the frame offset; rows above the chat area have no buffer
        // line.
        let chat_row = row.checked_sub(1)? as usize;
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
        // ratatui path (Phase 6): cache the editor + status snapshot
        // so write_line / write / render_viewport can redraw later
        // without an editor reference, then trigger a full repaint.
        use unicode_width::UnicodeWidthStr;
        let full = editor.buffer.as_str();
        let cursor_byte = editor.cursor.min(full.len());
        // For single-row input the cursor's display col is the
        // display width of buffer up to the cursor byte position
        // on the FIRST logical line. Multi-line wrap is a follow-up.
        let first_line_end = full.find('\n').unwrap_or(full.len());
        let line0 = &full[..first_line_end];
        let cursor_in_line0 = cursor_byte.min(first_line_end);
        let cursor_col = UnicodeWidthStr::width(&line0[..cursor_in_line0]) as u16;

        self.cached_input_text = line0.to_string();
        self.cached_input_cursor = cursor_col;
        self.cached_status = status.to_string();
        self.cached_is_running = is_running;
        // Also update input_rows from the editor so the layout
        // accounts for multi-line input even though we only paint
        // row 0 for now. wrap_input would give the precise count;
        // approximate by `lines + 1` for newlines.
        let logical_rows = (full.matches('\n').count() as u16).saturating_add(1);
        self.input_rows = logical_rows.clamp(1, MAX_INPUT_VISIBLE_LINES as u16);

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
mod tests {
    use super::*;

    /// dirge-ov2 Phase A: chat switching saves the prior chat's
    /// buffer and selection, then loads the target chat's snapshot.
    /// Round-trip preserves content.
    #[test]
    fn chat_snapshot_save_load_roundtrip() {
        let mut r = Renderer::new().expect("renderer");
        // Default chat is "main" at index 0.
        assert_eq!(r.active_chat(), 0);
        assert_eq!(r.chat_count(), 1);
        assert_eq!(r.chat_names(), vec!["main".to_string()]);

        // Seed main chat with some content.
        r.buffer.push(LineEntry {
            text: CompactString::new("main-line-1"),
            color: Color::White,
        });
        r.scroll_offset = 5;

        // Spawn a subagent chat and switch to it.
        let sub_idx = r.add_chat("subagent-1");
        assert_eq!(sub_idx, 1);
        assert_eq!(r.chat_count(), 2);
        r.switch_chat(sub_idx);
        assert_eq!(r.active_chat(), 1);

        // Subagent chat starts empty.
        assert!(r.buffer.is_empty());
        assert_eq!(r.scroll_offset, 0);

        // Add content to the subagent chat.
        r.buffer.push(LineEntry {
            text: CompactString::new("sub-line-1"),
            color: Color::Cyan,
        });
        r.scroll_offset = 2;

        // Switch back to main — its content must be restored.
        r.switch_chat(0);
        assert_eq!(r.buffer.len(), 1);
        assert_eq!(r.buffer[0].text.as_str(), "main-line-1");
        assert_eq!(r.scroll_offset, 5);

        // Switch back to subagent — its content also restored.
        r.switch_chat(1);
        assert_eq!(r.buffer.len(), 1);
        assert_eq!(r.buffer[0].text.as_str(), "sub-line-1");
        assert_eq!(r.scroll_offset, 2);

        // Switch to same chat is a no-op.
        r.switch_chat(1);
        assert_eq!(r.buffer.len(), 1);

        // Out-of-range index is a no-op (defensive — caller bug).
        r.switch_chat(99);
        assert_eq!(r.active_chat(), 1);
    }

    /// Create a renderer with a synthetic buffer of `n` short lines so we
    /// can drive scroll/append behavior without touching a real terminal.
    fn fresh_with_lines(n: usize) -> Renderer {
        let mut r = Renderer::new().expect("renderer");
        for i in 0..n {
            r.buffer.push(LineEntry {
                text: CompactString::new(&format!("line {i}")),
                color: Color::White,
            });
        }
        r.lines = r.buffer.len() as u16;
        r
    }

    /// Absolute index of the first visible line in the current viewport,
    /// matching the formula used by `render_viewport`.
    fn view_start(r: &Renderer) -> usize {
        let visible = r.visible_lines();
        let total = r.buffer.len();
        let start = if r.scroll_offset == 0 {
            total.saturating_sub(visible)
        } else {
            total.saturating_sub(r.scroll_offset + visible)
        };
        start.min(total.saturating_sub(visible))
    }

    // Regression: previously, when the user scrolled up while output was
    // streaming, scroll_offset stayed fixed but the buffer grew — so the
    // viewport drifted forward into newer content. The fix bumps
    // scroll_offset by one per appended line so the view stays anchored to
    // the same absolute lines.
    #[test]
    fn regression_scrolled_up_view_stays_anchored_through_appends() {
        let mut r = fresh_with_lines(50);
        // Scroll up 10 lines. View start changes; record it.
        for _ in 0..10 {
            r.scroll_line_up();
        }
        let pinned_start = view_start(&r);

        // Stream in 8 new lines while the user is scrolled up.
        for i in 0..8 {
            r.push_buffer_line(LineEntry {
                text: CompactString::new(&format!("new {i}")),
                color: Color::White,
            });
        }

        // The first visible line index hasn't moved.
        assert_eq!(view_start(&r), pinned_start);
    }

    // Regression: replace_from (used by the streaming-token markdown path)
    // also has to honor the scroll anchor. If the agent's current response
    // grows (or shrinks) while the user is scrolled up viewing earlier
    // content, the earlier content must stay in view.
    #[test]
    fn regression_replace_from_keeps_view_anchored_when_scrolled_up() {
        let mut r = fresh_with_lines(50);
        for _ in 0..10 {
            r.scroll_line_up();
        }
        let pinned_start = view_start(&r);

        // Replace from line 40 with twice as many lines.
        let new_lines: Vec<LineEntry> = (0..20)
            .map(|i| LineEntry {
                text: CompactString::new(&format!("repl {i}")),
                color: Color::White,
            })
            .collect();
        r.replace_from(40, new_lines);

        assert_eq!(view_start(&r), pinned_start);

        // Now replace with FEWER lines (response got shorter via re-render).
        let shorter: Vec<LineEntry> = (0..5)
            .map(|i| LineEntry {
                text: CompactString::new(&format!("sh {i}")),
                color: Color::White,
            })
            .collect();
        // After the first replace, len = 40 + 20 = 60. Now truncate at 40,
        // extend by 5 → len = 45. delta = -15. The view should attempt to
        // stay anchored at pinned_start, clamped.
        r.replace_from(40, shorter);
        let after = view_start(&r);
        // It must NOT have drifted upward (smaller absolute index) past where
        // the user originally was; staying ≥ pinned_start - shrink-room is ok.
        assert!(
            after <= pinned_start,
            "view must not skip past anchor; was {pinned_start}, now {after}"
        );
    }

    // When the user is AT the bottom (scroll_offset == 0), new content must
    // be visible — the view follows the bottom. The anchor behavior must not
    // accidentally pin the bottom-anchored view.
    #[test]
    fn at_bottom_view_follows_new_content() {
        let mut r = fresh_with_lines(50);
        assert_eq!(r.scroll_offset, 0);

        for i in 0..5 {
            r.push_buffer_line(LineEntry {
                text: CompactString::new(&format!("new {i}")),
                color: Color::White,
            });
        }
        assert_eq!(r.scroll_offset, 0, "bottom-anchored view must stay at 0");

        let visible = r.visible_lines();
        let total = r.buffer.len();
        assert_eq!(view_start(&r), total.saturating_sub(visible));
    }

    // Selection indices are absolute and must NOT shift when content
    // streams in. Prior to the anchor fix the selection rectangle visually
    // drifted because scroll_offset stayed put while the viewport advanced;
    // now the indices are still preserved and the viewport stays anchored,
    // so the selection rectangle stays where the user dragged it.
    #[test]
    fn selection_indices_stay_absolute_under_streaming_appends() {
        let mut r = fresh_with_lines(50);
        for _ in 0..10 {
            r.scroll_line_up();
        }
        r.selection_active = true;
        r.selection_start = Some((15, 0));
        r.selection_end = Some((20, 5));

        for i in 0..7 {
            r.push_buffer_line(LineEntry {
                text: CompactString::new(&format!("new {i}")),
                color: Color::White,
            });
        }

        // Selection indices are absolute and remain untouched.
        assert_eq!(r.selection_start, Some((15, 0)));
        assert_eq!(r.selection_end, Some((20, 5)));
    }

    // Boundary: a tiny buffer where appending pushes scroll_offset past
    // max_offset. The clamp inside push_buffer_line keeps it in range.
    #[test]
    fn push_clamps_scroll_offset_to_max_when_buffer_grows() {
        let mut r = fresh_with_lines(2);
        let visible = r.visible_lines();
        // Force a non-zero offset (clamp may already prevent it on tiny
        // buffers; assert behavior either way).
        r.scroll_offset = 100;
        for _ in 0..3 {
            r.push_buffer_line(LineEntry {
                text: CompactString::new("more"),
                color: Color::White,
            });
        }
        let max_offset = r.buffer.len().saturating_sub(visible);
        assert!(
            r.scroll_offset <= max_offset,
            "scroll_offset {} must be ≤ max {}",
            r.scroll_offset,
            max_offset
        );
    }

    // Streaming via commit_partial (the path used by `write` for streamed
    // tokens) also goes through push_buffer_line. Verify the partial commit
    // bumps the offset when scrolled up.
    #[test]
    fn commit_partial_routes_through_anchor_aware_push() {
        let mut r = fresh_with_lines(50);
        for _ in 0..10 {
            r.scroll_line_up();
        }
        let pinned_start = view_start(&r);

        r.partial = CompactString::new("a streamed token chunk");
        r.partial_color = Color::White;
        r.commit_partial();

        assert_eq!(view_start(&r), pinned_start);
    }

    // --- granular selection ----------------------------------------------

    fn fresh_with_text(lines: &[&str]) -> Renderer {
        let mut r = Renderer::new().unwrap();
        for s in lines {
            r.buffer.push(LineEntry {
                text: CompactString::new(s),
                color: Color::White,
            });
        }
        r
    }

    /// Same-row selection extracts the substring between start.1 and
    /// end.1 (char-indexed, exclusive end).
    #[test]
    fn selected_text_single_row_substring() {
        let mut r = fresh_with_text(&["hello world"]);
        r.selection_active = true;
        r.selection_start = Some((0, 6));
        r.selection_end = Some((0, 11));
        assert_eq!(r.selected_text(), Some("world".to_string()));
    }

    /// Reverse drag (end before start) still yields the same substring —
    /// `selected_text` normalizes to row-major order.
    #[test]
    fn selected_text_reverse_drag_normalizes() {
        let mut r = fresh_with_text(&["hello world"]);
        r.selection_active = true;
        r.selection_start = Some((0, 11));
        r.selection_end = Some((0, 6));
        assert_eq!(r.selected_text(), Some("world".to_string()));
    }

    /// Multi-row selection takes the tail of the start row, the full
    /// middle rows, and the head of the end row.
    #[test]
    fn selected_text_multi_row_spans_lines() {
        let mut r = fresh_with_text(&["first line", "middle", "last line"]);
        r.selection_active = true;
        r.selection_start = Some((0, 6)); // "line"
        r.selection_end = Some((2, 4)); // "last"
        assert_eq!(r.selected_text(), Some("line\nmiddle\nlast".to_string()));
    }

    /// Same-row empty selection (start == end) returns None — nothing
    /// selected yet, just a click.
    #[test]
    fn selected_text_empty_selection_returns_none() {
        let mut r = fresh_with_text(&["hello"]);
        r.selection_active = true;
        r.selection_start = Some((0, 3));
        r.selection_end = Some((0, 3));
        assert!(r.selected_text().is_none());
    }

    /// Multi-byte UTF-8: char indices ignore byte width. `é` and `🦀`
    /// each count as 1 char, not their byte widths.
    #[test]
    fn selected_text_handles_unicode() {
        let mut r = fresh_with_text(&["café 🦀 rust"]);
        r.selection_active = true;
        r.selection_start = Some((0, 0));
        r.selection_end = Some((0, 6)); // "café 🦀"
        assert_eq!(r.selected_text(), Some("café 🦀".to_string()));
    }

    /// Markdown rendering bakes SGR escapes into LineEntry::text;
    /// the selection path must strip them before handing the
    /// string to the clipboard. Columns reflect user-perceived
    /// character offsets in the visible glyphs, not the
    /// escape-laden source.
    #[test]
    fn selected_text_strips_ansi_escapes() {
        // Visible text is "hello red world" (15 chars). The buffer
        // line carries `\x1b[31m` around "red".
        let mut r = fresh_with_text(&[]);
        r.buffer.clear();
        r.buffer.push(LineEntry {
            text: CompactString::from("hello \x1b[31mred\x1b[0m world"),
            color: Color::Reset,
        });
        r.selection_active = true;
        // Select the full visible content (cols 0..15).
        r.selection_start = Some((0, 0));
        r.selection_end = Some((0, 15));
        assert_eq!(r.selected_text(), Some("hello red world".to_string()));

        // Substring selection lands on clean chars too —
        // "red world" is cols 6..15 of the stripped text.
        r.selection_end = Some((0, 15));
        r.selection_start = Some((0, 6));
        assert_eq!(r.selected_text(), Some("red world".to_string()));
    }

    /// `buffer_pos_at` clamps char_col to the line's length so dragging
    /// past the right edge anchors at end-of-line rather than
    /// silently extending past visible content.
    #[test]
    fn buffer_pos_at_clamps_past_eol() {
        let r = fresh_with_text(&["short"]);
        // Row 0 is the chat top frame in the ui-redesign; row 1 is
        // the first chat content row. `buffer_line_at_row` returns
        // Some(0) for row 1 (start = 0 after saturating, idx = 0).
        let pos = r.buffer_pos_at(1, 999);
        assert_eq!(pos, Some((0, 5)));
    }

    // --- B3-8: display-width-aware column mapping --------------

    #[test]
    fn display_col_to_char_index_ascii_round_trip() {
        // ASCII: 1 char = 1 display cell. char_index == display_col.
        assert_eq!(display_col_to_char_index("hello", 0), 0);
        assert_eq!(display_col_to_char_index("hello", 3), 3);
        assert_eq!(display_col_to_char_index("hello", 5), 5);
        // Past EOL clamps to char count.
        assert_eq!(display_col_to_char_index("hello", 99), 5);
    }

    #[test]
    fn display_col_to_char_index_cjk_compresses() {
        // "日本" — 2 chars, 4 display cells.
        let s = "日本";
        assert_eq!(display_col_to_char_index(s, 0), 0);
        // Display col 1: middle of 日 — anchor to its start (char 0).
        assert_eq!(display_col_to_char_index(s, 1), 0);
        assert_eq!(display_col_to_char_index(s, 2), 1); // start of 本
        assert_eq!(display_col_to_char_index(s, 3), 1); // middle of 本
        assert_eq!(display_col_to_char_index(s, 4), 2); // EOL
        assert_eq!(display_col_to_char_index(s, 99), 2);
    }

    #[test]
    fn display_col_to_char_index_emoji() {
        // "a🦀b" — 3 chars, widths 1 + 2 + 1 = 4 cells.
        let s = "a🦀b";
        assert_eq!(display_col_to_char_index(s, 0), 0); // start
        assert_eq!(display_col_to_char_index(s, 1), 1); // start of 🦀
        assert_eq!(display_col_to_char_index(s, 2), 1); // middle of 🦀
        assert_eq!(display_col_to_char_index(s, 3), 2); // start of b
        assert_eq!(display_col_to_char_index(s, 4), 3); // EOL
    }

    /// L-R3: buffer_pos_at clamps to VISIBLE char count (post ANSI
    /// strip) not raw char count. Without this, a click far right
    /// on a styled line would clamp past the visible-text length
    /// and selected_text's slice would either return an empty
    /// string or land in the middle of the escape bytes.
    #[test]
    fn buffer_pos_at_clamps_to_visible_chars_not_raw_bytes() {
        let mut r = fresh_with_text(&[]);
        r.buffer.clear();
        // Visible: "hello red world" — 15 chars. Raw: 25 chars
        // (including 10 chars of `\x1b[31m` + `\x1b[0m` escape).
        r.buffer.push(LineEntry {
            text: CompactString::from("hello \x1b[31mred\x1b[0m world"),
            color: Color::Reset,
        });
        // Click well past the visible end. content_indent() is 0
        // in the default test renderer, so col == char_col. Row 1
        // is the first chat content row (row 0 is the chat frame).
        let pos = r.buffer_pos_at(1, 999).expect("must resolve");
        assert_eq!(pos.1, 15, "clamp should hit visible length 15, not raw 25");
    }

    // --- wrap_input -------------------------------------------------------

    fn lines(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wrap_empty_buffer_has_one_row() {
        let (rows, cr, cc) = wrap_input(&lines(&[""]), 0, 0, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].logical_line, 0);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 0));
        assert_eq!((cr, cc), (0, 0));
    }

    #[test]
    fn wrap_short_line_no_split() {
        let (rows, cr, cc) = wrap_input(&lines(&["hi"]), 0, 2, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 2));
        assert_eq!((cr, cc), (0, 2));
    }

    #[test]
    fn wrap_splits_long_line_into_multiple_visual_rows() {
        // "abcdefghi" with wrap_width=3 -> 3 rows of 3 chars each.
        let (rows, cr, cc) = wrap_input(&lines(&["abcdefghi"]), 0, 5, 3);
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 3));
        assert_eq!((rows[1].char_start, rows[1].char_end), (3, 6));
        assert_eq!((rows[2].char_start, rows[2].char_end), (6, 9));
        // cursor at col 5 -> row 1, col 2
        assert_eq!((cr, cc), (1, 2));
    }

    #[test]
    fn wrap_cursor_at_exact_boundary_stays_on_filled_row() {
        // "abc" with wrap_width=3 — cursor at col 3 (end of line). Should
        // sit at the right edge of the only row, not on a phantom row 1.
        let (rows, cr, cc) = wrap_input(&lines(&["abc"]), 0, 3, 3);
        assert_eq!(rows.len(), 1);
        assert_eq!((cr, cc), (0, 3));
    }

    #[test]
    fn wrap_cursor_after_full_row_with_continuation() {
        // "abcdef" with wrap_width=3 — cursor at col 6 (end). Two rows, cursor
        // at end of row 1 (col 3), not at start of phantom row 2.
        let (rows, cr, cc) = wrap_input(&lines(&["abcdef"]), 0, 6, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!((cr, cc), (1, 3));
    }

    #[test]
    fn wrap_cursor_at_start_of_continuation_row() {
        // "abcdef" with wrap_width=3 — cursor at col 3 (just past first row).
        // Not the exact-boundary "at end of line" case: chars continue.
        let (rows, cr, cc) = wrap_input(&lines(&["abcdef"]), 0, 3, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!((cr, cc), (1, 0));
    }

    #[test]
    fn wrap_multiple_logical_lines() {
        // Two logical lines, second one has the cursor.
        let (rows, cr, cc) = wrap_input(&lines(&["abc", "defgh"]), 1, 4, 3);
        // Line 0: 1 row (3 chars); Line 1: 2 rows (3 + 2)
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].logical_line, 0);
        assert_eq!(rows[1].logical_line, 1);
        assert_eq!(rows[2].logical_line, 1);
        // Cursor at line 1, col 4 -> within line 1's row 1 (visual row 2 overall), col 1
        assert_eq!((cr, cc), (2, 1));
    }

    #[test]
    fn wrap_empty_then_filled_line_cursor_on_empty() {
        // ["", "abc"] with cursor on line 0 at col 0.
        let (rows, cr, cc) = wrap_input(&lines(&["", "abc"]), 0, 0, 3);
        // Line 0: 1 (empty) row; Line 1: 1 row of "abc"
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 0));
        assert_eq!((rows[1].char_start, rows[1].char_end), (0, 3));
        assert_eq!((cr, cc), (0, 0));
    }

    #[test]
    fn wrap_width_one_degenerate() {
        // wrap_width=1 in extremely narrow terminal — every char becomes its
        // own row. Should not panic and cursor should still map.
        let (rows, cr, cc) = wrap_input(&lines(&["abc"]), 0, 2, 1);
        assert_eq!(rows.len(), 3);
        assert_eq!((cr, cc), (2, 0));
    }
}
