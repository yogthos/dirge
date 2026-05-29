//! Core data types for the permission authorization engine (PDP).
//!
//! These are the normalized request/decision types every tool and
//! policy speaks. The design is a two-stage Policy Decision Point:
//! ordered *deciders* (may loosen) produce a base [`Effect`] per
//! resource, then monotonic *modifiers* (may only tighten, enforced
//! by [`Refined`]) refine it. Effects combine via [`Effect::meet`]
//! (most-restrictive-wins) — the same algebra is reused for the
//! per-resource modifier fold and the per-request multi-resource fold.
//!
//! See `engine/policy.rs` for the [`Decider`]/[`Modifier`] traits and
//! `engine/mod.rs` for the evaluation algorithm.

use std::path::PathBuf;

/// Coarse operation intent. This — not a loose `tool: &str` — is what
/// policies match on via [`crate::permission::engine::policy::Decider::applies_to`],
/// so "edit rules apply to Edit operations" is a directly evaluable
/// fact. Tool names map onto these during request normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    /// Read-only observation of a file (read, grep, list_dir, lsp,
    /// the semantic readers).
    Read,
    /// File mutation — the `write` and `edit`/`apply_patch` tools plus
    /// bash redirect targets and mutation paths all normalize here, so
    /// one rule governs them all (the old F2 write↔edit↔apply_patch
    /// aliasing dissolves into a single operation).
    Edit,
    /// Shell command execution (one segment of a bash invocation).
    Execute,
    /// Outbound network (webfetch / websearch).
    Network,
    /// MCP server tool invocation.
    Mcp,
    /// Memory store read/write.
    Memory,
    /// Skill load/list (read) or create/edit/patch/delete (write).
    Skill,
    /// Recursive sub-agent execution (the `task` tool). High-risk —
    /// not builtin-allowed and not coerced by Accept mode.
    Agent,
    /// Internal tools with no external effect (write_todo_list,
    /// task_status, question) — builtin-allowed.
    Meta,
    /// Uncategorized / plugin tools. Not builtin-allowed; falls to the
    /// configured rules or the default (Ask), and IS Accept-coercible.
    Other,
}

impl Operation {
    /// Operations that mutate state or reach outside the process. The
    /// loop guard and restrictive-mode tightening only concern these;
    /// pure reads are never gated by repetition.
    pub fn is_side_effecting(self) -> bool {
        matches!(
            self,
            Operation::Edit
                | Operation::Execute
                | Operation::Network
                | Operation::Mcp
                | Operation::Agent
        )
    }
}

/// A single thing being acted on. Path resolution is a *property of
/// the value* (computed once during normalization), which is what
/// lets a single evaluation path replace the old `check` (raw) vs
/// `check_path` (path) split and its `Scope::{Raw,Path,PathResolve}`
/// enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// A filesystem path. `resolved` is the canonical absolute form
    /// to hand to `File::open` (TOCTOU pin); `in_cwd`/`dev_null` are
    /// precomputed classifications the builtin-allow policy reads.
    Path {
        raw: String,
        resolved: PathBuf,
        in_cwd: bool,
        dev_null: bool,
    },
    /// One shell command segment. `head` is the leading executable
    /// token (for command-pattern matching / display).
    Command { raw: String, head: String },
    /// An MCP `server:tool` invocation.
    Mcp {
        server: String,
        name: String,
        raw: String,
    },
    /// An outbound URL (webfetch) or search query (websearch).
    Url(String),
    /// Free text that isn't a path/command/url — a grep pattern, a
    /// task prompt, an mcp identifier shown to the user.
    Bareword(String),
}

impl Resource {
    /// The string a pattern matches against, and the value shown in
    /// the permission prompt for this resource.
    pub fn match_key(&self) -> &str {
        match self {
            Resource::Path { resolved, raw, .. } => resolved.to_str().unwrap_or(raw),
            Resource::Command { raw, .. } => raw,
            Resource::Mcp { raw, .. } => raw,
            Resource::Url(u) => u,
            Resource::Bareword(b) => b,
        }
    }

