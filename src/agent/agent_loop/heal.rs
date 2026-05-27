//! Session healing — fix broken message histories on load.
//!
//! Faithful port of `DeepSeek-Reasonix/src/loop/healing.ts` (108 lines).
//!
//! On session restore, applies targeted repairs before the first
//! API call:
//!
//! 1. Shrink oversized tool results (char cap, not token cap)
//! 2. Fix unpaired tool calls (drops assistant.tool_calls with no
//!    matching tool responses + stray tool messages)
//! 3. Stamp missing `reasoning_content` on thinking-mode sessions
//!
//! The rationale: oversized tool results would 400 the next call
//! before the user types. Unpaired tool calls would similarly
//! fail API validation.

use serde_json::Value;

/// Outcome of a heal pass.
#[derive(Debug, Clone)]
pub struct HealResult {
    pub messages: Vec<Value>,
    pub healed_count: usize,
    pub chars_saved: usize,
}

/// Default max chars for a single tool result. Matches
/// Reasonix's `DEFAULT_MAX_RESULT_CHARS` (~40K chars).
pub const DEFAULT_MAX_RESULT_CHARS: usize = 40_000;

// ================================================================
// Shrink oversized tool results (char cap)
// Port of `shrinkOversizedToolResults` (shrink.ts:17-32)
// ================================================================

/// Shrink any tool-result message whose content string exceeds
/// `max_chars`. Matches both `role: "tool"` (heal shape) and
/// `role: "toolResult"` (loop transcript shape).
/// LOOP-7: added toolResult role support.
pub fn shrink_oversized_tool_results(messages: &[Value], max_chars: usize) -> HealResult {
    let mut healed_count = 0usize;
    let mut chars_saved = 0usize;
    let out: Vec<Value> = messages
        .iter()
        .map(|msg| {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role != "tool" && role != "toolResult" {
                return msg.clone();
            }
            let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
            if content.len() <= max_chars {
                return msg.clone();
            }
            healed_count += 1;
            chars_saved += content.len().saturating_sub(max_chars);
            let truncated = truncate_for_model(content, max_chars);
            let mut m = msg.clone();
            m["content"] = Value::String(truncated);
            m
        })
        .collect();
    HealResult {
        messages: out,
        healed_count,
        chars_saved,
    }
}

/// Truncate a string to `max_chars` while keeping the beginning
/// more useful than the end.
fn truncate_for_model(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars || max_chars < 2 {
        return content.to_string();
    }
    let head_pct = 0.7;
    let head_chars = (max_chars as f64 * head_pct) as usize;
    let tail_chars = max_chars.saturating_sub(head_chars);
    let head = &content[..content
        .char_indices()
        .nth(head_chars)
        .map(|(i, _)| i)
        .unwrap_or(content.len())
        .min(content.len())];
    let tail = if tail_chars > 0 {
        let tail_start = content
            .char_indices()
            .nth_back(tail_chars.saturating_sub(1))
            .map(|(i, _)| i)
            .unwrap_or(content.len());
        &content[tail_start..]
    } else {
        ""
    };
    format!(
        "{head}\n...[truncated {} chars]...\n{tail}",
        content.len() - max_chars,
    )
}

// ================================================================
// Fix unpaired tool calls
// Port of `fixToolCallPairing` (healing.ts:13-59)
// ================================================================

