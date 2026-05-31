#[cfg(feature = "lsp")]
use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::agent_loop::tool_input_repair::with_contract_hint;
use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, EditArgs, PermCheck, ToolError, check_perm_path_resolve};
#[cfg(feature = "lsp")]
use crate::lsp::manager::LspManager;

pub struct EditTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    cache: Option<ToolCache>,
    /// When set, the tool touches the edited file on the LSP server and
    /// appends any diagnostic block to its output. `None` reproduces the
    /// pre-LSP behaviour.
    #[cfg(feature = "lsp")]
    lsp_manager: Option<Arc<LspManager>>,
}

impl EditTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        EditTool {
            permission,
            ask_tx,
            cache: None,
            #[cfg(feature = "lsp")]
            lsp_manager: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        cache: ToolCache,
        #[cfg(feature = "lsp")] lsp_manager: Option<Arc<LspManager>>,
    ) -> Self {
        EditTool {
            permission,
            ask_tx,
            cache: Some(cache),
            #[cfg(feature = "lsp")]
            lsp_manager,
        }
    }

    pub(crate) fn show_diff(
        path: &str,
        content: &str,
        byte_pos: usize,
        old_text: &str,
        new_text: &str,
    ) -> String {
        let lines: Vec<&str> = content.lines().collect();
        let old_line_count = old_text.lines().count();
        let new_line_count = new_text.lines().count();
        let ctx: usize = 3;

        let match_line = content[..byte_pos].matches('\n').count();
        let start = match_line.saturating_sub(ctx);
        let ctx_after_start = (match_line + old_line_count).min(lines.len());
        let ctx_after_end = (ctx_after_start + ctx).min(lines.len());

        let ctx_before = match_line - start;
        let ctx_after = ctx_after_end - ctx_after_start;

        let mut result = format!("\n--- a/{}\n+++ b/{}\n", path, path);
        result.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n",
            old_start = start + 1,
            old_count = ctx_before + old_line_count + ctx_after,
            new_start = start + 1,
            new_count = ctx_before + new_line_count + ctx_after,
        ));

        for i in start..match_line {
            if let Some(line) = lines.get(i) {
                result.push_str(&format!(" {}\n", line));
            }
        }
        for line in old_text.lines() {
            result.push_str(&format!("-{}\n", line));
        }
        for line in new_text.lines() {
            result.push_str(&format!("+{}\n", line));
        }
        for i in ctx_after_start..ctx_after_end {
            if let Some(line) = lines.get(i) {
                result.push_str(&format!(" {}\n", line));
            }
        }

        result
    }
}

impl Tool for EditTool {
    const NAME: &'static str = "edit";

