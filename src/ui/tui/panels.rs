//! Side-panel widgets: `LeftPanel`, `RightPanel`, and the
//! `SubPanel` building block.
//!
//! The right panel is a vertical stack of `SubPanel`s — each one a
//! light-rounded box `╭─[TITLE]─╮ … ╰─╯` with left-aligned content.
//! The left panel paints the session vitals (CONTEXT / ACTIVITY / GIT);
//! when subagents are running it renders their status rows BELOW the
//! vitals rather than replacing them.
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
                buf[(area.x + 1 + i, area.y)].set_char('─').set_style(bs);
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
                buf.set_stringn(area.x + 1, y, format!(" {}", text), inner_w, text_style);
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

/// Left panel widget. Always renders the session vitals (context gauge,
/// activity ticker, git); when subagents are running, their status rows
/// render in a reserved region BELOW the vitals.
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

    /// Override the frame/border style so the left panel tracks the
    /// main chat frame's color (the theme header tone). Default is
    /// green to match the phosphor preset.
    pub fn border_style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

impl<'a> Widget for LeftPanel<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.subagents.is_empty() {
            paint_idle_card(buf, area, self.info, self.style);
            return;
        }
        // Running subagents render BELOW the vitals (CONTEXT/ACTIVITY/GIT),
        // not in place of them. Reserve the subagents' natural height,
        // capped at ~half the panel so the vitals always keep room; the
        // vitals lay out into the top region and self-clip.
        let natural_sub: u16 = LEFT_PANEL_TOP_PAD
            + self
                .subagents
                .iter()
                .map(|r| 2 + r.files.len() as u16)
                .sum::<u16>();
        let sub_h = natural_sub.min(area.height / 2);
        if sub_h == 0 {
            // Panel too short to host both — keep the vitals.
            paint_idle_card(buf, area, self.info, self.style);
            return;
        }
        let vitals_h = area.height - sub_h;
        paint_idle_card(
            buf,
            Rect::new(area.x, area.y, area.width, vitals_h),
            self.info,
            self.style,
        );
        // Subagent region: a dim "agents" label on its pad row, then the
        // status rows (paint_subagent_list starts content one row down).
        let sub_area = Rect::new(area.x, area.y + vitals_h, area.width, sub_h);
        buf.set_stringn(
            sub_area.x,
            sub_area.y,
            "── agents ──",
            sub_area.width as usize,
            Style::default().fg(RColor::DarkGray),
        );
        paint_subagent_list(buf, sub_area, self.subagents);
    }
}

/// One row of top padding so the left-panel content doesn't sit
/// flush against the unified top frame. Matches the right panel's
/// symmetric padding for visual balance.
const LEFT_PANEL_TOP_PAD: u16 = 1;

