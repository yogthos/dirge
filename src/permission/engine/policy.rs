//! The [`Decider`] and [`Modifier`] policy traits and the mutable
//! [`PolicyCtx`] they read.
//!
//! Two-stage authorization:
//! - **Deciders** (Stage A) run in registered precedence order; the
//!   first one that *claims* a resource sets its base [`Effect`].
//!   Deciders MAY produce any effect (including a looser Allow), which
//!   is why mode-loosening (Accept) lives here, in the audited base.
//! - **Modifiers** (Stage B) refine the base; [`Refined`] makes it
//!   impossible to loosen, so tightening-only concerns (Restrictive
//!   demotion, the loop guard) live here and their order can't matter.
//!
//! Policies are stateless strategy objects. All mutable engine state
//! lives in [`PolicyCtx`]: `decide`/`refine` READ it; the engine's
//! `commit` (after the decision resolves) is the only writer. That
//! single-writer split is what removes the doom-loop's
//! count-before-vs-after ordering bug by construction.

use std::collections::HashMap;

use super::types::{AccessRequest, Effect, Operation, Refined, Resource, Verdict};
use crate::permission::pattern::Pattern;

/// Per-(operation, resource) repeat counter backing the loop guard.
/// Keyed by a stable identity string so an agent re-requesting the
/// exact same gated action can be detected and ultimately hard-denied.
#[derive(Debug, Default)]
pub struct RepeatCounter {
    counts: HashMap<String, u32>,
}

impl RepeatCounter {
    fn key(op: Operation, key: &str) -> String {
        format!("{op:?}\x00{key}")
    }

    /// How many times this (op, resource-key) has been *committed*
    /// before now. The current request is not counted until `commit`.
    pub fn prior(&self, op: Operation, key: &str) -> u32 {
        self.counts.get(&Self::key(op, key)).copied().unwrap_or(0)
    }

    /// Record one occurrence. Called once per request from the
    /// engine's `commit`, never from `decide`/`refine`.
    pub fn record(&mut self, op: Operation, key: &str) {
        *self.counts.entry(Self::key(op, key)).or_insert(0) += 1;
    }

    /// Drop all counts (e.g. on cwd change, mirroring the old
    /// `set_working_dir` reset).
    pub fn clear(&mut self) {
        self.counts.clear();
    }
}

/// A session allowlist entry: a pattern the user said "allow always"
/// for, scoped to an [`Operation`]. Op-scoping is what dissolves the
/// old write↔edit↔apply_patch mirroring — they share `Operation::Edit`.
#[derive(Debug, Clone)]
pub struct AllowEntry {
    pub op: Operation,
    pub pattern: Pattern,
    pub original: String,
}

/// Session-scoped "allow always" grants. Read by `SessionAllowlistPolicy`,
/// mutated only via [`Self::add`] from the engine's commit path.
#[derive(Debug, Default)]
pub struct SessionAllowlist {
    entries: Vec<AllowEntry>,
}

impl SessionAllowlist {
    pub fn add(&mut self, op: Operation, original: &str, pattern: Pattern) {
        if self
            .entries
            .iter()
            .any(|e| e.op == op && e.original == original)
        {
            return; // dedup
        }
        self.entries.push(AllowEntry {
            op,
            pattern,
            original: original.to_string(),
        });
    }

    /// True if any grant for `op` matches `key`.
    pub fn allows(&self, op: Operation, key: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e.op == op && e.pattern.matches(key))
    }

    pub fn entries(&self) -> impl Iterator<Item = (Operation, &str)> {
        self.entries.iter().map(|e| (e.op, e.original.as_str()))
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn remove_at(&mut self, idx: usize) -> Option<(Operation, String)> {
        if idx >= self.entries.len() {
            return None;
        }
        let e = self.entries.remove(idx);
        Some((e.op, e.original))
    }

    /// Remove every grant matching `(op, original)`. Returns how many
    /// were dropped. Used by `/allow remove <n>`, which identifies the
    /// grant by its (op, original) rather than by engine index (the
    /// display list and this list aren't 1:1 — one display grant maps to
    /// an op-scoped engine entry plus its canonical-path twin).
    pub fn remove(&mut self, op: Operation, original: &str) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|e| !(e.op == op && e.original == original));
        before - self.entries.len()
    }
}

/// All mutable state the policies consult. Owned by the engine; read
/// during `authorize`, written only during `commit`.
#[derive(Debug, Default)]
pub struct PolicyCtx {
    pub repeat: RepeatCounter,
    pub allowlist: SessionAllowlist,
    /// Tools/operations the active prompt's frontmatter denies.
    pub prompt_deny: Vec<String>,
}

/// Stage A. A decider either claims a resource (sets its base effect)
/// or passes. Registered precedence order = the order claims are
/// considered; first claim wins.
pub trait Decider: Send + Sync {
    fn id(&self) -> &'static str;

    /// Whether this decider governs the given (operation, resource).
    /// This is the literal answer to "what rules apply to what action".
    fn applies_to(&self, op: Operation, resource: &Resource) -> bool;

    /// Claim the resource with a verdict, or `None` to pass to the
    /// next decider. `op` is the current claim's operation. Reads
    /// `ctx` but never mutates it.
    fn decide(
        &self,
        req: &AccessRequest,
        op: Operation,
        resource: &Resource,
        ctx: &PolicyCtx,
    ) -> Option<Verdict>;
}

/// Stage B. A modifier may only tighten the running effect for a
/// resource (enforced by [`Refined`]). Order-independent by
/// construction (the fold is `meet`).
pub trait Modifier: Send + Sync {
    fn id(&self) -> &'static str;

    fn applies_to(&self, op: Operation, resource: &Resource) -> bool;

    /// Refine the current effect. Must return [`Refined::tighten`] or
    /// [`Refined::noop`] — it cannot loosen `current`. `op` is the
    /// current claim's operation.
    fn refine(
        &self,
        req: &AccessRequest,
        op: Operation,
        resource: &Resource,
        current: Effect,
        ctx: &PolicyCtx,
    ) -> Refined;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_counter_counts_after_record() {
        let mut c = RepeatCounter::default();
        assert_eq!(c.prior(Operation::Execute, "cargo test"), 0);
        c.record(Operation::Execute, "cargo test");
        assert_eq!(c.prior(Operation::Execute, "cargo test"), 1);
        c.record(Operation::Execute, "cargo test");
        assert_eq!(c.prior(Operation::Execute, "cargo test"), 2);
        // distinct key is independent
        assert_eq!(c.prior(Operation::Execute, "cargo build"), 0);
        // op participates in the key
        assert_eq!(c.prior(Operation::Read, "cargo test"), 0);
        c.clear();
        assert_eq!(c.prior(Operation::Execute, "cargo test"), 0);
    }

    #[test]
    fn session_allowlist_op_scoped_match_and_dedup() {
        let mut al = SessionAllowlist::default();
        al.add(
            Operation::Execute,
            "cargo *",
            Pattern::new_command("cargo *"),
        );
        al.add(
            Operation::Execute,
            "cargo *",
            Pattern::new_command("cargo *"),
        ); // dedup
        assert_eq!(al.entries().count(), 1);
        assert!(al.allows(Operation::Execute, "cargo test --bin dirge"));
        assert!(!al.allows(Operation::Execute, "git status"));
        // op-scoped: an Edit grant doesn't satisfy an Execute query
        al.add(Operation::Edit, "src/**", Pattern::new("src/**"));
        assert!(al.allows(Operation::Edit, "src/main.rs"));
        assert!(!al.allows(Operation::Read, "src/main.rs"));
    }
}