    type Error = ToolError;
    type Args = EditArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "edit".to_string(),
            description: with_contract_hint(
                "edit",
                "Edit a file by replacing exact text. If old_text appears once, replaces it. If it appears multiple times and replace_all is false, returns all match locations with line numbers. Use replaceAll: true to replace every occurrence. Handles both LF and CRLF line endings.",
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The absolute path to the file to edit (must be absolute, not relative)" },
                    "old_text": { "type": "string", "description": "Exact text to find and replace" },
                    "new_text": { "type": "string", "description": "New text to replace with" },
                    "replace_all": { "type": "boolean", "description": "Replace all occurrences instead of just the first" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }

    async fn call(&self, args: EditArgs) -> Result<String, ToolError> {
        if args.old_text.is_empty() {
            return Err(ToolError::Msg(
                "old_text must not be empty. Provide the exact text to replace.".to_string(),
            ));
        }

        // Reject non-absolute paths immediately with a clear error
        // (shared guard; the schema requires an absolute path).
        crate::agent::tools::require_absolute_path(&args.path, "the edit path")
            .map_err(ToolError::Msg)?;
        // Audit H12: pin file operations to the canonical path the
        // permission check resolved.
        let resolved_path =
            check_perm_path_resolve(&self.permission, &self.ask_tx, "edit", &args.path).await?;

        // Pre-check size before reading. The edit tool isn't meant
        // for huge generated artifacts; cap at 100 MiB so an LLM
        // pointing it at a gigabyte log file fails fast rather
        // than OOM-ing the process. Matches the apply_patch cap.
        const MAX_EDIT_BYTES: u64 = 100 * 1024 * 1024;
        if let Ok(meta) = tokio::fs::metadata(&resolved_path).await
            && meta.len() > MAX_EDIT_BYTES
        {
            return Err(ToolError::Msg(format!(
                "file too large for edit: {} bytes (cap {} bytes); use bash with sed/awk for huge files",
                meta.len(),
                MAX_EDIT_BYTES,
            )));
        }
        let bytes = tokio::fs::read(&resolved_path).await?;
        let has_crlf = bytes.windows(2).any(|w| w == b"\r\n");
        let content = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
        let normalized_old = args.old_text.replace("\r\n", "\n");

        // B3-9 (audit fix): replacer cascade. Previously dirge did
        // a single exact-substring match and bailed with
        // "old_text not found" on any whitespace, indent, or
        // trailing-space drift. opencode's edit.ts:222-432 has a
        // 5-step cascade; pi's edit-diff.ts:91-132 has
        // fuzzyFindText. We port the three highest-value steps
        // (LineTrimmed, WhitespaceNormalized, IndentationFlexible)
        // which together catch the ~95% of LLM whitespace drift
        // failures. Each fallback is logged so the user sees
        // we matched with tolerance, not exactness.
        let (match_positions, fallback_used): (Vec<(usize, usize)>, Option<&'static str>) = {
            // Step 1: simple exact match (current behaviour).
            let exact: Vec<(usize, usize)> = content
                .match_indices(&normalized_old)
                .map(|(i, _)| (i, i + normalized_old.len()))
                .collect();
            if !exact.is_empty() {
                (exact, None)
            } else if let Some(matches) = find_line_trimmed_matches(&content, &normalized_old)
                && !matches.is_empty()
            {
                (matches, Some("line-trimmed"))
            } else if let Some(matches) =
                find_whitespace_normalized_matches(&content, &normalized_old)
                && !matches.is_empty()
            {
                (matches, Some("whitespace-normalized"))
            } else if let Some(matches) =
                find_indentation_flexible_matches(&content, &normalized_old)
                && !matches.is_empty()
            {
                (matches, Some("indentation-flexible"))
            } else {
                (Vec::new(), None)
            }
        };

        if match_positions.is_empty() {
            return Err(ToolError::Msg(format!(
                "old_text not found in '{}'.\nEnsure the exact text matches including whitespace and line endings. \
                Tried exact match, line-trimmed match, whitespace-normalized match, and indentation-flexible match.",
                args.path
            )));
        }

        // dirge-nj6d: the fuzzy fallback matchers can return OVERLAPPING
        // byte ranges (e.g. the whitespace-normalized matcher tries block
        // sizes up to +5, so two different start lines can cover the same
        // region). Splicing overlapping ranges — even in reverse — corrupts
        // the buffer or panics at a non-char boundary inside
        // `replace_range`. Keep only a disjoint set so every downstream
        // consumer (ambiguity count + replace_all splice) is safe.
        let match_ranges: Vec<(usize, usize)> = keep_disjoint_ranges(match_positions);
        // Reduce to start positions for backwards compat with the
        // downstream ambiguity-reporting and replacement logic.
        let match_positions: Vec<usize> = match_ranges.iter().map(|(s, _)| *s).collect();

        let do_replace_all = args.replace_all.unwrap_or(false);

        if match_positions.len() > 1 && !do_replace_all {
            let line_starts: Vec<usize> = std::iter::once(0)
                .chain(content.match_indices('\n').map(|(i, _)| i + 1))
                .collect();

            // Cap the per-match preview list so a pattern matching
            // thousands of lines doesn't return a thousand-line error
            // blob to the LLM — which would blow the agent's context
            // and crowd out the actual narrative. Show the first
            // MAX_AMBIGUOUS_MATCHES, then a single "...and N more"
            // line. 20 is enough to disambiguate any realistic case
            // (functions named identically, repeated string lits) while
            // keeping the error under a few KB.
            const MAX_AMBIGUOUS_MATCHES: usize = 20;
            let total_matches = match_positions.len();
            let preview_positions: &[usize] =
                &match_positions[..total_matches.min(MAX_AMBIGUOUS_MATCHES)];

            let mut match_info = Vec::with_capacity(preview_positions.len() + 1);
            for &byte_idx in preview_positions {
                let line_num = match line_starts.binary_search(&byte_idx) {
                    Ok(i) => i + 1,
                    Err(i) => i,
                };
                let line_start = line_starts.get(line_num - 1).copied().unwrap_or(0);
                let line_end = content[line_start..]
                    .find('\n')
                    .map(|e| line_start + e)
                    .unwrap_or(content.len());
                let line_text = &content[line_start..line_end];
                let truncated: String = line_text.chars().take(100).collect();
                match_info.push(format!("  Line {}: {}", line_num, truncated));
            }
            if total_matches > MAX_AMBIGUOUS_MATCHES {
                let remaining = total_matches - MAX_AMBIGUOUS_MATCHES;
                match_info.push(format!(
                    "  ... and {} more match{}",
                    remaining,
                    if remaining == 1 { "" } else { "es" },
                ));
            }

            return Err(ToolError::Msg(format!(
                "old_text matched {} times in {}:\n{}\n\nUse replace_all: true to replace all occurrences, or provide more surrounding context in old_text to narrow the match.",
                total_matches,
                args.path,
                match_info.join("\n"),
            )));
        }

        let byte_pos = match_positions[0];
        // B3-9: when the cascade fired, the matched substring may
        // differ from normalized_old (different whitespace/indent).
        // Replace by exact byte range instead of string.replace
        // (which would re-search normalized_old and not find it).
        let new_content = if do_replace_all {
            // For replace_all we splice every range in reverse
            // order so earlier offsets stay valid.
            let mut out = content.clone();
            let mut ranges = match_ranges.clone();
            ranges.sort_by_key(|r| std::cmp::Reverse(r.0));
            for (start, end) in ranges {
                out.replace_range(start..end, &args.new_text);
            }
            out
        } else {
            let (start, end) = match_ranges[0];
            let mut out = content.clone();
            out.replace_range(start..end, &args.new_text);
            out
        };

        // B3-9: surface the fallback used so the LLM sees we
        // didn't match exactly — helps it correct future calls.
        let fallback_note = match fallback_used {
            Some(label) => format!(
                " (matched via {} fallback — exact text not found; whitespace/indent tolerated)",
                label
            ),
            None => String::new(),
        };

        let output = if has_crlf {
            new_content.replace('\n', "\r\n")
        } else {
            new_content
        };

        // Phase-2 tree-sitter validation: refuse to write
        // syntactically-broken edits so the model sees the error
        // in the same turn. See docs/AGENTIC_LOOP_PLAN.md §2.
        #[cfg(feature = "semantic")]
        if let Err(errors) = crate::semantic::syntax_validator::check_syntax(
            std::path::Path::new(&resolved_path),
            &output,
        ) {
            return Err(ToolError::Msg(
                crate::semantic::syntax_validator::format_errors(
                    std::path::Path::new(&resolved_path),
                    &output,
                    &errors,
                ),
            ));
        }
        #[cfg(feature = "lsp")]
        let write_at = std::time::Instant::now();
        // Atomic write so a mid-write crash leaves the previous
        // content intact rather than a truncated half-write.
        crate::fs_atomic::atomic_write(std::path::Path::new(&resolved_path), output.as_bytes())
            .await?;
        crate::agent::tools::modified::mark_modified(std::path::Path::new(&resolved_path));
        // File mutated → invalidate cached reads/greps/listings for this turn.
        if let Some(ref cache) = self.cache {
            cache.clear();
        }

        // Path lives in the chamber banner (`╭─ EDIT ─ "<path>" ─╮`),
        // so don't repeat it. The diff block below is the meat;
        // this first line is a compact summary.
        let mut result = if do_replace_all {
            format!(
                "Applied edit ({} replacements){}",
                match_positions.len(),
                fallback_note
            )
        } else {
            format!("Applied edit{}", fallback_note)
        };
        // Mention the line delta when adding/removing lines so the
        // LLM can confirm the size of change without re-reading
        // the diff block. For replace_all the per-replacement
        // delta multiplies by the number of replacements — the
        // user wants the FILE delta, not the per-instance delta.
        let old_lines = args.old_text.lines().count();
        let new_lines = args.new_text.lines().count();
        let per_replacement_delta = new_lines as i64 - old_lines as i64;
        let total_delta = if do_replace_all {
            per_replacement_delta * (match_positions.len() as i64)
        } else {
            per_replacement_delta
        };
        if total_delta != 0 {
            result.push_str(&format!(" ({:+} lines)", total_delta));
        }

        // Always emit a diff. The earlier 20-line cap was meant to
        // keep LLM context lean, but in practice it silently hid
        // useful diffs for any non-trivial edit. Bump to 200 lines
        // per side which covers the vast majority of real edits;
        // edits larger than that are likely refactors where the
        // "edit + diff" pattern isn't the right tool anyway.
        // `old_lines` / `new_lines` already computed above for the
        // delta summary.
        if old_lines <= 200 && new_lines <= 200 {
            result.push_str(&Self::show_diff(
                &args.path,
                &content,
                byte_pos,
                &args.old_text,
                &args.new_text,
            ));
        }

        #[cfg(feature = "lsp")]
        {
            let path = std::path::Path::new(&resolved_path);
            result.push_str(
                &crate::agent::tools::write::append_lsp_block(
                    self.lsp_manager.as_ref(),
                    path,
                    write_at,
                )
                .await,
            );
        }
        Ok(result)
    }
}

// B3-9 — replacer cascade helpers. Port of opencode's edit.ts:240-540
// fallback ladder. Each helper returns a Vec of (start_byte,
// end_byte) byte ranges in `content` that match `find` under the
// helper's normalization. Empty Vec = no matches. The cascade
// tries each in priority order in the call site above.

/// dirge-nj6d: reduce a set of (start, end) byte ranges to a disjoint
/// subset. Sorts by start and greedily keeps a range only if it begins
/// at or after the previously-kept range's end; overlapping ranges are
/// dropped. This protects the reverse-order `replace_range` splice from
/// corruption / non-char-boundary panics when the fuzzy matchers emit
/// overlapping candidates. Stable preference: the earliest-starting
/// range of any overlapping cluster wins.
fn keep_disjoint_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_by_key(|r| r.0);
    let mut disjoint: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match disjoint.last() {
            Some(&(_, last_end)) if start < last_end => {} // overlaps kept → drop
            _ => disjoint.push((start, end)),
        }
    }
    disjoint
}

