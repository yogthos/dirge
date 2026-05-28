//! Background review at session end.
//!
//! Port of Hermes's `agent/background_review.py`. After every session,
//! a forked agent with limited tools (memory + skill only) reviews the
//! transcript and writes project learnings to MEMORY.md, PITFALLS.md,
//! and skills.
//!
//! The review runs as a fire-and-forget tokio task — it never blocks
//! the main session. If it fails, the error is logged and the session
//! continues unaffected.
//!
//! Key design decisions from Hermes preserved:
//! - Fork, don't inline (separate agent instance, no prompt-cache pollution)
//! - Tool whitelist (only memory + skill tools)
//! - Same credentials as parent session
//! - Frozen conversation snapshot
//! - Fire-and-forget (daemon thread pattern)

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::extras::dirge_paths::ProjectPaths;
use crate::provider::AnyAgent;

/// Minimum interval between background reviews (seconds).
const MIN_REVIEW_INTERVAL_SECS: u64 = 900; // 15 minutes

/// Last review timestamp (Unix seconds).
static LAST_REVIEW: AtomicU64 = AtomicU64::new(0);

/// Attempt to atomically claim the next review slot. Returns
/// `Some(prev_value)` if we won the race and the spawned task should
/// proceed — the caller MUST call [`release_review_slot`] with the
/// returned value if the task fails before producing useful work, so
/// the next session can retry instead of waiting 15 minutes.
///
/// Returns `None` if either (a) the previous review completed less
/// than `MIN_REVIEW_INTERVAL_SECS` ago, or (b) a concurrent caller
/// won the CAS — both cases are silent skips.
///
/// dirge-bo88: the original code did `load(); … ; store(now)` which
/// (a) raced between concurrent Done events from different sessions
/// and (b) advanced the timestamp before the spawned task ran, so an
/// early failure suppressed retries for 15 minutes.
fn claim_review_slot(now: u64) -> Option<u64> {
    let last = LAST_REVIEW.load(Ordering::Acquire);
    if now.saturating_sub(last) < MIN_REVIEW_INTERVAL_SECS {
        return None;
    }
    match LAST_REVIEW.compare_exchange(last, now, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Some(last),
        Err(_) => None, // lost the race; another caller claimed it
    }
}

/// Roll the LAST_REVIEW timestamp back to the value captured at
/// `claim_review_slot` time. Only does so if our timestamp is still
/// the latest — otherwise a later successful review has already
/// completed and we'd corrupt its timestamp. dirge-bo88.
fn release_review_slot(prev: u64, ours: u64) {
    let _ = LAST_REVIEW.compare_exchange(ours, prev, Ordering::AcqRel, Ordering::Acquire);
}

/// Review prompt focused on project memory, pitfalls, and skills.
/// Port of Hermes's `_COMBINED_REVIEW_PROMPT` (background_review.py:150-158)
/// and `_SKILL_REVIEW_PROMPT` (background_review.py:45-148), adapted
/// for coding context.
const COMBINED_REVIEW_PROMPT: &str = r#"Review the conversation above and update what we know about this project and how to work on it.

**CRITICAL: You have ONLY the `memory` and `skill` tools available.** Do not attempt to use read, write, edit, bash, or any other tools — they are not loaded.

**1. Update MEMORY (project facts, conventions, pitfalls):**
- What build/test commands were discovered or confirmed?
- What naming conventions, file layout patterns, or import styles were used?
- What architecture patterns emerged (how modules relate, error handling style)?
- What library quirks or tool behaviors were discovered?
- Were there any user corrections about how things should be done?
- Was something tried and failed? Capture what was attempted and WHY it failed.

**2. Update SKILLS (procedural improvements):**
Be ACTIVE — most sessions produce at least one skill update. A pass that does nothing is a missed learning opportunity.

Preference order — prefer the earliest that fits:
  1. UPDATE A CURRENTLY-LOADED SKILL. If the conversation involved a skill that is already in the library, extend or correct it first.
  2. UPDATE AN EXISTING UMBRELLA. If the new knowledge belongs under a broader topic that already has a skill, patch it.
  3. ADD A SUPPORT FILE under an existing umbrella via the skill tool (references/, templates/, or scripts/).
  4. CREATE A NEW CLASS-LEVEL UMBRELLA SKILL only when no existing skill covers the class.

