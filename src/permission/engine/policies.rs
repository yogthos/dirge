//! The registered dirge policy set.
//!
//! Deciders are listed here in precedence order (the order they must
//! be registered in [`super::Engine::new`]); the first to claim a
//! resource sets its base effect:
//!
//! 1. [`PromptDenyPolicy`]   — frontmatter `deny_tools`; terminal Deny, beats Yolo.
//! 2. [`YoloPolicy`]         — `mode == Yolo`; terminal Allow.
//! 3. [`ConfiguredDenyPolicy`] — configured `deny` (last-match), terminal above
//!    session-allow so a session grant can't override it (dirge-ct16). Below
//!    Yolo: Yolo's documented "all rules off" is preserved.
//! 4. [`SessionAllowlistPolicy`] — user "allow always"; terminal Allow.
//! 5. [`ConfiguredRulePolicy`]   — user rules, last-match-wins inside (the
//!    allow/ask cases; a last-match `deny` is pre-empted by `ConfiguredDeny`).
//! 6. [`BuiltinAllowPolicy`]     — read-only ops, memory/skill, dev-null, in-cwd writes.
//! 7. [`ExternalDirPolicy`]      — out-of-cwd paths → external_directory rule or Ask.
//! 8. [`DefaultActionPolicy`]    — the configured default (always claims; terminal).
//!
//! Accept-mode loosening is NOT a decider — it's a post-Stage-A base
//! coercion in `Engine::authorize` (it relaxes a base `Ask`→`Allow`,
//! the one place a mode loosens).
//!
//! Modes are folded into the deciders (not a separate stage): because
//! explicit user rules (ConfiguredRule) outrank the builtin/default
//! deciders, an explicit `allow` survives Restrictive automatically —
//! the provenance question that plagued the old `check`/`check_path`
//! Restrictive logic dissolves into precedence order.
//!
//! The sole Stage-B modifier is [`LoopGuardPolicy`], which only ever
//! tightens (never gates an already-allowed op) and hard-denies a
//! genuine retry loop.

use super::policy::{Decider, Modifier, PolicyCtx};
use super::types::{AccessRequest, Effect, Operation, Refined, Resource, Verdict};
use crate::permission::SecurityMode;
use crate::permission::pattern::Pattern;

/// Which operations a configured rule governs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpMatch {
    Any,
    One(Operation),
}

impl OpMatch {
    fn matches(self, op: Operation) -> bool {
        match self {
            OpMatch::Any => true,
            OpMatch::One(o) => o == op,
        }
    }
}

/// One configured authorization rule: "for this operation class
/// (optionally narrowed to a concrete tool) and resource pattern,
/// apply this effect." The ordered list reads top-to-bottom as the
/// precedence (last match wins within the list).
///
/// `tool` lets the legacy per-tool config (`grep`, `read`, …) map
/// faithfully even though several tools share an [`Operation`] — a
/// `grep` rule narrows to `tool == "grep"` so it doesn't also gate
/// `read`. The breaking op-based config (Phase 4) leaves `tool: None`.
#[derive(Debug, Clone)]
pub struct Rule {
    pub op: OpMatch,
    pub tool: Option<String>,
    pub pattern: Pattern,
    pub effect: Effect,
    pub original: String,
}

impl Rule {
    fn matches(&self, req: &AccessRequest, op: Operation, resource: &Resource) -> bool {
        self.op.matches(op)
            && self.tool.as_deref().is_none_or(|t| t == req.tool)
            && resource
                .match_candidates()
                .iter()
                .any(|k| self.pattern.matches(k))
    }
}

/// The last (top-to-bottom precedence) configured rule that matches this
/// claim, or `None`. "Last-match-wins" is the configured-rule semantics,
/// shared by [`ConfiguredRulePolicy`] and [`ConfiguredDenyPolicy`] so both
/// agree on which single rule governs a claim.
fn last_match<'r>(
    rules: &'r [Rule],
    req: &AccessRequest,
    op: Operation,
    resource: &Resource,
) -> Option<&'r Rule> {
    rules
        .iter()
        .filter(|r| r.matches(req, op, resource))
        .next_back()
}

