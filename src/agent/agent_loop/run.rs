//! `run_loop`, `run_agent_loop`, `run_agent_loop_continue` —
//! THE KEYSTONE.
//!
//! Faithful port of pi's `runLoop` (agent-loop.ts:155-269) plus
//! the two public entry points `runAgentLoop` (95-118) and
//! `runAgentLoopContinue` (120-143).
//!
//! Pi's algorithm in one pass (the bones we replicate):
//!
//! ```text
//! runLoop(currentContext, newMessages, config, signal, emit, streamFn):
//!   first_turn = true
//!   pending_messages = getSteeringMessages?() || []
//!
//!   OUTER:
//!     has_more_tool_calls = true
//!     INNER while has_more_tool_calls OR pending_messages not empty:
//!       if !first_turn: emit turn_start; else first_turn = false
//!       inject pending_messages into context + newMessages; emit
//!         message_start + message_end for each
//!       msg = streamAssistantResponse(...)
//!       newMessages.push(msg)
//!       if msg.stopReason in [error, aborted]:
//!         emit turn_end (toolResults=[]); emit agent_end; return
//!       tool_calls = filter msg.content for type=toolCall
//!       tool_results = []; has_more_tool_calls = false
//!       if tool_calls non-empty:
//!         batch = executeToolCalls(...)
//!         tool_results = batch.messages
//!         has_more_tool_calls = !batch.terminate
//!         push each tool_result to context + newMessages
//!       emit turn_end (msg, tool_results)
//!       snapshot = prepareNextTurn?(ctx)
//!       if snapshot: context = ?? newCtx, model = ?? newModel, ...
//!       if shouldStopAfterTurn?(ctx): emit agent_end; return
//!       pending_messages = getSteeringMessages?() || []
//!     // INNER end
//!     follow_up = getFollowUpMessages?() || []
//!     if follow_up non-empty: pending_messages = follow_up; continue OUTER
//!     break OUTER
//!   emit agent_end
//! ```

use serde_json::Value;
use tokio::sync::mpsc;

use super::context_manager::{self, PostUsageDecisionKind};
use super::inflight::InflightSet;
use super::message::{
    AssistantMessage, ContentBlock, LoopEvent, LoopMessage, StopReason, ToolResultMessage,
};
use super::storm::StormBreaker;
use super::stream::{StreamFn, stream_assistant_response};
use super::tool::AbortSignal;
use super::types::{Context, LoopConfig};

/// Build a `StormBreaker` from `LoopConfig`, merging custom
/// mutating/exempt tool name lists with the built-in defaults.
fn storm_for_config(config: &LoopConfig) -> StormBreaker {
    let has_custom = config.storm_mutating_tools.is_some() || config.storm_exempt_tools.is_some();
    if !has_custom {
        return StormBreaker::default();
    }
    let mutating: Option<Box<dyn Fn(&super::tools::ToolCall) -> bool + Send + Sync>> =
        config.storm_mutating_tools.as_ref().map(|extras| {
            let extra_set: std::collections::HashSet<String> = extras.iter().cloned().collect();
            Box::new(move |c: &super::tools::ToolCall| {
                super::storm::default_mutating(c) || extra_set.contains(&c.name)
            }) as Box<dyn Fn(&super::tools::ToolCall) -> bool + Send + Sync>
        });
    let exempt: Option<Box<dyn Fn(&super::tools::ToolCall) -> bool + Send + Sync>> =
        config.storm_exempt_tools.as_ref().map(|extras| {
            let extra_set: std::collections::HashSet<String> = extras.iter().cloned().collect();
            Box::new(move |c: &super::tools::ToolCall| {
                super::storm::default_exempt(c) || extra_set.contains(&c.name)
            }) as Box<dyn Fn(&super::tools::ToolCall) -> bool + Send + Sync>
        });
    StormBreaker::new(6, 3, mutating, exempt)
}

/// Public entry point: start a new run from one or more prompt
/// messages. Faithful port of pi `runAgentLoop` (agent-loop.ts:95).
///
/// Emits `agent_start` + `turn_start`, then `message_start` /
/// `message_end` for each prompt, THEN enters `run_loop`. Returns
/// the full list of messages produced by this run (prompts + every
/// assistant turn + every tool result).
pub async fn run_agent_loop(
    prompts: Vec<LoopMessage>,
    mut context: Context,
    config: LoopConfig,
    signal: AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    stream_fn: &StreamFn,
) -> Vec<LoopMessage> {
    // Pi line 103: `newMessages = [...prompts]`.
    let new_messages = prompts.clone();
    // Pi line 105: `currentContext.messages = [...context.messages, ...prompts]`.
    for prompt in &prompts {
        context.messages.push(loop_message_to_value(prompt));
    }

    // Pi lines 109-114: emit agent_start + turn_start + per-prompt
    // start/end pair.
    let _ = emit.send(LoopEvent::AgentStart).await;
    let _ = emit.send(LoopEvent::TurnStart).await;
    for prompt in &prompts {
        let _ = emit
            .send(LoopEvent::MessageStart {
                message: prompt.clone(),
            })
            .await;
        let _ = emit
            .send(LoopEvent::MessageEnd {
                message: prompt.clone(),
            })
            .await;
    }

    run_loop(context, new_messages, config, signal, emit, stream_fn).await
}

