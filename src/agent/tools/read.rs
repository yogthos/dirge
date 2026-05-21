#[cfg(feature = "lsp")]
use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, PermCheck, ReadArgs, ToolError, check_perm_path};
#[cfg(feature = "lsp")]
use crate::lsp::manager::{LspManager, TouchMode};

pub struct ReadTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub cache: Option<ToolCache>,
    /// When set, the tool fires off a `touch_file` to warm the LSP server
    /// so subsequent edits surface diagnostics quickly. Fire-and-forget:
    /// the read tool does not wait or surface diagnostics in its output.
    #[cfg(feature = "lsp")]
    pub lsp_manager: Option<Arc<LspManager>>,
}

impl ReadTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        ReadTool {
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
        ReadTool {
            permission,
            ask_tx,
            cache: Some(cache),
            #[cfg(feature = "lsp")]
            lsp_manager,
        }
    }
}

impl Tool for ReadTool {
    const NAME: &'static str = "read";

    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "read".to_string(),
            description: "Read the contents of a file. Supports text files. Defaults to first 2000 lines. Use offset/limit for large files.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                    "offset": { "type": "integer", "description": "Line number to start from (1-indexed)" },
                    "limit": { "type": "integer", "description": "Maximum number of lines to read" }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        check_perm_path(&self.permission, &self.ask_tx, "read", &args.path).await?;

        let cache_key = format!(
            "read:{}:{}:{}",
            args.path,
            args.offset.unwrap_or(1),
            args.limit.unwrap_or(2000),
        );

        if let Some(ref cache) = self.cache {
            if let Some(cached) = cache.get(&cache_key) {
                return Ok(cached);
            }
        }

        // F4: stream the file line-by-line via BufReader instead of
        // loading the whole thing into memory with `read_to_string`.
        // Removes the prior 10MB hard cap (large logs / generated
        // files were unreachable) and caps each individual line at
        // `MAX_LINE_BYTES` so a pathological minified-JS file with a
        // 100MB single line doesn't OOM us. opencode (`read.ts:119-150`)
        // and pi (`read.ts:215-328`) both stream + smart-truncate.
        //
        // Safety net: refuse files larger than 1GB. Reading 1GB into
        // even line-counting takes a while; if a user needs this we'd
        // tell them to use bash + head/tail/grep.
        const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
        const MAX_LINE_BYTES: usize = 16 * 1024;
        const TRUNC_MARKER: &str = " …[line truncated]";

        let metadata = tokio::fs::metadata(&args.path).await?;
        let file_size = metadata.len();
        if file_size > MAX_FILE_BYTES {
            return Err(ToolError::Msg(format!(
                "File too large ({} bytes). Max 1GB. Use bash + head/tail/grep for sampling.",
                file_size
            )));
        }

        let offset = args.offset.unwrap_or(1).max(1) - 1;
        let limit = args.limit.unwrap_or(2000);

        use tokio::io::AsyncBufReadExt;
        let file = tokio::fs::File::open(&args.path).await?;
        let reader = tokio::io::BufReader::new(file);
        let mut lines = reader.lines();
        let mut total_lines = 0usize;
        let mut excerpt_lines: Vec<(usize, String)> = Vec::with_capacity(limit);
        let want_end = offset.saturating_add(limit);
        let mut first_line = true;
        while let Some(line) = lines.next_line().await.transpose() {
            let mut line = line?;
            // F19: strip UTF-8 BOM from the FIRST line only. Old
            // Windows-saved files start with U+FEFF (0xEF 0xBB 0xBF);
            // when present, the BOM ended up as a leading 3-byte
            // invisible-character prefix in the LLM context.
            // opencode `read.ts` uses `Bom.readFile()` for the same
            // reason.
            if first_line {
                if let Some(stripped) = line.strip_prefix('\u{FEFF}') {
                    line = stripped.to_string();
                }
                first_line = false;
            }
            if line.len() > MAX_LINE_BYTES {
                // Truncate by byte index — careful to land on a UTF-8
                // boundary. Drop bytes until we find one.
                let mut truncate_at = MAX_LINE_BYTES;
                while !line.is_char_boundary(truncate_at) {
                    truncate_at -= 1;
                }
                line.truncate(truncate_at);
                line.push_str(TRUNC_MARKER);
            }
            let line_idx = total_lines;
            total_lines += 1;
            if line_idx >= offset && line_idx < want_end {
                excerpt_lines.push((line_idx, line));
            }
            // Past the requested range — keep counting to compute
            // `total_lines` for the header, but skip allocation.
        }

