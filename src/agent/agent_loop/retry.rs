//! Phase 4.5g — recovery / retry wrapper around `StreamFn`.
//!
//! Wraps an inner `StreamFn` so transient errors (Network,
//! RateLimit) automatically retry with exponential backoff +
//! Retry-After parsing. Non-transient errors (Auth,
//! ContextLength, Other) pass through unchanged so the loop can
//! surface them.
//!
//! ## When does retry fire?
//!
//! Only when the inner stream produces an `Error` event BEFORE
//! any committed content has been emitted. "Committed" means the
//! stream yielded a content-bearing event (Token delta, tool call
//! delta, etc.) that the caller can already observe downstream.
//! Retrying after committed content would emit duplicated tokens
//! to the consumer.
//!
//! Pi's existing `runner.rs` uses an equivalent gate
//! (`had_tool_calls`): once side effects have run, retry is
//! unsafe. Our gate is finer — it tracks any actual deltas, not
//! just tool calls — because mid-text retries would also produce
//! visible duplication.
//!
//! ## Backoff
//!
//! Uses `RecoveryPolicy::backoff_duration_for_msg` which combines
//! the policy's exponential schedule with `Retry-After` parsing
//! from the error message. Caps at 5 minutes (per the existing
//! policy). The wrapper sleeps with `tokio::time::sleep` so the
//! caller's task can still be aborted during backoff via the
//! signal.
//!
//! ## What this does NOT handle
//!
//! - `ContextOverflow` auto-compact. Pi/dirge's existing runner
//!   emits a distinct `ContextOverflow` AgentEvent for the UI to
//!   handle (run `/compress` + respawn). The new loop emits
//!   `Error` for a context-length error; the bridge could
//!   re-classify and emit `ContextOverflow` instead. Phase 4.5h
//!   wires this when flipping the default.
//! - Mid-tool-result retry. Once a tool has executed, side
//!   effects are permanent; the loop must not re-run them.
//!   `had_tool_calls` is the marker here (gated equivalently to
//!   pi: any tool dispatch → no retry).

use std::sync::Arc;

use futures::stream::StreamExt;

use crate::agent::recovery::{RecoveryPolicy, classify_error};

use super::message::{DeltaPhase, StreamEvent};
use super::stream::StreamFn;

#[cfg(test)]
use super::tool::AbortSignal;

/// Wrap an inner `StreamFn` with retry-on-transient-error
/// semantics.
///
/// Algorithm per outer invocation:
///   1. `attempts = 0`
///   2. Call inner stream; iterate events:
///      - non-Error events pass through; track `committed` if
///        the event represents observable content
///      - Error before committed → classify; if retryable AND
///        attempts < max_retries → sleep backoff, increment
///        attempts, restart inner stream
///      - Error after committed → pass through; stop
///      - non-retryable Error → pass through; stop
///   3. Stream ends normally → stop
///
/// Cancellation: `signal.is_cancelled()` is checked at retry
/// boundaries (between attempts). Mid-stream cancellation is
/// the inner stream's responsibility (the rig adapter's
/// `AbortSignal` poll inside its own loop).
pub fn retrying_stream_fn(inner: StreamFn, policy: RecoveryPolicy) -> StreamFn {
    let policy = Arc::new(policy);
    Arc::new(move |ctx, opts: super::stream::StreamOptions| {
        let inner = inner.clone();
        let policy = policy.clone();
        let signal_outer = opts.signal.clone();
        Box::pin(async_stream::stream! {
            let mut attempts: usize = 0;
            loop {
                if signal_outer.is_cancelled() {
                    yield StreamEvent::Error {
                        error: "operation aborted before stream started".to_string(),
                    };
                    return;
                }
                let mut inner_stream = inner(ctx.clone(), opts.clone());

                // Per-attempt state.
                let mut committed = false;
                let mut retry_msg: Option<String> = None;

                while let Some(evt) = inner_stream.next().await {
                    match &evt {
                        StreamEvent::Error { error } => {
                            // Decide on retry vs surface BEFORE
                            // yielding the Error event downstream.
                            if !committed {
                                let kind = classify_error(error);
                                if policy.should_retry(attempts, kind) {
                                    retry_msg = Some(error.clone());
                                    // Don't yield this Error — we're
                                    // about to retry.
                                    break;
                                }
                            }
                            // Either retry exhausted, non-retryable
                            // kind, or committed → surface.
                            yield evt;
                            return;
                        }
                        StreamEvent::Delta { phase, .. } => {
                            // Real content streamed → no future
                            // retry is safe (would duplicate).
                            if is_content_delta(*phase) {
                                committed = true;
                            }
                            yield evt;
                        }
                        StreamEvent::Done { .. } => {
                            // Normal completion — pass through and
                            // terminate. (Some non-content phases
                            // may have streamed but the run finished
                            // cleanly.)
                            yield evt;
                            return;
                        }
                        StreamEvent::Start { .. } => {
                            // Start synthesized at stream begin —
                            // doesn't itself count as content;
                            // re-emitting it on retry is fine
                            // (consumers see Start on every
                            // attempt). We still yield it so the
                            // first-attempt consumer sees the
                            // expected sequence.
                            yield evt;
                        }
                        StreamEvent::Retry { .. } => {
                            // Inner stream should never produce
                            // Retry — only the outer retry loop
                            // does. Treat as unexpected terminal.
                            yield evt;
                            return;
                        }
                    }
                }

                // Inner stream exited the loop without Done. Either
                // we broke for retry, or the stream ended without
                // emitting a terminal event.
                match retry_msg {
                    Some(err_msg) => {
                        let backoff = policy.backoff_duration_for_msg(attempts, &err_msg);
                        // Sleep — `tokio::time::sleep` is
                        // cancellable via task::abort but not via
                        // our AbortSignal. Honor the latter with
                        // a poll-after-sleep guard.
                        tokio::time::sleep(backoff).await;
                        if signal_outer.is_cancelled() {
                            yield StreamEvent::Error {
                                error: "operation aborted during retry backoff".to_string(),
                            };
                            return;
                        }
                        attempts += 1;
                        // PROV-2: surface the retry so the UI can
                        // show a banner instead of freezing.
                        yield StreamEvent::Retry {
                            attempt: attempts as u32,
                            delay_ms: backoff.as_millis() as u64,
                            error: err_msg,
                        };
                        // Loop continues — next outer iteration
                        // calls the inner stream again.
                    }
                    None => {
                        // Stream closed without Done and without
                        // an Error we wanted to retry. Treat as
                        // natural termination — caller's
                        // stream_assistant_response has its own
                        // defensive fallback at this point.
                        return;
                    }
                }
            }
        })
    })
}

