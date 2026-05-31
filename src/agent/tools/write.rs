use std::path::Path;
#[cfg(feature = "lsp")]
use std::sync::Arc;
#[cfg(feature = "lsp")]
use std::time::{Duration, Instant};

use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::agent_loop::tool_input_repair::with_contract_hint;
use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, PermCheck, ToolError, WriteArgs, check_perm_path_resolve};
#[cfg(feature = "lsp")]
use crate::lsp::diagnostic;
#[cfg(feature = "lsp")]
use crate::lsp::manager::{LspManager, TouchMode};

/// How long to wait for the LSP server to publish fresh diagnostics after
/// a write. Matches opencode's `DIAGNOSTICS_FULL_WAIT_TIMEOUT_MS`. Bounded
/// so a stuck server doesn't hold up the agent's turn.
#[cfg(feature = "lsp")]
const DIAGNOSTIC_WAIT: Duration = Duration::from_secs(10);

pub struct WriteTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    cache: Option<ToolCache>,
    /// When set, the tool touches the file on the LSP server after writing
    /// and appends any resulting diagnostic block to its output. `None`
    /// reproduces the pre-LSP behaviour exactly.
    #[cfg(feature = "lsp")]
    lsp_manager: Option<Arc<LspManager>>,
}

impl WriteTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        WriteTool {
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
        WriteTool {
            permission,
            ask_tx,
            cache: Some(cache),
            #[cfg(feature = "lsp")]
            lsp_manager,
        }
    }
}

impl Tool for WriteTool {
    const NAME: &'static str = "write";

    type Error = ToolError;
    type Args = WriteArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "write".to_string(),
            description: with_contract_hint(
                "write",
                "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The absolute path to the file to write (must be absolute, not relative)" },
                    "content": { "type": "string", "description": "Content to write to the file" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn call(&self, args: WriteArgs) -> Result<String, ToolError> {
        // Reject non-absolute paths immediately with a clear error
        // (shared guard; the schema requires an absolute path).
        // Without it the tool silently resolves "1" to "{cwd}/1" and
        // creates the file, confusing the model into thinking it wrote
        // to a real project path.
        crate::agent::tools::require_absolute_path(&args.path, "the write path")
            .map_err(ToolError::Msg)?;
        // Audit H12: pin file operations to the canonical path the
        // permission check ran against, so a symlink swap can't
        // redirect the write to an unauthorized target.
        let resolved_path =
            check_perm_path_resolve(&self.permission, &self.ask_tx, "write", &args.path).await?;

        let path = Path::new(&resolved_path);
        // Phase-2 tree-sitter validation: refuse to write
        // syntactically-broken code so the model sees the error
        // in the SAME turn and self-corrects. No-op for unknown
        // file types or when no `semantic-<lang>` feature is
        // built. See docs/AGENTIC_LOOP_PLAN.md §2.
        #[cfg(feature = "semantic")]
        if let Err(errors) = crate::semantic::syntax_validator::check_syntax(path, &args.content) {
            return Err(ToolError::Msg(
                crate::semantic::syntax_validator::format_errors(path, &args.content, &errors),
            ));
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = args.content.len();
        // Line count is useful for the LLM to confirm what it wrote
        // landed; cheap to compute on the in-memory string before
        // the write. `lines()` doesn't count a trailing empty line
        // (so "a\nb\n" is 2 lines, not 3) which matches read's
        // counting convention.
        let line_count = args.content.lines().count();
        let was_creation = !path.exists();
        #[cfg(feature = "lsp")]
        let write_at = Instant::now();
        // Atomic write: tmp + fsync + rename so a crash mid-write
        // leaves the previous file content intact, not a truncated
        // half-write. `tokio::fs::write` opens with O_TRUNC and
        // writes in-place — a corruption vector on power loss /
        // OOM-kill / SIGKILL.
        crate::fs_atomic::atomic_write(path, args.content.as_bytes()).await?;
        crate::agent::tools::modified::mark_modified(path);
        // File mutated → invalidate cached reads/greps/listings for this turn.
        if let Some(ref cache) = self.cache {
            cache.clear();
        }

        // Path lives in the chamber banner (`╭─ WRITE ─ "<path>" ─╮`),
        // so don't repeat it. Use the extra room to surface info the
        // LLM finds actionable: bytes, line count, and whether this
        // was a new-file creation vs overwrite. The verb up front
        // disambiguates the two — previously the LLM had to infer
        // creation by reading the surrounding context.
        let verb = if was_creation { "Created" } else { "Wrote" };
        #[allow(unused_mut)]
        let mut output = format!("{} {} bytes ({} lines)", verb, bytes, line_count);
        #[cfg(feature = "lsp")]
        output.push_str(&append_lsp_block(self.lsp_manager.as_ref(), path, write_at).await);
        Ok(output)
    }
}

/// Run `touch_file` + diagnostic-report assembly. Returns the appendable
/// block (empty string when there's nothing to surface or no manager).
/// Errors during touch/wait are intentionally swallowed — diagnostic
/// surfacing is a side-effect; the write tool's primary contract is
/// "wrote the file".
#[cfg(feature = "lsp")]
pub(crate) async fn append_lsp_block(
    manager: Option<&Arc<LspManager>>,
    path: &Path,
    after: Instant,
) -> String {
    let Some(manager) = manager else {
        return String::new();
    };
    manager
        .touch_file(
            path,
            TouchMode::AwaitPush {
                after,
                timeout: DIAGNOSTIC_WAIT,
            },
        )
        .await;
    let diagnostics = manager.all_diagnostics();
    diagnostic::build_report_block(path, &diagnostics)
}

#[cfg(all(test, feature = "lsp"))]
mod tests {
    use super::*;
    use crate::agent::tools::cache::ToolCache;
    use crate::lsp::manager::LspManager;
    use crate::lsp::spawn::{Spawned, Spawner};
    use futures::future::BoxFuture;
    use std::path::PathBuf;

    fn tempfile_in(dir: &Path, name: &str) -> PathBuf {
        dir.join(name)
    }

    /// Synthetic spawner — never actually invoked because the write paths
    /// we test don't have an extension the manager would claim.
    struct NopSpawner;
    impl Spawner for NopSpawner {
        fn spawn<'a>(
            &'a self,
            _server_id: &'a str,
            _root: &'a Path,
        ) -> BoxFuture<'a, std::io::Result<Spawned>> {
            Box::pin(async { Err(std::io::Error::other("not used")) })
        }
    }