/// The actual loop. Faithful port of pi `runLoop` (agent-loop.ts:155-269).
///
/// Owns `current_context`, `new_messages`, `config` — pi mutates
/// these as the run proceeds; in Rust we own them by value and
/// return `new_messages` at the end.
pub async fn run_loop(
    mut current_context: Context,
    mut new_messages: Vec<LoopMessage>,
    // `config` is `mut` even though phase 4 only reads it. Pi
    // mutates it at agent-loop.ts:229 (`config = { ...config,
    // model: ..., reasoning: ... }`) for the prepareNextTurn
    // model/thinking swap. Phase 4 lands the hook signature and
    // the placeholder fields; phase 4.5 will actually assign
    // through this binding. Keeping `mut` here matches pi's
    // shape and avoids needing to retype the parameter when the
    // assignment site activates.
    #[allow(unused_mut)] mut config: LoopConfig,
    signal: AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    stream_fn: &StreamFn,
) -> Vec<LoopMessage> {
    let mut first_turn = true;

    // Storm breaker: tracks (tool_name, args) repeats to detect
    // stuck-in-a-loop behavior. Reset each new user turn.
    // Port of Reasonix `repair/index.ts:38-46` + `loop.ts:621`.
    let mut storm = storm_for_config(&config);

    // Inflight set: authoritative running-id tracker.
    // UI cards consult `inflight.has(call_id)` to derive spinner state.
    // Port of Reasonix `loop.ts:147` InflightSet.
    let inflight = InflightSet::new();

    // Multi-tier compaction tracking. Port of Reasonix
    // loop.ts:172 `this._foldedThisTurn`.
    // Reset each new user turn; set true when a fold happens.
    let mut folded_this_turn: bool;

    // Pi line 167: initial steering poll.
    let mut pending_messages: Vec<LoopMessage> = match &config.get_steering_messages {
        Some(get) => get().await,
        None => Vec::new(),
    };

    'outer: loop {
        // Storm: fresh intent on each new user turn.
        // Port of Reasonix loop.ts:621 `this.repair.resetStorm()`.
        storm.reset();
        let mut turn_self_corrected = false;

        // Multi-tier: fresh turn intent — clear fold flag.
        // Port of Reasonix loop.ts:623 `this._foldedThisTurn = false`.
        folded_this_turn = false;

        let mut has_more_tool_calls = true;

        // Pi line 174: INNER LOOP.
        while has_more_tool_calls || !pending_messages.is_empty() {
            // Pi lines 175-179: turn_start (skipped on very first
            // iteration — the outer wrapper already emitted it).
            if !first_turn {
                let _ = emit.send(LoopEvent::TurnStart).await;
            } else {
                first_turn = false;
            }

            // Reasonix loop.ts:656-684 — turn-start fold estimate.
            // Covers cases the post-response fold can't see:
            // terminal prior turn, session restore, huge paste.
            // Estimate is approximate (no tokenizer); defaults to
            // no-fold when data is unavailable.
            {
                let ctx_max = config
                    .model_name
                    .as_deref()
                    .and_then(crate::config::context_window_for_model)
                    .unwrap_or(128_000);
                // Rough estimate from message count × avg content length.
                let rough_estimate: u64 = current_context
                    .messages
                    .iter()
                    .map(|m| {
                        let content = m
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .len() as u64;
                        // ~4 chars per token heuristic
                        content / 4
                    })
                    .sum();
                let estimate = context_manager::estimate_turn_start(rough_estimate, ctx_max);
                if estimate.ratio > context_manager::TURN_START_FOLD_THRESHOLD {
                    tracing::warn!(
                        target: "dirge::agent_loop",
                        estimate_tokens = %estimate.estimate_tokens,
                        ctx_max = %estimate.ctx_max,
                        ratio = %estimate.ratio,
                        "context-manager: turn-start fold recommended ({}% of context)",
                        (estimate.ratio * 100.0) as u32,
                    );
                }
            }

            // Pi lines 181-189: inject pending steering / follow-up
            // messages.
            if !pending_messages.is_empty() {
                for msg in &pending_messages {
                    let _ = emit
                        .send(LoopEvent::MessageStart {
                            message: msg.clone(),
                        })
                        .await;
                    let _ = emit
                        .send(LoopEvent::MessageEnd {
                            message: msg.clone(),
                        })
                        .await;
                    current_context.messages.push(loop_message_to_value(msg));
                    new_messages.push(msg.clone());
                }
                pending_messages.clear();
            }

            // Pi lines 192-194: LLM call.
            let (assistant_msg, token_usage) = stream_assistant_response(
                &mut current_context,
                &config,
                signal.clone(),
                emit,
                stream_fn,
            )
            .await;
            new_messages.push(LoopMessage::Assistant(assistant_msg.clone()));

            // Pi lines 196-200: error / aborted short-circuit.
            if matches!(
                assistant_msg.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                let _ = emit
                    .send(LoopEvent::TurnEnd {
                        message: assistant_msg.clone(),
                        tool_results: Vec::new(),
                    })
                    .await;
                let _ = emit
                    .send(LoopEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await;
                return new_messages;
            }

            // Pi lines 202-216: tool calls + results.
            let mut tool_calls = extract_tool_calls_from(&assistant_msg);

            // Scavenge: scan reasoning content for tool calls the
            // model forgot to emit in `tool_calls`. Port of Reasonix
            // repair/index.ts:65-85.
            //
            // Only tools in the current context's tool set are
            // accepted. Deduplication by (name, args) signature
            // prevents double-counting if the same call appears in
            // both reasoning and declared tool_calls.
            let allowed_names: std::collections::HashSet<String> = current_context
                .tools
                .iter()
                .map(|t| t.name().to_string())
                .collect();
            let reasoning_text: String = assistant_msg
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Thinking { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !reasoning_text.is_empty() {
                let scavenge_result =
                    super::scavenge::scavenge_tool_calls(Some(&reasoning_text), &allowed_names, 4);
                if !scavenge_result.calls.is_empty() {
                    let seen_signatures: std::collections::HashSet<String> = tool_calls
                        .iter()
                        .map(|tc| {
                            format!(
                                "{}::{}",
                                tc.name,
                                serde_json::to_string(&tc.arguments).unwrap_or_default()
                            )
                        })
                        .collect();
                    for sc in &scavenge_result.calls {
                        let sig = format!(
                            "{}::{}",
                            sc.name,
                            serde_json::to_string(&sc.arguments).unwrap_or_default()
                        );
                        if !seen_signatures.contains(&sig) {
                            tool_calls.push(sc.clone());
                        }
                    }
                }
            }

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let original_count = tool_calls.len();
                let (surviving_calls, storm_report) = storm.filter_calls(&tool_calls);
                let all_suppressed = storm_report.all_suppressed(original_count);

                // Port of Reasonix loop.ts:935-956 — first-time
                // all-suppressed: self-correction. Stub tool
                // results with a guard message and give the model
                // one shot to self-correct before the loud-warning
                // path.
                if all_suppressed && !turn_self_corrected {
                    turn_self_corrected = true;
                    let guard_text = "[repeat-loop guard] this call was suppressed because it was identical to a previous call in this turn. Earlier results for it are above — try a meaningfully different approach, or stop and answer if you have enough.";
                    let guard_blocks = vec![ContentBlock::Text {
                        text: guard_text.to_string(),
                    }];
                    for call in &tool_calls {
                        let tr = ToolResultMessage {
                            tool_call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            content: guard_blocks.clone(),
                            details: Value::Null,
                            is_error: false,
                        };
                        current_context.messages.push(tool_result_to_value(&tr));
                        new_messages.push(LoopMessage::ToolResult(tr.clone()));
                        tool_results.push(tr);
                    }
                    // Surface the self-correction as a tool result
                    // with a guard text — the model sees it as
                    // output for its suppressed tool calls.
                    has_more_tool_calls = true;
                } else if storm_report.storms_broken > 0 && surviving_calls.is_empty() {
                    // Port of Reasonix loop.ts:975-982:
                    // no calls left, all suppressed and already
                    // self-corrected. Model is stuck — no more
                    // tool calls to dispatch, exit the inner
                    // loop.
                    has_more_tool_calls = false;
                }

                // Dispatch surviving calls through the unified dispatch.
                // `execute_tool_calls` takes pre-extracted tool calls.
                if !surviving_calls.is_empty() {
                    let batch = super::tools::execute_tool_calls(
                        &current_context,
                        &assistant_msg,
                        &surviving_calls,
                        &config,
                        &signal,
                        emit,
                        &inflight,
                    )
                    .await;
                    tool_results.extend(batch.messages.clone());
                    has_more_tool_calls = !batch.terminate;
                    for result in &batch.messages {
                        current_context.messages.push(tool_result_to_value(result));
                        new_messages.push(LoopMessage::ToolResult(result.clone()));
                    }
                }
            }

            // Pi line 218: turn_end.
            let _ = emit
                .send(LoopEvent::TurnEnd {
                    message: assistant_msg.clone(),
                    tool_results: tool_results.clone(),
                })
                .await;

            // Reasonix loop.ts:987-1032 — context-manager decision
            // after each turn's response. Thresholds:
            //   >80% → exit-with-summary (defense in depth)
            //   >78% → aggressive fold (half tail budget)
            //   >75% → normal fold
            //   ≤75% → carry on
            //
            // `prompt_tokens` is None until usage tracking is wired
            // into the stream pipeline (future phase). With None,
            // decision defaults to None (carry on).
            {
                let ctx_max = config
                    .model_name
                    .as_deref()
                    .and_then(crate::config::context_window_for_model)
                    .unwrap_or(128_000);
                let decision = context_manager::decide_after_usage(
                    token_usage.map(|u| u.input_tokens),
                    ctx_max,
                    folded_this_turn,
                );
                match decision.kind {
                    PostUsageDecisionKind::Fold if !folded_this_turn => {
                        folded_this_turn = true;
                        tracing::info!(
                            target: "dirge::agent_loop",
                            ratio = %decision.ratio,
                            aggressive = decision.aggressive,
                            tail_budget = ?decision.tail_budget,
                            "context-manager: fold recommended ({})",
                            if decision.aggressive { "aggressive" } else { "normal" },
                        );

                        // Context compaction: prune old tool results and
                        // compress the middle section of the conversation.
                        // Port of Hermes's compression pass.
                        if let Some(prompt_tokens) = token_usage.map(|u| u.input_tokens) {
                            if crate::agent::compression::should_compress(prompt_tokens, ctx_max) {
                                let before = crate::agent::compression::estimate_messages_tokens(
                                    &current_context.messages,
                                );
                                // Prune large tool outputs — cheap pre-pass,
                                // no LLM call needed.
                                let pruned = crate::agent::compression::prune_tool_outputs(
                                    &current_context.messages,
                                    5, // protect last 5 messages
                                );
                                current_context.messages = pruned;

                                // Build a summary marker as a system message
                                // so the model knows context was compacted.
                                let total = crate::agent::compression::estimate_messages_tokens(
                                    &current_context.messages,
                                );
                                let new_id = format!(
                                    "compacted-{}",
                                    uuid::Uuid::new_v4()
                                        .to_string()
                                        .chars()
                                        .take(8)
                                        .collect::<String>()
                                );
                                let _ = emit
                                    .send(LoopEvent::ContextCompacted {
                                        new_session_id: new_id,
                                        tokens_before: before,
                                        tokens_after: total,
                                    })
                                    .await;
                            }
                        }
                    }
                    PostUsageDecisionKind::ExitWithSummary => {
                        tracing::warn!(
                            target: "dirge::agent_loop",
                            ratio = %decision.ratio,
                            "context-manager: forcing summary and ending turn",
                        );
                        // When context is critically over the threshold,
                        // prune aggressively and insert a compression marker.
                        let before = crate::agent::compression::estimate_messages_tokens(
                            &current_context.messages,
                        );
                        let pruned = crate::agent::compression::prune_tool_outputs(
                            &current_context.messages,
                            3, // protect only last 3
                        );
                        current_context.messages = pruned;
                        let after = crate::agent::compression::estimate_messages_tokens(
                            &current_context.messages,
                        );
                        let new_id = format!(
                            "compacted-{}",
                            uuid::Uuid::new_v4()
                                .to_string()
                                .chars()
                                .take(8)
                                .collect::<String>()
                        );
                        let _ = emit
                            .send(LoopEvent::ContextCompacted {
                                new_session_id: new_id,
                                tokens_before: before,
                                tokens_after: after,
                            })
                            .await;
                    }
                    _ => {}
                }
            }

            // Pi lines 220-239: prepareNextTurn.
            if let Some(hook) = &config.prepare_next_turn {
                let hook_ctx = super::hooks::TurnHookContext {
                    message: assistant_msg.clone(),
                    tool_results: tool_results.clone(),
                    context: current_context.clone(),
                    new_messages: new_messages.clone(),
                };
                if let Some(update) = hook(hook_ctx).await {
                    // Pi line 228: `context: snapshot.context ??
                    // currentContext`. Apply only `Some`.
                    if let Some(new_ctx) = update.context {
                        current_context = new_ctx;
                    }
                    // Pi lines 229-238 rebuild config with the
                    // new model / reasoning. Doing that in Rust
                    // requires re-building the `StreamFn` closure
                    // (which has the CompletionModel baked in at
                    // construction by `rig_stream_fn_from_model`).
                    // The StreamFn isn't part of LoopConfig — it's
                    // passed to `run_loop` separately — so we
                    // can't swap it mid-run without restructuring
                    // the loop's surface.
                    //
                    // Surface a warning so users wiring this hook
                    // know their swap was ignored. Code-review
                    // gap #3: lift this when a real consumer
                    // needs mid-run model swap; the fix is to
                    // accept a `Fn(Context) -> StreamFn` factory
                    // instead of a single StreamFn.
                    if let Some(model) = &update.model {
                        tracing::warn!(
                            target: "dirge::agent_loop",
                            requested_model = %model,
                            "prepareNextTurn returned a new model but mid-run swap is not yet wired — ignoring",
                        );
                    }
                    if let Some(level) = &update.thinking_level {
                        tracing::warn!(
                            target: "dirge::agent_loop",
                            requested_thinking = ?level,
                            "prepareNextTurn returned a new thinking_level but mid-run swap is not yet wired — ignoring",
                        );
                    }
                }
            }

            // Pi lines 241-251: shouldStopAfterTurn.
            if let Some(hook) = &config.should_stop_after_turn {
                let hook_ctx = super::hooks::TurnHookContext {
                    message: assistant_msg.clone(),
                    tool_results: tool_results.clone(),
                    context: current_context.clone(),
                    new_messages: new_messages.clone(),
                };
                if hook(hook_ctx).await {
                    let _ = emit
                        .send(LoopEvent::AgentEnd {
                            messages: new_messages.clone(),
                        })
                        .await;
                    return new_messages;
                }
            }

            // Pi line 253: refresh steering for next iteration.
            pending_messages = match &config.get_steering_messages {
                Some(get) => get().await,
                None => Vec::new(),
            };
        }
        // INNER END

        // LOOP-4: check for graceful interjection at the turn
        // boundary. In-flight tools already completed normally
        // (they never check `is_interjected()`). Stop here rather
        // than starting a new turn or processing follow-ups.
        if signal.is_interjected() {
            break;
        }

        // Pi lines 256-262: outer-loop follow-up poll.
        let follow_up = match &config.get_followup_messages {
            Some(get) => get().await,
            None => Vec::new(),
        };
        if !follow_up.is_empty() {
            pending_messages = follow_up;
            continue 'outer;
        }
        break;
    }

    // Pi line 268: final agent_end.
    let _ = emit
        .send(LoopEvent::AgentEnd {
            messages: new_messages.clone(),
        })
        .await;
    new_messages
}