/// Line-trimmed match. Match each logical block of N lines where
/// each line's .trim() equals the corresponding find line's
/// .trim(). Catches the common case of "LLM emitted the right
/// content but with slightly off indent or trailing whitespace."
/// Mirrors opencode `LineTrimmedReplacer` (edit.ts:244).
fn find_line_trimmed_matches(content: &str, find: &str) -> Option<Vec<(usize, usize)>> {
    let content_lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.is_empty() {
        return None;
    }
    // Line-start byte offsets for content.
    let mut line_starts = Vec::with_capacity(content_lines.len() + 1);
    line_starts.push(0usize);
    let mut acc = 0usize;
    for line in &content_lines {
        acc += line.len() + 1; // +1 for the \n separator
        line_starts.push(acc);
    }
    let mut out = Vec::new();
    for i in 0..=content_lines.len().saturating_sub(find_lines.len()) {
        let block = &content_lines[i..i + find_lines.len()];
        let all_trim_match = block
            .iter()
            .zip(find_lines.iter())
            .all(|(a, b)| a.trim() == b.trim());
        if !all_trim_match {
            continue;
        }
        let start_byte = line_starts[i];
        // End of the matched block (no trailing \n unless the
        // block ends with one in source). Compute by walking
        // forward: sum byte lengths + (n-1) interior newlines.
        let mut end_byte = start_byte;
        for (k, line) in block.iter().enumerate() {
            end_byte += line.len();
            if k < block.len() - 1 {
                end_byte += 1;
            }
        }
        out.push((start_byte, end_byte));
    }
    Some(out)
}

