//! Phase 4.5f — compose 4.5a (rig stream) + 4.5b (rig tool) + 4.5c
//! (event bridge) + 4.5d (plugin hooks) + 4.5e (steering) into a
//! single spawn function that returns an `AgentEvent`-emitting
//! runner.
//!
//! `LoopRunner` is the new path's public surface. It's
//! intentionally NOT `AgentRunner` from `runner.rs` because the
//! two paths coexist (per PLAN.md phase 4.5f — gated default
//! comes in 4.5h). The UI side ports happen later; for now this
//! is a parallel runner the rest of the test infrastructure
//! drives.
//!
//! ## Composition diagram
//!
//! ```text
//!                       spawn_loop_runner
//!                              │
//!                              ▼
//!     ┌────────────────────────────────────────────────────┐
//!     │  tokio::spawn:                                     │
//!     │                                                    │
//!     │   build LoopConfig from inputs:                    │
//!     │     • convert_to_llm = passthrough                 │
//!     │     • before_tool_call = plugin_hooks (if pm)      │
//!     │     • after_tool_call = plugin_hooks (if pm)       │
//!     │     • get_steering_messages = steering (if q)      │
//!     │                                                    │
//!     │   build Context { system_prompt, msgs, tools }     │
//!     │                                                    │
//!     │   spawn inner task: run_agent_loop(...)            │
//!     │      └─ emits LoopEvent on internal channel        │
//!     │                                                    │
//!     │   loop:                                            │
//!     │     receive LoopEvent                              │
//!     │     translate via EventBridge → Vec<AgentEvent>    │
//!     │     forward each on caller's event channel         │
//!     │                                                    │
//!     │   when inner task finishes, drain channel + exit   │
//!     └────────────────────────────────────────────────────┘
//! ```
//!
//! ## Phase 4.5f scope
//!
//! - **Does**: compose all sub-phase pieces into one async
//!   pipeline; produce `AgentEvent`s observable by existing UI /
//!   ACP code (via the bridge).
//! - **Does NOT**: wire to a real rig `CompletionModel` (that's
//!   the caller's `stream_fn`; phase 4.5f-2 will add a helper
//!   that builds `stream_fn` from a rig agent + tools). Recovery
//!   / retry on errors (phase 4.5g). Flag-gated dispatch from
//!   `runner.rs` (phase 4.5h).
//!
//! ## AbortSignal
//!
//! The runner exposes its `AbortSignal` so callers can cancel
//! the loop. The existing `AgentRunner.interject_tx` is a
//! different mechanism (graceful stop at tool-result boundary);
//! refining the two into one surface lands in phase 4.5g.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::event::AgentEvent;

use super::bridge::EventBridge;
use super::heal;
use super::message::{LoopMessage, UserMessage};
use super::run::run_agent_loop;
use super::steering::steering_from_queue;
use super::stream::StreamFn;
use super::tool::{AbortSignal, LoopTool};
use super::types::{Context, LoopConfig, QueueMode, ToolExecutionMode};

/// Public handle to a running loop. Mirrors the shape of
/// `runner::AgentRunner` (event channel + task handle + cancel
/// signal) without inheriting from it — both paths coexist.
pub struct LoopRunner {
    /// Channel of `AgentEvent`s. UI / ACP consume from here just
    /// like with the existing `AgentRunner`.
    pub event_rx: mpsc::Receiver<AgentEvent>,
    /// Task driving the loop. Caller can `task.abort()` to force-
    /// kill (alongside or instead of `signal.cancel()`).
    pub task: JoinHandle<()>,
    /// Cooperative cancellation. Tools poll this between steps;
    /// the loop checks it at turn boundaries.
    pub signal: AbortSignal,
}

impl LoopRunner {
    /// Phase 4.5h-6: adapt this LoopRunner to the existing
    /// `runner::AgentRunner` shape so legacy callsites
    /// (`provider::spawn_runner` → UI) work unchanged.
    ///
    /// The `signal` is hidden behind an `interject_tx` channel:
    /// when the UI sends a `()` on the channel, a bridge task
    /// translates it to `signal.cancel()`. From the run's
    /// perspective this is a graceful stop request — the loop
    /// observes the signal at its next turn-boundary check and
    /// surfaces via AgentEvent::Done.
    ///
    /// `interject_tx` capacity 64 matches `runner::spawn_agent`'s
    /// existing choice — UI hammers the interject keybind during
    /// long runs and bounded prevents an unbounded queue.
    pub fn into_agent_runner(self) -> crate::agent::runner::AgentRunner {
        let (interject_tx, mut interject_rx) = mpsc::channel::<()>(64);
        let (cancel_tx, mut cancel_rx) = mpsc::channel::<()>(64);
        let signal_for_interject = self.signal.clone();
        let signal_for_cancel = self.signal.clone();
        // First interject signal → GRACEFUL interjection (LOOP-4).
        // The loop stops at the next turn boundary; in-flight tools
        // complete normally.
        tokio::spawn(async move {
            if interject_rx.recv().await.is_some() {
                signal_for_interject.interject();
                // Drain remaining signals so the UI's bounded
                // channel doesn't backpressure on the second press.
                while interject_rx.try_recv().is_ok() {}
            }
        });
        // First cancel signal → HARD cancellation. The UI pairs
        // this with `JoinHandle::abort()` for a belt-and-suspenders
        // shutdown: abort kills the task, cancel gives the retry
        // loop and rig stream a chance to observe `is_cancelled()`
        // and exit through their clean-error paths first.
        tokio::spawn(async move {
            if cancel_rx.recv().await.is_some() {
                signal_for_cancel.cancel();
                while cancel_rx.try_recv().is_ok() {}
            }
        });
        crate::agent::runner::AgentRunner {
            event_rx: self.event_rx,
            task: self.task,
            interject_tx,
            cancel_tx,
        }
    }
}

