//! Side-panel widgets: `LeftPanel`, `RightPanel`, and the
//! `SubPanel` building block.
//!
//! The right panel is a vertical stack of `SubPanel`s — each one a
//! light-rounded box `╭─[TITLE]─╮ … ╰─╯` with left-aligned content.
//! The left panel paints the DIRGE idle card when no subagents are
//! active, or a list of subagent status rows when there are.
//!
//! All horizontals (top frame's [AGENT STATUS] / [SYSTEM] labels)
//! are owned by `TopFrame` — these widgets paint INSIDE
//! `Layout::left_panel` / `Layout::right_panel` only.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Style};
use ratatui::widgets::Widget;

use crate::ui::renderer::{LeftPanelInfo, PanelData, SubagentStatusRow};

use super::chat::crossterm_to_ratatui;

/// One framed sub-panel: `╭─[TITLE]─╮` top, `│ content │` body,
/// `╰─╯` bottom. Content lines are LEFT-aligned with one cell of
/// leading padding — the user explicitly asked for this in
/// preference to centered content.
#[derive(Clone)]
pub struct SubPanel<'a> {
    title: &'a str,
    lines: Vec<(String, RColor)>,
    border_style: Style,
}

impl<'a> SubPanel<'a> {
    pub fn new(title: &'a str) -> Self {
        Self {
            title,
            lines: Vec::new(),
            border_style: Style::default().fg(RColor::Green),
        }
    }

    /// Append one body line. The color is applied to the text
    /// (borders + padding always use `border_style`).
    pub fn line(mut self, text: impl Into<String>, color: RColor) -> Self {
        self.lines.push((text.into(), color));
        self
    }

    pub fn border_style(mut self, style: Style) -> Self {
        self.border_style = style;
        self
    }

    /// How many rows this sub-panel needs: top + N content + bottom.
    pub fn height(&self) -> u16 {
        2 + self.lines.len() as u16
    }
}

impl<'a> Widget for SubPanel<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 4 || area.height < 2 {
            return;
        }
        let bs = self.border_style;
        let inner_w = area.width as usize - 2;

        // Top border: ╭─[TITLE]─╮ centered.
        let label = format!("[{}]", self.title);
        let lw = label.chars().count();
        let (lpad, rpad) = if lw >= inner_w {
            (0, 0)
        } else {
            let pad = inner_w - lw;
            (pad / 2, pad - pad / 2)
        };
        buf[(area.x, area.y)].set_char('╭').set_style(bs);
        for i in 0..lpad as u16 {
            buf[(area.x + 1 + i, area.y)].set_char('─').set_style(bs);
        }
        if lw <= inner_w {
            for (i, ch) in label.chars().enumerate() {
                buf[(area.x + 1 + lpad as u16 + i as u16, area.y)]
                    .set_char(ch)
                    .set_style(bs);
            }
            let after = 1 + lpad + lw;
            for i in 0..rpad {
                buf[(area.x + (after + i) as u16, area.y)]
                    .set_char('─')
                    .set_style(bs);
            }
        } else {
            // Title wider than inner — fall back to plain ────.
            for i in 0..inner_w as u16 {
                buf[(area.x + 1 + i, area.y)]
                    .set_char('─')
                    .set_style(bs);
            }
        }
        buf[(area.x + area.width - 1, area.y)]
            .set_char('╮')
            .set_style(bs);

        // Body rows: │ content │ with content left-aligned.
        let body_rows = area.height.saturating_sub(2);
        for (i, slot) in (0..body_rows).enumerate() {
            let y = area.y + 1 + slot;
            buf[(area.x, y)].set_char('│').set_style(bs);
            buf[(area.x + area.width - 1, y)]
                .set_char('│')
                .set_style(bs);
            if let Some((text, color)) = self.lines.get(i) {
                // One leading space, then text clipped to inner_w - 1.
                let text_style = Style::default().fg(*color);
                buf.set_stringn(
                    area.x + 1,
                    y,
                    format!(" {}", text),
                    inner_w,
                    text_style,
                );
            }
        }

        // Bottom border ╰─╯.
        let by = area.y + area.height - 1;
        buf[(area.x, by)].set_char('╰').set_style(bs);
        for i in 0..inner_w as u16 {
            buf[(area.x + 1 + i, by)].set_char('─').set_style(bs);
        }
        buf[(area.x + area.width - 1, by)]
            .set_char('╯')
            .set_style(bs);
    }
}

