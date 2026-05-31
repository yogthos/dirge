//! dirge-0g6i — optional LLM auto-approval for permission prompts.
//!
//! When the user configures an `approval_provider`, a permission `Ask`
//! is routed to that model instead of the human: it sees the command,
//! the working directory, and a per-resource danger summary, and replies
//! `ALLOW` or `DENY: <reason>`. The decision is applied automatically.
//!
//! Fail-safe by construction: an unparseable verdict is treated as
//! DENY, and an LLM/transport error falls back to the human prompt (see
//! `agent::tools::enforce`). The evaluator is stored on the per-session
//! [`PermissionChecker`](crate::permission::checker::PermissionChecker)
//! (not a global), so it's session-scoped and unit-testable in
//! isolation. The deterministic surface (prompt building + verdict
//! parsing) is unit-tested; the LLM call itself is not.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::permission::engine::types::{Claim, Resource};

/// What the evaluator is asked to judge: one tool invocation, with the
/// resources it would touch already classified (in/out of project).
pub struct ApprovalRequest {
    pub tool: String,
    /// The command (bash) or path/identifier shown to a human prompt.
    pub command: String,
    pub working_dir: String,
    /// Human-readable, pre-classified per-resource lines (see
    /// [`summarize_claims`]). Empty for single-scope (non-bash) checks,
    /// where `command` already names the resource.
    pub resources: Vec<String>,
}

/// The evaluator's verdict.
pub enum ApprovalDecision {
    Allow,
    Deny(String),
}

/// Async judge: request → verdict. Boxed so the concrete provider/model
/// type doesn't leak into the permission layer.
pub type ApprovalFn = Arc<
    dyn Fn(
            ApprovalRequest,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<ApprovalDecision>> + Send>>
        + Send
        + Sync,
>;

/// System prompt for the evaluator. Spells out what counts as dangerous
/// so the judgement is consistent and conservative. Kept model-agnostic.
pub const EVALUATOR_PREAMBLE: &str = "\
You are the security gate for an autonomous coding agent. For each operation \
you receive, decide whether it is SAFE AND REASONABLE for an agent working on \
a software project to run WITHOUT a human confirming first.

Be conservative: when in doubt, DENY. It is fine to deny — a human will be \
asked instead. Only ALLOW operations that are clearly safe and scoped to the \
agent's work.

DENY operations such as:
- Deleting files outside the project, or recursive/forced deletes (rm -rf, \
  del /s) anywhere outside the project directory or a temp directory.
- Creating, modifying, or moving files OUTSIDE the project directory or a \
  system temp directory (/tmp, $TMPDIR).
- Committing, pushing, or otherwise mutating a git repository OUTSIDE the \
  project directory; or `git push` to a remote.
- Fetching and executing remote code from untrusted sources (curl|sh, \
  installing packages, running remote npx packages) when the intent is unclear.
- Privilege escalation (sudo, doas), disk/device operations (dd, mkfs, \
  fdisk), changing system configuration, or editing another user's files.
- Reading or transmitting credentials/secrets (~/.ssh, ~/.aws, .env with \
  tokens, keychains) to the network.

ALLOW operations such as:
- Reading, listing, searching files within or near the project.
- Building, testing, linting, or running the project's OWN code inside the \
  project directory.
- Creating or editing files inside the project directory or a temp directory.

Reply with EXACTLY ONE line, nothing else:
  ALLOW
or
  DENY: <short reason>";

/// Build the per-request user message handed to the evaluator.
pub fn build_evaluator_prompt(req: &ApprovalRequest) -> String {
    let mut s = String::new();
    s.push_str("Project (working) directory: ");
    s.push_str(&req.working_dir);
    s.push_str("\n\nOperation:\n  tool: ");
    s.push_str(&req.tool);
    s.push_str("\n  command/input: ");
    s.push_str(&req.command);
    if !req.resources.is_empty() {
        s.push_str("\n  resources it would touch:");
        for r in &req.resources {
            s.push_str("\n    - ");
            s.push_str(r);
        }
    }
    s.push_str("\n\nReply with ALLOW or DENY: <reason>.");
    s
}

/// Parse the evaluator's reply. Fail-safe: anything that isn't a clear
/// ALLOW is treated as DENY (an ambiguous judge must not auto-approve).
pub fn parse_decision(response: &str) -> ApprovalDecision {
    for line in response.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let upper = t.to_ascii_uppercase();
        if upper.starts_with("ALLOW") {
            return ApprovalDecision::Allow;
        }
        if upper.starts_with("DENY") {
            // Strip the "DENY" head, then any leading separator(s)
            // (`:`/`-`/whitespace) before the reason — without eating a
            // legitimate leading `-` inside the reason like "rm -rf …".
            let reason = t[4..].trim_start_matches([':', '-', ' ', '\t']).trim();
            let reason = if reason.is_empty() {
                "no reason given"
            } else {
                reason
            };
            return ApprovalDecision::Deny(reason.to_string());
        }
    }
    let preview: String = response.trim().chars().take(120).collect();
    ApprovalDecision::Deny(format!("unclear evaluator response: {preview:?}"))
}

