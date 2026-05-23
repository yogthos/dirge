use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::permission::pattern::Pattern;
use crate::permission::{Action, PermissionConfig, SecurityMode, ToolPerm};

pub type PermCheck = Arc<Mutex<PermissionChecker>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    Allowed,
    Ask,
    Denied(String),
}

pub struct PermissionChecker {
    rules: HashMap<String, Vec<(Pattern, Action)>>,
    default_action: Action,
    ext_dir_rules: Vec<(Pattern, Action)>,
    doom_loop_action: Action,
    working_dir: String,
    /// Cached canonical form of `working_dir`, computed once at
    /// construction (and refreshed by `set_working_dir`). Used by
    /// `is_external_path` to compare canonical paths without
    /// hitting the filesystem on every permission check — the
    /// canonicalize syscall is otherwise called once per
    /// read/write/edit/grep call, accumulating to hundreds of
    /// stat()s per session.
    working_dir_canonical: String,
    session_allowlist: Vec<(String, Pattern)>,
    recent_calls: VecDeque<(String, String)>,
    mode: SecurityMode,
    /// Tools denied by the currently-active prompt's frontmatter
    /// `deny_tools` list. Enforced at the top of every `check` /
    /// `check_path` call — even before Yolo mode's blanket allow.
    /// This is the permission-layer enforcement of plan/review/etc.
    /// modes; previously plan mode relied on prose ("don't write
    /// code") + inline `is_plan_file` gates in edit/write/apply_patch,
    /// which an adversarial / confused LLM could route around via
    /// `bash` or by bypassing the gate name-check.
    ///
    /// Updated by `set_prompt_deny_tools` whenever the active prompt
    /// changes (slash `/prompt <name>`, session load, startup). Empty
    /// when no prompt is active or the active prompt has no
    /// frontmatter.
    prompt_deny_tools: Vec<String>,
}

/// Tools that execute external code with broad effects. Accept mode
/// does NOT coerce `Ask → Allow` for these — the "I trust the agent
/// inside cwd" rationale that justifies the coercion for other
/// non-path tools doesn't generalize to shell + MCP servers.
fn is_high_risk_non_path_tool(tool: &str) -> bool {
    matches!(tool, "mcp_tool" | "bash")
}

/// Tool names where the input is a filesystem path. For these, `*` keeps
/// classic glob semantics (one segment, doesn't cross `/`). Everything else
/// is treated as shell/text where `*` means "any chars including /".
pub(crate) fn is_path_tool_name(tool: &str) -> bool {
    matches!(
        tool,
        "read"
            | "write"
            | "edit"
            | "list_dir"
            | "apply_patch"
            | "lsp"
            // grep / find_files / glob now also receive path-side
            // checks (the search-root path), so their rules use
            // path-glob semantics.
            | "grep"
            | "find_files"
            | "glob"
            // Semantic tools whose primary arg is a file path.
            | "list_symbols"
            | "get_symbol_body"
            | "find_callees"
            // #1 fix: repo_overview's arg is a directory path; user
            // rules like `"/etc/**": "deny"` need path-glob semantics
            // for `**` to span subpaths. Was missed when the tool
            // was added.
            | "repo_overview"
    )
}

/// Build a Pattern with the right `*` semantics for the given tool.
pub(crate) fn pattern_for_tool(tool: &str, pat: &str) -> Pattern {
    if is_path_tool_name(tool) {
        Pattern::new(pat)
    } else {
        Pattern::new_command(pat)
    }
}

