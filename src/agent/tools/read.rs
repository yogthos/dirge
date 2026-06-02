#[cfg(feature = "lsp")]
use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::agent_loop::tool_input_repair::with_contract_hint;
use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, PermCheck, ReadArgs, ToolError, check_perm_path_resolve};
#[cfg(feature = "lsp")]
use crate::lsp::manager::{LspManager, TouchMode};

/// Reject these extensions outright as binary — matches opencode's
/// `read.ts` extension list. Sampling-based detection below catches
/// anything not on this list (custom compiled artifacts, etc.).
fn is_binary_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let ext = match lower.rsplit_once('.') {
        Some((_, e)) => e,
        None => return false,
    };
    matches!(
        ext,
        "zip"
            | "tar"
            | "gz"
            | "tgz"
            | "bz2"
            | "xz"
            | "7z"
            | "rar"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "class"
            | "jar"
            | "war"
            | "wasm"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "ppt"
            | "pptx"
            | "odt"
            | "ods"
            | "odp"
            | "pdf"
            | "bin"
            | "dat"
            | "obj"
            | "o"
            | "a"
            | "lib"
            | "pyc"
            | "pyo"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "bmp"
            | "ico"
            | "mp3"
            | "mp4"
            | "mov"
            | "avi"
            | "ogg"
            | "wav"
            | "flac"
    )
}

