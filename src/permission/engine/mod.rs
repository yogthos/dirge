//! The permission authorization engine — a two-stage Policy Decision
//! Point (PDP).
//!
//! A tool normalizes its intent into one [`AccessRequest`] (possibly
//! many [`Resource`]s) and calls [`Engine::authorize`], which returns
//! a [`Decision`] carrying the effect AND a full trace of which policy
//! decided and why. This replaces the old `enforce` + `check` /
//! `check_path` split (which had drifted into seven divergences) and
//! the per-tool ad-hoc gates.
//!
//! Evaluation, per resource:
//! 1. **Stage A (deciders, first-claim-wins):** the registered
//!    deciders are consulted in precedence order; the first to claim
//!    the resource sets its base [`Effect`]. Deciders may loosen, so
//!    Accept-mode coercion belongs here in the audited base layer.
//! 2. **Stage B (modifiers, monotone):** each applicable modifier may
//!    only tighten via [`Refined`]; the fold is [`Effect::meet`], so
//!    modifier order is irrelevant. Restrictive demotion and the loop
//!    guard live here.
//!
//! Per request: the resource effects fold via [`Effect::meet`]
//! (most-restrictive-wins), so a bash command's segments + write
//! targets yield ONE atomic decision and at most one prompt.
//!
//! All mutable state lives in [`PolicyCtx`]; `authorize` only reads it,
//! and [`Engine::commit`] is the sole writer (called after the user's
//! decision resolves). That single-writer rule is why the loop guard
//! has no count-before-vs-after ordering bug.

// Phase 1 builds the engine in isolation with no call sites yet; the
// chokepoint swap (Phase 2) consumes every item below. Remove this
// allow once `authorize`/`Engine`/the policy types are wired in.
#![allow(dead_code)]

mod build;
mod classify;
pub mod policies;
pub mod policy;
pub mod types;

pub use build::{classify_path, tool_operation};

// Re-export the classification helpers used across the permission
// module (`engine::pattern_for_tool`, `engine::is_path_tool_name`).
pub use classify::{is_path_tool_name, pattern_for_tool};

use policy::{Decider, Modifier, PolicyCtx};
use types::{AccessRequest, Decision, Effect, Operation, Resource, TraceEntry};

/// Whether Accept mode may coerce a base `Ask` to `Allow` for this
/// (op, resource): low-risk operations (not shell/mcp/network/agent)
/// on a non-external resource. "trust the agent in cwd" doesn't
/// generalize to external code execution or out-of-tree paths.
fn accept_eligible(op: Operation, resource: &Resource) -> bool {
    let high_risk = matches!(
        op,
        Operation::Execute | Operation::Mcp | Operation::Network | Operation::Agent
    );
    let external_path = matches!(
        resource,
        Resource::Path {
            in_cwd: false,
            dev_null: false,
            ..
        }
    );
    !high_risk && !external_path
}

/// The registered-policy authorization engine. Holds the ordered
/// decider/modifier sets and the mutable [`PolicyCtx`].
pub struct Engine {
    deciders: Vec<Box<dyn Decider>>,
    modifiers: Vec<Box<dyn Modifier>>,
    ctx: PolicyCtx,
    /// Count of configured `Deny` rules (configured + external_directory),
    /// for the Yolo-mode "your deny rules are inert" startup warning.
    /// Set by `from_config`.
    pub(super) deny_rules: usize,
}

impl Engine {
    /// Construct an engine from an explicit, ordered policy set. The
    /// decider order IS the documented precedence. (The standard
    /// dirge policy set is assembled in a later phase; this keeps the
    /// core engine free of policy specifics and unit-testable with
    /// stub policies.)
    pub fn new(
        deciders: Vec<Box<dyn Decider>>,
        modifiers: Vec<Box<dyn Modifier>>,
        ctx: PolicyCtx,
    ) -> Self {
        Engine {
            deciders,
            modifiers,
            ctx,
            deny_rules: 0,
        }
    }

    /// Number of configured `Deny` rules — used to warn when Yolo mode
    /// renders them inert.
    pub fn deny_rule_count(&self) -> usize {
        self.deny_rules
    }

