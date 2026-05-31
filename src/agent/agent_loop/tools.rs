//! Tool execution dispatcher. Phase 2 ports the SEQUENTIAL path
//! and the shared helpers (`prepare_tool_call`,
//! `execute_prepared_tool_call`, `finalize_executed_tool_call`,
//! `should_terminate_tool_batch`). The parallel path lands in
//! phase 3.
//!
//! Faithful port of pi `agent-loop.ts:370-737`. Each helper cites
//! its pi line range.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use super::hooks::{AfterToolCallContext, BeforeToolCallContext};
use super::inflight::InflightSet;
use super::message::{
    AssistantMessage, ContentBlock, EscalationReason, LoopEvent, LoopMessage, ToolResultMessage,
};
use super::result::LoopToolResult;
use super::tool::{AbortSignal, LoopTool, LoopToolUpdate};
use super::types::{Context, LoopConfig};

/// Canonical prefix emitted by
/// `crate::semantic::syntax_validator::format_errors` when a
/// tree-sitter pre-write check rejects model-generated code. The
/// tool dispatcher detects this prefix in error-result content so
/// it can arm escalation WITHOUT the individual tools needing a
/// reference to `LoopConfig`. Keep in sync with
/// `syntax_validator::format_errors`.
pub(crate) const SYNTAX_CHECK_PREFIX: &str = "Syntax check failed for ";

/// Phase 4 part 1: arm the dual-client escalation for the NEXT
/// stream call. Decrements `escalation_remaining`; no-ops if the
/// budget is exhausted (logs at debug). If escalation is unarmed
/// (no `escalation_stream_fn`), the budget is still decremented —
/// this is intentional: a misconfigured session shouldn't pretend
/// to have unlimited escalation budget, and `stream_assistant_response`
/// will simply observe `pending=Some, escalation_stream_fn=None` and
/// fall back to the default stream.
pub(crate) fn try_arm_escalation(config: &LoopConfig, reason: EscalationReason) {
    use std::sync::atomic::Ordering;
    // Try to decrement the budget. `fetch_update` lets us peek-and-
    // decrement atomically; if it returns Err, the budget is zero
    // and we no-op.
    let res = config
        .escalation_remaining
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            if v == 0 { None } else { Some(v - 1) }
        });
    if res.is_err() {
        tracing::debug!(
            target: "dirge::agent_loop::escalation",
            cap = %config.escalation_max_per_session,
            "escalation budget exhausted; skipping arm",
        );
        return;
    }
    if let Ok(mut guard) = config.escalation_pending.lock() {
        tracing::debug!(
            target: "dirge::agent_loop::escalation",
            reason = ?reason,
            "escalation armed for next LLM call",
        );
        *guard = Some(reason);
    }
}

/// Batch return shape. Port of pi `ExecutedToolCallBatch`
/// (agent-loop.ts:390-393).
#[derive(Debug, Clone)]
pub struct ExecutedToolCallBatch {
    /// Tool-result messages to append to the transcript. Order
    /// matches the source order of the assistant's `toolCall`
    /// blocks (pi: this is true for parallel via the
    /// `orderedFinalizedCalls` re-emit in source order at
    /// agent-loop.ts:506-510; for sequential the iteration order
    /// IS the source order).
    pub messages: Vec<ToolResultMessage>,

    /// Early-termination signal. Pi semantics: TRUE iff every
    /// finalized result has `terminate == true` AND the batch
    /// is non-empty (`shouldTerminateToolBatch` at line 544).
    pub terminate: bool,
}

/// One tool call extracted from an assistant message. Port of pi
/// `AgentToolCall` (types.ts:47). Concrete struct rather than
/// reference to keep the dispatcher's data flow plain.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Internal: outcome of `prepare_tool_call`. Port of pi's
/// `PreparedToolCall | ImmediateToolCallOutcome` union
/// (agent-loop.ts:518-540).
enum PrepareOutcome {
    /// Tool found and validated; ready for execute.
    Prepared {
        tool: Arc<dyn LoopTool>,
        args: Value,
        /// Phase-2: repair-pass notes (e.g. "offset defaulted to
        /// 0 …") prepended to the tool result content so the
        /// model sees how its inputs were augmented. Empty for
        /// tools that didn't trigger any relational defaults.
        notes: Vec<String>,
    },
    /// Short-circuit error: tool missing, schema rejected,
    /// signal aborted, or beforeToolCall blocked.
    Immediate {
        result: LoopToolResult,
        is_error: bool,
    },
}

/// Internal: outcome of `execute_prepared_tool_call`. Port of pi
/// `ExecutedToolCallOutcome` (agent-loop.ts:531-534).
struct ExecutedOutcome {
    result: LoopToolResult,
    is_error: bool,
}

