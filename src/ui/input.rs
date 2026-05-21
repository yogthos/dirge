use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::picker::FilePicker;

const KILL_RING_MAX: usize = 10;

// `cursor` is a byte-offset into `buffer` (UTF-8). The helpers below move the
// cursor by one character boundary so we never land in the middle of a
// multibyte sequence — that would panic on the next insert/remove in
// `CompactString`/`String`.
enum KillDir {
    Prepend,
    Append,
}

#[derive(Default)]
struct YankState {
    index: usize,
    cursor: usize,
    len: usize,
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let len = s.len();
    let mut i = (idx + 1).min(len);
    while i < len && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn is_whitespace(ch: char) -> bool {
    ch.is_whitespace()
}

fn prev_word_boundary(s: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut idx = prev_char_boundary(s, cursor);
    // skip trailing whitespace
    while idx > 0 {
        let ch = s[..idx].chars().next_back().unwrap_or(' ');
        if !is_whitespace(ch) {
            break;
        }
        idx = prev_char_boundary(s, idx);
    }
    // determine character class at position
    if idx == 0 {
        return 0;
    }
    let ch = s[..idx].chars().next_back().unwrap_or(' ');
    let is_word = is_word_char(ch);
    // skip backward through same class
    while idx > 0 {
        let ch = s[..idx].chars().next_back().unwrap_or(' ');
        let current_is_word = is_word_char(ch);
        if current_is_word != is_word || is_whitespace(ch) {
            break;
        }
        let prev = prev_char_boundary(s, idx);
        if prev == idx {
            break;
        }
        idx = prev;
    }
    idx
}

fn next_word_boundary(s: &str, cursor: usize) -> usize {
    let len = s.len();
    if cursor >= len {
        return len;
    }
    let ch = s[cursor..].chars().next().unwrap_or(' ');
    let is_word = is_word_char(ch);
    let is_ws = is_whitespace(ch);
    let mut idx = cursor;
    // skip current class (word, punct, or whitespace)
    while idx < len {
        let ch = s[idx..].chars().next().unwrap_or(' ');
        let current_is_word = is_word_char(ch);
        let current_is_ws = is_whitespace(ch);
        if is_ws {
            if !current_is_ws {
                break;
            }
        } else if current_is_word != is_word {
            break;
        }
        idx = next_char_boundary(s, idx);
    }
    // skip whitespace and punctuation between words
    while idx < len {
        let ch = s[idx..].chars().next().unwrap_or(' ');
        if is_word_char(ch) {
            break;
        }
        idx = next_char_boundary(s, idx);
    }
    idx
}

fn cursor_line_start(s: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let haystack = &s[..cursor];
    match haystack.rfind('\n') {
        Some(pos) => pos + 1,
        None => 0,
    }
}

fn prev_line_start(s: &str, cursor: usize) -> Option<usize> {
    let line_start = cursor_line_start(s, cursor);
    if line_start == 0 {
        return None;
    }
    Some(cursor_line_start(s, line_start.saturating_sub(1)))
}

fn next_line_start(s: &str, cursor: usize) -> Option<usize> {
    let after = &s[cursor..];
    after.find('\n').map(|p| cursor + p + 1)
}

/// Threshold for collapsing pastes: anything with >= this many newlines becomes a
/// `[N lines pasted]` placeholder. Single-line and short pastes go in raw so a
/// quick paste-of-a-command isn't surprising.
const PASTE_COLLAPSE_LINES: usize = 4;

/// Sentinel character bracketing a paste placeholder in the buffer. The buffer
/// stores `\x01<index>\x01`, where `<index>` is the decimal index into
/// `pastes`. Because `\x01` is filtered out of bracketed-paste content (see
/// `handle_paste`) and ignored as a typeable key, it can't appear in normal
/// input — so its presence reliably marks a placeholder block.
const PASTE_MARK: char = '\x01';

pub struct InputEditor {
    pub buffer: CompactString,
    pub cursor: usize,
    history: Vec<CompactString>,
    history_pos: Option<usize>,
    pub picker: Option<FilePicker>,
    monochrome: bool,
    kill_ring: Vec<CompactString>,
    last_action_was_kill: bool,
    yank_state: Option<YankState>,
    /// Pasted text bodies indexed by the digits appearing between `\x01` marks
    /// in the buffer. `None` entries are tombstones for expanded pastes (so
    /// existing indices remain valid).
    pastes: Vec<Option<CompactString>>,
}

/// Find the marker block `\x01<digits>\x01` containing or starting at
/// `cursor`. Returns `(start_of_opening_mark, byte_after_closing_mark, index)`.
fn marker_containing(s: &str, cursor: usize) -> Option<(usize, usize, usize)> {
    let bytes = s.as_bytes();
    // Walk back from cursor to find an opening PASTE_MARK.
    let mut i = cursor.min(bytes.len());
    while i > 0 && bytes[i - 1] != PASTE_MARK as u8 {
        i -= 1;
    }
    if i == 0 {
        return None;
    }
    // i is just after a PASTE_MARK; the opening mark is at i-1.
    let open = i - 1;
    let rest = &bytes[i..];
    let close_rel = rest.iter().position(|&b| b == PASTE_MARK as u8)?;
    let close = i + close_rel;
    if cursor > close {
        return None;
    }
    let digits = std::str::from_utf8(&bytes[i..close]).ok()?;
    let idx = digits.parse::<usize>().ok()?;
    Some((open, close + 1, idx))
}

/// If `pos` falls strictly inside a marker block `(start, end)`, return
/// `start` (so cursor motion moves *before* the block). Otherwise return
/// `pos` unchanged.
fn skip_left_over_marker(s: &str, pos: usize) -> usize {
    for (start, end, _) in marker_blocks(s) {
        if pos > start && pos < end {
            return start;
        }
    }
    pos
}

/// If `pos` falls strictly inside a marker block `(start, end)`, return
/// `end` (so cursor motion moves *after* the block). Otherwise return
/// `pos` unchanged.
fn skip_right_over_marker(s: &str, pos: usize) -> usize {
    for (start, end, _) in marker_blocks(s) {
        if pos > start && pos < end {
            return end;
        }
    }
    pos
}

/// Move one cursor step left, treating any marker block as a single unit.
fn prev_pos(s: &str, cursor: usize) -> usize {
    skip_left_over_marker(s, prev_char_boundary(s, cursor))
}

/// Move one cursor step right, treating any marker block as a single unit.
fn next_pos(s: &str, cursor: usize) -> usize {
    skip_right_over_marker(s, next_char_boundary(s, cursor))
}

/// Word-skip left, but never land mid-marker. `prev_word_boundary` is
/// marker-blind (it sees `\x01` as punctuation and would happily split the
/// marker open), so we post-process with `skip_left_over_marker` to round any
/// in-marker landing back to the marker's left edge.
fn prev_word_pos(s: &str, cursor: usize) -> usize {
    skip_left_over_marker(s, prev_word_boundary(s, cursor))
}

/// Word-skip right, with the symmetric marker-safety post-process.
fn next_word_pos(s: &str, cursor: usize) -> usize {
    skip_right_over_marker(s, next_word_boundary(s, cursor))
}

/// What range a backspace at `cursor` should remove. If the character to the
/// left is the closing mark of a placeholder, return the whole block;
/// otherwise return a single char.
fn backspace_range(s: &str, cursor: usize) -> Option<(usize, usize)> {
    if cursor == 0 {
        return None;
    }
    if let Some((start, end, _)) = marker_containing(s, cursor.saturating_sub(1)) {
        if cursor == end {
            return Some((start, end));
        }
    }
    Some((prev_char_boundary(s, cursor), cursor))
}

/// What range a delete at `cursor` should remove. If the cursor sits at the
/// opening of a placeholder, return the whole block; otherwise a single char.
fn delete_range(s: &str, cursor: usize) -> Option<(usize, usize)> {
    if cursor >= s.len() {
        return None;
    }
    if let Some((start, end, _)) = marker_containing(s, cursor + 1) {
        if cursor == start {
            return Some((start, end));
        }
    }
    Some((cursor, next_char_boundary(s, cursor)))
}

/// Scan `s` and return each marker block as `(start, end, index)` in order.
fn marker_blocks(s: &str) -> Vec<(usize, usize, usize)> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == PASTE_MARK as u8 {
            let start = i;
            let body_start = i + 1;
            if let Some(rel) = bytes[body_start..]
                .iter()
                .position(|&b| b == PASTE_MARK as u8)
            {
                let close = body_start + rel;
                if let Ok(digits) = std::str::from_utf8(&bytes[body_start..close]) {
                    if let Ok(idx) = digits.parse::<usize>() {
                        out.push((start, close + 1, idx));
                        i = close + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    out
}

/// Compute the placeholder display string for a paste body.
fn placeholder_display(text: &str) -> String {
    let lines = text.matches('\n').count() + 1;
    format!("[{} lines pasted]", lines)
}

impl InputEditor {
    pub fn new() -> Self {
        InputEditor {
            buffer: CompactString::new(""),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            picker: None,
            monochrome: false,
            kill_ring: Vec::new(),
            last_action_was_kill: false,
            yank_state: None,
            pastes: Vec::new(),
        }
    }

    /// Insert pasted text. If it spans `PASTE_COLLAPSE_LINES` or more lines,
    /// store it and insert a `[N lines pasted]` placeholder; otherwise insert
    /// raw. If the same content was already pasted and is still represented
    /// by a placeholder, expand that placeholder inline instead (so a second
    /// paste of the same content reveals the body).
    /// Replace the entire buffer with `text` and move the cursor to
    /// the end. Used by `/fork` to restore the original user prompt
    /// into the editor for re-editing.
    pub fn set_text(&mut self, text: &str) {
        self.buffer = CompactString::new(text);
        self.cursor = self.buffer.len();
        self.pastes.clear();
        // Reset kill ring state so a subsequent yank doesn't paste
        // text from before the set_text (which would be jarring —
        // the editor was just rewritten by /fork). History position
        // also resets so Up/Down navigation starts from the new
        // baseline instead of mid-history.
        self.kill_ring.clear();
        self.yank_state = None;
        self.history_pos = None;
    }

    pub fn handle_paste(&mut self, text: &str) {
        // The file picker (`@query`) maintains its own filter state. A paste
        // landing here would write marker bytes into the buffer that the
        // picker doesn't know about, leaving a stale/corrupt query. Easiest
        // to just ignore pastes while the picker is active — the user can
        // close the picker (Esc) and re-paste.
        if self.picker.as_ref().is_some_and(|p| p.active) {
            return;
        }
        // Normalize line endings to `\n`. macOS-era clipboards and some
        // terminal paste streams deliver `\r` or `\r\n`. Without this the
        // line count comes out as 1, the collapse threshold isn't reached,
        // and the raw text gets inserted — with embedded `\r` chars that
        // the terminal then renders as carriage-returns, garbling the line.
        let normalized: String = text.replace("\r\n", "\n").replace('\r', "\n");
        // Strip PASTE_MARK so it can never appear in paste content and confuse
        // the marker parser.
        let cleaned: String = normalized.chars().filter(|&c| c != PASTE_MARK).collect();
        if cleaned.is_empty() {
            return;
        }
        let line_count = cleaned.matches('\n').count() + 1;
        if line_count < PASTE_COLLAPSE_LINES {
            self.insert_str(&cleaned);
            return;
        }
        // Auto-expand on repeat: if this body matches an existing placeholder
        // in the buffer, expand it inline rather than inserting another
        // placeholder.
        if let Some((start, end, idx)) =
            marker_blocks(&self.buffer).into_iter().find(|(_, _, idx)| {
                self.pastes
                    .get(*idx)
                    .and_then(|opt| opt.as_ref())
                    .map(|s| s.as_str() == cleaned.as_str())
                    .unwrap_or(false)
            })
        {
            let body = self.pastes[idx].take().unwrap();
            self.buffer.replace_range(start..end, body.as_str());
            // Place cursor at end of expanded text.
            self.cursor = start + body.len();
            self.history_pos = None;
            self.reset_kill_accumulation();
            return;
        }
        let idx = self.pastes.len();
        self.pastes.push(Some(CompactString::from(cleaned)));
        let marker = format!("{}{}{}", PASTE_MARK, idx, PASTE_MARK);
        self.insert_str(&marker);
    }

    fn insert_str(&mut self, s: &str) {
        self.buffer.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.history_pos = None;
        self.reset_kill_accumulation();
    }

    /// Remove a byte range from the buffer and place the cursor at `start`.
    /// If the range fully contains a placeholder marker block, the
    /// corresponding `pastes` slot is tombstoned so its body can be GC'd
    /// (idempotent — repeat removes are fine).
    fn remove_range(&mut self, start: usize, end: usize) {
        // Detect any marker block fully contained in the removed range and
        // free its stored body.
        for (mstart, mend, idx) in marker_blocks(&self.buffer) {
            if mstart >= start && mend <= end {
                if let Some(slot) = self.pastes.get_mut(idx) {
                    *slot = None;
                }
            }
        }
        self.buffer.replace_range(start..end, "");
        self.cursor = start;
    }

    /// Return the buffer with all placeholder markers expanded to their
    /// original paste bodies. Used at submit time so the agent receives the
    /// real text.
    pub fn expanded(&self) -> CompactString {
        Self::expand_with_pastes(&self.buffer, &self.pastes).into()
    }

    /// Expand markers in `s` using `pastes` for bodies. Free-function form
    /// so it can also be used to flatten markers in kill-ring entries
    /// before we clear `pastes`.
    fn expand_with_pastes(s: &str, pastes: &[Option<CompactString>]) -> String {
        let blocks = marker_blocks(s);
        if blocks.is_empty() {
            return s.to_string();
        }
        let mut out = String::with_capacity(s.len());
        let mut cur = 0;
        for (start, end, idx) in blocks {
            out.push_str(&s[cur..start]);
            if let Some(Some(body)) = pastes.get(idx) {
                out.push_str(body);
            }
            cur = end;
        }
        out.push_str(&s[cur..]);
        out
    }

    /// Return (display_text, display_cursor_col) for a logical line of the
    /// buffer with placeholders rendered as `[N lines pasted]`. Used by the
    /// renderer so the input bar shows a compact representation.
    pub fn render_line(&self, line: &str, cursor_in_line: usize) -> (String, usize) {
        let blocks = marker_blocks(line);
        if blocks.is_empty() {
            return (line.to_string(), cursor_in_line);
        }
        let mut out = String::with_capacity(line.len());
        let mut display_cursor = cursor_in_line;
        let mut cur = 0;
        for (start, end, idx) in blocks {
            // Carry plain text before the block.
            if cur < start {
                out.push_str(&line[cur..start]);
            }
            let placeholder = self
                .pastes
                .get(idx)
                .and_then(|o| o.as_ref())
                .map(|s| placeholder_display(s))
                .unwrap_or_else(|| "[expanded]".to_string());
            // Adjust the displayed cursor position if it lies after this block.
            if cursor_in_line >= end {
                let block_len = end - start;
                display_cursor = display_cursor - block_len + placeholder.len();
            } else if cursor_in_line > start && cursor_in_line < end {
                // Cursor logically inside a marker — pin it to the placeholder
                // boundary so it never appears mid-marker.
                display_cursor = out.len() + placeholder.len();
            }
            out.push_str(&placeholder);
            cur = end;
        }
        if cur < line.len() {
            out.push_str(&line[cur..]);
        }
        (out, display_cursor)
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.monochrome = monochrome;
        if let Some(picker) = self.picker.as_mut() {
            picker.set_monochrome(monochrome);
        }
    }

    pub fn start_picker(&mut self) {
        let picker = self.picker.get_or_insert_with(FilePicker::new);
        picker.set_monochrome(self.monochrome);
        picker.activate();
    }

    fn reset_kill_accumulation(&mut self) {
        self.last_action_was_kill = false;
        self.yank_state = None;
    }

    fn push_kill(&mut self, text: CompactString, direction: KillDir) {
        if text.is_empty() {
            return;
        }
        if self.last_action_was_kill && !self.kill_ring.is_empty() {
            let entry = &mut self.kill_ring[0];
            match direction {
                KillDir::Prepend => {
                    let mut new = text;
                    new.push_str(entry);
                    *entry = new;
                }
                KillDir::Append => {
                    entry.push_str(&text);
                }
            }
        } else {
            self.kill_ring.insert(0, text);
            if self.kill_ring.len() > KILL_RING_MAX {
                self.kill_ring.pop();
            }
        }
        self.last_action_was_kill = true;
    }

    pub fn handle_picker_key(&mut self, key: KeyEvent) -> bool {
        let picker = match self.picker.as_mut() {
            Some(p) if p.active => p,
            _ => return false,
        };

        match key.code {
            KeyCode::Char(c)
                if c == '\x08' || (c == 'h' && key.modifiers.contains(KeyModifiers::CONTROL)) =>
            {
                if picker.cursor > 0 {
                    picker.backspace();
                    self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                    self.buffer.remove(self.cursor);
                } else {
                    // `rfind` returns a *byte* offset and `self.cursor`
                    // is a byte offset (see line below where we add
                    // `c.len_utf8()`). The previous version mixed byte
                    // offsets with `chars().take(N)` which counts chars
                    // — corrupted any buffer containing multi-byte
                    // text before the `@`. Use byte-level slicing
                    // throughout.
                    if let Some(at) = self.buffer.rfind('@') {
                        let before = &self.buffer[..at];
                        let after = self.buffer.get(at + 1..).unwrap_or("");
                        let new_buf = format!("{}{}", before, after);
                        self.cursor = at;
                        self.buffer = new_buf.into();
                    }
                    picker.deactivate();
                }
                true
            }
            KeyCode::Char(c) => {
                picker.char_input(c);
                self.buffer.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                true
            }
            KeyCode::Backspace => {
                if picker.cursor > 0 {
                    picker.backspace();
                    self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                    self.buffer.remove(self.cursor);
                    true
                } else {
                    // Same byte-vs-char fix as the Esc branch above.
                    if let Some(at) = self.buffer.rfind('@') {
                        let before = &self.buffer[..at];
                        let after = self.buffer.get(at + 1..).unwrap_or("");
                        let new_buf = format!("{}{}", before, after);
                        self.cursor = at;
                        self.buffer = new_buf.into();
                    }
                    picker.deactivate();
                    true
                }
            }
            KeyCode::Tab => {
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::SHIFT)
                {
                    picker.select_prev();
                } else {
                    picker.select_next();
                }
                true
            }
            KeyCode::Up => {
                picker.select_prev();
                true
            }
            KeyCode::Down => {
                picker.select_next();
                true
            }
            KeyCode::Enter => {
                if let Some(path) = picker.selected_path() {
                    let path_str = path.to_string_lossy().to_string();
                    // Byte-level slicing — `rfind`, `picker.query.len()`,
                    // and `self.cursor` are all byte offsets. Previous
                    // version mixed byte indices with `chars()` iters
                    // and corrupted the buffer on multi-byte input.
                    if let Some(at) = self.buffer.rfind('@') {
                        let before = &self.buffer[..at];
                        let after_byte = at + 1 + picker.query.len();
                        let after = self.buffer.get(after_byte..).unwrap_or("");
                        let new_cursor = before.len() + path_str.len();
                        let new_buf = format!("{}{}{}", before, path_str, after);
                        self.cursor = new_cursor;
                        self.buffer = new_buf.into();
                    }
                }
                picker.deactivate();
                true
            }
            KeyCode::Esc => {
                let at_pos = self.buffer.rfind('@');
                if let Some(at) = at_pos {
                    let before: String = self.buffer.chars().take(at).collect();
                    let after: String = self
                        .buffer
                        .chars()
                        .skip(at + 1 + picker.query.len())
                        .collect();
                    self.buffer = format!("{}{}", before, after).into();
                    self.cursor = at;
                }
                picker.deactivate();
                true
            }
            _ => false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<CompactString> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // Ctrl+J is a portable newline trigger — many terminals never send
        // Shift+Enter as a distinct keystroke, but Ctrl+J always arrives.
        // Handled here before the Enter arm so it works even when the
        // terminal collapses Ctrl+J onto KeyCode::Enter.
        if ctrl && matches!(key.code, KeyCode::Char('j')) {
            if !self.picker.as_ref().is_some_and(|p| p.active) {
                self.buffer.insert(self.cursor, '\n');
                self.cursor += 1;
                self.history_pos = None;
                self.reset_kill_accumulation();
            }
            return None;
        }

        match key.code {
            KeyCode::Enter => {
                if self.picker.as_ref().is_some_and(|p| p.active) {
                    return None;
                }
                // Meta+Enter or Shift+Enter inserts newline
                if has_shift || alt {
                    self.buffer.insert(self.cursor, '\n');
                    self.cursor += 1;
                    self.history_pos = None;
                    return None;
                }
                // Plain Enter → submit. Expand any paste placeholders so the
                // agent receives the original text. Store the expanded form in
                // history too — history navigation can't rely on paste-index
                // continuity across turns.
                let submitted = self.expanded();
                if !submitted.is_empty() {
                    // Dedup against the most recent entry (bash/Emacs
                    // convention — pressing Enter on the same prompt
                    // twice shouldn't fill history with duplicates).
                    // Also cap history at 500 entries so a long-lived
                    // session doesn't grow it unboundedly.
                    const HISTORY_MAX: usize = 500;
                    let is_dupe = self
                        .history
                        .last()
                        .map(|prev| prev.as_str() == submitted.as_str())
                        .unwrap_or(false);
                    if !is_dupe {
                        self.history.push(submitted.clone());
                        if self.history.len() > HISTORY_MAX {
                            // Drop the oldest entries in batches so we
                            // aren't doing a shift on every submit
                            // once we hit the cap.
                            let drain_to = self.history.len() - HISTORY_MAX;
                            self.history.drain(..drain_to);
                        }
                    }
                }
                self.history_pos = None;
                self.buffer.clear();
                self.cursor = 0;
                // Flatten markers in kill-ring entries to their raw bodies
                // before dropping pastes — otherwise a later Ctrl+Y would
                // yank back marker bytes referencing indices we just
                // cleared, and `expanded()` would silently omit them.
                for entry in self.kill_ring.iter_mut() {
                    if entry.contains(PASTE_MARK) {
                        let expanded = Self::expand_with_pastes(entry, &self.pastes);
                        *entry = expanded.into();
                    }
                }
                self.pastes.clear();
                self.reset_kill_accumulation();
                if submitted.is_empty() {
                    None
                } else {
                    Some(submitted)
                }
            }

            // Ctrl+A → start of line
            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                self.reset_kill_accumulation();
                None
            }

            // Ctrl+E → end of line
            KeyCode::Char('e') if ctrl => {
                self.cursor = self.buffer.len();
                self.reset_kill_accumulation();
                None
            }

            // Ctrl+B → left one char
            KeyCode::Char('b') if ctrl => {
                if self.cursor > 0 {
                    self.cursor = prev_pos(&self.buffer, self.cursor);
                }
                self.reset_kill_accumulation();
                None
            }

            // Ctrl+F → right one char
            KeyCode::Char('f') if ctrl => {
                if self.cursor < self.buffer.len() {
                    self.cursor = next_pos(&self.buffer, self.cursor);
                }
                self.reset_kill_accumulation();
                None
            }

            // Ctrl+K → kill to end of line
            KeyCode::Char('k') if ctrl => {
                if self.cursor < self.buffer.len() {
                    let killed: CompactString = self.buffer[self.cursor..].into();
                    self.buffer.truncate(self.cursor);
                    self.push_kill(killed, KillDir::Append);
                }
                None
            }

            // Ctrl+U → kill to start of line
            KeyCode::Char('u') if ctrl => {
                if self.cursor > 0 {
                    let killed: CompactString = self.buffer[..self.cursor].into();
                    self.buffer = self.buffer[self.cursor..].into();
                    self.cursor = 0;
                    self.push_kill(killed, KillDir::Prepend);
                }
                None
            }

            // Ctrl+W → kill word before
            KeyCode::Char('w') if ctrl => {
                if self.cursor > 0 {
                    let start = prev_word_pos(&self.buffer, self.cursor);
                    let killed: CompactString = self.buffer[start..self.cursor].into();
                    self.buffer.replace_range(start..self.cursor, "");
                    self.cursor = start;
                    self.push_kill(killed, KillDir::Prepend);
                }
                None
            }

            // Ctrl+H or Backspace (plain)
            KeyCode::Char('h') if ctrl => {
                if let Some((start, end)) = backspace_range(&self.buffer, self.cursor) {
                    self.remove_range(start, end);
                }
                self.reset_kill_accumulation();
                None
            }

            // Ctrl+Y → yank
            KeyCode::Char('y') if ctrl => {
                if let Some(text) = self.kill_ring.first() {
                    let text = text.clone();
                    let len = text.len();
                    self.buffer.insert_str(self.cursor, &text);
                    self.yank_state = Some(YankState {
                        index: 0,
                        cursor: self.cursor,
                        len,
                    });
                    self.cursor += len;
                }
                self.last_action_was_kill = false;
                None
            }

            // Ctrl+N → history down
            KeyCode::Char('n') if ctrl => {
                self.history_down();
                self.reset_kill_accumulation();
                None
            }

            // Ctrl+P → history up
            KeyCode::Char('p') if ctrl => {
                self.history_up();
                self.reset_kill_accumulation();
                None
            }

            // Meta+Y → yank-pop (cycle kill ring)
            KeyCode::Char('y') if alt => {
                if let Some(ref state) = self.yank_state {
                    let range_end = state.cursor + state.len;
                    if self.kill_ring.len() > 1 && range_end <= self.buffer.len() {
                        let next = (state.index + 1) % self.kill_ring.len();
                        if let Some(text) = self.kill_ring.get(next) {
                            let text = text.clone();
                            self.buffer.replace_range(state.cursor..range_end, "");
                            self.buffer.insert_str(state.cursor, &text);
                            self.cursor = state.cursor + text.len();
                            self.yank_state = Some(YankState {
                                index: next,
                                cursor: state.cursor,
                                len: text.len(),
                            });
                        }
                    }
                }
                None
            }

            // Meta+D → delete word after
            KeyCode::Char('d') if alt => {
                if self.cursor < self.buffer.len() {
                    let end = next_word_pos(&self.buffer, self.cursor);
                    self.buffer.replace_range(self.cursor..end, "");
                }
                self.reset_kill_accumulation();
                None
            }

            // Meta+B → prev word (Emacs style)
            KeyCode::Char('b') if alt => {
                if self.cursor > 0 {
                    self.cursor = prev_word_pos(&self.buffer, self.cursor);
                }
                self.reset_kill_accumulation();
                None
            }

            // Meta+F → next word (Emacs style)
            KeyCode::Char('f') if alt => {
                if self.cursor < self.buffer.len() {
                    self.cursor = next_word_pos(&self.buffer, self.cursor);
                } else {
                    self.cursor = self.buffer.len();
                }
                self.reset_kill_accumulation();
                None
            }

            // Meta+Left → prev word
            KeyCode::Left if alt => {
                if self.cursor > 0 {
                    self.cursor = prev_word_pos(&self.buffer, self.cursor);
                }
                self.reset_kill_accumulation();
                None
            }

            // Meta+Right → next word
            KeyCode::Right if alt => {
                if self.cursor < self.buffer.len() {
                    self.cursor = next_word_pos(&self.buffer, self.cursor);
                } else {
                    self.cursor = self.buffer.len();
                }
                self.reset_kill_accumulation();
                None
            }

            // Meta+Backspace → delete word before
            KeyCode::Backspace if alt => {
                if self.cursor > 0 {
                    let start = prev_word_pos(&self.buffer, self.cursor);
                    self.buffer.replace_range(start..self.cursor, "");
                    self.cursor = start;
                }
                self.reset_kill_accumulation();
                None
            }

            // Plain char: only if not ctrl/alt-modified
            KeyCode::Char(c) if !ctrl && !alt => {
                if c == '@' {
                    let at_word_start = self.cursor == 0
                        || self.buffer.as_bytes().get(self.cursor - 1) == Some(&b' ');
                    if at_word_start {
                        self.start_picker();
                    }
                }
                self.buffer.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.history_pos = None;
                self.reset_kill_accumulation();
                None
            }

            KeyCode::Backspace => {
                if let Some((start, end)) = backspace_range(&self.buffer, self.cursor) {
                    self.remove_range(start, end);
                }
                self.reset_kill_accumulation();
                None
            }

            KeyCode::Delete => {
                if let Some((start, end)) = delete_range(&self.buffer, self.cursor) {
                    self.remove_range(start, end);
                }
                self.reset_kill_accumulation();
                None
            }

            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_pos(&self.buffer, self.cursor);
                }
                self.reset_kill_accumulation();
                None
            }

            KeyCode::Right => {
                if self.cursor < self.buffer.len() {
                    self.cursor = next_pos(&self.buffer, self.cursor);
                }
                self.reset_kill_accumulation();
                None
            }

            KeyCode::Home => {
                self.cursor = 0;
                self.reset_kill_accumulation();
                None
            }

            KeyCode::End => {
                self.cursor = self.buffer.len();
                self.reset_kill_accumulation();
                None
            }

            KeyCode::Up => {
                self.reset_kill_accumulation();
                // If already navigating history, continue.
                if self.history_pos.is_some() {
                    self.history_up();
                    return None;
                }
                // Try moving up within the multiline buffer first.
                if let Some(pos) = prev_line_start(&self.buffer, self.cursor) {
                    let line_start = cursor_line_start(&self.buffer, self.cursor);
                    let col = self.cursor - line_start;
                    let target_line_end = self.buffer[pos..]
                        .find('\n')
                        .map(|p| pos + p)
                        .unwrap_or(self.buffer.len());
                    self.cursor = (pos + col).min(target_line_end);
                    return None;
                }
                // At top of buffer → fall through to history.
                self.history_up();
                None
            }

            KeyCode::Down => {
                self.reset_kill_accumulation();
                // If already navigating history, continue.
                if self.history_pos.is_some() {
                    self.history_down();
                    return None;
                }
                // Try moving down within the multiline buffer first.
                if let Some(pos) = next_line_start(&self.buffer, self.cursor) {
                    let line_start = cursor_line_start(&self.buffer, self.cursor);
                    let col = self.cursor - line_start;
                    let target_line_end = self.buffer[pos..]
                        .find('\n')
                        .map(|p| pos + p)
                        .unwrap_or(self.buffer.len());
                    self.cursor = (pos + col).min(target_line_end);
                    return None;
                }
                // At bottom of buffer → fall through to history.
                self.history_down();
                None
            }

            KeyCode::Tab => {
                self.buffer.insert_str(self.cursor, "  ");
                self.cursor += 2;
                self.reset_kill_accumulation();
                None
            }

            _ => None,
        }
    }

    fn history_up(&mut self) {
        let hist_len = self.history.len();
        if hist_len == 0 {
            return;
        }
        let pos = match self.history_pos {
            Some(p) if p > 0 => p - 1,
            Some(_) => 0,
            None => hist_len - 1,
        };
        self.history_pos = Some(pos);
        self.buffer = self.history[pos].clone();
        self.cursor = self.buffer.len();
    }

    fn history_down(&mut self) {
        match self.history_pos {
            Some(pos) if pos + 1 < self.history.len() => {
                let new_pos = pos + 1;
                self.history_pos = Some(new_pos);
                self.buffer = self.history[new_pos].clone();
                self.cursor = self.buffer.len();
            }
            Some(_) => {
                self.history_pos = None;
                self.buffer.clear();
                self.cursor = 0;
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prev_word_boundary_basic() {
        assert_eq!(prev_word_boundary("hello world", 11), 6);
        assert_eq!(prev_word_boundary("hello world", 6), 0);
        assert_eq!(prev_word_boundary("hello world", 5), 0);
    }

    #[test]
    fn test_prev_word_from_middle() {
        assert_eq!(prev_word_boundary("hello world", 9), 6); // middle of "world"
    }

    #[test]
    fn test_prev_word_at_start() {
        assert_eq!(prev_word_boundary("hello", 0), 0);
    }

    #[test]
    fn test_prev_word_punctuation() {
        assert_eq!(prev_word_boundary("foo.bar", 7), 4); // start of "bar"
        assert_eq!(prev_word_boundary("foo.bar", 4), 0); // start of "foo"
    }

    #[test]
    fn test_next_word_boundary_basic() {
        // "hello world foo" = 15 bytes
        assert_eq!(next_word_boundary("hello world foo", 0), 6);
        assert_eq!(next_word_boundary("hello world foo", 6), 12);
        assert_eq!(next_word_boundary("hello world foo", 12), 15);
    }

    #[test]
    fn test_next_word_at_end() {
        assert_eq!(next_word_boundary("hello", 5), 5);
    }

    #[test]
    fn test_next_word_punctuation() {
        // With updated logic: from start, skip "foo" + "." → land at "bar_baz" (byte 4)
        assert_eq!(next_word_boundary("foo.bar_baz", 0), 4);
        assert_eq!(next_word_boundary("foo.bar_baz", 3), 4); // from '.', skip it → byte 4
        assert_eq!(next_word_boundary("foo.bar_baz", 4), 11); // skip "bar_baz" → end
    }

    #[test]
    fn test_prev_word_multibyte() {
        // "hå bør": h(0) å(1,2→3) sp(3) b(4) ø(5,6→7) r(7→8)
        assert_eq!(prev_word_boundary("hå bør", 7), 4); // from after 'ø' → start of "bør" at 4
        assert_eq!(prev_word_boundary("hå bør", 4), 0); // from start of "bør" → start of "hå" at 0
    }

    #[test]
    fn test_next_word_multibyte() {
        // "hå bør": 8 bytes. h(0) å(1-2=3) sp(3) b(4) ø(5-6=7) r(7→8)
        assert_eq!(next_word_boundary("hå bør", 0), 4); // skip "hå ", land at "b" (byte 4)
        assert_eq!(next_word_boundary("hå bør", 4), 8); // skip "bør", land at end
    }

    #[test]
    fn test_cursor_line_start() {
        assert_eq!(cursor_line_start("hello\nworld", 10), 6);
        assert_eq!(cursor_line_start("hello\nworld", 3), 0);
        assert_eq!(cursor_line_start("single", 6), 0);
    }

    #[test]
    fn test_prev_line_start() {
        assert_eq!(prev_line_start("hello\nworld", 10), Some(0));
        assert_eq!(prev_line_start("hello\nworld", 3), None);
    }

    #[test]
    fn test_next_line_start() {
        assert_eq!(next_line_start("hello\nworld", 0), Some(6));
        assert_eq!(next_line_start("hello\nworld", 10), None);
    }
}
