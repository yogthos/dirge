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
//! WIRING (LOOP-9): Steps 1-3 (pruning + threshold) execute on every
//! fold. Step 4 fires when `LoopSpawnConfig::summarize_fn` is `Some`
//! (forwarded as the final argument to `run_agent_loop_with_summarizer`
//! / `run_loop_with_summarizer`) and there's still meaningful material
//! to summarize after pruning. The same path runs under the
//! `ExitWithSummary` defense-in-depth branch. Step 5 inserts the
//! summary as a system message at the head of
//! `current_context.messages` with the filter-safe `SUMMARY_PREFIX`.
//! Step 6 (actual `session.id` mutation + `Session::compactions` push +
//! `save_session` persistence) is delegated to the event consumer
//! side via the existing `LoopEvent::ContextCompacted` channel — see
//! the audit note in AUDIT_REPORT.md §8.

use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;

/// Async summarization callback. Receives the fully-built structured
/// prompt (Hermes-style — see `build_summary_prompt`) and returns the
/// summary body produced by the auxiliary model. Callers wire this
/// as a thin "LLM call" closure; the prompt assembly + summary
/// validation live in `run_compaction_pass`.
///
/// `run_agent_loop_with_summarizer` plugs an implementation built
/// from `AnyClient::compress_messages` (or any other one-shot LLM
/// call). `None` disables the LLM pass — the loop falls back to
/// pruning only.
pub type SummarizeFn = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>> + Send + Sync,
>;

/// Filter-safe preamble injected before the summary so the model
/// treats it as reference, not active instructions.
/// Port of Hermes's SUMMARY_PREFIX (context_compressor.py:37-51).
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
const MIN_SUMMARY_TOKENS: u64 = 2000;
const SUMMARY_RATIO: f64 = 0.20;
const SUMMARY_TOKENS_CEILING: u64 = 12_000;

/// Chars-per-token rough estimate. Port of Hermes's _CHARS_PER_TOKEN.
const CHARS_PER_TOKEN: u64 = 4;

/// Hard floor for a compression model's context window (64K).
#[allow(dead_code)]
const MINIMUM_CONTEXT_LENGTH: u64 = 64_000;

/// Default protected head (system prompt + first user/assistant turn)
/// and tail (recent live exchanges) message counts. Port of Hermes
/// `protect_head_size` and `protect_last_n` defaults.
pub const PROTECT_HEAD_DEFAULT: usize = 2;
pub const PROTECT_TAIL_DEFAULT: usize = 5;

// ── Public API ───────────────────────────────────────────

/// Should compression be attempted?
/// True when prompt_tokens exceeds 75% of context_window.
/// Port of Hermes's threshold check.
pub fn should_compress(prompt_tokens: u64, context_window: u64) -> bool {
    let threshold = (0.75 * context_window as f64) as u64;
    prompt_tokens > threshold
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
pub fn build_summary_prompt(
    turns_to_summarize: &[Value],
    summary_budget: u64,
    previous_summary: Option<&str>,
    focus_topic: Option<&str>,
) -> String {
    let _summarizer_preamble = "\
You are a summarization agent creating a context checkpoint. \
Treat the conversation turns below as source material for a \
compact record of prior work. \
Produce only the structured summary; do not add a greeting, \
preamble, or prefix. \
Write the summary in the same language the user was using in the \
conversation — do not translate or switch to English.";

    // /compress <focus> argument. When the caller supplies a focus
    // topic, ask the model to allocate ~60-70% of its budget to
    // content related to that topic. Verbatim port of Hermes's
    // FOCUS TOPIC framing (context_compressor.py:1050-1054). Empty
    // / whitespace-only topics are ignored.
    let focus_block: String = match focus_topic.map(|t| t.trim()).filter(|t| !t.is_empty()) {
        Some(topic) => format!(
            "\n\nFOCUS TOPIC: \"{topic}\"\nThe user has requested that this \
            compaction PRIORITISE preserving all information related to the focus \
            topic above. For content related to \"{topic}\", include full detail — \
            exact values, file paths, command outputs, error messages, and \
            decisions. For content NOT related to the focus topic, summarise more \
            aggressively (brief one-liners or omit if truly irrelevant). The focus \
            topic sections should receive roughly 60-70% of the summary token \
            budget. Even for the focus topic, NEVER preserve API keys, tokens, \
            passwords, or credentials — use [REDACTED]."
        ),
        None => String::new(),
    };

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
NEW TURNS TO INCORPORATE:\n{serialized}{focus_block}\n\n\
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
TURNS TO SUMMARIZE:\n{serialized}{focus_block}\n\n\
Use this exact structure:\n\n\
{_template_sections}"
        )
    }
}