    /// Every string form a configured pattern should be tested
    /// against. Paths expose BOTH the canonical resolved form and the
    /// raw input, so a user rule written against either (literal
    /// `/etc/**`, a symlinked root, or a relative path) matches —
    /// mirroring the old `check_path` `matches(abs) || matches(raw)`.
    pub fn match_candidates(&self) -> Vec<&str> {
        match self {
            Resource::Path { raw, resolved, .. } => {
                let r = resolved.to_str().unwrap_or(raw);
                if r == raw { vec![r] } else { vec![r, raw] }
            }
            _ => vec![self.match_key()],
        }
    }
}

/// A single (operation, resource) the agent wants to perform. A bash
/// command decomposes into many claims with DIFFERENT operations
/// (command segments → Execute, redirect/mutation targets → Edit),
/// which is why the operation lives per-claim, not per-request.
#[derive(Debug, Clone)]
pub struct Claim {
    pub op: Operation,
    pub resource: Resource,
}

impl Claim {
    pub fn new(op: Operation, resource: Resource) -> Self {
        Claim { op, resource }
    }
}

/// One logical operation the agent wants to perform. A single bash
/// invocation produces ONE request holding many claims (command
/// segments + redirect targets + mutation paths), so it is authorized
/// atomically and prompts at most once — never the old N separate
/// `enforce()` calls.
#[derive(Debug, Clone)]
pub struct AccessRequest {
    /// Concrete tool name, for display and the decision trace.
    pub tool: String,
    pub claims: Vec<Claim>,
    pub mode: crate::permission::SecurityMode,
    /// Raw text shown in the Ask prompt.
    pub display_input: String,
}

impl AccessRequest {
    /// Convenience constructor for a single-claim request (the common
    /// case: one tool, one resource).
    pub fn single(
        tool: impl Into<String>,
        op: Operation,
        resource: Resource,
        mode: crate::permission::SecurityMode,
        display_input: impl Into<String>,
    ) -> Self {
        let display_input = display_input.into();
        AccessRequest {
            tool: tool.into(),
            claims: vec![Claim::new(op, resource)],
            mode,
            display_input,
        }
    }
}

/// The authorization lattice. Ordering IS the combination algebra:
/// `Allow < Ask < Deny`, so [`Effect::meet`] = `max` = most
/// restrictive. Deriving `Ord` in this variant order is load-bearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

impl Effect {
    /// Most-restrictive-wins combination. Used both to fold Stage-B
    /// modifier proposals into a resource's effect and to fold the
    /// per-resource effects of a multi-resource request into one.
    /// Commutative, associative, idempotent (it's `max` on a total
    /// order) — which is what makes modifier order irrelevant.
    pub fn meet(self, other: Effect) -> Effect {
        self.max(other)
    }
}

/// A decider's claim on a resource (Stage A). The first decider (in
/// registered precedence order) that returns `Some` sets the base
/// effect for that resource.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub effect: Effect,
    pub why: String,
}

impl Verdict {
    pub fn new(effect: Effect, why: impl Into<String>) -> Self {
        Verdict {
            effect,
            why: why.into(),
        }
    }
}

/// A modifier's output (Stage B). Constructible ONLY via
/// [`Refined::tighten`], whose result is `meet(current, proposed)` —
/// so a modifier can never *loosen* a decision. This makes the
/// dangerous direction unrepresentable rather than merely reviewed.
#[derive(Debug, Clone)]
pub struct Refined {
    pub(crate) effect: Effect,
    pub(crate) by: Option<&'static str>,
    pub(crate) why: String,
}

impl Refined {
    /// No change: the modifier abstained.
    pub fn noop(current: Effect) -> Self {
        Refined {
            effect: current,
            by: None,
            why: String::new(),
        }
    }

    /// Propose tightening to (at least) `proposed`. The stored effect
    /// is `meet(current, proposed)`, so passing a looser effect than
    /// `current` is a no-op — loosening is impossible by construction.
    pub fn tighten(
        current: Effect,
        proposed: Effect,
        by: &'static str,
        why: impl Into<String>,
    ) -> Self {
        let effect = current.meet(proposed);
        Refined {
            effect,
            by: if effect != current { Some(by) } else { None },
            why: if effect != current {
                why.into()
            } else {
                String::new()
            },
        }
    }

    pub fn effect(&self) -> Effect {
        self.effect
    }
}