impl PermissionChecker {
    pub fn new(
        config: &PermissionConfig,
        mode: SecurityMode,
        working_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let default_action = config.default.unwrap_or(Action::Allow);
        let doom_loop_action = config.doom_loop.unwrap_or(Action::Ask);

        let mut rules: HashMap<String, Vec<(Pattern, Action)>> = HashMap::new();
        for (tool_name, tool_perm) in [
            ("bash", &config.bash),
            ("read", &config.read),
            ("write", &config.write),
            ("edit", &config.edit),
            ("grep", &config.grep),
            ("find_files", &config.find_files),
            ("list_dir", &config.list_dir),
            // Adversarial-review #5 added; both are read-only walkers.
            ("glob", &config.glob),
            ("repo_overview", &config.repo_overview),
            ("write_todo_list", &config.write_todo_list),
            ("apply_patch", &config.apply_patch),
            ("lsp", &config.lsp),
            ("question", &config.question),
            // Newly-configurable tools (previously the perm checker
            // had no rules for them, so they always fell through to
            // the `*` default and couldn't be individually gated).
            ("webfetch", &config.webfetch),
            ("websearch", &config.websearch),
            ("task", &config.task),
            ("task_status", &config.task_status),
            ("memory", &config.memory),
            ("skill", &config.skill),
            ("list_symbols", &config.list_symbols),
            ("get_symbol_body", &config.get_symbol_body),
            ("find_definition", &config.find_definition),
            ("find_callers", &config.find_callers),
            ("find_callees", &config.find_callees),
            ("mcp_tool", &config.mcp_tool),
        ] {
            let Some(tp) = tool_perm else { continue };
            let mut entries = Vec::new();
            match tp {
                ToolPerm::Simple(action) => {
                    entries.push((pattern_for_tool(tool_name, "*"), *action));
                }
                ToolPerm::Granular(map) => {
                    for (pat, action) in map {
                        entries.push((pattern_for_tool(tool_name, pat), *action));
                    }
                }
            }
            rules.insert(tool_name.to_string(), entries);
        }

        // M2 (dirge-cep): merge the unified `tools` map. New configs
        // can declare rules for ANY tool name (including plugin / MCP
        // / future tools) without extending `PermissionConfig`. If a
        // tool is named in both the legacy per-field surface and the
        // `tools` map, the map wins — it's the explicit, newer shape
        // and the migration path is "move per-tool fields into
        // tools". `mcp_tool` (and the other umbrella names) take the
        // same syntax: `tools: { mcp_tool: { "mcp_tool:fs:*": "deny" } }`.
        if let Some(tools_map) = &config.tools {
            for (tool_name, tp) in tools_map {
                let mut entries = Vec::new();
                match tp {
                    ToolPerm::Simple(action) => {
                        entries.push((pattern_for_tool(tool_name, "*"), *action));
                    }
                    ToolPerm::Granular(map) => {
                        for (pat, action) in map {
                            entries.push((pattern_for_tool(tool_name, pat), *action));
                        }
                    }
                }
                rules.insert(tool_name.clone(), entries);
            }
        }

        if !rules.contains_key("bash") {
            let mut defaults = Vec::new();
            for (pat, action) in crate::permission::default_bash_rules() {
                defaults.push((pattern_for_tool("bash", pat), action));
            }
            rules.insert("bash".to_string(), defaults);
        }

        // MCP tools execute external code (the MCP server's
        // implementation, plus whatever effects the server has on
        // the filesystem / network / API services). The previous
        // default was the inherited `default_action` (Allow) since
        // `mcp_tool` had no rule installed; that let an entire
        // sequence of MCP calls execute silently, with only the
        // doom-loop detector eventually prompting on the 3rd
        // identical call. User reported running through several
        // MCP queries without ever being asked. Install a default
        // `Ask` rule when no explicit config exists. Users who
        // trust a specific MCP server can pin it with config:
        //
        //   "permission": {
        //     "mcp_tool": {
        //       "mcp_tool:lattice:*": "allow"
        //     }
        //   }
        //
        // …or accept once and pick "allow always" for the same
        // effect via the session allowlist.
        if !rules.contains_key("mcp_tool") {
            rules.insert(
                "mcp_tool".to_string(),
                vec![(pattern_for_tool("mcp_tool", "*"), Action::Ask)],
            );
        }

        // External-directory rules are always path patterns by definition.
        let ext_dir_rules = config
            .external_directory
            .as_ref()
            .map(|map| {
                map.iter()
                    .map(|(pat, action)| (Pattern::new(pat), *action))
                    .collect()
            })
            .unwrap_or_default();

        let working_dir = working_dir
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
            .to_string_lossy()
            .to_string();
        let working_dir_canonical = canonicalize_for_cache(&working_dir);

        PermissionChecker {
            rules,
            default_action,
            ext_dir_rules,
            doom_loop_action,
            working_dir,
            working_dir_canonical,
            session_allowlist: Vec::new(),
            recent_calls: VecDeque::with_capacity(16),
            mode,
            prompt_deny_tools: Vec::new(),
        }
    }

    /// Install the current prompt's deny-list. Called when the
    /// active prompt changes (startup, session load, `/prompt
    /// <name>`); pass an empty vec to clear.
    pub fn set_prompt_deny_tools(&mut self, denied: Vec<String>) {
        self.prompt_deny_tools = denied;
    }

    /// Returns true when `tool` is in the active prompt's
    /// `deny_tools` frontmatter list. Internal helper so both
    /// `check` and `check_path` share the same gate. Case-insensitive
    /// match (#7 fix): `deny_tools: [Edit]` correctly denies `edit`.
    fn is_prompt_denied(&self, tool: &str) -> bool {
        self.prompt_deny_tools
            .iter()
            .any(|t| t.eq_ignore_ascii_case(tool))
    }