/// Phase 4.5h-6: convert a `rig::completion::Message` (the shape
/// `runner::convert_history` produces from a `Session`) to one or
/// more `LoopMessage`s.
///
/// One rig message can map to MULTIPLE loop messages because:
///   - A `Message::User { content: OneOrMany<UserContent> }` with
///     `ToolResult` content blocks is rig's representation of a
///     tool result. In our shape each tool result is its own
///     `LoopMessage::ToolResult`.
///   - A `Message::Assistant` with mixed text + tool_call content
///     stays as one `LoopMessage::Assistant` (the LoopMessage's
///     content vec carries the mixed blocks).
///   - `Message::System` is dropped — system content goes to the
///     `Context.system_prompt`, not the message list.
pub fn rig_message_to_loop_messages(m: rig::completion::Message) -> Vec<LoopMessage> {
    use super::message::{AssistantMessage, ContentBlock, StopReason, ToolResultMessage};
    use rig::completion::message::{AssistantContent, Message, UserContent};
    match m {
        Message::System { .. } => Vec::new(),
        Message::User { content } => {
            // Walk the OneOrMany. Separate text parts (which
            // collectively become one User message) from
            // ToolResult parts (which each become their own
            // ToolResult message).
            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_results: Vec<LoopMessage> = Vec::new();
            for part in content.into_iter() {
                match part {
                    UserContent::Text(t) => text_parts.push(t.text),
                    UserContent::ToolResult(tr) => {
                        // Flatten ToolResultContent into a single
                        // text body. Multi-block tool results are
                        // rare; rig itself flattens these into a
                        // text representation downstream.
                        let body = tr
                            .content
                            .into_iter()
                            .filter_map(|c| match c {
                                rig::completion::message::ToolResultContent::Text(t) => {
                                    Some(t.text)
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        tool_results.push(LoopMessage::ToolResult(ToolResultMessage {
                            tool_call_id: tr.id,
                            tool_name: String::new(), // not recovered from rig
                            content: vec![ContentBlock::Text { text: body }],
                            details: serde_json::Value::Null,
                            is_error: false,
                        }));
                    }
                    // Image/Audio/Video/Document — rare in dirge's
                    // history. Drop with a no-op; chat history is
                    // text-centric.
                    _ => {}
                }
            }
            let mut out = Vec::new();
            if !text_parts.is_empty() {
                out.push(LoopMessage::User(UserMessage {
                    content: text_parts.join("\n"),
                }));
            }
            out.extend(tool_results);
            out
        }
        Message::Assistant { content, .. } => {
            let mut blocks: Vec<ContentBlock> = Vec::new();
            for part in content.into_iter() {
                match part {
                    AssistantContent::Text(t) => blocks.push(ContentBlock::Text { text: t.text }),
                    AssistantContent::ToolCall(tc) => {
                        blocks.push(ContentBlock::ToolCall {
                            id: tc.id,
                            name: tc.function.name,
                            arguments: tc.function.arguments,
                        });
                    }
                    AssistantContent::Reasoning(r) => {
                        // Flatten Reasoning.content into a
                        // single text body (matches the same
                        // strategy as rig_stream.rs).
                        let text = r
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                rig::completion::message::ReasoningContent::Text {
                                    text, ..
                                } => Some(text.clone()),
                                rig::completion::message::ReasoningContent::Summary(s) => {
                                    Some(s.clone())
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        blocks.push(ContentBlock::Thinking { text });
                    }
                    AssistantContent::Image(_) => {}
                }
            }
            if blocks.is_empty() {
                Vec::new()
            } else {
                // Determine stop_reason from content: ToolUse if
                // any tool call present; Stop otherwise.
                let has_tool = blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolCall { .. }));
                let stop_reason = if has_tool {
                    StopReason::ToolUse
                } else {
                    StopReason::Stop
                };
                vec![LoopMessage::Assistant(AssistantMessage {
                    content: blocks,
                    stop_reason,
                    error_message: None,
                })]
            }
        }
    }
}

/// Convenience: convert a vec of rig messages to a flat
/// loop-message history. Calls `rig_message_to_loop_messages`
/// per entry and flattens.
pub fn rig_history_to_loop_messages(history: Vec<rig::completion::Message>) -> Vec<LoopMessage> {
    history
        .into_iter()
        .flat_map(rig_message_to_loop_messages)
        .collect()
}

/// Convenience: extract any system-message content from a rig
/// history, returning the concatenated text. Used by
/// `provider::spawn_runner` to merge `Session`-side system
/// messages (compaction summaries, etc.) into the loop's
/// `Context.system_prompt`.
pub fn rig_history_system_prompt(history: &[rig::completion::Message]) -> String {
    use rig::completion::message::Message;
    history
        .iter()
        .filter_map(|m| match m {
            Message::System { content } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Inputs to `spawn_loop_runner`. Bundled to keep the call sites
/// readable as the number of optional pieces grows.
pub struct LoopSpawnConfig {
    /// Stream function — invoked once per LLM call. Phase 4.5f
    /// tests use mock streams; phase 4.5f-2 builds a real-rig
    /// variant via `wrap_rig_stream`.
    pub stream_fn: StreamFn,

    /// System prompt for every LLM call.
    pub system_prompt: String,

    /// Pre-existing conversation history. The loop appends new
    /// turns; returns the complete `new_messages` Vec when done.
    pub history: Vec<LoopMessage>,

    /// User prompt that starts this run.
    pub initial_prompt: String,

    /// Tool registry. Built via `RigToolAdapter::new(rig_tool)`
    /// for each existing dirge tool, or constructed directly from
    /// a custom `impl LoopTool`.
    pub tools: Vec<Arc<dyn LoopTool>>,

    /// Optional plugin manager. When set, `on-tool-start` and
    /// `on-tool-end` hooks dispatch through `plugin_hooks`.
    #[cfg(feature = "plugin")]
    pub plugin_mgr: Option<Arc<Mutex<crate::plugin::PluginManager>>>,

    /// Optional steering queue. When set, polled at every turn
    /// boundary so user-typed mid-run messages get injected as
    /// new user turns.
    pub steering_queue: Option<Arc<Mutex<VecDeque<String>>>>,

    /// Default tool-execution mode (per-tool overrides win). Pi
    /// defaults to Parallel; existing dirge tools that mutate
    /// shared state (bash, edit, write, apply_patch) should
    /// declare `Sequential` via `RigToolAdapter::with_execution_mode`.
    pub tool_execution: ToolExecutionMode,

    /// Channel capacity for the AgentEvent output. 256 matches
    /// the existing `runner::spawn_agent` choice.
    pub event_channel_capacity: usize,

    /// Provider name forwarded to `LoopConfig.provider_name` so
    /// the `getApiKey` hook receives the canonical provider
    /// identifier. Code review #2 — was missing; hook used to
    /// receive empty string.
    pub provider_name: Option<String>,

    /// Model identifier forwarded to `LoopConfig.model_name` so
    /// the `tool_input_repair` telemetry records `(model, tool,
    /// repair_kind)`. `None` is acceptable — telemetry falls back
    /// to `"unknown"`.
    pub model_name: Option<String>,

    /// LOOP-9 — optional summarizer callback forwarded to
    /// `LoopConfig.summarize_fn` so the run-loop's compaction path
    /// can call the auxiliary model. Production code builds this
    /// from `AnyClient::compress_messages`; tests can mock it.
    pub summarize_fn: Option<crate::agent::compression::SummarizeFn>,

    /// Phase-3: per-session loaded-tool set. When `Some`, the
    /// request builder filters tool defs sent to the model
    /// against this set + the always-on list. Must be the SAME
    /// Arc passed to the `ToolSearchTool` instance in `tools` —
    /// that's how the meta-tool's results surface to the next
    /// turn's request. `None` keeps the legacy "ship every tool
    /// every turn" behavior.
    pub tool_def_filter: Option<Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,

    /// Phase-3: whether dynamic-tool-search is on. Mirrors the
    /// `dynamic_tool_search` config knob. Carried alongside
    /// `tool_def_filter` for introspection.
    pub dynamic_tool_search: bool,

    /// Phase 4 part 1: alternate stream function used for ONE
    /// call after a repair-exhaustion or tree-sitter failure.
    /// `None` when no escalation is configured.
    pub escalation_stream_fn: Option<StreamFn>,

    /// Phase 4 part 1: provider name for the escalation route.
    /// Surfaced in `LoopEvent::EscalationActivated` so the UI can
    /// show the user which provider just took over.
    pub escalation_provider_name: Option<String>,

    /// Phase 4 part 1: per-session escalation cap. `None` uses the
    /// hardcoded default of 3.
    pub escalation_max_per_session: Option<usize>,

    /// Phase 4 part 2: optional file-touch tracker for the
    /// context-depth reminder system. `None` keeps the feature
    /// off (legacy behavior, byte-identical to today).
    pub file_touch_tracker:
        Option<std::sync::Arc<crate::agent::agent_loop::context_depth::FileTouchTracker>>,

    /// dirge-nqr: hard cap on assistant turns within a single run.
    /// `None` = unlimited. Forwarded to `LoopConfig.max_turns`.
    pub max_turns: Option<usize>,

    /// dirge-9tfq: per-session background-task store. When `Some`,
    /// `spawn_loop_runner` installs a `get_followup_messages` hook
    /// that drains the store's pending notifications at every
    /// outer-loop boundary and synthesises a `<system-reminder>`
    /// user message so the parent agent sees the subagent's result
    /// without needing the user to re-prompt. `None` keeps the
    /// legacy behaviour where completion only surfaces when the
    /// user types (via `prepend_pending_notifications` on the next
    /// prompt).
    pub bg_store: Option<crate::agent::tools::background::BackgroundStore>,

    /// dirge-h5tv: memory provider passed through to the auto-compaction
    /// path so `on_pre_compress` can fire when the loop folds messages.
    /// Pre-fix the hook only fired from `handle_compress` (the /compress
    /// slash command), so the silent auto-fold path dropped plugin-provider
    /// insights every time. `None` is a no-op (no provider attached, or a
    /// non-interactive test path).
    pub memory_provider: Option<std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>>,
}

impl LoopSpawnConfig {
    /// Build a minimal config — stream_fn + prompt only; empty
    /// history; no tools; no plugins; no steering; defaults
    /// elsewhere. Useful for tests; production code populates
    /// all fields explicitly.
    pub fn minimal(stream_fn: StreamFn, prompt: impl Into<String>) -> Self {
        Self {
            stream_fn,
            system_prompt: String::new(),
            history: Vec::new(),
            initial_prompt: prompt.into(),
            tools: Vec::new(),
            provider_name: None,
            model_name: None,
            #[cfg(feature = "plugin")]
            plugin_mgr: None,
            steering_queue: None,
            tool_execution: ToolExecutionMode::Parallel,
            event_channel_capacity: 256,
            summarize_fn: None,
            tool_def_filter: None,
            dynamic_tool_search: false,
            escalation_stream_fn: None,
            escalation_provider_name: None,
            escalation_max_per_session: None,
            file_touch_tracker: None,
            max_turns: None,
            bg_store: None,
            memory_provider: None,
        }
    }
}

/// Spawn a runner that composes the agent_loop pipeline.
///
/// Returns immediately with a `LoopRunner`; the loop runs on a
/// spawned tokio task and emits `AgentEvent`s on `event_rx`.
pub fn spawn_loop_runner(cfg: LoopSpawnConfig) -> LoopRunner {
    let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(cfg.event_channel_capacity);
    let signal = AbortSignal::new();
    let signal_for_task = signal.clone();

    // Build the LoopConfig at construction so the closure
    // doesn't have to. Plugin / steering hooks are installed if
    // their producers were supplied. `mut` is only required
    // under feature=plugin (the `before_tool_call` /
    // `after_tool_call` slots get assigned in that block);
    // silence the warning otherwise.
    #[cfg_attr(not(feature = "plugin"), allow(unused_mut))]
    let mut loop_config = LoopConfig {
        convert_to_llm: default_convert_to_llm(),
        transform_context: None,
        compaction_hooks: None,
        get_api_key: None,
        api_key: None,
        tool_execution: cfg.tool_execution,
        before_tool_call: None,
        after_tool_call: None,
        prepare_next_turn: None,
        should_stop_after_turn: None,
        get_steering_messages: cfg
            .steering_queue
            .map(|q| steering_from_queue(q, QueueMode::All)),
        // dirge-9tfq: when a background-task store is provided, install
        // a follow-up hook that surfaces subagent completions at the
        // outer-loop boundary. Without this, the parent agent only sees
        // results when the user re-prompts.
        get_followup_messages: cfg
            .bg_store
            .clone()
            .map(|store| crate::agent::tools::background::followup_from_background_store(store)),
        reasoning: None,
        thinking_budgets: None,
        headers: std::collections::HashMap::new(),
        metadata: std::collections::HashMap::new(),
        request_timeout: None,
        provider_name: cfg.provider_name.clone(),
        model_name: cfg.model_name.clone(),
        compact_model: None,
        storm_mutating_tools: None,
        storm_exempt_tools: None,
        repair_stats: std::sync::Arc::new(
            crate::agent::agent_loop::tool_input_repair::RepairStats::new(),
        ),
        truncation_notes: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        tool_def_filter: cfg.tool_def_filter.clone(),
        dynamic_tool_search: cfg.dynamic_tool_search,
        escalation_stream_fn: cfg.escalation_stream_fn.clone(),
        escalation_provider_name: cfg.escalation_provider_name.clone(),
        escalation_pending: std::sync::Arc::new(std::sync::Mutex::new(None)),
        escalation_max_per_session: cfg.escalation_max_per_session.unwrap_or(3),
        escalation_remaining: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            cfg.escalation_max_per_session.unwrap_or(3),
        )),
        file_touch_tracker: cfg.file_touch_tracker.clone(),
        max_turns: cfg.max_turns,
    };

    #[cfg(feature = "plugin")]
    {
        if let Some(pm) = cfg.plugin_mgr {
            // Phase 4.5d: before/after tool call hooks.
            loop_config.before_tool_call = Some(
                super::plugin_hooks::before_hook_from_plugin_manager(pm.clone()),
            );
            loop_config.after_tool_call = Some(
                super::plugin_hooks::after_hook_from_plugin_manager(pm.clone()),
            );
            // dirge-264x: plugin-driven context transform, dispatched
            // before each LLM call (stream_assistant_response reads
            // config.transform_context). Only install if the host
            // didn't supply one — it doesn't today, so this is the
            // sole consumer of the otherwise-always-None field.
            if loop_config.transform_context.is_none() {
                loop_config.transform_context = Some(
                    super::plugin_hooks::transform_context_from_plugin_manager(pm.clone()),
                );
            }
            // dirge-jia8: plugin compaction hooks (observe-only
            // before-compact + custom-summary on-compact), consumed
            // by run_compaction_pass.
            if loop_config.compaction_hooks.is_none() {
                loop_config.compaction_hooks = Some(
                    super::plugin_hooks::compaction_hooks_from_plugin_manager(pm.clone()),
                );
            }
            // Phase 5: pi-loop hook surface for plugins.
            // Each polls a dedicated Janet slot the plugin sets
            // via harness/* helpers. Hooks fire at the right
            // loop points (prepareNextTurn between turns;
            // shouldStopAfterTurn after every turn;
            // getSteeringMessages per turn boundary;
            // getFollowUpMessages at outer-loop boundary).
            loop_config.prepare_next_turn = Some(
                super::plugin_hooks::prepare_next_turn_from_plugin_manager(pm.clone()),
            );
            loop_config.should_stop_after_turn =
                Some(super::plugin_hooks::should_stop_after_turn_from_plugin_manager(pm.clone()));
            // Compose with caller-provided steering queue: if
            // BOTH are present, prefer the plugin one (plugin
            // hooks compose at runtime; the explicit
            // steering_queue was for legacy / test usage). Real
            // production wires one or the other.
            if loop_config.get_steering_messages.is_none() {
                loop_config.get_steering_messages = Some(
                    super::plugin_hooks::get_steering_messages_from_plugin_manager(pm.clone()),
                );
            }
            // dirge-9tfq: when both plugin AND background-store
            // followups are configured, run both at each boundary and
            // concatenate (background notifications first so the
            // subagent result is observed before any plugin-injected
            // continuation). Without composing, installing the plugin
            // hook would silently shadow subagent completion delivery.
            let plugin_followup =
                super::plugin_hooks::get_followup_messages_from_plugin_manager(pm);
            loop_config.get_followup_messages = match loop_config.get_followup_messages.take() {
                Some(bg_followup) => Some(std::sync::Arc::new(move || {
                    let bg = bg_followup.clone();
                    let pl = plugin_followup.clone();
                    Box::pin(async move {
                        let mut out = bg().await;
                        out.extend(pl().await);
                        out
                    })
                })),
                None => Some(plugin_followup),
            };
        }
    }

    let mut context = Context {
        system_prompt: cfg.system_prompt,
        messages: cfg.history.iter().map(loop_message_to_value).collect(),
        tools: cfg.tools,
    };
    let prompts = vec![LoopMessage::User(UserMessage {
        content: cfg.initial_prompt,
    })];
    let stream_fn = cfg.stream_fn;
    let summarize_fn = cfg.summarize_fn.clone();
    // dirge-h5tv: capture the provider before the move-closure so
    // auto-compaction can fire on_pre_compress mid-loop.
    let memory_provider = cfg.memory_provider.clone();

    let task = tokio::spawn(async move {
        // Inner channel for LoopEvents emitted by run_agent_loop.
        // Capacity matches the outer event channel — assumes each
        // LoopEvent expands to <= a small constant of AgentEvents
        // (typically 1-2 via the bridge).
        let (loop_tx, mut loop_rx) = mpsc::channel(256);
        let event_tx_inner = event_tx.clone();
        let signal_inner = signal_for_task.clone();

        // Heal messages loaded from disk before the first LLM call.
        // Shrinks oversized tool results and drops unpaired tool
        // calls that would otherwise 400 the next API request.
        let heal_result =
            heal::heal_loaded_messages(&context.messages, heal::DEFAULT_MAX_RESULT_CHARS);
        if heal_result.healed_count > 0 {
            tracing::info!(
                target: "dirge::agent_loop",
                healed = %heal_result.healed_count,
                chars_saved = %heal_result.chars_saved,
                "healed {} message(s) after session restore",
                heal_result.healed_count,
            );
            context.messages = heal_result.messages;
        }

        // Code-review bug #4 fix: run the loop AND the
        // translation pump in the SAME outer task via
        // `tokio::join!`. The earlier version spawned the loop
        // as a nested `tokio::spawn`, which meant a
        // `task.abort()` on the outer task would NOT abort the
        // nested one — tools could keep running silently after
        // the user thought they'd cancelled. Putting both
        // branches in one task gives them shared fate: outer
        // abort drops the futures at their next .await,
        // killing both. Tools that poll the AbortSignal still
        // observe the cancellation cooperatively.
        let loop_future = async move {
            let _final_messages = run_agent_loop(
                prompts,
                context,
                loop_config,
                signal_inner,
                &loop_tx,
                &stream_fn,
                summarize_fn,
                memory_provider,
            )
            .await;
            // Drop the sender so the pump observes channel
            // close and exits naturally.
            drop(loop_tx);
        };

        let pump_future = async {
            let mut bridge = EventBridge::new();
            while let Some(loop_evt) = loop_rx.recv().await {
                for agent_evt in bridge.translate(loop_evt) {
                    // If the receiver dropped (UI exited),
                    // stop pumping — loop_future continues
                    // naturally because its emit channel
                    // uses `let _ = .send`.
                    if event_tx_inner.send(agent_evt).await.is_err() {
                        return;
                    }
                }
            }
        };

        tokio::join!(loop_future, pump_future);
    });

    LoopRunner {
        event_rx,
        task,
        signal,
    }
}

/// Convert a `LoopMessage` into the placeholder `Value` shape
/// `Context.messages` carries. Duplicated from `run.rs`'s
/// internal helper because that one is private. Phase 4 plans
/// to swap `Vec<Value>` for a typed message list across the
/// module — when that lands this helper goes away.
fn loop_message_to_value(msg: &LoopMessage) -> Value {
    use super::message::{AssistantMessage, ToolResultMessage};
    fn assistant_to_value(a: &AssistantMessage) -> Value {
        serde_json::json!({
            "role": "assistant",
            "content": a.content,
            "stopReason": a.stop_reason,
            "errorMessage": a.error_message,
        })
    }
    fn tool_result_to_value(t: &ToolResultMessage) -> Value {
        serde_json::json!({
            "role": "toolResult",
            "toolCallId": t.tool_call_id,
            "toolName": t.tool_name,
            "content": t.content,
            "details": t.details,
            "isError": t.is_error,
        })
    }
    match msg {
        LoopMessage::User(u) => serde_json::json!({
            "role": "user",
            "content": u.content,
        }),
        LoopMessage::Assistant(a) => assistant_to_value(a),
        LoopMessage::ToolResult(t) => tool_result_to_value(t),
        LoopMessage::Custom(v) => v.clone(),
    }
}

/// Pass-through `convert_to_llm`. Phase 4.5f-2 will substitute a
/// rig-aware converter that maps our `LoopMessage` enum to rig's
/// `Message` type for the real-LLM path. For tests with mock
/// streams, the stream_fn doesn't actually consume the messages
/// — passthrough is fine.
/// Phase 7: default `convertToLlm` that keeps only LLM-bound
/// messages (user / assistant / toolResult) and drops everything
/// else. Pi's contract is that `convertToLlm` is the "filter to
/// what the model can see" step — custom message variants (UI
/// notifications, artifacts, plugin events) are dropped here so
/// they don't pollute the LLM context.
///
/// Renamed from `passthrough_converter` (phase 4.5f placeholder).
/// The earlier passthrough let LoopMessage::Custom values reach
/// the LLM verbatim, breaking pi parity. Custom messages
/// serialize through `loop_message_to_value` to whatever Value
/// shape the application chose; if they used role="user" they
/// slipped through. The filter here enforces the role-based
/// contract.
pub fn default_convert_to_llm() -> super::types::ConvertToLlmFn {
    Arc::new(|messages: &[Value]| {
        messages
            .iter()
            .filter(|m| {
                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                matches!(
                    role,
                    "user" | "assistant" | "tool" | "toolResult" | "system"
                )
            })
            .cloned()
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::message::{
        AssistantMessage, ContentBlock, StopReason, StreamEvent,
    };
    use crate::agent::agent_loop::result::LoopToolResult;
    use crate::agent::agent_loop::tool::LoopToolUpdate;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Drain the event channel.
    async fn drain(mut rx: mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Some(e) = rx.recv().await {
            out.push(e);
        }
        out
    }

    /// Stream factory returning the supplied messages in order.
    fn canned_factory(responses: Vec<AssistantMessage>) -> StreamFn {
        let counter = Arc::new(AtomicUsize::new(0));
        let responses = Arc::new(responses);
        Arc::new(move |_ctx, _opts| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let msg = responses.get(n).cloned().unwrap_or_else(|| {
                AssistantMessage::new(
                    vec![ContentBlock::Text {
                        text: "fallback".to_string(),
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

    fn text_response(s: &str) -> AssistantMessage {
        AssistantMessage::new(
            vec![ContentBlock::Text {
                text: s.to_string(),
            }],
            StopReason::Stop,
        )
    }

    fn tool_response(id: &str, name: &str, args: Value) -> AssistantMessage {
        AssistantMessage::new(
            vec![ContentBlock::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: args,
            }],
            StopReason::ToolUse,
        )
    }

    /// Mock echo tool used by tool-call tests.
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

    /// Minimal run: text-only canned response → AgentEvents
    /// include TurnStart / TurnEnd / Done in that order. No
    /// Token events because the canned mock provides the whole
    /// message in one Done event (no incremental TextDelta
    /// stream events); the final text lands on `Done.response`.
    /// A real LLM stream would produce TextDelta events that the
    /// bridge translates to Token chunks — exercised in phase
    /// 4.5a's tests against the rig adapter.
    #[tokio::test]
    async fn spawn_emits_expected_event_sequence_for_text_response() {
        let cfg =
            LoopSpawnConfig::minimal(canned_factory(vec![text_response("Hello world")]), "hi");
        let runner = spawn_loop_runner(cfg);
        let events = drain(runner.event_rx).await;
        let kinds: Vec<&str> = events.iter().map(agent_event_kind).collect();
        for required in ["TurnStart", "TurnEnd", "Done"] {
            assert!(kinds.contains(&required), "missing {required} in {kinds:?}");
        }
        // Final response text lands on Done.
        let done = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Done { response, .. } => Some(response.clone()),
                _ => None,
            })
            .expect("Done must be emitted");
        assert_eq!(done, "Hello world");
        let _ = runner.task.await;
    }

    /// Multi-turn run with a tool call: assistant emits toolCall
    /// → loop dispatches → second LLM call emits final text.
    /// AgentEvents include ToolCall + ToolStarted + ToolResult.
    #[tokio::test]
    async fn spawn_handles_tool_call_then_final_text() {
        let mut cfg = LoopSpawnConfig::minimal(
            canned_factory(vec![
                tool_response("call-1", "echo", serde_json::json!({"v": 1})),
                text_response("done"),
            ]),
            "go",
        );
        cfg.tools.push(Arc::new(EchoTool));
        cfg.tool_execution = ToolExecutionMode::Sequential;

        let runner = spawn_loop_runner(cfg);
        let events = drain(runner.event_rx).await;
        let kinds: Vec<&str> = events.iter().map(agent_event_kind).collect();
        for required in [
            "TurnStart",
            "ToolCall",
            "ToolStarted",
            "ToolResult",
            "TurnEnd",
            "Done",
        ] {
            assert!(kinds.contains(&required), "missing {required} in {kinds:?}");
        }
        let _ = runner.task.await;
    }

    /// Steering queue produces a mid-run interjection; the
    /// runner's second LLM call sees it. Verifies the full
    /// 4.5e + 4.5f integration.
    #[tokio::test]
    async fn spawn_with_steering_queue_injects_mid_run() {
        let queue = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let queue_writer = queue.clone();

        // Inspector: did the second LLM call see the interrupt?
        let saw = Arc::new(Mutex::new(false));
        let saw_clone = saw.clone();
        let counter = Arc::new(AtomicUsize::new(0));

        let factory: StreamFn = Arc::new(move |llm_ctx, _opts| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                let found = llm_ctx.messages.iter().any(|m| {
                    m.get("role").and_then(|r| r.as_str()) == Some("user")
                        && m.get("content")
                            .and_then(|c| c.as_str())
                            .map(|s| s.contains("interrupt"))
                            == Some(true)
                });
                *saw_clone.lock().unwrap() = found;
            } else if n == 0 {
                queue_writer
                    .lock()
                    .unwrap()
                    .push_back("interrupt".to_string());
            }
            let msg = if n == 0 {
                tool_response("call-1", "echo", serde_json::json!({}))
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

        let mut cfg = LoopSpawnConfig::minimal(factory, "start");
        cfg.tools.push(Arc::new(EchoTool));
        cfg.tool_execution = ToolExecutionMode::Sequential;
        cfg.steering_queue = Some(queue);

        let runner = spawn_loop_runner(cfg);
        let _events = drain(runner.event_rx).await;
        let _ = runner.task.await;

        assert!(
            *saw.lock().unwrap(),
            "steering should have injected the interrupt for the second LLM call"
        );
    }

    /// Aborting via the runner's signal cancels the loop. The
    /// task still completes (because the loop reaches a natural
    /// stopping point) but tools observing the signal can short-
    /// circuit. This test verifies the runner exposes a working
    /// signal — the actual mid-tool cancellation is exercised by
    /// phase 4.5g's recovery wrapper.
    #[tokio::test]
    async fn spawn_exposes_working_abort_signal() {
        let cfg = LoopSpawnConfig::minimal(canned_factory(vec![text_response("hi")]), "x");
        let runner = spawn_loop_runner(cfg);
        // Just verify the signal is observable / clonable.
        let s = runner.signal.clone();
        s.cancel();
        assert!(runner.signal.is_cancelled());
        let _ = runner.task.await;
    }

    /// Plugin-feature: install a `harness/block`-ing plugin;
    /// verify the tool is blocked and the resulting tool result
    /// surfaces as an error.
    #[cfg(feature = "plugin")]
    #[tokio::test]
    async fn spawn_with_plugin_block_hook_blocks_tool() {
        use crate::plugin::PluginManager;
        let pm = match PluginManager::try_new() {
            Ok(mgr) => Arc::new(Mutex::new(mgr)),
            Err(_) => {
                eprintln!("[skipped] PluginManager::try_new failed");
                return;
            }
        };
        {
            let mut mgr = pm.lock().unwrap();
            mgr.eval(r#"(defn deny [_ctx] (harness/block "policy"))"#)
                .unwrap();
            mgr.register("on-tool-start", "deny");
        }

        let factory = canned_factory(vec![
            tool_response("call-1", "echo", serde_json::json!({})),
            text_response("done"),
        ]);
        let mut cfg = LoopSpawnConfig::minimal(factory, "go");
        cfg.tools.push(Arc::new(EchoTool));
        cfg.tool_execution = ToolExecutionMode::Sequential;
        cfg.plugin_mgr = Some(pm);

        let runner = spawn_loop_runner(cfg);
        let events = drain(runner.event_rx).await;
        let _ = runner.task.await;

        // Tool result should be present and convey the block.
        let found_block_text = events.iter().any(|e| match e {
            AgentEvent::ToolResult { output, .. } => output.contains("policy"),
            _ => false,
        });
        assert!(
            found_block_text,
            "expected ToolResult to convey 'policy' block reason; got {events:?}"
        );
    }

    /// dirge-9tfq integration: while a background subagent is
    /// running, the parent agent finishes its initial turn and the
    /// inner loop drains (no more tool calls, no pending steering).
    /// Without this fix the run would terminate and the user would
    /// have to re-prompt to see the subagent's result.
    ///
    /// With `cfg.bg_store = Some(store)`, the outer-loop boundary
    /// poll picks up the completion notification, re-enters the
    /// inner loop with the result as `pending_messages`, and the
    /// model sees `[task <id>] completed: <result>` in its next
    /// turn. The final transcript contains both the synthetic
    /// follow-up user message AND a subsequent assistant turn that
    /// observed it.
    ///
    /// Stream factory is a state machine across three LLM calls:
    ///   call 0: emit a text-only response (initial work done)
    ///           — between this call and the next outer-poll, push
    ///           a completion into the store from outside.
    ///   call 1: the call AFTER the followup is injected; we
    ///           inspect llm_ctx.messages to assert the synthetic
    ///           reminder is present, then emit a final text.
    ///
    /// The parent loop transitions: turn 0 (text) → inner exits →
    /// outer polls followup → store has 1 notification → inject as
    /// pending → re-enter inner → turn 1 (sees notification) →
    /// exit.
    #[tokio::test]
    async fn parent_idle_during_subagent_run_resumes_on_completion() {
        use crate::agent::tools::background::{BackgroundStore, TaskState};

        let store = BackgroundStore::new();
        store.insert("sub-1".into());

        let saw_reminder = Arc::new(Mutex::new(false));
        let saw_clone = saw_reminder.clone();
        let counter = Arc::new(AtomicUsize::new(0));
        let store_for_factory = store.clone();

        let factory: StreamFn = Arc::new(move |llm_ctx, _opts| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => {
                    // First call: parent finishes its initial work
                    // and pretends to be idle. After we return,
                    // the inner loop exits (text response = no
                    // tool calls). Between this return and the
                    // outer-loop followup poll, the subagent
                    // "completes" — simulate that by notifying
                    // the store right now.
                    store_for_factory.notify(
                        "sub-1",
                        TaskState::Completed("subagent finished work".into()),
                    );
                }
                1 => {
                    // Second call: the followup must have been
                    // injected as a user message before this call.
                    // Inspect llm_ctx.messages for the marker.
                    let found = llm_ctx.messages.iter().any(|m| {
                        m.get("role").and_then(|r| r.as_str()) == Some("user")
                            && m.get("content").and_then(|c| c.as_str()).map(|s| {
                                s.contains("[task sub-1] completed:")
                                    && s.contains("subagent finished work")
                            }) == Some(true)
                    });
                    *saw_clone.lock().unwrap() = found;
                }
                _ => {}
            }
            let msg = if n == 0 {
                text_response("initial work done; awaiting subagent")
            } else {
                text_response("acknowledged subagent result")
            };
            let reason = msg.stop_reason;
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason,
                message: msg,
                usage: None,
            }]))
        });

        let mut cfg = LoopSpawnConfig::minimal(factory, "start work, then wait");
        cfg.bg_store = Some(store.clone());

        let runner = spawn_loop_runner(cfg);
        let _events = drain(runner.event_rx).await;
        let _ = runner.task.await;

        assert!(
            *saw_reminder.lock().unwrap(),
            "second LLM call must see the [task sub-1] completed marker; \
             the parent loop should have re-entered the inner loop with \
             the subagent completion as a pending user message",
        );
        // Pending queue must be drained after the followup fires —
        // otherwise the same notification would re-inject on every
        // outer-boundary poll and spam the model.
        assert!(
            store.drain_notifications().is_empty(),
            "completion must be consumed exactly once",
        );
    }

    /// dirge-9tfq: without a bg_store, the follow-up hook stays
    /// unset and the loop behaves byte-identically to pre-9tfq —
    /// no synthetic user message is injected and the run ends
    /// after the assistant's text-only response. Guards against a
    /// regression where the hook fires on every poll regardless of
    /// configuration.
    #[tokio::test]
    async fn no_bg_store_means_no_followup_injection() {
        let mut cfg = LoopSpawnConfig::minimal(canned_factory(vec![text_response("done")]), "hi");
        cfg.bg_store = None; // explicit for clarity

        let runner = spawn_loop_runner(cfg);
        let events = drain(runner.event_rx).await;
        let _ = runner.task.await;

        // Exactly one TurnEnd — outer loop did NOT re-enter with a
        // phantom follow-up.
        let turn_ends = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
            .count();
        assert_eq!(turn_ends, 1, "expected single turn; got {turn_ends}");
    }

    fn agent_event_kind(e: &AgentEvent) -> &'static str {
        match e {
            AgentEvent::Token(_) => "Token",
            AgentEvent::Reasoning(_) => "Reasoning",
            AgentEvent::ToolCall { .. } => "ToolCall",
            AgentEvent::ToolStarted { .. } => "ToolStarted",
            AgentEvent::ToolResult { .. } => "ToolResult",
            AgentEvent::Error(_) => "Error",
            AgentEvent::ContextOverflow { .. } => "ContextOverflow",
            AgentEvent::Done { .. } => "Done",
            AgentEvent::TurnStart { .. } => "TurnStart",
            AgentEvent::TurnEnd { .. } => "TurnEnd",
            AgentEvent::Interjected { .. } => "Interjected",
            AgentEvent::CustomMessage { .. } => "CustomMessage",
            AgentEvent::UserMessage { .. } => "UserMessage",
            AgentEvent::ContextCompacted { .. } => "ContextCompacted",
            AgentEvent::RetryNotice { .. } => "RetryNotice",
            AgentEvent::SystemNotice { .. } => "SystemNotice",
            AgentEvent::RepairStats { .. } => "RepairStats",
            AgentEvent::EscalationActivated { .. } => "EscalationActivated",
        }
    }

    /// Phase 7: `default_convert_to_llm` filters `role="custom"`
    /// messages out of the LlmContext.messages before the
    /// StreamFn sees them. Custom variants appear in the
    /// transcript (Context.messages) for UI rendering but never
    /// reach the LLM.
    ///
    /// Setup: Pre-load history with a mix of user / assistant /
    /// custom messages. Run the loop; the stream factory
    /// captures what its LlmContext.messages contains. Assert
    /// the custom variants are absent.
    #[tokio::test]
    async fn default_convert_to_llm_filters_custom_messages() {
        use std::sync::Mutex;
        // Custom message intermixed with normal history.
        let history = vec![
            LoopMessage::User(UserMessage {
                content: "first user".to_string(),
            }),
            LoopMessage::Custom(serde_json::json!({
                "role": "custom",
                "content": "UI-only notification",
            })),
            LoopMessage::Assistant(AssistantMessage::new(
                vec![ContentBlock::Text {
                    text: "first answer".to_string(),
                }],
                StopReason::Stop,
            )),
        ];

        let observed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let observed_clone = observed.clone();
        let stream_fn: StreamFn = Arc::new(move |ctx, _opts| {
            *observed_clone.lock().unwrap() = ctx.messages.clone();
            let msg = AssistantMessage::new(
                vec![ContentBlock::Text {
                    text: "done".to_string(),
                }],
                StopReason::Stop,
            );
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: msg,
                usage: None,
            }]))
        });

        let mut cfg = LoopSpawnConfig::minimal(stream_fn, "next turn please");
        cfg.history = history;
        let runner = spawn_loop_runner(cfg);
        let _ = drain(runner.event_rx).await;
        let _ = runner.task.await;

        let seen = observed.lock().unwrap().clone();
        // Stream factory observed messages. Custom should be
        // FILTERED — only user/assistant remain.
        let roles: Vec<String> = seen
            .iter()
            .map(|m| {
                m.get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("?")
                    .to_string()
            })
            .collect();
        assert!(
            !roles.contains(&"custom".to_string()),
            "Custom messages must be filtered before the LLM; got roles: {roles:?}"
        );
        // user + assistant + new user prompt = 3 (custom dropped).
        assert_eq!(
            roles.len(),
            3,
            "expected 3 LLM-visible messages; got {roles:?}"
        );
    }
}