/// Internal: outcome of `finalize_executed_tool_call`. Port of pi
/// `FinalizedToolCallOutcome` (agent-loop.ts:536-540).
#[derive(Debug, Clone)]
struct FinalizedOutcome {
    tool_call: ToolCall,
    result: LoopToolResult,
    is_error: bool,
}

/// Execute a batch of tool calls SEQUENTIALLY. Faithful port of
/// pi `executeToolCallsSequential` (agent-loop.ts:395-449).
///
/// Per-iteration:
///   1. emit `tool_execution_start`
///   2. prepare (lookup + prepareArguments + validate + before)
///   3. execute (if prepared) + finalize (afterToolCall)
///   4. emit `tool_execution_end`
///   5. emit `message_start` / `message_end` for the tool-result
///      message
///   6. if signal aborted: break
pub async fn execute_tool_calls_sequential(
    context: &Context,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &LoopConfig,
    signal: &AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    inflight: &InflightSet,
) -> ExecutedToolCallBatch {
    let mut finalized_calls: Vec<FinalizedOutcome> = Vec::with_capacity(tool_calls.len());
    let mut messages: Vec<ToolResultMessage> = Vec::with_capacity(tool_calls.len());

    for tool_call in tool_calls {
        // 1. tool_execution_start
        let _ = emit
            .send(LoopEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            })
            .await;

        // 2. prepare
        let prepared =
            prepare_tool_call(context, assistant_message, tool_call, config, signal).await;

        // 3. execute + finalize
        let finalized = match prepared {
            PrepareOutcome::Immediate { result, is_error } => FinalizedOutcome {
                tool_call: tool_call.clone(),
                result,
                is_error,
            },
            PrepareOutcome::Prepared { tool, args, notes } => {
                // LOOP-5: RAII guard ensures the inflight id is
                // removed even on cancellation / panic / `?`-bail.
                let _inflight = inflight.guard(&tool_call.id);
                let executed =
                    execute_prepared_tool_call(&tool, tool_call, &args, signal, emit).await;
                let mut finalized = finalize_executed_tool_call(
                    context,
                    assistant_message,
                    tool_call,
                    &args,
                    executed,
                    config,
                )
                .await;
                // Phase-2: prepend repair notes (e.g. relational
                // defaults) to the tool result content so the
                // model sees the auto-fill in the same turn.
                prepend_notes_to_result(&mut finalized.result, &notes);
                // Phase 4 part 1: detect tree-sitter syntactic
                // failure and arm escalation so the next LLM call
                // routes through the configured escalation
                // provider.
                maybe_arm_escalation_for_syntactic_failure(
                    config,
                    tool_call,
                    &finalized.result,
                    finalized.is_error,
                );
                finalized
                // _inflight dropped here → inflight.delete fires.
            }
        };

        // 4. tool_execution_end
        emit_tool_execution_end(&finalized, emit).await;

        // 5. tool-result message
        let result_msg = create_tool_result_message(&finalized);
        emit_tool_result_message(&result_msg, emit).await;

        finalized_calls.push(finalized);
        messages.push(result_msg);

        // 6. honor signal
        if signal.is_cancelled() {
            break;
        }
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    }
}

