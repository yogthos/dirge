use std::io::{self, Write};

use compact_str::CompactString;
use crossterm::ExecutableCommand;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType, ScrollUp};

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

/// Width of the optional right-hand info panel content area, in columns.
/// Plus one column for the vertical divider gives `PANEL_RESERVE`.
const PANEL_WIDTH: u16 = 32;
/// Total columns the panel costs the chat area when visible (panel + divider).
const PANEL_RESERVE: u16 = PANEL_WIDTH + 1;
/// Minimum terminal width at which `PanelMode::Auto` decides to show the
/// panel. Below this the chat content would be too cramped.
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
    /// Short directory name to show at the top of the panel.
    pub cwd: String,
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
}

pub struct Renderer {
    lines: u16,
    col: u16,
    spinner_tick: bool,
    buffer: Vec<LineEntry>,
    partial: CompactString,
    partial_color: Color,
    scroll_offset: usize,
    /// Number of rows the input area currently occupies (1 by default, grows
    /// up to MAX_INPUT_VISIBLE_LINES as the user adds newlines or types past
    /// the wrap width). The chat viewport shrinks by the same amount.
    input_rows: u16,
    monochrome: bool,
    pub selection_active: bool,
    pub selection_start: Option<usize>,
    pub selection_end: Option<usize>,
    panel_mode: PanelMode,
    /// Most-recently set panel snapshot. The UI rebuilds and pushes this
    /// before each redraw so render_viewport/draw_bottom can repaint the
    /// panel along with the rest of the screen.
    panel_data: PanelData,
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
            input_rows: 1,
            monochrome: false,
            selection_active: false,
            selection_start: None,
            selection_end: None,
            panel_mode: PanelMode::Auto,
            panel_data: PanelData::default(),
        })
    }

    pub fn set_panel_mode(&mut self, mode: PanelMode) {
        self.panel_mode = mode;
    }

    pub fn panel_mode(&self) -> PanelMode {
        self.panel_mode
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
            PanelMode::On => cols >= PANEL_RESERVE + 20,
            PanelMode::Auto => cols >= PANEL_AUTO_MIN_COLS,
        }
    }

    /// Content-area width in columns: terminal width minus the panel and
    /// divider when the panel is visible. All chat/input width math uses
    /// this so wrapping/clipping respects the panel.
    fn content_cols(&self) -> u16 {
        let (cols, _) = self.terminal_size();
        if self.panel_visible() {
            cols.saturating_sub(PANEL_RESERVE)
        } else {
            cols
        }
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

    fn max_line_width(&self) -> usize {
        self.content_cols().saturating_sub(1) as usize
    }

    pub fn line_width(&self) -> usize {
        self.max_line_width()
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
        rows.saturating_sub(self.input_rows + 1) as usize
    }

    /// The screen row index where the input box starts. Overlays that need
    /// to anchor *above* the input box (e.g. the file picker) should treat
    /// this as their bottom limit.
    pub fn input_top_row(&self) -> u16 {
        let (_, rows) = self.terminal_size();
        rows.saturating_sub(self.input_rows + 1)
    }

    pub fn buffer_line_at_row(&self, row: u16) -> Option<usize> {
        let (_, rows) = self.terminal_size();
        let visible = rows.saturating_sub(self.input_rows + 1) as usize;
        let total = self.buffer.len();
        if total == 0 {
            return None;
        }
        let start = if self.scroll_offset == 0 {
            total.saturating_sub(visible)
        } else {
            total.saturating_sub(self.scroll_offset + visible)
        };
        let start = start.min(total.saturating_sub(visible));
        let idx = start + row as usize;
        if idx < total { Some(idx) } else { None }
    }

    pub fn clear_selection(&mut self) {
        self.selection_active = false;
        self.selection_start = None;
        self.selection_end = None;
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = match (self.selection_start, self.selection_end) {
            (Some(s), Some(e)) if s <= e => (s, e),
            (Some(s), Some(e)) => (e, s),
            _ => return None,
        };
        let mut result = String::new();
        for i in start..=end {
            if let Some(entry) = self.buffer.get(i) {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(&entry.text);
            }
        }
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    fn wrap_line(&self, line: &str, max_width: usize) -> Vec<CompactString> {
        // `chars.chunks(0)` panic-er ("chunk size must be non-zero"). Kan
        // skje ved oppstart i en ikke-initialisert PTY eller midt i resize.
        if max_width == 0 {
            return vec![CompactString::new(line)];
        }
        let chars: Vec<char> = line.chars().collect();
        if chars.len() <= max_width {
            return vec![CompactString::new(line)];
        }
        chars
            .chunks(max_width)
            .map(|c| CompactString::new(c.iter().collect::<String>()))
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
        let (_, rows) = self.terminal_size();
        let content_cols = self.content_cols();
        let visible = rows.saturating_sub(self.input_rows + 1) as usize;
        let total = self.buffer.len();
        let mut stdout = io::stdout();
        // Keep the cursor hidden while we paint many rows; draw_bottom is
        // the only path that re-shows it at the input position.
        stdout.execute(Hide)?;

        let start = if self.scroll_offset == 0 {
            total.saturating_sub(visible)
        } else {
            total.saturating_sub(self.scroll_offset + visible)
        };
        let start = start.min(total.saturating_sub(visible));
        let end = (start + visible).min(total);

        // Width of the content band (excluding the divider column at
        // `content_cols`). All chat text is clipped here; the remaining
        // columns belong to the divider + panel.
        let content_band = content_cols.saturating_sub(1) as usize;
        // Left indent in columns to horizontally center chat content
        // inside the band. We then clip the per-line text to
        // `content_band - indent` so wide content can't spill into
        // the divider/panel.
        let indent = self.content_indent();
        let line_cap = content_band.saturating_sub(indent);
        for i in 0..visible {
            stdout.execute(MoveTo(0, i as u16))?;
            // Paint indent spaces (no color) so any stale text on the
            // left edge from a wider previous line gets wiped.
            if indent > 0 {
                write!(stdout, "{}", " ".repeat(indent))?;
            }
            let text_chars: usize = if start + i < end {
                let entry = &self.buffer[start + i];
                let line_idx = start + i;
                let text: String = entry.text.chars().take(line_cap).collect();
                let actual_chars = text.chars().count();

                let is_selected = self.selection_active
                    && self.selection_start.is_some()
                    && self.selection_end.is_some()
                    && {
                        let s = self.selection_start.unwrap();
                        let e = self.selection_end.unwrap();
                        let lo = s.min(e);
                        let hi = s.max(e);
                        line_idx >= lo && line_idx <= hi
                    };

                if is_selected {
                    write!(stdout, "{}", SetAttribute(Attribute::Reverse))?;
                }
                // Bold attribute simulates the CRT phosphor bloom: on
                // most modern terminals it nudges the glyphs to a
                // heavier weight and a brighter shade of the chosen
                // color. We apply it to bright tones only (the dim
                // green / dim grey colors must stay un-bloomed to
                // preserve the two-tone phosphor depth shown in the
                // reference btop screenshots).
                let bloom = crate::ui::theme::is_bright(entry.color);
                if bloom {
                    write!(stdout, "{}", SetAttribute(Attribute::Bold))?;
                }
                write!(stdout, "{}", SetForegroundColor(self.color(entry.color)))?;
                write!(stdout, "{}", text)?;
                if bloom {
                    write!(stdout, "{}", SetAttribute(Attribute::NormalIntensity))?;
                }
                if is_selected {
                    write!(stdout, "{}", SetAttribute(Attribute::NoReverse))?;
                }
                write!(stdout, "{}", ResetColor)?;
                actual_chars
            } else {
                0
            };
            if self.panel_visible() {
                // Pad to fill the content band so stale chars from
                // wider previous lines get wiped. With centering,
                // the written length per row is `indent + text_chars`;
                // the trailing pad fills `content_band - (indent + text)`.
                let written = indent + text_chars;
                let pad = content_band.saturating_sub(written);
                if pad > 0 {
                    write!(stdout, "{}", " ".repeat(pad))?;
                }
            } else {
                // No panel — safe to use the cheaper clear-to-end-of-line so
                // stale chars at the very last column also get wiped.
                write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
            }
        }

        if self.scroll_offset > 0 {
            let pct = if total > visible {
                ((total - self.scroll_offset - visible) * 100 / (total - visible)).min(100)
            } else {
                0
            };
            let indicator = format!(" SCROLL {}% ", pct);
            let x = content_cols.saturating_sub(indicator.len() as u16);
            stdout.execute(MoveTo(x, 0))?;
            write!(
                stdout,
                "{}",
                SetForegroundColor(self.color(crate::ui::theme::warn()))
            )?;
            write!(stdout, "{}", indicator)?;
            write!(stdout, "{}", ResetColor)?;
        }

        if self.panel_visible() {
            self.draw_panel(&mut stdout, rows)?;
        }

        stdout.flush()?;
        Ok(())
    }

    fn ensure_room(&mut self) {
        if self.scroll_offset > 0 {
            return;
        }
        let (_, rows) = self.terminal_size();
        if rows < 3 {
            return;
        }
        // Clear only the content band so the right-hand info panel keeps
        // its pixels when the chat viewport scrolls.
        let content_cols = self.content_cols();
        let max_content = rows.saturating_sub(self.input_rows + 1);
        if self.lines >= max_content {
            let mut stdout = io::stdout();
            let _ = stdout.execute(ScrollUp(1));
            self.lines = self.lines.saturating_sub(1);
            for &r in &[max_content.saturating_sub(1), max_content] {
                let _ = stdout.execute(MoveTo(0, r));
                let _ = write!(stdout, "{}", " ".repeat(content_cols as usize));
            }
            let _ = stdout.flush();
        }
    }

    fn content_row(&self) -> u16 {
        let (_, rows) = self.terminal_size();
        self.lines.min(rows.saturating_sub(self.input_rows + 2))
    }

    pub fn write_line(&mut self, text: &str, color: Color) -> io::Result<()> {
        self.commit_partial();
        let max_width = self.max_line_width();
        let indent = self.content_indent();
        for segment in text.split('\n') {
            let wrapped = self.wrap_line(segment, max_width);
            for chunk in &wrapped {
                self.push_buffer_line(LineEntry {
                    text: chunk.clone(),
                    color,
                });
                if self.scroll_offset == 0 {
                    self.ensure_room();
                    let mut stdout = io::stdout();
                    // Hide cursor so streaming agent output doesn't drag the
                    // hardware cursor across the chat area; draw_bottom shows
                    // it again at the input prompt.
                    stdout.execute(Hide)?;
                    let r = self.content_row();
                    stdout.execute(MoveTo(0, r))?;
                    stdout.execute(Clear(ClearType::CurrentLine))?;
                    // Indent so the directly-painted line matches the
                    // centered layout that `render_viewport` produces.
                    // Without this, the streaming path writes at col 0
                    // and the chat visibly jumps from centered (after
                    // a render_viewport repaint) to left-aligned
                    // (immediately after each write).
                    if indent > 0 {
                        write!(stdout, "{}", " ".repeat(indent))?;
                    }
                    write!(stdout, "{}", SetForegroundColor(self.color(color)))?;
                    writeln!(stdout, "{}", chunk)?;
                    write!(stdout, "{}", ResetColor)?;
                    self.lines = self.lines.saturating_add(1);
                    self.col = 0;
                }
            }
        }
        if self.scroll_offset == 0 {
            io::stdout().flush()?;
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
        // Hide cursor for the duration of this streamed write; draw_bottom
        // is the only place that re-shows it at the input prompt.
        if self.scroll_offset == 0 {
            io::stdout().execute(Hide)?;
        }
        // Same centering offset render_viewport / write_line use.
        // Without this the streaming token path paints at col 0 while
        // the rest of the chat is centered — content jumps left as it
        // streams and back to center on the next full repaint.
        let indent = self.content_indent() as u16;
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
                if self.scroll_offset == 0 {
                    self.ensure_room();
                    let mut stdout = io::stdout();
                    let r = self.content_row();
                    stdout.execute(MoveTo(indent + self.col, r))?;
                    if !segment.is_empty() {
                        write!(stdout, "{}", SetForegroundColor(self.color(color)))?;
                        write!(stdout, "{}", segment)?;
                        write!(stdout, "{}", ResetColor)?;
                    }
                    writeln!(stdout)?;
                    self.lines = self.lines.saturating_add(1);
                    self.col = 0;
                }
            } else if !segment.is_empty() {
                let chars: Vec<char> = segment.chars().collect();
                let mut idx = 0;
                while idx < chars.len() {
                    let avail = max_width.saturating_sub(self.col as usize);
                    if avail == 0 {
                        self.commit_partial();
                        if self.scroll_offset == 0 {
                            self.lines = self.lines.saturating_add(1);
                            self.col = 0;
                        }
                        continue;
                    }
                    let end = (idx + avail).min(chars.len());
                    let chunk: String = chars[idx..end].iter().collect();
                    self.partial_color = color;
                    self.partial.push_str(&chunk);
                    if self.scroll_offset == 0 {
                        self.ensure_room();
                        let mut stdout = io::stdout();
                        let r = self.content_row();
                        stdout.execute(MoveTo(indent + self.col, r))?;
                        write!(stdout, "{}", SetForegroundColor(self.color(color)))?;
                        write!(stdout, "{}", chunk)?;
                        write!(stdout, "{}", ResetColor)?;
                        self.col = self.col.saturating_add(chunk.chars().count() as u16);
                    }
                    idx = end;
                    if idx < chars.len() {
                        self.commit_partial();
                        if self.scroll_offset == 0 {
                            self.lines = self.lines.saturating_add(1);
                            self.col = 0;
                        }
                    }
                }
            }
        }
        if self.scroll_offset == 0 {
            io::stdout().flush()?;
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
        let (_, rows) = crossterm::terminal::size()?;
        // Width available for input/status content. When the info panel is
        // visible this is smaller than terminal width — using it everywhere
        // keeps the prompt + status text from spilling under the panel.
        let cols = self.content_cols();
        let mut stdout = io::stdout();

        let full_input: &str = editor.buffer.as_str();
        let full_cursor: usize = editor.cursor;

        // Break the buffer into logical lines (one per `\n`). An empty buffer
        // still shows one row so the prompt is visible.
        let logical_lines: Vec<&str> = if full_input.is_empty() {
            vec![""]
        } else {
            full_input.split('\n').collect()
        };
        let line_count = logical_lines.len();

        // Determine which logical line the cursor sits on, and the byte
        // offset within it.
        let cursor_line_start = full_input[..full_cursor]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let cursor_line_idx = full_input[..cursor_line_start].matches('\n').count();
        let cursor_col_in_line = full_cursor - cursor_line_start;

        // Wrap width: content width minus the 3-char prompt prefix
        // (`▌▌ ` idle, `░▌ `/`▒▌ ` spinner, `▏  ` continuation — all 3
        // columns) that sits in front of every visible row. Min 1 to
        // avoid div-by-zero in wrap math on ridiculous terminal sizes.
        let wrap_width = (cols.saturating_sub(3) as usize).max(1);

        // Pre-render each logical line (placeholder expansion etc.) into the
        // displayable form, alongside the cursor's display column on its line.
        let display_lines: Vec<String> = logical_lines
            .iter()
            .map(|line| editor.render_line(line, 0).0)
            .collect();
        let cursor_raw_line = logical_lines[cursor_line_idx];
        let (_, cursor_display_col) = editor.render_line(cursor_raw_line, cursor_col_in_line);

        // Soft-wrap to visual rows. Each visual row spans at most
        // `wrap_width` chars of one logical line; long lines emit multiple
        // visual rows. Cursor mapping accounts for end-of-line at the exact
        // wrap boundary (cursor stays at the right edge instead of jumping
        // to a phantom next row with no content under it).
        let (visual_rows, cursor_visual_row, cursor_visual_col) = wrap_input(
            &display_lines,
            cursor_line_idx,
            cursor_display_col,
            wrap_width,
        );
        let total_visual = visual_rows.len();
        let visible_input_rows = total_visual.clamp(1, MAX_INPUT_VISIBLE_LINES);

        // Window the visual rows so the cursor stays on screen.
        let first_visible_visual = if cursor_visual_row >= visible_input_rows {
            cursor_visual_row + 1 - visible_input_rows
        } else {
            0
        };

        // If the bottom panel grew or shrank, the chat viewport needs to be
        // repainted at its new size before we overwrite the bottom rows.
        let new_input_rows = visible_input_rows as u16;
        if new_input_rows != self.input_rows {
            self.input_rows = new_input_rows;
            self.render_viewport()?;
        }

        let status_row = rows.saturating_sub(1);
        let input_top = rows.saturating_sub(self.input_rows + 1);
        // Heavy block prompt indicator. While running, we tick a 4-stage
        // gradient through the block characters to suggest a phosphor
        // pulse — readable without becoming distracting.
        let prompt_main = if is_running {
            self.spinner_tick = !self.spinner_tick;
            // Two-state spinner using ░/▒ blocks so the eye registers
            // motion at slow refresh rates.
            if self.spinner_tick {
                "▒▌ "
            } else {
                "░▌ "
            }
        } else {
            "▌▌ "
        };
        // Continuation rows show a fainter single-block guide so wrapped
        // text reads as a vertical "chamber" attached to the prompt.
        let prompt_cont = "▏  ";

        // Pre-collect chars per logical line so each visual row's slice can
        // be cut without re-iterating.
        let display_chars: Vec<Vec<char>> =
            display_lines.iter().map(|s| s.chars().collect()).collect();

        // Input rows + status row also center under the chat band so
        // the prompt + status visually align with the chat content
        // above.
        let bottom_indent = self.content_indent();
        for row_offset in 0..visible_input_rows {
            let row = input_top + row_offset as u16;
            let vr_idx = first_visible_visual + row_offset;
            stdout.execute(MoveTo(0, row))?;
            write!(stdout, "{}", " ".repeat(cols as usize))?;
            stdout.execute(MoveTo(bottom_indent as u16, row))?;
            write!(
                stdout,
                "{}",
                SetForegroundColor(self.color(crate::ui::theme::accent()))
            )?;
            let is_prompt_row = vr_idx == 0;
            if is_prompt_row {
                write!(stdout, "{}", prompt_main)?;
            } else {
                write!(stdout, "{}", prompt_cont)?;
            }
            // Switch to the user-input text tone (bright phosphor)
            // before writing what the user typed, so the prompt
            // accent and the input text are visually distinct but
            // both on the green axis.
            write!(
                stdout,
                "{}",
                SetForegroundColor(self.color(crate::ui::theme::user()))
            )?;
            if let Some(vr) = visual_rows.get(vr_idx)
                && let Some(chars) = display_chars.get(vr.logical_line)
            {
                let slice: String = chars[vr.char_start..vr.char_end].iter().collect();
                write!(stdout, "{}", slice)?;
            }
            write!(stdout, "{}", ResetColor)?;
        }

        // Status row — also centered under the chat band.
        stdout.execute(MoveTo(0, status_row))?;
        write!(stdout, "{}", " ".repeat(cols as usize))?;
        stdout.execute(MoveTo(bottom_indent as u16, status_row))?;
        write!(
            stdout,
            "{}",
            SetForegroundColor(self.color(crate::ui::theme::dim()))
        )?;
        // Status bar with bracketed BBS-style segments. Reads as a row
        // of small chips: `▒░ STATUS ░▒  ▒░ NEXT ░▒  …` so each fact
        // gets visual breathing room and the bar feels like a single
        // designed element rather than dim flowing text.
        let scroll_prefix = if self.scroll_offset > 0 {
            "▒░ SCROLL ░▒  "
        } else {
            ""
        };
        let mut status_display = format!("{}▒░ {} ░▒", scroll_prefix, status);
        if line_count > 1 || total_visual > MAX_INPUT_VISIBLE_LINES {
            let hidden = total_visual.saturating_sub(visible_input_rows);
            if hidden > 0 {
                status_display
                    .push_str(&format!("  ▒░ {} lines · {} hidden ░▒", line_count, hidden));
            } else if line_count > 1 {
                status_display.push_str(&format!("  ▒░ {} lines ░▒", line_count));
            }
        }
        let token_est = editor.expanded().len() as u64 / 4;
        if token_est > 0 {
            status_display.push_str(&format!("  ▒░ {} tk ░▒", token_est));
        }
        let truncated: String = status_display.chars().take(cols as usize).collect();
        write!(stdout, "{}", truncated)?;
        write!(stdout, "{}", ResetColor)?;

        // Place the visible cursor on its row at the right column.
        let cursor_row =
            input_top + (cursor_visual_row.saturating_sub(first_visible_visual)) as u16;
        // Match the 3-column prompt prefix used in the loop above.
        // Match the indented prompt (`bottom_indent + 3-col prompt
        // prefix` + cursor offset within content).
        let cursor_x =
            (bottom_indent + 3 + cursor_visual_col).min(cols.saturating_sub(1) as usize) as u16;
        stdout.execute(MoveTo(cursor_x, cursor_row))?;

        if self.panel_visible() {
            self.draw_panel(&mut stdout, rows)?;
            // draw_panel moves the cursor — return it to the input.
            stdout.execute(MoveTo(cursor_x, cursor_row))?;
        }

        // draw_bottom is the only place the visible cursor belongs (at the
        // user's input position). Other renderer paths (render_viewport,
        // write, write_line) keep the cursor hidden so streaming output
        // doesn't drag the hardware cursor across the chat area.
        stdout.execute(Show)?;
        stdout.flush()?;
        Ok(())
    }

    /// Paint the right-hand info panel in the rightmost `PANEL_WIDTH` cols,
    /// preceded by a vertical divider. Uses cached `self.panel_data`. Caller
    /// is responsible for moving the cursor back if needed.
    fn draw_panel(&self, stdout: &mut io::Stdout, rows: u16) -> io::Result<()> {
        let (cols, _) = self.terminal_size();
        let panel_x = cols.saturating_sub(PANEL_WIDTH);
        let divider_x = panel_x.saturating_sub(1);
        // Effective panel content width = PANEL_WIDTH - 1 so we never
        // write to the terminal's absolute last column. On most
        // terminals, writing the bottom-right cell triggers an
        // implicit scroll-up — that shifts the panel content up by a
        // row each redraw and eats the DIRGE.SYS frame's top border.
        let width = (PANEL_WIDTH as usize).saturating_sub(1);
        let last_row = rows.saturating_sub(1); // status row — leave alone

        // Build the rendered lines first so we know how many we have, then
        // paint top-to-bottom up to last_row-1.
        let lines = self.build_panel_lines(width);

        // Themed panel colors. Source lines still use the legacy
        // sentinels (Color::Cyan = header, Color::Reset = dim,
        // Color::White = body, Color::Green/Red = status); the paint
        // stage remaps them to the active theme so the phosphor look
        // applies without rewriting `build_panel_lines`.
        let divider_color = self.color(crate::ui::theme::divider());
        let header_color = self.color(crate::ui::theme::header());
        let dim = self.color(crate::ui::theme::dim());
        let body = self.color(crate::ui::theme::agent());

        for row in 0..last_row {
            stdout.execute(MoveTo(divider_x, row))?;
            write!(stdout, "{}", SetForegroundColor(divider_color))?;
            write!(stdout, "│")?;
            write!(stdout, "{}", ResetColor)?;

            stdout.execute(MoveTo(panel_x, row))?;
            if let Some((text, color)) = lines.get(row as usize) {
                let painted = match *color {
                    Color::Cyan => header_color,
                    Color::Reset | Color::DarkGrey => dim,
                    Color::White => body,
                    other => self.color(other),
                };
                write!(stdout, "{}", SetForegroundColor(painted))?;
                write!(stdout, "{}", text)?;
                write!(stdout, "{}", ResetColor)?;
                // Pad to panel width so stale text from earlier draws is wiped.
                let len = text.chars().count();
                if len < width {
                    write!(stdout, "{}", " ".repeat(width - len))?;
                }
            } else {
                write!(stdout, "{}", " ".repeat(width))?;
            }
        }
        Ok(())
    }

    /// Materialize the panel content as `(line, color)` pairs of at most
    /// `width` chars each. Sections are separated by blank lines and a
    /// header. Lines past terminal height are dropped by the caller.
    fn build_panel_lines(&self, width: usize) -> Vec<(String, Color)> {
        let d = &self.panel_data;
        let mut out: Vec<(String, Color)> = Vec::new();

        let truncate = |s: &str, w: usize| -> String {
            let cc = s.chars().count();
            if cc <= w {
                s.to_string()
            } else if w <= 1 {
                "…".to_string()
            } else {
                let mut t: String = s.chars().take(w - 1).collect();
                t.push('…');
                t
            }
        };

        // Top padding rows. Same reasoning as the chat banner — without
        // these the DIRGE.SYS frame's `╭───╮` sits flush against the
        // terminal's row 0 and reads as cut off; some terminals also
        // shift the panel up by a row when the bottom-right cell is
        // touched, eating the top frame entirely. The padding gives
        // both visual breathing room and a buffer against that.
        out.push((String::new(), Color::Reset));
        out.push((String::new(), Color::Reset));

        // Inner width inside the frame's left+right borders.
        let inner = width.saturating_sub(2);
        // Helper: format a content row inside a closed pill — left
        // border, padded content, right border. Truncates content
        // that overflows so the pill stays a perfect rectangle.
        let row = |text: &str| -> String {
            let trimmed = truncate(text, inner);
            let len = trimmed.chars().count();
            let pad = inner.saturating_sub(len);
            format!("│{}{}│", trimmed, " ".repeat(pad))
        };
        // Helper: bottom border of a pill (`╰────────╯`).
        let bottom = || -> String { format!("╰{}╯", "─".repeat(inner)) };

        // Panel top pill: `╭─ DIRGE.SYS ────╮` … `╰──────────╯`.
        let dirge_label = " DIRGE.SYS ";
        let dirge_pre_len = dirge_label.chars().count() + 2;
        let dirge_dashes = inner.saturating_sub(dirge_label.chars().count() + 1);
        let _ = dirge_pre_len;
        out.push((
            format!("╭─{}{}╮", dirge_label, "─".repeat(dirge_dashes)),
            Color::Cyan,
        ));
        out.push((row(&format!(" {}", d.cwd)), Color::White));
        out.push((bottom(), Color::Cyan));

        // Section helper: closed pill with the section name on the
        // top border (`╭─ MCP ─────╮ … ╰────────╯`). Every content
        // row gets a `│ … │` left+right border so the section reads
        // as a discrete card, matching the btop / cool-retro-term
        // reference. Empty sections show `· (none)` in the dim
        // phosphor tone rather than grey.
        let push_section =
            |out: &mut Vec<(String, Color)>, title: &str, items: Vec<(String, Color)>| {
                out.push((String::new(), Color::Reset));
                let label = format!(" {} ", title);
                let pre_len = label.chars().count() + 1;
                let dashes = inner.saturating_sub(pre_len);
                let header = format!("╭─{}{}╮", label, "─".repeat(dashes));
                out.push((header, Color::Cyan));
                if items.is_empty() {
                    out.push((row(" · (none)"), Color::Reset));
                } else {
                    for (text, color) in items {
                        // Items arrive with a leading "  "; strip it
                        // and let `row` re-add the chamber prefix.
                        let content = text.strip_prefix("  ").unwrap_or(&text);
                        out.push((row(&format!(" {}", content)), color));
                    }
                }
                out.push((bottom(), Color::Cyan));
            };

        let mcp_items: Vec<(String, Color)> = d
            .mcp
            .iter()
            .map(|(name, ok)| {
                let glyph = if *ok { "●" } else { "○" };
                let color = if *ok { Color::Green } else { Color::Red };
                (format!("  {} {}", glyph, name), color)
            })
            .collect();
        push_section(&mut out, "MCP", mcp_items);

        let lsp_items: Vec<(String, Color)> = d
            .lsp
            .iter()
            .map(|(id, root, ok)| {
                let glyph = if *ok { "●" } else { "○" };
                let color = if *ok { Color::Green } else { Color::Red };
                (format!("  {} {} {}", glyph, id, root), color)
            })
            .collect();
        push_section(&mut out, "LSP", lsp_items);

        let todo_items: Vec<(String, Color)> = d
            .todos
            .iter()
            .map(|(status, text)| (format!("  {} {}", status, text), Color::White))
            .collect();
        push_section(&mut out, "TODOS", todo_items);

        let mod_items: Vec<(String, Color)> = d
            .modified
            .iter()
            .map(|p| (format!("  {}", p), Color::White))
            .collect();
        push_section(&mut out, "MODIFIED", mod_items);

        out
    }
}

/// One visible row of the input box after soft-wrapping. A logical line
/// (between newlines in the buffer) may produce multiple visual rows when
/// it exceeds the terminal's wrap width.
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
        let char_count = line.chars().count();
        let row_count = if char_count == 0 {
            1
        } else {
            char_count.div_ceil(wrap_width)
        };

        let base = rows.len();
        let mut emitted = row_count;

        if li == cursor_line_idx {
            let col = cursor_display_col;
            let (vr, vc) = if col > 0 && col == char_count && col.is_multiple_of(wrap_width) {
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
            let _ = child.wait();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        r.selection_start = Some(15);
        r.selection_end = Some(20);

        for i in 0..7 {
            r.push_buffer_line(LineEntry {
                text: CompactString::new(&format!("new {i}")),
                color: Color::White,
            });
        }

        // Selection indices are absolute and remain untouched.
        assert_eq!(r.selection_start, Some(15));
        assert_eq!(r.selection_end, Some(20));
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
