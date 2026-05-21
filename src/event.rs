use compact_str::CompactString;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Token(CompactString),
    Reasoning(CompactString),
    ToolCall {
        /// Provider call id (rig's `ToolCall.id`). Empty for older
        /// rig versions or providers that don't emit one; the UI
        /// uses it to pair this call with the corresponding
        /// `ToolResult` event for structured persistence (Phase 3).
        id: CompactString,
        name: CompactString,
        args: serde_json::Value,
    },
    ToolResult {
        /// Matching call id from the `ToolCall` event. Empty if the
        /// provider didn't emit one — the UI falls back to
        /// positional pairing (this result belongs to the most-
        /// recent unanswered ToolCall in the same turn).
        id: CompactString,
        output: CompactString,
    },
    Error(CompactString),
    Done {
        response: CompactString,
        tokens: u64,
        cost: f64,
    },
    /// Marks the start of one turn within an agent run. A "turn" is one
    /// LLM call + any tool calls it dispatched + the tool results
    /// returning. A pure-text response has exactly one turn (TurnStart 0
    /// → TurnEnd 0 → Done). A run with tool calls has multiple turns,
    /// with turn boundaries straddling tool-result/next-assistant
    /// content. Plugin hook authors (P3) consume these to bracket
    /// per-turn observability.
    TurnStart {
        index: u32,
    },
    /// Marks the end of one turn. Fires immediately before the next
    /// turn's TurnStart, or just before `Done` for the final turn.
    /// Empty runs (stream ended without any assistant content) emit
    /// neither TurnStart nor TurnEnd.
    TurnEnd {
        index: u32,
    },
    /// The runner observed an interjection request at a tool-result boundary
    /// and stopped the stream cleanly. Whatever assistant text had streamed
    /// so far is captured in `partial_response`. The UI is expected to
    /// commit it as an assistant message and then drain its interjection
    /// queue as the next user turn.
    Interjected {
        partial_response: CompactString,
        tokens: u64,
    },
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    Key(crossterm::event::KeyEvent),
    ScrollUp,
    ScrollDown,
    #[allow(dead_code)]
    MouseDown {
        row: u16,
        col: u16,
    },
    #[allow(dead_code)]
    MouseDrag {
        row: u16,
        col: u16,
    },
    #[allow(dead_code)]
    MouseUp {
        row: u16,
        col: u16,
    },
    Paste(String),
    /// Terminal was resized. Carries no payload — the renderer queries
    /// `crossterm::terminal::size()` directly; the variant is just a kick
    /// to repaint at the new dimensions.
    Resize,
}
