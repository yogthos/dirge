//! `Scene` — the pure rendering input.
//!
//! Captures every piece of state needed to paint one UI frame.
//! `render_frame(&Scene, &mut Frame)` is the single integration
//! point: the runtime wraps a `ratatui::Terminal` and calls it on
//! every redraw; tests build a `Scene` directly and assert against
//! a `TestBackend`.
//!
//! Keeping rendering pure (state → buffer, no I/O) means we can
//! test the entire UI without a terminal, and the runtime hook
//! becomes trivial: collect state, build Scene, draw.

use ratatui::Frame;
use ratatui::style::{Color as RColor, Style};

use super::bottom::{AvatarSpec, BottomBody, BottomStrip};
use super::chat::{ChatPane, crossterm_to_ratatui};
use super::frame::{ChatBotFrame, TopFrame};
use super::layout::Layout;
use super::panels::{LeftPanel, RightPanel};
use crate::ui::renderer::{LeftPanelInfo, LineEntry, PanelData, SubagentStatusRow};

#[allow(unused_imports)] // RColor stays in scope for the doctest example.
use ratatui::style::Color as _RColor;

/// All state needed to render one frame.
///
/// Borrowed references throughout so callers don't have to clone
/// the chat buffer or panel data on the redraw hot path.
pub struct Scene<'a> {
    /// Chat scrollback.
    pub chat_buffer: &'a [LineEntry],
    /// Rows to skip from the END of the buffer (0 = show newest).
    pub scroll_offset: usize,
    /// Number of input editor rows (clamped to MAX_INPUT_ROWS by Layout).
    pub input_rows: u16,
    /// Right panel data (MCP, LSP, TODOS, MODIFIED, sysload).
    pub panel_data: &'a PanelData,
    /// Left panel: idle card info (used when subagents is empty).
    pub left_info: &'a LeftPanelInfo,
    /// Left panel: subagent status rows (used when non-empty).
    pub subagents: &'a [SubagentStatusRow],
    /// Avatar face spec.
    pub avatar: Option<AvatarSpec<'a>>,
    /// Bottom strip body — editor input or overlay.
    pub body: BottomBody<'a>,
    /// Status row text.
    pub status: &'a str,
    /// Render side panels? (false on very narrow terminals.)
    pub show_side_panels: bool,
    /// Header / frame color.
    pub frame_color: crossterm::style::Color,
}

/// Paint the entire UI into `f`. Computes layout from the frame's
/// area + the scene's input_rows.
pub fn render_frame(scene: &Scene, f: &mut Frame<'_>) {
    let area = f.area();
    let layout = Layout::new(area.width, area.height, scene.input_rows);
    let frame_style = Style::default().fg(crossterm_to_ratatui(scene.frame_color));

    // Top frame (full width, across left panel + chat + right panel).
    f.render_widget(TopFrame::new(&layout).style(frame_style), area);

    // Left panel — idle card or subagent list. Skip on narrow terminals.
    if scene.show_side_panels && layout.left_panel.width >= 12 {
        f.render_widget(
            LeftPanel::new(scene.left_info, scene.subagents),
            layout.left_panel,
        );
    }

    // Chat region (content + ║ verticals).
    f.render_widget(
        ChatPane::new(&layout, scene.chat_buffer, scene.scroll_offset)
            .border_style(frame_style),
        area,
    );

    // Right panel — stacked sub-panels. Skip on narrow terminals.
    if scene.show_side_panels && layout.right_panel.width >= 16 {
        f.render_widget(RightPanel::new(scene.panel_data), layout.right_panel);
    }

    // Chat bottom frame (╚═══╝ in chat band only).
    f.render_widget(ChatBotFrame::new(&layout).style(frame_style), area);

    // Bottom strip (avatar + input box / overlay + status).
    let mut strip = BottomStrip::new(&layout)
        .status(scene.status)
        .border_style(frame_style)
        .body(scene.body);
    if let Some(avatar) = &scene.avatar {
        strip = strip.avatar(AvatarSpec {
            face: avatar.face,
            color: avatar.color,
        });
    }
    f.render_widget(strip, area);
}

// `BottomBody` is Copy so `render_frame` can pass it to BottomStrip
// directly without a clone helper.

/// Convenience builder for a Scene with sensible defaults — useful
/// in tests and in early-startup paths where most state is empty.
pub fn empty_scene<'a>(
    chat_buffer: &'a [LineEntry],
    panel_data: &'a PanelData,
    left_info: &'a LeftPanelInfo,
    subagents: &'a [SubagentStatusRow],
    status: &'a str,
) -> Scene<'a> {
    Scene {
        chat_buffer,
        scroll_offset: 0,
        input_rows: 1,
        panel_data,
        left_info,
        subagents,
        avatar: None,
        body: BottomBody::Editor {
            text: "",
            cursor_col: 0,
            is_running: false,
        },
        status,
        show_side_panels: true,
        frame_color: crossterm::style::Color::Green,
    }
}

