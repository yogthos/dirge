//! dirge-kwz — end-to-end integration tests for `run_agent_loop`
//! outside the `h7_smoke` provider-stream harness.
//!
//! These tests drive the full loop via mock `StreamFn`s + recording
//! `LoopTool`s. They exercise:
//!   1. single-turn happy path event sequence
//!   2. tool-call → result → second turn flow
//!   3. parallel tool-call ordering invariants
//!   4. storm-breaker tripping on 3rd identical call
//!   5. AbortSignal cancellation mid-stream
//!   6. repair-exhaustion arming the escalation stream
//!
//! Mock patterns mirror `run_tests.rs` (canned StreamFn factories,
//! inline LoopTool impls). The intent is end-to-end coverage of the
//! `run_agent_loop` public API as the loop's primary consumers will
//! drive it.

use super::*;
use crate::agent::agent_loop::message::{StreamEvent, UserMessage};
use crate::agent::agent_loop::result::LoopToolResult;
use crate::agent::agent_loop::stream::StreamFn;
use crate::agent::agent_loop::tool::{AbortSignal, LoopTool, LoopToolUpdate};
use crate::agent::agent_loop::types::{ConvertToLlmFn, LoopConfig, ToolExecutionMode};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

// ---------------------------------------------------------------
// Helpers — copy-paste-adapted from run_tests.rs.
// ---------------------------------------------------------------

fn identity_converter() -> ConvertToLlmFn {
    std::sync::Arc::new(|messages: &[Value]| {
        messages
            .iter()
            .filter(|m| {
                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                matches!(role, "user" | "assistant" | "tool" | "toolResult")
            })
            .cloned()
            .collect()
    })
}

fn build_config() -> LoopConfig {
    LoopConfig {
        convert_to_llm: identity_converter(),
        transform_context: None,
        compaction_hooks: None,
        get_api_key: None,
        api_key: None,
        tool_execution: ToolExecutionMode::Parallel,
        before_tool_call: None,
        after_tool_call: None,
        prepare_next_turn: None,
        should_stop_after_turn: None,
        get_steering_messages: None,
        get_followup_messages: None,
        reasoning: None,
        thinking_budgets: None,
        headers: std::collections::HashMap::new(),
        metadata: std::collections::HashMap::new(),
        request_timeout: None,
        provider_name: None,
        model_name: None,
        compact_model: None,
        storm_mutating_tools: None,
        storm_exempt_tools: None,
        repair_stats: std::sync::Arc::new(
            crate::agent::agent_loop::tool_input_repair::RepairStats::new(),
        ),
        truncation_notes: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        tool_def_filter: None,
        dynamic_tool_search: false,
        escalation_stream_fn: None,
        escalation_provider_name: None,
        escalation_pending: std::sync::Arc::new(std::sync::Mutex::new(None)),
        escalation_max_per_session: 3,
        escalation_remaining: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(3)),
        file_touch_tracker: None,
        verifier: None,
        max_turns: None,
    }
}

fn empty_context() -> Context {
    Context {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
    }
}

fn user(text: &str) -> LoopMessage {
    LoopMessage::User(UserMessage {
        content: text.to_string(),
    })
}

fn text_response(text: &str) -> AssistantMessage {
    AssistantMessage::new(
        vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        StopReason::Stop,
    )
}

fn tool_use_response(id: &str, name: &str, args: Value) -> AssistantMessage {
    AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
        }],
        StopReason::ToolUse,
    )
}

fn multi_tool_use_response(calls: Vec<(&str, &str, Value)>) -> AssistantMessage {
    let content = calls
        .into_iter()
        .map(|(id, name, args)| ContentBlock::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
        })
        .collect();
    AssistantMessage::new(content, StopReason::ToolUse)
}

async fn drain(rx: &mut mpsc::Receiver<LoopEvent>) -> Vec<LoopEvent> {
    let mut out = Vec::new();
    while let Some(e) = rx.recv().await {
        out.push(e);
    }
    out
}