/// Sample-based binary detection — opencode `read.ts:187-198`.
/// Any null byte → binary. Otherwise count "non-printable" bytes
/// (outside `\t\n\r` and printable ASCII range); if more than 30%
/// of the sample, treat as binary.
fn is_binary_content(sample: &[u8]) -> bool {
    if sample.is_empty() {
        return false;
    }
    let mut non_printable = 0usize;
    for &b in sample {
        if b == 0 {
            return true;
        }
        if b < 9 || (b > 13 && b < 32) {
            non_printable += 1;
        }
    }
    (non_printable * 100) / sample.len() > 30
}

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
            description: with_contract_hint(
                "read",
                "Read the contents of a file. Supports text files. Defaults to first 2000 lines. Use offset/limit for large files.",
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The absolute path to the file to read (must be absolute, not relative)",
                        "dirge-hints": {"semantic": "absolute_path"}
                    },
                    "offset": { "type": "integer", "description": "Line number to start from (1-indexed)" },
                    "limit": { "type": "integer", "description": "Maximum number of lines to read" }
                },
                "required": ["path"],
                // Phase-2: when `limit` is given but `offset` is
                // not (or vice versa), the harness auto-fills the
                // missing one with `offset = 0` and prepends a
                // Note: to the tool result so the model knows the
                // default was applied. Replaces the hardcoded note
                // in `read::call`'s body — that path can be
                // removed once every relational pairing migrates
                // here.
                "dirge-hints": {
                    "relational": [
                        {
                            "requires": ["offset", "limit"],
                            "defaults": {"offset": 0}
                        }
                    ]
                }
            }),
        }
    }

    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        // Reject non-absolute paths immediately with a clear error
        // (shared guard; the schema declares `semantic: absolute_path`).
        crate::agent::tools::require_absolute_path(&args.path, "the read path")
            .map_err(ToolError::Msg)?;
        // Audit H12: pin the path we'll actually open to the same
        // canonical form the permission check ran against, so a
        // symlink swap between check and open can't land us on a
        // different file than the user authorized.
        let resolved_path =
            check_perm_path_resolve(&self.permission, &self.ask_tx, "read", &args.path).await?;

        // Relational defaulting: when only one of offset/limit is
        // provided, fill the other with a sensible default and surface
        // the decision as a Note: line (not Error: — keeps TUI from
        // painting it red). This lets the model self-correct next turn.
        let (offset, limit, default_note) = match (args.offset, args.limit) {
            (Some(o), Some(l)) => (o.max(1) - 1, l, None),
            (Some(o), None) => (
                o.max(1) - 1,
                2000,
                Some(
                    "Note: limit was not provided; defaulted to 2000 lines. To read fewer or more, retry with both offset (1-indexed) and limit.",
                ),
            ),
            (None, Some(l)) => (
                0,
                l,
                Some(
                    "Note: offset was not provided; defaulted to line 1 (start of file). To start elsewhere, retry with both offset (1-indexed) and limit.",
                ),
            ),
            (None, None) => (0, 2000, None),
        };

        // LOOP-3: include the file's mtime+size in the cache key so
        // an external write (LSP, IDE, plugin-spawned bash, MCP
        // tool) invalidates the cache automatically. See
        // `cache::fs_stamp` for the encoding.
        let stamp = crate::agent::tools::cache::fs_stamp(std::path::Path::new(&args.path));
        let cache_key = format!(
            "read:{}:{}:{}:{}",
            args.path,
            offset.saturating_add(1), // 0-based → 1-based for cache key stability
            limit,
            stamp,
        );

        if let Some(ref cache) = self.cache
            && let Some(cached) = cache.get(&cache_key)
        {
            // A cache hit means the model has already seen this file's content
            // this session — keep the read-before-edit gate satisfied.
            cache.mark_read(std::path::Path::new(&resolved_path));
            return Ok(cached);
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

        let metadata = tokio::fs::metadata(&resolved_path).await?;
        let file_size = metadata.len();
        if file_size > MAX_FILE_BYTES {
            return Err(ToolError::Msg(format!(
                "File too large ({} bytes). Max 1GB. Use bash + head/tail/grep for sampling.",
                file_size
            )));
        }

        use tokio::io::{AsyncBufReadExt, AsyncReadExt};

        // Binary file detection — refuse before streaming so we
        // don't shovel multi-MB of corrupted UTF-8 into LLM
        // context. Pattern matches opencode `read.ts:153-198`:
        // reject by extension OR by sampling the first 4 KiB
        // for null bytes / non-printable density. The agent gets
        // a clear "Cannot read binary file" hint instead of
        // garbled output.
        if is_binary_extension(args.path.as_str()) {
            return Err(ToolError::Msg(format!(
                "Cannot read binary file: {} (use bash with a hex/xxd tool if you really need bytes)",
                args.path,
            )));
        }
        {
            let mut sniffer = tokio::fs::File::open(&resolved_path).await?;
            // Audit L17: don't always allocate 4 KiB. For a tiny file
            // (e.g. 100-byte config), a 4 KiB zeroed buffer plus
            // the partial read was wasteful; for hot reads across
            // thousands of small files it added up. Size the sample
            // to `min(file_size, 4096)`.
            let sample_size = (file_size as usize).min(4096);
            let mut sample = vec![0u8; sample_size];
            let n = sniffer.read(&mut sample).await?;
            sample.truncate(n);
            if is_binary_content(&sample) {
                return Err(ToolError::Msg(format!(
                    "Cannot read binary file: {} (null bytes / high non-printable density detected)",
                    args.path,
                )));
            }
        }

        let file = tokio::fs::File::open(&resolved_path).await?;
        let reader = tokio::io::BufReader::new(file);
        let mut lines = reader.lines();
        let mut total_lines = 0usize;
        let mut excerpt_lines: Vec<(usize, String)> = Vec::with_capacity(limit);
        let want_end = offset.saturating_add(limit);
        let mut first_line = true;
        // Audit L11: bail out once we've satisfied the requested
        // range plus a tiny buffer for the header's `total_lines`.
        // For a 1GB log with offset=0, limit=100, the previous code
        // read all 1GB just to populate `total_lines` for the
        // header. Cap the lookahead at LOOKAHEAD_PAST_WANT_END
        // lines past the range so the header gets a real count for
        // most real-world reads, and switch to an `≥N` marker when
        // we hit the cap.
        const LOOKAHEAD_PAST_WANT_END: usize = 1024;
        let lookahead_stop = want_end.saturating_add(LOOKAHEAD_PAST_WANT_END);
        let mut total_lines_is_lower_bound = false;
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
            if total_lines >= lookahead_stop {
                total_lines_is_lower_bound = true;
                break;
            }
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
        // Format the total-lines value as `≥N` when we stopped
        // counting at the lookahead cap (audit L11). The LLM treats
        // this as a lower bound and the user-facing chamber shows
        // the same hint.
        let total_label: String = if total_lines_is_lower_bound {
            format!("≥{}", total_lines)
        } else {
            total_lines.to_string()
        };
        let info = if total_lines == 0 {
            "(empty file)\n".to_string()
        } else if offset >= total_lines {
            format!(
                "({} lines total; offset {} is past end of file — nothing to show)\n",
                total_label,
                offset + 1,
            )
        } else {
            format!(
                "({} lines total, showing lines {}-{})\n\n{}",
                total_label,
                offset + 1,
                end,
                excerpt
            )
        };

        if let Some(ref cache) = self.cache {
            cache.set(&cache_key, info.clone());
            // Satisfy the read-before-edit gate (vix session_read_gate): the
            // model has now seen the on-disk content.
            cache.mark_read(std::path::Path::new(&resolved_path));
        }

        // Fire-and-forget LSP warmup so the server already has the file
        // open by the time the agent edits it (and we can wait_for_push
        // quickly). No diagnostic surfacing on read.
        #[cfg(feature = "lsp")]
        if let Some(manager) = self.lsp_manager.clone() {
            let path = std::path::PathBuf::from(&resolved_path);
            tokio::spawn(async move {
                manager.touch_file(&path, TouchMode::Notify).await;
            });
        }

        if let Some(note) = default_note {
            Ok(format!("{note}\n\n{info}"))
        } else {
            Ok(info)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::ReadArgs;

    /// Binary detection by extension — pdf/exe/o/zip/etc.
    #[test]
    fn test_is_binary_extension_known() {
        assert!(is_binary_extension("foo.pdf"));
        assert!(is_binary_extension("a.tar.gz"));
        assert!(is_binary_extension("dir/lib.so"));
        assert!(is_binary_extension("PHOTO.JPG"));
        assert!(is_binary_extension("class.pyc"));
        assert!(!is_binary_extension("source.rs"));
        assert!(!is_binary_extension("script.py"));
        assert!(!is_binary_extension("README.md"));
        assert!(!is_binary_extension("noext"));
    }

    /// Sample-based binary detection — null byte triggers, plain
    /// UTF-8 doesn't, 30%-non-printable triggers.
    #[test]
    fn test_is_binary_content_null_byte() {
        assert!(is_binary_content(b"hello\x00world"));
        assert!(!is_binary_content(b"hello world"));
        assert!(!is_binary_content(b"")); // empty isn't binary
        // Mostly non-printable → binary.
        let blob: Vec<u8> = (0..100).map(|_| 0x01u8).collect();
        assert!(is_binary_content(&blob));
        // UTF-8 multi-byte (Japanese) — should NOT trip the
        // non-printable heuristic since multi-byte UTF-8 bytes
        // are all >= 128.
        assert!(!is_binary_content("こんにちは世界".as_bytes()));
    }

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
        // Header reports total_lines as a lower-bound (≥N) when the
        // file is much longer than offset+limit+lookahead — audit L11
        // caps scanning to avoid reading 1GB just to populate the
        // total. For this 10k-line file with limit=5, lookahead=1024
        // makes the reported count ≥1029. Accept both the exact form
        // (small files where the whole was scanned) and the
        // lower-bound form (large files).
        let header = out.lines().next().unwrap_or("");
        assert!(
            header.contains("10000 lines total")
                || header.contains("10001 lines total")
                || header.contains("≥"),
            "expected total line count or lower-bound marker in header; got: {}",
            header,
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

    // ============================================================
    // Phase 3: relational defaulting tests
    // ============================================================

    /// Neither offset nor limit → silent defaults, no Note.
    #[tokio::test]
    async fn read_neither_offset_nor_limit_no_note() {
        let path = temp_path("rel-neither");
        std::fs::write(&path, "a\nb\nc\n").unwrap();

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

        assert!(!out.contains("Note:"), "no defaults → no note");
        assert!(out.contains("a"), "should read content");
    }

    /// offset alone → limit defaulted to 2000, Note surfaced.
    #[tokio::test]
    async fn read_offset_alone_defaults_limit_with_note() {
        let path = temp_path("rel-offset");
        std::fs::write(&path, "a\nb\nc\n").unwrap();

        let tool = ReadTool::new(None, None);
        let out = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: Some(2),
                limit: None,
            })
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(out.contains("Note:"), "offset-only should surface note");
        assert!(
            out.contains("limit was not provided"),
            "note should mention limit default: {out}",
        );
        assert!(!out.contains("Error:"), "must use Note: not Error:");
        // Should read from line 2 with default limit of 2000.
        assert!(out.contains("b"), "should contain content");
        assert!(
            !out.contains("1: a"),
            "should NOT contain content before offset"
        );
    }

    /// limit alone → offset defaulted to line 1, Note surfaced.
    #[tokio::test]
    async fn read_limit_alone_defaults_offset_with_note() {
        let path = temp_path("rel-limit");
        std::fs::write(&path, "a\nb\nc\n").unwrap();

        let tool = ReadTool::new(None, None);
        let out = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: None,
                limit: Some(2),
            })
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(out.contains("Note:"), "limit-only should surface note");
        assert!(
            out.contains("offset was not provided"),
            "note should mention offset default: {out}",
        );
        assert!(!out.contains("Error:"), "must use Note: not Error:");
        // Should read from start with limit 2.
        assert!(out.contains("1: a"), "should start from line 1");
        assert!(
            !out.contains("3: c"),
            "should stop at limit; got line 3: {out}"
        );
    }

    /// Both offset and limit → explicit values used, no Note.
    #[tokio::test]
    async fn read_both_offset_and_limit_no_note() {
        let path = temp_path("rel-both");
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();

        let tool = ReadTool::new(None, None);
        let out = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: Some(2),
                limit: Some(2),
            })
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(!out.contains("Note:"), "both set → no defaults → no note");
        assert!(out.contains("2: b"), "should start at line 2");
        assert!(out.contains("3: c"), "should include line 3");
        assert!(!out.contains("4: d"), "should stop after limit of 2");
        // Both provided means 1-indexed offset 2, limit 2 → lines 2 and 3.
    }
}