/// Serialize turns for the summarizer prompt. Each turn gets
/// role + content (text fields only, tool results truncated).
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
pub fn summary_budget(compressed_tokens: u64) -> u64 {
    let ratio_budget = (SUMMARY_RATIO * compressed_tokens as f64) as u64;
    ratio_budget.clamp(MIN_SUMMARY_TOKENS, SUMMARY_TOKENS_CEILING)
}

/// Validate that a summary contains the expected sections.
/// At minimum it should mention Active Task and have some structure.
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

/// Replace the middle section of `messages` with a single
/// system-summary message. Returns the new messages list.
/// Port of Hermes `compress` phase 4 (context_compressor.py:1632-1714).
///
/// `compress_start..compress_end` is dropped and replaced with one
/// system message carrying `SUMMARY_PREFIX + summary`. Messages
/// before `compress_start` (protected head — system prompt + first
/// exchange) and at or after `compress_end` (protected tail) are
/// preserved verbatim.
pub fn apply_summary(
    messages: &[Value],
    summary: &str,
    compress_start: usize,
    compress_end: usize,
) -> Vec<Value> {
    let n = messages.len();
    let compress_start = compress_start.min(n);
    let compress_end = compress_end.min(n).max(compress_start);

    let mut out: Vec<Value> =
        Vec::with_capacity(n.saturating_sub(compress_end - compress_start) + 1);
    // Protected head — copy verbatim.
    for msg in messages.iter().take(compress_start) {
        out.push(msg.clone());
    }
    // Summary marker — filter-safe prefix + body.
    let summary_msg = serde_json::json!({
        "role": "system",
        "content": format!("{}{}", SUMMARY_PREFIX, summary),
    });
    out.push(summary_msg);
    // Protected tail — copy verbatim.
    for msg in messages.iter().skip(compress_end) {
        out.push(msg.clone());
    }
    out
}

/// Compute the boundary `(compress_start, compress_end)` for the
/// middle section to summarize. Port of Hermes's
/// `_protect_head_size` + `_find_tail_cut_by_tokens`.
///
/// `protect_head` and `protect_tail` are message counts. Returns
/// `(0, 0)` to signal "nothing to compress" when the message list
/// is too short to safely partition.
pub fn compute_compress_window(
    messages: &[Value],
    protect_head: usize,
    protect_tail: usize,
) -> (usize, usize) {
    let n = messages.len();
    if n < protect_head + protect_tail + 1 {
        return (0, 0);
    }
    let start = protect_head;
    let end = n.saturating_sub(protect_tail);
    if start >= end {
        return (0, 0);
    }
    (start, end)
}

