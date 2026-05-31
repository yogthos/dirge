//! UTF-8-safe string truncation helpers (dirge-fb8t).
//!
//! Many display / error / telemetry sites cut a string at a fixed BYTE
//! offset (`&s[..n]`). That panics — "byte index N is not a char
//! boundary" — the instant a multibyte codepoint (CJK, emoji, an accented
//! filename) straddles the cut, and the input is routinely
//! model/user/path-controlled. These helpers floor/ceil the cut to a char
//! boundary and never panic.

/// Largest PREFIX of `s` that fits in `max_bytes` and ends on a char
/// boundary. Returns all of `s` when it's already within budget. Never
/// panics.
pub(crate) fn head(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Largest SUFFIX of `s` that fits in `max_bytes` and starts on a char
/// boundary. Returns all of `s` when it's already within budget. Never
/// panics. Used for path truncation where the tail (basename) matters.
pub(crate) fn tail(s: &str, max_bytes: usize) -> &str {
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_ascii_within_and_over_budget() {
        assert_eq!(head("hello", 10), "hello");
        assert_eq!(head("hello", 3), "hel");
        assert_eq!(head("hello", 0), "");
    }

    #[test]
    fn head_never_splits_a_multibyte_char() {
        // "é" is 2 bytes; cutting at byte 1 would split it.
        let s = "café"; // c a f é  -> bytes: 1 1 1 2 = 5
        assert_eq!(head(s, 4), "caf"); // byte 4 is mid-'é' -> floor to 3
        assert_eq!(head(s, 5), "café");
        // CJK (3 bytes each) and emoji (4 bytes) crossing the cut.
        let cjk = "日本語"; // 9 bytes
        assert_eq!(head(cjk, 4), "日"); // floor 4 -> 3
        assert_eq!(head(cjk, 3), "日");
        let emoji = "a😀b"; // 1 + 4 + 1
        assert_eq!(head(emoji, 3), "a"); // floor 3 -> 1 (😀 is bytes 1..5)
        assert_eq!(head(emoji, 5), "a😀");
    }

    /// The exact reported panic shape: a 3-byte char straddling a fixed
    /// byte cut must not panic.
    #[test]
    fn head_handles_multibyte_straddling_a_large_cut() {
        let mut s = "a".repeat(199);
        s.push('世'); // 3 bytes spanning offsets 199..202
        s.push_str(&"b".repeat(50));
        let cut = head(&s, 200); // byte 200 is mid-'世'
        assert!(s.starts_with(cut));
        assert_eq!(cut.len(), 199, "floored below the multibyte char");
    }

    #[test]
    fn tail_never_splits_a_multibyte_char() {
        let s = "café"; // bytes: c a f é(2) -> len 5
        assert_eq!(tail(s, 2), "é"); // 'é' is the last 2 bytes
        assert_eq!(tail(s, 3), "fé"); // start 2 is on a boundary ('f')
        assert_eq!(tail(s, 10), "café");
        let cjk = "日本語";
        assert_eq!(tail(cjk, 4), "語"); // last 4 bytes start mid-'本' -> ceil to '語'
        assert_eq!(tail(cjk, 0), "");
    }
}