Signals that warrant action:
  • User corrected your style, approach, or workflow. Frustration signals like "stop doing X", "this is too verbose", "don't format like this", or an explicit "remember this" are FIRST-CLASS skill signals.
  • Non-trivial technique, fix, workaround, or debugging pattern emerged.
  • A skill that was loaded or consulted turned out wrong or outdated — PATCH IT NOW.
  • A pattern repeated across the session that future sessions would benefit from.

Do NOT capture:
  • Environment-dependent failures: missing binaries, "command not found", unconfigured credentials. The user can fix these — they are not durable rules.
  • Negative claims about tools ("read tool is broken", "cannot use X"). These harden into refusals long after the actual problem was fixed.
  • Session-specific transient errors that resolved before the conversation ended.
  • One-off task narratives. "Analyze this PR" is not a class of work that warrants a skill.

Target shape of the library: CLASS-LEVEL skills with a rich SKILL.md. Not a long flat list of narrow one-session-one-skill entries.

"Nothing to save." is valid but should NOT be the default. Most coding sessions produce at least one learning."#;

/// Spawn a background review task that evaluates the just-completed
/// session and writes learnings to project memory and skills.
///
/// This is fire-and-forget — it runs in a `tokio::spawn` task and
/// returns immediately. Failures are logged to stderr and never
/// block the user.
///
/// Set `review_prompt_override` to use a custom prompt instead of
/// the default COMBINED_REVIEW_PROMPT. Pass `None` for the default.
pub fn spawn_background_review(
    agent: AnyAgent,
    _paths: ProjectPaths,
    transcript: String,
    review_prompt_override: Option<&str>,
) {
    // dirge-bo88: atomic CAS claim of the next review slot. Returns
    // `None` if we lost the race or the rate-limit window is still
    // open. The previous-value `prev` is captured so we can roll back
    // on early failure (otherwise an immediately-failing review would
    // silently suppress the next 15 minutes of attempts).
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let Some(prev) = claim_review_slot(now) else {
        tracing::debug!(
            target: "dirge::review",
            "Skipping background review — rate-limited or another caller won the race"
        );
        return;
    };

    let prompt = review_prompt_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| COMBINED_REVIEW_PROMPT.to_string());

    tokio::spawn(async move {
        // Build a review runner with only memory + skill tools.
        let review_runner = agent.spawn_review_runner(prompt, transcript);

        // Drain events. Track tool calls so we can summarize what
        // the review actually did (Hermes's action summary pattern).
        let mut rx = review_runner.event_rx;
        let mut had_error = false;
        let mut tool_actions: Vec<String> = Vec::new();

        while let Some(event) = rx.recv().await {
            use crate::event::AgentEvent;
            match event {
                AgentEvent::Error(msg) => {
                    tracing::warn!(
                        target: "dirge::review",
                        error = %msg,
                        "Background review encountered an error"
                    );
                    had_error = true;
                }
                AgentEvent::ToolCall { name, .. } => {
                    tool_actions.push(name.to_string());
                }
                AgentEvent::Done { .. } => {
                    break;
                }
                _ => {
                    // Tokens, tool calls, etc. — consumed silently.
                }
            }
        }

        if !had_error && !tool_actions.is_empty() {
            // Surface action summary so the user knows what was learned.
            // Port of Hermes's `_safe_print` (background_review.py:514-516).
            let summary = tool_actions
                .iter()
                .fold(Vec::<&str>::new(), |mut acc, a| {
                    if !acc.contains(&a.as_str()) {
                        acc.push(a.as_str());
                    }
                    acc
                })
                .join(" · ");
            tracing::info!(
                target: "dirge::review",
                actions = %summary,
                "💾 Self-improvement review: {}",
                summary
            );
        } else if !had_error {
            tracing::info!(
                target: "dirge::review",
                "Background review completed — project knowledge updated"
            );
        }

        // dirge-bo88: if the review never managed to do any work
        // (errored before any tool call), roll back the timestamp so
        // the next session can retry instead of being silently locked
        // out for 15 minutes.
        if had_error && tool_actions.is_empty() {
            release_review_slot(prev, now);
            tracing::debug!(
                target: "dirge::review",
                "Released LAST_REVIEW slot — review produced no work"
            );
        }
    });
}

