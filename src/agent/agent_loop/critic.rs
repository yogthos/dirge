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

/// Tag prefixed onto the critic's injected follow-up message. The agent
/// loop re-enters it as a user-role message (so the model acts on it); the
/// UI keys on this tag to render it under a distinct `<critic>` handle and
/// color instead of the user's. Shared so producer and renderer agree.
pub const CRITIC_TAG: &str = "[critic]";

/// System preamble for the critic: establishes its role and a calibrated —
/// not trigger-happy — stance. Passed as the LLM system prompt by
/// `build_critic_fn` so the model knows what it is BEFORE it sees the
/// transcript. The response FORMAT lives in [`build_prompt`] instead —
/// right next to the material being judged.
///
/// dirge-bedj: the stance was over-aggressive ("be skeptical", everything
/// "NOT complete") and constraint-blind, so it demanded actions the agent
/// was explicitly told not to take (e.g. pushing). It now (a) respects the
/// agent's own instructions and (b) blocks only on concrete, in-scope gaps.
pub const CRITIC_PREAMBLE: &str = "\
You are a code-review critic for an autonomous coding agent. You are given the instructions and \
constraints the assistant operates under, plus a transcript of what it just did to satisfy the \
user's request. Judge ONLY whether the task is actually complete and correct within those \
constraints — not style.\n\
\n\
Hard rules:\n\
- RESPECT the assistant's instructions. NEVER flag the absence of an action the instructions \
forbid or defer (e.g. if it was told not to push/commit/deploy, do NOT ask it to). Treat anything \
the instructions place out of scope as correctly omitted.\n\
- Block only on CONCRETE, in-scope incompleteness with evidence (e.g. the user asked for X and X \
is missing; a change was made but never built/tested when verification was expected).\n\
- Do NOT invent new requirements, scope, or \"nice to haves\". If you are unsure, PASS — a false \
block wastes a whole turn.";

/// Response-format instruction. Kept in the user prompt (not the system
/// preamble) so the verdict shape sits directly beside the transcript.
const CRITIC_FORMAT: &str = "\
Respond in EXACTLY this format and nothing else:\n\
On the first line, either `VERDICT: COMPLETE` or `VERDICT: INCOMPLETE`.\n\
If INCOMPLETE, follow with a short bullet list of the specific, concrete, in-scope issues to fix.";

/// Cap on the instructions/constraints block fed to the critic, so a large
/// system prompt (tool docs + project context) doesn't balloon the critic
/// call. Generous — the constraints that matter (AGENTS.md, prompt-mode
/// rules) sit early; a truncation note tells the critic more was elided.
const MAX_RULES_CHARS: usize = 16_000;