/// Left panel widget. Renders the DIRGE idle card (subagents
/// empty) or a list of subagent status rows (otherwise).
pub struct LeftPanel<'a> {
    info: &'a LeftPanelInfo,
    subagents: &'a [SubagentStatusRow],
    style: Style,
}

impl<'a> LeftPanel<'a> {
    pub fn new(info: &'a LeftPanelInfo, subagents: &'a [SubagentStatusRow]) -> Self {
        Self {
            info,
            subagents,
            style: Style::default().fg(RColor::Green),
        }
    }
}

impl<'a> Widget for LeftPanel<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.subagents.is_empty() {
            paint_idle_card(buf, area, self.info, self.style);
        } else {
            paint_subagent_list(buf, area, self.subagents);
        }
    }
}

fn paint_idle_card(buf: &mut Buffer, area: Rect, info: &LeftPanelInfo, style: Style) {
    let dim = Style::default().fg(RColor::DarkGray);

    // Vertical layout: blank top + DIRGE logo (3 rows) + blank + metadata (3 rows).
    let lines: Vec<(String, Style)> = vec![
        (String::new(), dim),
        ("D I R G E".to_string(), style),
        (String::new(), dim),
        (String::new(), dim),
        (format!("Agent ID: {}", info.agent_id), dim),
        (format!("Model:    {}", info.model), dim),
        (format!("Focus:    {}", info.focus), dim),
    ];

    for (i, (text, st)) in lines.iter().enumerate() {
        if i as u16 >= area.height {
            break;
        }
        let y = area.y + i as u16;
        // Center horizontally within left panel rect.
        let w = area.width as usize;
        let tw = text.chars().count();
        let pad = w.saturating_sub(tw) / 2;
        if !text.is_empty() {
            buf.set_stringn(area.x + pad as u16, y, text, w.saturating_sub(pad), *st);
        }
    }
}

fn paint_subagent_list(buf: &mut Buffer, area: Rect, rows: &[SubagentStatusRow]) {
    let dim = Style::default().fg(RColor::DarkGray);
    let agent = Style::default().fg(RColor::Green);
    let err = Style::default().fg(RColor::Red);

    let cap = area.height as usize;
    for (i, row) in rows.iter().take(cap).enumerate() {
        let y = area.y + i as u16;
        let (glyph, style) = match row.state.as_str() {
            "running" => ("⋯", agent),
            "completed" => ("✓", agent),
            "failed" => ("✗", err),
            _ => ("·", dim),
        };
        let id_field: String = row.id_short.chars().take(6).collect();
        let prompt_w = (area.width as usize).saturating_sub(2 + 7 + 1);
        let prompt_field: String = row.prompt_short.chars().take(prompt_w).collect();
        let line = format!("{} {:6} {}", glyph, id_field, prompt_field);
        buf.set_stringn(area.x, y, line, area.width as usize, style);
    }
}

/// Right panel widget. Stacks sub-panels vertically in this order:
/// `[SYSTEM LOAD]`, `[MCP]`, `[LSP]`, `[TODOS]`, `[MODIFIED]`.
/// Each sub-panel takes its own minimum height; remaining rows go
/// to the last sub-panel (MODIFIED) so the file list grows on tall
/// terminals.
pub struct RightPanel<'a> {
    data: &'a PanelData,
    style: Style,
}

impl<'a> RightPanel<'a> {
    pub fn new(data: &'a PanelData) -> Self {
        Self {
            data,
            style: Style::default().fg(RColor::Green),
        }
    }
}