/// Lookup tool, run `prepareArguments`, validate (TODO phase 3),
/// run `beforeToolCall`. Faithful port of pi `prepareToolCall`
/// (agent-loop.ts:562-626).
///
/// Important deviation from pi: phase 2 does NOT JSON-schema-
/// validate args. Pi calls `validateToolArguments(tool, toolCall)`
/// at line 580; we skip that step because dirge has no embedded
/// JSON-Schema validator (rig tools self-parse via serde). A
/// future phase can add a validator if a real schema-mismatch
/// case surfaces — for now any deserialization mismatch surfaces
/// from the tool's `execute` as a normal error.
async fn prepare_tool_call(
    context: &Context,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    config: &LoopConfig,
    signal: &AbortSignal,
) -> PrepareOutcome {
    // Find the tool by name. Pi line 569.
    let tool = match context.tools.iter().find(|t| t.name() == tool_call.name) {
        Some(t) => t.clone(),
        None => {
            return PrepareOutcome::Immediate {
                result: create_error_tool_result(&format!("Tool {} not found", tool_call.name)),
                is_error: true,
            };
        }
    };

    // prepareArguments compat shim. Pi line 579.
    let prepared_args = tool.prepare_arguments(tool_call.arguments.clone());

    // Schema validate + repair. Pi line 580.
    // Validate-then-repair semantics: valid inputs are never touched.
    // On validation failure, apply targeted repairs for the four
    // common open-model shape mistakes (null-for-optional,
    // JSON-string-as-array, {}-to-[], bare-string-to-array).
    // Phase-2: also collects relational-defaults `notes` to
    // prepend to the tool result so the model sees the auto-fill.
    let mut repair_notes: Vec<String> = Vec::new();
    let mut validated_args = match crate::agent::agent_loop::tool_input_repair::validate_and_repair(
        tool.parameters(),
        &prepared_args,
    ) {
        Ok(None) => {
            // Valid input — pass through untouched.
            prepared_args
        }
        Ok(Some(rr)) => {
            // LOOP-2: log the original args alongside the repair
            // kinds so a future audit can see exactly what the
            // model emitted vs what the loop dispatched. Truncate
            // the original to keep telemetry rows bounded —
            // multi-MB tool-call args are unusual but possible.
            let original_args = serde_json::to_string(&prepared_args).unwrap_or_default();
            let original_truncated: String = if original_args.len() > 4096 {
                format!(
                    "{}... ({} bytes truncated)",
                    &original_args[..4096],
                    original_args.len() - 4096
                )
            } else {
                original_args
            };
            tracing::info!(
                target: "tool_repair",
                model = config.model_name.as_deref().unwrap_or("unknown"),
                tool = %tool_call.name,
                repair = ?rr.kinds,
                original_args = %original_truncated,
                "tool input repaired"
            );
            // Phase-1 telemetry: bump the per-kind aggregate
            // counters once per call, not once per occurrence.
            // The repair pass can push the same kind multiple
            // times (e.g. `NullStripped` once per stripped null
            // field) — counting per-field would inflate the
            // session summary into "repaired N inputs" when only
            // one tool call actually had inputs touched. Dedupe
            // by kind so the user-visible counter is "tool calls
            // touched by this repair", which is the meaningful
            // metric. The full kinds vec is still passed to the
            // tracing event for per-call detail.
            let mut seen_kinds: std::collections::HashSet<
                crate::agent::agent_loop::tool_input_repair::RepairKind,
            > = std::collections::HashSet::new();
            for kind in &rr.kinds {
                if seen_kinds.insert(*kind) {
                    config.repair_stats.record(*kind);
                }
            }
            // Phase-2: carry relational-default notes forward
            // so the dispatcher can prepend them to the tool
            // result content.
            repair_notes = rr.notes;
            rr.repaired
        }
        Err(errors) => {
            let msg = crate::agent::agent_loop::tool_input_repair::format_structured_error(
                tool.parameters(),
                &prepared_args,
                &errors,
            );
            // Phase-1 telemetry: keep the original (truncated to
            // 16 KiB so an adversarial payload can't blow the log
            // ring) so the failure can be inspected offline. This
            // is the "tool_input_invalid" event from
            // agentic-features.md §2.6 — split out from the
            // generic `tool_repair = "failed"` log so structured-
            // log consumers can filter on it directly.
            let original_args = serde_json::to_string(&prepared_args).unwrap_or_default();
            let original_truncated: String = if original_args.len() > 16384 {
                format!(
                    "{}... ({} bytes truncated)",
                    &original_args[..16384],
                    original_args.len() - 16384
                )
            } else {
                original_args
            };
            tracing::warn!(
                target: "tool_input_invalid",
                model = config.model_name.as_deref().unwrap_or("unknown"),
                tool = %tool_call.name,
                validation_errors = ?errors,
                original_args = %original_truncated,
                "tool input invalid after repair pass"
            );
            config.repair_stats.record_invalid();
            // Phase 4 part 1: repair exhausted — arm escalation
            // for the next LLM call so a stronger model gets a
            // chance to emit valid tool args.
            try_arm_escalation(
                config,
                EscalationReason::RepairExhausted {
                    tool: tool_call.name.clone(),
                },
            );
            return PrepareOutcome::Immediate {
                result: create_error_tool_result(&msg),
                is_error: true,
            };
        }
    };

    // dirge-7bwx review-fix #2: drain truncation-repair notes
    // for this call_id and append to repair_notes. The loop-level
    // closer (`apply_truncation_repair` in `run.rs`) ran before
    // dispatch; its per-call notes live keyed by call_id on
    // `config.truncation_notes`. Reasonix surfaces these in
    // `report.notes` (`repair/index.ts:100-101, :106`); we attach
    // them to the per-call result so the model sees the repair in
    // the same turn rather than waiting for next-turn context.
    {
        let mut sink = config
            .truncation_notes
            .lock()
            .expect("truncation_notes poisoned");
        if let Some(notes) = sink.remove(&tool_call.id) {
            repair_notes.extend(notes);
        }
    }

    // beforeToolCall. Pi lines 581-605.
    if let Some(hook) = &config.before_tool_call {
        let hook_ctx = BeforeToolCallContext {
            assistant_message: assistant_message.clone(),
            tool_call_id: tool_call.id.clone(),
            tool_call_name: tool_call.name.clone(),
            args: validated_args.clone(),
        };
        let ret = hook(hook_ctx).await;
        // The hook may mutate args via the returned `args` field.
        // Thread it forward to execute. Pi mutates in-place; we
        // pass by value (documented in hooks.rs).
        validated_args = ret.args;

        if signal.is_cancelled() {
            return PrepareOutcome::Immediate {
                result: create_error_tool_result("Operation aborted"),
                is_error: true,
            };
        }
        if let Some(before_result) = ret.result
            && before_result.block.unwrap_or(false)
        {
            let reason = before_result
                .reason
                .unwrap_or_else(|| "Tool execution was blocked".to_string());
            return PrepareOutcome::Immediate {
                result: create_error_tool_result(&reason),
                is_error: true,
            };
        }
    }

    // Final signal check before returning prepared. Pi lines
    // 606-612.
    if signal.is_cancelled() {
        return PrepareOutcome::Immediate {
            result: create_error_tool_result("Operation aborted"),
            is_error: true,
        };
    }

    // Phase 4 part 2: file-touch tracker — record this prepared
    // tool call's args BEFORE dispatch so the streak counter is
    // up-to-date by the next steering-poll point.
    if let Some(tracker) = &config.file_touch_tracker {
        tracker.record_tool_call(&tool_call.name, &validated_args);
    }

    PrepareOutcome::Prepared {
        tool,
        args: validated_args,
        notes: repair_notes,
    }
}