/// Recording mock tool — records (id, args) per call and returns a
/// pre-canned content payload. The execution mode can be configured
/// so the same impl works for sequential and parallel batch tests.
#[derive(Debug)]
struct RecordingTool {
    name_str: String,
    mode: Option<ToolExecutionMode>,
    parameters: Value,
    /// (id, args) per call.
    calls: std::sync::Arc<Mutex<Vec<(String, Value)>>>,
    /// Per-call optional sleep so we can sequence completion order
    /// in the parallel test.
    sleep_ms: u64,
}

impl RecordingTool {
    fn new(name: &str) -> Self {
        Self {
            name_str: name.to_string(),
            mode: None,
            parameters: serde_json::json!({"type": "object"}),
            calls: std::sync::Arc::new(Mutex::new(Vec::new())),
            sleep_ms: 0,
        }
    }

    fn with_sleep_ms(mut self, ms: u64) -> Self {
        self.sleep_ms = ms;
        self
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

impl LoopTool for RecordingTool {
    fn name(&self) -> &str {
        &self.name_str
    }
    fn description(&self) -> &str {
        "Recording mock tool"
    }
    fn label(&self) -> &str {
        "Recording"
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.mode
    }
    fn execute<'a>(
        &'a self,
        id: &'a str,
        args: Value,
        _signal: AbortSignal,
        _on_update: LoopToolUpdate,
    ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>> {
        let calls = self.calls.clone();
        let id = id.to_string();
        let sleep_ms = self.sleep_ms;
        Box::pin(async move {
            if sleep_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
            }
            calls.lock().unwrap().push((id.clone(), args.clone()));
            Ok(LoopToolResult {
                content: vec![serde_json::json!({"type": "text", "text": "ok"})],
                details: args,
                terminate: None,
            })
        })
    }
}

/// Build a StreamFn that emits a scripted sequence of StreamEvent
/// batches — one batch per LLM call. Each batch is a Vec yielded
/// by `futures::stream::iter`.
fn scripted_stream(batches: Vec<Vec<StreamEvent>>) -> StreamFn {
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let batches = std::sync::Arc::new(batches);
    std::sync::Arc::new(move |_ctx, _opts| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let batch = batches.get(n).cloned().unwrap_or_else(|| {
            vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: AssistantMessage::new(
                    vec![ContentBlock::Text {
                        text: "end".to_string(),
                    }],
                    StopReason::Stop,
                ),
                usage: None,
            }]
        });
        Box::pin(futures::stream::iter(batch))
    })
}

// ===============================================================
// 1. Single-turn happy path
// ===============================================================

/// Mock stream emits `Start` → text deltas → `Done`. Loop should
/// emit: AgentStart, TurnStart, MessageStart (user), MessageEnd
/// (user), MessageStart (assistant), MessageEnd (assistant),
/// TurnEnd, AgentEnd.
#[tokio::test]
async fn kwz_single_turn_happy_path_event_sequence() {
    let starting = AssistantMessage::new(Vec::new(), StopReason::Stop);
    let partial_a = AssistantMessage::new(
        vec![ContentBlock::Text {
            text: "Hello".to_string(),
        }],
        StopReason::Stop,
    );
    let partial_b = AssistantMessage::new(
        vec![ContentBlock::Text {
            text: "Hello world".to_string(),
        }],
        StopReason::Stop,
    );

    let stream = scripted_stream(vec![vec![
        StreamEvent::Start {
            partial: starting.clone(),
        },
        StreamEvent::Delta {
            partial: partial_a.clone(),
            phase: super::message::DeltaPhase::TextStart,
        },
        StreamEvent::Delta {
            partial: partial_b.clone(),
            phase: super::message::DeltaPhase::TextDelta,
        },
        StreamEvent::Done {
            reason: StopReason::Stop,
            message: partial_b.clone(),
            usage: None,
        },
    ]]);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
    let messages = run_agent_loop(
        vec![user("hi")],
        empty_context(),
        build_config(),
        AbortSignal::new(),
        &tx,
        &stream,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    let events = drain(&mut rx).await;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind()).collect();

    // Strictly assert the required event types in the order they
    // should fire. `message_update` events fire between
    // message_start and message_end for the assistant turn; we
    // allow them but anchor the sequence by the bracketed events.
    let agent_start = kinds
        .iter()
        .position(|k| *k == "agent_start")
        .expect("agent_start fires");
    let turn_start = kinds
        .iter()
        .position(|k| *k == "turn_start")
        .expect("turn_start fires");
    let turn_end = kinds
        .iter()
        .position(|k| *k == "turn_end")
        .expect("turn_end fires");
    let agent_end = kinds
        .iter()
        .position(|k| *k == "agent_end")
        .expect("agent_end fires");

    assert!(agent_start < turn_start, "agent_start before turn_start");
    assert!(turn_start < turn_end, "turn_start before turn_end");
    assert!(turn_end < agent_end, "turn_end before agent_end");

    // Two message_start / message_end pairs: one for user prompt,
    // one for the assistant. (Plus message_update events between.)
    let starts = kinds.iter().filter(|k| **k == "message_start").count();
    let ends = kinds.iter().filter(|k| **k == "message_end").count();
    assert_eq!(starts, 2, "user + assistant message_start; got {kinds:?}");
    assert_eq!(ends, 2, "user + assistant message_end; got {kinds:?}");

    // Returned transcript: user + assistant.
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role(), "user");
    assert_eq!(messages[1].role(), "assistant");
}