    /// Public deny-list probe, used by code paths that route through
    /// `check_perm` with a UMBRELLA tool name (e.g. MCP tools always
    /// pass `"mcp_tool"`) and need to additionally check the
    /// CONCRETE name the LLM would think of (e.g. an MCP-exported
    /// `edit` should be blocked if the active prompt denies `edit`).
    /// Returns true if ANY of the supplied names hits the deny-list.
    pub fn any_prompt_denied(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.is_prompt_denied(n))
    }

    pub fn check(&mut self, tool: &str, input: &str) -> CheckResult {
        // Prompt-level deny list runs BEFORE every other gate,
        // including Yolo mode's blanket allow. This is the
        // permission-layer enforcement of plan/review/etc. modes:
        // the prompt's frontmatter declares which tools that mode
        // CANNOT use (e.g. plan mode denies edit/write/apply_patch/
        // bash), and the LLM gets a hard refusal instead of relying
        // on the prompt prose to dissuade it from calling. Yolo is
        // still "no rule-set, all calls allowed" but a prompt's
        // deny-list is a stronger contract — the user opted into
        // this mode, so we honor it even under Yolo.
        if self.is_prompt_denied(tool) {
            return CheckResult::Denied(format!(
                "Tool {tool:?} is denied by the active prompt's `deny_tools` frontmatter. Switch with `/prompt <other>` to use it."
            ));
        }
        if self.mode == SecurityMode::Yolo {
            return CheckResult::Allowed;
        }

        if self.is_session_allowed(tool, input) {
            return CheckResult::Allowed;
        }

        // Track both the action AND the matching pattern so denial
        // messages can name which rule blocked the call (was just
        // "Blocked by permission rules", giving the user no way to
        // identify and edit the offending rule).
        let mut matched: Vec<(Action, String)> = Vec::new();
        if let Some(rules) = self.rules.get(tool) {
            for (pattern, action) in rules {
                if pattern.matches(input) {
                    matched.push((*action, pattern.original.clone()));
                }
            }
        }

        let base = matched
            .last()
            .map(|(a, _)| *a)
            .unwrap_or(self.default_action);
        let last_pat = matched.last().map(|(_, p)| p.clone());
        let action = match self.mode {
            SecurityMode::Restrictive => {
                if matched.is_empty() && self.default_action == Action::Allow {
                    Action::Ask
                } else {
                    base
                }
            }
            SecurityMode::Accept => match base {
                Action::Ask => {
                    if self.is_path_tool(tool) && self.is_external_path(input) {
                        self.match_ext_dir(input).unwrap_or(Action::Ask)
                    } else if is_high_risk_non_path_tool(tool) {
                        // Accept mode coerces Ask → Allow for non-path
                        // tools on the assumption that "trust the
                        // agent inside cwd" generalizes. That breaks
                        // for tools that execute external code with
                        // arbitrary effects: MCP servers run third-
                        // party code; `bash` runs shell. Keep the Ask
                        // for these specifically. Review #1.
                        Action::Ask
                    } else {
                        Action::Allow
                    }
                }
                other => other,
            },
            SecurityMode::Standard => base,
            SecurityMode::Yolo => unreachable!(),
        };

        if action != Action::Deny {
            self.track_doom_loop(tool, input);
            if self.is_doom_loop(tool, input) {
                match self.doom_loop_action {
                    Action::Deny => {
                        // Name the call so the user can identify and
                        // either fix the LLM's behavior or relax the
                        // pattern.
                        let preview: String = input.chars().take(60).collect();
                        return CheckResult::Denied(format!(
                            "Doom loop: repeated identical {} call ({}{})",
                            tool,
                            preview,
                            if input.chars().count() > 60 {
                                "…"
                            } else {
                                ""
                            },
                        ));
                    }
                    Action::Ask => return CheckResult::Ask,
                    Action::Allow => {}
                }
            }
        }

        match action {
            Action::Allow => CheckResult::Allowed,
            Action::Ask => CheckResult::Ask,
            Action::Deny => CheckResult::Denied(match last_pat {
                Some(pat) => format!("Blocked by rule: {tool} {pat:?} → deny"),
                None => format!("Blocked: {tool} denied by default action"),
            }),
        }
    }

    pub fn check_path(&mut self, tool: &str, path: &str) -> CheckResult {
        // Prompt deny-list runs first, same reasoning as `check`.
        if self.is_prompt_denied(tool) {
            return CheckResult::Denied(format!(
                "Tool {tool:?} is denied by the active prompt's `deny_tools` frontmatter. Switch with `/prompt <other>` to use it."
            ));
        }
        if self.mode == SecurityMode::Yolo {
            return CheckResult::Allowed;
        }

        if self.is_session_allowed(tool, path) {
            return CheckResult::Allowed;
        }

        let abs_path = resolve_absolute(path, &self.working_dir);
        let mut matched: Vec<(Action, String)> = Vec::new();
        if let Some(rules) = self.rules.get(tool) {
            for (pattern, action) in rules {
                if pattern.matches(&abs_path) || pattern.matches(path) {
                    matched.push((*action, pattern.original.clone()));
                }
            }
        }

        let base = matched
            .last()
            .map(|(a, _)| *a)
            .unwrap_or(self.default_action);
        let last_pat = matched.last().map(|(_, p)| p.clone());

        // Audit H9: `external_directory` rules used to fire only in
        // `SecurityMode::Accept`. A user who configured
        // `external_directory = { "/external/safe/**" = "allow" }`
        // saw the rule silently ignored under Standard/Restrictive.
        // Pre-compute the overlay so each mode can opt into it
        // uniformly.
        let is_external = self.is_external_path(&abs_path);
        let ext_dir_action = if is_external {
            self.match_ext_dir(&abs_path)
        } else {
            None
        };

        let action = match self.mode {
            SecurityMode::Restrictive => {
                if let Some(a) = ext_dir_action {
                    a
                } else if matched.is_empty() && self.default_action == Action::Allow {
                    Action::Ask
                } else {
                    base
                }
            }
            SecurityMode::Accept => match base {
                Action::Ask => {
                    if is_external {
                        ext_dir_action.unwrap_or(Action::Ask)
                    } else {
                        Action::Allow
                    }
                }
                other => other,
            },
            SecurityMode::Standard => {
                // Explicit ext_dir rule overrides the base for external
                // paths. For non-external paths (or external paths
                // without a matching ext_dir rule) keep the prior
                // base-action behavior — the catch-all below will
                // demote unmatched external Allows to Ask.
                if let Some(a) = ext_dir_action {
                    a
                } else {
                    base
                }
            }
            SecurityMode::Yolo => unreachable!(),
        };

        let action = if matched.is_empty()
            && action == Action::Allow
            && is_external
            && ext_dir_action.is_none()
        {
            Action::Ask
        } else {
            action
        };

        if action != Action::Deny {
            self.track_doom_loop(tool, path);
            if self.is_doom_loop(tool, path) {
                match self.doom_loop_action {
                    Action::Deny => {
                        let preview: String = path.chars().take(80).collect();
                        return CheckResult::Denied(format!(
                            "Doom loop: repeated identical {} call ({}{})",
                            tool,
                            preview,
                            if path.chars().count() > 80 { "…" } else { "" },
                        ));
                    }
                    Action::Ask => return CheckResult::Ask,
                    Action::Allow => {}
                }
            }
        }

        match action {
            Action::Allow => CheckResult::Allowed,
            Action::Ask => CheckResult::Ask,
            Action::Deny => CheckResult::Denied(match last_pat {
                Some(pat) => format!("Blocked by rule: {tool} {pat:?} → deny"),
                None => format!("Blocked: {tool} denied by default action"),
            }),
        }
    }

    fn is_session_allowed(&self, tool: &str, input: &str) -> bool {
        for (allowed_tool, pattern) in &self.session_allowlist {
            if allowed_tool == tool && pattern.matches(input) {
                return true;
            }
        }
        false
    }

    pub fn add_session_allowlist(&mut self, tool: String, pattern_str: &str) {
        // Dedup against existing (tool, pattern.original) so
        // repeated "allow always" picks for the same command don't
        // accumulate identical entries. Cheap O(N) check — N here
        // is per-session and typically dozens at most.
        if self
            .session_allowlist
            .iter()
            .any(|(t, p)| t == &tool && p.original == pattern_str)
        {
            return;
        }
        let pattern = pattern_for_tool(&tool, pattern_str);
        self.session_allowlist.push((tool, pattern));
    }

    pub fn load_session_allowlist(&mut self, entries: &[(String, String)]) {
        // Route through `add_session_allowlist` so the same dedup
        // applies on load. Prevents duplicate entries either from a
        // malformed session file or from a host that loads twice.
        for (tool, pat) in entries {
            self.add_session_allowlist(tool.clone(), pat);
        }
    }

    pub fn allowlist_entries(&self) -> Vec<(String, String)> {
        self.session_allowlist
            .iter()
            .map(|(t, p)| (t.clone(), p.original.clone()))
            .collect()
    }

    /// Remove the allowlist entry at the given index (0-based,
    /// matching the display order in `/allow list`). Returns the
    /// removed `(tool, pattern)` on success, or `None` if the
    /// index is out of range. Used by `/allow remove <n>`.
    pub fn remove_session_allowlist_at(&mut self, idx: usize) -> Option<(String, String)> {
        if idx >= self.session_allowlist.len() {
            return None;
        }
        let (tool, pat) = self.session_allowlist.remove(idx);
        Some((tool, pat.original.clone()))
    }

    /// Remove ALL allowlist entries. Used by `/allow clear`.
    pub fn clear_session_allowlist(&mut self) {
        self.session_allowlist.clear();
    }

    pub fn set_mode(&mut self, mode: SecurityMode) {
        self.mode = mode;
    }

    /// Resolve a possibly-relative, possibly-symlinked path to its
    /// canonical form using the checker's own working_dir.
    /// Exposes `resolve_absolute` to callers that need the same
    /// canonical path the check ran against (audit H12 — pass this
    /// to `File::open` instead of the raw `args.path` to close the
    /// symlink-swap TOCTOU between check and open).
    pub fn resolve_path_for_tool(&self, path: &str) -> String {
        resolve_absolute(path, &self.working_dir)
    }

    /// Count of explicit `Deny` rules across all tools + the
    /// external-directory ruleset. Used by the host to warn the user
    /// when Yolo mode is active alongside non-empty deny rules —
    /// Yolo unconditionally returns `Allowed` before any rule
    /// lookup, so those deny rules are silently inert (audit H11).
    pub fn deny_rule_count(&self) -> usize {
        let in_tool_rules: usize = self
            .rules
            .values()
            .map(|v| v.iter().filter(|(_, a)| *a == Action::Deny).count())
            .sum();
        let in_ext_dir = self
            .ext_dir_rules
            .iter()
            .filter(|(_, a)| *a == Action::Deny)
            .count();
        in_tool_rules + in_ext_dir
    }

    pub fn mode(&self) -> SecurityMode {
        self.mode
    }

    pub fn set_working_dir(&mut self, dir: &str) {
        self.working_dir = dir.to_string();
        self.working_dir_canonical = canonicalize_for_cache(dir);
        // B3-5 (audit fix): clear session-scoped state that was
        // implicitly tied to the OLD cwd. Two concerns:
        //   1. `recent_calls` is the doom-loop counter — stale
        //      entries from before the cd would falsely trip the
        //      3-identical-calls limiter on the first calls in
        //      the new project.
        //   2. `session_allowlist` holds patterns the user
        //      approved for the prior project (e.g. `cd *`,
        //      `cargo *`). Carrying them silently to a new
        //      project means the user has implicitly granted
        //      those permissions there too — a privilege carry-
        //      over the audit flagged. Pi rebuilds the session
        //      on cwd change.
        self.recent_calls.clear();
        self.session_allowlist.clear();
    }

    fn is_path_tool(&self, tool: &str) -> bool {
        // Must match `is_path_tool_name` — these are the tools that
        // take a filesystem path as their permission input and need
        // `external_directory` rule consultation. `apply_patch` and
        // `lsp` are included because both route filesystem-path
        // strings through `check_perm_path`.
        is_path_tool_name(tool)
    }

    fn is_external_path(&self, path_str: &str) -> bool {
        // F18: previously `!is_absolute → return false`, which
        // treated `../../etc/passwd` as "internal" (not external).
        // In Accept mode that bypassed external_directory rules:
        // a relative `../../secret` would auto-allow because it
        // wasn't classified external. Now we resolve relative
        // paths against the working_dir (same logic as
        // `resolve_absolute`) before the starts_with check.
        let resolved = resolve_absolute(path_str, &self.working_dir);
        let p = Path::new(&resolved);
        if !p.is_absolute() {
            // resolve_absolute fell back to lexical join and the
            // result is still relative — usually means working_dir
            // itself is bogus. Treat as not-external; rules will
            // fall through to the default action.
            return false;
        }
        let cwd = Path::new(&self.working_dir);
        // Canonical cwd is precomputed (see `working_dir_canonical`).
        // Comparing against BOTH the canonical and literal forms
        // handles symlinked roots like macOS's `/tmp → /private/tmp`:
        // `resolved` is canonical (`/private/tmp/...`) but `cwd`
        // may still be the literal `/tmp` form. Without both checks
        // every in-tree access in such a setup would classify as
        // external.
        let canonical_cwd = Path::new(&self.working_dir_canonical);
        !p.starts_with(canonical_cwd) && !p.starts_with(cwd)
    }

    fn match_ext_dir(&self, path_str: &str) -> Option<Action> {
        for (pattern, action) in &self.ext_dir_rules {
            if pattern.matches(path_str) {
                return Some(*action);
            }
        }
        None
    }

    fn track_doom_loop(&mut self, tool: &str, input: &str) {
        self.recent_calls
            .push_back((tool.to_string(), input.to_string()));
        if self.recent_calls.len() > 16 {
            self.recent_calls.pop_front();
        }
    }

    fn is_doom_loop(&self, tool: &str, input: &str) -> bool {
        let count = self
            .recent_calls
            .iter()
            .filter(|(t, i)| t == tool && i == input)
            .count();
        count >= 3
    }
}