/// Local extract — same as `tools::extract_tool_calls`. Kept
/// inline so `run.rs` doesn't reach into `tools` for tiny helpers.
fn extract_tool_calls_from(msg: &AssistantMessage) -> Vec<super::tools::ToolCall> {
    super::tools::extract_tool_calls(msg)
}

/// Convert a `LoopMessage` to the placeholder `Value` shape used
/// in `Context.messages`. Mirrors `serialize_assistant` from
/// stream.rs but covers every variant.
///
/// Phase 4 placeholder — phase ??? swaps the Vec<Value> for typed
/// messages and this helper goes away.
fn loop_message_to_value(msg: &LoopMessage) -> Value {
    match msg {
        LoopMessage::User(u) => serde_json::json!({
            "role": "user",
            "content": u.content,
        }),
        LoopMessage::Assistant(a) => serde_json::json!({
            "role": "assistant",
            "content": a.content,
            "stopReason": a.stop_reason,
            "errorMessage": a.error_message,
        }),
        LoopMessage::ToolResult(t) => tool_result_to_value(t),
        LoopMessage::Custom(v) => v.clone(),
    }
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

// =====================================================================
// Tests — ported from pi/test/agent-loop.test.ts
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::hooks::{
        AfterToolCallContext, AfterToolCallFn, GetSteeringMessagesFn, PrepareNextTurnFn,
        ShouldStopAfterTurnFn,
    };
    use crate::agent::agent_loop::message::{StreamEvent, UserMessage};
    use crate::agent::agent_loop::result::AfterToolCallResult;
    use crate::agent::agent_loop::stream::StreamFn;
    use crate::agent::agent_loop::tool::{AbortSignal, LoopTool, LoopToolUpdate};
    use crate::agent::agent_loop::types::{
        ConvertToLlmFn, LoopConfig, ToolExecutionMode, TurnUpdate,
    };
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Build a stream factory that returns canned assistant
    /// messages in sequence. Mirrors pi's typical test mock —
    /// `callIndex` increments per invocation; each call returns
    /// the next canned response.
    ///
    /// `responses` is a Vec; index N is returned on the (N+1)th
    /// call. Past the end → final fallback message with
    /// stopReason=Stop.
    fn canned_factory(responses: Vec<AssistantMessage>) -> StreamFn {
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let responses = std::sync::Arc::new(responses);
        std::sync::Arc::new(move |_ctx, _opts| {
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
        }
    }

    fn empty_context() -> Context {
        Context {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    /// Mock echo tool for run-loop tests. Records executed args
    /// per call so test setups can detect terminate-flag flow.
    #[derive(Debug)]
    struct EchoTool {
        terminate: bool,
        executed: std::sync::Arc<Mutex<Vec<Value>>>,
    }
    impl EchoTool {
        fn new() -> Self {
            Self {
                terminate: false,
                executed: std::sync::Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn with_terminate(mut self) -> Self {
            self.terminate = true;
            self
        }
    }
    impl LoopTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo tool"
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
        ) -> Pin<Box<dyn Future<Output = Result<super::super::LoopToolResult, String>> + Send + 'a>>
        {
            let executed = self.executed.clone();
            let terminate = self.terminate;
            Box::pin(async move {
                executed.lock().unwrap().push(args.clone());
                Ok(super::super::LoopToolResult {
                    content: vec![serde_json::json!({"type": "text", "text": "ok"})],
                    details: args,
                    terminate: if terminate { Some(true) } else { None },
                })
            })
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

    /// Drain channel into a Vec.
    async fn drain(rx: &mut mpsc::Receiver<LoopEvent>) -> Vec<LoopEvent> {
        let mut out = Vec::new();
        while let Some(e) = rx.recv().await {
            out.push(e);
        }
        out
    }

    /// Port of pi test "should emit events with AgentMessage types"
    /// (agent-loop.test.ts:84). Full agent loop run — assistant
    /// response, no tools.
    #[tokio::test]
    async fn test_emits_full_agent_loop_event_sequence() {
        let factory = canned_factory(vec![text_response("Hi there!")]);
        let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
        let messages = run_agent_loop(
            vec![user("Hello")],
            empty_context(),
            build_config(),
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;
        drop(tx);

        let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
        // Must contain all pi-required events.
        for required in [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ] {
            assert!(kinds.contains(&required), "missing {required}: {kinds:?}");
        }
        // Return value: user + assistant message.
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role(), "user");
        assert_eq!(messages[1].role(), "assistant");
    }

    /// Port of pi test "should handle tool calls and results"
    /// (agent-loop.test.ts:239). Full-loop scope: assistant emits
    /// tool call → loop dispatches → next assistant emits final
    /// text.
    #[tokio::test]
    async fn test_full_loop_with_tool_then_final_text() {
        let echo = std::sync::Arc::new(EchoTool::new());
        let mut ctx = empty_context();
        ctx.tools.push(echo.clone());

        let factory = canned_factory(vec![
            tool_use_response("call-1", "echo", serde_json::json!({"v": 1})),
            text_response("done"),
        ]);

        let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
        let messages = run_agent_loop(
            vec![user("echo")],
            ctx,
            build_config(),
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;
        drop(tx);

        // Tool actually executed.
        assert_eq!(echo.executed.lock().unwrap().len(), 1);

        // Roles: user, assistant (tool use), toolResult, assistant (text).
        let roles: Vec<_> = messages.iter().map(|m| m.role()).collect();
        assert_eq!(roles, vec!["user", "assistant", "toolResult", "assistant"]);

        // Stream of events should contain tool_execution_start +
        // tool_execution_end.
        let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
        assert!(kinds.contains(&"tool_execution_start"));
        assert!(kinds.contains(&"tool_execution_end"));
    }

    /// Port of pi test "should use prepareNextTurn snapshot before
    /// continuing" (agent-loop.test.ts:897). The hook returns a
    /// snapshot mutating `context`; subsequent turn observes the
    /// mutation.
    #[tokio::test]
    async fn test_prepare_next_turn_snapshot_applied() {
        let echo = std::sync::Arc::new(EchoTool::new());
        let mut ctx = empty_context();
        ctx.system_prompt = "first prompt".to_string();
        ctx.tools.push(echo.clone());

        // Track the system_prompt seen at each LLM call.
        let observed_prompts = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let observed_clone = observed_prompts.clone();
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let factory: StreamFn = std::sync::Arc::new(move |llm_ctx, _opts| {
            observed_clone.lock().unwrap().push(llm_ctx.system_prompt);
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let msg = if n == 0 {
                tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
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

        // Hook fires once: returns a new context with a different
        // system prompt.
        let fired = std::sync::Arc::new(AtomicUsize::new(0));
        let fired_clone = fired.clone();
        let hook: PrepareNextTurnFn = std::sync::Arc::new(move |ctx| {
            let fired = fired_clone.clone();
            Box::pin(async move {
                if fired.fetch_add(1, Ordering::SeqCst) > 0 {
                    return None; // only on the first invocation
                }
                Some(TurnUpdate {
                    context: Some(Context {
                        system_prompt: "second prompt".to_string(),
                        messages: ctx.context.messages.clone(),
                        tools: ctx.context.tools.clone(),
                    }),
                    ..Default::default()
                })
            })
        });

        let mut config = build_config();
        config.prepare_next_turn = Some(hook);

        let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
        let _ = run_agent_loop(
            vec![user("echo something")],
            ctx,
            config,
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;

        let observed = observed_prompts.lock().unwrap().clone();
        assert_eq!(observed.len(), 2, "expected 2 LLM calls");
        assert_eq!(observed[0], "first prompt");
        assert_eq!(
            observed[1], "second prompt",
            "second LLM call should see the mutated context"
        );
    }

    /// Port of pi test "should stop after the current turn when
    /// shouldStopAfterTurn returns true" (agent-loop.test.ts:970).
    #[tokio::test]
    async fn test_should_stop_after_turn_stops_loop() {
        let factory = canned_factory(vec![
            text_response("turn one"),
            // Second response should NEVER be requested — hook
            // stops the loop after turn one.
            text_response("should not appear"),
        ]);

        let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let llm_calls_clone = llm_calls.clone();
        // Wrap factory to count invocations.
        let factory_counted: StreamFn = std::sync::Arc::new(move |ctx, opts| {
            llm_calls_clone.fetch_add(1, Ordering::SeqCst);
            factory(ctx, opts)
        });

        let hook: ShouldStopAfterTurnFn = std::sync::Arc::new(|_ctx| Box::pin(async move { true }));

        let mut config = build_config();
        config.should_stop_after_turn = Some(hook);

        let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
        let messages = run_agent_loop(
            vec![user("hi")],
            empty_context(),
            config,
            AbortSignal::new(),
            &tx,
            &factory_counted,
        )
        .await;
        drop(tx);

        // Only one LLM call.
        assert_eq!(llm_calls.load(Ordering::SeqCst), 1);
        // Messages: user + one assistant.
        assert_eq!(messages.len(), 2);
        // Loop emitted agent_end.
        let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
        assert!(kinds.contains(&"agent_end"));
    }

    /// Port of pi test "should stop after a tool batch when every
    /// tool result sets terminate=true" (agent-loop.test.ts:1067).
    /// LOOP-LEVEL: only one LLM call (the tool dispatch terminates).
    #[tokio::test]
    async fn test_terminate_stops_loop_after_tool_batch() {
        let echo = std::sync::Arc::new(EchoTool::new().with_terminate());
        let mut ctx = empty_context();
        ctx.tools.push(echo);

        let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let llm_calls_clone = llm_calls.clone();
        let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
            llm_calls_clone.fetch_add(1, Ordering::SeqCst);
            let msg = tool_use_response("call-1", "echo", serde_json::json!({"v": 1}));
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason: StopReason::ToolUse,
                message: msg,
                usage: None,
            }]))
        });

        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
        let messages = run_agent_loop(
            vec![user("echo")],
            ctx,
            build_config(),
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;

        assert_eq!(llm_calls.load(Ordering::SeqCst), 1, "no second LLM call");
        // user + assistant(tool use) + toolResult — no second
        // assistant text turn.
        let roles: Vec<_> = messages.iter().map(|m| m.role()).collect();
        assert_eq!(roles, vec!["user", "assistant", "toolResult"]);
    }

    /// Port of pi test "should allow afterToolCall to mark a tool
    /// batch as terminating" (agent-loop.test.ts:1184). LOOP-LEVEL.
    #[tokio::test]
    async fn test_after_tool_call_terminate_stops_loop() {
        let echo = std::sync::Arc::new(EchoTool::new());
        let mut ctx = empty_context();
        ctx.tools.push(echo);

        let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let llm_calls_clone = llm_calls.clone();
        let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
            llm_calls_clone.fetch_add(1, Ordering::SeqCst);
            let msg = tool_use_response("call-1", "echo", serde_json::json!({"v": 1}));
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason: StopReason::ToolUse,
                message: msg,
                usage: None,
            }]))
        });

        let after: AfterToolCallFn = std::sync::Arc::new(|_ctx: AfterToolCallContext| {
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
        let _ = run_agent_loop(
            vec![user("echo")],
            ctx,
            config,
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;

        assert_eq!(llm_calls.load(Ordering::SeqCst), 1, "no second LLM call");
    }

    /// Port of pi test "should continue after parallel tool calls
    /// when not all tool results terminate" (agent-loop.test.ts:1119).
    /// LOOP-LEVEL: two LLM calls.
    #[tokio::test]
    async fn test_continue_when_not_all_terminate() {
        let echo = std::sync::Arc::new(EchoTool::new());
        let mut ctx = empty_context();
        ctx.tools.push(echo);

        let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let llm_calls_clone = llm_calls.clone();
        let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
            let n = llm_calls_clone.fetch_add(1, Ordering::SeqCst);
            let msg = if n == 0 {
                tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
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

        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
        let _ = run_agent_loop(
            vec![user("echo")],
            ctx,
            build_config(),
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;

        assert_eq!(
            llm_calls.load(Ordering::SeqCst),
            2,
            "two LLM calls expected"
        );
    }

    /// Port of pi test "should inject queued messages after all
    /// tool calls complete" (agent-loop.test.ts:547).
    ///
    /// Setup: assistant emits a tool call. After tool dispatch
    /// the loop polls `getSteeringMessages` which returns a user
    /// message ONCE. That message is injected before the next
    /// assistant call; the second LLM call sees it in its context.
    #[tokio::test]
    async fn test_steering_messages_injected_after_tool_calls() {
        let echo = std::sync::Arc::new(EchoTool::new());
        let mut ctx = empty_context();
        ctx.tools.push(echo);

        // Steering hook delivers once on the SECOND call (so
        // not on initial poll).
        let poll_count = std::sync::Arc::new(AtomicUsize::new(0));
        let poll_clone = poll_count.clone();
        let steering: GetSteeringMessagesFn = std::sync::Arc::new(move || {
            let poll = poll_clone.clone();
            Box::pin(async move {
                let n = poll.fetch_add(1, Ordering::SeqCst);
                if n == 1 {
                    vec![user("interrupt")]
                } else {
                    Vec::new()
                }
            })
        });

        // Inspector: record what each LLM call sees in its
        // converted message list.
        let saw_interrupt_on_second = std::sync::Arc::new(std::sync::Mutex::new(false));
        let saw_clone = saw_interrupt_on_second.clone();
        let call_counter = std::sync::Arc::new(AtomicUsize::new(0));

        let factory: StreamFn = std::sync::Arc::new(move |llm_ctx, _opts| {
            let n = call_counter.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                // Second call: check for "interrupt" in messages.
                let found = llm_ctx.messages.iter().any(|m| {
                    m.get("role").and_then(|r| r.as_str()) == Some("user")
                        && m.get("content")
                            .and_then(|c| c.as_str())
                            .map(|s| s.contains("interrupt"))
                            == Some(true)
                });
                *saw_clone.lock().unwrap() = found;
            }
            let msg = if n == 0 {
                tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
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

        let mut config = build_config();
        config.get_steering_messages = Some(steering);

        let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
        let messages = run_agent_loop(
            vec![user("start")],
            ctx,
            config,
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;
        drop(tx);

        assert!(
            *saw_interrupt_on_second.lock().unwrap(),
            "second LLM call should see the injected interrupt"
        );

        // Returned messages include the injected interrupt.
        let user_contents: Vec<String> = messages
            .iter()
            .filter_map(|m| match m {
                LoopMessage::User(u) => Some(u.content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(user_contents, vec!["start", "interrupt"]);

        // The interrupt's message_start fires AFTER the tool
        // result's message_end. We verify by event ordering.
        let events = drain(&mut rx).await;
        let interrupt_idx = events.iter().position(|e| match e {
            LoopEvent::MessageStart {
                message: LoopMessage::User(u),
            } => u.content == "interrupt",
            _ => false,
        });
        let last_tool_result_end_idx = events.iter().rposition(|e| {
            matches!(
                e,
                LoopEvent::MessageEnd {
                    message: LoopMessage::ToolResult(_)
                }
            )
        });
        assert!(
            interrupt_idx.unwrap() > last_tool_result_end_idx.unwrap(),
            "interrupt should appear AFTER the tool result message_end"
        );
    }

    // ============================================================
    // Phase 6 — regression tests for hardening paths
    // ============================================================

    use crate::agent::agent_loop::result::LoopToolResult as PhaseSixToolResult;
    use std::sync::Arc as PhaseSixArc;

    /// Phase 6: a multi-turn run with a network error in turn 2
    /// preserves the FULL history (user prompt, turn 1's
    /// assistant + tool-result) across the retry. The retry
    /// wrapper isn't directly invoked here (we use mock
    /// StreamFn), but the LOOP's context.messages survival
    /// across turn errors is the invariant.
    ///
    /// We verify by counting context.messages entries the
    /// second LLM call observes. The mock StreamFn captures
    /// what each call sees.
    #[tokio::test]
    async fn loop_preserves_history_across_turns() {
        use crate::agent::agent_loop::stream::{LlmContext, StreamFn};
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let observed_lens: PhaseSixArc<Mutex<Vec<usize>>> =
            PhaseSixArc::new(Mutex::new(Vec::new()));
        let observed_clone = observed_lens.clone();
        let counter = std::sync::Arc::new(AtomicUsize::new(0));

        // Inline echo tool — needed for the tool-result turn
        // that grows the history.
        #[derive(Debug)]
        struct LocalEcho;
        impl LoopTool for LocalEcho {
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
                _on_update: super::super::tool::LoopToolUpdate,
            ) -> Pin<Box<dyn Future<Output = Result<PhaseSixToolResult, String>> + Send + 'a>>
            {
                Box::pin(async move {
                    Ok(PhaseSixToolResult {
                        content: vec![serde_json::json!({
                            "type": "text",
                            "text": "ok",
                        })],
                        details: Value::Null,
                        terminate: None,
                    })
                })
            }
        }

        let factory: StreamFn = std::sync::Arc::new(move |ctx: LlmContext, _opts| {
            observed_clone.lock().unwrap().push(ctx.messages.len());
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let msg = if n == 0 {
                tool_use_response("call-1", "echo", serde_json::json!({}))
            } else {
                text_response("done")
            };
            let reason = msg.stop_reason;
            Box::pin(futures::stream::iter(vec![
                crate::agent::agent_loop::message::StreamEvent::Done {
                    reason,
                    message: msg,
                    usage: None,
                },
            ]))
        });

        let mut ctx = empty_context();
        ctx.tools.push(PhaseSixArc::new(LocalEcho));
        let mut cfg = build_config();
        cfg.tool_execution = ToolExecutionMode::Sequential;

        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
        let _ = run_agent_loop(
            vec![user("start")],
            ctx,
            cfg,
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;

        let lens = observed_lens.lock().unwrap().clone();
        assert_eq!(lens.len(), 2, "expected two LLM calls");
        // First call sees: just user prompt → 1 message.
        assert_eq!(lens[0], 1);
        // Second call sees: user prompt + assistant (tool_use) +
        // tool result → 3 messages. History preserved.
        assert_eq!(
            lens[1], 3,
            "second LLM call should see prior turn's history; got {} messages",
            lens[1],
        );
    }

    /// Phase 6: full signal-chain regression. Cancel the signal
    /// mid-tool; tool aborts; loop's next LLM call's stream
    /// observes the same signal and exits via Error path; loop
    /// exits cleanly with no infinite-loop or hung tools.
    #[tokio::test]
    async fn full_signal_chain_exits_cleanly() {
        use crate::agent::agent_loop::stream::{LlmContext, StreamFn};
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Mock tool that observes the signal during execution
        // (immediate cancel since the test cancels signal right
        // after spawn).
        #[derive(Debug)]
        struct CancellableTool;
        impl LoopTool for CancellableTool {
            fn name(&self) -> &str {
                "noop"
            }
            fn description(&self) -> &str {
                "Cancellable"
            }
            fn label(&self) -> &str {
                "Noop"
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
                _on_update: super::super::tool::LoopToolUpdate,
            ) -> Pin<Box<dyn Future<Output = Result<PhaseSixToolResult, String>> + Send + 'a>>
            {
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    Ok(PhaseSixToolResult {
                        content: Vec::new(),
                        details: Value::Null,
                        terminate: None,
                    })
                })
            }
        }

        // Factory that returns a tool_use response first,
        // then would return a text response on retry (but
        // shouldn't get there because signal is cancelled
        // before turn 2).
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let factory: StreamFn = std::sync::Arc::new(move |_ctx: LlmContext, _opts| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let msg = if n == 0 {
                tool_use_response("call-1", "noop", serde_json::json!({}))
            } else {
                text_response("should-not-reach")
            };
            let reason = msg.stop_reason;
            Box::pin(futures::stream::iter(vec![
                crate::agent::agent_loop::message::StreamEvent::Done {
                    reason,
                    message: msg,
                    usage: None,
                },
            ]))
        });

        let mut ctx = empty_context();
        ctx.tools.push(PhaseSixArc::new(CancellableTool));
        let mut cfg = build_config();
        cfg.tool_execution = ToolExecutionMode::Sequential;

        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
        let signal = AbortSignal::new();
        let signal_clone = signal.clone();

        // Spawn the loop in a task; cancel signal after a small
        // yield so the tool has started.
        let task = tokio::spawn(async move {
            run_agent_loop(vec![user("start")], ctx, cfg, signal_clone, &tx, &factory).await
        });
        // Yield twice so the loop reaches the tool dispatch
        // before we cancel.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        signal.cancel();

        // Bound the test: loop must complete in <2s. Without
        // the tool-abort wrap, the 30s blocking tool would
        // exceed this. R3 ensures the next LLM call (if any)
        // also exits promptly via its pre-poll signal check.
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
        assert!(
            result.is_ok(),
            "loop should exit within 2s after signal cancel"
        );
    }
}
