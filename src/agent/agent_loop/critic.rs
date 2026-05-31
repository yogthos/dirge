//! Bounded in-loop LLM critic (F6 tier 3).
//!
//! When a `critic_provider` is configured, the verifier gate can escalate
//! from cheap signals to a single LLM judgement at the finalization
//! boundary: given the user's request and the work done this run, is the
//! task actually complete and correct? If the critic says no, its
//! concrete issues are injected as a follow-up and the loop continues;
//! otherwise the run finalizes. Bounded to one call per run (the caller
//! enforces this) and OFF unless a critic provider is configured — so it
//! never adds latency or cost to a default session.
//!
//! The actual LLM call is a [`CriticFn`] callback (mirrors
//! `compression::SummarizeFn`) built in the provider layer; this module
//! owns the prompt, the verdict parsing, and the loop-message wiring so
//! they're unit-testable without a model.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::message::{LoopMessage, UserMessage};

/// One-shot critic call: takes a fully-built prompt, returns the model's
/// raw verdict text. Mirrors `compression::SummarizeFn` so the provider
/// layer can build it from any configured model.
pub type CriticFn = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>> + Send + Sync,
>;

/// System preamble for the critic: establishes its role and the strict,
/// skeptical stance. Passed as the LLM system prompt by `build_critic_fn`
/// (replacing the stray "conversation summarizer" preamble) so the model
/// knows what it is BEFORE it sees the transcript. The exact response
/// FORMAT lives in [`build_prompt`] instead — right next to the material
/// being judged.
pub const CRITIC_PREAMBLE: &str = "\
You are a strict code-review critic for an autonomous coding agent. You are given a user's request \
and a transcript of what the assistant just did to satisfy it. Judge ONLY whether the task is \
actually complete and correct — not style. Be skeptical: unverified claims, partial work, ignored \
edge cases, and \"done\" without evidence are NOT complete. If you are genuinely unsure, pass — do \
not block on speculation.";

/// Response-format instruction. Kept in the user prompt (not the system
/// preamble) so the strict verdict shape sits directly beside the
/// transcript the critic is judging.
const CRITIC_FORMAT: &str = "\
Respond in EXACTLY this format and nothing else:\n\
On the first line, either `VERDICT: COMPLETE` or `VERDICT: INCOMPLETE`.\n\
If INCOMPLETE, follow with a short bullet list of the specific, concrete issues to fix.";

/// Build the critic prompt from a transcript of the run. The role lives
/// in [`CRITIC_PREAMBLE`] (the system slot); this carries the format +
/// the transcript.
pub fn build_prompt(transcript: &str) -> String {
    format!("{CRITIC_FORMAT}\n\n--- transcript ---\n{transcript}\n--- end transcript ---")
}

/// Parse the critic's raw response into a verdict. `Some(issues)` means
/// the critic judged the work incomplete (with the issue text); `None`
/// means complete — or the response was empty/ambiguous, in which case we
/// fail OPEN (don't block finalization on a confused critic).
pub fn parse_verdict(response: &str) -> Option<String> {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Look at the first non-empty line for the verdict token.
    let first = trimmed.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let upper = first.to_ascii_uppercase();
    if upper.contains("INCOMPLETE") {
        // Everything after the first line is the issue list; fall back to
        // the whole response if the model put issues on the first line.
        let rest = trimmed
            .splitn(2, '\n')
            .nth(1)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(trimmed);
        Some(rest.to_string())
    } else {
        // "COMPLETE", or anything that isn't a clear INCOMPLETE → pass.
        None
    }
}

/// Run the critic over a run transcript. Returns a one-element vec with a
/// `[critic]`-tagged follow-up message when the critic judged the work
/// incomplete; empty otherwise (complete, or the call errored — fail
/// open). Never panics on a critic error.
pub async fn run_critic(critic: &CriticFn, transcript: &str) -> Vec<LoopMessage> {
    let prompt = build_prompt(transcript);
    let response = match critic(prompt).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "dirge::critic", error = %e, "critic call failed; finalizing without it");
            return Vec::new();
        }
    };
    match parse_verdict(&response) {
        Some(issues) => vec![LoopMessage::User(UserMessage {
            content: format!(
                "[critic] A review of your work found it isn't done yet. Address these before \
                 reporting complete, or explain why they don't apply:\n{issues}"
            ),
        })],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_complete_returns_none() {
        assert!(parse_verdict("VERDICT: COMPLETE").is_none());
        assert!(parse_verdict("verdict: complete\n(looks good)").is_none());
    }

    #[test]
    fn parse_incomplete_returns_issues() {
        let v = parse_verdict("VERDICT: INCOMPLETE\n- missing test\n- error path unhandled");
        let issues = v.expect("should be incomplete");
        assert!(issues.contains("missing test"));
        assert!(issues.contains("error path"));
    }

    #[test]
    fn parse_empty_or_ambiguous_fails_open() {
        assert!(parse_verdict("").is_none());
        assert!(parse_verdict("   \n  ").is_none());
        assert!(parse_verdict("I think it's probably fine?").is_none());
    }

    #[test]
    fn prompt_embeds_transcript_and_format() {
        let p = build_prompt("user asked X; assistant edited foo.rs");
        assert!(p.contains("VERDICT: COMPLETE"));
        assert!(p.contains("VERDICT: INCOMPLETE"));
        assert!(p.contains("edited foo.rs"));
    }

    /// The system preamble states the critic's ROLE (not the summarizer's),
    /// and the response FORMAT is kept out of it (it lives in the prompt).
    #[test]
    fn preamble_establishes_critic_role_without_format() {
        let lower = CRITIC_PREAMBLE.to_ascii_lowercase();
        assert!(lower.contains("critic"), "preamble must name the role");
        assert!(
            !lower.contains("summarizer"),
            "preamble must not be the summarizer's"
        );
        // Format lives in the prompt, not the system preamble.
        assert!(!CRITIC_PREAMBLE.contains("VERDICT:"));
        assert!(build_prompt("t").contains("VERDICT:"));
    }

    #[tokio::test]
    async fn run_critic_injects_followup_when_incomplete() {
        let critic: CriticFn = Arc::new(|_prompt| {
            Box::pin(async { Ok("VERDICT: INCOMPLETE\n- the test was never run".to_string()) })
        });
        let msgs = run_critic(&critic, "did stuff").await;
        assert_eq!(msgs.len(), 1);
        let content = match &msgs[0] {
            LoopMessage::User(u) => &u.content,
            _ => panic!("expected user message"),
        };
        assert!(content.contains("[critic]"));
        assert!(content.contains("test was never run"));
    }

    #[tokio::test]
    async fn run_critic_silent_when_complete() {
        let critic: CriticFn =
            Arc::new(|_p| Box::pin(async { Ok("VERDICT: COMPLETE".to_string()) }));
        assert!(run_critic(&critic, "did stuff").await.is_empty());
    }

    #[tokio::test]
    async fn run_critic_fails_open_on_error() {
        let critic: CriticFn = Arc::new(|_p| Box::pin(async { anyhow::bail!("provider down") }));
        assert!(
            run_critic(&critic, "did stuff").await.is_empty(),
            "a critic error must not block finalization"
        );
    }
}
