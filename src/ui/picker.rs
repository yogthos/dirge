use std::io::Write;
use std::path::{Component, PathBuf};
use std::sync::{Arc, Mutex};

use crossterm::ExecutableCommand;
use crossterm::cursor::MoveTo;
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::terminal::Clear;

use super::resolve_color;

pub struct ListPicker {
    pub active: bool,
    pub items: Vec<String>,
    pub selected: usize,
    prompt: String,
    monochrome: bool,
}

impl ListPicker {
    pub fn new() -> Self {
        ListPicker {
            active: false,
            items: Vec::new(),
            selected: 0,
            prompt: String::new(),
            monochrome: false,
        }
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.monochrome = monochrome;
    }

    fn color(&self, color: Color) -> Color {
        resolve_color(color, self.monochrome)
    }

    pub fn activate(&mut self, prompt: &str, items: Vec<String>) {
        self.active = true;
        self.prompt = prompt.to_string();
        self.items = items;
        self.selected = 0;
    }

    pub fn deactivate(&mut self) {
        self.active = false;
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Option<usize> {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                None
            }
            KeyCode::Down => {
                if self.selected + 1 < self.items.len() {
                    self.selected += 1;
                }
                None
            }
            KeyCode::Enter => {
                if self.items.is_empty() {
                    self.active = false;
                    None
                } else {
                    let result = Some(self.selected);
                    self.active = false;
                    result
                }
            }
            KeyCode::Esc => {
                self.active = false;
                None
            }
            _ => None,
        }
    }

    pub fn draw(&self) -> std::io::Result<()> {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        crossterm::execute!(stdout, SetForegroundColor(self.color(Color::Cyan)),)?;
        writeln!(stdout, "  {}", self.prompt)?;
        writeln!(stdout)?;

        for (i, item) in self.items.iter().enumerate() {
            let truncated: String = item.chars().take(60).collect();
            if i == self.selected {
                crossterm::execute!(stdout, SetForegroundColor(self.color(Color::Green)),)?;
                writeln!(stdout, " ▸ {}", truncated)?;
            } else {
                crossterm::execute!(stdout, SetForegroundColor(self.color(Color::DarkGrey)),)?;
                writeln!(stdout, "   {}", truncated)?;
            }
        }
        crossterm::execute!(stdout, ResetColor)?;
        Ok(())
    }
}

pub struct FilePicker {
    pub active: bool,
    pub query: String,
    pub cursor: usize,
    pub matches: Vec<PathBuf>,
    pub selected: usize,
    file_cache: Arc<Mutex<Vec<PathBuf>>>,
    monochrome: bool,
}

impl FilePicker {
    pub fn new() -> Self {
        FilePicker {
            active: false,
            query: String::new(),
            cursor: 0,
            matches: Vec::new(),
            selected: 0,
            file_cache: Arc::new(Mutex::new(Vec::new())),
            monochrome: false,
        }
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.monochrome = monochrome;
    }

    fn color(&self, color: Color) -> Color {
        resolve_color(color, self.monochrome)
    }

    pub fn activate(&mut self) {
        self.active = true;
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.selected = 0;
        self.load_files();
        self.filter();
    }

    pub fn deactivate(&mut self) {
        self.active = false;
    }

    fn load_files(&mut self) {
        let files = walk_files(".");
        *self.file_cache.lock().unwrap_or_else(|e| e.into_inner()) = files;
    }

    pub fn char_input(&mut self, c: char) {
        // Convert char-index cursor to byte index for String::insert
        let byte_pos = self
            .query
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.query.len());
        self.query.insert(byte_pos, c);
        self.cursor += 1;
        self.filter();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 && !self.query.is_empty() {
            self.cursor -= 1;
            // Convert char-index cursor to byte index for String::remove
            let byte_pos = self
                .query
                .char_indices()
                .nth(self.cursor)
                .map(|(i, _)| i)
                .unwrap_or(self.query.len());
            self.query.remove(byte_pos);
            self.filter();
        }
    }

    fn filter(&mut self) {
        let cache = self.file_cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.is_empty() {
            self.matches.clear();
            return;
        }
        let query_lower = self.query.to_lowercase();
        self.matches = cache
            .iter()
            .filter(|p| {
                let lower = p.to_string_lossy().to_lowercase();
                lower.contains(&query_lower)
            })
            .take(50)
            .cloned()
            .collect();
        self.selected = 0;
    }

    pub fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.matches.is_empty() {
            self.selected = if self.selected == 0 {
                self.matches.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn selected_path(&self) -> Option<&PathBuf> {
        self.matches.get(self.selected)
    }

    #[cfg(test)]
    pub fn test_set_cache(&mut self, files: Vec<PathBuf>) {
        *self.file_cache.lock().unwrap_or_else(|e| e.into_inner()) = files;
    }

    /// Draw the picker overlay just above the input box.
    ///
    /// `input_top` is the screen row where the input box begins — the picker
    /// stops one row above it. With the multi-line input feature, the input
    /// box can be several rows tall, so we can't hardcode `rows - 2` here
    /// anymore.
    pub fn draw(&self, input_top: u16) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let (cols, _rows) = crossterm::terminal::size()?;
        let mut stdout = std::io::stdout();

        // Bottom anchor row for the picker — one above the input box.
        let bottom_anchor = input_top.saturating_sub(1);
        // Cap list height so we don't run off the top of the screen.
        let max_items = (bottom_anchor as usize).min(10);

        if self.matches.is_empty() {
            stdout.execute(MoveTo(0, bottom_anchor))?;
            write!(
                stdout,
                "{}",
                SetForegroundColor(self.color(Color::DarkGrey))
            )?;
            write!(stdout, "{}", "no matches")?;
            write!(stdout, "{}", ResetColor)?;
            stdout.flush()?;
            return Ok(());
        }

        let list_height = max_items.min(self.matches.len());
        let start_idx = self
            .selected
            .saturating_sub(list_height / 2)
            .min(self.matches.len().saturating_sub(list_height));
        let end_idx = (start_idx + list_height).min(self.matches.len());

        let top_row = bottom_anchor.saturating_sub(list_height as u16);

        for i in start_idx..end_idx {
            let render_row = top_row + (i - start_idx) as u16;
            stdout.execute(MoveTo(0, render_row))?;
            write!(
                stdout,
                "{}",
                Clear(crossterm::terminal::ClearType::CurrentLine)
            )?;

            let path = &self.matches[i];
            let display = path.to_string_lossy();
            let truncated: String = display
                .chars()
                .take(cols.saturating_sub(3) as usize)
                .collect();

            if i == self.selected {
                write!(stdout, "{}", SetForegroundColor(self.color(Color::Green)))?;
                write!(stdout, "▸ {}", truncated)?;
            } else {
                write!(
                    stdout,
                    "{}",
                    SetForegroundColor(self.color(Color::DarkGrey))
                )?;
                write!(stdout, "  {}", truncated)?;
            }
            write!(stdout, "{}", ResetColor)?;
        }
        stdout.flush()?;
        Ok(())
    }
}

fn walk_files(root: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .max_depth(Some(8))
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path
            .components()
            .any(|c| matches!(c, Component::Normal(n) if n.to_string_lossy().starts_with('.')))
        {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let rel = rel.trim_start_matches('/').to_string();
        files.push(PathBuf::from(rel));
        if files.len() >= 200 {
            break;
        }
    }
    files
}
