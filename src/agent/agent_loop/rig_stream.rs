//! Phase 4.5a — adapter from rig's `StreamingCompletionResponse`
//! to our pi-style `Stream<StreamEvent>`.
//!
//! Rig's lower-level streaming API
//! (`CompletionModel::stream(request)`) yields a
//! `Stream<Result<StreamedAssistantContent<R>, CompletionError>>`.
//! Rig DOES NOT dispatch tools at this level — that's the multi-
//! turn agent's job. Single-turn raw streaming is exactly what we
//! need for our own loop to drive turns.
//!
//! This module ports the wire-level event mapping; the
//! **input-side** adapter (build `CompletionRequest` from
//! `LlmContext`) lands in a follow-up sub-phase since it touches
//! tool definitions + message-shape conversion.
//!
//! Event mapping (rig `StreamedAssistantContent<R>` → pi `StreamEvent`):
//!
//! | Rig variant                          | Pi event                          |
//! |--------------------------------------|-----------------------------------|
//! | (synthesized at stream begin)        | `Start { partial: empty msg }`    |
//! | `Text(t)`                            | `Delta { phase: TextStart/Delta }`|
//! | `Reasoning(r)` (complete block)      | `Delta { phase: ThinkingEnd }`    |
//! | `ReasoningDelta { .. }`              | `Delta { phase: ThinkingStart/Delta }`|
//! | `ToolCall { tool_call, .. }`         | `Delta { ToolCallStart + End }`   |
//! | `ToolCallDelta { content, .. }`      | `Delta { phase: ToolCallStart/Delta }`|
//! | `Final(R)`                           | (silent — captured in Done's reason)|
//! | stream end                           | `Done { reason, message }`        |
//! | `Err(CompletionError)`               | `Error { error }`                 |
//!
//! Partial-message accumulation: the adapter builds up an
//! `AssistantMessage` incrementally as deltas arrive, mirroring
//! pi's `partialMessage` in agent-loop.ts:310-340. Each `Delta`
//! event carries the running partial so consumers can render
//! incremental updates.

use std::pin::Pin;

use async_stream::stream;
use futures::Stream;
use futures::stream::StreamExt;
use rig::completion::{CompletionError, GetTokenUsage};
use rig::streaming::{StreamedAssistantContent, StreamingCompletionResponse};

use super::message::{AssistantMessage, ContentBlock, DeltaPhase, StopReason, StreamEvent};

/// Wrap a rig `StreamingCompletionResponse` as a pi-style stream
/// of `StreamEvent`s. Single-turn — rig does NOT dispatch tools
/// from this raw stream; that's our loop's job.
///
/// Algorithm:
///   1. Yield `Start { partial: empty AssistantMessage }`.
///   2. For each rig chunk, accumulate into the partial and yield
///      a `Delta { phase, partial }` event with the running state.
///   3. On stream end (no error), yield `Done { reason, message }`
///      where `message` is the final assembled `AssistantMessage`
///      and `reason` is inferred from the content (`ToolUse` iff
///      any tool call is present, else `Stop`).
///   4. On `Err(CompletionError)`, yield `Error { error }` and
///      stop.
pub fn wrap_rig_stream<R>(
    rig_stream: StreamingCompletionResponse<R>,
    chunk_timeout: Option<std::time::Duration>,
    signal: Option<crate::agent::agent_loop::tool::AbortSignal>,
) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>>
where
    R: Clone + Unpin + Send + GetTokenUsage + 'static,
{
    wrap_streamed_assistant(Box::pin(rig_stream), chunk_timeout, signal)
}

