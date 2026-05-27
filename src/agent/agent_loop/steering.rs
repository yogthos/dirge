//! Phase 4.5e — adapt dirge's existing interjection_queue (a
//! `VecDeque<String>` filled by the UI when the user types during
//! a run) to the pi-style `GetSteeringMessagesFn` hook the loop
//! polls between turns.
//!
//! Existing dirge wiring: `ui/mod.rs` pushes each user-typed line
//! onto `interjection_queue`; when the run completes (`!is_running
//! && !interjection_queue.is_empty()`), the queue is drained,
//! concatenated with `\n\n`, and SPAWNED AS A NEW RUN. Pi's
//! semantics are richer: messages get injected MID-RUN at turn
//! boundaries, becoming user turns in the same run rather than
//! starting a fresh one. Phase 4.5e adapts the queue to pi's
//! semantics without changing the UI-side push pattern.
//!
//! Two consumption modes (`QueueMode` from phase 0):
//!   - `All`: drain the entire queue per poll. Multiple queued
//!     messages all inject before the next assistant turn.
//!   - `OneAtATime`: drain only the oldest per poll. Subsequent
//!     polls (at each turn boundary) drain the next one. Useful
//!     when the user typed several lines that should each be
//!     observed/processed by the model separately.
//!
//! Mutex pattern matches `plugin_hooks` — sync lock, drain
//! synchronously, release. No `.await` while held.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::hooks::GetSteeringMessagesFn;
use super::message::{LoopMessage, UserMessage};
use super::types::QueueMode;

/// Port of Reasonix `MID_TURN_STEER_WRAPPER` (loop.ts:54-55).
/// Prepended to every steering message so the model doesn't
/// treat it as a new task and abandon the current one.
pub const MID_TURN_STEER_WRAPPER: &str = "[Mid-turn steer queued by the user. Do not treat this as a new task; use it only as additional guidance for the current task after completing the current step.]";

/// Wrap steering content with the mid-turn steering preamble.
/// Port of Reasonix `formatSteerUserMessage` (loop.ts:57-59).
pub fn format_steer_user_message(content: &str) -> String {
    [MID_TURN_STEER_WRAPPER, content].join("\n")
}