/// Compact token count: `12.3k` / `980`.
fn kfmt(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn paint_idle_card(buf: &mut Buffer, area: Rect, info: &LeftPanelInfo, style: Style) {
    let dim = RColor::DarkGray;
    let warn = RColor::Yellow;
    let green = RColor::Green;
    let panel_w = area.width as usize;
    // Leave one trailing cell so a sub-panel's right border doesn't abut
    // the chat-frame divider (same caution as the subagent list).
    let box_w = area.width.saturating_sub(1);
    // Sub-panel borders follow the caller's frame style (the main
    // chat frame's color) so the left panel matches the theme rather
    // than locking to green.
    let bs = style;

    let mut dy = LEFT_PANEL_TOP_PAD;

    // DIRGE banner (centered). Identity (model/prompt) lives in the
    // status line, so it's not repeated here.
    let banner = "D I R G E";
    if dy < area.height {
        let bw = banner.chars().count();
        let bpad = panel_w.saturating_sub(bw) / 2;
        buf.set_stringn(
            area.x + bpad as u16,
            area.y + dy,
            banner,
            panel_w.saturating_sub(bpad),
            style,
        );
    }
    dy += 2;

    // Helper: render a SubPanel of `lines` at the current `dy` if it
    // fits, advancing `dy` past it + a 1-row spacer. No-op when out of
    // vertical room.
    let place = |buf: &mut Buffer, dy: &mut u16, title: &str, lines: Vec<(String, RColor)>| {
        let h = 2 + lines.len() as u16;
        if box_w < 4 || area.y + *dy + h > area.y + area.height {
            return;
        }
        let mut sp = SubPanel::new(title).border_style(bs);
        for (t, c) in lines {
            sp = sp.line(t, c);
        }
        sp.render(Rect::new(area.x, area.y + *dy, box_w, h), buf);
        *dy += h + 1;
    };

    // [CONTEXT] — fill bar + tokens/window + compaction count.
    let g = &info.context;
    let mut ctx_lines = vec![
        (
            format_bar("ctx", g.pct as f32),
            if g.fold_soon { warn } else { green },
        ),
        (
            format!("{}/{}  cmp:{}", kfmt(g.used), kfmt(g.window), g.compactions),
            dim,
        ),
    ];
    if g.fold_soon {
        ctx_lines.push(("⚠ compaction soon".to_string(), warn));
    }
    place(buf, &mut dy, "CONTEXT", ctx_lines);

    // [GIT] — branch + dirty counts + last commit (only when in a repo).
    // Rendered before activity is sized so activity can take the slack.
    let git_lines: Option<Vec<(String, RColor)>> = info.git.as_ref().map(|gs| {
        let mut v = vec![
            (
                format!(
                    "⎇ {}",
                    if gs.branch.is_empty() {
                        "?"
                    } else {
                        &gs.branch
                    }
                ),
                green,
            ),
            (
                format!("+{} ~{} ?{}", gs.staged, gs.unstaged, gs.untracked),
                if gs.staged + gs.unstaged + gs.untracked == 0 {
                    dim
                } else {
                    warn
                },
            ),
        ];
        if !gs.last_commit.is_empty() {
            v.push((gs.last_commit.clone(), dim));
        }
        v
    });
    let git_reserve = git_lines
        .as_ref()
        .map(|v| 2 + v.len() as u16 + 1)
        .unwrap_or(0);

    // [ACTIVITY] — recent tool ticker (newest last). Capped to whatever
    // vertical room is left after CONTEXT and the reserved GIT box. Only
    // rendered when at least one content row fits AFTER reserving GIT —
    // otherwise a forced 1-row activity/idle box would steal GIT's
    // reserved space and silently drop the [GIT] section on short panels.
    let avail = (area.y + area.height)
        .saturating_sub(area.y + dy)
        .saturating_sub(git_reserve);
    let max_act = avail.saturating_sub(2) as usize; // minus the box borders
    if max_act >= 1 {
        let act_lines: Vec<(String, RColor)> = if info.activity.is_empty() {
            vec![("· idle".to_string(), dim)]
        } else {
            info.activity
                .iter()
                .rev()
                .take(max_act)
                .rev()
                .map(|a| (a.clone(), dim))
                .collect()
        };
        place(buf, &mut dy, "ACTIVITY", act_lines);
    }

    if let Some(lines) = git_lines {
        place(buf, &mut dy, "GIT", lines);
    }
}

fn paint_subagent_list(buf: &mut Buffer, area: Rect, rows: &[SubagentStatusRow]) {
    let dim = Style::default().fg(RColor::DarkGray);
    let agent = Style::default().fg(RColor::Green);
    let err = Style::default().fg(RColor::Red);
    let file_style = Style::default().fg(RColor::DarkGray);

    // Format: hash line + prompt line (+ file lines if present).
    // Reserve one trailing cell so text doesn't run into the
    // chat-frame divider on the right.
    let id_indent = 3_u16; // indent for hash line after glyph
    let file_indent = 5_u16; // indent for file lines under hash
    let trailing_pad = 1_usize;
    let cap_rows = area.height.saturating_sub(LEFT_PANEL_TOP_PAD) as usize;
    let mut dy: u16 = LEFT_PANEL_TOP_PAD;
    for row in rows {
        let file_lines = &row.files;
        let row_height = 2_u16 + file_lines.len() as u16;
        if (dy + row_height - LEFT_PANEL_TOP_PAD) as usize > cap_rows {
            break;
        }
        let (glyph, style) = match row.state.as_str() {
            "running" => ("⋯", agent),
            "completed" => ("✓", agent),
            "failed" => ("✗", err),
            _ => ("·", dim),
        };
        // Hash line: glyph + " ..." + last 6 chars of id_short.
        let id_tail: String = row
            .id_short
            .chars()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let hash_line = format!("{} ...{}", glyph, id_tail);
        let hash_w = (area.width as usize).saturating_sub(trailing_pad);
        buf.set_stringn(area.x, area.y + dy, hash_line, hash_w, style);
        // Prompt line: indented, dim, truncated to fit width.
        let prompt_avail = (area.width as usize)
            .saturating_sub(id_indent as usize)
            .saturating_sub(trailing_pad);
        let prompt_field: String = row.prompt_short.chars().take(prompt_avail).collect();
        buf.set_stringn(
            area.x + id_indent,
            area.y + dy + 1,
            prompt_field,
            prompt_avail,
            dim,
        );
        dy += 2;
        // File lines: indented further, dim, one per file.
        for file in file_lines {
            let file_avail = (area.width as usize)
                .saturating_sub(file_indent as usize)
                .saturating_sub(trailing_pad);
            let file_field: String = if file.len() <= file_avail {
                file.clone()
            } else {
                // Left-truncate to preserve basename.
                format!("…{}", crate::text::tail(file, file_avail.saturating_sub(1)))
            };
            buf.set_stringn(
                area.x + file_indent,
                area.y + dy,
                file_field,
                file_avail,
                file_style,
            );
            dy += 1;
        }
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
    /// dirge-b11: rows to skip from the top of the MODIFIED list.
    /// Clamped at render time to `total - visible_rows` so the
    /// caller doesn't have to know the visible budget.
    modified_offset: usize,
}

impl<'a> RightPanel<'a> {
    pub fn new(data: &'a PanelData) -> Self {
        Self {
            data,
            style: Style::default().fg(RColor::Green),
            modified_offset: 0,
        }
    }

    /// Override the frame/border style so the right panel tracks the
    /// main chat frame's color (the theme header tone). Default is
    /// green to match the phosphor preset. Body text keeps its own
    /// semantic colors (amber SYSTEM data, dim placeholders).
    pub fn border_style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// dirge-b11: set the MODIFIED-list scroll offset. Clamped
    /// against the list's length at render time so a stale offset
    /// never points past the end.
    pub fn modified_offset(mut self, offset: usize) -> Self {
        self.modified_offset = offset;
        self
    }
}

/// dirge-b11: where would the MODIFIED sub-panel land if `RightPanel`
/// were rendered into `area` right now? Returns `None` when the
/// right panel collapses (too narrow / not enough vertical room).
///
/// Mirrors the layout math inside `RightPanel::render`: the four
/// fixed sub-panels (SYSTEM LOAD, MCP, LSP, TODOS) take their
/// natural height with a one-row spacer between, then MODIFIED
/// fills whatever's left. Used by the UI loop's mouse handler to
/// hit-test wheel events against the modified region.
///
/// Keeping this as a free function (rather than a method on
/// RightPanel) means the hit-test path doesn't have to construct
/// a throwaway widget — just call with `(data, layout.right_panel)`.
pub fn compute_modified_rect(data: &PanelData, area: Rect) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    // Heights of the four fixed sub-panels (top + N content + bot
    // = 2 + lines). Matches SubPanel::height(); empty sections use
    // a single "· (none)" row.
    let sysload_lines = if data.sysload.is_some() { 2 } else { 1 };
    let mcp_lines = data.mcp.len().max(1);
    let lsp_lines = data.lsp.len().max(1);
    let todos_lines = data.todos.len().max(1);
    let mut y = area.y + RIGHT_PANEL_TOP_PAD;
    for body_lines in [sysload_lines, mcp_lines, lsp_lines, todos_lines] {
        let h = 2 + body_lines as u16;
        if y + h > area.y + area.height {
            return None;
        }
        y += h + 1; // blank spacer
    }
    let remaining = (area.y + area.height).saturating_sub(y);
    if remaining < 3 {
        return None;
    }
    // dirge-sb2n: size to content like TODOS — two border rows plus one
    // row per modified file (min 1 for the "(none)" placeholder) — capped
    // at the space that actually remains. So an empty list is a single
    // line, not a tall blank box, and the panel grows as files are added;
    // only when the list overflows does it fill `remaining` and switch to
    // the scroll-footer view. Mirrored in `RightPanel::render`.
    let natural = (2 + data.modified.len().max(1)) as u16;
    let height = natural.min(remaining);
    let inner_w = area.width.saturating_sub(RIGHT_PANEL_TRAILING_PAD);
    Some(Rect::new(area.x, y, inner_w, height))
}

/// Right-panel top padding (rows). Mirrors LEFT_PANEL_TOP_PAD so
/// the two sides line up against the unified top frame.
const RIGHT_PANEL_TOP_PAD: u16 = 1;
/// One cell of trailing padding inside sub-panel content so it
/// doesn't run flush against the right │ border.
const RIGHT_PANEL_TRAILING_PAD: u16 = 1;
/// Amber tone — used for the [SYSTEM] title in the unified top
/// frame and for all body text inside the right panel.
const AMBER: RColor = RColor::Rgb(255, 191, 0);

impl<'a> Widget for RightPanel<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        // Body text in the [SYSTEM] pane is amber per spec.
        let dim = RColor::DarkGray;
        let body = AMBER;

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
        // MODIFIED is built below with knowledge of the remaining
        // row budget — keep it out of the fixed-height stack.

        // Stack vertically with one blank row between. Top padding
        // pushes the first sub-panel down by RIGHT_PANEL_TOP_PAD
        // rows, and `inner_w = area.width - RIGHT_PANEL_TRAILING_PAD`
        // leaves a one-cell margin so content doesn't run into the
        // outer divider on the right edge.
        let mut y = area.y + RIGHT_PANEL_TOP_PAD;
        let inner_w = area.width.saturating_sub(RIGHT_PANEL_TRAILING_PAD);
        // First four sub-panels get their natural height. MODIFIED
        // grows to fill the remaining vertical space (with a
        // `+N older` footer when truncated) — same growth model
        // the legacy panel had.
        let fixed = [sysload_panel, mcp_panel, lsp_panel, todos_panel];
        for panel in fixed {
            let h = panel.height();
            if y + h > area.y + area.height {
                break;
            }
            let rect = Rect::new(area.x, y, inner_w, h);
            panel.render(rect, buf);
            y += h + 1; // blank spacer
        }
        // MODIFIED: take whatever vertical room is left.
        let modified_top = y;
        let remaining = (area.y + area.height).saturating_sub(modified_top);
        if remaining >= 3 {
            let total = self.data.modified.len();
            // dirge-sb2n: size to content (2 borders + one row per file,
            // min 1 for "(none)") capped at the remaining space, matching
            // TODOS — an empty list is a single line, not a tall blank
            // box. Mirrors `compute_modified_rect`. Only when the list
            // overflows does `height == remaining` and the scroll-footer
            // path below take over.
            let height = ((2 + total.max(1)) as u16).min(remaining);
            let rect = Rect::new(area.x, modified_top, inner_w, height);
            let inner_rows = (height as usize).saturating_sub(2);
            let mut p = SubPanel::new("MODIFIED").border_style(self.style);
            if total == 0 {
                p = p.line("· (none)", dim);
            } else if total <= inner_rows {
                for f in &self.data.modified {
                    p = p.line(f.clone(), body);
                }
            } else {
                // dirge-b11: list overflows. Reserve last row for
                // the scroll footer and slide the visible window
                // by `modified_offset` rows. Clamp here so a stale
                // offset never reads past the end. The Renderer
                // performs the same clamp in tui_redraw, so this
                // is mainly defensive for direct-Scene callers
                // (tests).
                let head_rows = inner_rows.saturating_sub(1);
                let max_off = total.saturating_sub(head_rows);
                let offset = self.modified_offset.min(max_off);
                let end = (offset + head_rows).min(total);
                for f in self.data.modified.iter().take(end).skip(offset) {
                    p = p.line(f.clone(), body);
                }
                // Footer: when offset > 0 show both directions
                // (`↑ N newer / ↓ M older`); when offset == 0 keep
                // the original `+N older` shape for backwards
                // compatibility with screenshots / tests.
                let newer = offset;
                let older = total.saturating_sub(end);
                let footer = if newer > 0 {
                    format!("↑ {} newer / ↓ {} older", newer, older)
                } else {
                    format!("+{} older", older)
                };
                p = p.line(footer, dim);
            }
            p.render(rect, buf);
        }
    }
}

