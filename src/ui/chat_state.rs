//! Per-chat snapshot of the UI loop's streaming / chamber state plus
//! the save/load helpers used at chat-switch boundaries.
//!
//! dirge-ov2 Phase C: each chat carries its own streaming context so
//! switching away (Ctrl-N/P/X) and back doesn't shred the in-flight
//! response.
//!
//! Extracted from `ui/mod.rs` as a leaf module. The function shapes
//! (long argument lists by `&mut` reference) intentionally match the
//! UI loop's local variables so the call sites in `run_interactive`
//! don't need refactoring.

use crate::session::ToolCallEntry;

/// dirge-ov2 Phase C: per-chat snapshot of the UI loop's streaming /
/// chamber state. Saved when the user switches away from a chat;
/// restored when they switch back. Hot-path event handlers continue
/// to read/write the UI-loop locals directly — only chat-switch
/// boundaries pay for the swap.
#[derive(Default)]
pub(crate) struct ChatUiState {
    pub(crate) response_buf: String,
    pub(crate) response_start_line: Option<usize>,
    pub(crate) reasoning_buf: String,
    pub(crate) reasoning_start_line: Option<usize>,
    pub(crate) last_tool_name: Option<String>,
    pub(crate) last_tool_call_id: Option<String>,
    pub(crate) tool_chamber_open: bool,
    pub(crate) agent_line_started: bool,
    pub(crate) was_reasoning: bool,
    pub(crate) tool_calls_buf: Vec<ToolCallEntry>,
    pub(crate) tool_calls_this_run: u32,
}

impl ChatUiState {
    pub(crate) fn empty() -> Self {
        Self::default()
    }
}

/// dirge-ov2 Phase C: snapshot the UI loop's per-chat locals into the
/// supplied state slot. Called before switching chats so each chat's
/// streaming context survives the swap.
#[allow(clippy::too_many_arguments)]
pub(crate) fn save_chat_ui_state(
    slot: &mut ChatUiState,
    response_buf: &mut String,
    response_start_line: &mut Option<usize>,
    reasoning_buf: &mut String,
    reasoning_start_line: &mut Option<usize>,
    last_tool_name: &mut Option<String>,
    last_tool_call_id: &mut Option<String>,
    tool_chamber_open: &mut bool,
    agent_line_started: &mut bool,
    was_reasoning: &mut bool,
    tool_calls_buf: &mut Vec<ToolCallEntry>,
    tool_calls_this_run: &mut u32,
) {
    slot.response_buf = std::mem::take(response_buf);
    slot.response_start_line = response_start_line.take();
    slot.reasoning_buf = std::mem::take(reasoning_buf);
    slot.reasoning_start_line = reasoning_start_line.take();
    slot.last_tool_name = last_tool_name.take();
    slot.last_tool_call_id = last_tool_call_id.take();
    slot.tool_chamber_open = *tool_chamber_open;
    slot.agent_line_started = *agent_line_started;
    slot.was_reasoning = *was_reasoning;
    slot.tool_calls_buf = std::mem::take(tool_calls_buf);
    slot.tool_calls_this_run = *tool_calls_this_run;
}

/// dirge-ov2 Phase C: inverse of `save_chat_ui_state`. Loads the
/// supplied state slot into the UI loop's locals after a chat switch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn load_chat_ui_state(
    slot: &mut ChatUiState,
    response_buf: &mut String,
    response_start_line: &mut Option<usize>,
    reasoning_buf: &mut String,
    reasoning_start_line: &mut Option<usize>,
    last_tool_name: &mut Option<String>,
    last_tool_call_id: &mut Option<String>,
    tool_chamber_open: &mut bool,
    agent_line_started: &mut bool,
    was_reasoning: &mut bool,
    tool_calls_buf: &mut Vec<ToolCallEntry>,
    tool_calls_this_run: &mut u32,
) {
    *response_buf = std::mem::take(&mut slot.response_buf);
    *response_start_line = slot.response_start_line.take();
    *reasoning_buf = std::mem::take(&mut slot.reasoning_buf);
    *reasoning_start_line = slot.reasoning_start_line.take();
    *last_tool_name = slot.last_tool_name.take();
    *last_tool_call_id = slot.last_tool_call_id.take();
    *tool_chamber_open = slot.tool_chamber_open;
    *agent_line_started = slot.agent_line_started;
    *was_reasoning = slot.was_reasoning;
    *tool_calls_buf = std::mem::take(&mut slot.tool_calls_buf);
    *tool_calls_this_run = slot.tool_calls_this_run;
}
