use compact_str::CompactString;
use futures::StreamExt;
use rig::agent::{Agent, MultiTurnStreamItem};
use rig::completion::{CompletionModel, Message};
use rig::message::ToolResultContent;
use rig::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingChat};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agent::recovery::{self, ErrorKind, RecoveryPolicy};
use crate::agent::tools::ToolCache;
use crate::event::AgentEvent;
use crate::session::{MessageRole, Session};

/// Turn-boundary detector. Rig's multi-turn stream is a flat sequence
/// of assistant content + tool results + a final response; consumers
/// downstream (plugin hooks in P3, session-tree branch accounting in
/// P4) want to bracket per-turn observability. Track the boundaries
/// here as a pure state machine so the integration with rig stays
/// thin and the rules are unit-testable without spinning up an LLM.
///
/// One "turn" = one LLM call + the tool calls it dispatched + the
/// tool results returning. A pure-text response is one turn; a run
/// with two cycles of tool calls is two turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnState {
    /// Before the first assistant content or between turns. `next_index`
    /// is the index of the next turn that will open.
    Idle { next_index: u32 },
    /// Currently inside turn `index` — assistant content is streaming
    /// or being collected. Stays in this state across multiple
    /// Text/Reasoning/ToolCall events.
    InTurn { index: u32 },
    /// Inside turn `index`, after at least one tool result arrived.
    /// The boundary is only emitted lazily — when the next assistant
    /// content begins (→ TurnEnd index, then TurnStart index+1) or
    /// when the stream ends (→ TurnEnd index). This avoids needing
    /// lookahead to know whether more tool results are still coming.
    AwaitingNext { index: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Boundary {
    Start { index: u32 },
    End { index: u32 },
}

#[derive(Debug)]
pub(crate) struct TurnTracker {
    state: TurnState,
}

impl TurnTracker {
    pub fn new() -> Self {
        Self {
            state: TurnState::Idle { next_index: 0 },
        }
    }

    /// Call before forwarding an assistant Text / Reasoning / ToolCall
    /// event to the event channel. Returns the boundary events that
    /// must be emitted *first* (typically a TurnStart, optionally a
    /// preceding TurnEnd if we're transitioning from one turn to the
    /// next after tool results arrived).
    pub fn observe_assistant_content(&mut self) -> Vec<Boundary> {
        match self.state {
            TurnState::Idle { next_index } => {
                self.state = TurnState::InTurn { index: next_index };
                vec![Boundary::Start { index: next_index }]
            }
            TurnState::InTurn { .. } => Vec::new(),
            TurnState::AwaitingNext { index } => {
                let next = index + 1;
                self.state = TurnState::InTurn { index: next };
                vec![Boundary::End { index }, Boundary::Start { index: next }]
            }
        }
    }

    /// Call after forwarding a ToolResult event. Tool results don't
    /// emit boundaries directly — only the *next* assistant content
    /// (or stream end) does — so this just advances state.
    pub fn observe_tool_result(&mut self) {
        match self.state {
            TurnState::InTurn { index } | TurnState::AwaitingNext { index } => {
                self.state = TurnState::AwaitingNext { index };
            }
            TurnState::Idle { .. } => {
                // Shouldn't happen in a well-formed rig stream — a tool
                // result before any assistant content. Leave state alone.
            }
        }
    }

    /// Call when the stream terminates (FinalResponse, Error, or
    /// interjection abort). Emits a closing TurnEnd if a turn is open.
    pub fn observe_stream_end(&mut self) -> Vec<Boundary> {
        match self.state {
            TurnState::InTurn { index } | TurnState::AwaitingNext { index } => {
                self.state = TurnState::Idle {
                    next_index: index + 1,
                };
                vec![Boundary::End { index }]
            }
            TurnState::Idle { .. } => Vec::new(),
        }
    }
}

pub struct AgentRunner {
    pub event_rx: mpsc::Receiver<AgentEvent>,
    /// Handle to the spawned tokio task. The UI calls `abort()` on interrupt
    /// so in-flight LLM calls and tool execution actually stop, rather than
    /// running to completion in the background and emitting permission
    /// prompts after the user thought they cancelled.
    pub task: JoinHandle<()>,
    /// Send a unit signal to ask the runner to stop the stream at the next
    /// safe boundary (after the current tool call's result). The runner
    /// emits `AgentEvent::Interjected` with whatever assistant text had
    /// streamed so far, and the UI is responsible for queueing the next
    /// user turn. Unbounded because the signal payload is just `()`.
    pub interject_tx: mpsc::UnboundedSender<()>,
}

pub fn convert_history(session: &Session) -> Vec<Message> {
    use rig::OneOrMany;
    use rig::completion::message::AssistantContent;
    let (summary, first_kept) = session.compacted_context();
    let mut messages = Vec::new();

    if let Some(summary) = summary {
        messages.push(Message::system(format!(
            "[Previous conversation summary]\n{}",
            summary
        )));
    }

    for msg in &session.messages[first_kept..] {
        match msg.role {
            MessageRole::User => messages.push(Message::user(msg.content.to_string())),
            MessageRole::System => messages.push(Message::system(msg.content.to_string())),
            MessageRole::Assistant => {
                // Phase 3: if this assistant message has structured
                // tool calls, emit a single Assistant message with
                // text + tool_use content parts, followed by ONE
                // tool_result User message per call. The pairing
                // matches opencode's `toModelMessagesEffect`
                // (`message-v2.ts:630-899`); Anthropic + OpenAI
                // reject orphan tool_use blocks so we always emit a
                // result, marking Interrupted/Failed as error text
                // rather than skipping. Bare assistant messages
                // (no tool_calls) keep the prior simple shape.
                if msg.tool_calls.is_empty() {
                    messages.push(Message::assistant(msg.content.to_string()));
                    continue;
                }

                // Build the Assistant message's content blocks: text
                // first (if any) then each ToolCall.
                let mut parts: Vec<AssistantContent> = Vec::new();
                if !msg.content.is_empty() {
                    parts.push(AssistantContent::text(msg.content.to_string()));
                }
                for tc in &msg.tool_calls {
                    parts.push(AssistantContent::tool_call(
                        tc.id.clone(),
                        tc.name.clone(),
                        tc.args.clone(),
                    ));
                }
                // OneOrMany::many requires at least one element; we
                // always have at least one ToolCall here since
                // tool_calls is non-empty.
                let content = if parts.len() == 1 {
                    OneOrMany::one(parts.pop().unwrap())
                } else {
                    OneOrMany::many(parts).expect("non-empty parts vec")
                };
                messages.push(Message::Assistant { id: None, content });

                // One User tool_result per call. State maps to:
                //  Completed  → result text verbatim
                //  Interrupted → "[Tool execution was interrupted]"
                //  Failed     → "[Tool error: <message>]"
                for tc in &msg.tool_calls {
                    let body = match &tc.state {
                        crate::session::ToolCallState::Completed { result } => result.clone(),
                        crate::session::ToolCallState::Interrupted => {
                            "[Tool execution was interrupted]".to_string()
                        }
                        crate::session::ToolCallState::Failed { error } => {
                            format!("[Tool error: {}]", error)
                        }
                    };
                    messages.push(Message::tool_result(tc.id.clone(), body));
                }
            }
        }
    }

    messages
}

/// Outcome of a streaming pass — used by the retry loop to decide whether
/// it's safe to re-issue the request. We never buffer events themselves;
/// they're sent to the UI as they arrive so the user sees progress in real
/// time.
#[derive(Default)]
struct StreamOutcome {
    had_tool_calls: bool,
    error: Option<String>,
    /// Set when the UI requested an interjection and the runner honored it
    /// at a tool-result boundary. When true the retry loop skips retries and
    /// the run is considered terminated for this turn.
    interjected: bool,
}

async fn run_stream<M, P>(
    agent: &Agent<M, P>,
    prompt: &str,
    history: Vec<Message>,
    event_tx: &mpsc::Sender<AgentEvent>,
    interject_rx: &mut mpsc::UnboundedReceiver<()>,
) -> StreamOutcome
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
    P: rig::agent::PromptHook<M> + 'static,
{
    let mut outcome = StreamOutcome::default();
    let mut stream = agent.stream_chat(prompt.to_string(), history).await;
    // Accumulate assistant text so we can hand it back to the UI when an
    // interjection cuts the run short — otherwise the partial response would
    // be lost (the UI also tracks it, but echoing it here keeps the event
    // payload self-contained).
    let mut partial = String::new();
    let mut turns = TurnTracker::new();

    // Helper: forward any pending turn-boundary events. Called before
    // assistant-content events (to emit TurnStart / TurnEnd+TurnStart)
    // and at stream end (to emit a closing TurnEnd).
    async fn flush_boundaries(boundaries: Vec<Boundary>, event_tx: &mpsc::Sender<AgentEvent>) {
        for b in boundaries {
            let ev = match b {
                Boundary::Start { index } => AgentEvent::TurnStart { index },
                Boundary::End { index } => AgentEvent::TurnEnd { index },
            };
            let _ = event_tx.send(ev).await;
        }
    }

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                flush_boundaries(turns.observe_assistant_content(), event_tx).await;
                partial.push_str(&text.text);
                let _ = event_tx
                    .send(AgentEvent::Token(CompactString::from(text.text)))
                    .await;
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(
                r,
            ))) => {
                flush_boundaries(turns.observe_assistant_content(), event_tx).await;
                let _ = event_tx
                    .send(AgentEvent::Reasoning(CompactString::new(r.display_text())))
                    .await;
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                tool_call,
                ..
            })) => {
                flush_boundaries(turns.observe_assistant_content(), event_tx).await;
                outcome.had_tool_calls = true;
                let _ = event_tx
                    .send(AgentEvent::ToolCall {
                        id: CompactString::from(tool_call.id),
                        name: CompactString::from(tool_call.function.name),
                        args: tool_call.function.arguments,
                    })
                    .await;
            }
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                tool_result,
                ..
            })) => {
                outcome.had_tool_calls = true;
                let mut output = String::new();
                for c in tool_result.content.iter() {
                    if let ToolResultContent::Text(t) = c {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(&t.text);
                    }
                }
                let _ = event_tx
                    .send(AgentEvent::ToolResult {
                        id: CompactString::from(tool_result.id),
                        output: CompactString::from(output),
                    })
                    .await;
                turns.observe_tool_result();
                // Tool-result boundaries are the only safe place to honor an
                // interjection: the LLM has just received the result and is
                // about to plan its next action, so cutting here preserves
                // tool-call/result pairing in the rig conversation state.
                if interject_rx.try_recv().is_ok() {
                    // Drain any further pending signals so a flurry of typing
                    // doesn't queue them up for the *next* run.
                    while interject_rx.try_recv().is_ok() {}
                    flush_boundaries(turns.observe_stream_end(), event_tx).await;
                    let estimated_tokens = Session::estimate_tokens(&partial);
                    outcome.interjected = true;
                    let _ = event_tx
                        .send(AgentEvent::Interjected {
                            partial_response: CompactString::from(partial.as_str()),
                            tokens: estimated_tokens,
                        })
                        .await;
                    return outcome;
                }
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                flush_boundaries(turns.observe_stream_end(), event_tx).await;
                let response_text = res.response();
                let estimated_tokens = Session::estimate_tokens(response_text);
                let _ = event_tx
                    .send(AgentEvent::Done {
                        response: CompactString::from(response_text),
                        tokens: estimated_tokens,
                        cost: 0.0,
                    })
                    .await;
                return outcome;
            }
            Err(e) => {
                flush_boundaries(turns.observe_stream_end(), event_tx).await;
                outcome.error = Some(e.to_string());
                return outcome;
            }
            _ => {}
        }
    }
    // Stream ended without a FinalResponse (rare; usually means the
    // provider closed the stream unexpectedly). Still close any open turn.
    flush_boundaries(turns.observe_stream_end(), event_tx).await;
    outcome
}

