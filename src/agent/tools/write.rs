use std::path::{Path, PathBuf};
#[cfg(feature = "lsp")]
use std::sync::Arc;
#[cfg(feature = "lsp")]
use std::time::{Duration, Instant};

use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{
    AskSender, PermCheck, ToolError, WriteArgs, check_perm_path_resolve, is_plan_file,
};
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
    plan_file: Option<PathBuf>,
    cache: Option<ToolCache>,
    /// When set, the tool touches the file on the LSP server after writing
    /// and appends any resulting diagnostic block to its output. `None`
    /// reproduces the pre-LSP behaviour exactly.
    #[cfg(feature = "lsp")]
    lsp_manager: Option<Arc<LspManager>>,
}

impl WriteTool {
    #[allow(dead_code)]
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        plan_file: Option<PathBuf>,
    ) -> Self {
        WriteTool {
            permission,
            ask_tx,
            plan_file,
            cache: None,
            #[cfg(feature = "lsp")]
            lsp_manager: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        plan_file: Option<PathBuf>,
        cache: ToolCache,
        #[cfg(feature = "lsp")] lsp_manager: Option<Arc<LspManager>>,
    ) -> Self {
        WriteTool {
            permission,
            ask_tx,
            plan_file,
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
            description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                    "content": { "type": "string", "description": "Content to write to the file" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn call(&self, args: WriteArgs) -> Result<String, ToolError> {
        // Audit H12: pin file operations to the canonical path the
        // permission check ran against, so a symlink swap can't
        // redirect the write to an unauthorized target.
        let resolved_path =
            check_perm_path_resolve(&self.permission, &self.ask_tx, "write", &args.path).await?;

        if let Some(plan) = &self.plan_file
            && !is_plan_file(plan, &args.path) {
                return Err(ToolError::Msg(
                    "Plan mode: writes restricted to PLAN.md only. Use /prompt default to exit plan mode."
                        .to_string(),
                ));
            }

        let path = Path::new(&resolved_path);
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
        tokio::fs::write(path, &args.content).await?;
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

        let tool = WriteTool::with_cache(None, None, None, ToolCache::new(), None);
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
        let tool = WriteTool::with_cache(None, None, None, ToolCache::new(), Some(manager));

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
}