// ===============================================================
// 2. Tool-call turn
// ===============================================================

/// Mock stream: turn 1 emits tool call; loop dispatches against
/// RecordingTool; turn 2 emits final text. Assert tool invoked
/// once with the right args; second LLM call happened.
#[tokio::test]
async fn kwz_tool_call_turn_dispatches_and_continues() {
    let tool = std::sync::Arc::new(RecordingTool::new("echo"));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    let stream = scripted_stream(vec![
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: tool_use_response("call-1", "echo", serde_json::json!({"a": 1})),
            usage: None,
        }],
        vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: text_response("done"),
            usage: None,
        }],
    ]);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
    let messages = run_agent_loop(
        vec![user("call echo")],
        ctx,
        build_config(),
        AbortSignal::new(),
        &tx,
        &stream,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    // Tool invoked exactly once with the original args.
    let calls = tool.calls();
    assert_eq!(calls.len(), 1, "echo invoked once");
    assert_eq!(calls[0].0, "call-1");
    assert_eq!(calls[0].1, serde_json::json!({"a": 1}));

    // Second assistant turn ran (text_response landed).
    // Roles: user, assistant(tool_use), toolResult, assistant(text).
    let roles: Vec<&'static str> = messages.iter().map(|m| m.role()).collect();
    assert_eq!(roles, vec!["user", "assistant", "toolResult", "assistant"]);

    // Required events.
    let kinds: Vec<&str> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
    for required in [
        "agent_start",
        "tool_execution_start",
        "tool_execution_end",
        "agent_end",
    ] {
        assert!(kinds.contains(&required), "missing {required}: {kinds:?}");
    }
    // AgentEnd fires.
    assert_eq!(kinds.last().copied(), Some("agent_end"));
}

// ===============================================================
// 3. Parallel tool calls
// ===============================================================

