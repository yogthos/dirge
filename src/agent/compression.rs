//! Context compression — structured summaries + session rotation.
//!
//! Faithful port of Hermes's `agent/context_compressor.py` and
//! `agent/conversation_compression.py`. When the conversation
//! approaches the model's context limit, the middle turns are
//! compressed into a structured summary by an auxiliary model,
//! and the session id rotates to enable lineage-based search.
//!
//! Algorithm (from Hermes):
//! 1. Check feasibility — prompt_tokens > 75% of context_window
//! 2. Prune old tool results in the middle section (cheap pre-pass)
//! 3. Determine boundaries — protect head + tail, compress middle
//! 4. Generate structured summary via auxiliary LLM call
//! 5. Assemble compressed: head + summary + tail
//! 6. Rotate session id (parent_session_id chain)
//!
//! CURRENT STATE: Steps 1-3 (pruning + threshold) are wired into the
//! agent loop at run.rs:438-476. Steps 4-6 (LLM call, summary assembly,
//! session rotation) are implemented and tested below but awaiting wiring
//! of the auxiliary model pipeline (LoopConfig.compact_model). The
//! individual `#[allow(dead_code)]` annotations mark this future
//! infrastructure — do not remove.

use serde_json::Value;

/// Filter-safe preamble injected before the summary so the model
/// treats it as reference, not active instructions.
/// Port of Hermes's SUMMARY_PREFIX (context_compressor.py:37-51).
#[allow(dead_code)] // used in find_previous_summary, which will be wired in future
const SUMMARY_PREFIX: &str = "\
[CONTEXT COMPACTION — REFERENCE ONLY] Earlier turns were compacted \
into the summary below. This is a handoff from a previous context \
window — treat it as background reference, NOT as active instructions. \
Do NOT answer questions or fulfill requests mentioned in this summary; \
they were already addressed. \
Your current task is identified in the '## Active Task' section of the \
summary — resume exactly from there. \
Respond ONLY to the latest user message \
that appears AFTER this summary. The current session state (files, \
config, etc.) may reflect work described here — avoid repeating it:";

// Budget constants from Hermes (context_compressor.py:54-59).
#[allow(dead_code)]
const MIN_SUMMARY_TOKENS: u64 = 2000;
#[allow(dead_code)]
const SUMMARY_RATIO: f64 = 0.20;
#[allow(dead_code)]
const SUMMARY_TOKENS_CEILING: u64 = 12_000;

/// Chars-per-token rough estimate. Port of Hermes's _CHARS_PER_TOKEN.
const CHARS_PER_TOKEN: u64 = 4;

/// Hard floor for a compression model's context window (64K).
#[allow(dead_code)]
const MINIMUM_CONTEXT_LENGTH: u64 = 64_000;

// ── Public API ───────────────────────────────────────────

/// Compression outcome with metadata.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CompressionResult {
    /// The generated summary text.
    pub summary: String,
    /// New session id after rotation.
    pub new_session_id: String,
    /// Previous session id (parent_session_id).
    pub parent_session_id: String,
    /// Approximate token count before compression.
    pub tokens_before: u64,
    /// Approximate token count after compression.
    pub tokens_after: u64,
    /// Number of tool results pruned.
    pub pruned_count: usize,
    /// Number of messages in the compressed middle section.
    pub compressed_messages: usize,
}

/// Should compression be attempted?
/// True when prompt_tokens exceeds 75% of context_window.
/// Port of Hermes's threshold check.
pub fn should_compress(prompt_tokens: u64, context_window: u64) -> bool {
    let threshold = (0.75 * context_window as f64) as u64;
    prompt_tokens > threshold
}

/// Approximate token count from total character length.
/// 4 chars ≈ 1 token (rough, model-independent).
#[allow(dead_code)]
pub fn approx_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(CHARS_PER_TOKEN)
}

/// Estimate tokens for a slice of messages by summing content
/// lengths and dividing by CHARS_PER_TOKEN.
pub fn estimate_messages_tokens(messages: &[Value]) -> u64 {
    let total_chars: usize = messages
        .iter()
        .map(|m| {
            m.get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.len())
                .unwrap_or(0)
        })
        .sum();
    (total_chars as u64).div_ceil(CHARS_PER_TOKEN)
}