pub fn spawn_agent<M, P>(
    agent: Agent<M, P>,
    prompt: String,
    history: Vec<Message>,
    cache: ToolCache,
) -> AgentRunner
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
    P: rig::agent::PromptHook<M> + 'static,
{
    cache.clear();
    let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);
    let (interject_tx, mut interject_rx) = mpsc::unbounded_channel::<()>();

    let task = tokio::spawn(async move {
        let policy = RecoveryPolicy::default();
        let mut attempts = 0;

        loop {
            let outcome = run_stream(
                &agent,
                &prompt,
                history.clone(),
                &event_tx,
                &mut interject_rx,
            )
            .await;

            // Interjection takes precedence over retry: the user asked us to
            // stop and listen, so don't quietly re-issue the same prompt.
            if outcome.interjected {
                break;
            }

            let msg = match outcome.error {
                None => break,
                Some(m) => m,
            };

            let kind = recovery::classify_error(&msg);

            // Auth and unknown errors surface immediately with a
            // user-friendly headline + hint + cause breakdown.
            if kind == ErrorKind::Auth || kind == ErrorKind::Other {
                let friendly = recovery::user_facing_error(&msg, attempts + 1);
                let _ = event_tx
                    .send(AgentEvent::Error(CompactString::new(friendly)))
                    .await;
                break;
            }

            // Context-length errors aren't retryable without
            // compaction — the friendly formatter already points the
            // user at /compress.
            if kind == ErrorKind::ContextLength {
                let friendly = recovery::user_facing_error(&msg, attempts + 1);
                let _ = event_tx
                    .send(AgentEvent::Error(CompactString::new(friendly)))
                    .await;
                break;
            }

            // If any tool calls were dispatched, their side effects already
            // executed. Retrying would re-run them. Surface the error
            // without retrying — events already streamed live, so the user
            // sees what got done.
            if outcome.had_tool_calls {
                let friendly = recovery::user_facing_error(&msg, attempts + 1);
                let err = format!(
                    "{}\n  ↳ note: tool side effects already applied; not retrying.",
                    friendly,
                );
                let _ = event_tx
                    .send(AgentEvent::Error(CompactString::new(err)))
                    .await;
                break;
            }

            if !policy.should_retry(attempts, kind) {
                let friendly = recovery::user_facing_error(&msg, attempts + 1);
                let retry_msg = format!("{}\n  ↳ note: retries exhausted.", friendly);
                let _ = event_tx
                    .send(AgentEvent::Error(CompactString::new(retry_msg)))
                    .await;
                break;
            }

            // Emit retry notification as reasoning
            let retry_msg = format!(
                "retrying ({kind:?} error, attempt {attempt}/{max})...",
                kind = kind,
                attempt = attempts + 1,
                max = policy.max_retries(),
            );
            let _ = event_tx
                .send(AgentEvent::Reasoning(CompactString::new(retry_msg)))
                .await;

            let delay = policy.backoff_duration(attempts);
            tokio::time::sleep(delay).await;
            attempts += 1;
        }
    });

    AgentRunner {
        event_rx,
        task,
        interject_tx,
    }
}