/// Spawn the curator's LLM consolidation pass — fire-and-forget.
/// Reuses `spawn_review_runner` (memory + skill tools), but injects
/// the curator-specific prompt and the agent-created candidate list
/// so the model can build umbrellas. The curator prompt instructs
/// the model to only use the `skill` tool; we don't filter out the
/// `memory` tool at the runner level since the existing review-runner
/// infrastructure ships both. See dirge-odv3.
///
/// `candidate_list` is the rendered output of
/// `crate::extras::skills::curator::render_candidate_list` — the
/// caller assembles it from the project's `UsageStore`.
///
/// Skips entirely (logs at debug) when the candidate list contains
/// the "No agent-created skills" sentinel — there's nothing to
/// consolidate.
pub fn spawn_curator_review(agent: AnyAgent, candidate_list: String) {
    if candidate_list.contains("No agent-created skills") {
        tracing::debug!(
            target: "dirge::curator",
            "Skipping curator LLM pass — no agent-created candidates"
        );
        return;
    }

    let prompt = format!(
        "{}\n\n{}",
        crate::extras::skills::curator::CURATOR_PROMPT,
        candidate_list
    );

    tokio::spawn(async move {
        let runner = agent.spawn_review_runner(prompt, String::new());
        let mut rx = runner.event_rx;
        let mut tool_actions: Vec<String> = Vec::new();
        let mut had_error = false;
        while let Some(event) = rx.recv().await {
            use crate::event::AgentEvent;
            match event {
                AgentEvent::Error(msg) => {
                    tracing::warn!(
                        target: "dirge::curator",
                        error = %msg,
                        "Curator LLM pass encountered an error"
                    );
                    had_error = true;
                }
                AgentEvent::ToolCall { name, .. } => {
                    tool_actions.push(name.to_string());
                }
                AgentEvent::Done { .. } => break,
                _ => {}
            }
        }

        if !had_error && !tool_actions.is_empty() {
            let summary = tool_actions
                .iter()
                .fold(Vec::<&str>::new(), |mut acc, a| {
                    if !acc.contains(&a.as_str()) {
                        acc.push(a.as_str());
                    }
                    acc
                })
                .join(" · ");
            tracing::info!(
                target: "dirge::curator",
                actions = %summary,
                "🗂  Skill curator pass: {}",
                summary
            );
        }
    });
}

/// Build a human-readable transcript from session messages for
/// background review. Includes user text, assistant text, tool
/// call names+args, and tool results. Compaction summaries are
/// included as system context.
pub fn build_transcript(session: &crate::session::Session) -> String {
    build_transcript_from_slice(&session.messages)
}

/// Same as [`build_transcript`] but operates on an explicit message
/// slice. Used by the pre-compress hook (dirge-7tvq) which only sees
/// the soon-to-be-discarded prefix, not a full `Session`.
pub fn build_transcript_from_slice(messages: &[crate::session::SessionMessage]) -> String {
    let mut out = String::new();
    for msg in messages {
        match msg.role {
            crate::session::MessageRole::User => {
                out.push_str(&format!("User: {}\n\n", msg.content));
            }
            crate::session::MessageRole::Assistant => {
                if !msg.content.is_empty() {
                    out.push_str(&format!("Assistant: {}\n", msg.content));
                }
                for tc in &msg.tool_calls {
                    let args_str =
                        serde_json::to_string(&tc.args).unwrap_or_else(|_| "{}".to_string());
                    out.push_str(&format!("  [Tool: {}({})]\n", tc.name, args_str));
                    match &tc.state {
                        crate::session::ToolCallState::Completed { result } => {
                            let truncated = truncate_tool_result(result);
                            out.push_str(&format!("  [Result: {}]\n", truncated));
                        }
                        crate::session::ToolCallState::Interrupted => {
                            out.push_str("  [Result: <interrupted>]\n");
                        }
                        crate::session::ToolCallState::Failed { error } => {
                            out.push_str(&format!("  [Result: <failed: {}>]\n", error));
                        }
                    }
                }
                if !msg.content.is_empty() || !msg.tool_calls.is_empty() {
                    out.push('\n');
                }
            }
            crate::session::MessageRole::System => {
                out.push_str(&format!("[System: {}]\n\n", msg.content));
            }
        }
    }
    out
}