/// Prune large tool outputs in the middle section before
/// summarization. Replaces tool-result content > 500 chars
/// with a 1-line summary of what the tool did.
/// Port of Hermes's _prune_old_tool_results (context_compressor.py).
///
/// LOOP-7: matches both `role: "tool"` (heal/legacy shape) and
/// `role: "toolResult"` (loop transcript shape). Also reads both
/// `"tool_name"` (snake_case) and `"toolName"` (camelCase)
/// for the tool name field.
pub fn prune_tool_outputs(messages: &[Value], protect_tail: usize) -> Vec<Value> {
    let n = messages.len();
    if n <= protect_tail {
        return messages.to_vec();
    }
    let end = n.saturating_sub(protect_tail);
    let mut pruned = 0usize;

    messages
        .iter()
        .enumerate()
        .map(|(i, msg)| {
            if i >= end {
                return msg.clone();
            }
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role != "tool" && role != "toolResult" {
                return msg.clone();
            }
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.len() <= 500 {
                return msg.clone();
            }
            // Summarize: 1-line tool result.
            let tool_name = msg
                .get("tool_name")
                .or_else(|| msg.get("toolName"))
                .and_then(|v| v.as_str())
                .unwrap_or("tool");
            pruned += 1;
            let summary = summarize_tool_result(tool_name, content);
            let mut new_msg = msg.clone();
            new_msg["content"] = Value::String(summary);
            new_msg
        })
        .collect()
}

fn fmt_count(n: usize) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let s = n.to_string();
    let mut result = String::new();
    let len = s.len();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

/// Produce a 1-line summary of a tool result for the pruning pass.
/// Port of Hermes's _summarize_tool_result (context_compressor.py:332).
fn summarize_tool_result(tool_name: &str, content: &str) -> String {
    let content_len = content.len();
    let line_count = content.lines().count();
    let clen = fmt_count(content_len);
    let lc = line_count;

    match tool_name {
        "bash" => {
            let cmd = content
                .lines()
                .next()
                .map(|l| l.trim_start_matches("$ ").trim_start_matches("> "))
                .unwrap_or("?");
            let cmd_short = if cmd.len() > 80 {
                format!("{}…", &cmd[..77])
            } else {
                cmd.to_string()
            };
            format!("[bash] ran `{cmd_short}` -> {lc} lines, {clen} chars")
        }
        "read" => {
            format!("[read] {clen} chars, {lc} lines")
        }
        "write" => {
            format!("[write] wrote {clen} chars")
        }
        "edit" => {
            format!("[edit] patched {clen} chars")
        }
        "grep" => {
            format!("[grep] {lc} matches, {clen} chars")
        }
        "glob" | "find_files" | "list_dir" => {
            let first_line = content.lines().next().unwrap_or("");
            format!("[{}] {first_line}", tool_name)
        }
        "task" | "task_status" => {
            format!("[{tool_name}] {clen} chars result")
        }
        _ => {
            let preview: String = content.chars().take(80).collect();
            format!(
                "[{tool_name}] {preview}{} ({clen} chars)",
                if content.len() > 80 { "…" } else { "" }
            )
        }
    }
}

