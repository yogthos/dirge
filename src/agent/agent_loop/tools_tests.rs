use super::*;
use crate::agent::agent_loop::hooks::{BeforeToolCallFn, BeforeToolCallReturn};
use crate::agent::agent_loop::message::{ContentBlock, StopReason};
use crate::agent::agent_loop::result::{AfterToolCallResult, BeforeToolCallResult};
use crate::agent::agent_loop::types::{ConvertToLlmFn, ToolExecutionMode};
use std::pin::Pin;
use std::sync::Mutex;

/// Mock LoopTool that records its calls and returns a canned
/// result. Used by phase-2 tests in lieu of a real rig tool.
struct EchoTool {
    name: String,
    /// Set by tests to control whether `prepare_arguments`
    /// mutates the input shape (pi test 372).
    prepare_arguments_fn: Option<Box<dyn Fn(Value) -> Value + Send + Sync>>,
    /// Set by tests to override `execution_mode`. Phase 3
    /// uses this to force-sequential individual tools in a
    /// parallel-by-default batch (pi tests 653, 736).
    execution_mode: Option<ToolExecutionMode>,
    /// Set by tests to inject `terminate: true` into every
    /// result (pi test 1067).
    terminate: bool,
    /// Recorded args passed to `execute` (so tests can
    /// assert mutations from beforeToolCall took effect).
    executed_args: Arc<Mutex<Vec<Value>>>,
    /// Phase 3: artificial delay before returning. Used to
    /// make one tool slower than another so completion-order
    /// vs source-order is observable. Pi test 452 uses a
    /// `firstDone` promise; we use sleep for simplicity (the
    /// extra wall time is fine in a test).
    delay_ms: Option<u64>,
    /// Phase 3: per-call args-driven delay. Pi test 452 has
    /// the slow tool gated on `args.value === "first"`. We
    /// match: if `args.value == "first"`, sleep for
    /// `delay_first_ms`; if `args.value == "second"`, return
    /// immediately AND record whether the first was still
    /// running.
    delay_first_ms: Option<u64>,
    /// Phase 3: concurrency observer. Tracks (currently
    /// inside execute, max ever seen concurrently). The
    /// "parallel runs concurrent" test asserts max > 1 (pi
    /// test 823).
    concurrency: Arc<Mutex<(u32, u32)>>,
    /// Phase 3: set true when a "second" call sees a "first"
    /// call still in flight. Pi test 452 calls this
    /// `parallelObserved` at line 472.
    parallel_observed: Arc<Mutex<bool>>,
}

impl std::fmt::Debug for EchoTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EchoTool")
            .field("name", &self.name)
            .field("execution_mode", &self.execution_mode)
            .field("terminate", &self.terminate)
            .finish()
    }
}

impl EchoTool {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            prepare_arguments_fn: None,
            execution_mode: None,
            terminate: false,
            executed_args: Arc::new(Mutex::new(Vec::new())),
            delay_ms: None,
            delay_first_ms: None,
            concurrency: Arc::new(Mutex::new((0, 0))),
            parallel_observed: Arc::new(Mutex::new(false)),
        }
    }
    fn with_prepare(mut self, f: impl Fn(Value) -> Value + Send + Sync + 'static) -> Self {
        self.prepare_arguments_fn = Some(Box::new(f));
        self
    }
    fn with_terminate(mut self) -> Self {
        self.terminate = true;
        self
    }
    fn with_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = Some(mode);
        self
    }
    fn with_delay_ms(mut self, ms: u64) -> Self {
        self.delay_ms = Some(ms);
        self
    }
    /// Phase 3 test 452: gate the delay on
    /// `args.value == "first"`. Other values return
    /// immediately.
    fn with_delay_first_ms(mut self, ms: u64) -> Self {
        self.delay_first_ms = Some(ms);
        self
    }
    /// Snapshot of the (current, max) concurrency counter.
    fn concurrency_snapshot(&self) -> (u32, u32) {
        *self.concurrency.lock().unwrap()
    }
    fn parallel_was_observed(&self) -> bool {
        *self.parallel_observed.lock().unwrap()
    }
}