/// Execute a prepared tool call. Faithful port of pi
/// `executePreparedToolCall` (agent-loop.ts:628-663).
///
/// The tool's `on_update` callback emits `tool_execution_update`
/// events. Pi awaits all the update emits via
/// `Promise.all(updateEvents)`; we let them flow into the mpsc
/// channel as the tool calls them (`send().await` orders writes
/// per-channel anyway).
async fn execute_prepared_tool_call(
    tool: &Arc<dyn LoopTool>,
    tool_call: &ToolCall,
    args: &Value,
    signal: &AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
) -> ExecutedOutcome {
    // Build the on_update callback. Pi captures these via
    // `updateEvents` promise list (agent-loop.ts:633, 641-652).
    // We forward directly through the mpsc channel — same
    // ordering semantics since tokio channels are FIFO.
    let emit_clone = emit.clone();
    let id_clone = tool_call.id.clone();
    let name_clone = tool_call.name.clone();
    let args_clone = tool_call.arguments.clone();
    // LOOP-11: track dropped updates via an atomic counter so
    // they're visible in tracing instead of silently lost. The
    // tool itself shouldn't block on UI delivery — that would
    // back-pressure the model — so we keep `try_send` semantics
    // and just record the drop. The counter is per-tool-call;
    // the warning fires on first drop and the count is logged
    // once at the end of dispatch.
    let dropped_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dropped_clone = dropped_count.clone();
    let on_update: LoopToolUpdate = Arc::new(move |partial: &LoopToolResult| {
        // `try_send` rather than `.send().await` because the
        // callback is sync — pi's callback is sync too. If the
        // channel is closed/full, increment the dropped counter
        // (LOOP-11) rather than silently losing the event.
        let evt = LoopEvent::ToolExecutionUpdate {
            tool_call_id: id_clone.clone(),
            tool_name: name_clone.clone(),
            args: args_clone.clone(),
            partial_result: partial.clone(),
        };
        if emit_clone.try_send(evt).is_err() {
            let prev = dropped_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if prev == 0 {
                tracing::warn!(
                    target: "dirge::agent_loop::tools",
                    tool = %name_clone,
                    tool_call_id = %id_clone,
                    "ToolExecutionUpdate channel full or closed; dropping update events",
                );
            }
        }
    });

    // Phase 6 — make the tool dispatch responsive to
    // `AbortSignal` even when the underlying tool doesn't
    // poll the signal itself. We race the tool's execute()
    // against `wait_for_cancel` (instant, Notify-backed):
    //   - If the tool finishes first → use its result.
    //   - If the signal fires first → return an aborted
    //     result and DROP the tool's future. Dropping a
    //     future cancels it: it won't be polled again, its
    //     in-progress `.await`s unwind, and its RAII guards
    //     run — so e.g. bash's `PgKillGuard` SIGKILLs the
    //     whole process group (not just the immediate child)
    //     on the drop path. A partial side effect already
    //     committed before the drop (a half-written file)
    //     can't be undone — that's inherent to cancelling
    //     mid-operation.
    //
    // Caveat: a tool that detaches work via `tokio::spawn`
    // (not tied to its own future's lifetime) is responsible
    // for aborting that itself — dropping the execute future
    // can't reach a detached task.
    //
    // Tools that ALSO poll `is_cancelled()` get even cleaner
    // cancellation since they bail at their next checkpoint
    // rather than relying solely on the drop.
    let exec_future = tool.execute(&tool_call.id, args.clone(), signal.clone(), on_update);
    let signal_check = wait_for_cancel(signal.clone());

    let outcome = tokio::select! {
        biased;  // Prefer the cancel branch when both are ready
                 // — so a cancel that fires during a fast tool
                 // doesn't get masked by the tool's completion.
        _ = signal_check => {
            // Signal fired before the tool finished. Return an
            // aborted result; the loop's next signal check at
            // its turn boundary will exit cleanly.
            return ExecutedOutcome {
                result: create_error_tool_result(
                    "tool execution aborted by cancellation signal",
                ),
                is_error: true,
            };
        }
        result = exec_future => result,
    };

    // LOOP-11: surface the final dropped-update count if any
    // backpressure happened during execution. Logged at INFO so
    // diagnostics flow shows up under --verbose without flooding
    // normal output.
    let final_dropped = dropped_count.load(std::sync::atomic::Ordering::Relaxed);
    if final_dropped > 0 {
        tracing::info!(
            target: "dirge::agent_loop::tools",
            tool = %tool_call.name,
            tool_call_id = %tool_call.id,
            dropped = final_dropped,
            "ToolExecutionUpdate events dropped during tool execution",
        );
    }
    match outcome {
        Ok(result) => ExecutedOutcome {
            result,
            is_error: false,
        },
        Err(err) => ExecutedOutcome {
            result: create_error_tool_result(&err),
            is_error: true,
        },
    }
}

