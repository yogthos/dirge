pub(crate) mod apply_patch;
pub(crate) mod background;
mod bash;
pub(crate) mod bg_shell;
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
pub(crate) mod output_relay;
pub(crate) mod plan;
pub(crate) mod question;
mod read;
mod repo_overview;
#[cfg(feature = "semantic")]
pub mod semantic;
mod session_search;
mod skill;
pub mod task;
mod task_status;
pub(crate) mod todo;
pub mod tool_search;
mod webfetch;
mod websearch;
pub(crate) mod write;

pub use apply_patch::ApplyPatchTool;
pub use bash::BashTool;
pub use bg_shell::{BashOutputTool, KillShellTool};
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
pub use repo_overview::RepoOverviewTool;
pub use session_search::SessionSearchTool;
pub use skill::SkillTool;
pub use task::TaskTool;
pub use task_status::TaskStatusTool;
pub use todo::WriteTodoList;
#[allow(unused_imports)]
pub use tool_search::{ALWAYS_ON_TOOLS, TOOL_SEARCH_NAME, ToolMeta, ToolSearchTool};
pub use webfetch::WebFetchTool;
pub use websearch::WebSearchTool;
pub use write::WriteTool;

use std::io;

use serde::Deserialize;

use crate::permission::ask::{AskRequest, AskSender, UserDecision};
use crate::permission::checker::PermCheck;

pub const MAX_GREP_RESULTS: usize = 200;
pub const MAX_FIND_RESULTS: usize = 200;

/// Single source of truth for every built-in tool name dirge ships.
/// Used by:
///   - `agent/builder.rs` MCP collision filter — refuses to register
///     an MCP-exported tool with a colliding name.
///   - `context/prompts.rs` `deny_tools` validation — warns when a
///     prompt's frontmatter names something not in this set.
///
/// Previously these two sites maintained independent lists; review-
/// batch #7 unified them so adding a new tool only requires one edit.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read",
    "write",
    "edit",
    "bash",
    "grep",
    "find_files",
    "glob",
    "list_dir",
    "write_todo_list",
    "apply_patch",
    "memory",
    "skill",
    "task",
    "task_status",
    "bash_output",
    "kill_shell",
    "tool_search",
    "question",
    "webfetch",
    "websearch",
    "lsp",
    "repo_overview",
    "session_search",
    "list_symbols",
    "get_symbol_body",
    "find_definition",
    "find_callers",
    "find_callees",
    // plan_enter / plan_exit are unconditionally added when plan_tx
    // is in scope (they manage the plan mode state via plan_tx). An
    // MCP server exporting either name would shadow them and could
    // disable / hijack plan mode.
    "plan_enter",
    "plan_exit",
    // `mcp_tool` is the umbrella name McpTool calls go through.
    // Including it lets a prompt's `deny_tools: [mcp_tool]` deny
    // every MCP server's tools wholesale; the warn-on-unknown gate
    // in `context/prompts.rs` then accepts that entry.
    "mcp_tool",
];

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

/// Head-truncate `text` to at most `max_bytes` (landing on a UTF-8 char
/// boundary), appending a uniform marker noting how much was dropped.
/// Single source for the per-tool byte caps (dirge-06cp) so the marker
/// is consistent and truncation is never *silent*. Takes ownership and
/// returns the input untouched when it's within the cap (no copy).
/// `what` names the source for the marker (e.g. "bash output").
///
/// NOTE: this is for the in-tool byte ceilings only. The LLM-context cap
/// (head+tail, `compression`), the UI display cap (line-aware), grep's
/// per-line cap, and list_dir's per-item cap are deliberately separate
/// concerns/layers, not folded in here.
pub fn head_cap(text: String, max_bytes: usize, what: &str) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let total = text.len();
    let dropped = total - cut;
    let mut out = text;
    out.truncate(cut);
    out.push_str(&format!(
        "\n…[{what} truncated: dropped {dropped} of {total} bytes; narrow the command (head/grep) to keep context lean]"
    ));
    out
}