/// Whether a resource is a filesystem path OUTSIDE the working directory
/// (and not `/dev/null`). Governs the external-directory deciders.
fn is_external_path(resource: &Resource) -> bool {
    matches!(
        resource,
        Resource::Path {
            in_cwd: false,
            dev_null: false,
            ..
        }
    )
}

// ---------------------------------------------------------------------------
// Stage A — deciders
// ---------------------------------------------------------------------------

/// Frontmatter `deny_tools`: the active prompt forbids a tool/op. Runs
/// first so it beats even Yolo's blanket allow. Reads `ctx.prompt_deny`
/// (tool names, case-insensitive). For MCP resources the concrete
/// `server:tool` name is probed too.
pub struct PromptDenyPolicy;

impl Decider for PromptDenyPolicy {
    fn id(&self) -> &'static str {
        "prompt-deny"
    }
    fn applies_to(&self, _: Operation, _: &Resource) -> bool {
        true
    }
    fn decide(
        &self,
        req: &AccessRequest,
        _op: Operation,
        resource: &Resource,
        ctx: &PolicyCtx,
    ) -> Option<Verdict> {
        let denied = |name: &str| ctx.prompt_deny.iter().any(|d| d.eq_ignore_ascii_case(name));
        let hit =
            denied(&req.tool) || matches!(resource, Resource::Mcp { name, .. } if denied(name));
        hit.then(|| {
            Verdict::new(
                Effect::Deny,
                format!(
                    "tool {:?} denied by the active prompt's deny_tools",
                    req.tool
                ),
            )
        })
    }
}

/// `--yolo` / `mode: yolo`: allow everything (after prompt-deny).
pub struct YoloPolicy;

impl Decider for YoloPolicy {
    fn id(&self) -> &'static str {
        "yolo"
    }
    fn applies_to(&self, _: Operation, _: &Resource) -> bool {
        true
    }
    fn decide(
        &self,
        req: &AccessRequest,
        _op: Operation,
        _: &Resource,
        _: &PolicyCtx,
    ) -> Option<Verdict> {
        (req.mode == SecurityMode::Yolo).then(|| Verdict::new(Effect::Allow, "yolo mode"))
    }
}

/// Session "allow always" grants, scoped by [`Operation`]. Op-scoping
/// is what lets one Edit grant cover write/edit/apply_patch without the
/// old allowlist mirroring.
pub struct SessionAllowlistPolicy;

impl Decider for SessionAllowlistPolicy {
    fn id(&self) -> &'static str {
        "session-allow"
    }
    fn applies_to(&self, _: Operation, _: &Resource) -> bool {
        true
    }
    fn decide(
        &self,
        _req: &AccessRequest,
        op: Operation,
        resource: &Resource,
        ctx: &PolicyCtx,
    ) -> Option<Verdict> {
        resource
            .match_candidates()
            .iter()
            .any(|k| ctx.allowlist.allows(op, k))
            .then(|| Verdict::new(Effect::Allow, "allowed for this session"))
    }
}

/// User-configured rules, last-match-wins within the ordered list.
pub struct ConfiguredRulePolicy {
    pub rules: Vec<Rule>,
}

impl Decider for ConfiguredRulePolicy {
    fn id(&self) -> &'static str {
        "configured-rule"
    }
    fn applies_to(&self, op: Operation, resource: &Resource) -> bool {
        // Coarse: op + pattern (the tool narrowing is applied in
        // `decide`, which has the full request). A false positive here
        // only means `decide` runs and returns None — harmless.
        let cands = resource.match_candidates();
        self.rules
            .iter()
            .any(|r| r.op.matches(op) && cands.iter().any(|k| r.pattern.matches(k)))
    }
    fn decide(
        &self,
        req: &AccessRequest,
        op: Operation,
        resource: &Resource,
        _: &PolicyCtx,
    ) -> Option<Verdict> {
        last_match(&self.rules, req, op, resource)
            .map(|r| Verdict::new(r.effect, format!("rule {:?} → {:?}", r.original, r.effect)))
    }
}