/// Finalize an executed tool result via `afterToolCall`. Faithful
/// port of pi `finalizeExecutedToolCall` (agent-loop.ts:665-708).
///
/// Merge semantics (pi lines 689-695): each Some field of
/// `AfterToolCallResult` REPLACES the executed result's
/// corresponding field IN FULL. Omitted (None) fields keep the
/// original.
async fn finalize_executed_tool_call(
    context: &Context,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    args: &Value,
    executed: ExecutedOutcome,
    config: &LoopConfig,
) -> FinalizedOutcome {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(hook) = &config.after_tool_call {
        let hook_ctx = AfterToolCallContext {
            assistant_message: assistant_message.clone(),
            tool_call_id: tool_call.id.clone(),
            tool_call_name: tool_call.name.clone(),
            args: args.clone(),
            result: result.clone(),
            is_error,
        };
        // Pi catches hook errors and turns them into an error
        // tool result (agent-loop.ts:697-700). Our hook signature
        // doesn't have a Result return — closures that want to
        // signal errors do so via the `is_error` field. If a
        // future hook impl needs throw-and-catch behaviour we
        // extend the signature.
        if let Some(after) = hook(hook_ctx).await {
            result = LoopToolResult {
                content: after.content.unwrap_or(result.content),
                details: after.details.unwrap_or(result.details),
                terminate: after.terminate.or(result.terminate),
            };
            is_error = after.is_error.unwrap_or(is_error);
        }
    }

    // `context` is unused for now (pi passes it for symmetry with
    // beforeToolCall). Marker-binding to silence the warning until
    // a future hook impl uses it.
    let _ = context;

    // F6 (tier 2): feed the verifier gate the finished call + its result
    // so it knows, at finalization, whether code was edited and whether a
    // build/test command passed or failed. Post-execution (here, not at
    // prepare) is the only place the outcome is known.
    if let Some(verifier) = &config.verifier {
        verifier.record_outcome(&tool_call.name, args, &result, is_error);
    }

    FinalizedOutcome {
        tool_call: tool_call.clone(),
        result,
        is_error,
    }
}

/// `shouldTerminateToolBatch`: empty batch → false; otherwise
/// true iff EVERY result has `terminate == true`. Faithful port
/// of pi line 544.
fn should_terminate_tool_batch(finalized: &[FinalizedOutcome]) -> bool {
    !finalized.is_empty()
        && finalized
            .iter()
            .all(|f| f.result.terminate.unwrap_or(false))
}

/// Wait for an `AbortSignal` to fire. Resolves the instant the
/// signal is cancelled — `AbortSignal::cancelled` is `Notify`-backed,
/// so there's no polling latency. The caller races this against the
/// tool's execute() in a `tokio::select!`; when the tool arm wins,
/// this future is simply dropped.
async fn wait_for_cancel(signal: AbortSignal) {
    signal.cancelled().await;
}