pub async fn run_print<M, P>(
    agent: &Agent<M, P>,
    prompt: &str,
    max_turns: usize,
) -> anyhow::Result<String>
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
    P: rig::agent::PromptHook<M> + 'static,
{
    let mut stream = agent
        .stream_chat(prompt.to_string(), Vec::<Message>::new())
        .multi_turn(max_turns)
        .await;

    let mut full_response = String::new();

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                full_response.push_str(&text.text);
                print!("{}", text.text);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(
                r,
            ))) => {
                eprint!("{}", r.display_text());
                let _ = std::io::Write::flush(&mut std::io::stderr());
            }
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("Error: {}", e);
                break;
            }
        }
    }

    println!();
    Ok(full_response)
}

#[cfg(test)]
mod turn_tracker_tests {
    use super::{Boundary, TurnTracker};

    /// Pure-text response (no tool calls): TurnStart 0 fires on first
    /// assistant content, TurnEnd 0 fires on stream end.
    #[test]
    fn pure_text_emits_one_turn_around_content() {
        let mut t = TurnTracker::new();
        assert_eq!(
            t.observe_assistant_content(),
            vec![Boundary::Start { index: 0 }]
        );
        // Subsequent content in the same turn doesn't emit.
        assert!(t.observe_assistant_content().is_empty());
        assert!(t.observe_assistant_content().is_empty());
        assert_eq!(t.observe_stream_end(), vec![Boundary::End { index: 0 }]);
    }