/// Phases that represent observable downstream content. Once any
/// of these have streamed, we don't retry — the consumer has
/// already seen output and re-running would duplicate it.
///
/// PROV-5: tool-call deltas are NOT included. The model emitting
/// a tool-call JSON fragment is not the same as the tool actually
/// running — dispatch happens AFTER the stream ends, downstream
/// of this retry layer. A 503 mid-tool-call-emission is therefore
/// retryable; the consumer resets its partial-assistant state on
/// `StreamEvent::Retry` (see `stream.rs`) so the second attempt's
/// tool calls don't accumulate on top of the first attempt's.
fn is_content_delta(phase: DeltaPhase) -> bool {
    matches!(
        phase,
        DeltaPhase::TextStart
            | DeltaPhase::TextDelta
            | DeltaPhase::ThinkingStart
            | DeltaPhase::ThinkingDelta
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::message::{AssistantMessage, ContentBlock, StopReason};
    use crate::agent::agent_loop::stream::LlmContext;
    use crate::agent::recovery::RecoveryPolicy;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Drain a stream into a Vec.
    async fn drain(
        mut s: std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>,
    ) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(e) = s.next().await {
            out.push(e);
        }
        out
    }

    /// Build a StreamFn that returns a canned list of events,
    /// optionally tracking call count.
    fn canned_stream_fn(events: Vec<Vec<StreamEvent>>) -> StreamFn {
        let counter = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(Mutex::new(events));
        Arc::new(move |_ctx, _opts| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let attempts = events.lock().unwrap();
            let attempt_events = attempts.get(n).cloned().unwrap_or_default();
            Box::pin(futures::stream::iter(attempt_events))
        })
    }

    /// Stream factory + call counter pair for assertions.
    fn counted_canned(events: Vec<Vec<StreamEvent>>) -> (StreamFn, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(Mutex::new(events));
        let counter_clone = counter.clone();
        let factory: StreamFn = Arc::new(move |_ctx, _opts| {
            let n = counter_clone.fetch_add(1, Ordering::SeqCst);
            let attempts = events.lock().unwrap();
            let attempt_events = attempts.get(n).cloned().unwrap_or_default();
            Box::pin(futures::stream::iter(attempt_events))
        });
        (factory, counter)
    }

    fn ctx() -> LlmContext {
        LlmContext {
            system_prompt: String::new(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
        }
    }

    fn empty_assistant() -> AssistantMessage {
        AssistantMessage::new(vec![], StopReason::Stop)
    }

    fn assistant_with(text: &str) -> AssistantMessage {
        AssistantMessage::new(
            vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            StopReason::Stop,
        )
    }

    /// No errors → wrapper passes events through unchanged.
    #[tokio::test]
    async fn passthrough_when_no_errors() {
        let inner = canned_stream_fn(vec![vec![
            StreamEvent::Start {
                partial: empty_assistant(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant_with("hello"),
                usage: None,
            },
        ]]);
        let wrapped = retrying_stream_fn(inner, RecoveryPolicy::default());
        let events = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], StreamEvent::Start { .. }));
        assert!(matches!(events[1], StreamEvent::Done { .. }));
    }

    /// Network error on attempt 0; success on attempt 1 →
    /// wrapper retries; total 2 inner calls; consumer sees the
    /// successful stream's events.
    #[tokio::test]
    async fn retries_on_network_error() {
        // Cap attempts to a short backoff for the test.
        let policy = RecoveryPolicy::default();
        // Inner stream attempts:
        //  - attempt 0: only Error (network)
        //  - attempt 1: Start + Done
        let (factory, counter) = counted_canned(vec![
            vec![StreamEvent::Error {
                error: "connection timed out".to_string(),
            }],
            vec![
                StreamEvent::Start {
                    partial: empty_assistant(),
                },
                StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: assistant_with("after retry"),
                    usage: None,
                },
            ],
        ]);
        let wrapped = retrying_stream_fn(factory, policy);

        // We can't easily mock the sleep without injecting time;
        // accept the ~1s backoff cost for this test. Use
        // tokio::time::pause to make it free.
        tokio::time::pause();
        let drain_task = tokio::spawn(async move {
            drain(wrapped(
                ctx(),
                crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
            ))
            .await
        });
        // Advance virtual time to skip the backoff sleep.
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        let events = drain_task.await.unwrap();

        // Inner was called twice (one retry).
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        // Consumer events are FROM THE SUCCESSFUL ATTEMPT only.
        // The retried attempt's Error was suppressed.
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
        assert_eq!(kinds, vec!["retry", "start", "done"]);
    }

    /// Auth error → no retry, error passes through.
    #[tokio::test]
    async fn does_not_retry_auth_error() {
        let (factory, counter) = counted_canned(vec![vec![StreamEvent::Error {
            error: "401 unauthorized: invalid api key".to_string(),
        }]]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let events = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Error { .. }));
    }

    /// Context-length error → no retry.
    #[tokio::test]
    async fn does_not_retry_context_length_error() {
        let (factory, counter) = counted_canned(vec![vec![StreamEvent::Error {
            error: "context length exceeded: prompt is too long".to_string(),
        }]]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let events = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(matches!(events[0], StreamEvent::Error { .. }));
    }

    /// Error AFTER content streamed → no retry, error passes
    /// through. Mid-stream retries would emit duplicate tokens
    /// to the consumer.
    #[tokio::test]
    async fn does_not_retry_after_content_committed() {
        let (factory, counter) = counted_canned(vec![vec![
            StreamEvent::Start {
                partial: empty_assistant(),
            },
            // Real content streamed:
            StreamEvent::Delta {
                partial: assistant_with("partial "),
                phase: DeltaPhase::TextStart,
            },
            // Then network error:
            StreamEvent::Error {
                error: "connection reset".to_string(),
            },
        ]]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let events = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        ))
        .await;
        // No retry — only one inner call.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        // Consumer saw Start, Delta, Error in that order.
        assert!(matches!(events[0], StreamEvent::Start { .. }));
        assert!(matches!(
            events[1],
            StreamEvent::Delta {
                phase: DeltaPhase::TextStart,
                ..
            }
        ));
        assert!(matches!(events[2], StreamEvent::Error { .. }));
    }

    /// Rate-limit error retries, retry-after-ms gets honoured
    /// (we test this indirectly via virtual time — advance past
    /// the retry-after and check the retry happened).
    #[tokio::test]
    async fn retries_on_rate_limit_with_retry_after() {
        let (factory, counter) = counted_canned(vec![
            vec![StreamEvent::Error {
                // RateLimit kind triggers via the "rate limit"
                // substring in classify_error. retry-after-ms is
                // 50ms so a short virtual-time advance covers it.
                error: "rate limit hit. retry-after-ms: 50".to_string(),
            }],
            vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant_with("ok"),
                usage: None,
            }],
        ]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());

        tokio::time::pause();
        let task = tokio::spawn(async move {
            drain(wrapped(
                ctx(),
                crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
            ))
            .await
        });
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        let events = task.await.unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
    }

    /// Max retries exceeded → final Error surfaces.
    #[tokio::test]
    async fn surfaces_error_after_max_retries() {
        // Default policy has 5 max retries. 6 consecutive Network
        // errors → first 5 retried, 6th surfaces.
        let attempts: Vec<Vec<StreamEvent>> = (0..6)
            .map(|_| {
                vec![StreamEvent::Error {
                    error: "network: connection timed out".to_string(),
                }]
            })
            .collect();
        let (factory, counter) = counted_canned(attempts);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());

        tokio::time::pause();
        let task = tokio::spawn(async move {
            drain(wrapped(
                ctx(),
                crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
            ))
            .await
        });
        // Advance plenty for all backoffs to elapse.
        tokio::time::advance(std::time::Duration::from_secs(600)).await;
        let events = task.await.unwrap();

        // max_retries = 5: attempts 0..=4 retry, attempt 5 surfaces.
        // 0 fails → retry; 1,2,3,4 fail → retry; 5 fails →
        // !should_retry → surface. Total: 6 inner calls.
        assert_eq!(counter.load(Ordering::SeqCst), 6);
        assert!(matches!(events.last(), Some(StreamEvent::Error { .. })));
    }

    /// AbortSignal cancelled before first attempt → emit
    /// abort error, no inner calls.
    #[tokio::test]
    async fn aborted_before_attempt_emits_error() {
        let (factory, counter) = counted_canned(vec![]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let signal = AbortSignal::new();
        signal.cancel();
        let events = drain(wrapped(
            ctx(),
            crate::agent::agent_loop::StreamOptions::from_signal(signal),
        ))
        .await;
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(matches!(events[0], StreamEvent::Error { .. }));
    }

    /// Cancellation during retry backoff stops further attempts.
    /// Sequence: first attempt runs and fails → wrapper enters
    /// backoff sleep → test cancels → wrapper observes signal
    /// after sleep and emits abort Error. Second attempt's
    /// canned event is never delivered.
    #[tokio::test]
    async fn aborted_during_backoff_emits_error() {
        let (factory, counter) = counted_canned(vec![
            vec![StreamEvent::Error {
                error: "network glitch".to_string(),
            }],
            // Second attempt would succeed, but we abort first.
            vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant_with("never seen"),
                usage: None,
            }],
        ]);
        let wrapped = retrying_stream_fn(factory, RecoveryPolicy::default());
        let signal = AbortSignal::new();
        let signal_clone = signal.clone();

        tokio::time::pause();
        let task = tokio::spawn(async move {
            drain(wrapped(
                ctx(),
                crate::agent::agent_loop::StreamOptions::from_signal(signal_clone),
            ))
            .await
        });
        // Let the spawned task start and run the first inner
        // attempt. Without yielding, the cancel below races with
        // the task's signal check at the top of the loop —
        // making the test flake. yield_now hands the runtime to
        // the spawn so the first attempt can run + start
        // sleeping in the backoff before we cancel.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        // First attempt should have run by now and the wrapper
        // is in `tokio::time::sleep`. Cancel while it's sleeping.
        signal.cancel();
        // Advance virtual time past the backoff so the sleep
        // returns and the post-sleep cancellation check fires.
        tokio::time::advance(std::time::Duration::from_secs(600)).await;
        let events = task.await.unwrap();

        // Only the first attempt ran (second canned events
        // never consumed).
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        // Final event surfaces the abort.
        assert!(matches!(events.last(), Some(StreamEvent::Error { .. })));
    }

    /// `is_content_delta` returns true for text + thinking phases
    /// (which the user has already seen) and false for everything
    /// else. PROV-5: tool-call deltas are explicitly NOT committed
    /// — they're buffered until the stream ends and only then
    /// dispatched, so a 503 during ToolCallStart/Delta/End is
    /// safely retryable. The consumer in `stream.rs` resets its
    /// partial-assistant state on `StreamEvent::Retry`.
    #[test]
    fn is_content_delta_classifies_phases() {
        for phase in [
            DeltaPhase::TextStart,
            DeltaPhase::TextDelta,
            DeltaPhase::ThinkingStart,
            DeltaPhase::ThinkingDelta,
        ] {
            assert!(is_content_delta(phase), "{phase:?} should be content");
        }
        for phase in [
            DeltaPhase::TextEnd,
            DeltaPhase::ThinkingEnd,
            DeltaPhase::ToolCallStart,
            DeltaPhase::ToolCallDelta,
            DeltaPhase::ToolCallEnd,
        ] {
            assert!(!is_content_delta(phase), "{phase:?} should NOT be content");
        }
    }
}