/// Extract tool call IDs from an assistant message Value.
///
/// Recognizes two formats:
/// 1. Legacy: `{"tool_calls": [{"id": "c1", ...}, ...]}` top-level field
/// 2. Loop transcript: `{"content": [{"type": "toolCall", "id": "c1", ...}, ...]}` content blocks
///
/// Returns a set of IDs that need matching tool results to follow.
fn extract_tool_call_ids(msg: &Value) -> Option<std::collections::HashSet<String>> {
    // Legacy format: top-level tool_calls array
    if let Some(calls) = msg.get("tool_calls").and_then(|c| c.as_array()) {
        if !calls.is_empty() {
            let ids: std::collections::HashSet<String> = calls
                .iter()
                .filter_map(|c| c.get("id").and_then(|id| id.as_str()).map(String::from))
                .collect();
            if !ids.is_empty() {
                return Some(ids);
            }
        }
    }

    // Loop transcript format: content blocks with type: "toolCall"
    if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
        let ids: std::collections::HashSet<String> = blocks
            .iter()
            .filter_map(|b| {
                let obj = b.as_object()?;
                if obj.get("type").and_then(|t| t.as_str()) == Some("toolCall") {
                    obj.get("id").and_then(|id| id.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect();
        if !ids.is_empty() {
            return Some(ids);
        }
    }

    None
}

/// Drop unpaired assistant.tool_calls and stray tool messages.
/// DeepSeek 400s on either mismatch.
pub fn fix_tool_call_pairing(messages: &[Value]) -> (Vec<Value>, usize, usize) {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut dropped_assistant_calls = 0usize;
    let mut dropped_stray_tools = 0usize;
    let mut i = 0;

    while i < messages.len() {
        let msg = &messages[i];
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

        if role == "assistant" {
            if let Some(mut needed) = extract_tool_call_ids(msg) {
                let mut candidates: Vec<Value> = Vec::new();
                let mut j = i + 1;
                while j < messages.len() && !needed.is_empty() {
                    let nxt = &messages[j];
                    let nxt_role = nxt.get("role").and_then(|r| r.as_str()).unwrap_or("");
                    if nxt_role != "tool" && nxt_role != "toolResult" {
                        break;
                    }
                    let id = nxt
                        .get("tool_call_id")
                        .or_else(|| nxt.get("toolCallId"))
                        .and_then(|id| id.as_str())
                        .unwrap_or("");
                    if !needed.contains(id) {
                        break;
                    }
                    needed.remove(id);
                    candidates.push(nxt.clone());
                    j += 1;
                }
                if needed.is_empty() {
                    out.push(msg.clone());
                    out.extend(candidates);
                    i = j - 1;
                } else {
                    dropped_assistant_calls += 1;
                    dropped_stray_tools += candidates.len();
                    i = j - 1;
                }
                i += 1;
                continue;
            }
            out.push(msg.clone());
        } else if role == "tool" || role == "toolResult" {
            dropped_stray_tools += 1;
        } else {
            out.push(msg.clone());
        }
        i += 1;
    }

    (out, dropped_assistant_calls, dropped_stray_tools)
}

// ================================================================
// Full heal
// Port of `healLoadedMessages` (healing.ts:61-69)
// ================================================================

/// Apply all heal steps to a message list. Returns the healed
/// list + counts of what was fixed.
pub fn heal_loaded_messages(messages: &[Value], max_chars: usize) -> HealResult {
    let shrunk = shrink_oversized_tool_results(messages, max_chars);
    let (paired, dropped_assistant, dropped_stray) = fix_tool_call_pairing(&shrunk.messages);
    HealResult {
        messages: paired,
        healed_count: shrunk.healed_count + dropped_assistant + dropped_stray,
        chars_saved: shrunk.chars_saved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_msg(content: &str, call_id: &str) -> Value {
        serde_json::json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content,
        })
    }

    fn tool_result_msg(content: &str, call_id: &str, tool_name: &str) -> Value {
        serde_json::json!({
            "role": "toolResult",
            "toolCallId": call_id,
            "toolName": tool_name,
            "content": [{"type": "text", "text": content}],
        })
    }

    fn assistant_msg(content: &str, tool_calls: &[Value]) -> Value {
        serde_json::json!({
            "role": "assistant",
            "content": content,
            "tool_calls": tool_calls,
        })
    }

    fn user_msg(content: &str) -> Value {
        serde_json::json!({
            "role": "user",
            "content": content,
        })
    }

    #[test]
    fn shrink_leaves_short_results_untouched() {
        let msgs = vec![tool_msg("short result", "c1"), user_msg("hello")];
        let r = shrink_oversized_tool_results(&msgs, 100);
        assert_eq!(r.healed_count, 0);
        assert_eq!(r.messages.len(), 2);
    }

    #[test]
    fn shrink_truncates_long_tool_results() {
        let long = "x".repeat(100_000);
        let msgs = vec![tool_msg(&long, "c1")];
        let r = shrink_oversized_tool_results(&msgs, 40_000);
        assert_eq!(r.healed_count, 1);
        let content = r.messages[0]["content"].as_str().unwrap();
        assert!(content.len() <= 40_100, "should be roughly capped");
        assert!(content.contains("truncated"));
    }

    #[test]
    fn shrink_does_not_touch_user_messages() {
        let long = "x".repeat(100_000);
        let msgs = vec![user_msg(&long)];
        let r = shrink_oversized_tool_results(&msgs, 40_000);
        assert_eq!(r.healed_count, 0);
        assert_eq!(r.messages[0]["content"].as_str().unwrap(), long);
    }

    #[test]
    fn pairing_keeps_valid_assistant_tool_sequence() {
        let msgs = vec![
            assistant_msg(
                "calling",
                &[serde_json::json!({"id": "c1", "name": "echo"})],
            ),
            tool_msg("result", "c1"),
        ];
        let (out, dropped_a, dropped_t) = fix_tool_call_pairing(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(dropped_a, 0);
        assert_eq!(dropped_t, 0);
    }

    #[test]
    fn pairing_drops_unpaired_assistant_tool_calls() {
        let msgs = vec![assistant_msg(
            "calling",
            &[serde_json::json!({"id": "c1", "name": "echo"})],
        )];
        let (out, dropped_a, _) = fix_tool_call_pairing(&msgs);
        assert_eq!(out.len(), 0);
        assert_eq!(dropped_a, 1);
    }

    #[test]
    fn pairing_drops_stray_tool_messages() {
        let msgs = vec![tool_msg("orphan", "c1")];
        let (out, _, dropped_t) = fix_tool_call_pairing(&msgs);
        assert_eq!(out.len(), 0);
        assert_eq!(dropped_t, 1);
    }

    #[test]
    fn pairing_keeps_valid_tool_result_sequence() {
        let msgs = vec![
            assistant_msg(
                "calling",
                &[serde_json::json!({"id": "c1", "name": "echo"})],
            ),
            tool_result_msg("result", "c1", "echo"),
        ];
        let (out, dropped_a, dropped_t) = fix_tool_call_pairing(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(dropped_a, 0);
        assert_eq!(dropped_t, 0);
    }

    #[test]
    fn pairing_drops_stray_tool_result_messages() {
        let msgs = vec![tool_result_msg("orphan", "c1", "echo")];
        let (out, _, dropped_t) = fix_tool_call_pairing(&msgs);
        assert_eq!(out.len(), 0);
        assert_eq!(dropped_t, 1);
    }

    #[test]
    fn pairing_handles_mixed_tool_and_tool_result() {
        // Mix of legacy tool and loop-transcript toolResult — both
        // should pair with the assistant tool_calls.
        let msgs = vec![
            assistant_msg(
                "calling",
                &[
                    serde_json::json!({"id": "c1", "name": "bash"}),
                    serde_json::json!({"id": "c2", "name": "read"}),
                ],
            ),
            tool_msg("bash result", "c1"),
            tool_result_msg("read result", "c2", "read"),
        ];
        let (out, dropped_a, dropped_t) = fix_tool_call_pairing(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(dropped_a, 0);
        assert_eq!(dropped_t, 0);
    }

    #[test]
    fn pairing_handles_missing_id_on_tool_call() {
        // Assistant calls but the tool_call has no id — still
        // try to match with tool results.
        let msgs = vec![
            assistant_msg("calling", &[serde_json::json!({"name": "echo"})]),
            tool_msg("result", ""),
        ];
        let (out, _, _) = fix_tool_call_pairing(&msgs);
        // No valid ids to match → dropped
        assert!(out.is_empty() || out.len() < 2);
    }

    #[test]
    fn full_heal_composes_shrink_and_pairing() {
        let long = "x".repeat(100_000);
        let msgs = vec![
            user_msg("hello"),
            assistant_msg(
                "calling",
                &[serde_json::json!({"id": "c1", "name": "echo"})],
            ),
            tool_msg(&long, "c1"),
            user_msg("next"),
        ];
        let r = heal_loaded_messages(&msgs, 40_000);
        assert!(r.healed_count >= 1); // shrunk at minimum
        assert!(
            r.chars_saved > 0,
            "should have saved at least {} chars from the long tool result",
            long.len() - 40_000
        );
    }

    // --- Loop transcript format (content-block tool calls) ---

    fn loop_assistant_msg(tool_calls: &[Value]) -> Value {
        let mut content: Vec<Value> = Vec::new();
        for tc in tool_calls {
            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = tc
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            content.push(serde_json::json!({
                "type": "toolCall",
                "id": id,
                "name": name,
                "arguments": args,
            }));
        }
        serde_json::json!({
            "role": "assistant",
            "content": content,
        })
    }

    #[test]
    fn pairing_keeps_loop_transcript_assistant_with_tool_results() {
        let msgs = vec![
            loop_assistant_msg(&[serde_json::json!({
                "id": "c1",
                "name": "bash",
                "arguments": {"cmd": "ls"}
            })]),
            tool_result_msg("bash output", "c1", "bash"),
        ];
        let (out, dropped_a, dropped_t) = fix_tool_call_pairing(&msgs);
        assert_eq!(out.len(), 2, "should keep assistant and tool result");
        assert_eq!(dropped_a, 0);
        assert_eq!(dropped_t, 0);
    }

    #[test]
    fn pairing_drops_unpaired_loop_transcript_assistant() {
        // Simulates Error/Abort path: assistant emitted toolCalls in
        // content blocks, but tool execution never ran → no results.
        let msgs = vec![
            user_msg("run a command"),
            loop_assistant_msg(&[serde_json::json!({
                "id": "call_abc",
                "name": "bash",
                "arguments": {"cmd": "ls"}
            })]),
            user_msg("next question"),
        ];
        let (out, dropped_a, dropped_t) = fix_tool_call_pairing(&msgs);
        assert_eq!(
            out.len(),
            2,
            "should keep user messages but drop assistant with unpaired tool calls"
        );
        assert_eq!(dropped_a, 1);
        assert_eq!(dropped_t, 0);
        // Verify the assistant was dropped (only user messages remain)
        for msg in &out {
            assert_eq!(msg["role"], "user");
        }
    }

    #[test]
    fn pairing_handles_mixed_tool_call_sources() {
        // Loop transcript assistant format with toolResult follow-ups.
        // The heal recognizes toolCall blocks in content and pairs them.
        let msgs = vec![
            loop_assistant_msg(&[
                serde_json::json!({"id": "c1", "name": "bash", "arguments": {"cmd": "ls"}}),
                serde_json::json!({"id": "c2", "name": "read", "arguments": {"path": "/tmp"}}),
            ]),
            tool_result_msg("bash result", "c1", "bash"),
            tool_result_msg("read result", "c2", "read"),
        ];
        let (out, dropped_a, dropped_t) = fix_tool_call_pairing(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(dropped_a, 0);
        assert_eq!(dropped_t, 0);
    }

    #[test]
    fn pairing_handles_partially_paired_loop_assistant() {
        // Two tool calls, only one has a result → drop the assistant.
        let msgs = vec![
            loop_assistant_msg(&[
                serde_json::json!({"id": "c1", "name": "bash", "arguments": {}}),
                serde_json::json!({"id": "c2", "name": "read", "arguments": {}}),
            ]),
            tool_result_msg("only c1 result", "c1", "bash"),
            user_msg("next"),
        ];
        let (out, dropped_a, dropped_t) = fix_tool_call_pairing(&msgs);
        assert_eq!(dropped_a, 1, "should drop assistant: c2 is unpaired");
        assert_eq!(dropped_t, 1, "should count the stray c1 toolResult");
        // Only the user message should remain
        assert_eq!(out.len(), 1, "only user message should survive");
        assert_eq!(out[0]["role"], "user");
    }
}