/// Lower-level variant: wrap any `Stream<Result<StreamedAssistantContent<R>,
/// CompletionError>>`. Used by tests to feed canned event
/// sequences; production callers use [`wrap_rig_stream`] directly.
///
/// **Chunk timeout** (phase 4.5h-3): if `chunk_timeout` is `Some`,
/// each `raw.next().await` is wrapped in `tokio::time::timeout`.
/// On timeout we emit an Error event with `"timed out"` in the
/// message so the existing `recovery::classify_error` substring
/// match routes it to `ErrorKind::Network` and the retry wrapper
/// picks it up. Matches the existing runner.rs:285-306 pattern
/// exactly so cross-path retry behavior is identical.
///
/// `None` disables the timeout — useful for tests, debug
/// sessions, or providers known to have long legitimate gaps
/// where the default `300s` is still too short.
pub fn wrap_streamed_assistant<R>(
    mut raw: Pin<
        Box<dyn Stream<Item = Result<StreamedAssistantContent<R>, CompletionError>> + Send>,
    >,
    chunk_timeout: Option<std::time::Duration>,
    signal: Option<crate::agent::agent_loop::tool::AbortSignal>,
) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>>
where
    R: Clone + Unpin + Send + GetTokenUsage + 'static,
{
    Box::pin(stream! {
        // Step 1: synthesize Start with an empty partial. Pi
        // expects the first event to be Start; rig doesn't emit
        // one.
        let mut partial = AssistantMessage::new(Vec::new(), StopReason::Stop);
        yield StreamEvent::Start { partial: partial.clone() };

        let mut current_text_idx: Option<usize> = None;
        let mut current_thinking_idx: Option<usize> = None;
        // Track tool calls under construction so deltas can find
        // their target content block. Keyed by rig's
        // `internal_call_id`.
        let mut tool_indices: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        // Phase-1 item #4 (docs/AGENTIC_LOOP_PLAN.md): set of tool
        // calls whose `ToolCallEnd` hasn't fired yet. While any
        // entry is open we cap the WAIT FOR THE NEXT CHUNK at
        // `TOOL_CALL_GAP_TIMEOUT` — but the cap is reset every
        // time the provider sends ANY chunk (text, reasoning,
        // another tool-call delta). A model that legitimately
        // interleaves text + tool-call deltas keeps making
        // forward progress and never trips the gap timeout; only
        // a true mid-assembly stall (no chunks of ANY kind for
        // 30s while a tool call is open) fires.
        //
        // This addresses the review finding that the prior
        // "any chunk subject to the 30s timeout while a tool
        // call is open" semantic spuriously killed providers
        // that interleave reasoning between tool-call deltas.
        let mut open_tool_calls: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        // dirge-onlr/4xgd: resolved [timeouts].tool_call_gap_secs.
        let tool_call_gap_timeout: std::time::Duration =
            crate::timeout::Timeouts::get().tool_call_gap;
        // Wall-clock instant when the last forward-progress chunk
        // arrived. Used to compute the remaining gap budget while
        // a tool call is mid-assembly. Initialized to "now" so
        // the first wait starts with the full budget.
        let mut last_chunk_at = std::time::Instant::now();

        // Token usage captured from the Final(R) provider response.
        let mut token_usage: Option<super::message::TokenUsage> = None;

        loop {
            // Code review R3: honor AbortSignal between chunks.
            // The loop / tools already check signal at their
            // boundaries; here we add a per-chunk check so a
            // mid-stream cancel actually stops the rig request
            // rather than waiting for the next turn boundary.
            // Pre-poll check covers the case where signal was
            // cancelled BEFORE the first chunk arrived; the
            // post-await check catches cancellation that
            // happened DURING the chunk wait.
            if let Some(sig) = signal.as_ref()
                && sig.is_cancelled()
            {
                yield StreamEvent::Error {
                    error: "stream aborted by cancellation signal".to_string(),
                };
                return;
            }
            // Apply per-chunk timeout. When a tool call is
            // mid-assembly we narrow to the remaining gap budget
            // (TOOL_CALL_GAP_TIMEOUT minus elapsed since the last
            // chunk of any kind). Otherwise the configured
            // `chunk_timeout` is used as-is.
            let effective_timeout = if !open_tool_calls.is_empty() {
                let remaining = tool_call_gap_timeout.saturating_sub(last_chunk_at.elapsed());
                let gap_budget = if remaining.is_zero() {
                    // The forward-progress window already
                    // expired between iterations. Fire the
                    // timeout immediately rather than racing
                    // an effectively-zero `tokio::time::timeout`.
                    std::time::Duration::from_millis(1)
                } else {
                    remaining
                };
                match chunk_timeout {
                    Some(t) => Some(t.min(gap_budget)),
                    None => Some(gap_budget),
                }
            } else {
                chunk_timeout
            };
            let next = match effective_timeout {
                Some(t) => match tokio::time::timeout(t, raw.next()).await {
                    Ok(item) => item,
                    Err(_) => {
                        // Phrase using "timed out" so
                        // recovery::classify_error matches on
                        // it and routes to ErrorKind::Network for
                        // retry. Matches runner.rs:301-304.
                        let detail = if !open_tool_calls.is_empty() {
                            format!(
                                "stream chunk timed out after {}s while a tool call was mid-assembly (provider stalled emitting tool-call deltas — common DeepSeek symptom; the harness narrows to {}s while assembling tool calls)",
                                t.as_secs(),
                                tool_call_gap_timeout.as_secs(),
                            )
                        } else {
                            format!(
                                "stream chunk timed out after {}s (provider stalled or connection silently dropped) — bump `stream_chunk_timeout_secs` in config.json if your model has long reasoning gaps",
                                t.as_secs(),
                            )
                        };
                        yield StreamEvent::Error { error: detail };
                        return;
                    }
                },
                None => raw.next().await,
            };
            let item = match next {
                Some(item) => item,
                None => break,
            };
            // Forward-progress signal — refresh the gap window
            // so the next iteration's tool-call-gap budget
            // starts fresh. Applied to every chunk regardless
            // of kind (text, reasoning, tool-call-delta, final
            // ToolCall): any forward motion from the provider
            // is enough to reset the stall detector.
            last_chunk_at = std::time::Instant::now();
            match item {
                Ok(StreamedAssistantContent::Text(t)) => {
                    match current_text_idx {
                        Some(idx) => {
                            if let Some(ContentBlock::Text { text: existing }) =
                                partial.content.get_mut(idx)
                            {
                                existing.push_str(&t.text);
                            }
                            yield StreamEvent::Delta {
                                partial: partial.clone(),
                                phase: DeltaPhase::TextDelta,
                            };
                        }
                        None => {
                            current_text_idx = Some(partial.content.len());
                            partial
                                .content
                                .push(ContentBlock::Text { text: t.text.clone() });
                            // Other blocks are interrupted; reset
                            // their indices so subsequent chunks
                            // open fresh blocks.
                            current_thinking_idx = None;
                            yield StreamEvent::Delta {
                                partial: partial.clone(),
                                phase: DeltaPhase::TextStart,
                            };
                        }
                    }
                }
                Ok(StreamedAssistantContent::ReasoningDelta { id: _, reasoning }) => {
                    match current_thinking_idx {
                        Some(idx) => {
                            if let Some(ContentBlock::Thinking { text }) =
                                partial.content.get_mut(idx)
                            {
                                text.push_str(&reasoning);
                            }
                            yield StreamEvent::Delta {
                                partial: partial.clone(),
                                phase: DeltaPhase::ThinkingDelta,
                            };
                        }
                        None => {
                            current_thinking_idx = Some(partial.content.len());
                            partial.content.push(ContentBlock::Thinking { text: reasoning });
                            current_text_idx = None;
                            yield StreamEvent::Delta {
                                partial: partial.clone(),
                                phase: DeltaPhase::ThinkingStart,
                            };
                        }
                    }
                }
                Ok(StreamedAssistantContent::Reasoning(r)) => {
                    // Complete reasoning block emitted in one shot.
                    // `r.content` is `Vec<ReasoningContent>` — a
                    // tagged enum with Text / Encrypted / Redacted /
                    // Summary variants. We surface plain-text and
                    // Summary; encrypted/redacted payloads are
                    // opaque (no display benefit) so we skip them.
                    let text: String = r
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
                    partial.content.push(ContentBlock::Thinking { text });
                    current_thinking_idx = None;
                    current_text_idx = None;
                    yield StreamEvent::Delta {
                        partial: partial.clone(),
                        phase: DeltaPhase::ThinkingEnd,
                    };
                }
                Ok(StreamedAssistantContent::ToolCall {
                    tool_call,
                    internal_call_id,
                }) => {
                    // H-7 bug fix (scenario 3): some providers
                    // (DeepSeek, OpenAI in some configurations)
                    // emit BOTH streaming ToolCallDelta events
                    // AND a final Complete ToolCall for the SAME
                    // logical call. The earlier version always
                    // pushed a new ContentBlock here, producing
                    // a duplicate block and causing the loop to
                    // dispatch the tool TWICE — the next request
                    // then sent duplicate tool_call_ids in
                    // history and the provider rejected it
                    // (400). Fix: if a delta-built block exists
                    // for this `internal_call_id`, REPLACE it
                    // with the authoritative complete payload
                    // instead of pushing a new one. Emit only
                    // ToolCallEnd (the Delta path already emitted
                    // ToolCallStart) for the dedup case;
                    // freshly-pushed blocks emit Start + End
                    // as before.
                    let new_block = ContentBlock::ToolCall {
                        id: tool_call.id.clone(),
                        name: tool_call.function.name.clone(),
                        arguments: tool_call.function.arguments.clone(),
                    };
                    let was_existing =
                        tool_indices.contains_key(&internal_call_id);
                    if was_existing {
                        let idx = *tool_indices.get(&internal_call_id).unwrap();
                        if let Some(block) = partial.content.get_mut(idx) {
                            *block = new_block;
                        }
                    } else {
                        let idx = partial.content.len();
                        partial.content.push(new_block);
                        tool_indices.insert(internal_call_id.clone(), idx);
                    }
                    current_text_idx = None;
                    current_thinking_idx = None;
                    if !was_existing {
                        // Fresh push → emit Start.
                        yield StreamEvent::Delta {
                            partial: partial.clone(),
                            phase: DeltaPhase::ToolCallStart,
                        };
                    }
                    // Always emit End — marks the call complete.
                    yield StreamEvent::Delta {
                        partial: partial.clone(),
                        phase: DeltaPhase::ToolCallEnd,
                    };
                    // Phase-1 #4: clear the open-call marker now
                    // that the call is finalized. `was_existing`
                    // means deltas arrived first; either way the
                    // ToolCallEnd above closes it.
                    open_tool_calls.remove(&internal_call_id);
                }
                Ok(StreamedAssistantContent::ToolCallDelta {
                    id,
                    internal_call_id,
                    content,
                }) => {
                    // Streaming tool call. On first delta for this
                    // `internal_call_id` we open the block AND
                    // apply the content together, emitting a
                    // single `ToolCallStart`. Subsequent deltas
                    // merge into the existing block and emit
                    // `ToolCallDelta`. Mirrors the text/thinking
                    // pattern — the "start" event IS the first
                    // chunk, not a separate prologue.
                    let is_first = !tool_indices.contains_key(&internal_call_id);
                    let idx = if is_first {
                        let i = partial.content.len();
                        partial.content.push(ContentBlock::ToolCall {
                            id: id.clone(),
                            name: String::new(),
                            arguments: serde_json::Value::String(String::new()),
                        });
                        tool_indices.insert(internal_call_id.clone(), i);
                        // Phase-1 #4: mark this call open so the
                        // chunk-timeout narrows until ToolCallEnd
                        // fires.
                        open_tool_calls.insert(internal_call_id.clone());
                        current_text_idx = None;
                        current_thinking_idx = None;
                        i
                    } else {
                        *tool_indices.get(&internal_call_id).unwrap()
                    };
                    if let Some(ContentBlock::ToolCall {
                        id: existing_id,
                        name,
                        arguments,
                    }) = partial.content.get_mut(idx)
                    {
                        apply_tool_call_delta(existing_id, name, arguments, &id, content);
                    }
                    yield StreamEvent::Delta {
                        partial: partial.clone(),
                        phase: if is_first {
                            DeltaPhase::ToolCallStart
                        } else {
                            DeltaPhase::ToolCallDelta
                        },
                    };
                }
                Ok(StreamedAssistantContent::Final(r)) => {
                    token_usage = r.token_usage().map(|u| super::message::TokenUsage {
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                    });
                }
                Err(err) => {
                    yield StreamEvent::Error {
                        error: err.to_string(),
                    };
                    return;
                }
            }
        }

        // Stream ended normally — finalize with the assembled
        // partial. `stop_reason` is `ToolUse` iff any toolCall
        // block is present (pi's stopReason inference for raw
        // provider streams that don't emit a stop reason
        // explicitly), else `Stop`.
        let has_tool_calls = partial
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall { .. }));
        let final_message = AssistantMessage {
            content: partial.content,
            stop_reason: if has_tool_calls {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            },
            error_message: None,
        };
        yield StreamEvent::Done {
            reason: final_message.stop_reason,
            message: final_message,
            usage: token_usage,
        };
    })
}