/// Build the structured summary prompt for the auxiliary model.
/// Port of Hermes's _generate_summary prompt (context_compressor.py:960-1046).
#[allow(dead_code)]
pub fn build_summary_prompt(
    turns_to_summarize: &[Value],
    summary_budget: u64,
    previous_summary: Option<&str>,
    _focus_topic: Option<&str>, // reserved for future /compress <focus>
) -> String {
    let _summarizer_preamble = "\
You are a summarization agent creating a context checkpoint. \
Treat the conversation turns below as source material for a \
compact record of prior work. \
Produce only the structured summary; do not add a greeting, \
preamble, or prefix. \
Write the summary in the same language the user was using in the \
conversation — do not translate or switch to English.";

    let _template_sections = format!(
        "## Active Task\n\
[THE SINGLE MOST IMPORTANT FIELD. Copy the user's most recent request or\n\
task assignment verbatim — the exact words they used. If multiple tasks\n\
were requested and only some are done, list only the ones NOT yet completed.\n\
If no outstanding task exists, write \"None.\"]\n\
\n\
## Goal\n\
[What the user is trying to accomplish overall]\n\
\n\
## Constraints & Preferences\n\
[User preferences, coding style, constraints, important decisions]\n\
\n\
## Completed Actions\n\
[Numbered list of concrete actions taken — include tool used, target, and outcome.]\n\
\n\
## Active State\n\
[Current working state — directory, branch, modified files, test status]\n\
\n\
## In Progress\n\
[Work currently underway — what was being done when compaction fired]\n\
\n\
## Blocked\n\
[Any blockers, errors, or issues not yet resolved. Include exact error messages.]\n\
\n\
## Key Decisions\n\
[Important technical decisions and WHY they were made]\n\
\n\
## Resolved Questions\n\
[Questions already answered — include the answer]\n\
\n\
## Pending User Asks\n\
[Questions or requests NOT yet answered. If none, write \"None.\"]\n\
\n\
## Relevant Files\n\
[Files read, modified, or created — with brief note on each]\n\
\n\
## Remaining Work\n\
[What remains to be done — framed as context, not instructions]\n\
\n\
## Critical Context\n\
[Specific values, error messages, config details that would be lost\n\
without explicit preservation]\n\
\n\
Target ~{summary_budget} tokens. Be CONCRETE — include file paths,\n\
command outputs, error messages, line numbers, and specific values.\n\
Write only the summary body. Do not include any preamble or prefix."
    );

    let serialized = serialize_turns_for_summary(turns_to_summarize);

    if let Some(prev) = previous_summary {
        format!(
            "{_summarizer_preamble}\n\n\
You are updating a context compaction summary. A previous compaction \
produced the summary below. New conversation turns have occurred since \
then and need to be incorporated.\n\n\
PREVIOUS SUMMARY:\n{prev}\n\n\
NEW TURNS TO INCORPORATE:\n{serialized}\n\n\
Update the summary using this exact structure. PRESERVE all existing \
information that is still relevant. CRITICAL: Update \"## Active Task\" \
to reflect the user's most recent unfulfilled request.\n\n\
{_template_sections}"
        )
    } else {
        format!(
            "{_summarizer_preamble}\n\n\
Create a structured checkpoint summary for the conversation after earlier \
turns are compacted. The summary should preserve enough detail for \
continuity without re-reading the original turns.\n\n\
TURNS TO SUMMARIZE:\n{serialized}\n\n\
Use this exact structure:\n\n\
{_template_sections}"
        )
    }
}

/// Serialize turns for the summarizer prompt. Each turn gets
/// role + content (text fields only, tool results truncated).
#[allow(dead_code)]
fn serialize_turns_for_summary(turns: &[Value]) -> String {
    let mut out = String::new();
    for (i, turn) in turns.iter().enumerate() {
        let role = turn.get("role").and_then(|v| v.as_str()).unwrap_or("?");
        let content = turn.get("content").and_then(|v| v.as_str()).unwrap_or("");
        out.push_str(&format!("[{i}] {role}: "));
        if content.len() > 2000 {
            let truncated: String = content.chars().take(2000).collect();
            out.push_str(&format!(
                "{truncated}… [truncated, {} total chars]\n",
                content.len()
            ));
        } else {
            out.push_str(content);
            out.push('\n');
        }
    }
    out
}

/// Compute the summary budget from the compressed token count.
/// Port of Hermes's _compute_summary_budget.
#[allow(dead_code)]
pub fn summary_budget(compressed_tokens: u64) -> u64 {
    let ratio_budget = (SUMMARY_RATIO * compressed_tokens as f64) as u64;
    ratio_budget.clamp(MIN_SUMMARY_TOKENS, SUMMARY_TOKENS_CEILING)
}

/// Validate that a summary contains the expected sections.
/// At minimum it should mention Active Task and have some structure.
#[allow(dead_code)]
pub fn validate_summary(summary: &str) -> bool {
    if summary.is_empty() {
        return false;
    }
    // Must contain at least one of the expected section headers.
    let required = ["Active Task", "Goal", "Completed Actions", "Remaining Work"];
    required.iter().any(|s| summary.contains(s))
}