/// Phase 4 part 1: scan an error tool-result for the canonical
/// tree-sitter syntax-check failure prefix; arm escalation if
/// found.
///
/// Tools (`write` / `edit` / `apply_patch`) return their syntax-
/// failure as an `Err(String)` whose message starts with
/// `SYNTAX_CHECK_PREFIX`. The dispatcher converts that into an
/// error result via `create_error_tool_result(&err)`, which wraps
/// the string in a `{type: "text", text: ...}` block. We inspect
/// that block here without coupling the individual tools to
/// `LoopConfig`.
pub(crate) fn maybe_arm_escalation_for_syntactic_failure(
    config: &LoopConfig,
    tool_call: &ToolCall,
    result: &LoopToolResult,
    is_error: bool,
) {
    if !is_error {
        return;
    }
    let text = result.content.iter().find_map(|b| {
        let obj = b.as_object()?;
        if obj.get("type").and_then(|t| t.as_str()) == Some("text") {
            obj.get("text").and_then(|t| t.as_str())
        } else {
            None
        }
    });
    let text = match text {
        Some(t) => t,
        None => return,
    };
    if !text.starts_with(SYNTAX_CHECK_PREFIX) {
        return;
    }
    // Extract the path between the prefix and the trailing ": N error(s)…".
    let after = &text[SYNTAX_CHECK_PREFIX.len()..];
    let path = match after.find(':') {
        Some(i) => after[..i].to_string(),
        None => String::new(),
    };
    try_arm_escalation(
        config,
        EscalationReason::SyntacticFailure {
            tool: tool_call.name.clone(),
            path,
        },
    );
}

/// Build the "tool not found" / "operation aborted" / "blocked"
/// error result. Port of pi `createErrorToolResult` (line 710).
fn create_error_tool_result(message: &str) -> LoopToolResult {
    LoopToolResult {
        content: vec![serde_json::json!({"type": "text", "text": message})],
        details: serde_json::json!({}),
        terminate: None,
    }
}

/// Phase-2: prepend repair-pass notes (e.g. relational-defaults
/// "offset defaulted to 0") to a tool result's text content so
/// the model sees them in the same turn it dispatched the call.
/// No-op when `notes` is empty.
///
/// The notes are inserted as a SEPARATE leading text block when
/// possible, OR prepended to the first text block. Non-text
/// results (image-only, structured-only) get a fresh text block
/// inserted at index 0.
fn prepend_notes_to_result(result: &mut LoopToolResult, notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    let joined = notes.join("\n");
    let note_block = serde_json::json!({"type": "text", "text": joined});
    result.content.insert(0, note_block);
}

/// Emit the `tool_execution_end` event. Port of pi line 717.
async fn emit_tool_execution_end(finalized: &FinalizedOutcome, emit: &mpsc::Sender<LoopEvent>) {
    let _ = emit
        .send(LoopEvent::ToolExecutionEnd {
            tool_call_id: finalized.tool_call.id.clone(),
            tool_name: finalized.tool_call.name.clone(),
            result: finalized.result.clone(),
            is_error: finalized.is_error,
        })
        .await;
}

/// Build the `ToolResultMessage` artifact appended to the
/// transcript. Port of pi `createToolResultMessage` (line 727).
fn create_tool_result_message(finalized: &FinalizedOutcome) -> ToolResultMessage {
    // Pi shape: { role, toolCallId, toolName, content, details,
    // isError, timestamp }. Our LoopToolResult.content is
    // `Vec<Value>` (the raw blocks pi calls TextContent /
    // ImageContent); we need to map them to ContentBlock for the
    // message. Phase 1 represented blocks as either typed
    // ContentBlock variants OR raw Value depending on the path;
    // phase 2 unifies via a best-effort parse: if a block has
    // `type: "text"` we recognise it, else we wrap as raw text
    // with debug string.
    let content_blocks: Vec<ContentBlock> = finalized
        .result
        .content
        .iter()
        .map(content_value_to_block)
        .collect();

    ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: content_blocks,
        details: finalized.result.details.clone(),
        is_error: finalized.is_error,
    }
}

fn content_value_to_block(value: &Value) -> ContentBlock {
    // The single tool-result boundary: every result block is scrubbed
    // for credential-shaped substrings (dirge-tkyn) before it reaches
    // the LLM context, the persisted transcript, or the UI — so a
    // command like `cat .env` / `echo $API_KEY` can't leak a secret.
    // Recognise pi's `{type: "text", text: "..."}` shape.
    if let Some(obj) = value.as_object()
        && obj.get("type").and_then(|t| t.as_str()) == Some("text")
        && let Some(text) = obj.get("text").and_then(|t| t.as_str())
    {
        return ContentBlock::Text {
            text: crate::sandbox::redact_secrets(text).into_owned(),
        };
    }
    // Fallback: stringify the value. Better than dropping data.
    ContentBlock::Text {
        text: crate::sandbox::redact_secrets(&value.to_string()).into_owned(),
    }
}

/// Emit the message_start + message_end pair for the tool-result
/// message. Port of pi `emitToolResultMessage` (line 739).
async fn emit_tool_result_message(msg: &ToolResultMessage, emit: &mpsc::Sender<LoopEvent>) {
    let _ = emit
        .send(LoopEvent::MessageStart {
            message: LoopMessage::ToolResult(msg.clone()),
        })
        .await;
    let _ = emit
        .send(LoopEvent::MessageEnd {
            message: LoopMessage::ToolResult(msg.clone()),
        })
        .await;
}