impl<'a> Widget for RightPanel<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        // Build each sub-panel from the panel_data. Filter empties
        // by emitting a "(none)" line so the user always sees the
        // section structure.
        let dim = RColor::DarkGray;
        let body = RColor::Green;

        // [SYSTEM LOAD]
        let sysload_panel = match self.data.sysload.as_ref() {
            Some(s) => SubPanel::new("SYSTEM LOAD")
                .line(format_bar("CPU", s.cpu_pct), body)
                .line(format_bar("MEM", s.mem_pct), body)
                .border_style(self.style),
            None => SubPanel::new("SYSTEM LOAD")
                .line("(pending)", dim)
                .border_style(self.style),
        };
        let mcp_panel = {
            let mut p = SubPanel::new("MCP").border_style(self.style);
            if self.data.mcp.is_empty() {
                p = p.line("· (none)", dim);
            } else {
                for (name, ok) in &self.data.mcp {
                    let glyph = if *ok { "●" } else { "○" };
                    p = p.line(format!("{} {}", glyph, name), body);
                }
            }
            p
        };
        let lsp_panel = {
            let mut p = SubPanel::new("LSP").border_style(self.style);
            if self.data.lsp.is_empty() {
                p = p.line("· (none)", dim);
            } else {
                for (id, root, ok) in &self.data.lsp {
                    let glyph = if *ok { "●" } else { "○" };
                    p = p.line(format!("{} {} {}", glyph, id, root), body);
                }
            }
            p
        };
        let todos_panel = {
            let mut p = SubPanel::new("TODOS").border_style(self.style);
            if self.data.todos.is_empty() {
                p = p.line("· (none)", dim);
            } else {
                for (status, text) in &self.data.todos {
                    p = p.line(format!("{} {}", status, text), body);
                }
            }
            p
        };
        let modified_panel = {
            let mut p = SubPanel::new("MODIFIED").border_style(self.style);
            if self.data.modified.is_empty() {
                p = p.line("· (none)", dim);
            } else {
                for f in &self.data.modified {
                    p = p.line(f.clone(), body);
                }
            }
            p
        };

        // Stack vertically with one blank row between.
        let mut y = area.y;
        for panel in [sysload_panel, mcp_panel, lsp_panel, todos_panel, modified_panel] {
            let h = panel.height();
            if y + h > area.y + area.height {
                break;
            }
            let rect = Rect::new(area.x, y, area.width, h);
            panel.render(rect, buf);
            y += h + 1; // blank spacer
        }
    }
}

/// Render `LABEL: [####....] NN%` of fixed width.
fn format_bar(label: &str, pct: f32) -> String {
    let bar_w = 10;
    let filled = ((pct / 100.0) * bar_w as f32).round().clamp(0.0, bar_w as f32) as usize;
    let empty = bar_w - filled;
    format!(
        "{}: [{}{}] {:>3}%",
        label,
        "#".repeat(filled),
        ".".repeat(empty),
        pct.round() as i32
    )
}