/// Whitespace-normalized match. Collapse all whitespace runs in
/// both content and find to single spaces, then look for line-by-
/// line equality. Mirrors opencode `WhitespaceNormalizedReplacer`
/// (edit.ts:419). Catches "LLM tab vs spaces" / "double-spaces"
/// drift.
fn find_whitespace_normalized_matches(content: &str, find: &str) -> Option<Vec<(usize, usize)>> {
    fn normalize(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut prev_ws = false;
        for c in s.chars() {
            if c.is_whitespace() {
                if !prev_ws && !out.is_empty() {
                    out.push(' ');
                }
                prev_ws = true;
            } else {
                out.push(c);
                prev_ws = false;
            }
        }
        if out.ends_with(' ') {
            out.pop();
        }
        out
    }
    let norm_find = normalize(find);
    if norm_find.is_empty() {
        return None;
    }
    let find_lines: Vec<&str> = find.split('\n').collect();
    let content_lines: Vec<&str> = content.split('\n').collect();
    let mut line_starts = Vec::with_capacity(content_lines.len() + 1);
    line_starts.push(0usize);
    let mut acc = 0usize;
    for line in &content_lines {
        acc += line.len() + 1;
        line_starts.push(acc);
    }
    // Try block sizes from find_lines.len() up to find_lines.len()
    // + 5 (cap) so a single-line `find` can match a 3-line block in
    // content (LLM emitted "fn foo() { let x = 1; }" but source has
    // it on 3 lines). +5 covers typical re-formatting drift without
    // O(N²) blowup. For a given start line, keep only the SHORTEST
    // matching block size — multiple block sizes can hit the same
    // start when trailing empty lines normalize to nothing.
    use std::collections::HashMap;
    let mut by_start: HashMap<usize, (usize, usize)> = HashMap::new();
    let max_block = find_lines.len() + 5;
    for block_size in find_lines.len()..=max_block.min(content_lines.len()) {
        if block_size == 0 {
            continue;
        }
        for i in 0..=content_lines.len().saturating_sub(block_size) {
            let block = &content_lines[i..i + block_size];
            let block_text = block.join("\n");
            if normalize(&block_text) != norm_find {
                continue;
            }
            let start_byte = line_starts[i];
            let mut end_byte = start_byte;
            for (k, line) in block.iter().enumerate() {
                end_byte += line.len();
                if k < block.len() - 1 {
                    end_byte += 1;
                }
            }
            by_start
                .entry(start_byte)
                .and_modify(|cur| {
                    if end_byte < cur.1 {
                        *cur = (start_byte, end_byte);
                    }
                })
                .or_insert((start_byte, end_byte));
        }
    }
    let mut out: Vec<(usize, usize)> = by_start.into_values().collect();
    out.sort_by_key(|(s, _)| *s);
    Some(out)
}

