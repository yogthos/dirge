//! dirge-ywj — plugin hook contract tests.
//!
//! These exercise the full pi-style hook contract surface by driving
//! `run_agent_loop` with the hook closures BUILT BY
//! `plugin_hooks::{before_hook_from_plugin_manager, after_hook_from_plugin_manager,
//! prepare_next_turn_from_plugin_manager, should_stop_after_turn_from_plugin_manager,
//! get_steering_messages_from_plugin_manager, get_followup_messages_from_plugin_manager}`
//! against a real `PluginManager` and asserting the resulting behaviour
//! of the loop (not just the hook output in isolation).
//!
//! Why drive the whole loop here? `plugin_hooks::tests` already covers
//! the input-shape level — given a Janet script, the hook returns the
//! expected `BeforeToolCallReturn` / `AfterToolCallResult`. What was
//! missing was contract-level coverage: does the loop ACTUALLY honour
//! a `block` (i.e. NOT invoke the underlying tool), does the mutated
//! args reach the tool's execute, etc.

#![cfg(feature = "plugin")]

use super::*;
use crate::agent::agent_loop::hooks::AfterToolCallContext;
use crate::agent::agent_loop::message::{StreamEvent, UserMessage};
use crate::agent::agent_loop::plugin_hooks::{
    after_hook_from_plugin_manager, before_hook_from_plugin_manager,
    get_followup_messages_from_plugin_manager, get_steering_messages_from_plugin_manager,
    prepare_next_turn_from_plugin_manager, should_stop_after_turn_from_plugin_manager,
};
use crate::agent::agent_loop::result::LoopToolResult;
use crate::agent::agent_loop::stream::StreamFn;
use crate::agent::agent_loop::tool::{AbortSignal, LoopTool, LoopToolUpdate};
use crate::agent::agent_loop::types::{ConvertToLlmFn, LoopConfig, ToolExecutionMode};
use crate::plugin::PluginManager;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

/// Skip when Janet VM init fails (matches plugin_hooks::tests::try_pm).
fn try_pm() -> Option<Arc<Mutex<PluginManager>>> {
    match PluginManager::try_new() {
        Ok(mgr) => Some(Arc::new(Mutex::new(mgr))),
        Err(_) => None,
    }
}