/// Two parallel tool calls in one assistant message. Both invoked,
/// both ToolResult events land. Source-order assertions on the
/// message_start / message_end side; completion-order doesn't
/// matter for this test (we just verify ordering invariants).
#[tokio::test]
async fn kwz_parallel_tool_calls_both_dispatched() {
    let tool = std::sync::Arc::new(RecordingTool::new("echo").with_sleep_ms(20));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    let stream = scripted_stream(vec![
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: multi_tool_use_response(vec![
                ("call-A", "echo", serde_json::json!({"v": 1})),
                ("call-B", "echo", serde_json::json!({"v": 2})),
            ]),
            usage: None,
        }],
        vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: text_response("done"),
            usage: None,
        }],
    ]);

    let mut cfg = build_config();
    cfg.tool_execution = ToolExecutionMode::Parallel;

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(256);
    let _messages = run_agent_loop(
        vec![user("parallel")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &stream,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    // Tool invoked twice with distinct ids + args.
    let calls = tool.calls();
    assert_eq!(calls.len(), 2, "echo invoked twice");
    let ids: std::collections::HashSet<&str> = calls.iter().map(|(i, _)| i.as_str()).collect();
    assert!(ids.contains("call-A"), "saw call-A");
    assert!(ids.contains("call-B"), "saw call-B");

    let events = drain(&mut rx).await;

    // Both tool_execution_end events present.
    let exec_end_count = events
        .iter()
        .filter(|e| matches!(e, LoopEvent::ToolExecutionEnd { .. }))
        .count();
    assert_eq!(exec_end_count, 2, "both tool_execution_end events fire");

    // Tool-result message_start/end events fire in SOURCE order
    // (per pi spec, tools.rs:822). Verify the first ToolResult
    // message_end refers to call-A, the second to call-B.
    let tr_message_end_ids: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::MessageEnd {
                message: LoopMessage::ToolResult(t),
            } => Some(t.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        tr_message_end_ids,
        vec!["call-A".to_string(), "call-B".to_string()],
        "tool-result message_end fires in source order",
    );

    // Both ToolResult message_starts/ends precede the next
    // assistant turn's message_start. Locate the LAST ToolResult
    // message_end and the FIRST assistant message_start AFTER it.
    let last_tr_end_idx = events
        .iter()
        .rposition(|e| {
            matches!(
                e,
                LoopEvent::MessageEnd {
                    message: LoopMessage::ToolResult(_)
                }
            )
        })
        .expect("at least one tool-result message_end fired");
    let next_assistant_start_after =
        events
            .iter()
            .enumerate()
            .skip(last_tr_end_idx + 1)
            .find(|(_, e)| {
                matches!(
                    e,
                    LoopEvent::MessageStart {
                        message: LoopMessage::Assistant(_),
                    },
                )
            });
    assert!(
        next_assistant_start_after.is_some(),
        "an assistant MessageStart fires AFTER both tool-result message_ends",
    );
}

// ===============================================================
// 4. Storm breaker trips
// ===============================================================

/// Issue 3 identical tool calls across 3 turns. The storm breaker
/// keeps a sliding window across the inner loop (it's reset per
/// outer-loop user turn, not per inner turn). The 3rd identical
/// call should be suppressed.
///
/// Contract surprise found while writing this test: the storm
/// breaker's `reset()` fires at OUTER-loop boundaries, not inner
/// turns. Within a single user turn, identical repeats DO
/// accumulate. So driving 3 consecutive tool-use turns with the
/// same call against the same tool name + args should trip on the
/// 3rd. With Reasonix-style "self_corrected" logic, the third
/// turn gets stubbed with a guard-text result rather than the
/// model's call being dispatched.
#[tokio::test]
async fn kwz_storm_breaker_trips_on_repeat() {
    let tool = std::sync::Arc::new(RecordingTool::new("dup"));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    // Add `dup` to mutating list so the storm breaker counts the
    // calls (storm-exempt tools never trip). Built-in non-mutating
    // tool names like "read" are exempt by default.
    let mut cfg = build_config();
    cfg.storm_mutating_tools = Some(vec!["dup".to_string()]);

    // Five turns of identical tool calls; the breaker should
    // suppress the 3rd onward.
    let identical = || tool_use_response("call-x", "dup", serde_json::json!({"k": "v"}));
    let stream = scripted_stream(vec![
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: identical(),
            usage: None,
        }],
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: identical(),
            usage: None,
        }],
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: identical(),
            usage: None,
        }],
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: identical(),
            usage: None,
        }],
        // Final: text response so the loop exits cleanly if any
        // pending state remains.
        vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: text_response("done"),
            usage: None,
        }],
    ]);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(256);
    let messages = run_agent_loop(
        vec![user("repeat")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &stream,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    // Underlying tool was dispatched on the first 2 calls but NOT
    // on the 3rd identical call. The breaker's "all_suppressed +
    // !turn_self_corrected" path stubs the 3rd call with a guard
    // result, so the underlying tool sees at most 2 invocations.
    let invocations = tool.calls().len();
    assert!(
        invocations <= 2,
        "storm breaker should suppress identical calls past the threshold; \
         tool was invoked {invocations} times",
    );
    assert!(
        invocations >= 2,
        "first two identical calls should reach the tool",
    );

    // The transcript must include at least one tool-result message
    // (either from the real dispatch or from the breaker's guard
    // stub).
    let tool_result_count = messages
        .iter()
        .filter(|m| matches!(m, LoopMessage::ToolResult(_)))
        .count();
    assert!(
        tool_result_count >= 2,
        "at least 2 tool-result messages should land",
    );

    // At least one tool result should carry the guard-text content
    // showing the breaker spoke up (matches the canonical guard
    // string emitted by run.rs).
    let saw_guard = messages.iter().any(|m| match m {
        LoopMessage::ToolResult(t) => t.content.iter().any(|c| match c {
            ContentBlock::Text { text } => text.contains("repeat-loop guard"),
            _ => false,
        }),
        _ => false,
    });
    assert!(
        saw_guard,
        "expected the storm breaker's guard text to land in at least one tool result",
    );
}

/// F4 — in-session reflexion memory. When the storm guard fires, the
/// abandoned approach is recorded and the guard text carries the
/// running list of dead ends. Driving one storm should produce a guard
/// whose text includes the abandoned-approaches block naming the looped
/// tool, proving the reflexion buffer is wired into the guard (not just
/// a standalone module).
#[tokio::test]
async fn reflexion_buffer_surfaces_abandoned_approach_in_guard() {
    let tool = std::sync::Arc::new(RecordingTool::new("dup"));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    let mut cfg = build_config();
    cfg.storm_mutating_tools = Some(vec!["dup".to_string()]);

    let identical = || tool_use_response("call-x", "dup", serde_json::json!({"k": "v"}));
    let stream = scripted_stream(vec![
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: identical(),
            usage: None,
        }],
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: identical(),
            usage: None,
        }],
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: identical(),
            usage: None,
        }],
        vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: text_response("done"),
            usage: None,
        }],
    ]);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(256);
    let messages = run_agent_loop(
        vec![user("repeat")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &stream,
        None,
        None,
    )
    .await;
    drop(tx);

    let saw_reflexion = messages.iter().any(|m| match m {
        LoopMessage::ToolResult(t) => t.content.iter().any(|c| match c {
            ContentBlock::Text { text } => {
                text.contains("abandoned this run") && text.contains("dup(")
            }
            _ => false,
        }),
        _ => false,
    });
    assert!(
        saw_reflexion,
        "guard text should carry the reflexion buffer's abandoned-approaches block naming dup(...)",
    );
}