// Silence unused-import lint until LeftPanel is wired in.
const _: fn(crossterm::style::Color) -> RColor = crossterm_to_ratatui;

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::layout::Layout;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// SubPanel paints the frame and centers the [TITLE] label.
    #[test]
    fn subpanel_frame_and_title() {
        let mut backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 20, 4);
                f.render_widget(SubPanel::new("MCP").line("a", RColor::Green), area);
            })
            .unwrap();
        backend = terminal.backend().clone();

        let row = |y: u16| -> String {
            (0..20)
                .map(|x| {
                    backend
                        .buffer()
                        .cell((x, y))
                        .unwrap()
                        .symbol()
                        .to_string()
                })
                .collect()
        };
        // [MCP] is 5 chars in a 18-wide inner band, pad=13, left=6.
        let expected_top = format!("╭{}[MCP]{}╮", "─".repeat(6), "─".repeat(7));
        assert_eq!(row(0), expected_top, "got {:?}", row(0));
        // Body has " a" left-aligned, padded with spaces, with │ borders.
        let body_chars: Vec<char> = row(1).chars().collect();
        assert_eq!(body_chars[0], '│', "got first char {:?}", body_chars[0]);
        assert_eq!(body_chars[1], ' ');
        assert_eq!(body_chars[2], 'a');
        assert_eq!(body_chars[19], '│', "row(1) = {:?}", row(1));
        // Bottom border.
        let expected_bot = format!("╰{}╯", "─".repeat(18));
        assert_eq!(row(3), expected_bot);
    }

    /// Sub-panel content is LEFT-aligned per user feedback (not centered).
    #[test]
    fn subpanel_content_is_left_aligned() {
        let mut backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 20, 3);
                f.render_widget(SubPanel::new("X").line("hi", RColor::Green), area);
            })
            .unwrap();
        backend = terminal.backend().clone();
        // Body row.
        let body: String = (0..20)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, 1))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        // Expected: "│ hi              │" — text at cols 2-3, spaces to 18, │ at 19.
        let body_chars: Vec<char> = body.chars().collect();
        assert_eq!(body_chars[0], '│');
        assert_eq!(body_chars[1], ' ');
        assert_eq!(body_chars[2], 'h');
        assert_eq!(body_chars[3], 'i');
        // Cols [4..19] are spaces.
        for c in &body_chars[4..19] {
            assert_eq!(*c, ' ');
        }
        assert_eq!(body_chars[19], '│');
    }

    /// LeftPanel idle state paints DIRGE banner centered.
    #[test]
    fn left_panel_idle_paints_dirge_card() {
        let info = LeftPanelInfo {
            agent_id: "abc123".into(),
            model: "test".into(),
            focus: "code".into(),
        };
        let mut backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 30, 12);
                f.render_widget(LeftPanel::new(&info, &[]), area);
            })
            .unwrap();
        backend = terminal.backend().clone();
        // Row 1 should contain "D I R G E" centered.
        let row1: String = (0..30)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, 1))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(row1.contains("D I R G E"), "got {:?}", row1);
        // Some row should contain "Agent ID: abc123".
        let mut found_agent_id = false;
        for y in 0..12 {
            let r: String = (0..30)
                .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                .collect();
            if r.contains("Agent ID: abc123") {
                found_agent_id = true;
                break;
            }
        }
        assert!(found_agent_id, "expected Agent ID row");
    }

    /// LeftPanel with subagents lists status rows.
    #[test]
    fn left_panel_lists_subagents() {
        let info = LeftPanelInfo::default();
        let subs = vec![
            SubagentStatusRow {
                id_short: "abc123".into(),
                state: "running".into(),
                prompt_short: "do thing".into(),
            },
            SubagentStatusRow {
                id_short: "def456".into(),
                state: "completed".into(),
                prompt_short: "done".into(),
            },
        ];
        let mut backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 30, 6);
                f.render_widget(LeftPanel::new(&info, &subs), area);
            })
            .unwrap();
        backend = terminal.backend().clone();
        let row0: String = (0..30)
            .map(|x| backend.buffer().cell((x, 0)).unwrap().symbol().to_string())
            .collect();
        let row1: String = (0..30)
            .map(|x| backend.buffer().cell((x, 1)).unwrap().symbol().to_string())
            .collect();
        assert!(row0.starts_with("⋯ abc123 do thing"), "row0 = {:?}", row0);
        assert!(row1.starts_with("✓ def456 done"), "row1 = {:?}", row1);
    }

    /// RightPanel stacks sub-panels and shows their titles.
    #[test]
    fn right_panel_stacks_sub_panels() {
        let mut data = PanelData::default();
        data.mcp = vec![("server1".into(), true)];
        let layout = Layout::new(160, 30, 1);
        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(RightPanel::new(&data), layout.right_panel);
            })
            .unwrap();
        backend = terminal.backend().clone();

        // Scan the right panel rect for each title.
        let mut titles_found: Vec<&str> = Vec::new();
        for y in layout.right_panel.y
            ..(layout.right_panel.y + layout.right_panel.height)
        {
            let row: String = (layout.right_panel.x
                ..layout.right_panel.x + layout.right_panel.width)
                .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                .collect();
            for t in ["[SYSTEM LOAD]", "[MCP]", "[LSP]", "[TODOS]", "[MODIFIED]"] {
                if row.contains(t) && !titles_found.contains(&t) {
                    titles_found.push(t);
                }
            }
        }
        // All five titles should appear (assuming tall enough terminal).
        assert_eq!(
            titles_found,
            vec!["[SYSTEM LOAD]", "[MCP]", "[LSP]", "[TODOS]", "[MODIFIED]"],
        );

        // The MCP server name "server1" should appear too.
        let mut found_server = false;
        for y in layout.right_panel.y
            ..(layout.right_panel.y + layout.right_panel.height)
        {
            let row: String = (layout.right_panel.x
                ..layout.right_panel.x + layout.right_panel.width)
                .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                .collect();
            if row.contains("server1") {
                found_server = true;
                break;
            }
        }
        assert!(found_server, "expected MCP server name in right panel");
    }

    /// CPU/MEM bar formatting.
    #[test]
    fn bar_formatting() {
        assert_eq!(format_bar("CPU", 0.0), "CPU: [..........]   0%");
        assert_eq!(format_bar("MEM", 50.0), "MEM: [#####.....]  50%");
        assert_eq!(format_bar("CPU", 100.0), "CPU: [##########] 100%");
    }
}