/// dirge-ct16: configured `deny` rules made TERMINAL above session-allow
/// (and the allow/ask deciders). Registered between [`YoloPolicy`] and
/// [`SessionAllowlistPolicy`], so an explicit user deny can no longer be
/// silently overridden by a (broad, one-keypress) session allow-always
/// grant — while Yolo's documented "all rules off" behavior is preserved
/// (this sits BELOW Yolo).
///
/// Honors last-match-wins: it only claims when the LAST matching rule is a
/// `Deny`, so a later `allow` overriding an earlier `deny` is respected and
/// NOT turned terminal. Covers both the main rule list and
/// `external_directory` rules (for external paths) — both previously sat
/// below session-allow and shared the bypass. Main rules outrank
/// `external_directory` (mirroring the `ConfiguredRule` < `ExternalDir`
/// decider order): when a main rule matches, it alone decides terminality.
pub struct ConfiguredDenyPolicy {
    pub rules: Vec<Rule>,
    pub ext_rules: Vec<Rule>,
}

impl Decider for ConfiguredDenyPolicy {
    fn id(&self) -> &'static str {
        "configured-deny"
    }
    fn applies_to(&self, _: Operation, _: &Resource) -> bool {
        true // claims only on a last-match Deny; cheap to evaluate in decide
    }
    fn decide(
        &self,
        req: &AccessRequest,
        op: Operation,
        resource: &Resource,
        _: &PolicyCtx,
    ) -> Option<Verdict> {
        // Main rules govern any op/resource and outrank external_directory:
        // if a main rule matches, it alone decides terminality (we do NOT
        // fall through to ext rules — that would let an ext deny beat a
        // main allow, reversing precedence).
        if let Some(r) = last_match(&self.rules, req, op, resource) {
            return (r.effect == Effect::Deny).then(|| {
                Verdict::new(
                    Effect::Deny,
                    format!("rule {:?} → Deny (terminal)", r.original),
                )
            });
        }
        // No main rule matched: an external_directory deny on an external
        // mutating path is likewise terminal above session-allow.
        if matches!(op, Operation::Edit)
            && is_external_path(resource)
            && let Some(r) = last_match(&self.ext_rules, req, op, resource)
        {
            return (r.effect == Effect::Deny).then(|| {
                Verdict::new(
                    Effect::Deny,
                    format!("external_directory {:?} → Deny (terminal)", r.original),
                )
            });
        }
        None
    }
}

/// Built-in transparent allows: read-only observation anywhere, memory
/// reads (and writes outside Restrictive), skill reads (and writes
/// outside Restrictive), `/dev/null`, and in-cwd file writes/edits.
pub struct BuiltinAllowPolicy;

impl BuiltinAllowPolicy {
    /// The effect this policy would contribute, or `None` to pass.
    fn effect_for(op: Operation, req: &AccessRequest, resource: &Resource) -> Option<Effect> {
        let restrictive = req.mode == SecurityMode::Restrictive;
        match op {
            // Observation is always transparent — even in Restrictive,
            // even outside the cwd (the old global `read **` allow).
            Operation::Read => Some(Effect::Allow),

            // Memory/skill: reads always allowed; writes allowed except
            // under Restrictive, where they confirm.
            Operation::Memory | Operation::Skill => {
                let is_read = matches!(resource, Resource::Bareword(a) if is_read_action(op, a));
                if is_read || !restrictive {
                    Some(Effect::Allow)
                } else {
                    Some(Effect::Ask)
                }
            }

            // File mutation: dev-null is a harmless bit-bucket; in-cwd
            // writes are trusted (confirmed under Restrictive).
            Operation::Edit => match resource {
                Resource::Path { dev_null: true, .. } => Some(Effect::Allow),
                Resource::Path { in_cwd: true, .. } => Some(if restrictive {
                    Effect::Ask
                } else {
                    Effect::Allow
                }),
                _ => None,
            },

            // Internal/meta tools have no external effect.
            Operation::Meta => Some(Effect::Allow),

            // Execute / Network / Mcp / Agent (recursive task) are
            // never builtin-allowed; Other (unknown/plugin) falls to
            // configured rules or the default.
            Operation::Execute
            | Operation::Network
            | Operation::Mcp
            | Operation::Agent
            | Operation::Other => None,
        }
    }
}