    // Regression: when no LSP manager is provided, the tool's output must
    // be exactly what it was pre-LSP (just "Written N bytes to PATH").
    // The diagnostic-append code path must not perturb the no-manager case.
    #[tokio::test]
    async fn regression_no_manager_preserves_existing_output() {
        let dir = std::env::temp_dir().join(format!("dirge-write-no-mgr-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = tempfile_in(&dir, "no-mgr.txt");

        let tool = WriteTool::with_cache(None, None, ToolCache::new(), None);
        let out = tool
            .call(WriteArgs {
                path: path.to_string_lossy().into_owned(),
                content: "hello".into(),
            })
            .await
            .unwrap();
        // Path is in the chamber banner; body starts with the verb +
        // bytes + line count. Use `Created` since the test path
        // didn't exist beforehand. Single-line "hello" content → 1 line.
        assert_eq!(
            out, "Created 5 bytes (1 lines)",
            "unexpected write summary: {out}",
        );
        assert!(!out.contains("LSP errors"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // When a manager IS provided but has no diagnostics (mock spawner that
    // never gets called for the extension), the tool's output still starts
    // with the write confirmation and contains no diagnostic block.
    #[tokio::test]
    async fn manager_with_no_diagnostics_appends_nothing() {
        let dir = std::env::temp_dir().join(format!("dirge-write-with-mgr-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = tempfile_in(&dir, "with-mgr.unknown_ext");

        let manager = Arc::new(LspManager::new(Arc::new(NopSpawner), dir.clone()));
        let tool = WriteTool::with_cache(None, None, ToolCache::new(), Some(manager));

        let out = tool
            .call(WriteArgs {
                path: path.to_string_lossy().into_owned(),
                content: "hi".into(),
            })
            .await
            .unwrap();
        assert!(
            out.starts_with("Created 2 bytes") || out.starts_with("Wrote 2 bytes"),
            "expected `Created`/`Wrote 2 bytes` prefix; got: {out}",
        );
        assert!(!out.contains("LSP errors"), "got: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Non-absolute paths (like "1", "file.txt") must be rejected
    /// immediately with a clear error. Without this guard the tool
    /// silently resolves "1" → "{cwd}/1" and creates the file, which
    /// confuses the model into retrying the same nonsense write.
    #[tokio::test]
    async fn rejects_non_absolute_path() {
        let tool = WriteTool::with_cache(None, None, ToolCache::new(), None);
        for path in ["1", "file.txt", "src/main.rs"] {
            let err = tool
                .call(WriteArgs {
                    path: path.into(),
                    content: "hello".into(),
                })
                .await
                .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("absolute path"),
                "path {path:?}: expected absolute-path rejection; got: {msg}",
            );
        }
    }
}
