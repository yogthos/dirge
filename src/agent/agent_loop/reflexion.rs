//! In-session reflexion memory.
//!
//! A running log of the approaches the agent tried and abandoned this
//! run (because they looped), re-surfaced inside the repeat-loop guard
//! so the model is reminded of *every* dead end it has hit — not just
//! the immediate repeat — and doesn't cycle back to one it already
//! gave up on.
//!
//! This is Reflexion (Shinn et al. 2023) in miniature: verbal
//! reflections kept in an episodic buffer for the lifetime of one run.
//! It extends the existing one-shot reflect-then-pivot guard
//! (`run.rs`) rather than adding a new control path — the storm breaker
//! still decides *when* the agent is stuck; this only accumulates and
//! re-injects *what* it has already abandoned.

/// Episodic buffer of abandoned approaches for one run. Cheap, owned by
/// `run_loop`, persists across the outer (turn) loop so dead ends from
/// earlier turns are still surfaced later.
#[derive(Default)]
pub struct ReflectionLog {
    entries: Vec<String>,
}

impl ReflectionLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an abandoned approach. Deduplicated (the storm guard can
    /// fire repeatedly on the same call); returns `true` when the entry
    /// was newly added.
    pub fn record(&mut self, approach: impl Into<String>) -> bool {
        let approach = approach.into();
        if self.entries.iter().any(|e| e == &approach) {
            return false;
        }
        self.entries.push(approach);
        true
    }

    /// A formatted block listing every abandoned approach, or `None`
    /// when nothing has been recorded yet. Appended to the repeat-loop
    /// guard text.
    pub fn block(&self) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let mut s = String::from(
            "\n\nApproaches already tried and abandoned this run — do not return to any of these:",
        );
        for e in &self.entries {
            s.push_str("\n- ");
            s.push_str(e);
        }
        Some(s)
    }
}

/// A short, stable, UTF-8-safe signature of a tool call for the
/// reflection log: `name(args)` with the argument JSON clipped so a
/// huge payload can't bloat the guard text.
pub fn approach_signature(name: &str, args_json: &str) -> String {
    const MAX_ARG_CHARS: usize = 120;
    let clipped: String = if args_json.chars().count() > MAX_ARG_CHARS {
        let head: String = args_json.chars().take(MAX_ARG_CHARS).collect();
        format!("{head}…")
    } else {
        args_json.to_string()
    };
    format!("{name}({clipped})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_log_has_no_block() {
        let log = ReflectionLog::new();
        assert!(log.block().is_none());
    }

    #[test]
    fn records_and_formats_distinct_approaches() {
        let mut log = ReflectionLog::new();
        assert!(log.record("edit(a.rs)"));
        assert!(log.record("bash(cargo test)"));
        let block = log.block().expect("non-empty");
        assert!(block.contains("abandoned this run"));
        assert!(block.contains("edit(a.rs)"));
        assert!(block.contains("bash(cargo test)"));
    }

    #[test]
    fn dedups_repeated_approach() {
        let mut log = ReflectionLog::new();
        assert!(log.record("edit(a.rs)"));
        assert!(
            !log.record("edit(a.rs)"),
            "second identical record is a no-op"
        );
        // The deduped entry appears exactly once in the block.
        let block = log.block().expect("non-empty");
        assert_eq!(block.matches("edit(a.rs)").count(), 1);
    }

    #[test]
    fn signature_clips_long_args_without_splitting_utf8() {
        let long = "café ".repeat(100); // multi-byte chars, well over the clip
        let sig = approach_signature("edit", &long);
        assert!(sig.starts_with("edit("));
        assert!(
            sig.ends_with("…)"),
            "long args should be clipped with an ellipsis"
        );
        // Must be valid UTF-8 (no panic / no broken char) — building the
        // String above already proves it didn't slice mid-codepoint.
        assert!(sig.chars().count() < long.chars().count());
    }

    #[test]
    fn signature_keeps_short_args_verbatim() {
        let sig = approach_signature("dup", r#"{"k":"v"}"#);
        assert_eq!(sig, r#"dup({"k":"v"})"#);
    }
}