/// One-shot canonicalize for the working-directory cache. Best
/// effort: if canonicalize fails (cwd doesn't exist on disk, e.g.
/// in tests that pass a fixture path), fall back to the literal
/// string so the `starts_with` comparisons in `is_external_path`
/// still work for the literal form.
fn canonicalize_for_cache(working_dir: &str) -> String {
    std::fs::canonicalize(working_dir)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| working_dir.to_string())
}

fn resolve_absolute(path: &str, working_dir: &str) -> String {
    let p = Path::new(path);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(working_dir).join(p)
    };
    // F7: canonicalize so symlinks resolve to their real target.
    // Without this, a symlink like `safe_link -> /etc/passwd` would
    // be checked against rules as `safe_link`, bypassing any
    // `/etc/**` deny / external-directory rule. opencode handles
    // this implicitly via TypeScript fs APIs that follow links;
    // Rust requires the explicit call.
    //
    // Fallback: if canonicalize fails (path doesn't exist yet —
    // e.g. a write to a new file), normalize `.` / `..` lexically
    // and return the joined path as-is. The non-existence is
    // intentional for write ops; canonicalize() would return
    // NotFound and we'd lose the path entirely.
    match std::fs::canonicalize(&joined) {
        Ok(canonical) => canonical.to_string_lossy().to_string(),
        Err(_) => {
            // The path doesn't exist (write to new file, parent
            // also missing, etc.). Try canonicalize on the parent
            // then re-append the basename — catches
            // `/safe/parent/../../etc/passwd` style attacks where
            // the parent exists but the leaf doesn't. If even the
            // parent doesn't canonicalize, fall back to the
            // LEXICALLY-NORMALIZED join. We do NOT return the raw
            // lexical form: `Path::starts_with` matches by
            // components, and a string like
            // `/cwd/nonexistent/../../etc/passwd` whose first three
            // components are `/cwd` would classify as internal even
            // though the path actually escapes via `..`. Normalize
            // `.` / `..` lexically first so the starts_with check
            // sees the real prefix.
            if let (Some(parent), Some(name)) = (joined.parent(), joined.file_name())
                && let Ok(canonical_parent) = std::fs::canonicalize(parent)
            {
                return canonical_parent.join(name).to_string_lossy().to_string();
            }
            lexical_normalize(&joined).to_string_lossy().to_string()
        }
    }
}

