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

        if !rules.contains_key("bash") {
            let mut defaults = Vec::new();
            for (pat, action) in crate::permission::default_bash_rules() {
                defaults.push((pattern_for_tool("bash", pat), action));
            }
            rules.insert("bash".to_string(), defaults);
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
        }
    }

    pub fn check(&mut self, tool: &str, input: &str) -> CheckResult {
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
                    if self.is_external_path(&abs_path) {
                        self.match_ext_dir(&abs_path).unwrap_or(Action::Ask)
                    } else {
                        Action::Allow
                    }
                }
                other => other,
            },
            SecurityMode::Standard => base,
            SecurityMode::Yolo => unreachable!(),
        };

        let action =
            if matched.is_empty() && action == Action::Allow && self.is_external_path(&abs_path) {
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

    pub fn mode(&self) -> SecurityMode {
        self.mode
    }

    pub fn set_working_dir(&mut self, dir: &str) {
        self.working_dir = dir.to_string();
        self.working_dir_canonical = canonicalize_for_cache(dir);
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
            // LEXICAL join (matches pre-F7 behavior so rules on
            // not-yet-existing paths still match).
            if let (Some(parent), Some(name)) = (joined.parent(), joined.file_name())
                && let Ok(canonical_parent) = std::fs::canonicalize(parent)
            {
                return canonical_parent.join(name).to_string_lossy().to_string();
            }
            joined.to_string_lossy().to_string()
        }
    }
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