/// F6 — pre-finalization verifier gate. The agent edits a code file then
/// tries to finalize without running anything; the gate must inject the
/// one-time "verify before done" nudge, which re-enters the loop. Proves
/// the gate is wired through run_agent_loop end-to-end.
#[tokio::test]
async fn verifier_gate_nudges_when_code_edited_without_running() {
    let tool = std::sync::Arc::new(RecordingTool::new("edit"));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    let mut cfg = build_config();
    cfg.verifier = Some(crate::agent::agent_loop::verifier::VerifierGate::new());

    let stream = scripted_stream(vec![
        // Edit a code file...
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: tool_use_response("call-1", "edit", serde_json::json!({"path": "src/x.rs"})),
            usage: None,
        }],
        // ...then immediately try to finish.
        vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: text_response("all done"),
            usage: None,
        }],
        // After the nudge re-enters the loop, finish for real.
        vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: text_response("verified, done"),
            usage: None,
        }],
    ]);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(256);
    let messages = run_agent_loop(
        vec![user("change x")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &stream,
        None,
        None,
    )
    .await;
    drop(tx);

    let saw_nudge = messages.iter().any(|m| match m {
        LoopMessage::User(u) => u.content.contains("[verify-before-done]"),
        _ => false,
    });
    assert!(
        saw_nudge,
        "verifier gate should inject the verify-before-done nudge into the loop",
    );
}