/// Resolve `.` and `..` components of `p` without touching the
/// filesystem. `..` pops the previous `Normal` component; consecutive
/// `..` at the start (i.e. attempting to climb above root) are
/// retained as `..` so an attacker can't disguise an escape by
/// chaining enough `..` to underflow a real-path prefix check.
/// Doesn't follow symlinks — callers that need symlink resolution
/// should use `std::fs::canonicalize`; this helper exists for the
/// nonexistent-path fallback where canonicalize is impossible.
fn lexical_normalize(p: &Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    // No Normal to pop (we're at root, or only
                    // RootDir / Prefix / leading `..` so far) —
                    // keep the `..` so the result reflects the
                    // escape attempt rather than being silently
                    // swallowed.
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    let mut buf = PathBuf::new();
    for c in &out {
        buf.push(c.as_os_str());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionConfig;

    fn fresh_checker() -> PermissionChecker {
        PermissionChecker::new(
            &PermissionConfig::default(),
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/tmp")),
        )
    }

    /// Prompt-level deny list refuses the named tool before any
    /// rule matching, in every security mode. This is the
    /// permission-layer enforcement of plan/review modes
    /// (replaces the prompt-text-only "don't write code"
    /// restriction). Even Yolo respects the deny list — the user
    /// opted into the mode, that's a stronger contract than the
    /// security mode's blanket allow.
    #[test]
    fn prompt_deny_tools_refuses_listed_tool_in_every_mode() {
        for mode in [
            SecurityMode::Standard,
            SecurityMode::Accept,
            SecurityMode::Restrictive,
            SecurityMode::Yolo,
        ] {
            let mut checker = PermissionChecker::new(
                &PermissionConfig::default(),
                mode,
                Some(std::path::PathBuf::from("/tmp")),
            );
            checker.set_prompt_deny_tools(vec!["edit".to_string(), "write".to_string()]);
            assert!(
                matches!(checker.check("edit", "/tmp/foo"), CheckResult::Denied(_)),
                "edit must be denied in mode {:?} when prompt deny-list includes it",
                mode,
            );
            assert!(
                matches!(checker.check("write", "/tmp/foo"), CheckResult::Denied(_)),
                "write must be denied in mode {:?} when prompt deny-list includes it",
                mode,
            );
            // Unrelated tools still flow through normal rule eval.
            // `read` isn't in the deny list, so Yolo allows it
            // (other modes might Ask, that's mode-specific).
            if mode == SecurityMode::Yolo {
                assert!(matches!(
                    checker.check("read", "/tmp/foo"),
                    CheckResult::Allowed
                ));
            }
        }
    }

    /// M2 (dirge-cep): the unified `tools` map at the top of
    /// `PermissionConfig` lets rules be declared for ANY tool name
    /// (including ones dirge doesn't ship per-tool struct fields
    /// for — plugin-registered tools, future tools). Pin three
    /// invariants:
    ///   1. A rule in `tools` for a tool name with no legacy field
    ///      is honored.
    ///   2. A rule in `tools` for a tool name that ALSO has a
    ///      legacy field overrides the legacy field (explicit
    ///      newer shape wins).
    ///   3. The `Simple(action)` shape (string shorthand for
    ///      `{"*": action}`) works in the map.
    #[test]
    fn tools_map_unified_schema_honored_and_overrides_legacy() {
        use crate::permission::{PermissionConfig, ToolPerm};
        use std::collections::HashMap;

        // Tool with no legacy field — only reachable via `tools`.
        let mut tools_map = HashMap::new();
        let mut plugin_rules = HashMap::new();
        plugin_rules.insert("dangerous".to_string(), Action::Deny);
        tools_map.insert(
            "plugin_xyz".to_string(),
            ToolPerm::Granular(plugin_rules),
        );

        // Tool with a legacy field — map version should win.
        tools_map.insert("websearch".to_string(), ToolPerm::Simple(Action::Deny));

        let config = PermissionConfig {
            // Legacy field says Allow…
            websearch: Some(ToolPerm::Simple(Action::Allow)),
            tools: Some(tools_map),
            ..Default::default()
        };

        let mut checker = PermissionChecker::new(
            &config,
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/tmp")),
        );

        // (1) tools-only entry honored.
        assert!(matches!(
            checker.check("plugin_xyz", "dangerous"),
            CheckResult::Denied(_)
        ));

        // (2) tools map overrides legacy field.
        assert!(matches!(
            checker.check("websearch", "anything"),
            CheckResult::Denied(_)
        ));
    }

    /// Adversarial-review #1: the deny-list match must also fire for
    /// the umbrella `mcp_tool` name and the qualified `mcp_tool:srv:name`
    /// form, since MCP tools route through `check_perm("mcp_tool", …)`.
    /// `any_prompt_denied` is the API the MCP wrapper uses; pin its
    /// behavior here so a refactor can't silently re-open the bypass.
    #[test]
    fn prompt_deny_any_matches_concrete_and_qualified_mcp_names() {
        let mut checker = fresh_checker();
        // Plan-mode-style deny list.
        checker.set_prompt_deny_tools(vec!["edit".to_string(), "write".to_string()]);
        // Concrete MCP tool name matches.
        assert!(checker.any_prompt_denied(&["edit", "mcp_tool:fs:edit", "mcp_tool"]));
        // Umbrella match too.
        checker.set_prompt_deny_tools(vec!["mcp_tool".to_string()]);
        assert!(checker.any_prompt_denied(&["whatever", "mcp_tool:any:any", "mcp_tool"]));
        // Qualified-only deny.
        checker.set_prompt_deny_tools(vec!["mcp_tool:fs:write_file".to_string()]);
        assert!(checker.any_prompt_denied(&["write_file", "mcp_tool:fs:write_file", "mcp_tool"]));
        assert!(!checker.any_prompt_denied(&[
            "write_file",
            "mcp_tool:other:write_file",
            "mcp_tool:fs:write_other"
        ]));
    }

    /// Adversarial-review #7: case-insensitive deny-list. A prompt
    /// that says `deny_tools: [Edit]` must deny the tool registered
    /// as `edit`. (Frontmatter parser also lowercases at load, but
    /// pin the matcher-side guarantee here too.)
    #[test]
    fn prompt_deny_is_case_insensitive() {
        let mut checker = fresh_checker();
        checker.set_prompt_deny_tools(vec!["Edit".to_string(), "BASH".to_string()]);
        assert!(matches!(
            checker.check("edit", "foo"),
            CheckResult::Denied(_)
        ));
        assert!(matches!(
            checker.check("bash", "ls"),
            CheckResult::Denied(_)
        ));
    }

    /// User report: a sequence of MCP tool calls ran silently
    /// before any permission prompt fired. Root cause was that
    /// `mcp_tool` had no default rule, so the checker fell back to
    /// `default_action` (Allow). MCP tools execute external code;
    /// the default should be Ask. This test pins the new contract.
    #[test]
    fn mcp_tool_defaults_to_ask_when_unconfigured() {
        let mut checker = PermissionChecker::new(
            &PermissionConfig::default(),
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/tmp")),
        );
        let r = checker.check("mcp_tool", "mcp_tool:lattice:lattice_query");
        assert!(
            matches!(r, CheckResult::Ask),
            "unconfigured mcp_tool must default to Ask, got {:?}",
            r,
        );
    }

    /// Review #1: Accept mode previously coerced `Ask → Allow` for
    /// every non-path tool, silently bypassing the new default-Ask
    /// for `mcp_tool`. The coercion now special-cases
    /// `is_high_risk_non_path_tool` so MCP / shell keep their Ask
    /// even under `--accept`. (For bash, the legacy bash rule table
    /// already auto-allows safe commands by name; the special case
    /// here matters when an explicit user config sets bash to Ask —
    /// Accept mode must not silently undo that.)
    #[test]
    fn accept_mode_does_not_coerce_mcp_to_allow() {
        let mut checker = PermissionChecker::new(
            &PermissionConfig::default(),
            SecurityMode::Accept,
            Some(std::path::PathBuf::from("/tmp")),
        );
        // mcp_tool has its default-Ask rule installed; accept-mode
        // coercion must NOT downgrade it to Allow.
        let r = checker.check("mcp_tool", "mcp_tool:lattice:lattice_query");
        assert!(
            matches!(r, CheckResult::Ask),
            "Accept mode must NOT bypass mcp_tool's default-Ask, got {:?}",
            r,
        );
    }

    /// Accept mode STILL coerces other non-path Ask tools to Allow —
    /// the special-case is targeted, not a wholesale change.
    /// `question` (a non-path tool with Ask semantics in some
    /// configs) still gets the Accept-mode allow.
    #[test]
    fn accept_mode_still_coerces_safe_non_path_tools() {
        use std::collections::HashMap;
        let mut config = PermissionConfig::default();
        // Set question to Ask explicitly so Accept's coercion path
        // is exercised.
        let mut q_map: HashMap<String, Action> = HashMap::new();
        q_map.insert("*".to_string(), Action::Ask);
        config.question = Some(crate::permission::ToolPerm::Granular(q_map));
        let mut checker = PermissionChecker::new(
            &config,
            SecurityMode::Accept,
            Some(std::path::PathBuf::from("/tmp")),
        );
        let r = checker.check("question", "some question");
        assert!(
            matches!(r, CheckResult::Allowed),
            "Accept mode SHOULD coerce question's Ask → Allow (not high-risk), got {:?}",
            r,
        );
    }

    /// A user who explicitly configures mcp_tool rules retains
    /// control — the default-Ask only fires when no rule exists.
    #[test]
    fn mcp_tool_explicit_config_overrides_default_ask() {
        use std::collections::HashMap;
        let mut config = PermissionConfig::default();
        let mut granular = HashMap::new();
        granular.insert("mcp_tool:lattice:*".to_string(), Action::Allow);
        config.mcp_tool = Some(crate::permission::ToolPerm::Granular(granular));
        let mut checker = PermissionChecker::new(
            &config,
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/tmp")),
        );
        let r = checker.check("mcp_tool", "mcp_tool:lattice:lattice_query");
        assert!(
            matches!(r, CheckResult::Allowed),
            "explicit Allow rule must win, got {:?}",
            r,
        );
    }

    /// Empty deny list is a no-op — back to normal rule eval.
    #[test]
    fn prompt_deny_empty_is_noop() {
        let mut checker = fresh_checker();
        checker.set_prompt_deny_tools(Vec::new());
        // Under default rules in Standard mode, `read` is allowed.
        assert!(matches!(
            checker.check("read", "/tmp/foo"),
            CheckResult::Allowed
        ));
    }

    /// F7: `resolve_absolute` must follow symlinks so a symlink
    /// pointing at a deny-listed path can't bypass the rule.
    #[test]
    fn resolve_absolute_follows_symlinks() {
        // Create a temp dir with a real file + a symlink to it.
        // Use a unique dir per test process to avoid collisions
        // across parallel test runs.
        let dir =
            std::env::temp_dir().join(format!("dirge-f7-symlink-test-{}", std::process::id(),));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("real-secret.txt");
        std::fs::write(&target, "hunter2").unwrap();
        let link = dir.join("benign-name.txt");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();

        let resolved = resolve_absolute(link.to_str().unwrap(), "/");
        // The resolved path must match the real target, not the
        // symlink name. Canonicalize the comparand too — on macOS
        // /tmp is itself a symlink to /private/tmp.
        let expected = std::fs::canonicalize(&target)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(resolved, expected, "symlink should resolve to its target",);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F7: nonexistent paths (writes to new files) must still
    /// resolve sensibly. They can't canonicalize fully but we
    /// canonicalize the parent so `/real/parent/../../etc/passwd`
    /// becomes `/etc/passwd` instead of staying lexical.
    #[test]
    fn resolve_absolute_handles_nonexistent_via_parent_canonicalize() {
        let dir =
            std::env::temp_dir().join(format!("dirge-f7-newfile-test-{}", std::process::id(),));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let new_file = dir.join("does-not-exist-yet.txt");

        let resolved = resolve_absolute(new_file.to_str().unwrap(), "/");
        // The leaf doesn't canonicalize but the parent does.
        // Expected form: canonical(parent) / "does-not-exist-yet.txt"
        let expected_parent = std::fs::canonicalize(&dir).unwrap();
        let expected = expected_parent
            .join("does-not-exist-yet.txt")
            .to_string_lossy()
            .into_owned();
        assert_eq!(resolved, expected);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression for audit C3: when BOTH `canonicalize(joined)` AND
    /// `canonicalize(parent)` fail, the previous fallback returned
    /// the joined path with `..` components intact. Since
    /// `Path::starts_with` operates on path *components*, a crafted
    /// path like `/cwd/nonexistent_subdir/../../etc/passwd` would
    /// classify as internal because the first three components match
    /// `/cwd`. Attacker (LLM/agent) can synthesize such a path
    /// trivially. After the fix, `..` components are lexically
    /// resolved before the fallback returns, so the path escapes
    /// the cwd subtree.
    #[test]
    fn resolve_absolute_normalizes_dotdot_in_full_lexical_fallback() {
        // Working dir exists; subdirectory does NOT, ensuring both
        // canonicalize(joined) and canonicalize(parent) fail and we
        // hit the LEXICAL fallback path.
        let dir = std::env::temp_dir().join(format!("dirge-c3-traversal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.to_string_lossy().into_owned();

        // Build a path that, when joined to cwd, looks "inside" lexically
        // but actually escapes via `..` after a nonexistent intermediate.
        // joined = <cwd>/no_such_dir/no_such_subdir/../../../etc/passwd
        let traversal = "no_such_dir/no_such_subdir/../../../etc/passwd";

        let resolved = resolve_absolute(traversal, &cwd);

        // The escape attempt should NOT result in a path whose
        // components start with the cwd path. We don't insist on
        // exactly `/etc/passwd` (canonicalization of the cwd parent
        // can vary by host), but the resolved path must not be a
        // child of the cwd as the starts_with check would see it.
        let cwd_canonical = std::fs::canonicalize(&cwd).unwrap();
        let resolved_path = std::path::PathBuf::from(&resolved);
        assert!(
            !resolved_path.starts_with(&cwd_canonical) && !resolved_path.starts_with(&cwd),
            "lexical-fallback path-traversal should escape cwd subtree; got {:?}, cwd {:?}",
            resolved_path,
            cwd_canonical,
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Regression: "allow always" → `cd *` saved to session allowlist must
    // satisfy the NEXT bash check for `cd /absolute/path`. Before the fix,
    // path-glob semantics on `*` (`[^/]*`) refused to match the absolute
    // path, so the user was re-prompted every command.
    #[test]
    fn regression_session_allowlist_cd_star_matches_path_arg() {
        let mut checker = fresh_checker();
        checker.add_session_allowlist("bash".to_string(), "cd *");

        // The exact scenario from the bug report.
        let r1 = checker.check(
            "bash",
            "cd /Users/yogthos/src/work/rigging-workshop && git diff",
        );
        assert!(
            matches!(r1, CheckResult::Allowed),
            "expected Allowed, got {:?}",
            r1
        );

        let r2 = checker.check("bash", "cd /Users/yogthos/src/work/rigging-workshop");
        assert!(matches!(r2, CheckResult::Allowed));
    }

    /// Phase 5 — `/allow remove <idx>` plumbs through to
    /// `remove_session_allowlist_at`. Returns the removed entry's
    /// (tool, pattern) so the slash handler can confirm to the
    /// user what was removed.
    #[test]
    fn remove_session_allowlist_at_returns_removed_entry() {
        let mut checker = fresh_checker();
        checker.add_session_allowlist("bash".to_string(), "cargo *");
        checker.add_session_allowlist("bash".to_string(), "git *");
        checker.add_session_allowlist("read".to_string(), "/tmp/*");
        assert_eq!(checker.allowlist_entries().len(), 3);

        let removed = checker.remove_session_allowlist_at(1);
        assert_eq!(removed, Some(("bash".to_string(), "git *".to_string())),);
        // After removal, the indices shift: original [0]bash:cargo*,
        // [2]read:/tmp/* are now at [0] and [1].
        let after = checker.allowlist_entries();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0], ("bash".to_string(), "cargo *".to_string()));
        assert_eq!(after[1], ("read".to_string(), "/tmp/*".to_string()));
    }

    /// Out-of-range index returns None rather than panicking. The
    /// slash handler shows a clear error in that case.
    #[test]
    fn remove_session_allowlist_at_out_of_range_returns_none() {
        let mut checker = fresh_checker();
        checker.add_session_allowlist("bash".to_string(), "cargo *");
        assert_eq!(checker.remove_session_allowlist_at(99), None);
        assert_eq!(checker.remove_session_allowlist_at(1), None);
        // Existing entry still there.
        assert_eq!(checker.allowlist_entries().len(), 1);
    }

    /// `clear` empties the allowlist entirely. Different from
    /// `reset_to_new` (which clears EVERYTHING) — this is the
    /// user-facing nuke for just allowlist grants.
    #[test]
    fn clear_session_allowlist_empties_the_list() {
        let mut checker = fresh_checker();
        checker.add_session_allowlist("bash".to_string(), "cargo *");
        checker.add_session_allowlist("bash".to_string(), "git *");
        assert_eq!(checker.allowlist_entries().len(), 2);
        checker.clear_session_allowlist();
        assert!(checker.allowlist_entries().is_empty());
    }

    // Adding the same (tool, pattern) twice must not duplicate the
    // entry. The audit flagged that "allow always" picks for the
    // same command repeated across a long session accumulate
    // identical entries, wasting space and causing redundant
    // matches on every subsequent check.
    #[test]
    fn add_session_allowlist_dedupes_identical_entries() {
        let mut checker = fresh_checker();
        checker.add_session_allowlist("bash".to_string(), "cargo *");
        checker.add_session_allowlist("bash".to_string(), "cargo *");
        checker.add_session_allowlist("bash".to_string(), "cargo *");
        let entries = checker.allowlist_entries();
        assert_eq!(
            entries.len(),
            1,
            "expected dedup; got {} entries: {:?}",
            entries.len(),
            entries,
        );
    }

    // Distinct patterns for the same tool stay separate — only
    // exact (tool, pattern) duplicates dedupe.
    #[test]
    fn add_session_allowlist_keeps_distinct_patterns_for_same_tool() {
        let mut checker = fresh_checker();
        checker.add_session_allowlist("bash".to_string(), "cargo *");
        checker.add_session_allowlist("bash".to_string(), "git *");
        let entries = checker.allowlist_entries();
        assert_eq!(entries.len(), 2, "got: {:?}", entries);
    }

    // `load_session_allowlist` (called on session resume) also
    // dedupes against existing entries. If the host calls load
    // twice (test harness, reconnect, etc.) the entries don't
    // double up.
    #[test]
    fn load_session_allowlist_dedupes_against_existing() {
        let mut checker = fresh_checker();
        let entries = vec![
            ("bash".to_string(), "cargo *".to_string()),
            ("bash".to_string(), "cargo *".to_string()),
        ];
        checker.load_session_allowlist(&entries);
        // Even within a single load, dupes get collapsed.
        assert_eq!(checker.allowlist_entries().len(), 1);
        // Loading the same entries again should not add more.
        checker.load_session_allowlist(&entries);
        assert_eq!(checker.allowlist_entries().len(), 1);
    }

    // Path-tool patterns still get filesystem-glob semantics — adding
    // `src/*` doesn't allow nested files. Force default Ask so we can read
    // the session-allowlist contribution in isolation from the default.
    #[test]
    fn path_tool_session_allowlist_keeps_one_segment_semantics() {
        let mut cfg = PermissionConfig::default();
        cfg.default = Some(Action::Ask);
        let mut checker = PermissionChecker::new(
            &cfg,
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/tmp")),
        );
        checker.add_session_allowlist("read".to_string(), "src/*");

        // One-segment hit from the session allowlist.
        assert!(matches!(
            checker.check_path("read", "src/main.rs"),
            CheckResult::Allowed
        ));
        // Nested path: not in allowlist, falls through to default Ask.
        let nested = checker.check_path("read", "src/agent/main.rs");
        assert!(
            matches!(nested, CheckResult::Ask),
            "src/* must not match nested path; got {:?}",
            nested
        );
    }

    // load_session_allowlist roundtrip: persisted patterns from a previous
    // session should match the way they did when saved.
    #[test]
    fn regression_load_session_allowlist_preserves_command_semantics() {
        let mut checker = fresh_checker();
        let saved = vec![("bash".to_string(), "cd *".to_string())];
        checker.load_session_allowlist(&saved);

        let r = checker.check("bash", "cd /home/me/project");
        assert!(matches!(r, CheckResult::Allowed));
    }

    #[test]
    fn pattern_for_tool_distinguishes_path_and_command_tools() {
        assert!(pattern_for_tool("bash", "cd *").matches("cd /a/b/c"));
        assert!(!pattern_for_tool("read", "cd *").matches("cd /a/b/c"));
        assert!(pattern_for_tool("read", "cd *").matches("cd file"));
    }
}