#[cfg(feature = "dap")]
pub mod debug {
    //! Debug panel widget — renders DAP session state.
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color as RColor, Style};
    use ratatui::widgets::Widget;

    use crate::dap::types::*;

    use super::SubPanel;

    const AMBER: RColor = RColor::Rgb(255, 191, 0);
    const RIGHT_PANEL_TOP_PAD: u16 = 1;
    const RIGHT_PANEL_TRAILING_PAD: u16 = 1;

    /// Right panel widget that shows DAP debug session state.
    pub struct DebugRightPanel<'a> {
        data: &'a DebugPanelData,
        style: Style,
    }

    impl<'a> DebugRightPanel<'a> {
        pub fn new(data: &'a DebugPanelData) -> Self {
            Self {
                data,
                style: Style::default().fg(RColor::Green),
            }
        }
    }

    impl<'a> Widget for DebugRightPanel<'a> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            if area.width == 0 || area.height == 0 {
                return;
            }

            let dim = RColor::DarkGray;
            let body_color = AMBER;

            let mut y = area.y + RIGHT_PANEL_TOP_PAD;
            let inner_w = area.width.saturating_sub(RIGHT_PANEL_TRAILING_PAD);

            let summary = self.data.session_summary.as_ref();

            // [DEBUG] — session status
            let debug_panel = {
                let mut p = SubPanel::new("DEBUG").border_style(self.style);
                if let Some(s) = summary {
                    p = p.line(format!("status: {:?}", s.status), body_color);
                    p = p.line(format!("adapter: {}", s.adapter_name), body_color);
                    if let Some(ref reason) = s.stop_reason {
                        p = p.line(format!("reason: {}", reason), body_color);
                    }
                    if let Some(tid) = s.thread_id {
                        p = p.line(format!("thread: {}", tid), body_color);
                    }
                } else {
                    p = p.line("· (no session)", dim);
                }
                p
            };
            let h = debug_panel.height();
            if y + h <= area.y + area.height {
                debug_panel.render(Rect::new(area.x, y, inner_w, h), buf);
                y += h + 1;
            }

            // [THREADS]
            let threads_panel = {
                let mut p = SubPanel::new("THREADS").border_style(self.style);
                if self.data.threads.is_empty() {
                    p = p.line("· (pending)", dim);
                } else {
                    for t in &self.data.threads {
                        p = p.line(format!("{} {}", t.id, t.name), body_color);
                    }
                }
                p
            };
            let h = threads_panel.height();
            if y + h <= area.y + area.height {
                threads_panel.render(Rect::new(area.x, y, inner_w, h), buf);
                y += h + 1;
            }

            // [FRAMES]
            let frames_panel = {
                let mut p = SubPanel::new("FRAMES").border_style(self.style);
                if self.data.frames.is_empty() {
                    p = p.line("· (pending)", dim);
                } else {
                    for f in &self.data.frames {
                        let src = f
                            .source
                            .as_ref()
                            .and_then(|s| s.name.as_deref())
                            .unwrap_or("??");
                        p = p.line(
                            format!("{} {}:{} {}", f.id, src, f.line, f.name),
                            body_color,
                        );
                    }
                }
                p
            };
            let h = frames_panel.height();
            if y + h <= area.y + area.height {
                frames_panel.render(Rect::new(area.x, y, inner_w, h), buf);
                y += h + 1;
            }

            // [VARIABLES]
            let variables_panel = {
                let mut p = SubPanel::new("VARIABLES").border_style(self.style);
                if self.data.variables.is_empty() {
                    p = p.line("· (pending)", dim);
                } else {
                    for v in &self.data.variables {
                        let val = &v.value;
                        let type_hint = v
                            .type_field
                            .as_deref()
                            .map(|t| format!(": {t}"))
                            .unwrap_or_default();
                        p = p.line(format!("{} = {}{}", v.name, val, type_hint), body_color);
                    }
                }
                p
            };
            let h = variables_panel.height();
            if y + h <= area.y + area.height {
                variables_panel.render(Rect::new(area.x, y, inner_w, h), buf);
                y += h + 1;
            }

            // [BREAKPOINTS]
            let bp_count = summary.map(|s| s.breakpoint_count).unwrap_or(0);
            let fbp_count = summary.map(|s| s.function_breakpoint_count).unwrap_or(0);
            let bp_panel = SubPanel::new("BREAKPOINTS")
                .line(
                    format!("source: {}  func: {}", bp_count, fbp_count),
                    body_color,
                )
                .border_style(self.style);
            let h = bp_panel.height();
            if y + h <= area.y + area.height {
                bp_panel.render(Rect::new(area.x, y, inner_w, h), buf);
                y += h + 1;
            }

            // [OUTPUT] — grow to fill remaining vertical space.
            let remaining = (area.y + area.height).saturating_sub(y);
            if remaining >= 3 {
                let rect = Rect::new(area.x, y, inner_w, remaining);
                let inner_rows = (remaining as usize).saturating_sub(2);
                let mut p = SubPanel::new("OUTPUT").border_style(self.style);
                let output = &self.data.output;
                if output.is_empty() {
                    p = p.line("· (none)", dim);
                } else {
                    let lines: Vec<&str> = output.lines().collect();
                    let total = lines.len();
                    if total <= inner_rows {
                        for line in &lines {
                            p = p.line(*line, body_color);
                        }
                    } else {
                        for line in lines.iter().take(inner_rows.saturating_sub(1)) {
                            p = p.line(*line, body_color);
                        }
                        let footer = if self.data.output_truncated {
                            format!("+{} more (truncated)", total.saturating_sub(inner_rows - 1))
                        } else {
                            format!("+{} more", total.saturating_sub(inner_rows - 1))
                        };
                        p = p.line(footer, dim);
                    }
                }
                p.render(rect, buf);
            }
        }
    }
}