    /// Read-only access to the mutable context (for the UI's
    /// allowlist listing, `/why`, etc.).
    pub fn ctx(&self) -> &PolicyCtx {
        &self.ctx
    }

    pub fn ctx_mut(&mut self) -> &mut PolicyCtx {
        &mut self.ctx
    }

    /// Authorize a request. Pure read over `self` + `ctx`; no state is
    /// mutated (call [`Engine::commit`] after the decision resolves).
    pub fn authorize(&self, req: &AccessRequest) -> Decision {
        let mut trace: Vec<TraceEntry> = Vec::new();
        let mut resolved_paths = Vec::new();
        // Per resource: (effect, binding-trace-entry).
        let mut per_resource: Vec<(Effect, Option<TraceEntry>)> = Vec::new();

        for (ri, claim) in req.claims.iter().enumerate() {
            let op = claim.op;
            let resource = &claim.resource;
            if let Resource::Path { resolved, .. } = resource {
                resolved_paths.push(resolved.clone());
            }

            // ---- Stage A: deciders, first claim wins ----
            let mut base = Effect::Ask; // defensive default; DefaultActionPolicy normally claims
            let mut binding: Option<TraceEntry> = None;
            for d in &self.deciders {
                if !d.applies_to(op, resource) {
                    trace.push(TraceEntry {
                        policy: d.id(),
                        resource: ri,
                        effect: None,
                        why: "not applicable".to_string(),
                        applied: false,
                    });
                    continue;
                }
                match d.decide(req, op, resource, &self.ctx) {
                    Some(v) => {
                        let entry = TraceEntry {
                            policy: d.id(),
                            resource: ri,
                            effect: Some(v.effect),
                            why: v.why,
                            applied: true,
                        };
                        base = v.effect;
                        trace.push(entry.clone());
                        binding = Some(entry);
                        break; // first decisive claim wins
                    }
                    None => trace.push(TraceEntry {
                        policy: d.id(),
                        resource: ri,
                        effect: None,
                        why: "passed".to_string(),
                        applied: true,
                    }),
                }
            }

            // ---- Mode coercion (Accept): the one place a mode
            // LOOSENS. Accept turns a base `Ask` into `Allow` for
            // low-risk, in-tree operations, regardless of whether the
            // Ask came from a rule or the default. It lives here in the
            // base layer (not as a tighten-only Stage-B modifier) and
            // applies AFTER Stage A so it can relax a rule's Ask —
            // matching the legacy Accept semantics. Deny is never
            // touched (only `Ask` is coerced); high-risk ops
            // (Execute/Mcp/Network/Agent) and external paths are
            // excluded.
            if req.mode == crate::permission::SecurityMode::Accept
                && base == Effect::Ask
                && accept_eligible(op, resource)
            {
                base = Effect::Allow;
                let entry = TraceEntry {
                    policy: "accept-mode",
                    resource: ri,
                    effect: Some(Effect::Allow),
                    why: "accept mode coerced Ask→Allow".to_string(),
                    applied: true,
                };
                trace.push(entry.clone());
                binding = Some(entry);
            }

            // ---- Stage B: modifiers, monotone tighten ----
            let mut eff = base;
            for m in &self.modifiers {
                if !m.applies_to(op, resource) {
                    trace.push(TraceEntry {
                        policy: m.id(),
                        resource: ri,
                        effect: None,
                        why: "not applicable".to_string(),
                        applied: false,
                    });
                    continue;
                }
                let refined = m.refine(req, op, resource, eff, &self.ctx);
                if refined.by.is_some() {
                    let entry = TraceEntry {
                        policy: refined.by.unwrap(),
                        resource: ri,
                        effect: Some(refined.effect()),
                        why: refined.why.clone(),
                        applied: true,
                    };
                    trace.push(entry.clone());
                    eff = refined.effect();
                    binding = Some(entry); // the tightening modifier now owns the outcome
                } else {
                    trace.push(TraceEntry {
                        policy: m.id(),
                        resource: ri,
                        effect: None,
                        why: "no change".to_string(),
                        applied: true,
                    });
                }
            }

            per_resource.push((eff, binding));
        }

        // ---- Per-request fold: most restrictive across resources ----
        let final_effect = per_resource
            .iter()
            .map(|(e, _)| *e)
            .fold(Effect::Allow, Effect::meet);

        // The deciding entry is the binding of the (first) resource
        // whose effect equals the final (most-restrictive) effect.
        let deciding = per_resource
            .into_iter()
            .find(|(e, _)| *e == final_effect)
            .and_then(|(_, b)| b);

        Decision {
            effect: final_effect,
            deciding,
            trace,
            resolved_paths,
        }
    }

