//! Per-turn streaming-token batching for the `on-message-update`
//! plugin hook.
//!
//! Dispatching Janet code on every streamed token would tank
//! throughput — a single LLM response can be hundreds of tokens.
//! `TokenBatcher` collects tokens since the last flush and surfaces
//! them in batches once a threshold is reached. The host fires
//! `on-message-update` with the accumulated text each time the
//! batcher yields a flush.
//!
//! Batching is token-count based rather than time based so the
//! behavior is deterministic and unit-testable without mocking
//! `Instant`. The threshold defaults to [`DEFAULT_BATCH_TOKENS`]; it's
//! chosen so an `on-message-update` fires roughly once per readable
//! sentence rather than per linguistic token.

/// Threshold token count at which `TokenBatcher` yields a flush.
/// Tuned for typical LLM streaming (per-word or per-sub-word tokens):
/// ~16 tokens is roughly half a sentence, granular enough to be
/// useful for live observability but coarse enough that Janet
/// dispatch overhead stays well below 1% of run time.
pub const DEFAULT_BATCH_TOKENS: usize = 16;

/// Accumulates streamed tokens and yields the full accumulated text
/// when the batch threshold is crossed. Calls between flushes do not
/// drop content — every byte the agent emits is returned by either a
/// `push` or `flush_remaining`.
#[derive(Debug)]
pub struct TokenBatcher {
    buffer: String,
    n_tokens: usize,
    threshold: usize,
}

impl Default for TokenBatcher {
    fn default() -> Self {
        Self::with_threshold(DEFAULT_BATCH_TOKENS)
    }
}

impl TokenBatcher {
    pub fn with_threshold(threshold: usize) -> Self {
        Self {
            buffer: String::new(),
            n_tokens: 0,
            threshold: threshold.max(1),
        }
    }

    /// Append one token. Returns `Some(accumulated_text)` once the
    /// threshold is crossed — the buffer is drained as part of the
    /// flush. Returns `None` otherwise; the caller doesn't need to
    /// remember partial state.
    pub fn push(&mut self, token: &str) -> Option<String> {
        self.buffer.push_str(token);
        self.n_tokens += 1;
        if self.n_tokens >= self.threshold {
            self.n_tokens = 0;
            Some(std::mem::take(&mut self.buffer))
        } else {
            None
        }
    }

    /// Drain any unflushed content. Called when the turn ends so the
    /// trailing tokens (between the last threshold flush and the end)
    /// still reach `on-message-update`. Returns `None` when the
    /// buffer is already empty so callers can skip a no-op dispatch.
    pub fn flush_remaining(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            None
        } else {
            self.n_tokens = 0;
            Some(std::mem::take(&mut self.buffer))
        }
    }

    /// Reset the batcher to its initial empty state. Used at
    /// `TurnStart` so the new turn's tokens accumulate from zero.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.n_tokens = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A batcher with the default threshold returns None until the
    /// threshold is reached, then yields the full accumulated text.
    #[test]
    fn push_buffers_until_threshold_then_yields() {
        let mut b = TokenBatcher::with_threshold(3);
        assert_eq!(b.push("a"), None);
        assert_eq!(b.push("b"), None);
        // Third token crosses the threshold.
        assert_eq!(b.push("c"), Some("abc".to_string()));
    }

    /// After a flush, the next push starts a fresh batch.
    #[test]
    fn flush_starts_a_new_batch() {
        let mut b = TokenBatcher::with_threshold(2);
        assert_eq!(b.push("x"), None);
        assert_eq!(b.push("y"), Some("xy".to_string()));
        // Buffer drained; the next push doesn't carry "xy" forward.
        assert_eq!(b.push("z"), None);
        assert_eq!(b.push("w"), Some("zw".to_string()));
    }

    /// `flush_remaining` returns the trailing partial batch when the
    /// turn ends mid-batch.
    #[test]
    fn flush_remaining_drains_partial_batch() {
        let mut b = TokenBatcher::with_threshold(4);
        b.push("a");
        b.push("b");
        // Below threshold — no implicit flush yet.
        assert_eq!(b.flush_remaining(), Some("ab".to_string()));
        // Now empty; flush_remaining returns None to spare callers a
        // pointless dispatch.
        assert_eq!(b.flush_remaining(), None);
    }

    /// `reset` wipes both the buffer and the token counter so a new
    /// turn doesn't carry over partial content from the previous one.
    #[test]
    fn reset_clears_buffer_and_counter() {
        let mut b = TokenBatcher::with_threshold(3);
        b.push("a");
        b.push("b");
        b.reset();
        assert_eq!(b.flush_remaining(), None);
        // After reset, count restarts from zero.
        assert_eq!(b.push("x"), None);
        assert_eq!(b.push("y"), None);
        assert_eq!(b.push("z"), Some("xyz".to_string()));
    }

    /// A threshold of zero is clamped to one to avoid div-by-zero
    /// surprises; every push then yields immediately.
    #[test]
    fn zero_threshold_clamps_to_one() {
        let mut b = TokenBatcher::with_threshold(0);
        // Every push crosses the (clamped) threshold of 1.
        assert_eq!(b.push("a"), Some("a".to_string()));
        assert_eq!(b.push("b"), Some("b".to_string()));
    }

    /// No content is ever lost — the concatenation of all push +
    /// flush_remaining outputs equals the concatenated inputs.
    #[test]
    fn no_content_is_dropped_across_a_run() {
        let mut b = TokenBatcher::with_threshold(3);
        let tokens = ["The ", "quick ", "brown ", "fox ", "jumps ", "over"];
        let mut collected = String::new();
        for t in &tokens {
            if let Some(batch) = b.push(t) {
                collected.push_str(&batch);
            }
        }
        if let Some(tail) = b.flush_remaining() {
            collected.push_str(&tail);
        }
        assert_eq!(collected, "The quick brown fox jumps over");
    }
}