/// The complement: editing code AND running a shell command suppresses
/// the nudge — the gate stays silent when the agent did verify.
#[tokio::test]
async fn verifier_gate_silent_when_a_command_ran() {
    let edit_tool = std::sync::Arc::new(RecordingTool::new("edit"));
    let bash_tool = std::sync::Arc::new(RecordingTool::new("bash"));
    let mut ctx = empty_context();
    ctx.tools.push(edit_tool);
    ctx.tools.push(bash_tool);

    let mut cfg = build_config();
    cfg.verifier = Some(crate::agent::agent_loop::verifier::VerifierGate::new());
    cfg.tool_execution = ToolExecutionMode::Sequential;

    let stream = scripted_stream(vec![
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: tool_use_response("c1", "edit", serde_json::json!({"path": "src/x.rs"})),
            usage: None,
        }],
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: tool_use_response("c2", "bash", serde_json::json!({"command": "cargo test"})),
            usage: None,
        }],
        vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: text_response("done"),
            usage: None,
        }],
    ]);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(256);
    let messages = run_agent_loop(
        vec![user("change x")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &stream,
        None,
        None,
    )
    .await;
    drop(tx);

    let saw_nudge = messages.iter().any(|m| match m {
        LoopMessage::User(u) => u.content.contains("[verify-before-done]"),
        _ => false,
    });
    assert!(
        !saw_nudge,
        "no nudge expected when the agent ran a command after editing",
    );
}

/// Phase D — F1 (few-shot exemplars) and F6 (verifier gate) compose in a
/// single realistic run without interfering. A code-edit task should:
///   1. inject exemplars into the FIRST LLM call (F1), and
///   2. after the model edits code and tries to finish, surface the
///      verify-before-done nudge in a LATER LLM call (F6).
/// A custom factory inspects exactly what each LLM call received, so this
/// proves both features are live end-to-end in the same run.
#[tokio::test]
async fn exemplars_and_verifier_compose_in_one_run() {
    use std::sync::Arc;

    let tool = Arc::new(RecordingTool::new("edit"));
    let mut ctx = empty_context();
    ctx.tools.push(tool);

    let mut cfg = build_config();
    cfg.verifier = Some(crate::agent::agent_loop::verifier::VerifierGate::new());
    cfg.tool_execution = ToolExecutionMode::Sequential;

    let saw_exemplars = Arc::new(Mutex::new(false));
    let saw_nudge = Arc::new(Mutex::new(false));
    let se = saw_exemplars.clone();
    let sn = saw_nudge.clone();
    let counter = Arc::new(AtomicUsize::new(0));

    let factory: StreamFn = Arc::new(move |llm_ctx, _opts| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let has = |needle: &str| {
            llm_ctx.messages.iter().any(|m| {
                m.get("content")
                    .and_then(|c| c.as_str())
                    .map(|s| s.contains(needle))
                    == Some(true)
            })
        };
        if n == 0 {
            *se.lock().unwrap() = has("[Tool-use examples]");
        }
        if n == 2 {
            *sn.lock().unwrap() = has("[verify-before-done]");
        }
        let msg = if n == 0 {
            tool_use_response("c1", "edit", serde_json::json!({"path": "src/auth.rs"}))
        } else {
            text_response("done")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    });

    let (tx, _rx) = mpsc::channel::<LoopEvent>(256);
    let _messages = run_agent_loop(
        vec![user("change the handle_login function in src")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None,
    )
    .await;
    drop(tx);

    assert!(
        *saw_exemplars.lock().unwrap(),
        "F1: exemplars should be injected into the first LLM call for a code-edit task",
    );
    assert!(
        *saw_nudge.lock().unwrap(),
        "F6: verify-before-done nudge should reach a later LLM call after edit-without-run",
    );
}

// ===============================================================
// 5. AbortSignal mid-turn
// ===============================================================

/// Cancel the signal mid-tool. The loop should exit cleanly (no
/// panic, no hang) — bounded by a hard timeout so a regression
/// can't lock the test runner.
#[tokio::test]
async fn kwz_abort_signal_mid_turn_exits_cleanly() {
    // A tool that polls the signal — sleep is long enough that the
    // signal cancellation should hit while it's running.
    #[derive(Debug)]
    struct SlowTool;
    impl LoopTool for SlowTool {
        fn name(&self) -> &str {
            "slow"
        }
        fn description(&self) -> &str {
            "Slow"
        }
        fn label(&self) -> &str {
            "Slow"
        }
        fn parameters(&self) -> &Value {
            static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            _args: Value,
            _signal: AbortSignal,
            _on_update: LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>> {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(LoopToolResult::default())
            })
        }
    }

    let mut ctx = empty_context();
    ctx.tools.push(std::sync::Arc::new(SlowTool));

    let stream = scripted_stream(vec![
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: tool_use_response("call-1", "slow", serde_json::json!({})),
            usage: None,
        }],
        // Second turn shouldn't be reached.
        vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: text_response("should not appear"),
            usage: None,
        }],
    ]);

    let mut cfg = build_config();
    cfg.tool_execution = ToolExecutionMode::Sequential;

    let signal = AbortSignal::new();
    let signal_clone = signal.clone();

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let task = tokio::spawn(async move {
        run_agent_loop(
            vec![user("start")],
            ctx,
            cfg,
            signal_clone,
            &tx,
            &stream,
            None,
            None, // memory_provider — test default
        )
        .await
    });

    // Let the loop reach the tool dispatch then cancel.
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    signal.cancel();

    // Hard bound — the abort-on-cancel path in tools.rs races
    // execute against the signal poll, so the tool's 30s sleep
    // must not lock us out.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
    assert!(
        outcome.is_ok(),
        "loop must exit within 2s of signal cancel — got {outcome:?}",
    );
}