/// Execute a batch of tool calls IN PARALLEL. Faithful port of
/// pi `executeToolCallsParallel` (agent-loop.ts:451-516).
///
/// Key invariants pi enforces and this port preserves:
///
/// 1. **Preflight is sequential** — `prepare_tool_call` runs
///    in source order for every call. Pi tests beforeToolCall
///    hook ordering at line 469.
///
/// 2. **Immediate outcomes finalize sync** — errors from
///    prepare (tool not found / blocked / aborted) skip the
///    parallel-execute machinery entirely. They emit
///    `tool_execution_end` IMMEDIATELY (before any prepared
///    lambda runs).
///
/// 3. **Prepared outcomes become async lambdas** — each
///    lambda's `tool_execution_end` event fires AT COMPLETION
///    (inside the lambda), so end events arrive in COMPLETION
///    order, not source order.
///
/// 4. **`tool_execution_end` events: completion order** — this is
///    what pi:452 verifies. A slow tool at source position 1 +
///    a fast tool at source position 2 produces end events
///    `[tool-2, tool-1]`.
///
/// 5. **Tool-result `message_start`/`message_end` events: source
///    order** — emitted AFTER all lambdas resolve via
///    `Promise.all` (pi line 502, `orderedFinalizedCalls`). Pi
///    iterates THAT array (source-ordered) to emit messages.
///    pi:452 also verifies this — tool-result message_end IDs
///    `[tool-1, tool-2]`.
///
/// 6. **Signal abort short-circuits the prepare loop** but
///    leaves already-queued lambdas to complete (pi lines
///    478-480, 497-499 — the `break` is after pushing).
pub async fn execute_tool_calls_parallel(
    context: &Context,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &LoopConfig,
    signal: &AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    inflight: &InflightSet,
) -> ExecutedToolCallBatch {
    use futures::future::join_all;
    use std::pin::Pin;

    type ResolveFuture = Pin<Box<dyn Future<Output = FinalizedOutcome> + Send>>;

    let mut entries: Vec<ResolveFuture> = Vec::with_capacity(tool_calls.len());

    // Phase 1: preflight — sequentially prepare each call.
    for tool_call in tool_calls {
        // 1. Emit tool_execution_start. Pi line 462.
        let _ = emit
            .send(LoopEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            })
            .await;

        let prepared =
            prepare_tool_call(context, assistant_message, tool_call, config, signal).await;

        match prepared {
            PrepareOutcome::Immediate { result, is_error } => {
                // Pi line 470-481: immediate finalize, emit end NOW,
                // push the finalized value (not a future).
                let finalized = FinalizedOutcome {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                };
                emit_tool_execution_end(&finalized, emit).await;
                entries.push(Box::pin(futures::future::ready(finalized)));
                if signal.is_cancelled() {
                    break;
                }
            }
            PrepareOutcome::Prepared { tool, args, notes } => {
                // Pi lines 484-496: push an async lambda that
                // executes, finalizes, AND emits its
                // tool_execution_end at the end. The
                // tool_execution_end ordering THEREFORE matches
                // completion order, not source order.
                //
                // LOOP-5: use the Drop-guard form so a cancel/panic
                // that aborts the future doesn't leak the inflight
                // id (which would leave the UI spinner stuck).
                let tool_call_clone = tool_call.clone();
                let assistant_clone = assistant_message.clone();
                let config_clone = config.clone();
                let context_clone = context.clone();
                let signal_clone = signal.clone();
                let emit_clone = emit.clone();
                let inflight_clone = inflight.clone();
                let call_id = tool_call.id.clone();
                entries.push(Box::pin(async move {
                    let _guard = inflight_clone.guard(&call_id);
                    let executed = execute_prepared_tool_call(
                        &tool,
                        &tool_call_clone,
                        &args,
                        &signal_clone,
                        &emit_clone,
                    )
                    .await;
                    let mut finalized = finalize_executed_tool_call(
                        &context_clone,
                        &assistant_clone,
                        &tool_call_clone,
                        &args,
                        executed,
                        &config_clone,
                    )
                    .await;
                    // Phase-2: prepend repair notes so the model
                    // sees them in the same tool result.
                    prepend_notes_to_result(&mut finalized.result, &notes);
                    // Phase 4 part 1: detect tree-sitter syntactic
                    // failure and arm escalation for the next LLM
                    // call. Uses the SAME config Arc state as the
                    // sequential path — the cloned LoopConfig
                    // shares `escalation_pending` / `escalation_remaining`.
                    maybe_arm_escalation_for_syntactic_failure(
                        &config_clone,
                        &tool_call_clone,
                        &finalized.result,
                        finalized.is_error,
                    );
                    // Emit end AT COMPLETION. This is the key
                    // difference from sequential (which emits
                    // end immediately after each call).
                    emit_tool_execution_end(&finalized, &emit_clone).await;
                    // _guard dropped here → inflight.delete fires.
                    finalized
                }));
                if signal.is_cancelled() {
                    break;
                }
            }
        }
    }

    // Phase 2: await all lambdas concurrently. `join_all`
    // preserves input ORDER — the resulting Vec is in source
    // order even though completion order may differ. Pi uses
    // `Promise.all` with the same semantics.
    let finalized: Vec<FinalizedOutcome> = join_all(entries).await;

    // Phase 3: emit tool-result message_start + message_end IN
    // SOURCE ORDER. Pi lines 502-510 — iterate the
    // source-ordered array.
    let mut messages: Vec<ToolResultMessage> = Vec::with_capacity(finalized.len());
    for f in &finalized {
        let msg = create_tool_result_message(f);
        emit_tool_result_message(&msg, emit).await;
        messages.push(msg);
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized),
    }
}