impl LoopTool for EchoTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Echo tool"
    }
    fn label(&self) -> &str {
        "Echo"
    }
    fn parameters(&self) -> &Value {
        // Phase 2 doesn't validate; an empty object is fine.
        static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.execution_mode
    }
    fn prepare_arguments(&self, args: Value) -> Value {
        if let Some(f) = &self.prepare_arguments_fn {
            f(args)
        } else {
            args
        }
    }
    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        args: Value,
        _signal: AbortSignal,
        _on_update: LoopToolUpdate,
    ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>> {
        let recorded = self.executed_args.clone();
        let terminate = self.terminate;
        let delay_ms = self.delay_ms;
        let delay_first_ms = self.delay_first_ms;
        let concurrency = self.concurrency.clone();
        let parallel_observed = self.parallel_observed.clone();
        Box::pin(async move {
            // Phase 3: track concurrency on entry.
            {
                let mut c = concurrency.lock().unwrap();
                c.0 += 1;
                if c.0 > c.1 {
                    c.1 = c.0;
                }
            }
            // Phase 3 pi:452: per-call delay gated on
            // args.value. The "second" tool checks whether
            // "first" is still running and records the
            // parallel observation.
            let value_str = args
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(ms) = delay_first_ms
                && value_str == "first"
            {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
            if delay_first_ms.is_some() && value_str == "second" {
                // Pi:472 — record that first was still in
                // flight when second ran.
                let c = concurrency.lock().unwrap();
                if c.0 > 1 {
                    *parallel_observed.lock().unwrap() = true;
                }
            }
            if let Some(ms) = delay_ms {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
            recorded.lock().unwrap().push(args.clone());
            // Phase 3: decrement concurrency on exit.
            {
                let mut c = concurrency.lock().unwrap();
                c.0 -= 1;
            }
            let text = format!("echoed: {}", args);
            Ok(LoopToolResult {
                content: vec![serde_json::json!({"type": "text", "text": text})],
                details: args,
                terminate: if terminate { Some(true) } else { None },
            })
        })
    }
}

fn identity_converter() -> ConvertToLlmFn {
    Arc::new(|messages: &[Value]| messages.to_vec())
}

fn build_config() -> LoopConfig {
    LoopConfig {
        convert_to_llm: identity_converter(),
        transform_context: None,
        compaction_hooks: None,
        get_api_key: None,
        api_key: None,
        tool_execution: ToolExecutionMode::Sequential,
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
        critic_fn: None,
        max_turns: None,
    }
}

fn build_context(tool: Arc<dyn LoopTool>) -> Context {
    Context {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    }
}

/// Port of pi test "should handle tool calls and results"
/// (agent-loop.test.ts:239). Phase-2 scope: verify the
/// sequential dispatcher actually invokes the tool, emits
/// the expected lifecycle events, and produces a non-error
/// tool-result message. The full agent-loop flow (assistant
/// turn → tool → next assistant turn) is verified in phase 4.
#[tokio::test]
async fn test_handle_tool_calls_and_results() {
    let echo = Arc::new(EchoTool::new("echo"));
    let context = build_context(echo.clone());
    let assistant_msg = AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: "tool-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"value": "hello"}),
        }],
        StopReason::ToolUse,
    );
    let tool_calls = extract_tool_calls(&assistant_msg);
    assert_eq!(tool_calls.len(), 1);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
    let config = build_config();
    let signal = AbortSignal::new();

    let batch = execute_tool_calls_sequential(
        &context,
        &assistant_msg,
        &tool_calls,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    drop(tx);

    // Tool executed; args reached `execute`.
    let recorded = echo.executed_args.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["value"], "hello");
    drop(recorded);

    // Batch shape: one non-error message; not terminating.
    assert_eq!(batch.messages.len(), 1);
    assert!(!batch.messages[0].is_error);
    assert!(!batch.terminate);

    // Event sequence: tool_execution_start →
    // tool_execution_end → message_start (toolResult) →
    // message_end (toolResult).
    let mut kinds = Vec::new();
    while let Some(e) = rx.recv().await {
        kinds.push(e.kind().to_string());
    }
    assert_eq!(
        kinds,
        vec![
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
        ]
    );
}

/// Port of pi test "should execute mutated beforeToolCall
/// args without revalidation" (agent-loop.test.ts:310). The
/// before-hook mutates `args.value` to a new value; the tool
/// must see the mutated args.
#[tokio::test]
async fn test_before_tool_call_mutates_args() {
    let echo = Arc::new(EchoTool::new("echo"));
    let context = build_context(echo.clone());
    let assistant_msg = AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: "tool-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"value": "hello"}),
        }],
        StopReason::ToolUse,
    );
    let tool_calls = extract_tool_calls(&assistant_msg);

    // Hook: replace args.value with 123.
    let before: BeforeToolCallFn = Arc::new(|ctx: BeforeToolCallContext| {
        Box::pin(async move {
            let mut args = ctx.args.clone();
            if let Some(obj) = args.as_object_mut() {
                obj.insert("value".to_string(), serde_json::json!(123));
            }
            BeforeToolCallReturn { result: None, args }
        })
    });
    let mut config = build_config();
    config.before_tool_call = Some(before);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
    let signal = AbortSignal::new();
    let _ = execute_tool_calls_sequential(
        &context,
        &assistant_msg,
        &tool_calls,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    drop(tx);
    while rx.recv().await.is_some() {}

    // The tool must have observed the MUTATED args.
    let recorded = echo.executed_args.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["value"], serde_json::json!(123));
}

/// Port of pi test "should prepare tool arguments for
/// validation" (agent-loop.test.ts:372). The
/// `prepare_arguments` shim transforms the raw provider args
/// `{oldText, newText}` into the schema-shape
/// `{edits: [{oldText, newText}]}` before the tool executes.
#[tokio::test]
async fn test_prepare_arguments_shim() {
    let edit = Arc::new(EchoTool::new("edit").with_prepare(|args: Value| {
        // Pi-faithful: if input has oldText+newText at the
        // top level, wrap into `{edits: [{oldText, newText}]}`.
        if let Some(obj) = args.as_object()
            && obj.contains_key("oldText")
            && obj.contains_key("newText")
        {
            return serde_json::json!({
                "edits": [{
                    "oldText": obj.get("oldText").unwrap(),
                    "newText": obj.get("newText").unwrap(),
                }]
            });
        }
        args
    }));
    let context = build_context(edit.clone());
    let assistant_msg = AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: "tool-1".to_string(),
            name: "edit".to_string(),
            arguments: serde_json::json!({"oldText": "before", "newText": "after"}),
        }],
        StopReason::ToolUse,
    );
    let tool_calls = extract_tool_calls(&assistant_msg);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
    let config = build_config();
    let signal = AbortSignal::new();
    let _ = execute_tool_calls_sequential(
        &context,
        &assistant_msg,
        &tool_calls,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    drop(tx);
    while rx.recv().await.is_some() {}

    let recorded = edit.executed_args.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    let edits = recorded[0].get("edits").expect("shim should produce edits");
    let arr = edits.as_array().expect("edits is array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["oldText"], "before");
    assert_eq!(arr[0]["newText"], "after");
}

/// Phase-2 scope of pi test "should stop after a tool batch
/// when every tool result sets terminate=true"
/// (agent-loop.test.ts:1067). Pi's test verifies the LOOP
/// stops; phase 2 verifies the DISPATCHER returns
/// `terminate: true`. Loop-level verification lands in
/// phase 4 when the loop drives the dispatcher.
#[tokio::test]
async fn test_dispatcher_terminate_when_all_results_terminate() {
    let echo = Arc::new(EchoTool::new("echo").with_terminate());
    let context = build_context(echo.clone());
    let assistant_msg = AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: "tool-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({}),
        }],
        StopReason::ToolUse,
    );
    let tool_calls = extract_tool_calls(&assistant_msg);
    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let config = build_config();
    let signal = AbortSignal::new();
    let batch = execute_tool_calls_sequential(
        &context,
        &assistant_msg,
        &tool_calls,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    assert!(
        batch.terminate,
        "single terminate=true should set batch.terminate"
    );
}