        let end = (offset + limit).min(total_lines);
        let width = (total_lines.to_string().len()).max(1);
        let excerpt: String = excerpt_lines
            .into_iter()
            .map(|(idx, line)| format!("{:>width$}: {}", idx + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        // Path lives in the chamber banner (`╭─ READ ─ "<path>" ─╮`),
        // so don't repeat it here — that was visible duplication.
        // The first line inside the chamber is now a compact metadata
        // summary, followed by a blank line and the excerpt.
        //
        // Edge cases:
        // - Empty file (total_lines = 0): no useful range; just say
        //   "(empty file)" without the showing-lines clause that
        //   would otherwise render the nonsense "showing lines 1-0".
        // - offset past EOF (`offset >= total_lines`): no lines
        //   match; report that explicitly instead of showing the
        //   backwards `X-(X-1)` range.
        let info = if total_lines == 0 {
            "(empty file)\n".to_string()
        } else if offset >= total_lines {
            format!(
                "({} lines total; offset {} is past end of file — nothing to show)\n",
                total_lines,
                offset + 1,
            )
        } else {
            format!(
                "({} lines total, showing lines {}-{})\n\n{}",
                total_lines,
                offset + 1,
                end,
                excerpt
            )
        };

        if let Some(ref cache) = self.cache {
            cache.set(&cache_key, info.clone());
        }

        // Fire-and-forget LSP warmup so the server already has the file
        // open by the time the agent edits it (and we can wait_for_push
        // quickly). No diagnostic surfacing on read.
        #[cfg(feature = "lsp")]
        if let Some(manager) = self.lsp_manager.clone() {
            let path = std::path::PathBuf::from(&args.path);
            tokio::spawn(async move {
                manager.touch_file(&path, TouchMode::Notify).await;
            });
        }

        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::ReadArgs;

    /// Verifies the line-numbering format used in read output.
    /// The model sees this format and must strip "NNN: " prefixes when passing text to edit.
    #[test]
    fn test_line_number_format() {
        let content = "line one\nline two\nline three\n";
        let total_lines = content.lines().count();
        let excerpt: String = content
            .lines()
            .take(3)
            .enumerate()
            .map(|(i, line)| {
                let width = (total_lines.to_string().len()).max(1);
                format!("{:>width$}: {}", i + 1, line)
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(excerpt, "1: line one\n2: line two\n3: line three");
    }

    fn temp_path(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("dirge-read-test-{}-{}", std::process::id(), suffix,))
    }

    /// F4: pathological lines (e.g. 100KB single line from minified JS
    /// or accidentally cat'd binary) truncate at MAX_LINE_BYTES with a
    /// clear marker. Without this, the LLM context could be flooded
    /// with a single multi-MB line.
    #[tokio::test]
    async fn read_truncates_pathological_long_lines() {
        let path = temp_path("longline");
        let pathological = "a".repeat(100_000);
        std::fs::write(&path, format!("short\n{}\nshort2", pathological)).unwrap();

        let tool = ReadTool::new(None, None);
        let out = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(
            out.contains("…[line truncated]"),
            "truncation marker missing"
        );
        assert!(
            out.len() < 100_000,
            "output should not contain the full 100k line; got {} bytes",
            out.len(),
        );
        assert!(out.contains("short"), "first short line missing");
        assert!(out.contains("short2"), "trailing short line missing");
    }

    /// F4: files >10MB used to be rejected outright. Stream-read
    /// should handle them up to the new 1GB safety net. Skip the
    /// expensive case in CI but at least verify a 1MB file works.
    #[tokio::test]
    async fn read_handles_files_larger_than_old_10mb_cap() {
        let path = temp_path("mediumfile");
        // 1MB of repeated 100-byte lines = ~10_000 lines.
        let line = "x".repeat(99);
        let body = (0..10_000)
            .map(|_| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &body).unwrap();
        assert!(body.len() > 900_000, "fixture is at least ~1MB");

        let tool = ReadTool::new(None, None);
        let result = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: Some(1),
                limit: Some(5),
            })
            .await;
        let _ = std::fs::remove_file(&path);

        let out = result.expect("read of medium file must succeed");
        // Header reports total_lines as the real count, not capped.
        assert!(
            out.contains("10000 lines total") || out.contains("10001 lines total"),
            "expected ~10000 line total in header; got: {}",
            out.lines().next().unwrap_or(""),
        );
        // Only the first 5 are in the excerpt.
        let body_lines: Vec<&str> = out.lines().skip(2).collect();
        assert_eq!(body_lines.len(), 5);
    }

    /// F19: UTF-8 BOM (U+FEFF, bytes 0xEF 0xBB 0xBF) at the start
    /// of a file is stripped before the line reaches the LLM. The
    /// raw 3-byte prefix would otherwise render as an
    /// invisible-character at the start of line 1.
    #[tokio::test]
    async fn read_strips_utf8_bom_from_first_line() {
        let path = temp_path("bom");
        let bom = "\u{FEFF}";
        std::fs::write(&path, format!("{bom}first\nsecond")).unwrap();

        let tool = ReadTool::new(None, None);
        let out = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);

        // Body lines (after the metadata header + blank line).
        let body: Vec<&str> = out.lines().skip(2).collect();
        assert_eq!(body, vec!["1: first", "2: second"]);
        // No BOM byte anywhere in the output.
        assert!(
            !out.contains('\u{FEFF}'),
            "BOM should be stripped: {:?}",
            out,
        );
    }