impl Decider for BuiltinAllowPolicy {
    fn id(&self) -> &'static str {
        "builtin-allow"
    }
    fn applies_to(&self, op: Operation, resource: &Resource) -> bool {
        // Cheap structural check; the precise effect (and the
        // Restrictive Ask) is computed in `decide`.
        matches!(
            op,
            Operation::Read | Operation::Memory | Operation::Skill | Operation::Meta
        ) || matches!(
            (op, resource),
            (Operation::Edit, Resource::Path { in_cwd: true, .. })
                | (Operation::Edit, Resource::Path { dev_null: true, .. })
        )
    }
    fn decide(
        &self,
        req: &AccessRequest,
        op: Operation,
        resource: &Resource,
        _: &PolicyCtx,
    ) -> Option<Verdict> {
        Self::effect_for(op, req, resource).map(|e| {
            let why = match e {
                Effect::Allow => "built-in allow",
                _ => "restrictive mode confirms writes",
            };
            Verdict::new(e, why)
        })
    }
}

/// Read-only action names for memory/skill (everything else is a write).
fn is_read_action(op: Operation, action: &str) -> bool {
    match op {
        Operation::Memory => action == "view",
        Operation::Skill => action == "load" || action == "list",
        _ => false,
    }
}

/// Paths outside the working directory: honor `external_directory`
/// rules, otherwise require confirmation. Only claims external paths.
pub struct ExternalDirPolicy {
    pub rules: Vec<Rule>,
}

impl Decider for ExternalDirPolicy {
    fn id(&self) -> &'static str {
        "external-dir"
    }
    fn applies_to(&self, op: Operation, resource: &Resource) -> bool {
        // Only governs mutating access to external paths; external
        // reads are already allowed by BuiltinAllow (higher precedence).
        matches!(op, Operation::Edit) && is_external_path(resource)
    }
    fn decide(
        &self,
        req: &AccessRequest,
        op: Operation,
        resource: &Resource,
        _: &PolicyCtx,
    ) -> Option<Verdict> {
        if !is_external_path(resource) {
            return None;
        }
        let matched = last_match(&self.rules, req, op, resource);
        match matched {
            Some(r) => Some(Verdict::new(
                r.effect,
                format!("external_directory {:?} → {:?}", r.original, r.effect),
            )),
            None => Some(Verdict::new(Effect::Ask, "outside the working directory")),
        }
    }
}

/// The configured default — always claims, so every resource has a
/// base effect. Demotes a default of Allow to Ask under Restrictive.
pub struct DefaultActionPolicy {
    pub default: Effect,
}

impl Decider for DefaultActionPolicy {
    fn id(&self) -> &'static str {
        "default"
    }
    fn applies_to(&self, _: Operation, _: &Resource) -> bool {
        true
    }
    fn decide(
        &self,
        req: &AccessRequest,
        _op: Operation,
        _: &Resource,
        _: &PolicyCtx,
    ) -> Option<Verdict> {
        let eff = if req.mode == SecurityMode::Restrictive && self.default == Effect::Allow {
            Effect::Ask
        } else {
            self.default
        };
        Some(Verdict::new(eff, "default action"))
    }
}

// ---------------------------------------------------------------------------
// Stage B — modifiers (monotone, tighten-only)
// ---------------------------------------------------------------------------

/// Breaks genuine retry loops WITHOUT ever gating an allowed op. It
/// only acts when the base effect is already `Ask` (the op was going
/// to prompt anyway) and the same (op, resource) has been prompted
/// `threshold`+ times before — then it hard-denies. Reads-only,
/// in-cwd writes, memory/skill, dev-null (all `Allow`) are structurally
/// untouched. The counter is bumped in `Engine::commit`, never here.
pub struct LoopGuardPolicy {
    pub threshold: u32,
}