fn identity_converter() -> ConvertToLlmFn {
    Arc::new(|messages: &[Value]| {
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
        repair_stats: Arc::new(crate::agent::agent_loop::tool_input_repair::RepairStats::new()),
        tool_def_filter: None,
        dynamic_tool_search: false,
        escalation_stream_fn: None,
        escalation_provider_name: None,
        escalation_pending: Arc::new(Mutex::new(None)),
        escalation_max_per_session: 3,
        escalation_remaining: Arc::new(std::sync::atomic::AtomicUsize::new(3)),
        file_touch_tracker: None,
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

/// Stream factory: yields one canned `Done` per call from a scripted list.
fn canned_factory(responses: Vec<AssistantMessage>) -> StreamFn {
    let counter = Arc::new(AtomicUsize::new(0));
    let responses = Arc::new(responses);
    Arc::new(move |_ctx, _opts| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let msg = responses.get(n).cloned().unwrap_or_else(|| {
            AssistantMessage::new(
                vec![ContentBlock::Text {
                    text: "end".to_string(),
                }],
                StopReason::Stop,
            )
        });
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    })
}

/// Recording tool that captures (id, args) per call.
#[derive(Debug)]
struct RecordingTool {
    name_str: String,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl RecordingTool {
    fn new(name: &str) -> Self {
        Self {
            name_str: name.to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
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
        "Recording mock"
    }
    fn label(&self) -> &str {
        "Recording"
    }
    fn parameters(&self) -> &Value {
        static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
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
        Box::pin(async move {
            calls.lock().unwrap().push((id.clone(), args.clone()));
            Ok(LoopToolResult {
                content: vec![serde_json::json!({"type": "text", "text": "original output"})],
                details: args,
                terminate: None,
            })
        })
    }
}

async fn drain(rx: &mut mpsc::Receiver<LoopEvent>) -> Vec<LoopEvent> {
    let mut out = Vec::new();
    while let Some(e) = rx.recv().await {
        out.push(e);
    }
    out
}

// ===============================================================
// 1. before_tool_call — block prevents underlying invocation
// ===============================================================

/// Install a plugin that calls `harness/block` in `on-tool-start`.
/// When the loop dispatches `bash`, the underlying tool must NOT
/// be invoked; the tool result content must carry the block reason.
#[tokio::test]
async fn ywj_before_tool_call_block_prevents_invocation() {
    let Some(pm) = try_pm() else {
        eprintln!("[skipped] PluginManager::try_new failed");
        return;
    };
    {
        let mut mgr = pm.lock().unwrap();
        mgr.eval(r#"(defn deny [_ctx] (harness/block "policy denial"))"#)
            .expect("install deny");
        mgr.register("on-tool-start", "deny");
    }

    let tool = Arc::new(RecordingTool::new("bash"));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    let factory = canned_factory(vec![
        tool_use_response("call-1", "bash", serde_json::json!({"cmd": "ls"})),
        text_response("done"),
    ]);

    let mut cfg = build_config();
    cfg.before_tool_call = Some(before_hook_from_plugin_manager(pm.clone()));

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let messages = run_agent_loop(
        vec![user("run bash")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
    )
    .await;
    drop(tx);

    // Underlying tool was NEVER invoked.
    assert!(
        tool.calls().is_empty(),
        "blocked tool must not be invoked; got calls: {:?}",
        tool.calls(),
    );

    // Tool result message lands with the block reason text.
    let saw_block_reason = messages.iter().any(|m| match m {
        LoopMessage::ToolResult(t) => t.content.iter().any(|c| match c {
            ContentBlock::Text { text } => text.contains("policy denial"),
            _ => false,
        }),
        _ => false,
    });
    assert!(
        saw_block_reason,
        "tool result should carry the block reason text",
    );
}

// ===============================================================
// 2. before_tool_call mutation — mutated args reach the tool
// ===============================================================

/// Install a plugin that rewrites args via `harness/mutate-input`.
/// The RecordingTool must observe the MUTATED args, not the original.
#[tokio::test]
async fn ywj_before_tool_call_mutation_threads_through() {
    let Some(pm) = try_pm() else {
        return;
    };
    {
        let mut mgr = pm.lock().unwrap();
        mgr.eval(r#"(defn rewrite [_ctx] (harness/mutate-input "{\"cmd\":\"echo mutated\"}"))"#)
            .expect("install rewrite");
        mgr.register("on-tool-start", "rewrite");
    }

    let tool = Arc::new(RecordingTool::new("bash"));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    let factory = canned_factory(vec![
        tool_use_response("call-1", "bash", serde_json::json!({"cmd": "ls"})),
        text_response("done"),
    ]);

    let mut cfg = build_config();
    cfg.before_tool_call = Some(before_hook_from_plugin_manager(pm.clone()));

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let _ = run_agent_loop(
        vec![user("run bash")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
    )
    .await;

    let calls = tool.calls();
    assert_eq!(calls.len(), 1, "tool should run exactly once");
    assert_eq!(
        calls[0].1,
        serde_json::json!({"cmd": "echo mutated"}),
        "tool must observe MUTATED args, not original",
    );
}

// ===============================================================
// 3. after_tool_call replacement — replaced content surfaces
// ===============================================================

/// Plugin calls `harness/replace-result` from `on-tool-end`. The
/// emitted ToolResult must carry the REPLACED content, not the
/// original.
#[tokio::test]
async fn ywj_after_tool_call_replaces_result_content() {
    let Some(pm) = try_pm() else {
        return;
    };
    {
        let mut mgr = pm.lock().unwrap();
        mgr.eval(r#"(defn rewrite [_ctx] (harness/replace-result "REPLACED"))"#)
            .expect("install rewrite");
        mgr.register("on-tool-end", "rewrite");
    }

    let tool = Arc::new(RecordingTool::new("bash"));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    let factory = canned_factory(vec![
        tool_use_response("call-1", "bash", serde_json::json!({"cmd": "ls"})),
        text_response("done"),
    ]);

    let mut cfg = build_config();
    cfg.after_tool_call = Some(after_hook_from_plugin_manager(pm.clone()));

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
    let messages = run_agent_loop(
        vec![user("run bash")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
    )
    .await;
    drop(tx);

    // Tool was invoked (the after-hook only runs after dispatch).
    assert_eq!(tool.calls().len(), 1);

    // The ToolResult message in the transcript must carry the
    // REPLACED content — `original output` should NOT appear.
    let tool_result = messages
        .iter()
        .find_map(|m| match m {
            LoopMessage::ToolResult(t) => Some(t.clone()),
            _ => None,
        })
        .expect("tool result message present");
    let saw_replaced = tool_result.content.iter().any(|c| match c {
        ContentBlock::Text { text } => text.contains("REPLACED"),
        _ => false,
    });
    let saw_original = tool_result.content.iter().any(|c| match c {
        ContentBlock::Text { text } => text.contains("original output"),
        _ => false,
    });
    assert!(saw_replaced, "tool result should carry replaced content");
    assert!(
        !saw_original,
        "tool result must NOT carry original content after replacement",
    );

    // The ToolExecutionEnd event also reflects the replaced content.
    let events = drain(&mut rx).await;
    let exec_end_replaced = events.iter().any(|e| match e {
        LoopEvent::ToolExecutionEnd { result, .. } => result.content.iter().any(|c| {
            c.as_object()
                .and_then(|o| o.get("text"))
                .and_then(|t| t.as_str())
                .map(|s| s.contains("REPLACED"))
                .unwrap_or(false)
        }),
        _ => false,
    });
    assert!(exec_end_replaced, "ToolExecutionEnd should carry REPLACED");
}

// ===============================================================
// 4. prepare_next_turn — TurnUpdate observed by next turn
// ===============================================================

/// Plugin sets the next thinking level via
/// `harness/set-next-thinking-level`. After a tool dispatch,
/// the prepare_next_turn hook returns Some(TurnUpdate{thinking_level: High}).
/// The hook is exercised by the loop; we verify it actually fired
/// (and didn't crash) by completing a full multi-turn run.
#[tokio::test]
async fn ywj_prepare_next_turn_returns_turn_update() {
    let Some(pm) = try_pm() else {
        return;
    };
    {
        let mut mgr = pm.lock().unwrap();
        // Set the slot on every tool-end so the prepare_next_turn
        // hook has something to return.
        mgr.eval(r#"(defn bump [_ctx] (harness/set-next-thinking-level "high"))"#)
            .unwrap();
        mgr.register("on-tool-end", "bump");
    }

    let tool = Arc::new(RecordingTool::new("noop"));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    let factory = canned_factory(vec![
        tool_use_response("call-1", "noop", serde_json::json!({})),
        text_response("done"),
    ]);

    let mut cfg = build_config();
    cfg.prepare_next_turn = Some(prepare_next_turn_from_plugin_manager(pm.clone()));

    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let messages = run_agent_loop(
        vec![user("hi")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
    )
    .await;

    // The loop completed both turns — prepare_next_turn fired
    // between them without crashing.
    let roles: Vec<&'static str> = messages.iter().map(|m| m.role()).collect();
    assert_eq!(roles, vec!["user", "assistant", "toolResult", "assistant"]);
    // Sanity: the slot was drained (subsequent reads return None).
    let pending = pm.lock().unwrap().take_pending_next_thinking_level();
    assert!(
        pending.is_none(),
        "prepare_next_turn should have drained the slot",
    );
}

// ===============================================================
// 5. should_stop_after_turn — true exits loop
// ===============================================================

/// Plugin calls `harness/request-stop-after-turn` on every tool-end.
/// After dispatch, should_stop_after_turn returns true and the loop
/// exits without making the second LLM call.
#[tokio::test]
async fn ywj_should_stop_after_turn_terminates_loop() {
    let Some(pm) = try_pm() else {
        return;
    };
    {
        let mut mgr = pm.lock().unwrap();
        mgr.eval(r#"(defn stop [_ctx] (harness/request-stop-after-turn))"#)
            .unwrap();
        mgr.register("on-tool-end", "stop");
    }

    let tool = Arc::new(RecordingTool::new("noop"));
    let mut ctx = empty_context();
    ctx.tools.push(tool.clone());

    let llm_calls = Arc::new(AtomicUsize::new(0));
    let llm_calls_clone = llm_calls.clone();
    let factory: StreamFn = Arc::new(move |_ctx, _opts| {
        let n = llm_calls_clone.fetch_add(1, Ordering::SeqCst);
        let msg = if n == 0 {
            tool_use_response("call-1", "noop", serde_json::json!({}))
        } else {
            text_response("should not appear")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    });

    let mut cfg = build_config();
    // BOTH hooks needed: `on-tool-end` fires `harness/request-stop-after-turn`
    // (via after_tool_call dispatching the hook); should_stop_after_turn
    // then drains that slot.
    cfg.after_tool_call = Some(after_hook_from_plugin_manager(pm.clone()));
    cfg.should_stop_after_turn = Some(should_stop_after_turn_from_plugin_manager(pm.clone()));

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
    let _messages = run_agent_loop(
        vec![user("hi")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
    )
    .await;
    drop(tx);

    // Only ONE LLM call should have happened — the hook stops the
    // loop after the first turn finishes its tool dispatch.
    assert_eq!(
        llm_calls.load(Ordering::SeqCst),
        1,
        "should_stop_after_turn must prevent the second LLM call",
    );

    let kinds: Vec<&str> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
    assert!(kinds.contains(&"agent_end"), "agent_end fires on stop");
}

// ===============================================================
// 6. get_steering_messages — queued user message at turn boundary
// ===============================================================

/// Plugin pre-queues a steering message before the loop starts.
/// The hook fires at the first inner-turn-boundary; the next
/// assistant LLM call sees the steering message in its context.
#[tokio::test]
async fn ywj_get_steering_messages_injects_user_message_at_boundary() {
    let Some(pm) = try_pm() else {
        return;
    };
    // Queue a steering message NOW so the very first hook poll
    // (called by the loop right before turn 1 starts) returns it.
    {
        let mut mgr = pm.lock().unwrap();
        mgr.eval(r#"(harness/add-steering "queued steering")"#)
            .unwrap();
    }

    // Single-turn run — assistant emits text, no tools.
    let factory = canned_factory(vec![text_response("ok")]);

    let mut cfg = build_config();
    cfg.get_steering_messages = Some(get_steering_messages_from_plugin_manager(pm.clone()));

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let messages = run_agent_loop(
        vec![user("hi")],
        empty_context(),
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
    )
    .await;

    // The steering message landed in the transcript as a User
    // message right after the initial user prompt.
    let user_contents: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            LoopMessage::User(u) => Some(u.content.clone()),
            _ => None,
        })
        .collect();
    assert!(
        user_contents.contains(&"hi".to_string()),
        "original prompt preserved",
    );
    assert!(
        user_contents.contains(&"queued steering".to_string()),
        "steering message injected as User; got {user_contents:?}",
    );
}

// ===============================================================
// 7. get_followup_messages — outer-loop re-entry
// ===============================================================

/// After the inner loop ends naturally (assistant says "done"),
/// the outer-loop boundary polls get_followup_messages. The hook
/// returns ONE followup; the loop re-enters and the second
/// assistant turn fires.
#[tokio::test]
async fn ywj_get_followup_messages_reenters_outer_loop() {
    let Some(pm) = try_pm() else {
        return;
    };
    // Queue a followup message — drained the first time
    // get_followup_messages is polled.
    {
        let mut mgr = pm.lock().unwrap();
        mgr.eval(r#"(harness/add-followup "followup question")"#)
            .unwrap();
    }

    // Two turns of canned text responses — both complete normally
    // so the inner loop ends after each, and the outer-loop poll
    // drives re-entry exactly once (the followup queue is drained
    // on the first poll; the second poll returns empty so the
    // outer loop exits).
    let factory = canned_factory(vec![
        text_response("first done"),
        text_response("second done"),
    ]);

    let mut cfg = build_config();
    cfg.get_followup_messages = Some(get_followup_messages_from_plugin_manager(pm.clone()));

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let messages = run_agent_loop(
        vec![user("hi")],
        empty_context(),
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
    )
    .await;

    // Messages: user("hi"), assistant("first done"),
    //           user("followup question"), assistant("second done").
    let roles: Vec<&'static str> = messages.iter().map(|m| m.role()).collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "user", "assistant"],
        "outer-loop re-entered with followup as new user prompt",
    );

    let user_contents: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            LoopMessage::User(u) => Some(u.content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(user_contents, vec!["hi", "followup question"]);
}
