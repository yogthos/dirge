use std::io::{self, Write};

use compact_str::CompactString;
use crossterm::ExecutableCommand;
use crossterm::cursor::MoveTo;
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

pub struct Renderer {
    lines: u16,
    col: u16,
    spinner_tick: bool,
    buffer: Vec<LineEntry>,
    partial: CompactString,
    partial_color: Color,
    scroll_offset: usize,
    input_scroll_offset: usize,
    /// Number of rows the input area currently occupies (1 by default, grows
    /// up to MAX_INPUT_VISIBLE_LINES as the user adds newlines via
    /// Shift+Enter / Meta+Enter / Ctrl+J). The chat viewport shrinks by the
    /// same amount so the total layout fits.
    input_rows: u16,
    monochrome: bool,
    pub selection_active: bool,
    pub selection_start: Option<usize>,
    pub selection_end: Option<usize>,
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
            input_scroll_offset: 0,
            input_rows: 1,
            monochrome: false,
            selection_active: false,
            selection_start: None,
            selection_end: None,
        })
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
        let (cols, _) = self.terminal_size();
        cols.saturating_sub(1) as usize
    }

    pub fn line_width(&self) -> usize {
        self.max_line_width()
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
        let (cols, rows) = self.terminal_size();
        let visible = rows.saturating_sub(self.input_rows + 1) as usize;
        let total = self.buffer.len();
        let mut stdout = io::stdout();

        let start = if self.scroll_offset == 0 {
            total.saturating_sub(visible)
        } else {
            total.saturating_sub(self.scroll_offset + visible)
        };
        let start = start.min(total.saturating_sub(visible));
        let end = (start + visible).min(total);

        for i in 0..visible {
            stdout.execute(MoveTo(0, i as u16))?;
            if start + i < end {
                let entry = &self.buffer[start + i];
                let line_idx = start + i;
                let text: String = entry
                    .text
                    .chars()
                    .take(cols.saturating_sub(1) as usize)
                    .collect();

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
                write!(stdout, "{}", SetForegroundColor(self.color(entry.color)))?;
                write!(stdout, "{}", text)?;
                if is_selected {
                    write!(stdout, "{}", SetAttribute(Attribute::NoReverse))?;
                }
                write!(stdout, "{}", ResetColor)?;
            }
            write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
        }

        if self.scroll_offset > 0 {
            let pct = if total > visible {
                ((total - self.scroll_offset - visible) * 100 / (total - visible)).min(100)
            } else {
                0
            };
            let indicator = format!(" SCROLL {}% ", pct);
            let x = cols.saturating_sub(indicator.len() as u16);
            stdout.execute(MoveTo(x, 0))?;
            write!(
                stdout,
                "{}",
                SetForegroundColor(self.color(Color::DarkYellow))
            )?;
            write!(stdout, "{}", indicator)?;
            write!(stdout, "{}", ResetColor)?;
        }

        stdout.flush()?;
        Ok(())
    }

    fn ensure_room(&mut self) {
        if self.scroll_offset > 0 {
            return;
        }
        let (cols, rows) = self.terminal_size();
        if rows < 3 {
            return;
        }
        let max_content = rows.saturating_sub(self.input_rows + 1);
        if self.lines >= max_content {
            let mut stdout = io::stdout();
            let _ = stdout.execute(ScrollUp(1));
            self.lines = self.lines.saturating_sub(1);
            for &r in &[max_content.saturating_sub(1), max_content] {
                let _ = stdout.execute(MoveTo(0, r));
                let _ = write!(stdout, "{}", " ".repeat(cols as usize));
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
                    let r = self.content_row();
                    stdout.execute(MoveTo(0, r))?;
                    stdout.execute(Clear(ClearType::CurrentLine))?;
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
                    stdout.execute(MoveTo(self.col, r))?;
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
                        stdout.execute(MoveTo(self.col, r))?;
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
        let (cols, rows) = crossterm::terminal::size()?;
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

        // Decide how many input rows to show. Cap at MAX_INPUT_VISIBLE_LINES
        // so the chat viewport doesn't disappear under huge pastes / drafts.
        let visible_input_rows = line_count.min(MAX_INPUT_VISIBLE_LINES).max(1);

        // Pick which slice of logical lines to render so the cursor's line is
        // always on screen. Simple scroll-to-keep-cursor-visible strategy.
        let first_visible_line = if cursor_line_idx >= visible_input_rows {
            cursor_line_idx + 1 - visible_input_rows
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
        let prompt_main = if is_running {
            self.spinner_tick = !self.spinner_tick;
            if self.spinner_tick { ". " } else { ": " }
        } else {
            "> "
        };
        // Continuation rows get a dimmer prompt so the user can see at a
        // glance which row is "the" prompt and which are wrapped lines.
        let prompt_cont = "· ";

        // Reserve right-edge space for the token counter so it never
        // overdraws line content. Width is the width of the longest
        // realistic counter string ("  (NNNN tk)" ~= 12 chars).
        let token_est = editor.expanded().len() as u64 / 4;
        let counter_reserve: u16 = if token_est > 0 { 12 } else { 0 };
        let visible_width = cols.saturating_sub(2 + counter_reserve) as usize;

        // Recompute horizontal scroll based on the cursor's line. Each draw
        // re-anchors so very long single lines still pan horizontally.
        let cursor_raw_line = logical_lines[cursor_line_idx];
        let (_, cursor_display_col) = editor.render_line(cursor_raw_line, cursor_col_in_line);
        if cursor_display_col < self.input_scroll_offset {
            self.input_scroll_offset = cursor_display_col;
        } else if cursor_display_col >= self.input_scroll_offset + visible_width {
            self.input_scroll_offset = cursor_display_col - visible_width + 1;
        }

        // Render each visible logical line.
        for row_offset in 0..visible_input_rows {
            let row = input_top + row_offset as u16;
            let line_idx = first_visible_line + row_offset;
            stdout.execute(MoveTo(0, row))?;
            write!(stdout, "{}", " ".repeat(cols as usize))?;
            stdout.execute(MoveTo(0, row))?;
            write!(stdout, "{}", SetForegroundColor(self.color(Color::Cyan)))?;
            if line_idx == 0 {
                write!(stdout, "{}", prompt_main)?;
            } else {
                write!(stdout, "{}", prompt_cont)?;
            }
            write!(stdout, "{}", ResetColor)?;
            let raw_line = logical_lines.get(line_idx).copied().unwrap_or("");
            let (display_line, _) = editor.render_line(raw_line, 0);
            // Apply horizontal scroll if this line contains the cursor; other
            // lines render from column 0 (they'll get truncated if too wide).
            let scroll = if line_idx == cursor_line_idx {
                self.input_scroll_offset
            } else {
                0
            };
            let visible: String = display_line
                .chars()
                .skip(scroll)
                .take(visible_width)
                .collect();
            write!(stdout, "{}", visible)?;
        }

        // Token counter on the last visible row, sitting in the reserved
        // right-edge gap (see `counter_reserve` above). Doesn't overdraw
        // line content because `visible_width` already excludes this band.
        if counter_reserve > 0 {
            let last_row = input_top + (visible_input_rows as u16 - 1);
            let counter = format!("  ({} tk)", token_est);
            let counter_col = cols.saturating_sub(counter.len() as u16);
            stdout.execute(MoveTo(counter_col, last_row))?;
            write!(
                stdout,
                "{}",
                SetForegroundColor(self.color(Color::DarkGrey))
            )?;
            write!(stdout, "{}", counter)?;
            write!(stdout, "{}", ResetColor)?;
        }

        // Status row.
        stdout.execute(MoveTo(0, status_row))?;
        write!(stdout, "{}", " ".repeat(cols as usize))?;
        stdout.execute(MoveTo(0, status_row))?;
        write!(
            stdout,
            "{}",
            SetForegroundColor(self.color(Color::DarkGrey))
        )?;
        let mut status_display = if self.scroll_offset > 0 {
            format!("-- SCROLL -- {}", status)
        } else {
            status.to_string()
        };
        if line_count > MAX_INPUT_VISIBLE_LINES {
            // Buffer is bigger than the visible window — tell the user how
            // many extra logical lines there are.
            status_display.push_str(&format!(
                " [{} lines, {} hidden]",
                line_count,
                line_count - MAX_INPUT_VISIBLE_LINES
            ));
        } else if line_count > 1 {
            status_display.push_str(&format!(" [{} lines]", line_count));
        }
        let truncated: String = status_display.chars().take(cols as usize).collect();
        write!(stdout, "{}", truncated)?;
        write!(stdout, "{}", ResetColor)?;

        // Place the visible cursor on its row at the right column.
        let cursor_row = input_top + (cursor_line_idx - first_visible_line) as u16;
        let cursor_x = (2 + cursor_display_col.saturating_sub(self.input_scroll_offset)) as u16;
        stdout.execute(MoveTo(cursor_x, cursor_row))?;
        stdout.flush()?;
        Ok(())
    }
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
}
