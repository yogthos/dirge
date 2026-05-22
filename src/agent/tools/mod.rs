pub(crate) mod apply_patch;
pub(crate) mod background;
mod bash;
pub(crate) mod cache;
pub(crate) mod edit;
mod find_files;
mod glob;
mod grep;
mod list_dir;
#[cfg(feature = "lsp")]
mod lsp;
mod memory;
pub(crate) mod modified;
pub(crate) mod plan;
pub(crate) mod question;
mod read;
#[cfg(feature = "semantic")]
pub mod semantic;
mod skill;
mod task;
mod task_status;
pub(crate) mod todo;
mod webfetch;
mod websearch;
pub(crate) mod write;

pub use apply_patch::ApplyPatchTool;
pub use bash::BashTool;
pub use cache::ToolCache;
pub use edit::EditTool;
pub use find_files::FindFilesTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_dir::ListDirTool;
#[cfg(feature = "lsp")]
pub use lsp::LspTool;
pub use memory::MemoryTool;
pub use plan::{PlanEnterTool, PlanExitTool};
pub use question::QuestionTool;
pub use read::ReadTool;
pub use skill::SkillTool;
pub use task::TaskTool;
pub use task_status::TaskStatusTool;
pub use todo::WriteTodoList;
pub use webfetch::WebFetchTool;
pub use websearch::WebSearchTool;
pub use write::WriteTool;

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::permission::ask::{AskRequest, AskSender, UserDecision};
use crate::permission::checker::{CheckResult, PermCheck};

pub const MAX_GREP_RESULTS: usize = 200;
pub const MAX_FIND_RESULTS: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Msg(String),
}

impl From<io::Error> for ToolError {
    fn from(e: io::Error) -> Self {
        ToolError::Msg(e.to_string())
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError::Msg(e.to_string())
    }
}

pub fn is_skip_dir(name: &str) -> bool {
    matches!(name, "node_modules" | "target")
}

#[derive(Deserialize)]
pub struct ReadArgs {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct EditArgs {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    pub replace_all: Option<bool>,
}

#[derive(Deserialize)]
pub struct BashArgs {
    pub command: String,
    pub timeout: Option<u64>,
}

#[derive(Deserialize)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub include: Option<String>,
    pub context_lines: Option<usize>,
    /// Include dotfiles / hidden files in the search. Default
    /// `false` — F2 carryover from find_files/glob/list_dir: grep
    /// also walks the filesystem and should not silently surface
    /// `.env`, `.git/` internals, etc. by default.
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Deserialize)]
pub struct FindFilesArgs {
    pub pattern: String,
    pub path: Option<String>,
    /// Include dotfiles / hidden files (e.g. `.env`, `.gitignore`).
    /// Default `false` — by default the listing skips hidden files
    /// so secrets in `.env` or `.git/` internals don't get pulled
    /// into LLM context inadvertently. Set `true` when the agent
    /// explicitly needs to inspect dotfiles.
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Deserialize)]
pub struct ListDirArgs {
    pub path: Option<String>,
    /// Include dotfiles in the listing. See `FindFilesArgs::include_hidden`
    /// for the rationale; default `false` for safety.
    #[serde(default)]
    pub include_hidden: bool,
}