    /// Record the request after its decision resolves. The sole state
    /// mutation point: bumps the loop-guard counter for prompted
    /// requests so a genuine retry loop can eventually be hard-denied.
    /// Allowed/denied requests don't accumulate retry pressure.
    pub fn commit(&mut self, req: &AccessRequest, decision: &Decision) {
        if decision.effect == Effect::Ask {
            // Bump once per distinct (op, resource) the request carries —
            // NOT once per claim. A bash request can hold duplicate claims
            // (e.g. the same path as both a mutation target and a redirect
            // target); double-counting them would let the loop guard
            // hard-deny before the real retry threshold.
            let mut seen = std::collections::HashSet::new();
            for claim in &req.claims {
                let key = claim.resource.match_key();
                if seen.insert((claim.op, key)) {
                    self.ctx.repeat.record(claim.op, key);
                }
            }
        }
    }

    /// Register a session-scoped "allow always" grant (from the UI's
    /// AllowAlways reply).
    pub fn allow_always(&mut self, op: Operation, original: &str) {
        let pattern = if op == Operation::Execute || op == Operation::Mcp {
            crate::permission::pattern::Pattern::new_command(original)
        } else {
            crate::permission::pattern::Pattern::new(original)
        };
        self.ctx.allowlist.add(op, original, pattern);
    }
}

#[cfg(test)]
mod tests {
    use super::policy::*;
    use super::types::*;
    use super::*;
    use crate::permission::SecurityMode;
    use std::path::PathBuf;

    // ---- stub policies to exercise the algorithm in isolation ----

    struct AlwaysDecide(&'static str, Effect, bool /* applies */);
    impl Decider for AlwaysDecide {
        fn id(&self) -> &'static str {
            self.0
        }
        fn applies_to(&self, _: Operation, _: &Resource) -> bool {
            self.2
        }
        fn decide(
            &self,
            _: &AccessRequest,
            _: Operation,
            _: &Resource,
            _: &PolicyCtx,
        ) -> Option<Verdict> {
            Some(Verdict::new(self.1, "stub"))
        }
    }
    struct TightenTo(&'static str, Effect);
    impl Modifier for TightenTo {
        fn id(&self) -> &'static str {
            self.0
        }
        fn applies_to(&self, _: Operation, _: &Resource) -> bool {
            true
        }
        fn refine(
            &self,
            _: &AccessRequest,
            _: Operation,
            _: &Resource,
            cur: Effect,
            _: &PolicyCtx,
        ) -> Refined {
            Refined::tighten(cur, self.1, self.0, "stub tighten")
        }
    }

    fn req(resources: Vec<Resource>) -> AccessRequest {
        AccessRequest {
            tool: "test".to_string(),
            claims: resources
                .into_iter()
                .map(|r| Claim::new(Operation::Execute, r))
                .collect(),
            mode: SecurityMode::Standard,
            display_input: "test".to_string(),
        }
    }
    fn cmd(s: &str) -> Resource {
        Resource::Command {
            raw: s.to_string(),
            head: s.split_whitespace().next().unwrap_or("").to_string(),
        }
    }
    fn path(p: &str) -> Resource {
        Resource::Path {
            raw: p.to_string(),
            resolved: PathBuf::from(p),
            in_cwd: false,
            dev_null: false,
        }
    }