    /// F19: only the FIRST line gets BOM-stripped. A mid-file BOM
    /// (extremely rare but possible) is preserved as a regular
    /// character.
    #[tokio::test]
    async fn read_only_strips_bom_at_start_of_file() {
        let path = temp_path("bom-mid");
        let bom = "\u{FEFF}";
        std::fs::write(&path, format!("first\n{bom}second")).unwrap();

        let tool = ReadTool::new(None, None);
        let out = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);

        // The mid-file BOM stays.
        assert!(out.contains('\u{FEFF}'));
    }

    /// Self-review bug: empty file used to render the nonsense
    /// range `"(0 lines total, showing lines 1-0)"`. Now reports
    /// `"(empty file)"` directly.
    #[tokio::test]
    async fn read_reports_empty_file_explicitly() {
        let path = temp_path("empty");
        std::fs::write(&path, "").unwrap();

        let tool = ReadTool::new(None, None);
        let out = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(
            out.contains("(empty file)"),
            "expected explicit empty marker; got: {out:?}",
        );
        assert!(
            !out.contains("showing lines 1-0"),
            "must NOT show the nonsense backwards range: {out:?}",
        );
    }

    /// Self-review bug: offset past EOF used to render
    /// `"showing lines 100-1"` (backwards). Now reports the past-
    /// end condition explicitly.
    #[tokio::test]
    async fn read_reports_offset_past_eof_explicitly() {
        let path = temp_path("short");
        std::fs::write(&path, "only line\n").unwrap();

        let tool = ReadTool::new(None, None);
        let out = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: Some(100),
                limit: Some(10),
            })
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(
            out.contains("past end of file"),
            "expected past-end marker; got: {out:?}",
        );
        assert!(
            !out.contains("showing lines 100-1"),
            "must NOT show backwards range: {out:?}",
        );
    }
}