/// Render `LABEL: [####....] NN%` of fixed width.
fn format_bar(label: &str, pct: f32) -> String {
    let bar_w = 10;
    let filled = ((pct / 100.0) * bar_w as f32)
        .round()
        .clamp(0.0, bar_w as f32) as usize;
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
    use super::super::layout::Layout;
    use super::*;
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
                .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
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

    /// The left + right panels' frame borders follow the border
    /// style passed by the caller (the main chat frame's color),
    /// not a hardcoded green. Guards the "panels match the main
    /// panel's color scheme" contract.
    #[test]
    fn side_panels_borders_follow_caller_style() {
        let magenta = RColor::Magenta;
        let style = Style::default().fg(magenta);

        // Find the first rounded corner / vertical border cell in a
        // region and return its fg color.
        let border_fg = |backend: &TestBackend, area: Rect| -> Option<RColor> {
            for y in area.y..area.y + area.height {
                for x in area.x..area.x + area.width {
                    let cell = backend.buffer().cell((x, y)).unwrap();
                    if matches!(cell.symbol(), "╭" | "╮" | "╰" | "╯" | "│") {
                        return Some(cell.fg);
                    }
                }
            }
            None
        };

        // Left panel.
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let mut backend = TestBackend::new(24, 20);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        let area = Rect::new(0, 0, 24, 20);
        terminal
            .draw(|f| {
                f.render_widget(LeftPanel::new(&info, &subs).border_style(style), area);
            })
            .unwrap();
        backend = terminal.backend().clone();
        assert_eq!(
            border_fg(&backend, area),
            Some(magenta),
            "left panel border should follow caller style"
        );

        // Right panel.
        let pd = PanelData::default();
        let mut backend = TestBackend::new(24, 24);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        let area = Rect::new(0, 0, 24, 24);
        terminal
            .draw(|f| {
                f.render_widget(RightPanel::new(&pd).border_style(style), area);
            })
            .unwrap();
        backend = terminal.backend().clone();
        assert_eq!(
            border_fg(&backend, area),
            Some(magenta),
            "right panel border should follow caller style"
        );
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
            .map(|x| backend.buffer().cell((x, 1)).unwrap().symbol().to_string())
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

    /// LeftPanel idle state paints the DIRGE banner + the vitals
    /// sections (CONTEXT / ACTIVITY / GIT) with live data.
    #[test]
    fn left_panel_idle_paints_vitals() {
        use crate::ui::panel_data::{ContextGauge, GitSnapshot};
        let info = LeftPanelInfo {
            context: ContextGauge {
                used: 12_300,
                window: 128_000,
                pct: 80,
                compactions: 2,
                fold_soon: true,
            },
            activity: vec!["read run.rs".into(), "bash cargo test".into()],
            git: Some(GitSnapshot {
                branch: "main".into(),
                staged: 1,
                unstaged: 2,
                untracked: 0,
                last_commit: "wip".into(),
            }),
        };
        let backend = TestBackend::new(30, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(LeftPanel::new(&info, &[]), Rect::new(0, 0, 30, 30));
            })
            .unwrap();
        let backend = terminal.backend().clone();
        let dump: String = (0..30)
            .map(|y| {
                (0..30)
                    .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("D I R G E"), "banner missing:\n{dump}");
        assert!(dump.contains("CONTEXT"), "context section missing:\n{dump}");
        assert!(dump.contains("80%"), "context pct missing:\n{dump}");
        assert!(
            dump.contains("compaction soon"),
            "fold warning missing:\n{dump}"
        );
        assert!(
            dump.contains("ACTIVITY"),
            "activity section missing:\n{dump}"
        );
        assert!(
            dump.contains("cargo test"),
            "activity entry missing:\n{dump}"
        );
        assert!(dump.contains("GIT"), "git section missing:\n{dump}");
        assert!(dump.contains("main"), "git branch missing:\n{dump}");
        assert!(dump.contains("+1 ~2 ?0"), "git counts missing:\n{dump}");
    }

    /// Regression: on a SHORT panel the GIT section must still render —
    /// the ACTIVITY box must not steal GIT's reserved rows.
    #[test]
    fn left_panel_short_keeps_git() {
        use crate::ui::panel_data::{ContextGauge, GitSnapshot};
        let info = LeftPanelInfo {
            context: ContextGauge {
                used: 1000,
                window: 128_000,
                pct: 10,
                compactions: 0,
                fold_soon: false,
            },
            activity: vec!["read a.rs".into(), "edit b.rs".into()],
            git: Some(GitSnapshot {
                branch: "main".into(),
                staged: 0,
                unstaged: 1,
                untracked: 0,
                last_commit: "x".into(),
            }),
        };
        // 15 rows is enough for CONTEXT + GIT but not a forced ACTIVITY box.
        let backend = TestBackend::new(28, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| f.render_widget(LeftPanel::new(&info, &[]), Rect::new(0, 0, 28, 15)))
            .unwrap();
        let backend = terminal.backend().clone();
        let dump: String = (0..15)
            .map(|y| {
                (0..28)
                    .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("CONTEXT"), "context missing:\n{dump}");
        assert!(
            dump.contains("GIT") && dump.contains("main"),
            "GIT dropped on a short panel:\n{dump}"
        );
    }

    /// LeftPanel with subagents renders the vitals AND the subagent rows
    /// BELOW them (subagents no longer replace the vitals).
    #[test]
    fn left_panel_subagents_render_below_vitals() {
        use crate::ui::panel_data::ContextGauge;
        let info = LeftPanelInfo {
            context: ContextGauge {
                used: 5000,
                window: 128_000,
                pct: 4,
                compactions: 0,
                fold_soon: false,
            },
            activity: vec!["read x.rs".into()],
            git: None,
        };
        let subs = vec![
            SubagentStatusRow {
                id_short: "abc123".into(),
                state: "running".into(),
                prompt_short: "do thing".into(),
                files: vec!["src/main.rs".into()],
            },
            SubagentStatusRow {
                id_short: "def456".into(),
                state: "completed".into(),
                prompt_short: "done".into(),
                files: vec![],
            },
        ];
        let backend = TestBackend::new(30, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| f.render_widget(LeftPanel::new(&info, &subs), Rect::new(0, 0, 30, 24)))
            .unwrap();
        let backend = terminal.backend().clone();
        let rows: Vec<String> = (0..24)
            .map(|y| {
                (0..30)
                    .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                    .collect()
            })
            .collect();
        let dump = rows.join("\n");
        // Vitals still present.
        assert!(dump.contains("CONTEXT"), "vitals dropped:\n{dump}");
        // Subagents present, below the vitals, under the "agents" header.
        assert!(dump.contains("agents"), "agents header missing:\n{dump}");
        assert!(dump.contains("...abc123"), "subagent row missing:\n{dump}");
        assert!(
            dump.contains("do thing"),
            "subagent prompt missing:\n{dump}"
        );
        let agents_y = rows.iter().position(|r| r.contains("agents")).unwrap();
        let context_y = rows.iter().position(|r| r.contains("CONTEXT")).unwrap();
        assert!(
            context_y < agents_y,
            "CONTEXT ({context_y}) must sit above agents ({agents_y})"
        );
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
        for y in layout.right_panel.y..(layout.right_panel.y + layout.right_panel.height) {
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
        for y in layout.right_panel.y..(layout.right_panel.y + layout.right_panel.height) {
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

    /// dirge-sb2n (A): the MODIFIED sub-panel collapses to a single
    /// content line (3 rows incl. borders) when the list is empty —
    /// matching the TODOS "(none)" behaviour — instead of expanding to
    /// fill all the remaining vertical space.
    #[test]
    fn modified_rect_collapses_to_one_line_when_empty() {
        let data = PanelData::default(); // modified is empty
        let area = Rect::new(0, 0, 40, 30);
        let rect = compute_modified_rect(&data, area).expect("rect");
        assert_eq!(
            rect.height, 3,
            "empty MODIFIED should be 3 rows (1 content line), got {}",
            rect.height
        );
    }

    /// dirge-sb2n (A): the box grows with the file count — two border
    /// rows plus one row per modified file — while it still fits.
    #[test]
    fn modified_rect_grows_with_file_count() {
        let mut data = PanelData::default();
        data.modified = vec!["a.rs".into(), "b.rs".into(), "c.rs".into()];
        let area = Rect::new(0, 0, 40, 30);
        let rect = compute_modified_rect(&data, area).expect("rect");
        assert_eq!(rect.height, 5, "3 files → 2 borders + 3 rows");
    }

    /// dirge-sb2n (A): when the list is longer than the room left, the
    /// box caps at the remaining space (the scroll-footer path takes
    /// over) rather than overflowing the panel or its natural height.
    #[test]
    fn modified_rect_caps_at_remaining_space() {
        let mut data = PanelData::default();
        data.modified = (0..100).map(|i| format!("f{i}.rs")).collect();
        let area = Rect::new(0, 0, 40, 30);
        let rect = compute_modified_rect(&data, area).expect("rect");
        assert!(
            rect.y + rect.height <= area.y + area.height,
            "must stay within the panel"
        );
        assert!(
            rect.height < (2 + 100),
            "must cap below natural height when overflowing, got {}",
            rect.height
        );
        assert!(rect.height >= 3);
    }

    /// dirge-sb2n (A): the rendered MODIFIED box is exactly 3 rows tall
    /// when empty — top border (with the [MODIFIED] title), one "(none)"
    /// content line, bottom border — proving the render path agrees with
    /// `compute_modified_rect` and the box no longer paints a tall blank
    /// region. Pins the actual user-visible symptom.
    #[test]
    fn modified_box_paints_three_rows_when_empty() {
        let data = PanelData::default(); // everything empty
        let layout = Layout::new(160, 40, 1);
        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| f.render_widget(RightPanel::new(&data), layout.right_panel))
            .unwrap();
        let backend = terminal.backend().clone();

        let row = |y: u16| -> String {
            (layout.right_panel.x..layout.right_panel.x + layout.right_panel.width)
                .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                .collect()
        };
        let y_range = layout.right_panel.y..(layout.right_panel.y + layout.right_panel.height);
        let title_y = y_range
            .clone()
            .find(|&y| row(y).contains("[MODIFIED]"))
            .expect("MODIFIED title should render");
        // The first bottom-border row at or after the title closes the box.
        let bottom_y = (title_y + 1..layout.right_panel.y + layout.right_panel.height)
            .find(|&y| row(y).contains('╰'))
            .expect("MODIFIED box should have a bottom border");
        assert_eq!(
            bottom_y - title_y,
            2,
            "empty MODIFIED box should be 3 rows (top, (none), bottom)"
        );
    }

    /// dirge-sb2n: with N files the MODIFIED box is content-sized — 2
    /// borders + one row per file — NOT pane-filling. Complements
    /// `modified_box_paints_three_rows_when_empty` (the empty case) and
    /// pins the user-visible symptom from the report: 4 files paint a
    /// 6-row box on a tall panel.
    #[test]
    fn modified_box_grows_to_content_height_with_files() {
        let mut data = PanelData::default();
        data.modified = vec![
            "src/verify.ts".into(),
            "test/todatests.test.ts".into(),
            "src/graph.ts".into(),
            "src/interpreter.ts".into(),
        ];
        let layout = Layout::new(160, 40, 1);
        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| f.render_widget(RightPanel::new(&data), layout.right_panel))
            .unwrap();
        let backend = terminal.backend().clone();
        let row = |y: u16| -> String {
            (layout.right_panel.x..layout.right_panel.x + layout.right_panel.width)
                .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                .collect()
        };
        let title_y = (layout.right_panel.y..layout.right_panel.y + layout.right_panel.height)
            .find(|&y| row(y).contains("[MODIFIED]"))
            .expect("MODIFIED title");
        let bottom_y = (title_y + 1..layout.right_panel.y + layout.right_panel.height)
            .find(|&y| row(y).contains('╰'))
            .expect("MODIFIED bottom border");
        assert_eq!(
            bottom_y - title_y,
            5,
            "4 files → 6-row box (2 borders + 4 rows), not pane-filling"
        );
    }

    /// CPU/MEM bar formatting.
    #[test]
    fn bar_formatting() {
        assert_eq!(format_bar("CPU", 0.0), "CPU: [..........]   0%");
        assert_eq!(format_bar("MEM", 50.0), "MEM: [#####.....]  50%");
        assert_eq!(format_bar("CPU", 100.0), "CPU: [##########] 100%");
    }

    /// DebugRightPanel renders VARIABLES and all sub-panel titles
    /// when DebugPanelData is populated with session state.
    #[cfg(feature = "dap")]
    #[test]
    fn debug_panel_renders_variables() {
        use crate::dap::types::{DebugPanelData, SessionStatus, Variable};

        let data = DebugPanelData {
            adapter: "test".into(),
            status: SessionStatus::Stopped,
            session_summary: Some(crate::dap::types::SessionSummary {
                id: "s1".into(),
                adapter_name: "test".into(),
                program: None,
                status: SessionStatus::Stopped,
                breakpoint_count: 1,
                function_breakpoint_count: 2,
                stop_reason: Some("breakpoint".into()),
                thread_id: Some(1),
                output: String::new(),
                output_truncated: false,
                exit_code: None,
                capabilities: None,
                languages: vec![],
            }),
            threads: vec![],
            frames: vec![],
            scopes: vec![],
            breakpoints: vec![],
            variables: vec![
                Variable {
                    name: "x".into(),
                    value: "42".into(),
                    type_field: Some("i32".into()),
                    presentation_hint: None,
                    evaluate_name: None,
                    variables_reference: 0,
                    named_variables: None,
                    indexed_variables: None,
                    memory_reference: None,
                },
                Variable {
                    name: "msg".into(),
                    value: "\"hello\"".into(),
                    type_field: Some("String".into()),
                    presentation_hint: None,
                    evaluate_name: None,
                    variables_reference: 0,
                    named_variables: None,
                    indexed_variables: None,
                    memory_reference: None,
                },
            ],
            output: "hello\nworld\n".into(),
            output_truncated: false,
            exit_code: None,
        };

        let backend = TestBackend::new(30, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 30, 30);
                f.render_widget(debug::DebugRightPanel::new(&data), area);
            })
            .unwrap();
        let backend = terminal.backend().clone();

        let dump: String = (0..30)
            .map(|y| {
                (0..30)
                    .map(|x| backend.buffer().cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(dump.contains("[VARIABLES]"), "VARIABLES title:\n{dump}");
        assert!(dump.contains("msg = \"hello\""), "variable value:\n{dump}");
        assert!(dump.contains(": String"), "variable type:\n{dump}");
        assert!(dump.contains("x = 42"), "simple variable:\n{dump}");
        assert!(dump.contains("source: 1  func: 2"), "bp counts:\n{dump}");
        assert!(dump.contains("[DEBUG]"), "DEBUG title:\n{dump}");
        assert!(dump.contains("[BREAKPOINTS]"), "BREAKPOINTS title:\n{dump}");
        assert!(dump.contains("[OUTPUT]"), "OUTPUT title:\n{dump}");
        assert!(dump.contains("hello"), "output:\n{dump}");
    }
}