/// Umbrella dispatcher. Picks sequential vs parallel based on:
///   - `config.tool_execution == Sequential` → sequential
///   - ANY tool in the batch has `execution_mode == Sequential` →
///     sequential (forces the WHOLE batch sequential — pi at
///     line 381 `hasSequentialToolCall`)
///   - otherwise → parallel
///
/// dirge-tc4r: synthesize an error tool-result for every `tool_call_id`
/// in `calls` that has NO matching result in `results`.
///
/// An assistant message keeps ALL of its `tool_calls`, but the storm
/// breaker can suppress SOME of them (a partial multi-call batch), and a
/// cancelled / interrupted batch stops early — either way fewer results
/// get appended than there were calls, leaving an orphaned
/// `tool_call_id`. The next provider request then 400s ("an assistant
/// message with 'tool_calls' must be followed by tool messages responding
/// to each 'tool_call_id'"). Backfilling a synthetic error result keeps
/// the transcript well-formed (no 400) AND shows the model the gap (an
/// error it can react to) instead of throwing the raw provider error at
/// the user. Pure — unit-tested.
pub(crate) fn backfill_missing_tool_results(
    calls: &[ToolCall],
    results: &[ToolResultMessage],
) -> Vec<ToolResultMessage> {
    let answered: std::collections::HashSet<&str> =
        results.iter().map(|r| r.tool_call_id.as_str()).collect();
    calls
        .iter()
        .filter(|c| !answered.contains(c.id.as_str()))
        .map(|c| ToolResultMessage {
            tool_call_id: c.id.clone(),
            tool_name: c.name.clone(),
            content: vec![ContentBlock::Text {
                text: "[tool call not executed: it was suppressed as a repeated/looping call, or \
                       the run was interrupted. Do NOT repeat it — try a different approach.]"
                    .to_string(),
            }],
            details: Value::Null,
            is_error: true,
        })
        .collect()
}

/// Faithful port of pi `executeToolCalls` (agent-loop.ts:370-388).
pub async fn execute_tool_calls(
    context: &Context,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &LoopConfig,
    signal: &AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    inflight: &InflightSet,
) -> ExecutedToolCallBatch {
    let has_sequential = tool_calls.iter().any(|tc| {
        context
            .tools
            .iter()
            .find(|t| t.name() == tc.name)
            .and_then(|t| t.execution_mode())
            == Some(super::types::ToolExecutionMode::Sequential)
    });
    if config.tool_execution == super::types::ToolExecutionMode::Sequential || has_sequential {
        execute_tool_calls_sequential(
            context,
            assistant_message,
            tool_calls,
            config,
            signal,
            emit,
            inflight,
        )
        .await
    } else {
        execute_tool_calls_parallel(
            context,
            assistant_message,
            tool_calls,
            config,
            signal,
            emit,
            inflight,
        )
        .await
    }
}

/// Convenience: extract tool calls from the assistant message, then
/// dispatch through [`execute_tool_calls`].
#[cfg(test)]
pub async fn execute_tool_calls_from_msg(
    context: &Context,
    assistant_message: &AssistantMessage,
    config: &LoopConfig,
    signal: &AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    inflight: &InflightSet,
) -> ExecutedToolCallBatch {
    let tool_calls = extract_tool_calls(assistant_message);
    execute_tool_calls(
        context,
        assistant_message,
        &tool_calls,
        config,
        signal,
        emit,
        inflight,
    )
    .await
}

/// Extract `ToolCall`s from an assistant message's content. Port
/// of pi line 380 `message.content.filter((c) => c.type ===
/// "toolCall")` adapted to our typed enum.
pub fn extract_tool_calls(msg: &AssistantMessage) -> Vec<ToolCall> {
    msg.content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Some(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

// =====================================================================
// Tests — ported from pi/test/agent-loop.test.ts
// Inlined tests were extracted to the sibling `tools_tests.rs` file;
// `#[path = "..."]` pulls it in as the `tests` child module so the
// `use super::*` references inside continue to resolve.
// =====================================================================

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
