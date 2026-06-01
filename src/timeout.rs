//! Shared deadline / timeout primitives (dirge-onlr).
//!
//! Two things live here:
//!
//! * [`Deadline`] — an elapsed-aware budget for a *whole* operation,
//!   including any internal retries. Generalizes the per-call budget
//!   that previously lived inline in the MCP tool as a `remaining_budget`
//!   free function. An operation that re-wraps each retry attempt in a
//!   fresh `tokio::time::timeout` can take up to 2× its nominal limit; a
//!   `Deadline` shared across attempts caps the *total* wait instead.
//!
//! * [`Timeouts`] — the single source of truth for dirge's named
//!   per-operation timeout defaults. These were previously five-plus
//!   magic-number `const`s scattered across config, the stream loop, the
//!   MCP client, and the LSP manager. Config overrides merge onto
//!   [`Timeouts::DEFAULT`] via `Config::resolve_timeouts`.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Elapsed-aware budget for an operation and all of its retries.
///
/// Holds a start instant and a total budget; `remaining()` reports how
/// much of the budget is left right now, saturating at zero so it can be
/// fed straight into `tokio::time::timeout` (which then fires
/// immediately and surfaces the exhausted state).
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    start: Instant,
    budget: Duration,
}

impl Deadline {
    /// Start a budget of `budget`, counting from now.
    pub fn start(budget: Duration) -> Self {
        Self {
            start: Instant::now(),
            budget,
        }
    }

    /// Construct from an explicit start instant. Lets tests control
    /// elapsed time without sleeping.
    #[cfg(test)]
    pub fn from_start(start: Instant, budget: Duration) -> Self {
        Self { start, budget }
    }

    /// The total budget this deadline was created with.
    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// Time left before the deadline, measured against `now`. Saturates
    /// at [`Duration::ZERO`] (never negative) once the deadline passes.
    pub fn remaining_at(&self, now: Instant) -> Duration {
        self.budget
            .saturating_sub(now.saturating_duration_since(self.start))
    }

    /// Time left before the deadline, against the current clock.
    pub fn remaining(&self) -> Duration {
        self.remaining_at(Instant::now())
    }

    /// Whether the budget is spent (no time remaining).
    pub fn is_expired(&self) -> bool {
        self.remaining().is_zero()
    }
}

/// Named per-operation timeout defaults — the single definition for
/// values that used to be magic-number `const`s in several modules.
///
/// Field semantics are documented per-field; the `DEFAULT_*_SECS`
/// associated constants are the raw second values so a call site that
/// only needs the number (e.g. a JSON schema description, or a `u64`
/// default) can reference one source.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Per-chunk read deadline for a streaming LLM response, applied to
    /// every `stream.next().await`. A finite timeout exists because
    /// `reqwest`'s streaming doesn't detect silently-dropped TCP
    /// connections (no RST/FIN — reads block forever); it converts that
    /// into a retryable `Network` error for the `spawn_agent` retry loop.
    /// 5 minutes (not the original 120s, which was too aggressive for
    /// reasoning-heavy models that routinely produce 2-4 minute chunk
    /// gaps). Bump via `stream_chunk_timeout_secs` for longer budgets.
    pub stream_chunk: Duration,
    /// Stall window while a tool call is mid-assembly in the stream.
    pub tool_call_gap: Duration,
    /// Total budget for one MCP tool call, including reconnect + retry.
    pub mcp_call: Duration,
    /// MCP server `initialize` handshake.
    pub mcp_init: Duration,
    /// Any non-`initialize` LSP request.
    pub lsp_request: Duration,
    /// LSP `initialize` handshake.
    pub lsp_initialize: Duration,
    /// Default `bash` tool command timeout (when the call omits one).
    pub bash: Duration,
}

impl Timeouts {
    pub const DEFAULT_STREAM_CHUNK_SECS: u64 = 300;
    pub const DEFAULT_TOOL_CALL_GAP_SECS: u64 = 30;
    pub const DEFAULT_MCP_CALL_SECS: u64 = 120;
    pub const DEFAULT_MCP_INIT_SECS: u64 = 10;
    pub const DEFAULT_LSP_REQUEST_SECS: u64 = 30;
    pub const DEFAULT_LSP_INITIALIZE_SECS: u64 = 45;
    pub const DEFAULT_BASH_SECS: u64 = 120;