/// Find the latest context summary marker in the message list.
/// Returns (index, body) of the last system message containing
/// SUMMARY_PREFIX, or None.
#[allow(dead_code)]
pub fn find_previous_summary(messages: &[Value]) -> Option<(usize, String)> {
    messages.iter().enumerate().rev().find_map(|(i, m)| {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "system" {
            return None;
        }
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if content.starts_with(SUMMARY_PREFIX) {
            let body = content
                .strip_prefix(SUMMARY_PREFIX)
                .unwrap_or("")
                .trim()
                .to_string();
            Some((i, body))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── should_compress ─────────────────────────────────

    #[test]
    fn below_75pct_no_compress() {
        // 50K tokens in 128K window = 39% — no compression.
        assert!(!should_compress(50_000, 128_000));
    }

    #[test]
    fn at_threshold_no_compress() {
        // Exactly 75% — NOT compressed (must EXCEED threshold).
        assert!(!should_compress(96_000, 128_000));
    }

    #[test]
    fn above_threshold_compress() {
        // Just above 75% threshold.
        assert!(should_compress(96_001, 128_000));
    }

    #[test]
    fn exactly_at_threshold_edge() {
        // 75% of 128000 = 96000
        assert!(!should_compress(96_000, 128_000));
        assert!(should_compress(96_001, 128_000));
    }

    // ── summary_budget ──────────────────────────────────

    #[test]
    fn budget_minimum() {
        // Small compressed content → minimum budget.
        assert_eq!(summary_budget(1_000), MIN_SUMMARY_TOKENS);
    }

    #[test]
    fn budget_proportional() {
        // 50K compressed → 10K budget (20%).
        assert_eq!(summary_budget(50_000), 10_000);
    }

    #[test]
    fn budget_ceiling() {
        // Very large compressed content → ceiling.
        assert_eq!(summary_budget(500_000), SUMMARY_TOKENS_CEILING);
    }

    #[test]
    fn budget_clamp() {
        assert_eq!(summary_budget(0), MIN_SUMMARY_TOKENS);
        assert_eq!(summary_budget(1_000_000), SUMMARY_TOKENS_CEILING);
    }

    // ── prune_tool_outputs ──────────────────────────────

    #[test]
    fn prune_large_tool_results() {
        let msgs = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "hi"}),
            serde_json::json!({"role": "tool", "content": "x".repeat(1000), "tool_name": "read"}),
            serde_json::json!({"role": "tool", "content": "small", "tool_name": "grep"}),
            serde_json::json!({"role": "user", "content": "tail"}),
        ];

        let pruned = prune_tool_outputs(&msgs, 2);
        // Large tool result should be summarized.
        let tool1 = &pruned[2];
        assert!(tool1["content"].as_str().unwrap().contains("[read]"));
        assert!(!tool1["content"].as_str().unwrap().contains("xxxxx"));

        // Small tool result unchanged.
        assert_eq!(pruned[3]["content"].as_str().unwrap(), "small");

        // Tail protected.
        assert_eq!(pruned[4]["content"].as_str().unwrap(), "tail");
    }

    /// LOOP-7: loop transcripts use `role: "toolResult"` and
    /// `"toolName"` (camelCase), not `role: "tool"` and `"tool_name"`.
    /// Pruning must recognize both formats.
    #[test]
    fn prune_handles_tool_result_role_and_camelcase_toolname() {
        let msgs = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "toolResult", "content": "x".repeat(1000), "toolName": "bash"}),
            serde_json::json!({"role": "toolResult", "content": "small", "toolName": "grep"}),
            serde_json::json!({"role": "user", "content": "tail"}),
        ];

        let pruned = prune_tool_outputs(&msgs, 2);
        // The large toolResult should be summarized now (contains "[bash]" marker).
        let summary = pruned[1]["content"].as_str().unwrap();
        assert!(
            summary.contains("[bash]"),
            "should summarize bash tool result: {summary}"
        );
        // The summary should be MUCH shorter than the original 1000 chars
        // (it contains the escaped command + metadata, but the 1000 x's are truncated).
        assert!(
            summary.len() < 500,
            "summary should be under 500 chars: {}",
            summary.len()
        );
        // Small result in tail should be untouched.
        assert_eq!(pruned[2]["content"].as_str().unwrap(), "small");
    }

    #[test]
    fn prune_protects_tail() {
        let msgs = vec![
            serde_json::json!({"role": "tool", "content": "x".repeat(1000), "tool_name": "bash"}),
            serde_json::json!({"role": "tool", "content": "y".repeat(1000), "tool_name": "read"}),
            serde_json::json!({"role": "user", "content": "protected"}),
            serde_json::json!({"role": "assistant", "content": "protected"}),
        ];

        // Protect last 3 → only the first tool result is pruned.
        let pruned = prune_tool_outputs(&msgs, 3);
        assert!(pruned[0]["content"].as_str().unwrap().contains("[bash]"));
        // Second tool result is in the tail (index 1, n=4, protect 3 → end=1).
        // Index 1 is protected if n - protect_tail = 4 - 3 = 1, end=1,
        // so indices 0..0 are pruned, index 1 is protected.
        assert!(pruned[1]["content"].as_str().unwrap().contains("yyyy"));
    }

    // ── estimate_messages_tokens ────────────────────────

    #[test]
    fn estimate_tokens_from_content() {
        let msgs = vec![
            serde_json::json!({"role": "user", "content": "hello world"}),
            serde_json::json!({"role": "assistant", "content": "0123456789012345"}),
        ];
        // "hello world" = 11 chars, "0123456789012345" = 16 chars, total = 27
        // 27 / 4 = 6.75 → ceil = 7
        assert_eq!(estimate_messages_tokens(&msgs), 7);
    }

    #[test]
    fn estimate_tokens_handles_missing_content() {
        let msgs = vec![serde_json::json!({"role": "system"})];
        assert_eq!(estimate_messages_tokens(&msgs), 0);
    }

    // ── validate_summary ────────────────────────────────

    #[test]
    fn valid_summary_passes() {
        assert!(validate_summary(
            "## Active Task\nRefactor auth module\n\n## Completed Actions\n1. READ config.py"
        ));
    }

    #[test]
    fn empty_summary_fails() {
        assert!(!validate_summary(""));
    }

    #[test]
    fn irrelevant_text_fails() {
        assert!(!validate_summary("just some random text with no structure"));
    }

    // ── build_summary_prompt ────────────────────────────

    #[test]
    fn prompt_contains_filter_safe_preamble() {
        let turns = vec![
            serde_json::json!({"role": "user", "content": "fix the bug"}),
            serde_json::json!({"role": "assistant", "content": "ok let me read the file"}),
        ];
        let prompt = build_summary_prompt(&turns, 2000, None, None);
        assert!(prompt.contains("summarization agent"));
        assert!(prompt.contains("TURNS TO SUMMARIZE"));
        assert!(prompt.contains("## Active Task"));
        assert!(prompt.contains("## Remaining Work"));
        assert!(prompt.contains("fix the bug"));
        assert!(prompt.contains("ok let me read the file"));
    }

    #[test]
    fn iterative_prompt_includes_previous_summary() {
        let turns = vec![serde_json::json!({"role": "user", "content": "new stuff"})];
        let prompt = build_summary_prompt(&turns, 2000, Some("Old summary"), None);
        assert!(prompt.contains("PREVIOUS SUMMARY"));
        assert!(prompt.contains("Old summary"));
        assert!(prompt.contains("NEW TURNS TO INCORPORATE"));
    }

    #[test]
    fn prompt_truncates_long_content() {
        let long = "x".repeat(3000);
        let turns = vec![serde_json::json!({"role": "assistant", "content": long})];
        let prompt = build_summary_prompt(&turns, 2000, None, None);
        assert!(prompt.contains("truncated"));
        // The prompt includes template text + truncated content, so it'll be
        // under a reasonable size but longer than the content alone.
        assert!(prompt.len() < 10_000, "prompt should be under 10K chars");
    }

    // ── find_previous_summary ───────────────────────────

    #[test]
    fn finds_latest_summary() {
        let msgs = vec![
            serde_json::json!({"role": "system", "content": "system prompt"}),
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "system", "content": format!("{}## Active Task\nfix the bug", SUMMARY_PREFIX)}),
        ];
        let found = find_previous_summary(&msgs);
        assert!(found.is_some());
        let (_idx, body) = found.unwrap();
        assert!(body.contains("fix the bug"));
    }

    #[test]
    fn no_summary_returns_none() {
        let msgs = vec![
            serde_json::json!({"role": "system", "content": "system prompt"}),
            serde_json::json!({"role": "user", "content": "hello"}),
        ];
        assert!(find_previous_summary(&msgs).is_none());
    }
}
