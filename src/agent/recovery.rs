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

    /// F14: combine `backoff_duration` with the provider's
    /// requested `Retry-After`. Prefer whichever is longer (since
    /// retrying earlier than the server asked just earns another
    /// 429), but cap at 5 minutes so a misformatted header can't
    /// stall the agent forever.
    pub fn backoff_duration_for_msg(&self, attempts: usize, error_msg: &str) -> Duration {
        let computed = self.backoff_duration(attempts);
        match retry_after_from_error_msg(error_msg) {
            Some(server_wants) => {
                const CAP: Duration = Duration::from_secs(300);
                let chosen = server_wants.max(computed);
                if chosen > CAP { CAP } else { chosen }
            }
            None => computed,
        }
    }
}

/// Parse a `Retry-After` value out of an error message. Looks for
/// (in order):
/// 1. Anthropic-style `retry-after-ms: <N>` — milliseconds.
/// 2. Standard `Retry-After: <N>` — seconds.
/// 3. JSON body `"retry_after": <N>` — seconds.
///
/// Returns `None` if no recognized form is present. Robust to the
/// `:` being absent (some providers emit `retry-after 30`).
pub(crate) fn retry_after_from_error_msg(msg: &str) -> Option<Duration> {
    fn parse_after_label(msg: &str, label: &str) -> Option<u64> {
        // Case-insensitive search WITHOUT lowercasing the whole
        // message: previously we lowercased `msg` and then indexed
        // into the ORIGINAL `msg` at the lowered string's byte
        // offset. For ASCII that's identical, but `to_lowercase`
        // can change byte length for some unicode (e.g. Turkish
        // `İ` → `i̇` is 2 → 3 bytes). The mismatched offset could
        // land mid-UTF-8 and panic on `&msg[...]`. Now we scan the
        // original bytes window-by-window with case-insensitive
        // ASCII comparison. The label itself is fixed-ASCII so this
        // is sound — we just need to be case-insensitive against
        // the message's casing.
        let label_bytes = label.as_bytes();
        let msg_bytes = msg.as_bytes();
        if msg_bytes.len() < label_bytes.len() {
            return None;
        }
        let mut idx = None;
        for i in 0..=msg_bytes.len() - label_bytes.len() {
            let window = &msg_bytes[i..i + label_bytes.len()];
            if window
                .iter()
                .zip(label_bytes.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
            {
                idx = Some(i);
                break;
            }
        }
        let idx = idx?;
        // `idx` is now a byte offset into the original `msg`.
        // Land at a char boundary (the ASCII label match guarantees
        // we're on a boundary, but `idx + label.len()` could still
        // hit one — for ASCII labels it can't, but defend anyway).
        let after = idx + label.len();
        if !msg.is_char_boundary(after) {
            return None;
        }
        let tail = &msg[after..];
        let tail = tail.trim_start_matches([':', ' ', '\t', '"']).trim_start();
        // Consume contiguous digits, with a hard cap so a malformed
        // header (`Retry-After: 999999999999999999999`) doesn't
        // produce a parsed integer that overflows or is absurdly
        // large before the 5-min cap applies in the caller. Cap at
        // 10^10 — any value larger is clearly bogus, and the cap
        // saturates rather than overflowing u64.
        let n: String = tail
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .take(11)
            .collect();
        if n.is_empty() {
            return None;
        }
        n.parse().ok()
    }

    if let Some(ms) = parse_after_label(msg, "retry-after-ms") {
        return Some(Duration::from_millis(ms));
    }
    if let Some(secs) = parse_after_label(msg, "retry-after") {
        return Some(Duration::from_secs(secs));
    }
    if let Some(secs) = parse_after_label(msg, "retry_after") {
        return Some(Duration::from_secs(secs));
    }
    None
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

    // Anthropic's `overloaded_error` is a transient capacity signal —
    // structurally a rate-limit response without the "rate limit" /
    // "too many" wording. Classify as RateLimit so the retry-with-
    // backoff policy applies; previously it fell through to `Other`
    // and the user saw a one-shot failure on transient backend
    // pressure.
    if lower.contains("overloaded") {
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

    /// Anthropic returns `{"type": "overloaded_error", ...}` when its
    /// service is at capacity. The body is structurally similar to a
    /// rate-limit (transient + retryable) but doesn't contain the
    /// "rate limit" / "too many" / "429" patterns. Without explicit
    /// handling it falls into `Other` and dirge doesn't retry —
    /// users see a one-shot failure on a transient backend issue.
    #[test]
    fn classify_anthropic_overloaded_error_as_retryable() {
        assert_eq!(
            classify_error("overloaded_error: Anthropic API is overloaded"),
            ErrorKind::RateLimit,
        );
        // Just the lowercase token is enough — provider stringifies
        // the structured error differently across rig versions.
        assert_eq!(
            classify_error("Provider overloaded; please retry later"),
            ErrorKind::RateLimit,
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

    /// F14: Anthropic-style `retry-after-ms` parses as ms.
    #[test]
    fn retry_after_parses_anthropic_ms() {
        let msg = "rate limited: retry-after-ms: 5000";
        assert_eq!(
            retry_after_from_error_msg(msg),
            Some(Duration::from_millis(5000)),
        );
    }

    /// Standard HTTP `Retry-After: <seconds>` parses as seconds.
    #[test]
    fn retry_after_parses_standard_seconds() {
        let msg = "HTTP 429 Too Many Requests\nRetry-After: 30";
        assert_eq!(
            retry_after_from_error_msg(msg),
            Some(Duration::from_secs(30)),
        );
    }

    /// JSON body form: `"retry_after": 12`.
    #[test]
    fn retry_after_parses_json_body() {
        let msg = r#"{"error":"rate_limit","retry_after":12}"#;
        assert_eq!(
            retry_after_from_error_msg(msg),
            Some(Duration::from_secs(12)),
        );
    }

    /// Bare-without-colon variant (some proxies log `retry-after 30`).
    #[test]
    fn retry_after_parses_no_colon() {
        let msg = "got 429, retry-after 7 next time";
        assert_eq!(
            retry_after_from_error_msg(msg),
            Some(Duration::from_secs(7)),
        );
    }

    /// No retry-after present → None.
    #[test]
    fn retry_after_returns_none_when_absent() {
        let msg = "generic network error: connection reset";
        assert_eq!(retry_after_from_error_msg(msg), None);
    }

    /// Regression: messages with multi-byte UTF-8 BEFORE the label
    /// previously could panic — the original parser found the
    /// label in a lowercased copy and indexed into the original
    /// at that byte offset. `to_lowercase` can change byte length
    /// (Turkish `İ` is 2 bytes lowercase as `i̇` = 3 bytes), so
    /// the offsets disagreed and `&msg[idx + label.len()..]` could
    /// land mid-UTF-8 → panic. Now the search is on byte windows
    /// of the original string with case-insensitive ASCII compare.
    #[test]
    fn retry_after_handles_unicode_before_label() {
        // Provider error message with a Turkish capital I before
        // the label. Lowercasing produces a different byte length.
        let msg = "İoError: Retry-After: 8";
        assert_eq!(
            retry_after_from_error_msg(msg),
            Some(Duration::from_secs(8)),
        );
    }

    /// Case-insensitive matching against the label name itself.
    /// `RETRY-AFTER-MS` and `retry-after-ms` should both parse.
    #[test]
    fn retry_after_label_match_is_case_insensitive() {
        assert_eq!(
            retry_after_from_error_msg("rate limited: RETRY-AFTER-MS: 750"),
            Some(Duration::from_millis(750)),
        );
        assert_eq!(
            retry_after_from_error_msg("Retry-After-Ms: 750"),
            Some(Duration::from_millis(750)),
        );
    }

    /// Pathological huge digit run: cap at 11 digits before parse,
    /// so `Retry-After: 999999999999999999999...` doesn't overflow
    /// or produce a 100-year wait before the upper cap clamps.
    #[test]
    fn retry_after_caps_pathological_digit_run() {
        let msg = "Retry-After: 99999999999999999999999";
        let parsed = retry_after_from_error_msg(msg);
        // 11 digits = max ~10^11 seconds — `backoff_duration_for_msg`
        // will cap at 5 minutes, but the unsanitized parse must
        // produce SOMETHING (not None, not a panic). We don't pin
        // the exact value; just verify it's bounded by the cap
        // behavior in `backoff_duration_for_msg`.
        assert!(parsed.is_some(), "must parse, not return None");
        let policy = RecoveryPolicy::default();
        let d = policy.backoff_duration_for_msg(0, msg);
        assert!(
            d <= Duration::from_secs(300),
            "backoff must cap at 5min; got {:?}",
            d,
        );
    }

    /// `backoff_duration_for_msg` picks the longer of the
    /// computed exponential backoff and the server's retry-after,
    /// capped at 5 minutes.
    #[test]
    fn backoff_duration_for_msg_prefers_longer_value() {
        let policy = RecoveryPolicy::default();
        // attempts=0 → ~1s computed. retry-after=10s → 10s wins.
        let d = policy.backoff_duration_for_msg(0, "Retry-After: 10");
        assert!(d >= Duration::from_secs(10) && d < Duration::from_secs(11));

        // Server asks for ms below computed → computed wins.
        let d = policy.backoff_duration_for_msg(3, "retry-after-ms: 50");
        // 2^3 = 8s computed.
        assert!(d >= Duration::from_secs(8));
    }

    /// Cap retry-after at 5 minutes in case the header is bogus.
    #[test]
    fn backoff_duration_for_msg_caps_at_5_minutes() {
        let policy = RecoveryPolicy::default();
        let d = policy.backoff_duration_for_msg(0, "Retry-After: 9999");
        assert!(d <= Duration::from_secs(300));
    }
}