    /// The built-in defaults. `const` so a call site that still wants a
    /// plain `Duration` constant can read one field
    /// (`Timeouts::DEFAULT.lsp_request`) instead of redefining the value.
    pub const DEFAULT: Timeouts = Timeouts {
        stream_chunk: Duration::from_secs(Self::DEFAULT_STREAM_CHUNK_SECS),
        tool_call_gap: Duration::from_secs(Self::DEFAULT_TOOL_CALL_GAP_SECS),
        mcp_call: Duration::from_secs(Self::DEFAULT_MCP_CALL_SECS),
        mcp_init: Duration::from_secs(Self::DEFAULT_MCP_INIT_SECS),
        lsp_request: Duration::from_secs(Self::DEFAULT_LSP_REQUEST_SECS),
        lsp_initialize: Duration::from_secs(Self::DEFAULT_LSP_INITIALIZE_SECS),
        bash: Duration::from_secs(Self::DEFAULT_BASH_SECS),
    };

    /// Install the process-wide resolved timeouts. Call once at startup
    /// after config load (`Config::resolve_timeouts`). Subsequent calls
    /// are ignored — same OnceLock-set-once convention as `ui::theme::init`
    /// (dirge-4xgd). Threading the resolved struct through every subsystem
    /// constructor would mean signature churn across LSP/MCP/bash/stream;
    /// a startup-installed global is the lighter, idiomatic fit for
    /// process-wide config that's read but never mutated after load.
    pub fn init(resolved: Timeouts) {
        let _ = RESOLVED.set(resolved);
    }

    /// The process-wide resolved timeouts, or [`Timeouts::DEFAULT`] when
    /// `init` hasn't run (unit tests, early startup). Every consumer reads
    /// its timeout through this so a `[timeouts]` config override applies
    /// everywhere from one place.
    pub fn get() -> Timeouts {
        RESOLVED.get().copied().unwrap_or(Self::DEFAULT)
    }
}

/// Process-wide resolved timeouts, installed once at startup by
/// [`Timeouts::init`]. Read via [`Timeouts::get`].
static RESOLVED: OnceLock<Timeouts> = OnceLock::new();

impl Default for Timeouts {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_reports_remaining() {
        let start = Instant::now();
        let d = Deadline::from_start(start, Duration::from_secs(120));
        assert_eq!(d.remaining_at(start), Duration::from_secs(120));
        assert_eq!(
            d.remaining_at(start + Duration::from_secs(50)),
            Duration::from_secs(70),
        );
        assert_eq!(d.budget(), Duration::from_secs(120));
    }

    #[test]
    fn deadline_saturates_at_zero_past_the_budget() {
        let start = Instant::now();
        let d = Deadline::from_start(start, Duration::from_secs(10));
        // Past the deadline → zero, never an underflow/negative.
        assert_eq!(
            d.remaining_at(start + Duration::from_secs(25)),
            Duration::ZERO,
        );
    }

    #[test]
    fn zero_budget_is_immediately_expired() {
        // A spent budget feeds Duration::ZERO into tokio::time::timeout,
        // which fires immediately — the budget-exhausted path.
        assert!(Deadline::start(Duration::ZERO).is_expired());
    }

    #[test]
    fn fresh_budget_is_not_expired() {
        assert!(!Deadline::start(Duration::from_secs(60)).is_expired());
    }

    #[test]
    fn defaults_match_documented_values() {
        let t = Timeouts::DEFAULT;
        assert_eq!(t.stream_chunk, Duration::from_secs(300));
        assert_eq!(t.tool_call_gap, Duration::from_secs(30));
        assert_eq!(t.mcp_call, Duration::from_secs(120));
        assert_eq!(t.mcp_init, Duration::from_secs(10));
        assert_eq!(t.lsp_request, Duration::from_secs(30));
        assert_eq!(t.lsp_initialize, Duration::from_secs(45));
        assert_eq!(t.bash, Duration::from_secs(120));
        // Default impl agrees with the DEFAULT const.
        assert_eq!(Timeouts::default().mcp_call, t.mcp_call);
    }
}
