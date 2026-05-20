use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrorKind {
    ContextLength,
    RateLimit,
    Network,
    Auth,
    Other,
}

pub struct RecoveryPolicy {
    max_retries: usize,
    backoff_base: Duration,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff_base: Duration::from_secs(1),
        }
    }
}

impl RecoveryPolicy {
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    pub fn should_retry(&self, attempts: usize, kind: ErrorKind) -> bool {
        if attempts >= self.max_retries {
            return false;
        }
        matches!(kind, ErrorKind::Network | ErrorKind::RateLimit)
    }

    pub fn backoff_duration(&self, attempts: usize) -> Duration {
        let exp = 1u64 << attempts.min(6); // cap at 2^6 = 64s
        let base = self.backoff_base.as_millis() as u64;
        let ms = base.saturating_mul(exp);
        // Additive jitter up to +25% so concurrent agents don't retry in
        // lockstep against a rate-limited endpoint. Never shorter than the
        // policy minimum. Seeded from the system clock — pseudo-random is
        // sufficient here.
        let jitter = pseudo_random(attempts as u64) % (ms / 4).max(1);
        Duration::from_millis(ms.saturating_add(jitter))
    }
}

fn pseudo_random(salt: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    // splitmix64 finalizer for decent dispersion
    let mut z = nanos.wrapping_add(salt).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn classify_error(msg: &str) -> ErrorKind {
    let lower = msg.to_lowercase();

    // Auth: HTTP status codes in error context
    if lower.contains(" 401 ")
        || lower.contains(" 403 ")
        || lower.contains("error 401")
        || lower.contains("error 403")
        || lower.starts_with("401 ")
        || lower.starts_with("403 ")
    {
        return ErrorKind::Auth;
    }

    if lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("authentication failed")
    {
        return ErrorKind::Auth;
    }

    if lower.contains("rate limit") || lower.contains("too many requests") {
        return ErrorKind::RateLimit;
    }

    if lower.contains(" 429 ") || lower.contains("error 429") || lower.starts_with("429 ") {
        return ErrorKind::RateLimit;
    }

    // HTTP status codes for server errors (502/503/504 are unambiguous)
    if lower.contains(" 503 ")
        || lower.contains(" 502 ")
        || lower.contains(" 504 ")
        || lower.starts_with("503 ")
        || lower.starts_with("502 ")
        || lower.starts_with("504 ")
    {
        return ErrorKind::Network;
    }

    // Context-length indicators
    if lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("reduce the length of the messages")
        || lower.contains("request too large")
    {
        return ErrorKind::ContextLength;
    }

    // Network errors — check for specific phrases (avoid "connection" false positive)
    if lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("broken pipe")
        || lower.contains("dns error")
        || lower.contains("tls")
        || lower.contains("ssl")
        || lower.contains("timed out")
        || lower.contains("request timeout")
        || lower.contains("server error")
        // Mid-stream decode failures from reqwest/rig — the connection
        // returned bytes but they didn't deserialize into the expected
        // JSON envelope. Almost always transient (network blip,
        // truncated chunked response, provider hiccup), so it should
        // be retried like any other network error rather than surfacing
        // as a hard "Other" failure.
        || lower.contains("error decoding response body")
        || lower.contains("invalid response body")
        || lower.contains("decode error")
    {
        return ErrorKind::Network;
    }

    ErrorKind::Other
}

/// Map a raw error message to a one-line user-facing explanation
/// that names *what* failed and *what to try next*. Used by the agent
/// runner when surfacing errors to the chat — beats dumping a stack
/// of `CompletionError: ProviderError: Http client error: …` at the
/// user.
///
/// The original message is appended in parentheses as the cause so
/// the user (and any bug reports) still have the underlying details.
pub fn user_facing_error(msg: &str, attempts: usize) -> String {
    let kind = classify_error(msg);
    let lower = msg.to_lowercase();

    let (headline, hint) = match kind {
        ErrorKind::Auth => (
            "authentication failed talking to the LLM provider",
            "check your API key env var (e.g. OPENROUTER_API_KEY) and provider config",
        ),
        ErrorKind::RateLimit => (
            "provider rate-limited the request",
            "wait a moment and retry, or switch to a different model via /model",
        ),
        ErrorKind::ContextLength => (
            "conversation exceeds the model's context window",
            "run /compress to summarize older turns and try again",
        ),
        ErrorKind::Network if lower.contains("error decoding response body") => (
            "lost the response stream from the provider (truncated or malformed body)",
            "usually transient — retry. If it persists the provider may be having issues or returning non-JSON (HTML error pages, plaintext)",
        ),
        ErrorKind::Network => (
            "network error reaching the LLM provider",
            "check connectivity / firewall / proxy; the request will retry automatically",
        ),
        ErrorKind::Other => (
            "the LLM provider returned an error we didn't recognize",
            "see the cause below; consider /model to try a different provider",
        ),
    };

    let attempts_note = if attempts > 1 {
        format!(" (after {} attempt(s))", attempts)
    } else {
        String::new()
    };

    format!(
        "{}{}\n  ↳ hint: {}\n  ↳ cause: {}",
        headline, attempts_note, hint, msg
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_context_length() {
        assert_eq!(
            classify_error("context_length_exceeded: prompt too long"),
            ErrorKind::ContextLength
        );
        assert_eq!(
            classify_error("reduce the length of the messages"),
            ErrorKind::ContextLength
        );
        assert_eq!(
            classify_error("request too large for model"),
            ErrorKind::ContextLength
        );
    }

    #[test]
    fn test_classify_network() {
        assert_eq!(classify_error("connection refused"), ErrorKind::Network);
        assert_eq!(
            classify_error("connection reset by peer"),
            ErrorKind::Network
        );
        assert_eq!(classify_error("request timed out"), ErrorKind::Network);
        assert_eq!(
            classify_error("503 service unavailable"),
            ErrorKind::Network
        );
        // Reqwest decode failure mid-stream — rig surfaces it as
        // `CompletionError: ProviderError: Http client error: error
        // decoding response body`. Should be retried like any other
        // transient network blip rather than surfacing as Other.
        assert_eq!(
            classify_error(
                "CompletionError: ProviderError: Http client error: error decoding response body"
            ),
            ErrorKind::Network
        );
        assert_eq!(classify_error("decode error: EOF"), ErrorKind::Network);
    }

    /// `user_facing_error` produces a multi-line message with headline,
    /// hint, and cause. The cause must contain the original raw
    /// message so debug context isn't lost.
    #[test]
    fn user_facing_error_includes_cause() {
        let raw = "CompletionError: ProviderError: Http client error: error decoding response body";
        let pretty = user_facing_error(raw, 1);
        assert!(pretty.contains("lost the response stream"));
        assert!(pretty.contains("hint:"));
        assert!(pretty.contains("cause:"));
        assert!(pretty.contains(raw));
    }

    /// Auth errors get a distinct headline pointing at the API key.
    #[test]
    fn user_facing_error_classifies_auth() {
        let pretty = user_facing_error("401 unauthorized", 1);
        assert!(pretty.contains("authentication failed"));
        assert!(pretty.contains("API key"));
    }

    /// Context-length errors point at /compress.
    #[test]
    fn user_facing_error_classifies_context_length() {
        let pretty = user_facing_error("maximum context length exceeded", 1);
        assert!(pretty.contains("/compress"));
    }

    #[test]
    fn test_classify_rate_limit() {
        assert_eq!(classify_error("rate limit exceeded"), ErrorKind::RateLimit);
        assert_eq!(
            classify_error("429 too many requests"),
            ErrorKind::RateLimit
        );
    }

    #[test]
    fn test_classify_auth() {
        assert_eq!(classify_error("401 unauthorized"), ErrorKind::Auth);
        assert_eq!(classify_error("invalid api key"), ErrorKind::Auth);
    }

    #[test]
    fn test_classify_other() {
        assert_eq!(classify_error("something else"), ErrorKind::Other);
        assert_eq!(classify_error("file not found"), ErrorKind::Other);
        // "connection" alone should not trigger network
        assert_eq!(
            classify_error("database connection closed"),
            ErrorKind::Other
        );
        // "reset" alone should not trigger
        assert_eq!(classify_error("form reset successful"), ErrorKind::Other);
        // "500" in non-HTTP context should not trigger
        assert_eq!(classify_error("processed 500 items"), ErrorKind::Other);
    }

    #[test]
    fn test_retry_policy() {
        let policy = RecoveryPolicy::default();

        // Network errors are retryable
        assert!(policy.should_retry(0, ErrorKind::Network));
        assert!(policy.should_retry(1, ErrorKind::Network));
        assert!(policy.should_retry(2, ErrorKind::Network));
        assert!(!policy.should_retry(3, ErrorKind::Network));

        // Rate limits are retryable
        assert!(policy.should_retry(0, ErrorKind::RateLimit));

        // Context length is NOT retryable (needs compaction)
        assert!(!policy.should_retry(0, ErrorKind::ContextLength));

        // Auth is not retryable
        assert!(!policy.should_retry(0, ErrorKind::Auth));

        // Other is not retryable
        assert!(!policy.should_retry(0, ErrorKind::Other));
    }

    #[test]
    fn test_backoff_duration() {
        let policy = RecoveryPolicy::default();
        let d0 = policy.backoff_duration(0);
        let d1 = policy.backoff_duration(1);
        let d2 = policy.backoff_duration(2);

        assert!(d0 >= Duration::from_secs(1));
        assert!(d1 >= Duration::from_secs(2));
        assert!(d2 >= Duration::from_secs(4));
    }

    #[test]
    fn test_backoff_overflow_guard() {
        let policy = RecoveryPolicy::default();
        let d = policy.backoff_duration(20); // capped at attempts=6 via min()
        // 1s * 2^6 = 64s plus up to +25% jitter = 80s ceiling
        assert!(d >= Duration::from_secs(64));
        assert!(d < Duration::from_secs(81));
    }

    #[test]
    fn test_backoff_jitter_present() {
        let policy = RecoveryPolicy::default();
        // Repeated calls at the same attempt count should yield differing values
        // most of the time. Run a small batch and confirm we see at least two
        // distinct values — proves jitter is wired in.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            seen.insert(policy.backoff_duration(3));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            seen.len() > 1,
            "expected jittered backoff to vary across calls"
        );
    }
}