async fn handle_ask_inner(
    ask_tx: &AskSender,
    permission: &PermCheck,
    tool: &str,
    input: &str,
) -> Result<(), ToolError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    ask_tx
        .send(AskRequest {
            tool: tool.to_string(),
            input: input.to_string(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| ToolError::Msg("Permission system unavailable".to_string()))?;
    match reply_rx.await {
        Ok(UserDecision::AllowOnce) => Ok(()),
        Ok(UserDecision::AllowAlways(pattern)) => {
            permission
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .add_session_allowlist(tool.to_string(), &pattern);
            Ok(())
        }
        _ => Err(ToolError::Msg("Permission denied by user".to_string())),
    }
}

pub async fn check_perm(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    input_key: &str,
) -> Result<(), ToolError> {
    let Some(perm) = permission else {
        return Ok(());
    };
    let result = {
        let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        guard.check(tool, input_key)
    };
    match result {
        CheckResult::Allowed => Ok(()),
        CheckResult::Denied(reason) => {
            Err(ToolError::Msg(format!("Permission denied: {}", reason)))
        }
        CheckResult::Ask => {
            let Some(tx) = ask_tx else {
                return Err(ToolError::Msg(
                    "Permission denied (non-interactive mode)".to_string(),
                ));
            };
            handle_ask_inner(tx, perm, tool, input_key).await
        }
    }
}

pub async fn check_perm_path(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
) -> Result<(), ToolError> {
    let Some(perm) = permission else {
        return Ok(());
    };
    let result = {
        let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        guard.check_path(tool, path)
    };
    match result {
        CheckResult::Allowed => Ok(()),
        CheckResult::Denied(reason) => {
            Err(ToolError::Msg(format!("Permission denied: {}", reason)))
        }
        CheckResult::Ask => {
            let Some(tx) = ask_tx else {
                return Err(ToolError::Msg(
                    "Permission denied (non-interactive mode)".to_string(),
                ));
            };
            handle_ask_inner(tx, perm, tool, path).await
        }
    }
}

/// Same as `check_perm_path` but additionally returns the canonical
/// path the check resolved to (symlinks followed, `..` normalized).
/// Tools that perform a follow-up file operation (read/edit/write/
/// apply_patch) should pass this canonical path to the file API
/// instead of re-using the original `args.path`. Without this, the
/// OS dereferences the symlink a SECOND time at open, and a swap
/// between check-time and open-time lands the operation on a
/// different file than the one the user authorized (audit H12).
pub async fn check_perm_path_resolve(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
) -> Result<String, ToolError> {
    let Some(perm) = permission else {
        // No permission checker (e.g. ACP path) — pass the path
        // through unchanged. We still want callers to operate on
        // SOME resolved form, but without a checker we can't pin
        // an authoritative canonical path.
        return Ok(path.to_string());
    };
    let (result, resolved) = {
        let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        let resolved = guard.resolve_path_for_tool(path);
        let r = guard.check_path(tool, path);
        (r, resolved)
    };
    match result {
        CheckResult::Allowed => Ok(resolved),
        CheckResult::Denied(reason) => {
            Err(ToolError::Msg(format!("Permission denied: {}", reason)))
        }
        CheckResult::Ask => {
            let Some(tx) = ask_tx else {
                return Err(ToolError::Msg(
                    "Permission denied (non-interactive mode)".to_string(),
                ));
            };
            handle_ask_inner(tx, perm, tool, path).await?;
            Ok(resolved)
        }
    }
}

/// Check whether `candidate` refers to the plan file at `plan_file`.
///
/// Handles relative paths (`PLAN.md`, `./PLAN.md`), absolute paths,
/// and the case where the candidate file doesn't exist yet (the agent
/// is about to create it). Falls back to canonicalizing the parent
/// directory when the file itself can't be resolved.
pub fn is_plan_file(plan_file: &Path, candidate: &str) -> bool {
    let candidate = Path::new(candidate);

    // Canonicalize the candidate: if the file exists, resolve it.
    // If it doesn't (the agent is about to create it), resolve
    // the parent directory and join the file name.
    let resolved = canonicalize_or_parent(candidate);

    // Same for the plan file itself. Normally PLAN.md exists by the
    // time the agent tries to edit it (it was created first), but
    // be defensive in case canonicalize fails.
    let plan_resolved = canonicalize_or_parent(plan_file);

    resolved == plan_resolved
}

/// Canonicalize a path. If the path itself doesn't exist (e.g. a file
/// about to be created), canonicalize its parent directory and join
/// the file name back.
fn canonicalize_or_parent(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        let parent = path.parent().unwrap_or(Path::new("."));
        let file_name = path.file_name().unwrap_or_default();
        std::fs::canonicalize(parent)
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(file_name)
    })
}
