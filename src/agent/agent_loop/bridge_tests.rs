use super::*;
use crate::agent::agent_loop::message::{
    AssistantMessage, ContentBlock, StopReason, ToolResultMessage, UserMessage,
};
use crate::agent::agent_loop::result::LoopToolResult;

/// Convenience: build a partial assistant message with a single
/// text block.
fn assistant_with_text(s: &str) -> AssistantMessage {
    AssistantMessage::new(
        vec![ContentBlock::Text {
            text: s.to_string(),
        }],
        StopReason::Stop,
    )
}

/// Convenience: assistant with thinking content.
fn assistant_with_thinking(s: &str) -> AssistantMessage {
    AssistantMessage::new(
        vec![ContentBlock::Thinking {
            text: s.to_string(),
        }],
        StopReason::Stop,
    )
}

/// `TurnStart` increments the counter; the emitted event
/// carries the value PRIOR to the increment (so the first
/// TurnStart is index 0). `TurnEnd` matches.
#[test]
fn turn_start_end_index_round_trips() {
    let mut bridge = EventBridge::new();
    let s0 = bridge.translate(LoopEvent::TurnStart);
    let e0 = bridge.translate(LoopEvent::TurnEnd {
        message: assistant_with_text("hi"),
        tool_results: Vec::new(),
    });
    let s1 = bridge.translate(LoopEvent::TurnStart);
    let e1 = bridge.translate(LoopEvent::TurnEnd {
        message: assistant_with_text("again"),
        tool_results: Vec::new(),
    });

    assert!(matches!(
        s0.as_slice(),
        [AgentEvent::TurnStart { index: 0 }]
    ));
    assert!(matches!(e0.as_slice(), [AgentEvent::TurnEnd { index: 0 }]));
    assert!(matches!(
        s1.as_slice(),
        [AgentEvent::TurnStart { index: 1 }]
    ));
    assert!(matches!(e1.as_slice(), [AgentEvent::TurnEnd { index: 1 }]));
}

/// `AgentStart` is a no-op — dirge has no equivalent event.
/// `AgentEnd` produces `Done` with the final assistant text.
#[test]
fn agent_start_no_op_agent_end_emits_done() {
    let mut bridge = EventBridge::new();
    assert!(bridge.translate(LoopEvent::AgentStart).is_empty());

    let messages = vec![
        LoopMessage::User(UserMessage {
            content: "hi".to_string(),
        }),
        LoopMessage::Assistant(assistant_with_text("final answer")),
    ];
    let out = bridge.translate(LoopEvent::AgentEnd { messages });
    assert_eq!(out.len(), 1);
    match &out[0] {
        AgentEvent::Done {
            response,
            tokens,
            cost,
        } => {
            assert_eq!(response.as_str(), "final answer");
            assert_eq!(*tokens, 0);
            assert_eq!(*cost, 0.0);
        }
        _ => panic!("expected Done"),
    }
}

/// Phase 4.5h-1: AgentEnd carrying an assistant message with
/// stop_reason=Error + a context-length error string →
/// `AgentEvent::ContextOverflow` (not `Done` or `Error`).
/// UI consumes ContextOverflow by running `/compress` and
/// respawning a fresh runner with the same prompt against
/// the compacted history.
#[test]
fn agent_end_context_length_error_emits_context_overflow() {
    let mut bridge = EventBridge::new();
    let mut a = assistant_with_text("");
    a.stop_reason = StopReason::Error;
    a.error_message = Some("prompt is too long: maximum context length exceeded".to_string());
    let messages = vec![
        LoopMessage::User(UserMessage {
            content: "summarize this huge doc".to_string(),
        }),
        LoopMessage::Assistant(a),
    ];
    let out = bridge.translate(LoopEvent::AgentEnd { messages });
    assert_eq!(out.len(), 1);
    match &out[0] {
        AgentEvent::ContextOverflow { prompt, error } => {
            assert_eq!(prompt.as_str(), "summarize this huge doc");
            assert!(
                error.contains("context length") || error.contains("too long"),
                "error text should mention context length"
            );
        }
        other => panic!("expected ContextOverflow, got {other:?}"),
    }
}