impl Modifier for LoopGuardPolicy {
    fn id(&self) -> &'static str {
        "loop-guard"
    }
    fn applies_to(&self, _: Operation, _: &Resource) -> bool {
        true // gated on `current == Ask` inside refine
    }
    fn refine(
        &self,
        req: &AccessRequest,
        op: Operation,
        resource: &Resource,
        current: Effect,
        ctx: &PolicyCtx,
    ) -> Refined {
        // NEVER touch an allowed (or already-denied) op. Only an op
        // that is going to prompt can be a retry loop.
        if current != Effect::Ask {
            return Refined::noop(current);
        }
        let prior = ctx.repeat.prior(op, resource.match_key());
        if prior >= self.threshold {
            let preview: String = resource.match_key().chars().take(60).collect();
            Refined::tighten(
                current,
                Effect::Deny,
                "loop-guard",
                format!(
                    "Doom loop: repeated identical {} call ({preview}) prompted {prior}× — breaking retry loop",
                    req.tool
                ),
            )
        } else {
            Refined::noop(current)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::engine::Engine;
    use std::path::PathBuf;

    fn engine(default: Effect) -> Engine {
        Engine::new(
            vec![
                Box::new(PromptDenyPolicy),
                Box::new(YoloPolicy),
                Box::new(ConfiguredDenyPolicy {
                    rules: vec![],
                    ext_rules: vec![],
                }),
                Box::new(SessionAllowlistPolicy),
                Box::new(ConfiguredRulePolicy { rules: vec![] }),
                Box::new(BuiltinAllowPolicy),
                Box::new(ExternalDirPolicy { rules: vec![] }),
                Box::new(DefaultActionPolicy { default }),
            ],
            vec![Box::new(LoopGuardPolicy { threshold: 3 })],
            PolicyCtx::default(),
        )
    }

    fn req(op: Operation, mode: SecurityMode, resources: Vec<Resource>) -> AccessRequest {
        AccessRequest {
            tool: format!("{op:?}").to_lowercase(),
            claims: resources
                .into_iter()
                .map(|r| crate::permission::engine::types::Claim::new(op, r))
                .collect(),
            mode,
            display_input: String::new(),
        }
    }
    fn path(p: &str, in_cwd: bool, dev_null: bool) -> Resource {
        Resource::Path {
            raw: p.to_string(),
            resolved: PathBuf::from(p),
            in_cwd,
            dev_null,
        }
    }
    fn cmd(s: &str) -> Resource {
        Resource::Command {
            raw: s.to_string(),
            head: s.split_whitespace().next().unwrap_or("").to_string(),
        }
    }
    fn url(u: &str) -> Resource {
        Resource::Url(u.to_string())
    }
    fn word(s: &str) -> Resource {
        Resource::Bareword(s.to_string())
    }

    #[test]
    fn read_always_allowed_even_external_and_restrictive() {
        for mode in [SecurityMode::Standard, SecurityMode::Restrictive] {
            let e = engine(Effect::Ask);
            let d = e.authorize(&req(
                Operation::Read,
                mode,
                vec![path("/etc/x", false, false)],
            ));
            assert_eq!(d.effect, Effect::Allow, "read external in {mode:?}");
            assert_eq!(d.deciding.unwrap().policy, "builtin-allow");
        }
    }

    #[test]
    fn in_cwd_write_allowed_standard_confirms_restrictive() {
        let e = engine(Effect::Ask);
        let d = e.authorize(&req(
            Operation::Edit,
            SecurityMode::Standard,
            vec![path("/proj/x", true, false)],
        ));
        assert_eq!(d.effect, Effect::Allow);
        let d = e.authorize(&req(
            Operation::Edit,
            SecurityMode::Restrictive,
            vec![path("/proj/x", true, false)],
        ));
        assert_eq!(d.effect, Effect::Ask);
    }

    #[test]
    fn external_write_asks_dev_null_allows() {
        let e = engine(Effect::Ask);
        let d = e.authorize(&req(
            Operation::Edit,
            SecurityMode::Standard,
            vec![path("/etc/x", false, false)],
        ));
        assert_eq!(d.effect, Effect::Ask);
        assert_eq!(d.deciding.unwrap().policy, "external-dir");

        let d = e.authorize(&req(
            Operation::Edit,
            SecurityMode::Standard,
            vec![path("/dev/null", false, true)],
        ));
        assert_eq!(d.effect, Effect::Allow);
    }

    #[test]
    fn memory_skill_transparent_standard_writes_confirm_restrictive() {
        let e = engine(Effect::Ask);
        // standard: all actions allow
        for (op, action) in [
            (Operation::Memory, "add"),
            (Operation::Memory, "view"),
            (Operation::Skill, "create:x"),
            (Operation::Skill, "load"),
        ] {
            let d = e.authorize(&req(op, SecurityMode::Standard, vec![word(action)]));
            assert_eq!(d.effect, Effect::Allow, "{op:?} {action} standard");
        }
        // restrictive: reads allow, writes ask
        let d = e.authorize(&req(
            Operation::Memory,
            SecurityMode::Restrictive,
            vec![word("view")],
        ));
        assert_eq!(d.effect, Effect::Allow);
        let d = e.authorize(&req(
            Operation::Memory,
            SecurityMode::Restrictive,
            vec![word("add")],
        ));
        assert_eq!(d.effect, Effect::Ask);
        let d = e.authorize(&req(
            Operation::Skill,
            SecurityMode::Restrictive,
            vec![word("load")],
        ));
        assert_eq!(d.effect, Effect::Allow);
        let d = e.authorize(&req(
            Operation::Skill,
            SecurityMode::Restrictive,
            vec![word("create:x")],
        ));
        assert_eq!(d.effect, Effect::Ask);
    }

    #[test]
    fn execute_falls_to_default_ask_not_builtin() {
        let e = engine(Effect::Ask);
        let d = e.authorize(&req(
            Operation::Execute,
            SecurityMode::Standard,
            vec![cmd("rm -rf x")],
        ));
        assert_eq!(d.effect, Effect::Ask);
        assert_eq!(d.deciding.unwrap().policy, "default");
    }

    #[test]
    fn prompt_deny_beats_yolo() {
        let mut e = engine(Effect::Ask);
        e.ctx_mut().prompt_deny = vec!["execute".to_string()];
        let d = e.authorize(&req(
            Operation::Execute,
            SecurityMode::Yolo,
            vec![cmd("anything")],
        ));
        assert_eq!(d.effect, Effect::Deny);
        assert_eq!(d.deciding.unwrap().policy, "prompt-deny");
    }

    #[test]
    fn yolo_allows_otherwise() {
        let e = engine(Effect::Ask);
        let d = e.authorize(&req(
            Operation::Execute,
            SecurityMode::Yolo,
            vec![cmd("rm -rf /")],
        ));
        assert_eq!(d.effect, Effect::Allow);
        assert_eq!(d.deciding.unwrap().policy, "yolo");
    }

    #[test]
    fn accept_coerces_default_but_not_execute() {
        let e = engine(Effect::Ask);
        // a meta op with no builtin claim would default to Ask; accept → Allow
        let d = e.authorize(&req(
            Operation::Network,
            SecurityMode::Accept,
            vec![Resource::Url("http://x".into())],
        ));
        // Network is high-risk → accept does NOT coerce
        assert_eq!(d.effect, Effect::Ask);
        // Execute also not coerced
        let d = e.authorize(&req(
            Operation::Execute,
            SecurityMode::Accept,
            vec![cmd("x")],
        ));
        assert_eq!(d.effect, Effect::Ask);
    }

    /// Concise rule constructor for tests (`tool: None`, op-based).
    fn rule(op: OpMatch, pattern: Pattern, effect: Effect) -> Rule {
        let original = pattern.original.clone();
        Rule {
            op,
            tool: None,
            pattern,
            effect,
            original,
        }
    }

    /// Build an engine whose ConfiguredRulePolicy carries `rules`.
    fn engine_with_rules(rules: Vec<Rule>) -> Engine {
        Engine::new(
            vec![
                Box::new(PromptDenyPolicy),
                Box::new(YoloPolicy),
                Box::new(ConfiguredDenyPolicy {
                    rules: rules.clone(),
                    ext_rules: vec![],
                }),
                Box::new(SessionAllowlistPolicy),
                Box::new(ConfiguredRulePolicy { rules }),
                Box::new(BuiltinAllowPolicy),
                Box::new(ExternalDirPolicy { rules: vec![] }),
                Box::new(DefaultActionPolicy {
                    default: Effect::Ask,
                }),
            ],
            vec![Box::new(LoopGuardPolicy { threshold: 3 })],
            PolicyCtx::default(),
        )
    }

    #[test]
    fn configured_rule_last_match_wins_and_beats_builtin() {
        let e = engine_with_rules(vec![
            rule(
                OpMatch::One(Operation::Execute),
                Pattern::new_command("cargo *"),
                Effect::Allow,
            ),
            rule(
                OpMatch::One(Operation::Read),
                Pattern::new("/secret/**"),
                Effect::Deny,
            ),
        ]);
        // explicit allow on execute cargo (would otherwise default to Ask)
        let d = e.authorize(&req(
            Operation::Execute,
            SecurityMode::Standard,
            vec![cmd("cargo test")],
        ));
        assert_eq!(d.effect, Effect::Allow);
        assert_eq!(d.deciding.unwrap().policy, "configured-rule");
        // explicit deny on read beats builtin-allow (rule has higher precedence)
        let d = e.authorize(&req(
            Operation::Read,
            SecurityMode::Standard,
            vec![path("/secret/k", false, false)],
        ));
        assert_eq!(d.effect, Effect::Deny);
        // The deny is now decided by the terminal configured-deny decider
        // (dirge-ct16), which sits above session-allow.
        assert_eq!(d.deciding.unwrap().policy, "configured-deny");
    }

    #[test]
    fn last_match_wins_within_rule_list() {
        // two matching rules; the later one wins
        let e = engine_with_rules(vec![
            rule(OpMatch::Any, Pattern::new("/proj/**"), Effect::Deny),
            rule(
                OpMatch::One(Operation::Edit),
                Pattern::new("/proj/ok/**"),
                Effect::Allow,
            ),
        ]);
        let d = e.authorize(&req(
            Operation::Edit,
            SecurityMode::Standard,
            vec![path("/proj/ok/a.rs", true, false)],
        ));
        assert_eq!(d.effect, Effect::Allow);
        let d = e.authorize(&req(
            Operation::Edit,
            SecurityMode::Standard,
            vec![path("/proj/no/a.rs", true, false)],
        ));
        assert_eq!(d.effect, Effect::Deny);
    }

    #[test]
    fn loop_guard_never_gates_allowed_only_hard_denies_ask_retries() {
        // builtin-allowed read repeated many times: stays Allowed.
        let mut e = engine(Effect::Ask);
        let r = req(
            Operation::Read,
            SecurityMode::Standard,
            vec![path("/proj/x", true, false)],
        );
        for _ in 0..6 {
            let d = e.authorize(&r);
            assert_eq!(d.effect, Effect::Allow, "repeated read must never prompt");
            e.commit(&r, &d);
        }
        // an Ask op retried past threshold escalates to Deny.
        let mut e = engine(Effect::Ask);
        let r = req(
            Operation::Execute,
            SecurityMode::Standard,
            vec![cmd("frobnicate")],
        );
        let mut effects = vec![];
        for _ in 0..5 {
            let d = e.authorize(&r);
            effects.push(d.effect);
            e.commit(&r, &d);
        }
        // first `threshold` (3) are Ask, then hard-Deny.
        assert_eq!(effects[0], Effect::Ask);
        assert_eq!(effects[1], Effect::Ask);
        assert_eq!(effects[2], Effect::Ask);
        assert_eq!(
            effects[3],
            Effect::Deny,
            "4th identical Ask retry hard-denied"
        );
        assert_eq!(effects[4], Effect::Deny);
    }

    #[test]
    fn approved_repeats_never_trip_loop_guard() {
        // A repeated Ask op that the user APPROVES each time (modeled by
        // `note_allowed` after each commit) must keep prompting forever —
        // it never accrues toward the hard-deny.
        let mut e = engine(Effect::Ask);
        let r = req(
            Operation::Network,
            SecurityMode::Standard,
            vec![url("https://example.com/x")],
        );
        for i in 0..10 {
            let d = e.authorize(&r);
            assert_eq!(d.effect, Effect::Ask, "iteration {i}: must keep asking");
            e.commit(&r, &d);
            e.note_allowed(&r); // user approved this prompt
        }
    }

    #[test]
    fn denials_still_hard_deny_even_after_an_earlier_approval() {
        // approve once (resets), then 4 denials → still hard-denies, so
        // the protection against a genuinely-stuck loop is preserved.
        let mut e = engine(Effect::Ask);
        let r = req(
            Operation::Execute,
            SecurityMode::Standard,
            vec![cmd("frobnicate")],
        );
        let d = e.authorize(&r);
        e.commit(&r, &d);
        e.note_allowed(&r); // approved → counter reset to 0
        let mut effects = vec![];
        for _ in 0..5 {
            let d = e.authorize(&r);
            effects.push(d.effect);
            e.commit(&r, &d); // denied at the prompt → no note_allowed
        }
        assert_eq!(
            effects[3],
            Effect::Deny,
            "hard-deny still trips on pure denials"
        );
    }

    // ---- dirge-ct16: configured `deny` is terminal above session-allow ----

    #[test]
    fn configured_deny_beats_session_allow() {
        // A configured DENY must not be overridable by a (broad) session
        // allow-always grant. Scenario: user denies edit of /etc/secret/**
        // in config, then allow-always'd a sibling path → a broad /etc/**
        // session grant. The narrow deny must still hold.
        let mut e = engine_with_rules(vec![rule(
            OpMatch::One(Operation::Edit),
            Pattern::new("/etc/secret/**"),
            Effect::Deny,
        )]);
        e.allow_always(Operation::Edit, "/etc/**");
        let d = e.authorize(&req(
            Operation::Edit,
            SecurityMode::Standard,
            vec![path("/etc/secret/k", false, false)],
        ));
        assert_eq!(
            d.effect,
            Effect::Deny,
            "a session allow-always must not override a configured deny",
        );
        assert_eq!(d.deciding.unwrap().policy, "configured-deny");
    }

    #[test]
    fn configured_deny_respects_last_match_allow() {
        // Last-match-wins is preserved: a later `allow` overriding an
        // earlier `deny` must NOT be turned terminal by the deny decider.
        let e = engine_with_rules(vec![
            rule(OpMatch::Any, Pattern::new("/proj/**"), Effect::Deny),
            rule(
                OpMatch::One(Operation::Edit),
                Pattern::new("/proj/ok/**"),
                Effect::Allow,
            ),
        ]);
        let d = e.authorize(&req(
            Operation::Edit,
            SecurityMode::Standard,
            vec![path("/proj/ok/a.rs", true, false)],
        ));
        assert_eq!(
            d.effect,
            Effect::Allow,
            "later allow still wins by last-match"
        );
        let d = e.authorize(&req(
            Operation::Edit,
            SecurityMode::Standard,
            vec![path("/proj/no/a.rs", true, false)],
        ));
        assert_eq!(d.effect, Effect::Deny);
        assert_eq!(d.deciding.unwrap().policy, "configured-deny");
    }

    #[test]
    fn yolo_still_overrides_configured_deny() {
        // We deliberately kept config-deny BELOW yolo: yolo's documented
        // "all rules off" contract (and its startup warning) is preserved.
        let e = engine_with_rules(vec![rule(
            OpMatch::One(Operation::Edit),
            Pattern::new("/etc/**"),
            Effect::Deny,
        )]);
        let d = e.authorize(&req(
            Operation::Edit,
            SecurityMode::Yolo,
            vec![path("/etc/x", false, false)],
        ));
        assert_eq!(d.effect, Effect::Allow);
        assert_eq!(d.deciding.unwrap().policy, "yolo");
    }
}