/// Phase-2 scope of pi test "should allow afterToolCall to
/// mark a tool batch as terminating" (agent-loop.test.ts:1184).
/// afterToolCall returns `{ terminate: true }` even though
/// the underlying tool didn't set terminate; the override
/// propagates.
#[tokio::test]
async fn test_after_tool_call_can_set_terminate() {
    let echo = Arc::new(EchoTool::new("echo")); // no inherent terminate
    let context = build_context(echo);
    let assistant_msg = AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: "tool-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({}),
        }],
        StopReason::ToolUse,
    );
    let tool_calls = extract_tool_calls(&assistant_msg);

    let after: crate::agent::agent_loop::hooks::AfterToolCallFn = Arc::new(|_ctx| {
        Box::pin(async move {
            Some(AfterToolCallResult {
                content: None,
                details: None,
                is_error: None,
                terminate: Some(true),
            })
        })
    });
    let mut config = build_config();
    config.after_tool_call = Some(after);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let signal = AbortSignal::new();
    let batch = execute_tool_calls_sequential(
        &context,
        &assistant_msg,
        &tool_calls,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    assert!(
        batch.terminate,
        "afterToolCall override should mark batch terminating"
    );
}

/// Tool not found → immediate error result. Port of pi
/// `prepareToolCall` line 569-576 — the "Tool X not found"
/// short-circuit.
#[tokio::test]
async fn test_tool_not_found_immediate_error() {
    let echo = Arc::new(EchoTool::new("echo"));
    let context = build_context(echo);
    let assistant_msg = AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: "tool-1".to_string(),
            name: "nonexistent".to_string(),
            arguments: serde_json::json!({}),
        }],
        StopReason::ToolUse,
    );
    let tool_calls = extract_tool_calls(&assistant_msg);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let config = build_config();
    let signal = AbortSignal::new();
    let batch = execute_tool_calls_sequential(
        &context,
        &assistant_msg,
        &tool_calls,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    assert_eq!(batch.messages.len(), 1);
    assert!(batch.messages[0].is_error);
    // Error message contains the missing-tool name.
    match &batch.messages[0].content[0] {
        ContentBlock::Text { text } => assert!(
            text.contains("nonexistent"),
            "error text should name the missing tool: {text}"
        ),
        _ => panic!("expected text content block"),
    }
}

/// beforeToolCall block=true → immediate error with reason.
/// Port of pi `prepareToolCall` lines 598-604.
#[tokio::test]
async fn test_before_tool_call_block_with_reason() {
    let echo = Arc::new(EchoTool::new("echo"));
    let context = build_context(echo.clone());
    let assistant_msg = AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: "tool-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({}),
        }],
        StopReason::ToolUse,
    );
    let tool_calls = extract_tool_calls(&assistant_msg);

    let before: BeforeToolCallFn = Arc::new(|ctx: BeforeToolCallContext| {
        Box::pin(async move {
            BeforeToolCallReturn {
                result: Some(BeforeToolCallResult {
                    block: Some(true),
                    reason: Some("policy violation".to_string()),
                }),
                args: ctx.args,
            }
        })
    });
    let mut config = build_config();
    config.before_tool_call = Some(before);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let signal = AbortSignal::new();
    let batch = execute_tool_calls_sequential(
        &context,
        &assistant_msg,
        &tool_calls,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;

    // Tool never executed.
    assert!(echo.executed_args.lock().unwrap().is_empty());
    // Result is an error with our reason text.
    assert!(batch.messages[0].is_error);
    match &batch.messages[0].content[0] {
        ContentBlock::Text { text } => {
            assert!(text.contains("policy violation"), "got: {text}");
        }
        _ => panic!("expected text content block"),
    }
}

/// `should_terminate_tool_batch` invariants:
///   - empty batch → false
///   - some terminate=false → false
///   - all terminate=true → true
/// Faithful port of pi line 544.
#[test]
fn should_terminate_invariants() {
    let make = |terminate: Option<bool>| FinalizedOutcome {
        tool_call: ToolCall {
            id: "x".into(),
            name: "x".into(),
            arguments: Value::Null,
        },
        result: LoopToolResult {
            content: vec![],
            details: Value::Null,
            terminate,
        },
        is_error: false,
    };
    assert!(!should_terminate_tool_batch(&[]));
    assert!(!should_terminate_tool_batch(&[make(Some(false))]));
    assert!(!should_terminate_tool_batch(&[make(None)]));
    assert!(!should_terminate_tool_batch(&[
        make(Some(true)),
        make(Some(false))
    ]));
    assert!(should_terminate_tool_batch(&[make(Some(true))]));
    assert!(should_terminate_tool_batch(&[
        make(Some(true)),
        make(Some(true)),
    ]));
}

