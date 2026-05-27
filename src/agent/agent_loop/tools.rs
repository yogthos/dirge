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
use super::message::{AssistantMessage, ContentBlock, LoopEvent, LoopMessage, ToolResultMessage};
use super::result::LoopToolResult;
use super::tool::{AbortSignal, LoopTool, LoopToolUpdate};
use super::types::{Context, LoopConfig};

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
            PrepareOutcome::Prepared { tool, args } => {
                // LOOP-5: RAII guard ensures the inflight id is
                // removed even on cancellation / panic / `?`-bail.
                let _inflight = inflight.guard(&tool_call.id);
                let executed =
                    execute_prepared_tool_call(&tool, tool_call, &args, signal, emit).await;
                finalize_executed_tool_call(
                    context,
                    assistant_message,
                    tool_call,
                    &args,
                    executed,
                    config,
                )
                .await
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
            rr.repaired
        }
        Err(errors) => {
            let msg = crate::agent::agent_loop::tool_input_repair::format_structured_error(
                tool.parameters(),
                &prepared_args,
                &errors,
            );
            tracing::info!(
                target: "tool_repair",
                model = config.model_name.as_deref().unwrap_or("unknown"),
                tool = %tool_call.name,
                repair = "failed",
                "tool input repair failed"
            );
            return PrepareOutcome::Immediate {
                result: create_error_tool_result(&msg),
                is_error: true,
            };
        }
    };

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

    PrepareOutcome::Prepared {
        tool,
        args: validated_args,
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
    // against a signal-poll loop:
    //   - If the tool finishes first → use its result.
    //   - If the signal fires first → return an aborted
    //     result. The tool's future is dropped, which
    //     stops further poll progress. Side effects already
    //     run (e.g. an in-flight bash process) are NOT
    //     killed — the rig::Tool surface doesn't expose
    //     cancellation, and forcing a kill would risk
    //     orphaned state. The dropped future may continue
    //     scheduling work in the background; consumers
    //     should not rely on its absence.
    //
    // This gives the loop UX responsive to Ctrl+C even with
    // legacy tools that don't poll the signal. Tools that
    // DO poll it (a future generation of LoopTool impls)
    // get cleaner cancellation since they finish quickly
    // when cancelled.
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

/// Wait for an `AbortSignal` to fire. Polls at 50ms intervals
/// since `AbortSignal` doesn't expose an async-await primitive
/// (it's an `Arc<AtomicBool>` wrapper). 50ms gives a snappy UX
/// (user-perceptible Ctrl+C response) without busy-looping.
///
/// Returns when the signal is cancelled. The caller races this
/// future against the tool's execute() in a `tokio::select!`.
/// Cancellation of the wait future is automatic when the
/// select arm doesn't win — `tokio::time::sleep` is
/// abort-on-drop, and the `is_cancelled()` check is cheap.
async fn wait_for_cancel(signal: AbortSignal) {
    loop {
        if signal.is_cancelled() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
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
    // Recognise pi's `{type: "text", text: "..."}` shape.
    if let Some(obj) = value.as_object()
        && obj.get("type").and_then(|t| t.as_str()) == Some("text")
        && let Some(text) = obj.get("text").and_then(|t| t.as_str())
    {
        return ContentBlock::Text {
            text: text.to_string(),
        };
    }
    // Fallback: stringify the value. Better than dropping data.
    ContentBlock::Text {
        text: value.to_string(),
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
            PrepareOutcome::Prepared { tool, args } => {
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
                    let finalized = finalize_executed_tool_call(
                        &context_clone,
                        &assistant_clone,
                        &tool_call_clone,
                        &args,
                        executed,
                        &config_clone,
                    )
                    .await;
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
            &tool_calls,
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
            &tool_calls,
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
