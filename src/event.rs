use compact_str::CompactString;

/// Structured classification of tool output for richer downstream
/// rendering. Most tools return plain text and use `Text`; tools
/// that surface file references (`read`, `find_files`,
/// `list_dir`) can opt into `File` so consumers (ACP, future UI
/// features) can render file refs as resource links rather than
/// blobs of text.
///
/// The classification is currently coarse — assigned by the
/// runner based on tool NAME rather than via per-tool plumbing —
/// since that's enough to drive opencode/ACP-style file-link
/// surfaces without touching every tool's `type Output = String`
/// contract. A future refactor could thread the variant through
/// the rig `Tool` trait for finer control.
#[derive(Debug, Clone, Default)]
pub enum ToolContent {
    /// Plain text output — the default for every tool that
    /// returns prose, JSON, command output, diffs, etc.
    #[default]
    Text,
    /// Tool surfaced one or more file paths (read returned the
    /// content of a specific file; find_files returned a listing).
    /// Consumers can render as a clickable resource link instead
    /// of a text blob.
    File,
}

/// What a compaction pass actually did, so consumers (UI / telemetry)
/// can distinguish a cheap pruning-only pass from a real summary — and,
/// crucially, surface when the LLM summarizer is *failing*
/// (IMPROVEMENTS_PLAN #5). A spike in `PruneAndFailedSummary` is an
/// early warning that the summarizer is broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum CompactionKind {
    /// Pruning only — no LLM summarizer ran (none wired, circuit breaker
    /// open, or the middle was empty).
    PruneOnly,
    /// Pruning + a successful LLM/plugin summary.
    PruneAndSummary,
    /// Pruning + a failed summary (error or invalid) — fell back to the
    /// pruned context.
    PruneAndFailedSummary,
    /// Pruning only because the summarizer circuit breaker is OPEN (it
    /// failed too many times this run). Distinct from `PruneOnly` so the
    /// ongoing-failure signal stays visible after the breaker latches
    /// rather than masquerading as a healthy no-summarizer deployment.
    PruneSummarizerDisabled,
}

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
    /// Fired immediately AFTER `ToolCall` — marks the transition
    /// from "LLM has emitted this call" to "dispatch is imminent".
    /// Semantically: between this event and the matching
    /// `ToolResult`, the tool is *running*. Consumers use it to:
    ///   - Show per-tool spinners / status badges
    ///   - Emit ACP `ToolCallStatus::InProgress` updates (the
    ///     ACP protocol distinguishes pending / in_progress /
    ///     completed; without this dirge skipped the in_progress
    ///     transition)
    ///   - Plugin observability hooks that need a "started" tick
    ///     distinct from "LLM decided to call"
    ///
    /// The id matches the corresponding `ToolCall.id` so consumers
    /// can pair them. UI consumers that already track in-flight
    /// state via "saw ToolCall, no matching ToolResult" can ignore
    /// this event safely — it's purely additive.
    ///
    /// `name` is intentionally omitted — consumers correlate by
    /// `id` against the immediately-prior `ToolCall` which already
    /// carries the name. Keeping the variant lean (one field)
    /// keeps the per-event allocation cheap; the runner emits
    /// many of these per turn.
    ToolStarted {
        #[cfg_attr(not(feature = "acp"), allow(dead_code))]
        id: CompactString,
    },
    ToolResult {
        /// Matching call id from the `ToolCall` event. Empty if the
        /// provider didn't emit one — the UI falls back to
        /// positional pairing (this result belongs to the most-
        /// recent unanswered ToolCall in the same turn).
        id: CompactString,
        output: CompactString,
        #[cfg_attr(not(feature = "acp"), allow(dead_code))]
        kind: ToolContent,
    },
    Error(CompactString),
    /// The streaming run failed with a context-length error. Audit
    /// H17: the UI used to render this as a hard `Error` and stop;
    /// users had to manually `/compress` then re-issue. Now the
    /// runner emits `ContextOverflow` carrying the prompt it was
    /// trying to send so the UI can auto-compact the session and
    /// respawn the run with the same prompt against the compacted
    /// history.
    ContextOverflow {
        prompt: CompactString,
        error: CompactString,
    },
    /// Context was compacted mid-run — old tool results pruned,
    /// session rotated. The UI persists the split via session DB,
    /// mutates `Session::id` in-place, and calls
    /// `Session::compress_reporting(summary, first_kept_index, …)`
    /// to push a `Compaction` entry. `summary` is empty when only
    /// the cheap tool-output pruner ran (no LLM summary was
    /// generated).
    ContextCompacted {
        new_session_id: CompactString,
        tokens_before: u64,
        tokens_after: u64,
        summary: CompactString,
        first_kept_index: usize,
        /// Whether this pass was pruning-only, prune+summary, or
        /// prune+failed-summary (IMPROVEMENTS_PLAN #5).
        compaction_kind: CompactionKind,
        /// Model that produced the summary, if known. `None` for
        /// pruning-only passes (and currently for summary passes — the
        /// summarizer closure is opaque; threading the model name is a
        /// follow-up).
        summary_model: Option<CompactString>,
    },
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
    /// Plugin-emitted custom message reaching the UI mid-stream.
    /// Carries the raw JSON payload the plugin queued via
    /// `harness/add-custom-message`. The UI looks up a registered
    /// renderer (see `PluginManager::list_message_renderers`) by
    /// the payload's `type` field; without one it falls back to a
    /// default formatter. Port of pi's `LoopMessage::Custom` →
    /// `registerMessageRenderer` lookup (extensions/types.ts:1171).
    CustomMessage {
        payload: serde_json::Value,
    },
    /// The runner observed an interjection request at a tool-result boundary
    /// and stopped the stream cleanly. Whatever assistant text had streamed
    /// so far is captured in `partial_response`. The UI commits it as an
    /// assistant message and then drains its interjection queue as the next
    /// user turn.
    ///
    /// Constructed by the bridge when the LLM stream is cancelled via the
    /// interject signal (rig_stream error message "stream aborted by
    /// cancellation signal" is recognized as a graceful stop, not a hard
    /// error).
    Interjected {
        partial_response: CompactString,
        tokens: u64,
    },
    /// User message injected mid-run via the steering queue.
    /// The UI renders this as a user chat message so the user sees
    /// their interjected guidance appear in the log when the agent
    /// processes it at a turn boundary.
    UserMessage {
        content: CompactString,
    },
    /// The retry layer is about to re-attempt a stream request after
    /// a transient error. `attempt` is 1-indexed (the Nth retry).
    /// PROV-2: consumers should surface a temporary banner so the
    /// user isn't staring at silence during backoff.
    RetryNotice {
        attempt: u32,
        delay_ms: u64,
        error: CompactString,
    },
    /// A dirge-originated log/notice line for the user — e.g. the
    /// max-agent-turns cap message. The UI renders it as a `<system>`
    /// log line in the warning color so it reads as a tool/runtime
    /// notice rather than a message the user typed (which would carry
    /// the `<you>` prefix). Not persisted to the session.
    SystemNotice {
        content: CompactString,
    },
    /// Per-run input-repair telemetry, emitted just before
    /// `AgentEvent::Done`. The UI prints a one-line summary
    /// ("repaired 3 inputs: 1 md-link, 2 null-strip; 0 invalid")
    /// when at least one repair fired. Empty snapshots aren't
    /// emitted at all. Phase-1 of docs/AGENTIC_LOOP_PLAN.md.
    RepairStats {
        snapshot: crate::agent::agent_loop::tool_input_repair::RepairStatsSnapshot,
    },
    /// Phase 4 part 1 — dual-client tiering: the NEXT LLM call has
    /// been swapped to the configured escalation provider after a
    /// repair-exhaustion or tree-sitter syntactic failure. One-shot;
    /// subsequent calls revert to the default model. The UI surfaces
    /// this so the user knows about the unexpected provider change.
    EscalationActivated {
        provider: CompactString,
        reason: crate::agent::agent_loop::message::EscalationReason,
    },
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    Key(crossterm::event::KeyEvent),
    Paste(String),
    /// Terminal was resized. Carries no payload — the renderer queries
    /// `crossterm::terminal::size()` directly; the variant is just a kick
    /// to repaint at the new dimensions.
    Resize,
    /// Mouse wheel scrolled up — scroll the output pane up by one line.
    /// Mouse capture is on (see `TerminalGuard::new`) so the wheel reaches
    /// the app instead of being absorbed by the terminal, which under the
    /// alt screen would push the TUI off-view.
    ScrollUp {
        row: u16,
        col: u16,
    },
    /// Mouse wheel scrolled down — scroll the output pane down by one line.
    /// See `ScrollUp` for the `(row, col)` semantics.
    ScrollDown {
        row: u16,
        col: u16,
    },
    /// Left mouse button pressed at terminal cell `(row, col)` — starts
    /// an app-level drag selection. Consumed by `ui::selection::handle`
    /// before any UI-state-specific consumer sees it.
    MouseDown {
        row: u16,
        col: u16,
    },
    /// Left mouse button dragged to terminal cell `(row, col)` — extends
    /// the active selection.
    MouseDrag {
        row: u16,
        col: u16,
    },
    /// Left mouse button released at terminal cell `(row, col)` —
    /// finalizes the selection, copies it to the clipboard, and clears
    /// the highlight.
    MouseUp {
        row: u16,
        col: u16,
    },
}