// =================================================================
// Phase 3 tests — parallel dispatcher + per-tool sequential override
// =================================================================

/// Helper: build two ToolCalls for echo with "first" / "second"
/// values matching pi:452's setup.
fn two_echo_calls() -> Vec<ToolCall> {
    vec![
        ToolCall {
            id: "tool-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"value": "first"}),
        },
        ToolCall {
            id: "tool-2".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"value": "second"}),
        },
    ]
}

fn assistant_with_calls(calls: &[ToolCall]) -> AssistantMessage {
    let content = calls
        .iter()
        .map(|c| ContentBlock::ToolCall {
            id: c.id.clone(),
            name: c.name.clone(),
            arguments: c.arguments.clone(),
        })
        .collect();
    AssistantMessage::new(content, StopReason::ToolUse)
}

/// Port of pi test "should emit tool_execution_end in
/// completion order but persist tool results in source order"
/// (agent-loop.test.ts:452). THE key parallel-correctness
/// test:
///   - tool-1 ("first") sleeps 50ms
///   - tool-2 ("second") returns immediately
///   → tool_execution_end events in COMPLETION order:
///     [tool-2, tool-1]
///   → message_end events for tool-results in SOURCE order:
///     [tool-1, tool-2]
///   → parallel_observed = true (second saw first in flight)
#[tokio::test]
async fn test_tool_execution_end_completion_order_results_source_order() {
    let echo = Arc::new(EchoTool::new("echo").with_delay_first_ms(50));
    let context = build_context(echo.clone());
    let calls = two_echo_calls();
    let assistant = assistant_with_calls(&calls);

    let mut config = build_config();
    config.tool_execution = ToolExecutionMode::Parallel;

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
    let signal = AbortSignal::new();
    let _batch = execute_tool_calls_parallel(
        &context,
        &assistant,
        &calls,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    drop(tx);

    // Drain events; collect ordering observations.
    let mut tool_execution_end_ids: Vec<String> = Vec::new();
    let mut tool_result_message_end_ids: Vec<String> = Vec::new();
    while let Some(e) = rx.recv().await {
        match &e {
            LoopEvent::ToolExecutionEnd { tool_call_id, .. } => {
                tool_execution_end_ids.push(tool_call_id.clone());
            }
            LoopEvent::MessageEnd { message } => {
                if let LoopMessage::ToolResult(t) = message {
                    tool_result_message_end_ids.push(t.tool_call_id.clone());
                }
            }
            _ => {}
        }
    }

    // Completion order: tool-2 (fast) finishes before tool-1
    // (slow).
    assert_eq!(
        tool_execution_end_ids,
        vec!["tool-2".to_string(), "tool-1".to_string()],
        "tool_execution_end should be in completion order"
    );
    // Source order: tool-1 then tool-2.
    assert_eq!(
        tool_result_message_end_ids,
        vec!["tool-1".to_string(), "tool-2".to_string()],
        "tool-result message_end should be in source order"
    );
    // Concurrency observed: tool-2 saw tool-1 still running.
    assert!(
        echo.parallel_was_observed(),
        "second tool should have observed first still in flight"
    );
}

/// Port of pi test "should force sequential execution when a
/// tool has executionMode=sequential even with default
/// parallel config" (agent-loop.test.ts:653).
///
/// Setup: one tool, executionMode=Sequential. Config defaults
/// to Parallel. Even though only ONE tool is in the batch,
/// the umbrella dispatcher should route through the
/// sequential path because the tool ITSELF declares sequential.
///
/// We verify by introspecting the EchoTool's concurrency
/// counter — sequential dispatch never exceeds 1 in flight.
#[tokio::test]
async fn test_per_tool_sequential_forces_sequential_route() {
    let echo = Arc::new(
        EchoTool::new("echo")
            .with_execution_mode(ToolExecutionMode::Sequential)
            .with_delay_first_ms(20),
    );
    let context = build_context(echo.clone());
    let calls = two_echo_calls();
    let assistant = assistant_with_calls(&calls);

    let mut config = build_config();
    // Config default is Parallel; per-tool override should
    // win.
    config.tool_execution = ToolExecutionMode::Parallel;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let signal = AbortSignal::new();
    let batch = execute_tool_calls_from_msg(
        &context,
        &assistant,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    drop(tx);

    // Sequential dispatch: max concurrency == 1.
    let (_current, max) = echo.concurrency_snapshot();
    assert_eq!(
        max, 1,
        "per-tool Sequential should force max concurrency = 1, got {max}"
    );
    assert_eq!(batch.messages.len(), 2);
}

/// Port of pi test "should force sequential execution when
/// one of multiple tools has executionMode=sequential"
/// (agent-loop.test.ts:736).
///
/// Setup: two DIFFERENT tools, one marked Sequential. Even
/// though the OTHER tool defaults to Parallel, the batch
/// runs sequentially because ANY tool with Sequential forces
/// the whole batch.
#[tokio::test]
async fn test_one_sequential_among_many_forces_sequential() {
    let echo_seq = Arc::new(
        EchoTool::new("echo_seq")
            .with_execution_mode(ToolExecutionMode::Sequential)
            .with_delay_ms(10),
    );
    let echo_par = Arc::new(EchoTool::new("echo_par").with_delay_ms(10));

    // Tool registry has BOTH tools — dispatcher resolves by
    // name.
    let context = Context {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![echo_seq.clone(), echo_par.clone()],
    };

    let calls = vec![
        ToolCall {
            id: "tool-1".into(),
            name: "echo_par".into(),
            arguments: serde_json::json!({"v": 1}),
        },
        ToolCall {
            id: "tool-2".into(),
            name: "echo_seq".into(),
            arguments: serde_json::json!({"v": 2}),
        },
    ];
    let assistant = assistant_with_calls(&calls);

    let mut config = build_config();
    config.tool_execution = ToolExecutionMode::Parallel;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let signal = AbortSignal::new();
    let _ = execute_tool_calls_from_msg(
        &context,
        &assistant,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    drop(tx);

    // Neither tool ever saw concurrency > 1.
    let (_, max_seq) = echo_seq.concurrency_snapshot();
    let (_, max_par) = echo_par.concurrency_snapshot();
    assert_eq!(max_seq, 1, "echo_seq max should be 1");
    assert_eq!(max_par, 1, "echo_par max should be 1");
}

/// Port of pi test "should allow parallel execution when all
/// tools have executionMode=parallel" (agent-loop.test.ts:823).
///
/// All tools allow parallel + config is Parallel → dispatcher
/// routes through parallel path → max concurrency should
/// exceed 1 when there's more than one tool call.
#[tokio::test]
async fn test_all_parallel_runs_concurrent() {
    let echo = Arc::new(EchoTool::new("echo").with_delay_first_ms(30));
    let context = build_context(echo.clone());
    let calls = two_echo_calls();
    let assistant = assistant_with_calls(&calls);

    let mut config = build_config();
    config.tool_execution = ToolExecutionMode::Parallel;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let signal = AbortSignal::new();
    let _ = execute_tool_calls_from_msg(
        &context,
        &assistant,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    drop(tx);

    let (_current, max) = echo.concurrency_snapshot();
    assert!(
        max >= 2,
        "parallel dispatch should run >=2 tools concurrently, got {max}"
    );
}

/// Phase-3 scope of pi test "should continue after parallel
/// tool calls when not all tool results terminate"
/// (agent-loop.test.ts:1119). Pi's test asserts the LOOP
/// continues to a second LLM call. Phase 3 verifies the
/// DISPATCHER returns `terminate: false` when not every
/// result has terminate=true. Loop-continue verification
/// lands in phase 4.
#[tokio::test]
async fn test_parallel_batch_not_terminating_when_mixed() {
    // Two tools: one terminating, one not. Result: batch
    // terminate = false (pi line 544: ALL must terminate).
    let echo_term = Arc::new(EchoTool::new("term").with_terminate());
    let echo_norm = Arc::new(EchoTool::new("norm"));
    let context = Context {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![echo_term, echo_norm],
    };
    let calls = vec![
        ToolCall {
            id: "tool-1".into(),
            name: "term".into(),
            arguments: serde_json::json!({}),
        },
        ToolCall {
            id: "tool-2".into(),
            name: "norm".into(),
            arguments: serde_json::json!({}),
        },
    ];
    let assistant = assistant_with_calls(&calls);

    let mut config = build_config();
    config.tool_execution = ToolExecutionMode::Parallel;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let signal = AbortSignal::new();
    let batch = execute_tool_calls_from_msg(
        &context,
        &assistant,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    drop(tx);

    assert!(
        !batch.terminate,
        "batch should NOT terminate when only some results have terminate=true"
    );
    assert_eq!(batch.messages.len(), 2);
}

/// Defensive: parallel dispatch where the prepare phase
/// short-circuits (tool not found) for one call still
/// returns batch with that call as an error. The OTHER call
/// (prepared) runs concurrently. Verifies immediate + async
/// entries coexist in the parallel path.
#[tokio::test]
async fn test_parallel_mixes_immediate_and_async() {
    let echo = Arc::new(EchoTool::new("echo").with_delay_first_ms(20));
    let context = build_context(echo);
    let calls = vec![
        ToolCall {
            id: "tool-1".into(),
            name: "nonexistent".into(), // → immediate error
            arguments: serde_json::json!({}),
        },
        ToolCall {
            id: "tool-2".into(),
            name: "echo".into(),
            arguments: serde_json::json!({"value": "first"}),
        },
    ];
    let assistant = assistant_with_calls(&calls);

    let mut config = build_config();
    config.tool_execution = ToolExecutionMode::Parallel;

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
    let signal = AbortSignal::new();
    let batch = execute_tool_calls_parallel(
        &context,
        &assistant,
        &calls,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    drop(tx);

    // First result is an error (tool not found); second is ok.
    assert_eq!(batch.messages.len(), 2);
    assert!(batch.messages[0].is_error);
    assert!(!batch.messages[1].is_error);

    // Tool-result message_end events still in source order.
    let mut tool_result_ids: Vec<String> = Vec::new();
    while let Some(e) = rx.recv().await {
        if let LoopEvent::MessageEnd {
            message: LoopMessage::ToolResult(t),
        } = e
        {
            tool_result_ids.push(t.tool_call_id);
        }
    }
    assert_eq!(
        tool_result_ids,
        vec!["tool-1".to_string(), "tool-2".to_string()]
    );
}

// ============================================================
// Phase 6 — abort signal awareness during tool execution
// ============================================================

/// A `LoopTool` that blocks for a configurable duration
/// without polling the signal. Simulates a legacy tool
/// (e.g. bash, web fetch) that the agent_loop wraps via
/// `RigToolAdapter` and that doesn't natively support
/// cancellation.
#[derive(Debug)]
struct BlockingTool {
    delay: std::time::Duration,
}

impl LoopTool for BlockingTool {
    fn name(&self) -> &str {
        "block"
    }
    fn description(&self) -> &str {
        "Blocks for a fixed duration without polling signal."
    }
    fn label(&self) -> &str {
        "Block"
    }
    fn parameters(&self) -> &Value {
        static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
    }
    fn execute<'a>(
        &'a self,
        _id: &'a str,
        _args: Value,
        _signal: AbortSignal, // intentionally NOT polled
        _on_update: LoopToolUpdate,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(LoopToolResult {
                content: vec![serde_json::json!({
                    "type": "text",
                    "text": "completed",
                })],
                details: Value::Null,
                terminate: None,
            })
        })
    }
}

/// Phase 6 regression: a tool that doesn't poll the abort
/// signal STILL gets cancelled when the dispatcher's
/// `tokio::select!` observes the signal first. The tool's
/// future is dropped; the dispatched returns an "aborted"
/// error result so the loop can continue (or exit) cleanly.
///
/// Without the select wrap, a long-running tool would block
/// the loop until completion regardless of signal state —
/// Ctrl+C would feel unresponsive.
#[tokio::test]
async fn aborted_tool_returns_aborted_error_promptly() {
    let blocking = Arc::new(BlockingTool {
        // 10s — far longer than the test should take if abort
        // works. If select doesn't honor the signal, the test
        // either hangs or finishes in 10s.
        delay: std::time::Duration::from_secs(10),
    });
    let mut ctx = Context::default();
    ctx.tools.push(blocking.clone());
    let signal = AbortSignal::new();
    // Cancel BEFORE the dispatch starts — the select's
    // signal-poll arm should win immediately.
    signal.cancel();
    let calls = vec![ToolCall {
        id: "tc-1".to_string(),
        name: "block".to_string(),
        arguments: serde_json::json!({}),
    }];
    let assistant = AssistantMessage::new(
        calls
            .iter()
            .map(|c| ContentBlock::ToolCall {
                id: c.id.clone(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            })
            .collect(),
        StopReason::ToolUse,
    );
    let cfg = build_config();
    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let started = std::time::Instant::now();
    let batch = execute_tool_calls_sequential(
        &ctx,
        &assistant,
        &calls,
        &cfg,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "expected near-instant abort; elapsed {elapsed:?}"
    );
    assert_eq!(batch.messages.len(), 1);
    let block = &batch.messages[0].content[0];
    let text = match block {
        ContentBlock::Text { text } => text.clone(),
        other => panic!("expected Text block; got {other:?}"),
    };
    assert!(
        text.contains("aborted"),
        "expected aborted message; got: {text:?}"
    );
    assert!(batch.messages[0].is_error);
}

/// dirge-2mw0: on cancellation the dispatcher DROPS the tool's execute
/// future — it does not leave it running detached. Proven concretely: a
/// probe tool holds an RAII guard inside its future that flips a flag on
/// Drop, and sets a separate `completed` flag only if it runs to the
/// end. After a mid-execution cancel we observe `dropped == true` and
/// `completed == false`.
#[tokio::test]
async fn cancelled_tool_future_is_dropped_not_detached() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[derive(Debug)]
    struct DropProbeTool {
        started: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
        completed: Arc<AtomicBool>,
    }
    impl LoopTool for DropProbeTool {
        fn name(&self) -> &str {
            "probe"
        }
        fn description(&self) -> &str {
            "Signals start, then sleeps; flips a flag if dropped."
        }
        fn label(&self) -> &str {
            "Probe"
        }
        fn parameters(&self) -> &Value {
            static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            _args: Value,
            _signal: AbortSignal, // intentionally NOT polled
            _on_update: LoopToolUpdate,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>>
        {
            let started = self.started.clone();
            let dropped = self.dropped.clone();
            let completed = self.completed.clone();
            Box::pin(async move {
                let _guard = DropFlag(dropped);
                started.store(true, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                completed.store(true, Ordering::SeqCst);
                Ok(LoopToolResult {
                    content: vec![serde_json::json!({"type": "text", "text": "done"})],
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(AtomicBool::new(false));
    let mut ctx = Context::default();
    ctx.tools.push(Arc::new(DropProbeTool {
        started: started.clone(),
        dropped: dropped.clone(),
        completed: completed.clone(),
    }));
    let signal = AbortSignal::new();
    let calls = vec![ToolCall {
        id: "tc-1".to_string(),
        name: "probe".to_string(),
        arguments: serde_json::json!({}),
    }];
    let assistant = AssistantMessage::new(
        calls
            .iter()
            .map(|c| ContentBlock::ToolCall {
                id: c.id.clone(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            })
            .collect(),
        StopReason::ToolUse,
    );
    let cfg = build_config();
    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);

    // Cancel AFTER the tool has started executing, so we exercise the
    // mid-flight drop path (not a pre-dispatch short-circuit).
    let canceller = {
        let signal = signal.clone();
        let started = started.clone();
        async move {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            signal.cancel();
        }
    };
    let inflight = InflightSet::new();
    let dispatch =
        execute_tool_calls_sequential(&ctx, &assistant, &calls, &cfg, &signal, &tx, &inflight);

    let (batch, _) = tokio::time::timeout(std::time::Duration::from_secs(5), async move {
        tokio::join!(dispatch, canceller)
    })
    .await
    .expect("cancellation must drop the future promptly, not wait out the 10s sleep");

    assert!(started.load(Ordering::SeqCst), "tool should have started");
    assert!(
        dropped.load(Ordering::SeqCst),
        "tool future must be dropped on cancel"
    );
    assert!(
        !completed.load(Ordering::SeqCst),
        "tool must NOT run to completion after cancel (no detached execution)"
    );
    assert!(
        batch.messages[0].is_error,
        "result should be the abort error"
    );
}

// ── dirge-du5k/7bwx: truncation brace-closer end-to-end ──────────

/// dirge-du5k + dirge-7bwx end-to-end: a tool call arriving with a
/// truncated arguments string (canonical `max_tokens`-mid-call
/// failure mode) is healed by `apply_truncation_repair` at the
/// loop-level pre-dispatch site (dirge-7bwx hoist matching
/// Reasonix `repair/index.ts:88-109`), the tool executes with the
/// parsed args, AND the per-run `RepairStats` records the
/// TruncationFixed kind. Proves the brace-closer is wired through
/// the actual dispatch pipeline at the post-hoist call site, not
/// just the validator unit.
#[tokio::test]
async fn truncation_repair_end_to_end_through_dispatch() {
    use crate::agent::agent_loop::tool_input_repair::RepairKind;

    let echo = Arc::new(EchoTool::new("echo"));
    let context = build_context(echo.clone());

    // Simulates rig accumulating a streamed arg string that got
    // cut off after the second quote — model hit max_tokens
    // mid-tool-call. Note: this is Value::String, not
    // Value::Object, because the accumulator path in
    // rig_stream::apply_tool_call_delta keeps the running buffer
    // as a string until either a Complete event lands or the
    // stream ends.
    let truncated = r#"{"value": "hello"#;
    let assistant_msg = AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: "tool-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::Value::String(truncated.to_string()),
        }],
        StopReason::ToolUse,
    );
    let mut tool_calls = extract_tool_calls(&assistant_msg);
    assert_eq!(tool_calls.len(), 1);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
    let config = build_config();
    let signal = AbortSignal::new();

    // dirge-7bwx: heal truncated args BEFORE dispatch (in the
    // real loop, run.rs does this between scavenge merge and
    // storm filter). The previous in-validator pre-pass was
    // removed — repair now lives at the loop level only.
    crate::agent::agent_loop::run::apply_truncation_repair(
        &mut tool_calls,
        &config.repair_stats,
        &config.truncation_notes,
    );

    let batch = execute_tool_calls_sequential(
        &context,
        &assistant_msg,
        &tool_calls,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;
    drop(tx);

    // 1. Tool was called — args reached `execute` AS A PARSED
    //    OBJECT, not the raw truncated string.
    let recorded = echo.executed_args.lock().unwrap();
    assert_eq!(
        recorded.len(),
        1,
        "tool must have been invoked exactly once"
    );
    let received = &recorded[0];
    assert!(
        received.is_object(),
        "args must reach execute as an Object, not Value::String; got: {received:?}",
    );
    assert_eq!(
        received["value"], "hello",
        "the closer must have closed the unterminated string preserving its content",
    );
    drop(recorded);

    // 2. Batch is non-error, non-terminating (the call succeeded).
    assert_eq!(batch.messages.len(), 1);
    assert!(
        !batch.messages[0].is_error,
        "truncation-repaired call must dispatch as a normal success: {:?}",
        batch.messages[0],
    );
    assert!(!batch.terminate);

    // 3. Per-run RepairStats records the TruncationFixed counter.
    let snap = config.repair_stats.snapshot();
    assert_eq!(
        snap.truncation_fixed, 1,
        "RepairStats.truncation_fixed must increment by 1; got snapshot {:?}",
        snap,
    );
    // No spurious increments on other kinds.
    assert_eq!(snap.null_stripped, 0);
    assert_eq!(snap.json_string_to_array, 0);
    assert_eq!(snap.invalid, 0);

    // 4. The repair-kind is in the per-call telemetry too.
    //    (Sanity check that the stats snapshot reflects the same
    //    fact as the per-call hot path.)
    assert!(
        RepairKind::ALL.contains(&RepairKind::TruncationFixed),
        "TruncationFixed must appear in RepairKind::ALL for telemetry iteration",
    );

    // Drain the event stream — should look like a normal
    // execution (start/end/message_start/message_end), with no
    // error events injected by the repair pass.
    let mut kinds = Vec::new();
    while let Some(e) = rx.recv().await {
        kinds.push(e.kind().to_string());
    }
    assert_eq!(
        kinds,
        vec![
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
        ],
        "event sequence must match a non-truncated success path",
    );
}

/// dirge-du5k end-to-end (negative case): a truly unrecoverable
/// args string (the closer hard-fallback path) is NOT silently
/// substituted with `{}` — the model sees a real validation
/// error so it can retry with a correctly-shaped call.
///
/// This is the safety property that distinguishes the brace
/// closer from "always succeed by lying": fabricating empty
/// args would let `read_file()` succeed against an empty path
/// and mask the real failure from the model.
#[tokio::test]
async fn truncation_hard_fallback_does_not_fabricate_args() {
    // Custom tool whose schema REQUIRES a `path` field. EchoTool's
    // {"type": "object"} would happily accept an empty {} and let
    // the test pass by accident.
    use crate::agent::agent_loop::tool::LoopTool;
    use std::sync::OnceLock;

    #[derive(Debug)]
    struct StrictPathTool {
        name: String,
        executed: Arc<Mutex<Vec<Value>>>,
    }
    impl LoopTool for StrictPathTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "needs path"
        }
        fn label(&self) -> &str {
            "strict"
        }
        fn parameters(&self) -> &Value {
            static SCHEMA: OnceLock<Value> = OnceLock::new();
            SCHEMA.get_or_init(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                })
            })
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            args: Value,
            _signal: AbortSignal,
            _on_update: LoopToolUpdate,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<LoopToolResult, String>> + Send + 'a>>
        {
            let executed = self.executed.clone();
            Box::pin(async move {
                executed.lock().unwrap().push(args);
                Ok(LoopToolResult {
                    content: vec![serde_json::json!({"type": "text", "text": "ok"})],
                    details: serde_json::Value::Null,
                    terminate: None,
                })
            })
        }
    }

    let tool = Arc::new(StrictPathTool {
        name: "strict".to_string(),
        executed: Arc::new(Mutex::new(Vec::new())),
    });
    let context = build_context(tool.clone());

    // Stack-unrecoverable garbage. Stray closers with no matching
    // opens — closer flips to fallback={}.
    let assistant_msg = AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: "tool-1".to_string(),
            name: "strict".to_string(),
            arguments: serde_json::Value::String("}}}}}".to_string()),
        }],
        StopReason::ToolUse,
    );
    let tool_calls = extract_tool_calls(&assistant_msg);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let config = build_config();
    let signal = AbortSignal::new();

    let batch = execute_tool_calls_sequential(
        &context,
        &assistant_msg,
        &tool_calls,
        &config,
        &signal,
        &tx,
        &InflightSet::new(),
    )
    .await;

    // The strict tool must NOT have been called with a fabricated
    // empty object — the closer's hard fallback must propagate as
    // a tool_input_invalid error, not a silent success.
    let executed = tool.executed.lock().unwrap();
    assert!(
        executed.is_empty(),
        "strict tool must NOT receive fabricated args; got: {:?}",
        *executed,
    );

    // Result is an error batch.
    assert_eq!(batch.messages.len(), 1);
    assert!(
        batch.messages[0].is_error,
        "hard-fallback must dispatch as an error so the model sees the failure",
    );

    // RepairStats records the invalid, NOT a TruncationFixed.
    let snap = config.repair_stats.snapshot();
    assert_eq!(snap.truncation_fixed, 0);
    assert_eq!(snap.invalid, 1);
}

// dirge-tkyn: the tool-result boundary (`content_value_to_block`) scrubs
// credential-shaped substrings so a tool's output can't carry a secret
// into the transcript / LLM context / UI.
#[test]
fn content_value_to_block_redacts_secrets() {
    let v = serde_json::json!({
        "type": "text",
        "text": "OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz0123456789"
    });
    match content_value_to_block(&v) {
        ContentBlock::Text { text } => {
            assert!(
                !text.contains("sk-abcdefghijklmnopqrstuvwxyz0123456789"),
                "secret must be redacted at the result boundary; got {text}"
            );
            assert!(text.contains("[REDACTED]"), "got {text}");
        }
        other => panic!("expected text block, got {other:?}"),
    }
}

#[test]
fn content_value_to_block_passes_plain_text_through() {
    let v = serde_json::json!({"type": "text", "text": "build ok: 12 files"});
    match content_value_to_block(&v) {
        ContentBlock::Text { text } => assert_eq!(text, "build ok: 12 files"),
        other => panic!("expected text block, got {other:?}"),
    }
}

// dirge-tc4r: tool-result backfill for orphaned tool_call_ids.

fn tc(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: serde_json::Value::Null,
    }
}

fn trm(id: &str, name: &str) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        content: vec![],
        details: serde_json::Value::Null,
        is_error: false,
    }
}

