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

/// LOOP-9 — context-compaction worker. Runs the cheap pruning pass
/// first; when a summarizer callback is wired AND pruning alone
/// didn't free enough headroom (compressed token count is still
/// above the pruner's protection floor), invokes the auxiliary
/// summarizer + replaces the middle section of `current_context.messages`
/// with a structured-summary system message.
///
/// Emits `LoopEvent::ContextCompacted` with a rotated session id
/// once the pass finishes (whether pruning-only or pruning+summary).
/// Session.id rotation + DB persistence is delegated to the event
/// consumer side via this event channel.
async fn run_compaction_pass(
    current_context: &mut Context,
    summarize_fn: &Option<crate::agent::compression::SummarizeFn>,
    protect_tail: usize,
    emit: &mpsc::Sender<LoopEvent>,
) {
    run_compaction_pass_with_focus(current_context, summarize_fn, protect_tail, None, emit).await
}

/// Same as `run_compaction_pass` but accepts an optional focus
/// topic to splice into the Hermes-style summary prompt. Wired by
/// the `/compress <focus>` slash command path. The auto-triggered
/// compaction (`PostUsageDecisionKind::Fold` / `ExitWithSummary`)
/// continues to use the no-focus wrapper above.
async fn run_compaction_pass_with_focus(
    current_context: &mut Context,
    summarize_fn: &Option<crate::agent::compression::SummarizeFn>,
    protect_tail: usize,
    focus_topic: Option<String>,
    emit: &mpsc::Sender<LoopEvent>,
) {
    use crate::agent::compression;

    let before = compression::estimate_messages_tokens(&current_context.messages);

    // First pass: cheap tool-output pruning. No LLM call.
    let pruned = compression::prune_tool_outputs(&current_context.messages, protect_tail);
    current_context.messages = pruned;
    let after_prune = compression::estimate_messages_tokens(&current_context.messages);

    // Second pass: if a summarizer is wired AND we still have
    // meaningful material to summarize, build the Hermes-style
    // structured prompt, call the auxiliary model, validate the
    // returned summary, and replace the middle section.
    let mut after_summary = after_prune;
    let mut applied_summary = String::new();
    // first_kept_index defaults to "no message was folded out" —
    // pruner-only path doesn't drop messages by index, just trims
    // their content in place. compress_reporting handles that
    // gracefully (zero-width fold).
    let mut applied_first_kept = current_context.messages.len();
    if let Some(sfn) = summarize_fn {
        let (start, end) = compression::compute_compress_window(
            &current_context.messages,
            compression::PROTECT_HEAD_DEFAULT,
            protect_tail.max(compression::PROTECT_TAIL_DEFAULT),
        );
        if start < end {
            let middle: Vec<serde_json::Value> = current_context.messages[start..end].to_vec();
            // Carry forward any previous summary body for iterative
            // re-compression (Hermes _find_latest_context_summary).
            let prev =
                compression::find_previous_summary(&current_context.messages).map(|(_, body)| body);
            let budget =
                compression::summary_budget(compression::estimate_messages_tokens(&middle));
            let prompt = compression::build_summary_prompt(
                &middle,
                budget,
                prev.as_deref(),
                focus_topic.as_deref(),
            );
            match sfn(prompt).await {
                Ok(summary) if compression::validate_summary(&summary) => {
                    let new_msgs =
                        compression::apply_summary(&current_context.messages, &summary, start, end);
                    current_context.messages = new_msgs;
                    after_summary =
                        compression::estimate_messages_tokens(&current_context.messages);
                    applied_summary = summary;
                    // After apply_summary, the head (0..start) is
                    // preserved, then a single summary message
                    // takes the place of the middle, then the tail
                    // resumes. The first KEPT original-index slot
                    // is therefore `start` — anything below was
                    // protected, anything above was folded.
                    applied_first_kept = start;
                }
                Ok(_) => {
                    tracing::warn!(
                        target: "dirge::agent_loop",
                        "compaction summarizer returned an unvalidated summary — keeping pruned context",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "dirge::agent_loop",
                        error = %e,
                        "compaction summarizer failed — keeping pruned context",
                    );
                }
            }
        }
    }

    let new_id = compression::rotate_session_id();
    let _ = emit
        .send(LoopEvent::ContextCompacted {
            new_session_id: new_id,
            tokens_before: before,
            tokens_after: after_summary,
            summary: applied_summary,
            first_kept_index: applied_first_kept,
        })
        .await;
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
    context: Context,
    config: LoopConfig,
    signal: AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    stream_fn: &StreamFn,
) -> Vec<LoopMessage> {
    run_agent_loop_with_summarizer(prompts, context, config, signal, emit, stream_fn, None).await
}