/// Build a `GetSteeringMessagesFn` that drains from a shared
/// `Arc<Mutex<VecDeque<String>>>` according to `mode`.
///
/// Each drained string becomes a `LoopMessage::User`. The loop
/// injects them BEFORE the next assistant turn (per pi's
/// `getSteeringMessages` contract at agent-loop.ts:181-189).
///
/// Empty queue → empty `Vec` (no injection).
pub fn steering_from_queue(
    queue: Arc<Mutex<VecDeque<String>>>,
    mode: QueueMode,
) -> GetSteeringMessagesFn {
    Arc::new(move || {
        let queue = queue.clone();
        Box::pin(async move {
            // Lock, drain per mode, release.
            let drained: Vec<String> = {
                let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                match mode {
                    QueueMode::All => q.drain(..).collect(),
                    QueueMode::OneAtATime => q.pop_front().into_iter().collect(),
                }
            };
            drained
                .into_iter()
                .map(|content| {
                    LoopMessage::User(UserMessage {
                        // Port of Reasonix loop.ts:740-743 —
                        // wrap every steering message with the
                        // mid-turn preamble so the model doesn't
                        // abandon the current task.
                        content: format_steer_user_message(&content),
                    })
                })
                .collect()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty queue → empty Vec. No allocation of a phantom user
    /// message.
    #[tokio::test]
    async fn empty_queue_returns_empty() {
        let queue = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let hook = steering_from_queue(queue, QueueMode::All);
        let messages = hook().await;
        assert!(messages.is_empty());
    }

    /// `QueueMode::All` drains every queued string in FIFO
    /// order and wraps each as `LoopMessage::User` with the
    /// mid-turn steer wrapper prepended (Reasonix loop.ts:54-58).
    #[tokio::test]
    async fn all_mode_drains_fifo() {
        let queue = Arc::new(Mutex::new(VecDeque::<String>::from(vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ])));
        let hook = steering_from_queue(queue.clone(), QueueMode::All);
        let messages = hook().await;
        assert_eq!(messages.len(), 3);
        let contents: Vec<_> = messages
            .iter()
            .map(|m| match m {
                LoopMessage::User(u) => u.content.clone(),
                _ => panic!("expected User"),
            })
            .collect();
        // Each message is wrapped with the steer wrapper preamble.
        assert!(contents[0].starts_with(MID_TURN_STEER_WRAPPER));
        assert!(contents[0].ends_with("first"));
        assert!(contents[1].ends_with("second"));
        assert!(contents[2].ends_with("third"));
        // Queue is empty after drain.
        assert!(queue.lock().unwrap().is_empty());
    }

    /// `QueueMode::OneAtATime` drains only the oldest per poll;
    /// subsequent polls drain the next. Each wrapped with the
    /// mid-turn steer wrapper.
    #[tokio::test]
    async fn one_at_a_time_drains_oldest_only() {
        let queue = Arc::new(Mutex::new(VecDeque::<String>::from(vec![
            "first".to_string(),
            "second".to_string(),
        ])));
        let hook = steering_from_queue(queue.clone(), QueueMode::OneAtATime);

        let m1 = hook().await;
        assert_eq!(m1.len(), 1);
        assert!(matches!(
            &m1[0],
            LoopMessage::User(u) if u.content.starts_with(MID_TURN_STEER_WRAPPER) && u.content.ends_with("first")
        ));

        // One left.
        assert_eq!(queue.lock().unwrap().len(), 1);

        let m2 = hook().await;
        assert_eq!(m2.len(), 1);
        assert!(matches!(
            &m2[0],
            LoopMessage::User(u) if u.content.contains("second")
        ));

        // Empty now.
        let m3 = hook().await;
        assert!(m3.is_empty());
    }

    /// Concurrent enqueue from a producer task (simulating the UI
    /// pushing while the loop polls) is visible on the next poll.
    /// The mutex guarantees memory visibility; nothing is
    /// lost.
    #[tokio::test]
    async fn producer_enqueue_visible_on_next_poll() {
        let queue = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let hook = steering_from_queue(queue.clone(), QueueMode::All);

        // First poll: nothing yet.
        assert!(hook().await.is_empty());

        // Producer pushes from another task.
        let pushed = queue.clone();
        tokio::spawn(async move {
            pushed.lock().unwrap().push_back("mid-run".to_string());
        })
        .await
        .unwrap();

        // Second poll: sees the new message, wrapped with steer preamble.
        let messages = hook().await;
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0],
            LoopMessage::User(u) if u.content.starts_with(MID_TURN_STEER_WRAPPER) && u.content.ends_with("mid-run")
        ));
    }

    /// Same queue can be polled concurrently — Mutex serializes
    /// access. Use `All` mode so each poll either gets the full
    /// queue or empty (no torn drain).
    #[tokio::test]
    async fn concurrent_polls_serialize() {
        let queue = Arc::new(Mutex::new(VecDeque::<String>::from(vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
        ])));
        let hook = steering_from_queue(queue.clone(), QueueMode::All);
        // Race two polls.
        let h1 = hook.clone();
        let h2 = hook.clone();
        let (r1, r2) = tokio::join!(h1(), h2());
        // One got all 3, the other got 0. No interleaving.
        let lens = [r1.len(), r2.len()];
        let mut sorted = lens;
        sorted.sort();
        assert_eq!(sorted, [0, 3]);
    }

    /// Confirms `Send + Sync` bounds on the produced
    /// `GetSteeringMessagesFn` so it can ship through async
    /// boundaries (e.g. spawned task closures). Compile-time
    /// check; if it builds, it passes.
    #[tokio::test]
    async fn fn_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        let queue = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let hook = steering_from_queue(queue, QueueMode::All);
        assert_send_sync(&hook);
    }

    /// End-to-end integration: wire the steering hook into a real
    /// `run_agent_loop` and verify the queued message is injected
    /// at the right turn boundary. Mirrors pi's test 547 ("should
    /// inject queued messages after all tool calls complete")
    /// but uses our queue-based steering source rather than pi's
    /// inline async closure.
    ///
    /// Setup:
    ///   - first LLM call returns a tool_use response (echo)
    ///   - between turns, the steering queue produces "interrupt"
    ///   - second LLM call returns final text
    ///
    /// Assertions:
    ///   - The interrupt was injected before the second LLM call
    ///   - The new_messages return value includes the interrupt
    #[tokio::test]
    async fn integration_steering_queue_injects_between_turns() {
        use crate::agent::agent_loop::message::{
            AssistantMessage, ContentBlock, StopReason, StreamEvent,
        };
        use crate::agent::agent_loop::result::LoopToolResult;
        use crate::agent::agent_loop::run::run_agent_loop;
        use crate::agent::agent_loop::stream::StreamFn;
        use crate::agent::agent_loop::tool::{AbortSignal, LoopTool, LoopToolUpdate};
        use crate::agent::agent_loop::tools::extract_tool_calls;
        use crate::agent::agent_loop::types::{Context, LoopConfig, ToolExecutionMode};
        use serde_json::Value;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Mock echo tool.
        #[derive(Debug)]
        struct EchoTool;
        impl LoopTool for EchoTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                "Echo"
            }
            fn label(&self) -> &str {
                "Echo"
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
            ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>>
            {
                Box::pin(async move {
                    Ok(LoopToolResult {
                        content: vec![serde_json::json!({"type": "text", "text": "ok"})],
                        details: Value::Null,
                        terminate: None,
                    })
                })
            }
        }

        // Steering queue: starts empty; producer pushes after the
        // tool call has run.
        let queue = Arc::new(Mutex::new(VecDeque::<String>::new()));

        // Inspector: capture what the SECOND LLM call sees.
        let saw_interrupt = Arc::new(Mutex::new(false));
        let saw_clone = saw_interrupt.clone();
        let call_counter = Arc::new(AtomicUsize::new(0));
        let queue_writer = queue.clone();

        let factory: StreamFn = Arc::new(move |llm_ctx, _opts| {
            let n = call_counter.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                // Second call: inspect ctx for the interrupt.
                let found = llm_ctx.messages.iter().any(|m| {
                    m.get("role").and_then(|r| r.as_str()) == Some("user")
                        && m.get("content")
                            .and_then(|c| c.as_str())
                            .map(|s| s.contains("interrupt"))
                            == Some(true)
                });
                *saw_clone.lock().unwrap() = found;
            } else if n == 0 {
                // After the first call returns a tool_use, the
                // loop dispatches the tool and then polls
                // steering. Push the interrupt now so the next
                // poll picks it up.
                queue_writer
                    .lock()
                    .unwrap()
                    .push_back("interrupt".to_string());
            }
            let msg = if n == 0 {
                AssistantMessage::new(
                    vec![ContentBlock::ToolCall {
                        id: "call-1".to_string(),
                        name: "echo".to_string(),
                        arguments: serde_json::json!({}),
                    }],
                    StopReason::ToolUse,
                )
            } else {
                AssistantMessage::new(
                    vec![ContentBlock::Text {
                        text: "done".to_string(),
                    }],
                    StopReason::Stop,
                )
            };
            let reason = msg.stop_reason;
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason,
                message: msg,
                usage: None,
            }]))
        });

        let mut config = LoopConfig {
            convert_to_llm: Arc::new(|messages: &[Value]| {
                messages
                    .iter()
                    .filter(|m| {
                        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                        matches!(role, "user" | "assistant" | "tool" | "toolResult")
                    })
                    .cloned()
                    .collect()
            }),
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
        };
        config.get_steering_messages = Some(steering_from_queue(queue.clone(), QueueMode::All));

        let mut ctx = Context::default();
        ctx.tools.push(Arc::new(EchoTool));

        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let messages = run_agent_loop(
            vec![LoopMessage::User(UserMessage {
                content: "start".to_string(),
            })],
            ctx,
            config,
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;

        assert!(
            *saw_interrupt.lock().unwrap(),
            "second LLM call should see the injected interrupt"
        );

        // Check messages: user "start" + assistant tool_use +
        // tool result + user "interrupt" (wrapped with steer preamble) + assistant "done".
        let user_contents: Vec<String> = messages
            .iter()
            .filter_map(|m| match m {
                LoopMessage::User(u) => Some(u.content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(user_contents[0], "start");
        assert!(
            user_contents[1].starts_with(MID_TURN_STEER_WRAPPER),
            "steering message should be wrapped with preamble"
        );
        assert!(user_contents[1].ends_with("interrupt"));
    }
}