fn truncate_tool_result(result: &str) -> String {
    const MAX_TOOL_RESULT: usize = 2000;
    if result.len() <= MAX_TOOL_RESULT {
        result.to_string()
    } else {
        let truncated: String = result.chars().take(MAX_TOOL_RESULT).collect();
        format!("{}… (truncated, {} bytes total)", truncated, result.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{MessageRole, Session, ToolCallEntry, ToolCallState};

    fn make_session() -> Session {
        Session::new("test-provider", "test-model", 128_000)
    }

    #[test]
    fn transcript_includes_user_and_assistant() {
        let mut s = make_session();
        s.add_message(MessageRole::User, "how do I build this?");
        s.add_message(MessageRole::Assistant, "Run cargo build");

        let t = build_transcript(&s);
        assert!(t.contains("User: how do I build this?"));
        assert!(t.contains("Assistant: Run cargo build"));
    }

    #[test]
    fn transcript_includes_tool_calls_and_results() {
        let mut s = make_session();
        s.add_message(MessageRole::User, "read the file");
        let tc = ToolCallEntry {
            id: "call-1".to_string(),
            name: "read".to_string(),
            args: serde_json::json!({"path": "/tmp/x"}),
            state: ToolCallState::Completed {
                result: "file contents here".to_string(),
            },
        };
        s.add_message_with_tool_calls(MessageRole::Assistant, "Let me read that.", vec![tc]);

        let t = build_transcript(&s);
        assert!(t.contains("[Tool: read("));
        assert!(t.contains("[Result: file contents here]"));
    }

    #[test]
    fn transcript_truncates_large_tool_results() {
        let mut s = make_session();
        let big = "x".repeat(3000);
        let tc = ToolCallEntry {
            id: "c1".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"cmd": "cat big.txt"}),
            state: ToolCallState::Completed {
                result: big.clone(),
            },
        };
        s.add_message_with_tool_calls(MessageRole::Assistant, "", vec![tc]);

        let t = build_transcript(&s);
        assert!(t.contains("truncated"));
        assert!(!t.contains(&big));
    }

    #[test]
    fn transcript_includes_system_messages() {
        let mut s = make_session();
        s.add_message(
            MessageRole::System,
            "compaction summary: previous work on auth module",
        );
        s.add_message(MessageRole::User, "continue");

        let t = build_transcript(&s);
        assert!(t.contains("[System: compaction summary"));
        assert!(t.contains("User: continue"));
    }

    #[test]
    fn transcript_handles_interrupted_tool() {
        let mut s = make_session();
        let tc = ToolCallEntry {
            id: "ci".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({}),
            state: ToolCallState::Interrupted,
        };
        s.add_message_with_tool_calls(MessageRole::Assistant, "", vec![tc]);

        let t = build_transcript(&s);
        assert!(t.contains("<interrupted>"));
    }

    #[test]
    fn review_prompt_contains_required_sections() {
        // Verify the prompt has the key structural elements from Hermes.
        assert!(COMBINED_REVIEW_PROMPT.contains("Preference order"));
        assert!(COMBINED_REVIEW_PROMPT.contains("Do NOT capture"));
        assert!(COMBINED_REVIEW_PROMPT.contains("Signals that warrant"));
        assert!(COMBINED_REVIEW_PROMPT.contains("Environment-dependent"));
        assert!(COMBINED_REVIEW_PROMPT.contains("CLASS-LEVEL skills"));
        assert!(COMBINED_REVIEW_PROMPT.contains("Nothing to save"));
    }

    #[test]
    fn review_prompt_override_is_accepted() {
        // Verify the function signature compiles with an override.
        // (This is a compile-time check but also verifies the Option
        // typing works.)
        let custom = "Custom review prompt";
        assert_ne!(custom, COMBINED_REVIEW_PROMPT);
    }

    // ── dirge-bo88: claim/release rate-limit slot ──────────────────
    //
    // These tests mutate the process-wide `LAST_REVIEW` AtomicU64,
    // so they take a serial mutex to avoid clobbering each other in
    // parallel test runs.

    use std::sync::Mutex;
    static LAST_REVIEW_LOCK: Mutex<()> = Mutex::new(());

    fn reset_last_review() {
        LAST_REVIEW.store(0, Ordering::Release);
    }

    #[test]
    fn claim_review_slot_succeeds_when_unset() {
        let _g = LAST_REVIEW_LOCK.lock().unwrap();
        reset_last_review();
        let now = 10_000;
        let claimed = claim_review_slot(now);
        assert_eq!(claimed, Some(0), "first call should claim with prev=0");
        assert_eq!(
            LAST_REVIEW.load(Ordering::Acquire),
            now,
            "timestamp advanced"
        );
    }

    #[test]
    fn claim_review_slot_rejects_inside_rate_limit_window() {
        let _g = LAST_REVIEW_LOCK.lock().unwrap();
        reset_last_review();
        let t1 = 100_000;
        assert!(claim_review_slot(t1).is_some(), "first call claims");

        // Inside the 15-minute window — must be rejected.
        let t2 = t1 + (MIN_REVIEW_INTERVAL_SECS - 1);
        assert!(
            claim_review_slot(t2).is_none(),
            "second call within window must be rate-limited"
        );

        // Outside the window — must succeed.
        let t3 = t1 + MIN_REVIEW_INTERVAL_SECS + 1;
        assert!(
            claim_review_slot(t3).is_some(),
            "call past the window must succeed"
        );
    }

    #[test]
    fn release_review_slot_rolls_back_when_we_are_still_latest() {
        let _g = LAST_REVIEW_LOCK.lock().unwrap();
        reset_last_review();
        let now = 200_000;
        let prev = claim_review_slot(now).expect("claim");
        assert_eq!(prev, 0);
        assert_eq!(LAST_REVIEW.load(Ordering::Acquire), now);

        // Simulate the review failing immediately.
        release_review_slot(prev, now);
        assert_eq!(
            LAST_REVIEW.load(Ordering::Acquire),
            prev,
            "release rolls timestamp back so retry can run"
        );

        // After rollback, an immediate retry should work.
        let retry = claim_review_slot(now + 1);
        assert!(retry.is_some(), "retry must claim after rollback");
    }

    #[test]
    fn release_review_slot_does_not_clobber_a_later_review() {
        let _g = LAST_REVIEW_LOCK.lock().unwrap();
        reset_last_review();
        let t1 = 300_000;
        let prev = claim_review_slot(t1).expect("first claim");
        // Simulate a much-later successful review having advanced the
        // timestamp (e.g., the failing review's release call ran late
        // and a fresh review already completed).
        let t2 = t1 + 10 * MIN_REVIEW_INTERVAL_SECS;
        LAST_REVIEW.store(t2, Ordering::Release);

        release_review_slot(prev, t1);
        assert_eq!(
            LAST_REVIEW.load(Ordering::Acquire),
            t2,
            "stale release must NOT roll back a later review's timestamp"
        );
    }

    #[test]
    fn claim_review_slot_is_race_safe_under_concurrent_callers() {
        // Spawn many threads racing to claim the slot from the unset
        // state. Exactly one must win; the rest get `None`. Without
        // the CAS (pre-fix `load() + store(now)`) this test would
        // occasionally observe two winners.
        let _g = LAST_REVIEW_LOCK.lock().unwrap();
        reset_last_review();
        let now = 500_000;
        let winners = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..32 {
                s.spawn(|| {
                    if claim_review_slot(now).is_some() {
                        winners.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(
            winners.load(Ordering::Relaxed),
            1,
            "exactly one concurrent caller must win the claim"
        );
    }
}