/// Extract a required, non-blank string argument for a multiplexer
/// tool's action, with a uniform error message. Replaces the per-action
/// `ok_or_else(|| Msg("X is required for 'Y'"))` checks that memory and
/// skill each hand-rolled with slightly different wording (dirge-8k3k).
///
/// Kept as a call-site helper rather than a schema-driven
/// `validate_and_repair` rule on purpose: a missing field there returns
/// `Err` from the repair layer, which arms model escalation — overkill
/// for a "you forgot a field for this action" error. Same reasoning as
/// [`require_absolute_path`].
pub fn required_nonblank<'a>(
    value: Option<&'a str>,
    field: &str,
    action: &str,
) -> Result<&'a str, ToolError> {
    match value {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(ToolError::Msg(format!(
            "`{field}` is required for action '{action}'"
        ))),
    }
}

/// Enforce that a tool argument is an absolute filesystem path.
///
/// Single source for the check + error message shared by read, write,
/// edit, and apply_patch (dirge-e1r9). These tools all declare
/// `dirge-hints.semantic = "absolute_path"` in their schema and used to
/// each re-implement `Path::is_absolute()` with a slightly different
/// error string. `subject` names the field for the message (e.g.
/// `"read path"`, `"apply_patch rename target"`). Returns the message
/// as a plain `String`; callers wrap it (`.map_err(ToolError::Msg)?`).
pub fn require_absolute_path(path: &str, subject: &str) -> Result<(), String> {
    if std::path::Path::new(path).is_absolute() {
        Ok(())
    } else {
        Err(format!(
            "{subject} must be an absolute path like '/home/user/project/file.txt', \
             not a relative path or bare filename — got {path:?}"
        ))
    }
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
    /// When true, run the command detached: the tool returns immediately
    /// with a shell id and the command's output is delivered later via the
    /// background-completion notification (same channel as background
    /// subagents). Defaults to false (synchronous).
    #[serde(default)]
    pub background: Option<bool>,
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

/// dirge-0g6i: if an `approval_provider` LLM is configured, let it judge
/// an otherwise-`Ask` decision instead of prompting the human. Shared by
/// both [`enforce`] (single scope) and [`enforce_request`] (multi-claim
/// bash) so the evaluation path isn't duplicated. Returns:
///
/// - `Some(Ok(()))` → auto-approved; caller proceeds.
/// - `Some(Err(..))` → auto-denied (with the evaluator's reason).
/// - `None` → no evaluator configured, OR the evaluator call errored →
///   caller falls back to the human prompt (fail-open to the human,
///   never silently allow).
async fn try_auto_approve(
    perm: &PermCheck,
    tool: &str,
    command: &str,
    resources: Vec<String>,
) -> Option<Result<(), ToolError>> {
    use crate::permission::approval::{ApprovalDecision, ApprovalRequest};
    // One lock: pull the evaluator (clone the Arc) + working dir, then
    // drop the lock BEFORE the await so we never hold it across the LLM
    // call. `None` evaluator → caller falls back to the human prompt.
    let (f, working_dir) = {
        let g = perm.lock().unwrap_or_else(|e| e.into_inner());
        match g.approval_fn() {
            Some(f) => (f, g.working_dir().to_string()),
            None => return None,
        }
    };
    let req = ApprovalRequest {
        tool: tool.to_string(),
        command: command.to_string(),
        working_dir,
        resources,
    };
    match f(req).await {
        Ok(ApprovalDecision::Allow) => {
            tracing::info!(target: "dirge::permission", tool, command, "auto-approval: ALLOW");
            Some(Ok(()))
        }
        Ok(ApprovalDecision::Deny(reason)) => {
            tracing::info!(target: "dirge::permission", tool, command, %reason, "auto-approval: DENY");
            Some(Err(ToolError::Msg(format!(
                "Auto-approval denied by approval_provider: {reason}"
            ))))
        }
        Err(e) => {
            tracing::warn!(target: "dirge::permission", error = %e, "approval_provider call failed; falling back to human prompt");
            None
        }
    }
}

/// Scope arg passed to the [`enforce`] chokepoint. Discriminates
/// path-style checks (`Path` / `PathResolve`, route through
/// `PermissionChecker::check_path`, glob with `*` excluding `/`) from
/// raw checks (`Raw`, route through `PermissionChecker::check`, shell-
/// style patterns where `*` matches across `/`).
///
/// `PathResolve` additionally canonicalizes the path (resolving
/// symlinks, normalizing `..`) and returns the resolved path so the
/// calling tool can open EXACTLY the path the user authorized
/// (audit H12 — TOCTOU symlink swap defense).
pub enum Scope<'a> {
    /// Non-path tool input. Examples: a bash command string, an MCP
    /// `server:tool` identifier, a grep pattern, a URL.
    Raw(&'a str),
    /// Filesystem path; check_path-style rule matching.
    Path(&'a str),
    /// Filesystem path with canonical resolution returned in the
    /// `Ok` value of [`enforce`]. Use this from tools that follow
    /// the permission check with a file open (read / write / edit /
    /// apply_patch) — the resolved path pins the file across the
    /// check↔open window.
    PathResolve(&'a str),
}

/// **Single chokepoint for all tool permission decisions in dirge.**
///
/// Ported from maki's `PermissionManager::enforce`
/// (`maki-agent/src/permissions.rs:283-350`): one function, one
/// signature, internal dispatch based on [`Scope`]. The legacy
/// `check_perm` / `check_perm_path` / `check_perm_path_resolve`
/// trio are retained as thin back-compat wrappers that delegate
/// here, so existing call sites continue to compile unchanged.
///
/// Returns the (possibly canonicalized) scope string on success.
/// `Raw` and `Path` scopes echo their input; `PathResolve` returns
/// the canonical path. Callers that don't need the return value
/// can discard with `enforce(...).await?;`.
///
/// Future milestones planning to compose against this chokepoint:
///   - **M2 (dirge-cep)**: replace per-tool `PermissionConfig`
///     fields with a uniform rule schema. `enforce` keeps its
///     signature; only the underlying checker changes.
///   - **M3 (dirge-6ab)**: tree-sitter-parse bash commands inside
///     `enforce` and recurse per-segment so `git diff && rm -rf /`
///     gets BOTH `git` AND `rm` checked. Currently the bash tool
///     does its own segmenting in [`crate::agent::tools::bash`];
///     M3 collapses that into the chokepoint.
///   - **M4 (dirge-ojn)**: flip unmatched-tool default from Allow
///     to Ask. Pure config change inside the underlying checker.
pub async fn enforce(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    scope: Scope<'_>,
) -> Result<String, ToolError> {
    let raw_scope: &str = match &scope {
        Scope::Raw(s) | Scope::Path(s) | Scope::PathResolve(s) => s,
    };

    let Some(perm) = permission else {
        // No checker installed (e.g. ACP / --no-tools paths). Pass
        // through with the original scope text — matches the legacy
        // `check_perm_path_resolve` fallback. Raw/Path callers
        // discard the return; PathResolve callers see the
        // unchanged input.
        return Ok(raw_scope.to_string());
    };

    // M-engine (Phase 2b): route the decision through the unified
    // authorization engine. The old per-tool F2 write↔edit↔apply_patch
    // aliasing is gone — those tools normalize to `Operation::Edit`,
    // so one rule governs the trio by construction. Path-vs-raw is a
    // property of the resource (built in `authorize_scope`), so there
    // is no Scope-dispatched `check`/`check_path` split here.
    let is_path = matches!(scope, Scope::Path(_) | Scope::PathResolve(_));
    let (effect, reason, resolved) = {
        let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        let decision = guard.authorize_scope(tool, raw_scope, is_path);
        // Only PathResolve callers want the canonicalized path back
        // (to pin the file across the check→open window); Raw/Path
        // callers echo their input, matching the legacy contract.
        let resolved = match scope {
            Scope::PathResolve(_) => decision
                .resolved_paths
                .first()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| raw_scope.to_string()),
            _ => raw_scope.to_string(),
        };
        (decision.effect, decision.reason(), resolved)
    };

    use crate::permission::engine::types::Effect;
    match effect {
        Effect::Allow => Ok(resolved),
        Effect::Deny => Err(ToolError::Msg(format!("Permission denied: {reason}"))),
        Effect::Ask => {
            // dirge-0g6i: optional LLM auto-approval before the human prompt.
            if let Some(outcome) = try_auto_approve(perm, tool, raw_scope, Vec::new()).await {
                outcome?; // Deny → propagate; Allow → fall through.
                perm.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .note_allowed_scope(tool, raw_scope, is_path);
                return Ok(resolved);
            }
            let Some(tx) = ask_tx else {
                return Err(ToolError::Msg(
                    "Permission denied (non-interactive mode)".to_string(),
                ));
            };
            handle_ask_inner(tx, perm, tool, raw_scope).await?;
            // Approved → clear the loop-guard counter so a repeated call
            // the user keeps allowing never trips the doom-loop hard-deny
            // (only repeatedly-denied prompts accumulate).
            perm.lock()
                .unwrap_or_else(|e| e.into_inner())
                .note_allowed_scope(tool, raw_scope, is_path);
            Ok(resolved)
        }
    }
}

/// Authorize a pre-built, possibly multi-claim [`AccessRequest`]
/// atomically: ONE decision, at most ONE prompt. This is the entry
/// point for tools (bash) that decompose a single invocation into
/// several claims (command segments + redirect/mutation targets) — the
/// per-resource effects fold most-restrictive-wins, so the whole
/// command is allowed/denied/prompted as a unit instead of gate-by-gate.
///
/// On `Ask`, the single prompt shows the request's `display_input` (the
/// whole command); "allow always" allowlists that command. In-cwd write
/// targets are builtin-allowed and don't re-prompt; external targets are
/// (correctly) re-scrutinized on the next run.
pub async fn enforce_request(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    req: crate::permission::engine::types::AccessRequest,
) -> Result<(), ToolError> {
    use crate::permission::engine::types::Effect;
    let Some(perm) = permission else {
        return Ok(()); // no checker (ACP / --no-tools) → pass through
    };
    let (effect, reason) = {
        let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        let decision = guard.authorize_request(&req);
        (decision.effect, decision.reason())
    };
    match effect {
        Effect::Allow => Ok(()),
        Effect::Deny => Err(ToolError::Msg(format!("Permission denied: {reason}"))),
        Effect::Ask => {
            // dirge-0g6i: optional LLM auto-approval. The evaluator sees a
            // per-claim danger summary (operation + in/out-of-project) so
            // it can judge bash compounds and redirect targets precisely.
            let resources = crate::permission::approval::summarize_claims(&req.claims);
            if let Some(outcome) =
                try_auto_approve(perm, &req.tool, &req.display_input, resources).await
            {
                outcome?; // Deny → propagate; Allow → fall through.
                perm.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .note_allowed_request(&req);
                return Ok(());
            }
            let Some(tx) = ask_tx else {
                return Err(ToolError::Msg(
                    "Permission denied (non-interactive mode)".to_string(),
                ));
            };
            handle_ask_inner(tx, perm, &req.tool, &req.display_input).await?;
            // Approved → clear the loop-guard counter (see `enforce`).
            perm.lock()
                .unwrap_or_else(|e| e.into_inner())
                .note_allowed_request(&req);
            Ok(())
        }
    }
}

/// Back-compat wrapper for the legacy non-path check. Delegates to
/// [`enforce`] with [`Scope::Raw`]. New code should call `enforce`
/// directly.
pub async fn check_perm(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    input_key: &str,
) -> Result<(), ToolError> {
    enforce(permission, ask_tx, tool, Scope::Raw(input_key))
        .await
        .map(|_| ())
}

/// Back-compat wrapper for the legacy path check. Delegates to
/// [`enforce`] with [`Scope::Path`]. New code should call `enforce`
/// directly.
pub async fn check_perm_path(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
) -> Result<(), ToolError> {
    enforce(permission, ask_tx, tool, Scope::Path(path))
        .await
        .map(|_| ())
}

/// Back-compat wrapper for the legacy resolve-and-check entrypoint.
/// Delegates to [`enforce`] with [`Scope::PathResolve`] and returns
/// the canonical path. New code should call `enforce` directly.
///
/// Tools that perform a follow-up file operation (read/edit/write/
/// apply_patch) MUST pass this canonical path to the file API
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
    enforce(permission, ask_tx, tool, Scope::PathResolve(path)).await
}

// `is_plan_file` and `canonicalize_or_parent` were removed when the
// prompt-level PLAN.md gate moved into the permission checker via
// `deny_tools` frontmatter. The few historical callers (WriteTool,
// EditTool, ApplyPatchTool) now drop the file-name comparison and
// rely on the prompt's deny-list to refuse the entire tool in plan
// mode.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{
        Action, OpSpec, PermissionConfig, RuleConfig, SecurityMode, checker::PermissionChecker,
    };
    use std::sync::{Arc, Mutex};

    /// Test helper: build a single op-based rule (tool-agnostic).
    fn rule(op: OpSpec, pattern: &str, effect: Action) -> RuleConfig {
        RuleConfig {
            op,
            pattern: pattern.to_string(),
            effect,
            tool: None,
        }
    }

    // dirge-8k3k: required_nonblank extracts a present, non-blank value
    // or errors with a uniform "`field` is required for action 'x'".
    #[test]
    fn required_nonblank_extracts_or_errors() {
        assert_eq!(
            required_nonblank(Some("hello"), "content", "add").unwrap(),
            "hello"
        );
        for bad in [None, Some(""), Some("   \t")] {
            let msg = required_nonblank(bad, "content", "add")
                .unwrap_err()
                .to_string();
            assert!(msg.contains("content"), "names the field: {msg}");
            assert!(msg.contains("add"), "names the action: {msg}");
        }
    }

    // dirge-06cp: head_cap returns short input untouched and marks any
    // truncation (never silent), landing on a UTF-8 boundary.
    #[test]
    fn head_cap_passes_short_and_marks_truncation() {
        assert_eq!(head_cap("short".to_string(), 100, "x"), "short");

        let capped = head_cap("a".repeat(50), 10, "bash output");
        assert!(capped.starts_with(&"a".repeat(10)), "kept head: {capped}");
        assert!(capped.contains("truncated"), "marked: {capped}");
        assert!(
            capped.contains("dropped 40 of 50 bytes"),
            "counts: {capped}"
        );

        // Multibyte: 'é' is 2 bytes; a cap of 5 must land on a boundary
        // (4 bytes = 2 chars) without panicking or splitting a char.
        let capped = head_cap("é".repeat(10), 5, "x");
        assert!(capped.starts_with("éé"), "boundary-safe head: {capped}");
        assert!(capped.contains("truncated"));
    }

    // dirge-e1r9: the shared absolute-path guard accepts absolute paths
    // and rejects relative / bare ones with a single uniform message.
    #[test]
    fn require_absolute_path_accepts_absolute_rejects_relative() {
        assert!(require_absolute_path("/home/user/x.rs", "read path").is_ok());
        for bad in ["x.rs", "./x.rs", "../x.rs", "src/x.rs", "1"] {
            let err = require_absolute_path(bad, "read path")
                .expect_err("relative path must be rejected");
            assert!(err.contains("absolute path"), "message: {err}");
            assert!(err.contains(bad), "message names the offending path: {err}");
        }
    }

    /// F2 (dirge-jlj): `enforce(write, ...)` MUST also consult the
    /// `edit` rules. A user writing `edit: { "**": "deny" }`
    /// blocks `write` AND `apply_patch` too — matching opencode's
    /// `EDIT_TOOLS` aliasing.
    #[tokio::test]
    async fn enforce_write_aliases_to_edit_deny() {
        let config = PermissionConfig {
            rules: vec![rule(OpSpec::Edit, "**", Action::Deny)],
            ..Default::default()
        };
        let checker = PermissionChecker::new(
            &config,
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/tmp")),
        );
        let perm: PermCheck = Arc::new(Mutex::new(checker));

        let result = enforce(
            &Some(perm.clone()),
            &None,
            "write",
            Scope::PathResolve("/tmp/x.rs"),
        )
        .await;
        assert!(
            matches!(result, Err(_)),
            "edit deny should propagate to write; got {result:?}",
        );

        let result = enforce(
            &Some(perm),
            &None,
            "apply_patch",
            Scope::PathResolve("/tmp/x.rs"),
        )
        .await;
        assert!(
            matches!(result, Err(_)),
            "edit deny should propagate to apply_patch; got {result:?}",
        );
    }

    /// F2: most-restrictive-wins. If `write` is explicitly Allow
    /// but `edit` is Deny, the Deny wins.
    #[tokio::test]
    async fn enforce_write_alias_most_restrictive_wins() {
        // write/edit/apply_patch share Operation::Edit, so both rules
        // live in ONE ordered ruleset (last-match-wins): allow all,
        // then deny /etc/**.
        let config = PermissionConfig {
            rules: vec![
                rule(OpSpec::Edit, "**", Action::Allow),
                rule(OpSpec::Edit, "/etc/**", Action::Deny),
            ],
            ..Default::default()
        };
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm: PermCheck = Arc::new(Mutex::new(checker));

        // `/etc/passwd`: write allows (`**`), edit denies (`/etc/**`).
        // More restrictive (deny) wins.
        let result = enforce(
            &Some(perm.clone()),
            &None,
            "write",
            Scope::PathResolve("/etc/passwd"),
        )
        .await;
        assert!(matches!(result, Err(_)));

        // `/tmp/x.rs`: write/edit/apply_patch now share Operation::Edit,
        // so both rules live in ONE ruleset, last-match-wins. The
        // `write: { "**": allow }` rule (added before the edit deny)
        // matches `/tmp/x.rs`; the `/etc/**` deny does not → Allow.
        // This is the F2 dissolution: "allow all writes except /etc".
        let result = enforce(&Some(perm), &None, "write", Scope::PathResolve("/tmp/x.rs")).await;
        assert!(
            result.is_ok(),
            "/tmp/x.rs: `write **: allow` governs (edit `/etc/**` deny doesn't match) → Allow; got {result:?}",
        );
    }

    /// F2 negative: tools NOT in EDIT_TOOLS aren't aliased.
    /// `read` shouldn't be affected by edit rules.
    #[tokio::test]
    async fn enforce_read_does_not_alias_to_edit() {
        let config = PermissionConfig {
            rules: vec![rule(OpSpec::Edit, "**", Action::Deny)],
            ..Default::default()
        };
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm: PermCheck = Arc::new(Mutex::new(checker));

        // read has builtin-allow `**: allow` → succeeds
        // regardless of edit's deny.
        let result = enforce(
            &Some(perm),
            &None,
            "read",
            Scope::PathResolve("anywhere.rs"),
        )
        .await;
        assert!(
            matches!(result, Ok(_)),
            "read isn't aliased to edit; should pass via builtin-allow; got {result:?}",
        );
    }
}