/// Indentation-flexible match. Strip the minimum common leading
/// whitespace from both find and each candidate block, then
/// compare. Mirrors opencode `IndentationFlexibleReplacer`
/// (edit.ts:463). Catches the case where the LLM emitted code
/// with a different baseline indent than the source.
fn find_indentation_flexible_matches(content: &str, find: &str) -> Option<Vec<(usize, usize)>> {
    fn strip_min_indent(s: &str) -> String {
        let lines: Vec<&str> = s.split('\n').collect();
        let min_indent = lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
            .min()
            .unwrap_or(0);
        lines
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    String::from(*l)
                } else {
                    // Slice off the first min_indent characters
                    // safely (each is whitespace, so single-byte
                    // ASCII; but use char-aware slice anyway).
                    let mut chars = l.chars();
                    for _ in 0..min_indent {
                        chars.next();
                    }
                    chars.collect::<String>()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    let norm_find = strip_min_indent(find);
    let find_lines: Vec<&str> = find.split('\n').collect();
    let content_lines: Vec<&str> = content.split('\n').collect();
    let mut line_starts = Vec::with_capacity(content_lines.len() + 1);
    line_starts.push(0usize);
    let mut acc = 0usize;
    for line in &content_lines {
        acc += line.len() + 1;
        line_starts.push(acc);
    }
    let mut out = Vec::new();
    for i in 0..=content_lines.len().saturating_sub(find_lines.len()) {
        let block = &content_lines[i..i + find_lines.len()];
        let block_text = block.join("\n");
        if strip_min_indent(&block_text) != norm_find {
            continue;
        }
        let start_byte = line_starts[i];
        let mut end_byte = start_byte;
        for (k, line) in block.iter().enumerate() {
            end_byte += line.len();
            if k < block.len() - 1 {
                end_byte += 1;
            }
        }
        out.push((start_byte, end_byte));
    }
    Some(out)
}

#[cfg(test)]
mod fuzzy_tests {
    use super::*;

    #[test]
    fn line_trimmed_matches_indent_drift() {
        let content = "fn foo() {\n    let x = 1;\n    let y = 2;\n}\n";
        // LLM emitted with no leading indent.
        let find = "let x = 1;\nlet y = 2;";
        let m = find_line_trimmed_matches(content, find).unwrap();
        assert_eq!(m.len(), 1);
        let (s, e) = m[0];
        assert_eq!(&content[s..e], "    let x = 1;\n    let y = 2;");
    }

    #[test]
    fn line_trimmed_no_match_when_content_differs() {
        let content = "let x = 1;\nlet y = 2;\n";
        let find = "let x = 1;\nlet z = 3;";
        let m = find_line_trimmed_matches(content, find).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn whitespace_normalized_matches_tab_vs_spaces() {
        let content = "fn  foo()  {\n\tlet x = 1;\n}\n";
        let find = "fn foo() { let x = 1; }";
        let m = find_whitespace_normalized_matches(content, find).unwrap();
        // Block spans the 3 lines fn... { ... } when joined.
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn indentation_flexible_matches_re_indented_block() {
        let content = "fn foo() {\n        let x = 1;\n        let y = 2;\n}\n";
        // LLM emitted the inner block with NO baseline indent.
        let find = "let x = 1;\nlet y = 2;";
        let m = find_indentation_flexible_matches(content, find).unwrap();
        assert_eq!(m.len(), 1);
        let (s, e) = m[0];
        assert_eq!(&content[s..e], "        let x = 1;\n        let y = 2;");
    }

    // ── dirge-nj6d: overlapping-range dedup for replace_all ──

    #[test]
    fn keep_disjoint_drops_overlaps_keeps_earliest() {
        // Two overlapping clusters; earliest-starting range of each wins.
        let input = vec![(0, 10), (5, 15), (12, 20), (20, 25)];
        assert_eq!(
            keep_disjoint_ranges(input),
            vec![(0, 10), (12, 20), (20, 25)],
        );
    }

    #[test]
    fn keep_disjoint_sorts_unsorted_input() {
        // Unsorted input with a nested range fully inside an earlier one.
        let input = vec![(20, 25), (0, 30), (5, 8)];
        // After sort: (0,30),(5,8),(20,25). (5,8) and (20,25) both inside
        // (0,30) → dropped.
        assert_eq!(keep_disjoint_ranges(input), vec![(0, 30)]);
    }

    #[test]
    fn keep_disjoint_adjacent_ranges_are_kept() {
        // end-exclusive: (0,5) and (5,10) touch but don't overlap.
        let input = vec![(0, 5), (5, 10)];
        assert_eq!(keep_disjoint_ranges(input), vec![(0, 5), (5, 10)]);
    }

    #[test]
    fn keep_disjoint_empty() {
        assert!(keep_disjoint_ranges(Vec::new()).is_empty());
    }

    /// Mirrors the call-site reverse-order `replace_range` splice over a
    /// set that originated as OVERLAPPING matcher output. Without
    /// `keep_disjoint_ranges` this corrupts the buffer (and can panic at a
    /// non-char boundary); with it, the splice is safe and correct.
    #[test]
    fn replace_all_reverse_splice_over_deduped_ranges_is_safe() {
        let content = "aXbXc".to_string();
        // (1,3) and (1,2) overlap; (3,4) is disjoint.
        let overlapping = vec![(1, 3), (1, 2), (3, 4)];
        let ranges = keep_disjoint_ranges(overlapping);
        assert_eq!(ranges, vec![(1, 3), (3, 4)]);
        let mut out = content.clone();
        for (s, e) in ranges.into_iter().rev() {
            out.replace_range(s..e, "_");
        }
        assert_eq!(out, "a__c");
    }
}
