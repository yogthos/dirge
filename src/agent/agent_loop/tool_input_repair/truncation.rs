//! JSON truncation repair. Split out of
//! `agent/agent_loop/tool_input_repair.rs` (dirge-4y4l stage 10a).
//!
//! Fixes the failure mode where a model hits `max_tokens` mid-tool-call,
//! leaving the streamed-and-accumulated argument string unterminated
//! (open string, dangling key, open brace, trailing comma). Pure
//! string-level repair — no dependency on the JSON Schema or the
//! validate/repair machinery.

use serde_json::Value;

/// dirge-du5k — outcome of [`repair_truncated_json`]. Port of
/// Reasonix `TruncationRepairResult` (repair/truncation.ts:3-9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationRepairResult {
    /// The repaired JSON string. Always parseable as JSON when
    /// `fallback` is `false`. Equals `"{}"` when `fallback`.
    pub repaired: String,
    /// `true` when the repair actually changed the input.
    pub changed: bool,
    /// Human-readable notes describing each step (closed string,
    /// trimmed trailing comma, popped brace, etc.). Surfaced to
    /// the model so it adapts subsequent calls.
    pub notes: Vec<String>,
    /// `true` when every repair attempt failed and the result is
    /// the hard-fallback `"{}"`. The original args are lost; the
    /// caller should surface this to the model as a tool error.
    pub fallback: bool,
}

/// Stack-based JSON brace / bracket / string closer. Port of
/// Reasonix `repair/truncation.ts:repairTruncatedJson` (lines
/// 11-100). Fixes the specific failure mode where a model hits
/// `max_tokens` mid-tool-call and the streamed-and-accumulated
/// arg string is left unterminated (open string, dangling key,
/// open brace, trailing comma).
///
/// Walks the input once tracking an open-stack of `{ / [ / "`.
/// At EOF emits the matching closers in reverse order, after
/// trimming a trailing comma and filling a dangling `"key":`
/// with `null`. Returns the original input unchanged on a
/// fast-path parseable check.
///
/// Hard fallback is `"{}"` recorded as `fallback: true`.
pub fn repair_truncated_json(input: &str) -> TruncationRepairResult {
    if input.trim().is_empty() {
        let changed = input != "{}";
        return TruncationRepairResult {
            repaired: "{}".to_string(),
            changed,
            notes: if changed {
                vec!["empty input → {}".to_string()]
            } else {
                Vec::new()
            },
            fallback: false,
        };
    }
    // Fast path: already parseable.
    if serde_json::from_str::<Value>(input).is_ok() {
        return TruncationRepairResult {
            repaired: input.to_string(),
            changed: false,
            notes: Vec::new(),
            fallback: false,
        };
    }

    // Stack tracks open `{ / [ / "` — `"` is included so the
    // EOF-flush path can close an unterminated string.
    let mut stack: Vec<char> = Vec::new();
    let mut escaped = false;
    let mut in_string = false;
    let mut last_significant: Option<usize> = None;

    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if !c.is_whitespace() {
            last_significant = Some(i);
        }
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if in_string {
            if c == '\\' {
                escaped = true;
                i += 1;
                continue;
            }
            if c == '"' {
                in_string = false;
                if matches!(stack.last(), Some('"')) {
                    stack.pop();
                }
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            stack.push('"');
        } else if c == '{' || c == '[' {
            stack.push(c);
        } else if c == '}' || c == ']' {
            // Pop only when the top matches — a stray closer
            // without a matching open is left untouched (parse
            // will reject it; the fallback covers that case).
            if let Some(&top) = stack.last() {
                let matches = (top == '{' && c == '}') || (top == '[' && c == ']');
                if matches {
                    stack.pop();
                }
            }
        }
        i += 1;
    }

    let mut notes = Vec::new();
    let cut = last_significant.map(|i| i + 1).unwrap_or(input.len());
    let mut s = input[..cut].to_string();

    // Trim a trailing comma which would block re-parse.
    if s.ends_with(',') {
        s.pop();
        notes.push("trimmed trailing comma".to_string());
    }

    // If we ended on a dangling key `"foo":`, fill with `null`
    // so the value parses. Match the trailing pattern by
    // walking back over whitespace and looking for `":`.
    if ends_with_dangling_key(&s) {
        s.push_str(" null");
        notes.push("filled dangling key with null".to_string());
    }

    // Close an unterminated string.
    if in_string {
        s.push('"');
        if matches!(stack.last(), Some('"')) {
            stack.pop();
        }
        notes.push("closed unterminated string".to_string());
    }

    // Pop remaining open structures in reverse order.
    while let Some(top) = stack.pop() {
        match top {
            '{' => s.push('}'),
            '[' => s.push(']'),
            '"' => s.push('"'),
            _ => {}
        }
    }

    if serde_json::from_str::<Value>(&s).is_ok() {
        return TruncationRepairResult {
            repaired: s.clone(),
            changed: s != input,
            notes,
            fallback: false,
        };
    }

    // Closer exhausted — hard fallback to `{}`. Preserve a
    // bounded preview of the input so the operator can audit.
    const PREVIEW_CAP: usize = 500;
    let preview = if input.len() <= PREVIEW_CAP {
        input.to_string()
    } else {
        let mut cap = PREVIEW_CAP;
        while !input.is_char_boundary(cap) && cap > 0 {
            cap -= 1;
        }
        format!("{} …[+{} chars]", &input[..cap], input.len() - cap)
    };
    notes.push("fallback to {}".to_string());
    notes.push(format!(
        "unrecoverable truncation — original args preview: {}",
        preview
    ));
    TruncationRepairResult {
        repaired: "{}".to_string(),
        changed: true,
        notes,
        fallback: true,
    }
}

/// Does the trimmed string end with a key followed by `:` and
/// no value yet? `"\"foo\":"` or `"\"foo\" :\t"` etc.
fn ends_with_dangling_key(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = bytes.len();
    while i > 0 && (bytes[i - 1] as char).is_whitespace() {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b':' {
        return false;
    }
    i -= 1;
    while i > 0 && (bytes[i - 1] as char).is_whitespace() {
        i -= 1;
    }
    i > 0 && bytes[i - 1] == b'"'
}
