pub mod allowlist;
pub mod ask;
pub mod checker;
pub mod engine;
pub mod path;
pub mod pattern;

/// Push the active prompt's `deny_tools` list into the permission
/// checker so subsequent tool calls observe the new restriction.
/// Best-effort: a poisoned mutex falls through to `into_inner`,
/// matching the recovery pattern used elsewhere on the checker.
/// `None` perm (e.g. `--no-tools` builds) is a no-op.
pub fn apply_prompt_deny(perm: &Option<checker::PermCheck>, deny: &[String]) {
    if let Some(p) = perm {
        let mut guard = p.lock().unwrap_or_else(|e| e.into_inner());
        guard.set_prompt_deny_tools(deny.to_vec());
    }
}

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

/// The operation class a rule governs — the resource KIND, not a tool
/// name. `Any` (`"*"`) matches every operation; `tool` on the rule can
/// narrow to a concrete tool when needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpSpec {
    #[default]
    #[serde(rename = "*", alias = "any")]
    Any,
    Read,
    Edit,
    Execute,
    Network,
    Mcp,
    Memory,
    Skill,
    Agent,
    Meta,
}

/// One configured authorization rule. The ordered `rules` list reads
/// top-to-bottom; **last match wins**. Glob style is inferred from the
/// operation (path-style for read/edit, shell-style for execute/etc.).
///
/// ```jsonc
/// { "op": "edit",    "match": "/etc/**",  "effect": "deny" }
/// { "op": "execute", "match": "cargo *",  "effect": "allow" }
/// { "op": "mcp",     "match": "lattice:*", "effect": "allow" }
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    #[serde(default)]
    pub op: OpSpec,
    #[serde(rename = "match")]
    pub pattern: String,
    pub effect: Action,
    /// Optional: narrow the rule to a single concrete tool name (e.g.
    /// `"grep"` so a Read rule doesn't also gate `read`).
    #[serde(default)]
    pub tool: Option<String>,
}

/// Permission configuration: a default effect, an ordered rule list,
/// out-of-project rules, and the loop-guard toggle. Built-in defaults
/// (read-only/memory/skill/dev-null/in-cwd-write allows + the curated
/// safe-bash rules) live in the engine and aren't configured here.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionConfig {
    /// Fallback effect when no rule matches (alias `*`). Defaults to Ask.
    #[serde(rename = "*", alias = "default")]
    pub default: Option<Action>,
    /// Loop-guard control: `"allow"` disables the retry-loop hard-deny;
    /// any other value (default) keeps it on.
    pub doom_loop: Option<Action>,
    /// Ordered authorization rules (last match wins).
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    /// Rules for paths OUTSIDE the working directory (op defaults to
    /// `*`). An out-of-project write with no matching rule prompts.
    #[serde(default)]
    pub external_directory: Vec<RuleConfig>,
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
    // Allow-list ordering / shape — three buckets:
    //   1. Read-only inspection (cat / ls / grep / etc.)
    //   2. Project-scoped dev workflow inside CWD (cargo / git
    //      writes that stay local / make / npm test / language
    //      runners). Same trust model as the CWD-scoped write/edit
    //      allow installed in `checker.rs:install_cwd_allow_rules`:
    //      if you trust the agent to edit project files, running
    //      project code is the same trust level.
    //   3. Filesystem mutators (mkdir / touch / mv / cp) — they
    //      ALSO route their path arguments through the `write` rules
    //      via `extract_mutation_paths`, so the CWD-allow on write
    //      still gates the actual filesystem destination.
    //
    // Patterns use `**` (any chars including `/`) instead of exact
    // match because every prior exact pattern (`cargo build`,
    // `git status`, etc.) silently re-prompted on common flagged
    // invocations like `cargo build --release` or `git status -s` —
    // friction that drove the "permissions are too aggressive"
    // complaint.
    //
    // Intentionally NOT auto-allowed:
    //   - `git push **`           — side effect outside the project
    //   - `git rebase/reset/stash`— destructive, can lose work
    //   - `npm install **`, `pip install **` — executes install
    //     scripts as arbitrary code outside the repo tree
    //   - `sudo **`               — privilege escalation always asks
    //   - `curl/wget`             — network egress always asks
    vec![
        // Read-only inspection
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
        ("rg **", Action::Allow),
        ("find **", Action::Allow),
        ("file **", Action::Allow),
        ("stat **", Action::Allow),
        ("env", Action::Allow),
        ("date **", Action::Allow),
        ("whoami", Action::Allow),
        ("hostname", Action::Allow),
        // Git — local read/write inside the repo
        ("git status **", Action::Allow),
        ("git log **", Action::Allow),
        ("git diff **", Action::Allow),
        ("git show **", Action::Allow),
        ("git branch **", Action::Allow),
        ("git add **", Action::Allow),
        ("git commit **", Action::Allow),
        ("git checkout **", Action::Allow),
        ("git switch **", Action::Allow),
        ("git pull **", Action::Allow),
        ("git fetch **", Action::Allow),
        ("git remote **", Action::Allow),
        ("git tag **", Action::Allow),
        ("git blame **", Action::Allow),
        ("git restore **", Action::Allow),
        ("git rev-parse **", Action::Allow),
        ("git rev-list **", Action::Allow),
        ("git ls-files **", Action::Allow),
        ("git config --get **", Action::Allow),
        // Rust toolchain
        ("cargo check **", Action::Allow),
        ("cargo build **", Action::Allow),
        ("cargo test **", Action::Allow),
        ("cargo fmt **", Action::Allow),
        ("cargo clippy **", Action::Allow),
        ("cargo run **", Action::Allow),
        ("cargo doc **", Action::Allow),
        ("cargo tree **", Action::Allow),
        ("cargo metadata **", Action::Allow),
        ("rustc --version", Action::Allow),
        // Filesystem mutators — path args still route through
        // `write` rules via `extract_mutation_paths` (F1 dirge-dvy),
        // so the CWD-allow on write still gates the destination.
        ("mkdir **", Action::Allow),
        ("touch **", Action::Allow),
        ("mv **", Action::Allow),
        ("cp **", Action::Allow),
        ("ln **", Action::Allow),
        ("chmod **", Action::Allow),
        // Node / npm / yarn / pnpm — runners (NOT installers)
        ("npm test **", Action::Allow),
        ("npm run **", Action::Allow),
        ("npm ls **", Action::Allow),
        ("npx **", Action::Allow),
        ("node **", Action::Allow),
        ("yarn run **", Action::Allow),
        ("pnpm run **", Action::Allow),
        // Python — runners + read-only pip
        ("python **", Action::Allow),
        ("python3 **", Action::Allow),
        ("pytest **", Action::Allow),
        ("ruff **", Action::Allow),
        ("black **", Action::Allow),
        ("mypy **", Action::Allow),
        ("pip list **", Action::Allow),
        ("pip show **", Action::Allow),
        ("pip freeze", Action::Allow),
        // Go
        ("go build **", Action::Allow),
        ("go test **", Action::Allow),
        ("go run **", Action::Allow),
        ("go fmt **", Action::Allow),
        ("go vet **", Action::Allow),
        ("go mod **", Action::Allow),
        // Make + general task runners
        ("make **", Action::Allow),
        ("just **", Action::Allow),
        // Hard denies — destructive system-level operations
        ("rm -rf /**", Action::Deny),
        ("sudo rm -rf /**", Action::Deny),
        ("dd **", Action::Deny),
        ("mkfs **", Action::Deny),
        ("fdisk **", Action::Deny),
        ("mkswap **", Action::Deny),
    ]
}