/// Build the critic prompt. `rules` is the assistant's own system prompt /
/// instructions (so the critic judges against the SAME constraints the
/// agent had — dirge-bedj); `transcript` is what the agent did. The role
/// lives in [`CRITIC_PREAMBLE`]; this carries the format + both bodies.
pub fn build_prompt(rules: &str, transcript: &str) -> String {
    let rules = rules.trim();
    let rules_block = if rules.is_empty() {
        "(no special constraints provided)".to_string()
    } else if rules.len() > MAX_RULES_CHARS {
        let head: String = rules.chars().take(MAX_RULES_CHARS).collect();
        format!("{head}\n…(instructions truncated)")
    } else {
        rules.to_string()
    };
    format!(
        "{CRITIC_FORMAT}\n\n\
         --- assistant instructions & constraints (judge within these; never demand a \
         forbidden/out-of-scope action) ---\n{rules_block}\n--- end instructions ---\n\n\
         --- transcript ---\n{transcript}\n--- end transcript ---"
    )
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

/// Run the critic over a run transcript. `rules` is the assistant's own
/// system prompt / instructions, passed so the critic judges within the
/// SAME constraints the agent had (dirge-bedj). Returns a one-element vec
/// with a [`CRITIC_TAG`]-prefixed follow-up message when the critic judged
/// the work incomplete; empty otherwise (complete, or the call errored —
/// fail open). Never panics on a critic error.
pub async fn run_critic(critic: &CriticFn, rules: &str, transcript: &str) -> Vec<LoopMessage> {
    let prompt = build_prompt(rules, transcript);
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
                "{CRITIC_TAG} A review of your work found it may not be done yet. Address these \
                 before reporting complete, or explain why they don't apply (e.g. they're out of \
                 scope or something you were told not to do):\n{issues}"
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
    fn prompt_embeds_transcript_format_and_rules() {
        let p = build_prompt(
            "RULE: never push to remote.",
            "user asked X; assistant edited foo.rs",
        );
        assert!(p.contains("VERDICT: COMPLETE"));
        assert!(p.contains("VERDICT: INCOMPLETE"));
        assert!(p.contains("edited foo.rs"));
        // dirge-bedj: the agent's own constraints are included so the
        // critic judges within them.
        assert!(p.contains("never push to remote"), "rules must be embedded");
        assert!(
            p.to_lowercase().contains("forbidden") || p.to_lowercase().contains("out-of-scope"),
            "prompt must instruct the critic to respect constraints",
        );
    }

    #[test]
    fn empty_rules_render_a_placeholder_not_blank() {
        let p = build_prompt("", "did stuff");
        assert!(p.contains("no special constraints"));
    }

    #[test]
    fn build_prompt_caps_large_rules() {
        let huge = "x".repeat(MAX_RULES_CHARS + 5_000);
        let p = build_prompt(&huge, "t");
        assert!(p.contains("instructions truncated"));
        // The rules block is bounded (cap + the transcript/format scaffold,
        // well under the untruncated size).
        assert!(p.len() < MAX_RULES_CHARS + 4_000);
    }

    /// The system preamble states the critic's ROLE, keeps FORMAT out, and
    /// (dirge-bedj) instructs it to respect the agent's constraints.
    #[test]
    fn preamble_is_calibrated_and_constraint_aware() {
        let lower = CRITIC_PREAMBLE.to_ascii_lowercase();
        assert!(lower.contains("critic"), "preamble must name the role");
        assert!(!lower.contains("summarizer"));
        // Format lives in the prompt, not the system preamble.
        assert!(!CRITIC_PREAMBLE.contains("VERDICT:"));
        assert!(build_prompt("", "t").contains("VERDICT:"));
        // Must not demand forbidden actions, and must respect instructions.
        assert!(
            lower.contains("respect"),
            "must say to respect instructions"
        );
        assert!(
            lower.contains("never flag the absence") || lower.contains("forbid"),
            "must forbid demanding disallowed actions",
        );
        assert!(lower.contains("unsure"), "must keep the fail-open guidance");
    }

    #[tokio::test]
    async fn run_critic_injects_followup_when_incomplete() {
        let critic: CriticFn = Arc::new(|_prompt| {
            Box::pin(async { Ok("VERDICT: INCOMPLETE\n- the test was never run".to_string()) })
        });
        let msgs = run_critic(&critic, "rules", "did stuff").await;
        assert_eq!(msgs.len(), 1);
        let content = match &msgs[0] {
            LoopMessage::User(u) => &u.content,
            _ => panic!("expected user message"),
        };
        assert!(content.starts_with(CRITIC_TAG));
        assert!(content.contains("test was never run"));
    }

    #[tokio::test]
    async fn run_critic_passes_rules_into_prompt() {
        use std::sync::Mutex;
        let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
        let critic: CriticFn = Arc::new(move |prompt: String| {
            *seen2.lock().unwrap() = prompt;
            Box::pin(async { Ok("VERDICT: COMPLETE".to_string()) })
        });
        let _ = run_critic(&critic, "RULE: do not deploy", "did stuff").await;
        assert!(
            seen.lock().unwrap().contains("do not deploy"),
            "the agent's constraints must reach the critic prompt",
        );
    }

    #[tokio::test]
    async fn run_critic_silent_when_complete() {
        let critic: CriticFn =
            Arc::new(|_p| Box::pin(async { Ok("VERDICT: COMPLETE".to_string()) }));
        assert!(run_critic(&critic, "rules", "did stuff").await.is_empty());
    }

    #[tokio::test]
    async fn run_critic_fails_open_on_error() {
        let critic: CriticFn = Arc::new(|_p| Box::pin(async { anyhow::bail!("provider down") }));
        assert!(
            run_critic(&critic, "rules", "did stuff").await.is_empty(),
            "a critic error must not block finalization"
        );
    }
}
