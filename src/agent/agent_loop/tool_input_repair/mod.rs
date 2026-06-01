//! DeepSeek tool-input repair layer.
//!
//! Validates tool arguments against the JSON Schema, then applies
//! targeted repairs for the four shape failures common with open
//! models. Validate-then-repair semantics: valid inputs are never
//! touched.
//!
//! Phase 1 — repair layer (four shape fixes).
//! Phase 2 — markdown auto-link unwrap (dependent on schema walker).
//! Phase 4 — structured error formatting.
//! Phase 5 — telemetry.

use serde_json::Value;

mod error_fmt;
mod hints;
mod semantic;
mod truncation;
mod validate;
pub use error_fmt::*;
pub use hints::*;
pub use semantic::*;
pub use truncation::*;
pub use validate::*;

/// Kinds of repair applied. Used for telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepairKind {
    NullStripped,
    JsonStringToArray,
    ObjectToArray,
    BareStringToArray,
    MdLinkUnwrapped,
    /// dirge-du5k — unbalanced JSON closed by stack-based brace
    /// closer. Reasonix Pillar 2 pass 3: a model that hits
    /// `max_tokens` mid-tool-call leaves the arg string with
    /// unterminated strings / open braces / open brackets / a
    /// dangling `"key":`. The closer walks the input, tracks the
    /// open stack, and emits the matching closers (plus `null`
    /// for dangling keys, plus a comma trim) so the call is
    /// dispatchable. Hard fallback is `{}` (recorded but flagged
    /// in the result).
    TruncationFixed,
}

// `as_str` and `ALL` are part of the Phase-1 telemetry surface
// (docs/AGENTIC_LOOP_PLAN.md). They're not consumed yet — the
// tracing event emits `repair = ?rr.kinds` via Debug, and the
// aggregate counter has dedicated per-kind fields. Both will be
// used by Phase 1's structured-log consumer / dashboard work.
#[allow(dead_code)]
impl RepairKind {
    /// Stable string name for tracing fields and aggregation keys.
    pub fn as_str(self) -> &'static str {
        match self {
            RepairKind::NullStripped => "null_stripped",
            RepairKind::JsonStringToArray => "json_string_to_array",
            RepairKind::ObjectToArray => "object_to_array",
            RepairKind::BareStringToArray => "bare_string_to_array",
            RepairKind::MdLinkUnwrapped => "md_link_unwrapped",
            RepairKind::TruncationFixed => "truncation_fixed",
        }
    }

    /// All variants in declaration order. Used by `RepairStats` to
    /// iterate the per-kind atomic counters.
    pub const ALL: &'static [RepairKind] = &[
        RepairKind::NullStripped,
        RepairKind::JsonStringToArray,
        RepairKind::ObjectToArray,
        RepairKind::BareStringToArray,
        RepairKind::MdLinkUnwrapped,
        RepairKind::TruncationFixed,
    ];
}

/// Outcome of input repair.
#[derive(Debug, Clone)]
pub struct RepairResult {
    pub repaired: Value,
    pub kinds: Vec<RepairKind>,
    /// Human-readable notes the repair pass wants the model to
    /// see in the tool result. Phase-2: relational defaults
    /// (`offset` auto-set to 0 when only `limit` was supplied)
    /// surface a `"Note: offset defaulted to 0 …"` here. The
    /// tool dispatcher prepends these to the eventual tool
    /// result content so the model sees the augmentation and
    /// adapts subsequent calls.
    pub notes: Vec<String>,
}

/// Per-RepairKind atomic counter. Shared across an agent run via
/// `Arc<RepairStats>` so the run-finish event can emit a single
/// `LoopEvent::RepairStats` snapshot.
///
/// Phase 1 of the agentic-loop plan (docs/AGENTIC_LOOP_PLAN.md):
/// makes repair telemetry aggregable instead of tracing-only. The
/// tracing logs still fire (with `tool` + `model` + `original_args`
/// fields) for the per-call breakdown — the counter is purely the
/// cumulative-per-run number the user sees at session end.
#[derive(Debug, Default)]
pub struct RepairStats {
    null_stripped: std::sync::atomic::AtomicU64,
    json_string_to_array: std::sync::atomic::AtomicU64,
    object_to_array: std::sync::atomic::AtomicU64,
    bare_string_to_array: std::sync::atomic::AtomicU64,
    md_link_unwrapped: std::sync::atomic::AtomicU64,
    /// dirge-du5k — count of brace-closer wins (truncated JSON
    /// that the closer successfully re-parsed).
    truncation_fixed: std::sync::atomic::AtomicU64,
    /// Count of repair attempts that exhausted without success
    /// (tool_input_invalid events). Surfaced alongside per-kind
    /// counts so the rate is visible at the same glance.
    invalid: std::sync::atomic::AtomicU64,
}

impl RepairStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment the counter for a successful repair.
    pub fn record(&self, kind: RepairKind) {
        use std::sync::atomic::Ordering;
        let cell = match kind {
            RepairKind::NullStripped => &self.null_stripped,
            RepairKind::JsonStringToArray => &self.json_string_to_array,
            RepairKind::ObjectToArray => &self.object_to_array,
            RepairKind::BareStringToArray => &self.bare_string_to_array,
            RepairKind::MdLinkUnwrapped => &self.md_link_unwrapped,
            RepairKind::TruncationFixed => &self.truncation_fixed,
        };
        cell.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the invalid-input counter (repair exhausted).
    pub fn record_invalid(&self) {
        self.invalid
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Snapshot the counters into a fixed-shape struct for emission
    /// at AgentEnd. Cheap (5 atomic loads + 1 alloc).
    pub fn snapshot(&self) -> RepairStatsSnapshot {
        use std::sync::atomic::Ordering;
        RepairStatsSnapshot {
            null_stripped: self.null_stripped.load(Ordering::Relaxed),
            json_string_to_array: self.json_string_to_array.load(Ordering::Relaxed),
            object_to_array: self.object_to_array.load(Ordering::Relaxed),
            bare_string_to_array: self.bare_string_to_array.load(Ordering::Relaxed),
            md_link_unwrapped: self.md_link_unwrapped.load(Ordering::Relaxed),
            truncation_fixed: self.truncation_fixed.load(Ordering::Relaxed),
            invalid: self.invalid.load(Ordering::Relaxed),
        }
    }
}

/// Immutable snapshot of `RepairStats` taken at AgentEnd. Used in
/// the `LoopEvent::RepairStats` event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairStatsSnapshot {
    pub null_stripped: u64,
    pub json_string_to_array: u64,
    pub object_to_array: u64,
    pub bare_string_to_array: u64,
    pub md_link_unwrapped: u64,
    pub truncation_fixed: u64,
    pub invalid: u64,
}

impl RepairStatsSnapshot {
    /// Sum of every successful-repair counter.
    pub fn total_successful(&self) -> u64 {
        self.null_stripped
            + self.json_string_to_array
            + self.object_to_array
            + self.bare_string_to_array
            + self.md_link_unwrapped
            + self.truncation_fixed
    }

    /// `true` when every counter is zero. Used by the UI to skip
    /// printing a no-op summary at session end.
    pub fn is_empty(&self) -> bool {
        self.total_successful() == 0 && self.invalid == 0
    }
}

#[cfg(test)]
mod tests;