    #[test]
    fn first_decider_claim_wins() {
        let e = Engine::new(
            vec![
                Box::new(AlwaysDecide("a", Effect::Allow, true)),
                Box::new(AlwaysDecide("b", Effect::Deny, true)),
            ],
            vec![],
            PolicyCtx::default(),
        );
        let d = e.authorize(&req(vec![cmd("x")]));
        assert_eq!(d.effect, Effect::Allow);
        assert_eq!(d.deciding.unwrap().policy, "a");
    }

    #[test]
    fn non_applicable_decider_is_skipped() {
        let e = Engine::new(
            vec![
                Box::new(AlwaysDecide("skipme", Effect::Allow, false)),
                Box::new(AlwaysDecide("real", Effect::Deny, true)),
            ],
            vec![],
            PolicyCtx::default(),
        );
        let d = e.authorize(&req(vec![cmd("x")]));
        assert_eq!(d.effect, Effect::Deny);
        assert_eq!(d.deciding.unwrap().policy, "real");
        // skipped decider recorded as applied:false
        assert!(d.trace.iter().any(|t| t.policy == "skipme" && !t.applied));
    }

    #[test]
    fn modifier_tightens_but_cannot_loosen() {
        // base Allow, modifier tries to tighten to Ask -> Ask
        let e = Engine::new(
            vec![Box::new(AlwaysDecide("base", Effect::Allow, true))],
            vec![Box::new(TightenTo("tighten", Effect::Ask))],
            PolicyCtx::default(),
        );
        let d = e.authorize(&req(vec![cmd("x")]));
        assert_eq!(d.effect, Effect::Ask);
        assert_eq!(d.deciding.unwrap().policy, "tighten");

        // base Deny, modifier "tightens" to Ask -> stays Deny (loosening blocked)
        let e = Engine::new(
            vec![Box::new(AlwaysDecide("base", Effect::Deny, true))],
            vec![Box::new(TightenTo("tighten", Effect::Ask))],
            PolicyCtx::default(),
        );
        let d = e.authorize(&req(vec![cmd("x")]));
        assert_eq!(d.effect, Effect::Deny);
        assert_eq!(d.deciding.unwrap().policy, "base");
    }

    #[test]
    fn multi_resource_folds_most_restrictive() {
        // one Allow resource + one Deny resource -> Deny overall
        struct PerResource;
        impl Decider for PerResource {
            fn id(&self) -> &'static str {
                "perres"
            }
            fn applies_to(&self, _: Operation, _: &Resource) -> bool {
                true
            }
            fn decide(
                &self,
                _: &AccessRequest,
                _: Operation,
                r: &Resource,
                _: &PolicyCtx,
            ) -> Option<Verdict> {
                let eff = if r.match_key().contains("bad") {
                    Effect::Deny
                } else {
                    Effect::Allow
                };
                Some(Verdict::new(eff, "perres"))
            }
        }
        let e = Engine::new(vec![Box::new(PerResource)], vec![], PolicyCtx::default());
        let d = e.authorize(&req(vec![cmd("good"), cmd("bad"), path("/x")]));
        assert_eq!(d.effect, Effect::Deny);
        assert_eq!(d.resolved_paths, vec![PathBuf::from("/x")]);
    }

    #[test]
    fn commit_only_counts_prompted_requests() {
        let mut e = Engine::new(
            vec![Box::new(AlwaysDecide("ask", Effect::Ask, true))],
            vec![],
            PolicyCtx::default(),
        );
        let r = req(vec![cmd("loopy")]);
        assert_eq!(e.ctx().repeat.prior(Operation::Execute, "loopy"), 0);
        let d = e.authorize(&r);
        e.commit(&r, &d);
        assert_eq!(e.ctx().repeat.prior(Operation::Execute, "loopy"), 1);

        // an allowed request does not accumulate retry pressure
        let mut e2 = Engine::new(
            vec![Box::new(AlwaysDecide("allow", Effect::Allow, true))],
            vec![],
            PolicyCtx::default(),
        );
        let d2 = e2.authorize(&r);
        e2.commit(&r, &d2);
        assert_eq!(e2.ctx().repeat.prior(Operation::Execute, "loopy"), 0);
    }
}