// ===============================================================
// 6. Repair-exhaustion arms escalation
// ===============================================================

/// Install an escalation_stream_fn; first stream emits a tool call
/// with args that fail schema validation AND cannot be repaired
/// (we use a tool whose parameters schema requires a field we
/// omit). Verify: `EscalationActivated` event fires; the SECOND
/// LLM call goes through `escalation_stream_fn`.
#[tokio::test]
async fn kwz_repair_exhaustion_arms_escalation_stream() {
    // Tool with a strict schema: requires `name` (string).
    #[derive(Debug)]
    struct StrictTool;
    impl LoopTool for StrictTool {
        fn name(&self) -> &str {
            "strict"
        }
        fn description(&self) -> &str {
            "Strict"
        }
        fn label(&self) -> &str {
            "Strict"
        }
        fn parameters(&self) -> &Value {
            static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                    },
                    "required": ["name"],
                })
            })
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            args: Value,
            _signal: AbortSignal,
            _on_update: LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>> {
            Box::pin(async move {
                Ok(LoopToolResult {
                    content: vec![serde_json::json!({"type": "text", "text": "ok"})],
                    details: args,
                    terminate: None,
                })
            })
        }
    }

    let mut ctx = empty_context();
    ctx.tools.push(std::sync::Arc::new(StrictTool));

    // Default (primary) stream: first call emits a tool call with
    // args missing `name`. Escalation stream: emits a final text
    // response. The SECOND LLM call should route through
    // escalation_stream_fn, not the default.
    let default_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let escalation_calls = std::sync::Arc::new(AtomicUsize::new(0));

    let default_calls_clone = default_calls.clone();
    let default_stream: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
        let n = default_calls_clone.fetch_add(1, Ordering::SeqCst);
        // Call 0: invalid tool call (missing required `name`).
        // Subsequent calls (shouldn't occur): bail with text.
        let batch = if n == 0 {
            vec![StreamEvent::Done {
                reason: StopReason::ToolUse,
                message: tool_use_response("call-1", "strict", serde_json::json!({"bogus": 1})),
                usage: None,
            }]
        } else {
            vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: text_response("default-fallback"),
                usage: None,
            }]
        };
        Box::pin(futures::stream::iter(batch))
    });

    let escalation_calls_clone = escalation_calls.clone();
    let escalation_stream: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
        escalation_calls_clone.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: text_response("escalation-done"),
            usage: None,
        }]))
    });

    let mut cfg = build_config();
    cfg.escalation_stream_fn = Some(escalation_stream);
    cfg.escalation_provider_name = Some("alt-provider".to_string());

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(256);
    let _messages = run_agent_loop(
        vec![user("go")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &default_stream,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    // The default stream ran once (the failing tool-call turn);
    // the escalation stream ran once (the next turn).
    assert_eq!(
        default_calls.load(Ordering::SeqCst),
        1,
        "default stream invoked exactly once",
    );
    assert_eq!(
        escalation_calls.load(Ordering::SeqCst),
        1,
        "escalation stream invoked exactly once",
    );

    // EscalationActivated event fired with the configured provider
    // and a RepairExhausted reason for "strict".
    let events = drain(&mut rx).await;
    let mut saw_escalation = false;
    for e in &events {
        if let LoopEvent::EscalationActivated { provider, reason } = e {
            assert_eq!(provider, "alt-provider");
            match reason {
                super::message::EscalationReason::RepairExhausted { tool } => {
                    assert_eq!(tool, "strict");
                }
                other => panic!("expected RepairExhausted, got {other:?}"),
            }
            saw_escalation = true;
        }
    }
    assert!(saw_escalation, "expected EscalationActivated event");
}

// ===============================================================
// dirge-nqr — max_turns cap actually terminates the run
// ===============================================================

/// LoopConfig.max_turns = Some(2). The mock stream emits a tool
/// call on EVERY turn (would otherwise loop forever). After two
/// assistant turns complete, the loop should terminate with the
/// max-turns notice appended to new_messages.
#[tokio::test]
async fn nqr_max_turns_cap_terminates_run() {
    let tool = std::sync::Arc::new(RecordingTool::new("echo"));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    // Stream emits tool calls indefinitely — the cap should bite.
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let stream: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
        let n = calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("call-{n}");
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: tool_use_response(&id, "echo", serde_json::json!({"i": n})),
            usage: None,
        }]))
    });

    let mut cfg = build_config();
    cfg.max_turns = Some(2);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
    let messages = run_agent_loop(
        vec![user("loop forever")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &stream,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    // Stream invoked exactly max_turns times (no more).
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);

    // Last message is the dirge-nqr cap-reached notice.
    let last = messages.last().expect("at least one message");
    let role = last.role();
    assert_eq!(role, "user");
    let LoopMessage::User(u) = last else {
        panic!("expected user message, got {role:?}");
    };
    assert!(
        u.content.contains("Max agent turns"),
        "cap notice missing: {:?}",
        u.content
    );

    // The cap is surfaced to the user as a SystemNotice (a `<system>`
    // log line), NOT as a MessageStart { User } (which would render with
    // the `<you>` prefix as if the user typed it).
    let events = drain(&mut rx).await;
    let notice = events.iter().find_map(|e| match e {
        LoopEvent::SystemNotice { content } => Some(content.clone()),
        _ => None,
    });
    assert!(
        notice
            .as_deref()
            .is_some_and(|c| c.contains("Max agent turns")),
        "expected a SystemNotice carrying the cap message, events: {:?}",
        events.iter().map(|e| e.kind()).collect::<Vec<_>>()
    );
    // The cap text must NOT be emitted as a user MessageStart/MessageEnd.
    let leaked_as_user = events.iter().any(|e| match e {
        LoopEvent::MessageStart {
            message: LoopMessage::User(u),
        }
        | LoopEvent::MessageEnd {
            message: LoopMessage::User(u),
        } => u.content.contains("Max agent turns"),
        _ => false,
    });
    assert!(
        !leaked_as_user,
        "cap notice must not be emitted as a <you> user message"
    );

    // agent_end still fires after the cap.
    let kinds: Vec<&str> = events.iter().map(|e| e.kind()).collect();
    assert_eq!(kinds.last().copied(), Some("agent_end"));
}

/// Default behavior (max_turns = None) is unchanged: the loop
/// runs until the stream itself stops asking for tool calls.
#[tokio::test]
async fn nqr_unlimited_when_max_turns_none() {
    let tool = std::sync::Arc::new(RecordingTool::new("echo"));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    // Tool call, then text response — no cap should be needed.
    let stream = scripted_stream(vec![
        vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: tool_use_response("call-1", "echo", serde_json::json!({})),
            usage: None,
        }],
        vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: text_response("done"),
            usage: None,
        }],
    ]);

    let cfg = build_config(); // max_turns: None
    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let messages = run_agent_loop(
        vec![user("hi")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &stream,
        None,
        None, // memory_provider — test default
    )
    .await;

    // No cap notice present.
    for m in &messages {
        if let LoopMessage::User(u) = m {
            assert!(
                !u.content.contains("Max agent turns"),
                "unexpected cap notice"
            );
        }
    }
}