    /// Single tool call: turn 0 has text + tool call + tool result;
    /// turn 1 starts on the next assistant content (post-tool reply).
    #[test]
    fn single_tool_call_produces_two_turns() {
        let mut t = TurnTracker::new();
        // Turn 0 opens on first assistant text.
        assert_eq!(
            t.observe_assistant_content(),
            vec![Boundary::Start { index: 0 }]
        );
        // Tool result mid-turn doesn't emit anything; it just primes the
        // tracker for a possible next turn.
        t.observe_tool_result();
        // The next assistant content closes turn 0 and opens turn 1.
        assert_eq!(
            t.observe_assistant_content(),
            vec![Boundary::End { index: 0 }, Boundary::Start { index: 1 },]
        );
        // Stream end closes turn 1.
        assert_eq!(t.observe_stream_end(), vec![Boundary::End { index: 1 }]);
    }

    /// Multiple tool calls in one turn (parallel tools): turn 0
    /// receives several ToolResult events; only the final transition
    /// (or stream end) advances the turn index.
    #[test]
    fn multiple_tool_results_collapse_into_one_turn_boundary() {
        let mut t = TurnTracker::new();
        assert_eq!(
            t.observe_assistant_content(),
            vec![Boundary::Start { index: 0 }]
        );
        // Three tool results in a row — none emit boundaries.
        t.observe_tool_result();
        t.observe_tool_result();
        t.observe_tool_result();
        // Stream ends without a follow-up assistant message (e.g. agent
        // chose not to continue). Single closing TurnEnd, index unchanged.
        assert_eq!(t.observe_stream_end(), vec![Boundary::End { index: 0 }]);
    }