/// Apply a rig `ToolCallDeltaContent` to an in-progress tool
/// call. Rig deltas carry either the tool name (via
/// `ToolCallDeltaContent::Name`) or argument fragments (via
/// `Delta`). Some providers also re-emit the provider-supplied
/// `id` per delta — we update if non-empty.
fn apply_tool_call_delta(
    existing_id: &mut String,
    name: &mut String,
    arguments: &mut serde_json::Value,
    new_id: &str,
    content: rig::streaming::ToolCallDeltaContent,
) {
    use rig::streaming::ToolCallDeltaContent;
    if existing_id.is_empty() && !new_id.is_empty() {
        *existing_id = new_id.to_string();
    }
    match content {
        ToolCallDeltaContent::Name(n) => {
            *name = n;
        }
        ToolCallDeltaContent::Delta(chunk) => {
            // Args are emitted as JSON-string fragments by most
            // providers. We accumulate into a string; downstream
            // code parses lazily when the value is read.
            if let serde_json::Value::String(s) = arguments {
                s.push_str(&chunk);
            } else {
                *arguments = serde_json::Value::String(chunk);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::message::{Reasoning, ReasoningContent, Text, ToolCall, ToolFunction};
    use rig::streaming::ToolCallDeltaContent;

    /// Minimal R type for tests — needs Clone + Unpin + Send + GetTokenUsage.
    /// We don't actually instantiate it via `Final`.
    #[derive(Clone, Debug)]
    struct TestResponse;

    impl GetTokenUsage for TestResponse {
        fn token_usage(&self) -> Option<rig::completion::Usage> {
            None
        }
    }

    /// Build a stream from a Vec of canned items.
    fn raw_stream(
        items: Vec<Result<StreamedAssistantContent<TestResponse>, CompletionError>>,
    ) -> Pin<
        Box<
            dyn Stream<Item = Result<StreamedAssistantContent<TestResponse>, CompletionError>>
                + Send,
        >,
    > {
        Box::pin(futures::stream::iter(items))
    }

    /// Drain a wrapped stream into a Vec of events.
    async fn drain(mut s: Pin<Box<dyn Stream<Item = StreamEvent> + Send>>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(e) = s.next().await {
            out.push(e);
        }
        out
    }

    /// Concise per-event label for assertion ergonomics.
    fn label(e: &StreamEvent) -> String {
        match e {
            StreamEvent::Start { .. } => "start".into(),
            StreamEvent::Delta { phase, .. } => format!("delta:{phase:?}"),
            StreamEvent::Done { reason, .. } => format!("done:{reason:?}"),
            StreamEvent::Error { .. } => "error".into(),
            StreamEvent::Retry { .. } => "retry".into(),
        }
    }

    /// Single text response: Start → TextStart → TextDelta → Done.
    #[tokio::test]
    async fn wraps_simple_text_response() {
        let raw = raw_stream(vec![
            Ok(StreamedAssistantContent::Text(Text {
                text: "Hello".to_string(),
            })),
            Ok(StreamedAssistantContent::Text(Text {
                text: " world".to_string(),
            })),
        ]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let labels: Vec<_> = events.iter().map(label).collect();
        assert_eq!(
            labels,
            vec![
                "start".to_string(),
                "delta:TextStart".to_string(),
                "delta:TextDelta".to_string(),
                "done:Stop".to_string(),
            ]
        );
        match events.last().unwrap() {
            StreamEvent::Done { message, .. } => {
                assert_eq!(message.content.len(), 1);
                match &message.content[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "Hello world"),
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected Done last"),
        }
    }

    /// Complete tool call: ToolCallStart + ToolCallEnd, Done with
    /// stopReason=ToolUse.
    #[tokio::test]
    async fn wraps_complete_tool_call() {
        let raw = raw_stream(vec![Ok(StreamedAssistantContent::ToolCall {
            tool_call: ToolCall {
                id: "call_1".to_string(),
                call_id: None,
                function: ToolFunction {
                    name: "echo".to_string(),
                    arguments: serde_json::json!({"value": "hi"}),
                },
                signature: None,
                additional_params: None,
            },
            internal_call_id: "internal_1".to_string(),
        })]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let labels: Vec<_> = events.iter().map(label).collect();
        assert_eq!(
            labels,
            vec![
                "start",
                "delta:ToolCallStart",
                "delta:ToolCallEnd",
                "done:ToolUse",
            ]
        );
        match events.last().unwrap() {
            StreamEvent::Done { message, .. } => {
                assert_eq!(message.content.len(), 1);
                if let ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } = &message.content[0]
                {
                    assert_eq!(id, "call_1");
                    assert_eq!(name, "echo");
                    assert_eq!(arguments["value"], "hi");
                } else {
                    panic!("expected toolCall");
                }
            }
            _ => panic!("expected Done"),
        }
    }

    /// Streaming tool call: Name delta + arg fragments assembled.
    #[tokio::test]
    async fn wraps_streaming_tool_call_deltas() {
        let raw = raw_stream(vec![
            Ok(StreamedAssistantContent::ToolCallDelta {
                id: "call_2".to_string(),
                internal_call_id: "internal_2".to_string(),
                content: ToolCallDeltaContent::Name("write".to_string()),
            }),
            Ok(StreamedAssistantContent::ToolCallDelta {
                id: "call_2".to_string(),
                internal_call_id: "internal_2".to_string(),
                content: ToolCallDeltaContent::Delta("{\"pa".to_string()),
            }),
            Ok(StreamedAssistantContent::ToolCallDelta {
                id: "call_2".to_string(),
                internal_call_id: "internal_2".to_string(),
                content: ToolCallDeltaContent::Delta("th\":\"/tmp/x\"}".to_string()),
            }),
        ]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let labels: Vec<_> = events.iter().map(label).collect();
        assert_eq!(
            labels,
            vec![
                "start",
                "delta:ToolCallStart",
                "delta:ToolCallDelta",
                "delta:ToolCallDelta",
                "done:ToolUse",
            ]
        );
        match events.last().unwrap() {
            StreamEvent::Done { message, .. } => {
                if let ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } = &message.content[0]
                {
                    assert_eq!(id, "call_2");
                    assert_eq!(name, "write");
                    assert_eq!(arguments.as_str().unwrap(), "{\"path\":\"/tmp/x\"}");
                } else {
                    panic!("expected toolCall");
                }
            }
            _ => panic!("expected Done"),
        }
    }

    /// H-7 regression: DeepSeek (and some OpenAI configs) emit
    /// BOTH streaming `ToolCallDelta` events AND a final
    /// `ToolCall { tool_call }` complete event for the SAME
    /// logical call (same `internal_call_id`). Earlier code
    /// pushed two separate ContentBlock::ToolCall entries,
    /// causing the loop to dispatch the tool TWICE.
    ///
    /// Expected behavior: the delta-built block is REPLACED by
    /// the complete-event payload (single block, single
    /// dispatch). Only ToolCallStart from the first delta;
    /// ToolCallEnd from the complete event. Provider's complete
    /// args overwrite the accumulated-string args from deltas.
    #[tokio::test]
    async fn wraps_provider_emitting_both_deltas_and_complete_dedups() {
        let raw = raw_stream(vec![
            // Streaming deltas first.
            Ok(StreamedAssistantContent::ToolCallDelta {
                id: "call_x".to_string(),
                internal_call_id: "internal_x".to_string(),
                content: ToolCallDeltaContent::Name("echo_tool".to_string()),
            }),
            Ok(StreamedAssistantContent::ToolCallDelta {
                id: "call_x".to_string(),
                internal_call_id: "internal_x".to_string(),
                content: ToolCallDeltaContent::Delta("{\"text\":\"pineapple\"}".to_string()),
            }),
            // Then the SAME logical call as a Complete event
            // (with the same internal_call_id).
            Ok(StreamedAssistantContent::ToolCall {
                tool_call: ToolCall {
                    id: "call_x".to_string(),
                    call_id: None,
                    function: ToolFunction {
                        name: "echo_tool".to_string(),
                        arguments: serde_json::json!({"text": "pineapple"}),
                    },
                    signature: None,
                    additional_params: None,
                },
                internal_call_id: "internal_x".to_string(),
            }),
        ]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let final_msg = events
            .iter()
            .rev()
            .find_map(|e| {
                if let StreamEvent::Done { message, .. } = e {
                    Some(message.clone())
                } else {
                    None
                }
            })
            .expect("Done");
        // Critical assertion: ONE tool call block, not two.
        let tool_call_count = final_msg
            .content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
            .count();
        assert_eq!(
            tool_call_count, 1,
            "expected 1 ToolCall block after dedup; got {tool_call_count}. \
             This is the h-7 scenario-3 regression — DeepSeek and some OpenAI \
             configs emit both delta + complete for the same call."
        );
        // The single block should carry the Complete event's
        // payload (parsed args), not the delta-accumulated
        // string.
        if let ContentBlock::ToolCall {
            id,
            name,
            arguments,
        } = &final_msg.content[0]
        {
            assert_eq!(id, "call_x");
            assert_eq!(name, "echo_tool");
            // Should be a parsed object, not a JSON string.
            assert!(
                arguments.is_object(),
                "args should be a parsed object after dedup; got: {arguments:?}"
            );
            assert_eq!(arguments["text"], "pineapple");
        } else {
            panic!("expected ToolCall block");
        }

        // Event sequence should have ToolCallStart (from first
        // delta) followed by ToolCallDelta(s) and a single
        // ToolCallEnd (from the complete event). No second
        // ToolCallStart from the complete event because dedup
        // path skips it.
        let starts = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    StreamEvent::Delta {
                        phase: DeltaPhase::ToolCallStart,
                        ..
                    }
                )
            })
            .count();
        let ends = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    StreamEvent::Delta {
                        phase: DeltaPhase::ToolCallEnd,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(starts, 1, "expected 1 ToolCallStart; got {starts}");
        assert_eq!(ends, 1, "expected 1 ToolCallEnd; got {ends}");
    }

    /// Reasoning deltas accumulate into a Thinking block.
    #[tokio::test]
    async fn wraps_reasoning_deltas() {
        let raw = raw_stream(vec![
            Ok(StreamedAssistantContent::ReasoningDelta {
                id: None,
                reasoning: "Let me think".to_string(),
            }),
            Ok(StreamedAssistantContent::ReasoningDelta {
                id: None,
                reasoning: " about this".to_string(),
            }),
        ]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let labels: Vec<_> = events.iter().map(label).collect();
        assert_eq!(
            labels,
            vec![
                "start",
                "delta:ThinkingStart",
                "delta:ThinkingDelta",
                "done:Stop",
            ]
        );
        match events.last().unwrap() {
            StreamEvent::Done { message, .. } => {
                if let ContentBlock::Thinking { text } = &message.content[0] {
                    assert_eq!(text, "Let me think about this");
                } else {
                    panic!("expected thinking");
                }
            }
            _ => panic!("expected Done"),
        }
    }

    /// Complete reasoning block (one-shot).
    #[tokio::test]
    async fn wraps_complete_reasoning() {
        // `Reasoning` is `#[non_exhaustive]`; use its public
        // constructor.
        let raw = raw_stream(vec![Ok(StreamedAssistantContent::Reasoning(
            Reasoning::new("All thinking"),
        ))]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        assert!(matches!(events[0], StreamEvent::Start { .. }));
        assert!(matches!(
            events[1],
            StreamEvent::Delta {
                phase: DeltaPhase::ThinkingEnd,
                ..
            }
        ));
        assert!(matches!(events[2], StreamEvent::Done { .. }));
    }

    /// Error chunk emits Error and stops the stream.
    #[tokio::test]
    async fn wraps_error_emits_error_and_stops() {
        let raw = raw_stream(vec![
            Ok(StreamedAssistantContent::Text(Text {
                text: "partial".to_string(),
            })),
            Err(CompletionError::ProviderError("bad upstream".to_string())),
            Ok(StreamedAssistantContent::Text(Text {
                text: " more text".to_string(),
            })),
        ]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        assert!(matches!(events.last(), Some(StreamEvent::Error { .. })));
        let dones = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Done { .. }))
            .count();
        assert_eq!(dones, 0);
    }

    // ── dirge-ets0: Scavenge provider-coverage matrix ────────────
    //
    // Pillar 2 audit found that scavenge only reads
    // ContentBlock::Thinking. The end-to-end claim is that ALL
    // three reasoning surfaces (DeepSeek reasoning_content, OpenAI
    // o1 summary, Anthropic extended thinking) route through rig
    // into Thinking, so tool-call JSON the model forgot to put in
    // the structured tool_calls field gets recovered.
    //
    // These tests drive the full pipeline:
    // 1. Construct the rig-level streaming events for each
    //    provider shape.
    // 2. Run them through `wrap_streamed_assistant`.
    // 3. Extract the final AssistantMessage's Thinking content
    //    (the same surface run.rs:558-566 reads).
    // 4. Feed it to `scavenge_tool_calls`.
    // 5. Assert the orphan tool call was recovered.

    use crate::agent::agent_loop::scavenge::scavenge_tool_calls;
    use std::collections::HashSet;

    /// Extract the same `reasoning_text` string `run.rs:558-566`
    /// constructs from an AssistantMessage. Centralized helper
    /// so the test matrix mirrors the production reasoning-text
    /// shape verbatim — if run.rs ever changes how it joins
    /// Thinking blocks, these tests must change with it.
    fn reasoning_text_of(message: &AssistantMessage) -> String {
        message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Thinking { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn allowed_set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// DeepSeek pattern: provider streams the `reasoning_content`
    /// field as chunked `ReasoningDelta` events. The chunks may
    /// straddle JSON tokens. End-to-end: a model that forgot to
    /// emit the call in `tool_calls` but described it in
    /// reasoning must be recovered by scavenge.
    #[tokio::test]
    async fn provider_coverage_deepseek_reasoning_delta_chunks() {
        // Three chunks with the orphan tool-call JSON straddling
        // chunk boundaries — the worst case for naive joiners.
        let raw = raw_stream(vec![
            Ok(StreamedAssistantContent::ReasoningDelta {
                id: None,
                reasoning: "I should call ".to_string(),
            }),
            Ok(StreamedAssistantContent::ReasoningDelta {
                id: None,
                reasoning: r#"{"name": "get_weather", "arguments""#.to_string(),
            }),
            Ok(StreamedAssistantContent::ReasoningDelta {
                id: None,
                reasoning: r#": {"city": "SF"}}"#.to_string(),
            }),
        ]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let message = match events.last() {
            Some(StreamEvent::Done { message, .. }) => message.clone(),
            _ => panic!("expected Done"),
        };
        // Verify the Thinking block was assembled correctly from
        // the chunks before scavenge runs against it.
        let reasoning = reasoning_text_of(&message);
        assert!(
            reasoning.contains(r#"{"name": "get_weather", "arguments": {"city": "SF"}}"#),
            "chunks must reassemble into the full JSON: {reasoning:?}",
        );
        // End-to-end scavenge.
        let allowed = allowed_set(&["get_weather"]);
        let result = scavenge_tool_calls(Some(&reasoning), &allowed, 4);
        assert_eq!(
            result.calls.len(),
            1,
            "scavenge must recover the orphan call from DeepSeek-style \
             reasoning_content chunks: {result:?}",
        );
        assert_eq!(result.calls[0].name, "get_weather");
        assert_eq!(result.calls[0].arguments["city"], "SF");
    }

    /// OpenAI o1 pattern: provider emits a single complete
    /// Reasoning event with `ReasoningContent::Summary`. The
    /// summary is a redacted overview of the model's internal
    /// thinking — but if a tool-call JSON shows up in it (rare
    /// but observed), scavenge must still recover it.
    #[tokio::test]
    async fn provider_coverage_openai_o1_summary_reasoning() {
        let mut reasoning = Reasoning::new("");
        // Public constructor builds an empty Reasoning; mutate
        // its content via the same path the provider takes.
        reasoning.content = vec![ReasoningContent::Summary(
            r#"Plan: {"name": "search", "arguments": {"q": "rust async"}}"#.to_string(),
        )];
        let raw = raw_stream(vec![Ok(StreamedAssistantContent::Reasoning(reasoning))]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let message = match events.last() {
            Some(StreamEvent::Done { message, .. }) => message.clone(),
            _ => panic!("expected Done"),
        };
        let reasoning_text = reasoning_text_of(&message);
        let allowed = allowed_set(&["search"]);
        let result = scavenge_tool_calls(Some(&reasoning_text), &allowed, 4);
        assert_eq!(
            result.calls.len(),
            1,
            "scavenge must recover orphan call from o1 Summary: \
             reasoning={reasoning_text:?}, result={result:?}",
        );
        assert_eq!(result.calls[0].name, "search");
        assert_eq!(result.calls[0].arguments["q"], "rust async");
    }

    /// Anthropic extended-thinking pattern: provider emits a
    /// complete Reasoning event with one or more
    /// `ReasoningContent::Text` entries. End-to-end recovery
    /// must work identically to the o1 case.
    #[tokio::test]
    async fn provider_coverage_anthropic_extended_thinking_text() {
        let mut reasoning = Reasoning::new("");
        reasoning.content = vec![
            ReasoningContent::Text {
                text: "Let me look this up.".to_string(),
                signature: None,
            },
            ReasoningContent::Text {
                text: r#"I'll dispatch: {"name": "search", "arguments": {"q": "anthropic"}}"#
                    .to_string(),
                signature: None,
            },
        ];
        let raw = raw_stream(vec![Ok(StreamedAssistantContent::Reasoning(reasoning))]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let message = match events.last() {
            Some(StreamEvent::Done { message, .. }) => message.clone(),
            _ => panic!("expected Done"),
        };
        let reasoning_text = reasoning_text_of(&message);
        let allowed = allowed_set(&["search"]);
        let result = scavenge_tool_calls(Some(&reasoning_text), &allowed, 4);
        assert_eq!(
            result.calls.len(),
            1,
            "scavenge must recover orphan call from Anthropic-style \
             multi-text reasoning: {result:?}",
        );
        assert_eq!(result.calls[0].name, "search");
        assert_eq!(result.calls[0].arguments["q"], "anthropic");
    }

    /// Anthropic-specific edge: `ReasoningContent::Encrypted` and
    /// `Redacted` payloads. These are opaque (the model never
    /// emits them as scavengeable text) — they MUST be dropped
    /// without panicking and without producing a Thinking block
    /// with garbled bytes. Documents the intentional gap so a
    /// future change that *does* surface them is conscious.
    #[tokio::test]
    async fn provider_coverage_anthropic_encrypted_thinking_is_dropped_silently() {
        // Use the rig API directly so we don't depend on whether
        // these variants are constructible from public APIs.
        let mut reasoning = Reasoning::new("");
        reasoning.content = vec![
            ReasoningContent::Text {
                text: "visible reasoning".to_string(),
                signature: None,
            },
            ReasoningContent::Encrypted("OPAQUE_BYTES".to_string()),
        ];
        let raw = raw_stream(vec![Ok(StreamedAssistantContent::Reasoning(reasoning))]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let message = match events.last() {
            Some(StreamEvent::Done { message, .. }) => message.clone(),
            _ => panic!("expected Done"),
        };
        let reasoning_text = reasoning_text_of(&message);
        // Visible text survives.
        assert!(
            reasoning_text.contains("visible reasoning"),
            "Text content must survive: {reasoning_text:?}",
        );
        // Encrypted payload does NOT leak into the reasoning
        // surface — scavenge would otherwise try to parse opaque
        // bytes as JSON and could produce spurious notes.
        assert!(
            !reasoning_text.contains("OPAQUE_BYTES"),
            "encrypted payload must be dropped, not appended: {reasoning_text:?}",
        );
        // Scavenge on the remaining text finds nothing actionable
        // (no JSON in the visible portion). Important: it must
        // not crash on the encrypted-was-dropped path.
        let allowed = allowed_set(&["search"]);
        let result = scavenge_tool_calls(Some(&reasoning_text), &allowed, 4);
        assert!(
            result.calls.is_empty(),
            "no orphan call in visible text; scavenge must return empty",
        );
    }

    /// Cross-provider negative: an orphan call to a tool the
    /// model isn't allowed to call must be ignored regardless of
    /// which reasoning surface surfaced it. Defense against the
    /// failure mode where the model hallucinates a `rm -rf /`
    /// tool in reasoning and scavenge would otherwise dispatch it.
    #[tokio::test]
    async fn provider_coverage_orphan_call_to_disallowed_tool_is_ignored() {
        let raw = raw_stream(vec![Ok(StreamedAssistantContent::ReasoningDelta {
            id: None,
            reasoning: r#"{"name": "rm_rf_slash", "arguments": {}}"#.to_string(),
        })]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let message = match events.last() {
            Some(StreamEvent::Done { message, .. }) => message.clone(),
            _ => panic!("expected Done"),
        };
        let reasoning_text = reasoning_text_of(&message);
        // Only "search" is allowed; "rm_rf_slash" is not.
        let allowed = allowed_set(&["search"]);
        let result = scavenge_tool_calls(Some(&reasoning_text), &allowed, 4);
        assert!(
            result.calls.is_empty(),
            "scavenge must skip disallowed tools regardless of reasoning surface",
        );
    }

    /// Multiple Thinking blocks (interleaved with text content)
    /// MUST all be joined the same way `run.rs:558-566` does so
    /// a tool call that straddles a text→thinking→text boundary
    /// gets recovered. Catches a regression where some future
    /// refactor might forget to concat all Thinking blocks.
    #[tokio::test]
    async fn provider_coverage_multiple_thinking_blocks_all_scavenged() {
        let mut r1 = Reasoning::new("");
        r1.content = vec![ReasoningContent::Text {
            text: r#"first: {"name": "get_weather", "arguments": {"city": "SF"}}"#.to_string(),
            signature: None,
        }];
        let mut r2 = Reasoning::new("");
        r2.content = vec![ReasoningContent::Text {
            text: r#"second: {"name": "search", "arguments": {"q": "x"}}"#.to_string(),
            signature: None,
        }];
        let raw = raw_stream(vec![
            Ok(StreamedAssistantContent::Reasoning(r1)),
            Ok(StreamedAssistantContent::Text(Text {
                text: "between".to_string(),
            })),
            Ok(StreamedAssistantContent::Reasoning(r2)),
        ]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let message = match events.last() {
            Some(StreamEvent::Done { message, .. }) => message.clone(),
            _ => panic!("expected Done"),
        };
        let reasoning_text = reasoning_text_of(&message);
        let allowed = allowed_set(&["get_weather", "search"]);
        let result = scavenge_tool_calls(Some(&reasoning_text), &allowed, 4);
        assert_eq!(
            result.calls.len(),
            2,
            "both Thinking blocks must contribute to scavenge: {result:?}",
        );
        let names: Vec<&str> = result.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"get_weather"));
        assert!(names.contains(&"search"));
    }

    /// dirge-ets0 end-to-end: full chain stream → assistant
    /// message → scavenge → dedupe → tool_calls. Mirrors the
    /// integration in `run.rs:558-636` to prove the wiring works
    /// across the boundary, not just at the surface points the
    /// per-provider tests check.
    ///
    /// Scenario: model emits ONE structured tool call AND a
    /// reasoning block containing the SAME call (provider double-
    /// emit, e.g. R1 leaking the call into reasoning_content) PLUS
    /// a NEW orphan call. After integration:
    /// - the structured call stays exactly once (dedupe wins)
    /// - the orphan call is appended (novel signature)
    /// - no third copy of the structured call shows up
    #[tokio::test]
    async fn provider_coverage_end_to_end_scavenge_dedupe_chain() {
        use rig::completion::message::{ToolCall as RigToolCall, ToolFunction as RigToolFunction};

        // Stream: structured tool call + reasoning describing
        // the same call AND a new one.
        let raw = raw_stream(vec![
            Ok(StreamedAssistantContent::ReasoningDelta {
                id: None,
                reasoning: format!(
                    "Plan: call get_weather. {} Then maybe also {}",
                    r#"{"name": "get_weather", "arguments": {"city": "SF"}}"#,
                    r#"{"name": "search", "arguments": {"q": "tide"}}"#,
                ),
            }),
            Ok(StreamedAssistantContent::ToolCall {
                tool_call: RigToolCall {
                    id: "call-1".to_string(),
                    function: RigToolFunction {
                        name: "get_weather".to_string(),
                        arguments: serde_json::json!({"city": "SF"}),
                    },
                    call_id: None,
                    signature: None,
                    additional_params: None,
                },
                internal_call_id: "internal-1".to_string(),
            }),
        ]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let message = match events.last() {
            Some(StreamEvent::Done { message, .. }) => message.clone(),
            _ => panic!("expected Done"),
        };

        // Mirror run.rs:535-554 — collect structured tool calls.
        let mut tool_calls: Vec<crate::agent::agent_loop::tools::ToolCall> = message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some(crate::agent::agent_loop::tools::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
                _ => None,
            })
            .collect();
        assert_eq!(
            tool_calls.len(),
            1,
            "structured tool call must be extracted exactly once"
        );

        // Mirror run.rs:558-636 — scavenge + dedupe.
        let reasoning_text = reasoning_text_of(&message);
        let allowed = allowed_set(&["get_weather", "search"]);
        let scavenge_result = scavenge_tool_calls(Some(&reasoning_text), &allowed, 4);
        assert_eq!(
            scavenge_result.calls.len(),
            2,
            "scavenge must find both reasoning-embedded calls",
        );

        // Same canonical-JSON dedupe shape as run.rs.
        fn canonical(v: &serde_json::Value) -> String {
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
                        s.push_str(&canonical(&m[*k]));
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
                        s.push_str(&canonical(e));
                    }
                    s.push(']');
                    s
                }
                other => serde_json::to_string(other).unwrap_or_default(),
            }
        }
        let seen: HashSet<String> = tool_calls
            .iter()
            .map(|tc| format!("{}::{}", tc.name, canonical(&tc.arguments)))
            .collect();
        for sc in &scavenge_result.calls {
            let sig = format!("{}::{}", sc.name, canonical(&sc.arguments));
            if !seen.contains(&sig) {
                tool_calls.push(sc.clone());
            }
        }

        // Final assertion: structured call preserved, orphan
        // added, no double-count.
        assert_eq!(
            tool_calls.len(),
            2,
            "expected 2 calls (1 structured + 1 novel scavenged); got: {:?}",
            tool_calls.iter().map(|t| &t.name).collect::<Vec<_>>(),
        );
        let names: Vec<&str> = tool_calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["get_weather", "search"]);
        // Structured call's id is preserved (the reasoning copy
        // had no id and would have been ignored only if dedupe
        // hit — which it must).
        assert_eq!(tool_calls[0].id, "call-1");
    }

    /// Mixed content: text → reasoning → text produces 3 blocks
    /// because the reasoning resets the text-block index.
    #[tokio::test]
    async fn wraps_mixed_content_resets_block_indices() {
        let raw = raw_stream(vec![
            Ok(StreamedAssistantContent::Text(Text {
                text: "hi ".to_string(),
            })),
            Ok(StreamedAssistantContent::ReasoningDelta {
                id: None,
                reasoning: "thinking".to_string(),
            }),
            Ok(StreamedAssistantContent::Text(Text {
                text: "done".to_string(),
            })),
        ]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        let final_msg = events
            .iter()
            .rev()
            .find_map(|e| {
                if let StreamEvent::Done { message, .. } = e {
                    Some(message.clone())
                } else {
                    None
                }
            })
            .expect("Done");
        assert_eq!(final_msg.content.len(), 3);
        assert!(matches!(
            &final_msg.content[0],
            ContentBlock::Text { text } if text == "hi "
        ));
        assert!(matches!(
            &final_msg.content[1],
            ContentBlock::Thinking { text } if text == "thinking"
        ));
        assert!(matches!(
            &final_msg.content[2],
            ContentBlock::Text { text } if text == "done"
        ));
    }

    // =================================================================
    // Phase 4.5h-3 — chunk timeout enforcement tests
    // =================================================================

    use std::time::Duration;

    /// Stream that yields one item then stalls forever. Use with
    /// `tokio::time::pause` so the stall is virtual.
    fn stalling_stream() -> Pin<
        Box<
            dyn Stream<Item = Result<StreamedAssistantContent<TestResponse>, CompletionError>>
                + Send,
        >,
    > {
        use futures::stream;
        Box::pin(stream::unfold(0u32, |n| async move {
            if n == 0 {
                Some((
                    Ok(StreamedAssistantContent::Text(Text {
                        text: "first chunk".to_string(),
                    })),
                    1,
                ))
            } else {
                // Stall: future that never resolves. Under
                // `tokio::time::pause` this triggers the
                // timeout deterministically.
                let () = futures::future::pending().await;
                None
            }
        }))
    }

    /// Stream that yields a partial ToolCallDelta then stalls
    /// forever. Models the "DeepSeek stalled mid-tool-call"
    /// failure that Phase-1 item #4 targets.
    fn tool_call_delta_then_stall() -> Pin<
        Box<
            dyn Stream<Item = Result<StreamedAssistantContent<TestResponse>, CompletionError>>
                + Send,
        >,
    > {
        use futures::stream;
        use rig::streaming::ToolCallDeltaContent;
        Box::pin(stream::unfold(0u32, |n| async move {
            if n == 0 {
                Some((
                    Ok(StreamedAssistantContent::ToolCallDelta {
                        id: "call_a".to_string(),
                        internal_call_id: "ica_a".to_string(),
                        content: ToolCallDeltaContent::Name("read".to_string()),
                    }),
                    1,
                ))
            } else {
                let () = futures::future::pending().await;
                None
            }
        }))
    }

    /// `None` chunk_timeout → no timeout enforcement. Verifies
    /// the disabled-timeout path is identical to the pre-h-3
    /// behavior.
    #[tokio::test]
    async fn chunk_timeout_none_disables_timeout() {
        let raw = raw_stream(vec![Ok(StreamedAssistantContent::Text(Text {
            text: "ok".to_string(),
        }))]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        // Normal completion — no Error.
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Done { .. })));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamEvent::Error { .. }))
        );
    }

    /// Phase-1 #4 fix: forward-progress chunks (text, reasoning,
    /// another tool-call delta) reset the gap budget. A
    /// provider that emits one ToolCallDelta, then a few
    /// TextDeltas across e.g. 25s, then more ToolCallDeltas
    /// should NOT trigger the gap timeout — only true silence
    /// of 30s does. Regression test for the review finding.
    #[tokio::test]
    async fn gap_timeout_resets_on_interleaved_text_delta() {
        use rig::streaming::ToolCallDeltaContent;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        tokio::time::pause();
        let step = Arc::new(AtomicU32::new(0));
        let step_clone = step.clone();
        let raw: Pin<
            Box<
                dyn Stream<Item = Result<StreamedAssistantContent<TestResponse>, CompletionError>>
                    + Send,
            >,
        > = Box::pin(futures::stream::unfold(0u32, move |n| {
            let step = step_clone.clone();
            async move {
                step.store(n, Ordering::SeqCst);
                match n {
                    0 => Some((
                        Ok(StreamedAssistantContent::ToolCallDelta {
                            id: "c1".to_string(),
                            internal_call_id: "ic1".to_string(),
                            content: ToolCallDeltaContent::Name("read".to_string()),
                        }),
                        1,
                    )),
                    1 => {
                        // Sleep 20s — within the 30s gap budget.
                        tokio::time::sleep(Duration::from_secs(20)).await;
                        Some((
                            Ok(StreamedAssistantContent::Text(Text {
                                text: "thinking…".to_string(),
                            })),
                            2,
                        ))
                    }
                    2 => {
                        // Sleep another 20s — still under 30s
                        // since the previous text delta reset
                        // the budget.
                        tokio::time::sleep(Duration::from_secs(20)).await;
                        Some((
                            Ok(StreamedAssistantContent::Text(Text {
                                text: "more thinking…".to_string(),
                            })),
                            3,
                        ))
                    }
                    _ => None,
                }
            }
        }));
        let drain_task = tokio::spawn(async move {
            drain(wrap_streamed_assistant(
                raw,
                Some(Duration::from_secs(300)),
                None,
            ))
            .await
        });
        tokio::time::advance(Duration::from_secs(50)).await;
        let events = drain_task.await.unwrap();

        // The stream should complete naturally (Done) rather
        // than timeout. The 30s gap budget never expires
        // because each ~20s wait is followed by a chunk.
        let has_timeout_error = events.iter().any(|e| {
            matches!(
                e,
                StreamEvent::Error { error } if error.contains("timed out")
            )
        });
        assert!(
            !has_timeout_error,
            "gap timeout should NOT fire when forward progress \
             (text deltas) keeps arriving within the 30s window: \
             events = {events:?}",
        );
    }

    /// Phase-1 #4: when a tool call is mid-assembly, the chunk
    /// timeout narrows to the gap-timeout (30s) even if the
    /// configured `chunk_timeout` is much larger. Without this,
    /// a provider stalled emitting tool-call deltas would wait
    /// the full 300s default before erroring.
    #[tokio::test]
    async fn tool_call_gap_timeout_fires_within_30s_even_with_large_chunk_timeout() {
        tokio::time::pause();
        let raw = tool_call_delta_then_stall();
        let drain_task = tokio::spawn(async move {
            drain(wrap_streamed_assistant(
                raw,
                Some(Duration::from_secs(300)),
                None,
            ))
            .await
        });
        // Advance just past the gap timeout. The broad 300s
        // timeout would not have fired yet.
        tokio::time::advance(Duration::from_secs(31)).await;
        let events = drain_task.await.unwrap();

        let last = events.last().expect("must have events");
        match last {
            StreamEvent::Error { error } => {
                assert!(
                    error.contains("timed out"),
                    "error must contain 'timed out' for retry routing: {error}"
                );
                assert!(
                    error.contains("tool call was mid-assembly") || error.contains("tool-call"),
                    "error should explain the tighter tool-call timeout: {error}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// Stalled stream + `Some(timeout)` → Error event with
    /// "timed out" substring. The substring is the contract:
    /// `recovery::classify_error` matches on it and routes to
    /// `ErrorKind::Network` for retry.
    #[tokio::test]
    async fn chunk_timeout_fires_with_classifiable_error() {
        tokio::time::pause();
        let raw = stalling_stream();
        let drain_task = tokio::spawn(async move {
            drain(wrap_streamed_assistant(
                raw,
                Some(Duration::from_secs(5)),
                None,
            ))
            .await
        });
        tokio::time::advance(Duration::from_secs(10)).await;
        let events = drain_task.await.unwrap();

        // Sequence: Start, Delta(TextStart for "first chunk"),
        // Error("timed out ..."). No Done.
        let last = events.last().expect("must have events");
        match last {
            StreamEvent::Error { error } => {
                assert!(
                    error.contains("timed out"),
                    "error text must contain 'timed out' for recovery::classify_error \
                     to route this to ErrorKind::Network — got: {error}"
                );
            }
            other => panic!("expected Error as last event, got {other:?}"),
        }
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Done { .. })),
            "no Done after timeout"
        );
    }

    /// R3 regression: AbortSignal cancellation between chunks
    /// produces an Error event and stops the stream. Earlier
    /// versions silently ignored opts.signal at the rig
    /// adapter level — mid-stream cancel had no effect until
    /// the next turn boundary.
    #[tokio::test]
    async fn signal_cancels_stream_mid_flight() {
        use crate::agent::agent_loop::tool::AbortSignal;
        let raw = raw_stream(vec![
            Ok(StreamedAssistantContent::Text(Text {
                text: "first".to_string(),
            })),
            Ok(StreamedAssistantContent::Text(Text {
                text: " second".to_string(),
            })),
        ]);
        let signal = AbortSignal::new();
        signal.cancel();
        let events = drain(wrap_streamed_assistant(raw, None, Some(signal))).await;
        // Pre-loop signal check fires before the first chunk
        // poll. Expect: Start, Error (no Text deltas).
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                StreamEvent::Start { .. } => "start",
                StreamEvent::Delta { .. } => "delta",
                StreamEvent::Done { .. } => "done",
                StreamEvent::Error { .. } => "error",
                StreamEvent::Retry { .. } => "retry",
            })
            .collect();
        assert_eq!(kinds, vec!["start", "error"]);
        match events.last().unwrap() {
            StreamEvent::Error { error } => {
                assert!(
                    error.contains("aborted"),
                    "expected 'aborted' in error message; got: {error}"
                );
            }
            _ => panic!("expected Error last"),
        }
    }

    /// R3: signal=None means the cancellation check is skipped.
    /// Pre-R3 behavior preserved when callers don't supply a
    /// signal (e.g. ad-hoc tests).
    #[tokio::test]
    async fn signal_none_does_not_affect_stream() {
        let raw = raw_stream(vec![Ok(StreamedAssistantContent::Text(Text {
            text: "ok".to_string(),
        }))]);
        let events = drain(wrap_streamed_assistant(raw, None, None)).await;
        // Normal completion — no Error.
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Done { .. })));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamEvent::Error { .. }))
        );
    }

    /// Fast stream + tight timeout still completes normally —
    /// timeout only fires when a chunk takes longer than the
    /// deadline, not when the whole stream does. (Per-chunk
    /// semantics, matching runner.rs.)
    #[tokio::test]
    async fn chunk_timeout_does_not_fire_on_fast_stream() {
        let raw = raw_stream(vec![
            Ok(StreamedAssistantContent::Text(Text {
                text: "fast 1".to_string(),
            })),
            Ok(StreamedAssistantContent::Text(Text {
                text: " 2".to_string(),
            })),
        ]);
        // Tight timeout (10ms) but all events fire
        // immediately from the iter stream — no real wait.
        let events = drain(wrap_streamed_assistant(
            raw,
            Some(Duration::from_millis(10)),
            None,
        ))
        .await;
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Done { .. })));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamEvent::Error { .. }))
        );
    }
}