/// Render each claim as a pre-classified, human-readable line for the
/// evaluator — surfacing the danger signals (operation + whether a path
/// is inside the project) so the model doesn't have to re-derive them.
pub fn summarize_claims(claims: &[Claim]) -> Vec<String> {
    claims
        .iter()
        .map(|c| {
            let op = format!("{:?}", c.op).to_uppercase();
            match &c.resource {
                Resource::Command { raw, .. } => format!("{op} a command: {raw}"),
                Resource::Path {
                    resolved,
                    in_cwd,
                    dev_null,
                    ..
                } => {
                    let loc = if *dev_null {
                        "/dev/null (discarded)".to_string()
                    } else if *in_cwd {
                        "INSIDE the project".to_string()
                    } else {
                        "OUTSIDE the project".to_string()
                    };
                    format!("{op} a file {} — {}", resolved.display(), loc)
                }
                Resource::Url(u) => format!("{op} a network URL: {u}"),
                Resource::Mcp { server, name, .. } => {
                    format!("{op} an MCP tool {server}:{name}")
                }
                Resource::Bareword(b) => format!("{op}: {b}"),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_allow_variants() {
        assert!(matches!(parse_decision("ALLOW"), ApprovalDecision::Allow));
        assert!(matches!(parse_decision("allow"), ApprovalDecision::Allow));
        assert!(matches!(
            parse_decision("  ALLOW  "),
            ApprovalDecision::Allow
        ));
        // Leading chatter then a clear verdict line.
        assert!(matches!(
            parse_decision("Thinking...\nALLOW"),
            ApprovalDecision::Allow
        ));
    }

    #[test]
    fn parse_deny_with_reason() {
        match parse_decision("DENY: writes outside the project") {
            ApprovalDecision::Deny(r) => assert_eq!(r, "writes outside the project"),
            _ => panic!("expected deny"),
        }
        match parse_decision("deny - rm -rf on system path") {
            ApprovalDecision::Deny(r) => assert_eq!(r, "rm -rf on system path"),
            _ => panic!("expected deny"),
        }
        match parse_decision("DENY") {
            ApprovalDecision::Deny(r) => assert_eq!(r, "no reason given"),
            _ => panic!("expected deny"),
        }
    }

    /// Fail-safe: an unparseable / non-verdict response must DENY, never
    /// silently allow.
    #[test]
    fn unparseable_response_fails_safe_to_deny() {
        assert!(matches!(
            parse_decision("I'm not sure about this one"),
            ApprovalDecision::Deny(_)
        ));
        assert!(matches!(parse_decision(""), ApprovalDecision::Deny(_)));
    }

    #[test]
    fn evaluator_prompt_includes_command_and_resources() {
        let req = ApprovalRequest {
            tool: "bash".into(),
            command: "rm -rf /tmp/x && npx foo".into(),
            working_dir: "/work/proj".into(),
            resources: vec!["EXECUTE a command: npx foo".into()],
        };
        let p = build_evaluator_prompt(&req);
        assert!(p.contains("/work/proj"));
        assert!(p.contains("rm -rf /tmp/x && npx foo"));
        assert!(p.contains("EXECUTE a command: npx foo"));
        assert!(p.contains("ALLOW") && p.contains("DENY"));
    }
}