    /// Three back-to-back tool cycles produce three turns, with the
    /// boundary indices counting up monotonically.
    #[test]
    fn alternating_text_and_tools_advances_turn_index() {
        let mut t = TurnTracker::new();
        // Turn 0
        assert_eq!(
            t.observe_assistant_content(),
            vec![Boundary::Start { index: 0 }]
        );
        t.observe_tool_result();
        // → Turn 1
        assert_eq!(
            t.observe_assistant_content(),
            vec![Boundary::End { index: 0 }, Boundary::Start { index: 1 },]
        );
        t.observe_tool_result();
        // → Turn 2
        assert_eq!(
            t.observe_assistant_content(),
            vec![Boundary::End { index: 1 }, Boundary::Start { index: 2 },]
        );
        assert_eq!(t.observe_stream_end(), vec![Boundary::End { index: 2 }]);
    }

    /// Empty stream (no assistant content, no tool results, just an
    /// immediate end) emits no boundaries at all.
    #[test]
    fn empty_stream_emits_no_boundaries() {
        let mut t = TurnTracker::new();
        assert!(t.observe_stream_end().is_empty());
    }

    /// Stream end after assistant content but before any tool results
    /// still closes the open turn. This is the "tool was called but
    /// the run aborted before results came back" case.
    #[test]
    fn stream_end_during_tool_dispatch_still_closes_turn() {
        let mut t = TurnTracker::new();
        // Assistant emitted a tool call but no result yet.
        assert_eq!(
            t.observe_assistant_content(),
            vec![Boundary::Start { index: 0 }]
        );
        assert_eq!(t.observe_stream_end(), vec![Boundary::End { index: 0 }]);
    }

    /// Lone tool result with no preceding assistant content is silently
    /// dropped (shouldn't happen with rig, but tracker stays robust).
    #[test]
    fn tool_result_without_open_turn_is_a_noop() {
        let mut t = TurnTracker::new();
        t.observe_tool_result();
        // No turn was opened, so stream-end emits nothing either.
        assert!(t.observe_stream_end().is_empty());
    }

    /// A fresh `observe_assistant_content` AFTER `observe_stream_end`
    /// opens a new turn at index+1 — supports the runner's retry loop
    /// where the same tracker survives across stream restarts.
    #[test]
    fn tracker_can_be_reused_across_stream_restarts() {
        let mut t = TurnTracker::new();
        t.observe_assistant_content(); // turn 0 start
        t.observe_stream_end(); // turn 0 end
        // Next stream's first content opens turn 1, not 0.
        assert_eq!(
            t.observe_assistant_content(),
            vec![Boundary::Start { index: 1 }]
        );
    }
}