// Keep RColor in scope so the example-style doctest in this module
// doesn't have to re-import it.
const _: RColor = RColor::Green;

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// End-to-end render: empty buffer, no overlay, defaults.
    /// Verifies the top frame title shows up and the chat band
    /// renders ║ borders.
    #[test]
    fn renders_empty_scene_with_frames_and_borders() {
        let buf: Vec<LineEntry> = Vec::new();
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let scene = empty_scene(&buf, &pd, &info, &subs, "ready");

        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal.draw(|f| render_frame(&scene, f)).unwrap();
        backend = terminal.backend().clone();

        // Top frame row contains all three titles.
        let row0: String = (0..160)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, 0))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(row0.contains("[AGENT STATUS]"));
        assert!(row0.contains("[AGENT LOG STREAM]"));
        assert!(row0.contains("[SYSTEM]"));

        // Chat ║ verticals on row 1.
        let layout = Layout::new(160, 30, 1);
        assert_eq!(
            backend
                .buffer()
                .cell((layout.chat_v_left_col, 1))
                .unwrap()
                .symbol(),
            "║"
        );
        assert_eq!(
            backend
                .buffer()
                .cell((layout.chat_v_right_col, 1))
                .unwrap()
                .symbol(),
            "║"
        );

        // Status row contains the status text.
        let status_row: String = (0..160)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, layout.status.y))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(status_row.starts_with("ready"));
    }

    /// When an overlay is active, the editor is REPLACED inside
    /// the bottom frame — no second box anywhere.
    #[test]
    fn overlay_replaces_input_editor() {
        use crossterm::style::Color as CC;
        let buf: Vec<LineEntry> = Vec::new();
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let overlay_lines: Vec<(String, CC)> = vec![
            ("⚠ PERMISSION REQUIRED".into(), CC::Yellow),
            ("tool: read_file".into(), CC::Yellow),
        ];
        let scene = Scene {
            chat_buffer: &buf,
            scroll_offset: 0,
            input_rows: 4,
            panel_data: &pd,
            left_info: &info,
            subagents: &subs,
            avatar: None,
            body: BottomBody::Overlay {
                title: "[ALERT]",
                lines: &overlay_lines,
            },
            status: "permission required",
            show_side_panels: true,
            frame_color: crossterm::style::Color::Green,
        };

        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal.draw(|f| render_frame(&scene, f)).unwrap();
        backend = terminal.backend().clone();
        let layout = Layout::new(160, 30, 4);

        // The input box top border should have "[ALERT]" centered.
        let top_y = layout.input_box.y;
        let top_row: String = (layout.input_box.x..layout.input_box.x + layout.input_box.width)
            .map(|x| backend.buffer().cell((x, top_y)).unwrap().symbol().to_string())
            .collect();
        assert!(top_row.contains("[ALERT]"), "got top {:?}", top_row);

        // First overlay line ("⚠ PERMISSION REQUIRED") shows in row 1.
        let body_row: String = (layout.input_box.x..layout.input_box.x + layout.input_box.width)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, top_y + 1))
                    .unwrap()
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(
            body_row.contains("PERMISSION REQUIRED"),
            "got body {:?}",
            body_row
        );
    }

    /// Side panel suppression: a narrow terminal (line_w ≤
    /// CHAT_CONTENT_MAX_W) has zero-width left/right panels, so
    /// LeftPanel / RightPanel widgets shouldn't paint into them.
    /// The top frame still draws — just without the side titles.
    #[test]
    fn narrow_terminal_skips_side_panels() {
        let buf: Vec<LineEntry> = Vec::new();
        let pd = PanelData::default();
        let info = LeftPanelInfo {
            agent_id: "abc".into(),
            model: "m".into(),
            focus: "code".into(),
        };
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let mut scene = empty_scene(&buf, &pd, &info, &subs, "narrow");
        scene.show_side_panels = true; // request side panels even though they collapse

        let mut backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal.draw(|f| render_frame(&scene, f)).unwrap();
        backend = terminal.backend().clone();
        let layout = Layout::new(60, 20, 1);

        // Side panels have zero width — no DIRGE banner anywhere.
        assert_eq!(layout.left_panel.width, 0);
        assert_eq!(layout.right_panel.width, 0);
        let mut found_dirge = false;
        for y in 0..20 {
            let r: String = (0..60)
                .map(|x| {
                    backend
                        .buffer()
                        .cell((x, y))
                        .unwrap()
                        .symbol()
                        .to_string()
                })
                .collect();
            if r.contains("D I R G E") {
                found_dirge = true;
                break;
            }
        }
        assert!(!found_dirge, "DIRGE banner should not appear on narrow term");
    }

    /// Chat content from the scene's buffer paints into the chat
    /// region with the expected text in the expected rows.
    #[test]
    fn chat_buffer_paints_into_chat_region() {
        let buf: Vec<LineEntry> = vec![
            LineEntry {
                text: "first line".into(),
                color: crossterm::style::Color::Green,
            },
            LineEntry {
                text: "second line".into(),
                color: crossterm::style::Color::Cyan,
            },
        ];
        let pd = PanelData::default();
        let info = LeftPanelInfo::default();
        let subs: Vec<SubagentStatusRow> = Vec::new();
        let scene = empty_scene(&buf, &pd, &info, &subs, "");

        let mut backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend.clone()).unwrap();
        terminal.draw(|f| render_frame(&scene, f)).unwrap();
        backend = terminal.backend().clone();
        let layout = Layout::new(160, 30, 1);

        // Lines paint top-anchored at chat.y, chat.y + 1.
        let read = |y: u16| -> String {
            (layout.chat.x..layout.chat.x + layout.chat.width)
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
        assert!(read(layout.chat.y).starts_with("first line"));
        assert!(read(layout.chat.y + 1).starts_with("second line"));
    }
}