/// One line of the decision audit trail: which policy looked at which
/// resource, whether it applied, what it voted and why. The full
/// `Vec<TraceEntry>` on a [`Decision`] answers "why did this happen?".
#[derive(Debug, Clone)]
pub struct TraceEntry {
    pub policy: &'static str,
    /// Index into `AccessRequest::resources`.
    pub resource: usize,
    /// The effect this policy contributed, if it applied and voted.
    pub effect: Option<Effect>,
    pub why: String,
    /// Whether the policy was applicable to this (op, resource).
    pub applied: bool,
}

/// The result of [`crate::permission::engine::Engine::authorize`].
#[derive(Debug, Clone)]
pub struct Decision {
    pub effect: Effect,
    /// The single trace entry that set the final effect (the
    /// most-restrictive contributing vote). `None` only if there were
    /// no resources.
    pub deciding: Option<TraceEntry>,
    /// Every applicable policy's vote across every resource, in
    /// evaluation order, plus `applied: false` stubs for policies
    /// that opted out (with the reason).
    pub trace: Vec<TraceEntry>,
    /// Canonical absolute paths for the request's `Path` resources,
    /// in resource order — handed to the follow-up file op so the
    /// authorized path is the opened path (TOCTOU pin; replaces the
    /// old `PathResolve` scope return value).
    pub resolved_paths: Vec<PathBuf>,
}

impl Decision {
    /// Human-readable reason for the final effect, sourced from the
    /// deciding policy. Used for Ask prompts and Deny messages.
    pub fn reason(&self) -> String {
        match &self.deciding {
            Some(e) if !e.why.is_empty() => format!("{} ({})", e.why, e.policy),
            Some(e) => format!("decided by {}", e.policy),
            None => "no resources to authorize".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_lattice_order() {
        assert!(Effect::Allow < Effect::Ask);
        assert!(Effect::Ask < Effect::Deny);
        assert!(Effect::Allow < Effect::Deny);
    }

    #[test]
    fn meet_is_most_restrictive() {
        assert_eq!(Effect::Allow.meet(Effect::Ask), Effect::Ask);
        assert_eq!(Effect::Ask.meet(Effect::Allow), Effect::Ask);
        assert_eq!(Effect::Allow.meet(Effect::Deny), Effect::Deny);
        assert_eq!(Effect::Ask.meet(Effect::Deny), Effect::Deny);
        assert_eq!(Effect::Allow.meet(Effect::Allow), Effect::Allow);
        assert_eq!(Effect::Deny.meet(Effect::Deny), Effect::Deny);
    }

    #[test]
    fn meet_lattice_laws() {
        let all = [Effect::Allow, Effect::Ask, Effect::Deny];
        for a in all {
            // idempotent
            assert_eq!(a.meet(a), a);
            for b in all {
                // commutative
                assert_eq!(a.meet(b), b.meet(a));
                for c in all {
                    // associative
                    assert_eq!(a.meet(b).meet(c), a.meet(b.meet(c)));
                    // monotone: meeting never decreases (never loosens)
                    assert!(a.meet(b) >= a);
                }
            }
        }
    }

    #[test]
    fn refined_cannot_loosen() {
        // Proposing a looser effect than current is a no-op.
        let r = Refined::tighten(Effect::Deny, Effect::Allow, "x", "tried to loosen");
        assert_eq!(r.effect(), Effect::Deny);
        assert!(r.by.is_none(), "no-op tighten records no author");

        // Proposing a stricter effect tightens.
        let r = Refined::tighten(Effect::Allow, Effect::Ask, "loopguard", "retry loop");
        assert_eq!(r.effect(), Effect::Ask);
        assert_eq!(r.by, Some("loopguard"));

        // noop preserves.
        assert_eq!(Refined::noop(Effect::Ask).effect(), Effect::Ask);
    }

    #[test]
    fn side_effecting_classification() {
        assert!(Operation::Edit.is_side_effecting());
        assert!(Operation::Execute.is_side_effecting());
        assert!(Operation::Network.is_side_effecting());
        assert!(Operation::Mcp.is_side_effecting());
        assert!(!Operation::Read.is_side_effecting());
        assert!(!Operation::Meta.is_side_effecting());
        // Memory/Skill are coarse; write-vs-read is decided per-action
        // by the policies, not by this coarse flag.
        assert!(!Operation::Memory.is_side_effecting());
    }
}
