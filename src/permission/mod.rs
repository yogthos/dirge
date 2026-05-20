pub mod ask;
pub mod checker;
pub mod pattern;

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolPerm {
    Simple(Action),
    Granular(HashMap<String, Action>),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PermissionConfig {
    #[serde(rename = "*")]
    pub default: Option<Action>,
    pub bash: Option<ToolPerm>,
    pub read: Option<ToolPerm>,
    pub write: Option<ToolPerm>,
    pub edit: Option<ToolPerm>,
    pub grep: Option<ToolPerm>,
    pub find_files: Option<ToolPerm>,
    pub list_dir: Option<ToolPerm>,
    pub write_todo_list: Option<ToolPerm>,
    /// `apply_patch` — bulk multi-file patch tool. Mutates the
    /// filesystem like `write`/`edit`; deserves per-pattern rules.
    pub apply_patch: Option<ToolPerm>,
    /// `lsp` — language-server queries (definition, references,
    /// hover, etc.). Reads project files via the language server.
    pub lsp: Option<ToolPerm>,
    /// `question` — interactive user-input solicitation tool. Per-
    /// pattern rules let users restrict which kinds of questions
    /// the agent can ask.
    pub question: Option<ToolPerm>,
    pub external_directory: Option<HashMap<String, Action>>,
    pub doom_loop: Option<Action>,
}

/// Per-session security mode. Selected via `--yolo` / `--accept-all` /
/// `--restrictive` CLI flags or the `default_permission_mode` config
/// key. Mode precedence (high to low): `Yolo > Accept > Restrictive >
/// Standard`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SecurityMode {
    /// Every rule in `PermissionConfig` is consulted; tools with no
    /// matching rule fall back to the `*` default action.
    Standard,
    /// Like `Standard`, but any tool whose rule resolves to `Allow`
    /// *via the `*` fallback* (no explicit allow rule matched) gets
    /// upgraded to `Ask`. Explicit allow rules still allow; explicit
    /// deny rules still deny. The semantic difference from
    /// `Standard`: "if nothing explicitly approved this, ask the
    /// user." It does NOT flip every Allow to Ask.
    Restrictive,
    /// Auto-allows tools whose targets resolve inside the working
    /// directory; tools touching paths outside `cwd` still consult
    /// `external_directory` rules. Useful for fast iteration on a
    /// trusted project.
    Accept,
    /// Bypasses every check. Use with caution.
    Yolo,
}

impl std::fmt::Display for SecurityMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecurityMode::Standard => write!(f, "standard"),
            SecurityMode::Restrictive => write!(f, "restrictive"),
            SecurityMode::Accept => write!(f, "accept"),
            SecurityMode::Yolo => write!(f, "yolo"),
        }
    }
}

pub fn default_bash_rules() -> Vec<(&'static str, Action)> {
    vec![
        ("ls **", Action::Allow),
        ("cd **", Action::Allow),
        ("pwd", Action::Allow),
        ("echo **", Action::Allow),
        ("which **", Action::Allow),
        ("type **", Action::Allow),
        ("cat **", Action::Allow),
        ("head **", Action::Allow),
        ("tail **", Action::Allow),
        ("wc **", Action::Allow),
        ("sort **", Action::Allow),
        ("uniq **", Action::Allow),
        ("cut **", Action::Allow),
        ("diff **", Action::Allow),
        ("grep **", Action::Allow),
        ("find **", Action::Allow),
        ("git status", Action::Allow),
        ("git log **", Action::Allow),
        ("git diff **", Action::Allow),
        ("git show **", Action::Allow),
        ("git branch **", Action::Allow),
        ("cargo check", Action::Allow),
        ("cargo build", Action::Allow),
        ("cargo test", Action::Allow),
        ("cargo fmt", Action::Allow),
        ("cargo clippy", Action::Allow),
        ("mkdir **", Action::Allow),
        ("touch **", Action::Allow),
        ("npm run **", Action::Allow),
        ("pip list", Action::Allow),
        ("pip show **", Action::Allow),
        ("rm -rf /**", Action::Deny),
        ("sudo rm -rf /**", Action::Deny),
        ("dd **", Action::Deny),
        ("mkfs **", Action::Deny),
        ("fdisk **", Action::Deny),
        ("mkswap **", Action::Deny),
    ]
}