/// Partial suppression: 3 calls, 2 answered → exactly the 1 missing id is
/// backfilled with an error result. This is the exact shape that 400s the
/// provider when left unbackfilled.
#[test]
fn backfill_fills_only_the_unanswered_id() {
    let calls = [tc("a", "edit"), tc("b", "read"), tc("c", "bash")];
    let results = [trm("a", "edit"), trm("c", "bash")];
    let back = backfill_missing_tool_results(&calls, &results);
    assert_eq!(back.len(), 1, "exactly one orphan");
    assert_eq!(back[0].tool_call_id, "b");
    assert_eq!(back[0].tool_name, "read");
    assert!(back[0].is_error, "backfill must be an error result");
}

/// All answered → no backfill (the common, healthy path).
#[test]
fn backfill_empty_when_every_call_is_answered() {
    let calls = [tc("a", "x"), tc("b", "y")];
    let results = [trm("a", "x"), trm("b", "y")];
    assert!(backfill_missing_tool_results(&calls, &results).is_empty());
}

/// None answered (whole batch suppressed/interrupted) → all ids backfilled.
#[test]
fn backfill_fills_all_when_none_answered() {
    let calls = [tc("a", "x"), tc("b", "y")];
    let back = backfill_missing_tool_results(&calls, &[]);
    assert_eq!(back.len(), 2);
    let ids: std::collections::HashSet<&str> =
        back.iter().map(|r| r.tool_call_id.as_str()).collect();
    assert!(ids.contains("a") && ids.contains("b"));
    assert!(back.iter().all(|r| r.is_error));
}