/// Like `run_agent_loop` but accepts an optional summarizer callback
/// for LOOP-9 context compaction. When `Some`, the loop's compaction
/// path runs a structured summarization pass after the cheap
/// `prune_tool_outputs` pre-pass — see
/// `crate::agent::compression::SummarizeFn` for the contract.
pub async fn run_agent_loop_with_summarizer(
    prompts: Vec<LoopMessage>,
    mut context: Context,
    config: LoopConfig,
    signal: AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    stream_fn: &StreamFn,
    summarize_fn: Option<crate::agent::compression::SummarizeFn>,
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

    run_loop_with_summarizer(
        context,
        new_messages,
        config,
        signal,
        emit,
        stream_fn,
        summarize_fn,
    )
    .await
}

/// The actual loop. Faithful port of pi `runLoop` (agent-loop.ts:155-269).
///
/// Owns `current_context`, `new_messages`, `config` — pi mutates
/// these as the run proceeds; in Rust we own them by value and
/// return `new_messages` at the end.
pub async fn run_loop(
    current_context: Context,
    new_messages: Vec<LoopMessage>,
    config: LoopConfig,
    signal: AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    stream_fn: &StreamFn,
) -> Vec<LoopMessage> {
    run_loop_with_summarizer(
        current_context,
        new_messages,
        config,
        signal,
        emit,
        stream_fn,
        None,
    )
    .await
}

/// LOOP-9 variant: identical to `run_loop` plus an optional
/// `summarize_fn` callback for context compaction's second pass.
pub async fn run_loop_with_summarizer(
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
    summarize_fn: Option<crate::agent::compression::SummarizeFn>,
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
                    // LOOP-12: canonicalize the JSON so different
                    // key orders or numeric reprs (1 vs 1.0) for the
                    // same logical call don't slip past dedupe.
                    // `serde_json::to_string` on a `Map` preserves
                    // insertion order, which can vary between the
                    // assistant-emitted call and the scavenge-parsed
                    // form. `canonical_json` sorts keys and forces
                    // a stable number representation.
                    fn canonical_json(v: &serde_json::Value) -> String {
                        match v {
                            serde_json::Value::Object(m) => {
                                let mut keys: Vec<&String> = m.keys().collect();
                                keys.sort();
                                let mut s = String::from("{");
                                for (i, k) in keys.iter().enumerate() {
                                    if i > 0 {
                                        s.push(',');
                                    }
                                    s.push_str(&serde_json::to_string(k).unwrap_or_default());
                                    s.push(':');
                                    s.push_str(&canonical_json(&m[*k]));
                                }
                                s.push('}');
                                s
                            }
                            serde_json::Value::Array(a) => {
                                let mut s = String::from("[");
                                for (i, e) in a.iter().enumerate() {
                                    if i > 0 {
                                        s.push(',');
                                    }
                                    s.push_str(&canonical_json(e));
                                }
                                s.push(']');
                                s
                            }
                            serde_json::Value::Number(n) => {
                                // Normalize integers-stored-as-floats
                                // (`1.0` ≡ `1`) so reps match.
                                if let Some(i) = n.as_i64() {
                                    i.to_string()
                                } else if let Some(f) = n.as_f64() {
                                    if f.fract() == 0.0 && f.is_finite() {
                                        (f as i64).to_string()
                                    } else {
                                        f.to_string()
                                    }
                                } else {
                                    n.to_string()
                                }
                            }
                            other => serde_json::to_string(other).unwrap_or_default(),
                        }
                    }
                    let seen_signatures: std::collections::HashSet<String> = tool_calls
                        .iter()
                        .map(|tc| format!("{}::{}", tc.name, canonical_json(&tc.arguments)))
                        .collect();
                    for sc in &scavenge_result.calls {
                        let sig = format!("{}::{}", sc.name, canonical_json(&sc.arguments));
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
                                run_compaction_pass(
                                    &mut current_context,
                                    &summarize_fn,
                                    5, // protect last 5 messages
                                    emit,
                                )
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
                        // prune aggressively then run the structured-summary
                        // pass if a summarizer is wired.
                        run_compaction_pass(
                            &mut current_context,
                            &summarize_fn,
                            3, // protect only last 3
                            emit,
                        )
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
// Inlined tests were extracted to the sibling `run_tests.rs` file;
// `#[path = "..."]` pulls it in as the `tests` child module so the
// `use super::*` references inside continue to resolve.
// =====================================================================

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