/// Phase 4.5h-1: AgentEnd with stop_reason=Error but NOT a
/// context-length signal → `AgentEvent::Error`.
#[test]
fn agent_end_non_context_error_emits_error() {
    let mut bridge = EventBridge::new();
    let mut a = assistant_with_text("");
    a.stop_reason = StopReason::Error;
    a.error_message = Some("401 unauthorized: invalid api key".to_string());
    let messages = vec![
        LoopMessage::User(UserMessage {
            content: "hi".to_string(),
        }),
        LoopMessage::Assistant(a),
    ];
    let out = bridge.translate(LoopEvent::AgentEnd { messages });
    assert_eq!(out.len(), 1);
    match &out[0] {
        AgentEvent::Error(msg) => {
            assert!(msg.contains("unauthorized"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Interject channel cancellation surfaces as a stream Error
/// with the message "stream aborted by cancellation signal"
/// (rig_stream.rs:124). The bridge must recognise this as a
/// graceful interjection — emit `Interjected` so the UI drains
/// its queued messages, NOT `Error` which would drop them
/// (the user's bug report on this).
#[test]
fn agent_end_cancellation_emits_interjected_not_error() {
    let mut bridge = EventBridge::new();
    let mut a = assistant_with_text("partial response before interject");
    a.stop_reason = StopReason::Error;
    a.error_message = Some("stream aborted by cancellation signal".to_string());
    let messages = vec![
        LoopMessage::User(UserMessage {
            content: "do a long thing".to_string(),
        }),
        LoopMessage::Assistant(a),
    ];
    let out = bridge.translate(LoopEvent::AgentEnd { messages });
    assert_eq!(out.len(), 1);
    match &out[0] {
        AgentEvent::Interjected {
            partial_response,
            tokens,
        } => {
            assert_eq!(
                partial_response.as_str(),
                "partial response before interject"
            );
            assert_eq!(*tokens, 0);
        }
        other => panic!("expected Interjected, got {other:?}"),
    }
}

/// Phase 4.5h-1: AgentEnd with stop_reason=Error but no
/// error_message → still emits Error with a placeholder
/// message. Defensive — error variant should never produce
/// a misleading Done.
#[test]
fn agent_end_error_without_message_still_emits_error() {
    let mut bridge = EventBridge::new();
    let mut a = assistant_with_text("");
    a.stop_reason = StopReason::Error;
    a.error_message = None;
    let messages = vec![LoopMessage::Assistant(a)];
    let out = bridge.translate(LoopEvent::AgentEnd { messages });
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0], AgentEvent::Error(_)));
}

/// AgentEnd with stop_reason=Aborted → Done with empty
/// response. Graceful abort doesn't produce an error event.
/// Interjected (the UI's "user said stop") is a runner-level
/// concern handled separately.
#[test]
fn agent_end_aborted_emits_done() {
    let mut bridge = EventBridge::new();
    let mut a = assistant_with_text("partial work");
    a.stop_reason = StopReason::Aborted;
    let messages = vec![LoopMessage::Assistant(a)];
    let out = bridge.translate(LoopEvent::AgentEnd { messages });
    assert_eq!(out.len(), 1);
    // Done — the loop ended (no Error). Response field carries
    // whatever text had assembled.
    match &out[0] {
        AgentEvent::Done { response, .. } => {
            assert_eq!(response.as_str(), "partial work");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

/// `AgentEnd` with no assistant message → `Done` with empty
/// response.
#[test]
fn agent_end_no_assistant_done_empty_response() {
    let mut bridge = EventBridge::new();
    let messages = vec![LoopMessage::User(UserMessage {
        content: "hi".to_string(),
    })];
    let out = bridge.translate(LoopEvent::AgentEnd { messages });
    match &out[0] {
        AgentEvent::Done { response, .. } => {
            assert_eq!(response.as_str(), "");
        }
        _ => panic!("expected Done"),
    }
}

/// `MessageUpdate { TextStart }` emits a single `Token` with
/// the initial text chunk. The bridge tracks this as
/// "last_text_emitted" so subsequent deltas emit only the new
/// portion.
#[test]
fn text_delta_emits_token_chunks() {
    let mut bridge = EventBridge::new();
    // First chunk: "Hello"
    let out = bridge.translate(LoopEvent::MessageUpdate {
        message: assistant_with_text("Hello"),
        phase: DeltaPhase::TextStart,
    });
    assert_eq!(out.len(), 1);
    match &out[0] {
        AgentEvent::Token(s) => assert_eq!(s.as_str(), "Hello"),
        _ => panic!("expected Token"),
    }
    // Second chunk: "Hello world" (provider appended " world")
    let out = bridge.translate(LoopEvent::MessageUpdate {
        message: assistant_with_text("Hello world"),
        phase: DeltaPhase::TextDelta,
    });
    assert_eq!(out.len(), 1);
    match &out[0] {
        AgentEvent::Token(s) => assert_eq!(s.as_str(), " world"),
        _ => panic!("expected Token"),
    }
    // Third update with no new text → no event.
    let out = bridge.translate(LoopEvent::MessageUpdate {
        message: assistant_with_text("Hello world"),
        phase: DeltaPhase::TextDelta,
    });
    assert!(out.is_empty());
}

/// `MessageUpdate` for reasoning produces `Reasoning` events
/// using the same delta tracking.
#[test]
fn reasoning_delta_emits_reasoning_chunks() {
    let mut bridge = EventBridge::new();
    let out = bridge.translate(LoopEvent::MessageUpdate {
        message: assistant_with_thinking("Let me think"),
        phase: DeltaPhase::ThinkingStart,
    });
    match &out[0] {
        AgentEvent::Reasoning(s) => assert_eq!(s.as_str(), "Let me think"),
        _ => panic!("expected Reasoning"),
    }
    let out = bridge.translate(LoopEvent::MessageUpdate {
        message: assistant_with_thinking("Let me think about this"),
        phase: DeltaPhase::ThinkingDelta,
    });
    match &out[0] {
        AgentEvent::Reasoning(s) => assert_eq!(s.as_str(), " about this"),
        _ => panic!("expected Reasoning"),
    }
}

/// `MessageUpdate` with text and reasoning interleaved tracks
/// them independently. Verifies the per-kind delta state.
#[test]
fn text_and_reasoning_tracked_independently() {
    let mut bridge = EventBridge::new();
    // Reasoning arrives first.
    let _ = bridge.translate(LoopEvent::MessageUpdate {
        message: AssistantMessage::new(
            vec![ContentBlock::Thinking {
                text: "thinking".to_string(),
            }],
            StopReason::Stop,
        ),
        phase: DeltaPhase::ThinkingStart,
    });
    // Then text arrives in a separate block.
    let out = bridge.translate(LoopEvent::MessageUpdate {
        message: AssistantMessage::new(
            vec![
                ContentBlock::Thinking {
                    text: "thinking".to_string(),
                },
                ContentBlock::Text {
                    text: "answer".to_string(),
                },
            ],
            StopReason::Stop,
        ),
        phase: DeltaPhase::TextStart,
    });
    // Token for "answer", not for the previously-emitted
    // "thinking".
    assert_eq!(out.len(), 1);
    match &out[0] {
        AgentEvent::Token(s) => assert_eq!(s.as_str(), "answer"),
        _ => panic!("expected Token"),
    }
}

/// `ToolExecutionStart` produces TWO events: `ToolCall` then
/// `ToolStarted`. Bridge also records the name for later
/// classification.
#[test]
fn tool_execution_start_emits_call_and_started() {
    let mut bridge = EventBridge::new();
    let out = bridge.translate(LoopEvent::ToolExecutionStart {
        tool_call_id: "call-1".to_string(),
        tool_name: "read".to_string(),
        args: serde_json::json!({"path": "/tmp/x"}),
    });
    assert_eq!(out.len(), 2);
    match &out[0] {
        AgentEvent::ToolCall { id, name, args } => {
            assert_eq!(id.as_str(), "call-1");
            assert_eq!(name.as_str(), "read");
            assert_eq!(args["path"], "/tmp/x");
        }
        _ => panic!("expected ToolCall"),
    }
    match &out[1] {
        AgentEvent::ToolStarted { id } => {
            assert_eq!(id.as_str(), "call-1");
        }
        _ => panic!("expected ToolStarted"),
    }
}

/// `ToolExecutionEnd` → `ToolResult` with `ToolContent::File`
/// for tool names that surface file refs.
#[test]
fn tool_execution_end_classifies_file_tools_as_file() {
    let mut bridge = EventBridge::new();
    // Record the tool name first.
    let _ = bridge.translate(LoopEvent::ToolExecutionStart {
        tool_call_id: "call-1".to_string(),
        tool_name: "read".to_string(),
        args: serde_json::json!({}),
    });
    let out = bridge.translate(LoopEvent::ToolExecutionEnd {
        tool_call_id: "call-1".to_string(),
        tool_name: "read".to_string(),
        result: LoopToolResult {
            content: vec![serde_json::json!({
                "type": "text",
                "text": "file contents here"
            })],
            details: serde_json::Value::Null,
            terminate: None,
        },
        is_error: false,
    });
    assert_eq!(out.len(), 1);
    match &out[0] {
        AgentEvent::ToolResult { id, output, kind } => {
            assert_eq!(id.as_str(), "call-1");
            assert_eq!(output.as_str(), "file contents here");
            assert!(matches!(kind, ToolContent::File));
        }
        _ => panic!("expected ToolResult"),
    }
}

/// `ToolExecutionEnd` → `ToolResult` with `ToolContent::Text`
/// for tools that aren't in the file-classification set.
#[test]
fn tool_execution_end_classifies_other_tools_as_text() {
    let mut bridge = EventBridge::new();
    let _ = bridge.translate(LoopEvent::ToolExecutionStart {
        tool_call_id: "call-2".to_string(),
        tool_name: "bash".to_string(),
        args: serde_json::json!({}),
    });
    let out = bridge.translate(LoopEvent::ToolExecutionEnd {
        tool_call_id: "call-2".to_string(),
        tool_name: "bash".to_string(),
        result: LoopToolResult {
            content: vec![serde_json::json!({"type": "text", "text": "stdout"})],
            details: serde_json::Value::Null,
            terminate: None,
        },
        is_error: false,
    });
    match &out[0] {
        AgentEvent::ToolResult { kind, .. } => {
            assert!(matches!(kind, ToolContent::Text));
        }
        _ => panic!("expected ToolResult"),
    }
}

/// `MessageStart` / `MessageEnd` are no-ops at the AgentEvent
/// boundary (dirge handles user-message rendering / done
/// `LoopMessage::Custom` flowing through `MessageStart` becomes
/// an `AgentEvent::CustomMessage` carrying the same JSON payload
/// — the bridge keeps the payload opaque so the UI's renderer
/// lookup gets the full structure.
#[test]
fn message_start_custom_emits_custom_message_event() {
    let mut bridge = EventBridge::new();
    let payload = serde_json::json!({"type": "status", "content": "hello"});
    let events = bridge.translate(LoopEvent::MessageStart {
        message: LoopMessage::Custom(payload.clone()),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::CustomMessage { payload: out } => assert_eq!(out, &payload),
        other => panic!("unexpected event: {other:?}"),
    }
}

/// `MessageStart` for User messages now emits `UserMessage`
/// so steering-injected user messages are displayed in the UI
/// log. ToolResult and Assistant MessageStart remain no-ops.
/// `MessageEnd` is always a no-op.
#[test]
fn message_start_end_behavior() {
    let mut bridge = EventBridge::new();
    let user_msg = LoopMessage::User(UserMessage {
        content: "hi".to_string(),
    });
    // User messages now emit UserMessage.
    let events = bridge.translate(LoopEvent::MessageStart {
        message: user_msg.clone(),
    });
    assert_eq!(events.len(), 1);
    match &events[0] {
        AgentEvent::UserMessage { content } => assert_eq!(content.as_str(), "hi"),
        other => panic!("expected UserMessage, got {other:?}"),
    }
    // MessageEnd is still a no-op for user messages.
    assert!(
        bridge
            .translate(LoopEvent::MessageEnd { message: user_msg })
            .is_empty()
    );

    // ToolResult MessageStart/End remain no-ops.
    let tool_msg = LoopMessage::ToolResult(ToolResultMessage {
        tool_call_id: "c1".to_string(),
        tool_name: "echo".to_string(),
        content: Vec::new(),
        details: serde_json::Value::Null,
        is_error: false,
    });
    assert!(
        bridge
            .translate(LoopEvent::MessageStart {
                message: tool_msg.clone()
            })
            .is_empty()
    );
    assert!(
        bridge
            .translate(LoopEvent::MessageEnd { message: tool_msg })
            .is_empty()
    );
}

/// `MessageUpdate` with End-phase markers are no-ops (the
/// content was already streamed via the corresponding Delta).
#[test]
fn message_update_end_phases_are_no_ops() {
    let mut bridge = EventBridge::new();
    for phase in [
        DeltaPhase::TextEnd,
        DeltaPhase::ThinkingEnd,
        DeltaPhase::ToolCallStart,
        DeltaPhase::ToolCallDelta,
        DeltaPhase::ToolCallEnd,
    ] {
        let out = bridge.translate(LoopEvent::MessageUpdate {
            message: assistant_with_text("any"),
            phase,
        });
        assert!(out.is_empty(), "phase {phase:?} should be no-op");
    }
}

/// `flatten_content` joins multiple text blocks with newlines.
/// Image / other-type blocks fall back to JSON stringify (not
/// dropped).
#[test]
fn flatten_content_joins_text_blocks() {
    let blocks = vec![
        serde_json::json!({"type": "text", "text": "line 1"}),
        serde_json::json!({"type": "text", "text": "line 2"}),
    ];
    assert_eq!(flatten_content(&blocks), "line 1\nline 2");
}

/// `flatten_content` falls back to JSON stringify for
/// non-text blocks. Preserves the data so consumers can
/// inspect.
#[test]
fn flatten_content_stringifies_unknown_blocks() {
    let blocks = vec![
        serde_json::json!({"type": "text", "text": "hello"}),
        serde_json::json!({"type": "image", "url": "https://example/x.png"}),
    ];
    let out = flatten_content(&blocks);
    assert!(out.contains("hello"));
    assert!(out.contains("image"));
}

/// Bridge can be reused across hand-crafted event sequences
/// without polluting state between unrelated calls — except
/// for the documented per-run state (turn_index, last_text).
/// This test confirms state is properly threaded through a
/// realistic full-run event sequence (TurnStart → tokens →
/// tool call → tool result → tokens → TurnEnd → AgentEnd).
#[test]
fn full_run_event_sequence_translates_correctly() {
    let mut bridge = EventBridge::new();
    let mut all = Vec::new();

    all.extend(bridge.translate(LoopEvent::AgentStart));
    all.extend(bridge.translate(LoopEvent::TurnStart));
    all.extend(bridge.translate(LoopEvent::MessageUpdate {
        message: assistant_with_text("Sure, "),
        phase: DeltaPhase::TextStart,
    }));
    all.extend(bridge.translate(LoopEvent::MessageUpdate {
        message: assistant_with_text("Sure, I'll help."),
        phase: DeltaPhase::TextDelta,
    }));
    all.extend(bridge.translate(LoopEvent::ToolExecutionStart {
        tool_call_id: "c1".to_string(),
        tool_name: "read".to_string(),
        args: serde_json::json!({"path": "/x"}),
    }));
    all.extend(bridge.translate(LoopEvent::ToolExecutionEnd {
        tool_call_id: "c1".to_string(),
        tool_name: "read".to_string(),
        result: LoopToolResult {
            content: vec![serde_json::json!({"type": "text", "text": "data"})],
            details: serde_json::Value::Null,
            terminate: None,
        },
        is_error: false,
    }));
    all.extend(bridge.translate(LoopEvent::TurnEnd {
        message: assistant_with_text("Sure, I'll help."),
        tool_results: Vec::new(),
    }));
    all.extend(bridge.translate(LoopEvent::AgentEnd {
        messages: vec![LoopMessage::Assistant(assistant_with_text(
            "final response",
        ))],
    }));

    // Expected sequence: TurnStart(0), Token("Sure, "),
    // Token("I'll help."), ToolCall, ToolStarted, ToolResult,
    // TurnEnd(0), Done.
    let kinds: Vec<_> = all
        .iter()
        .map(|e| match e {
            AgentEvent::Token(_) => "Token",
            AgentEvent::Reasoning(_) => "Reasoning",
            AgentEvent::ToolCall { .. } => "ToolCall",
            AgentEvent::ToolStarted { .. } => "ToolStarted",
            AgentEvent::ToolResult { .. } => "ToolResult",
            AgentEvent::TurnStart { .. } => "TurnStart",
            AgentEvent::TurnEnd { .. } => "TurnEnd",
            AgentEvent::Done { .. } => "Done",
            AgentEvent::Error(_) => "Error",
            AgentEvent::ContextOverflow { .. } => "ContextOverflow",
            AgentEvent::ContextCompacted { .. } => "ContextCompacted",
            AgentEvent::Interjected { .. } => "Interjected",
            AgentEvent::CustomMessage { .. } => "CustomMessage",
            AgentEvent::UserMessage { .. } => "UserMessage",
            AgentEvent::RetryNotice { .. } => "RetryNotice",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "TurnStart",
            "Token",
            "Token",
            "ToolCall",
            "ToolStarted",
            "ToolResult",
            "TurnEnd",
            "Done",
        ]
    );
}