/// Generate a new session id with a `compacted-` prefix to
/// disambiguate from fresh sessions. Port of Hermes's
/// `parent_session_id` rotation pattern (conversation_compression.py:383).
pub fn rotate_session_id() -> String {
    format!(
        "compacted-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .chars()
            .take(8)
            .collect::<String>()
    )
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

    // ── apply_summary / compute_compress_window ─────────

    #[test]
    fn apply_summary_inserts_system_message_with_prefix() {
        let msgs = vec![
            serde_json::json!({"role": "system", "content": "you are an agent"}),
            serde_json::json!({"role": "user", "content": "first user msg"}),
            serde_json::json!({"role": "assistant", "content": "old assistant"}),
            serde_json::json!({"role": "user", "content": "old user"}),
            serde_json::json!({"role": "assistant", "content": "old assistant 2"}),
            serde_json::json!({"role": "user", "content": "recent user"}),
            serde_json::json!({"role": "assistant", "content": "recent assistant"}),
        ];
        let summary = "## Active Task\nfix the bug\n## Remaining Work\nrun tests";
        let out = apply_summary(&msgs, summary, 2, 5);
        // Head preserved (2 messages) + 1 summary + tail (2 messages) = 5.
        assert_eq!(out.len(), 5);
        assert_eq!(out[0]["content"].as_str().unwrap(), "you are an agent");
        assert_eq!(out[1]["content"].as_str().unwrap(), "first user msg");
        // Summary message at index 2.
        assert_eq!(out[2]["role"].as_str().unwrap(), "system");
        let s = out[2]["content"].as_str().unwrap();
        assert!(
            s.starts_with(SUMMARY_PREFIX),
            "summary should start with prefix"
        );
        assert!(s.contains("## Active Task"));
        assert!(s.contains("fix the bug"));
        // Tail.
        assert_eq!(out[3]["content"].as_str().unwrap(), "recent user");
        assert_eq!(out[4]["content"].as_str().unwrap(), "recent assistant");
    }

    #[test]
    fn compute_window_partitions_correctly() {
        let msgs: Vec<Value> = (0..10)
            .map(|i| serde_json::json!({"role": "user", "content": format!("msg {i}")}))
            .collect();
        let (start, end) = compute_compress_window(&msgs, 2, 3);
        assert_eq!(start, 2);
        assert_eq!(end, 7);
    }

    #[test]
    fn compute_window_short_list_returns_zero() {
        let msgs: Vec<Value> = (0..3)
            .map(|i| serde_json::json!({"role": "user", "content": format!("msg {i}")}))
            .collect();
        // 3 messages with head=2, tail=3 — too short.
        assert_eq!(compute_compress_window(&msgs, 2, 3), (0, 0));
    }

    #[test]
    fn rotate_session_id_prefix_and_length() {
        let id = rotate_session_id();
        assert!(id.starts_with("compacted-"));
        // "compacted-" (10) + 8 hex chars = 18.
        assert_eq!(id.len(), 18);
    }

    // ── full-wire integration: prompt → mock summarizer → applied ──

    /// LOOP-9: integration-style test exercising the full compaction
    /// wire end-to-end. Builds a long conversation, calls the prompt
    /// builder, runs a mock summarizer (no LLM), applies the result.
    /// Asserts the summary lands as a system message and the older
    /// turns are gone.
    #[tokio::test]
    async fn full_compaction_wire_with_mock_summarizer() {
        // Build a long conversation: system + 20 turns.
        let mut msgs: Vec<Value> = vec![
            serde_json::json!({"role": "system", "content": "you are an agent"}),
            serde_json::json!({"role": "user", "content": "initial task"}),
        ];
        for i in 0..18 {
            let role = if i % 2 == 0 { "assistant" } else { "user" };
            msgs.push(serde_json::json!({
                "role": role,
                "content": format!("turn {i} content with some length to make tokens"),
            }));
        }
        msgs.push(serde_json::json!({"role": "user", "content": "latest user request"}));

        let n_before = msgs.len();

        // 1. should_compress at the threshold.
        let tokens = estimate_messages_tokens(&msgs);
        // With small messages this is well under 75% — bypass via direct call.
        let _ = tokens;

        // 2. compute window.
        let (start, end) =
            compute_compress_window(&msgs, PROTECT_HEAD_DEFAULT, PROTECT_TAIL_DEFAULT);
        assert!(start < end);
        let middle = &msgs[start..end];
        assert!(!middle.is_empty());

        // 3. build prompt.
        let prompt = build_summary_prompt(
            middle,
            summary_budget(estimate_messages_tokens(middle)),
            None,
            None,
        );
        assert!(prompt.contains("TURNS TO SUMMARIZE"));
        assert!(prompt.contains("turn 0"));

        // 4. mock summarizer — implements SummarizeFn shape.
        let summarizer: SummarizeFn = Arc::new(|_prompt| {
            Box::pin(async move {
                Ok("## Active Task\nlatest user request\n\n\
                    ## Completed Actions\n1. turn 0\n2. turn 1\n\n\
                    ## Remaining Work\nfinish the task"
                    .to_string())
            })
        });
        let summary = summarizer(prompt.clone()).await.expect("summarizer ok");

        // 5. validate.
        assert!(validate_summary(&summary));

        // 6. apply.
        let compressed = apply_summary(&msgs, &summary, start, end);

        // Assertions: head + 1 summary + tail.
        assert_eq!(
            compressed.len(),
            PROTECT_HEAD_DEFAULT + 1 + PROTECT_TAIL_DEFAULT,
            "compressed should be head(2) + summary(1) + tail(5) = 8",
        );
        // Original was much longer.
        assert!(compressed.len() < n_before);
        // The summary message has SUMMARY_PREFIX.
        let summary_msg = &compressed[PROTECT_HEAD_DEFAULT];
        assert_eq!(summary_msg["role"].as_str().unwrap(), "system");
        let body = summary_msg["content"].as_str().unwrap();
        assert!(body.starts_with(SUMMARY_PREFIX));
        assert!(body.contains("## Active Task"));
        // The latest user message is preserved in the tail.
        let last = compressed.last().unwrap();
        assert_eq!(last["content"].as_str().unwrap(), "latest user request");
        // Session id rotates.
        let new_id = rotate_session_id();
        assert!(new_id.starts_with("compacted-"));
    }
}
