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
    session_allowlist: Vec<(String, Pattern)>,
    recent_calls: VecDeque<(String, String)>,
    mode: SecurityMode,
}

/// Tool names where the input is a filesystem path. For these, `*` keeps
/// classic glob semantics (one segment, doesn't cross `/`). Everything else
/// is treated as shell/text where `*` means "any chars including /".
pub(crate) fn is_path_tool_name(tool: &str) -> bool {
    matches!(tool, "read" | "write" | "edit" | "list_dir")
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

        PermissionChecker {
            rules,
            default_action,
            ext_dir_rules,
            doom_loop_action,
            working_dir,
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

        let mut matched: Vec<Action> = Vec::new();
        if let Some(rules) = self.rules.get(tool) {
            for (pattern, action) in rules {
                if pattern.matches(input) {
                    matched.push(*action);
                }
            }
        }

        let base = matched.last().copied().unwrap_or(self.default_action);
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
                        return CheckResult::Denied(
                            "Doom loop: repeated identical tool call".to_string(),
                        );
                    }
                    Action::Ask => return CheckResult::Ask,
                    Action::Allow => {}
                }
            }
        }

        match action {
            Action::Allow => CheckResult::Allowed,
            Action::Ask => CheckResult::Ask,
            Action::Deny => CheckResult::Denied("Blocked by permission rules".to_string()),
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
        let mut matched: Vec<Action> = Vec::new();
        if let Some(rules) = self.rules.get(tool) {
            for (pattern, action) in rules {
                if pattern.matches(&abs_path) || pattern.matches(path) {
                    matched.push(*action);
                }
            }
        }

        let base = matched.last().copied().unwrap_or(self.default_action);
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
                        return CheckResult::Denied(
                            "Doom loop: repeated identical tool call".to_string(),
                        );
                    }
                    Action::Ask => return CheckResult::Ask,
                    Action::Allow => {}
                }
            }
        }

        match action {
            Action::Allow => CheckResult::Allowed,
            Action::Ask => CheckResult::Ask,
            Action::Deny => CheckResult::Denied("Blocked by permission rules".to_string()),
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
        let pattern = pattern_for_tool(&tool, pattern_str);
        self.session_allowlist.push((tool, pattern));
    }

    pub fn load_session_allowlist(&mut self, entries: &[(String, String)]) {
        for (tool, pat) in entries {
            self.session_allowlist
                .push((tool.clone(), pattern_for_tool(tool, pat)));
        }
    }

    #[allow(dead_code)]
    pub fn allowlist_entries(&self) -> Vec<(String, String)> {
        self.session_allowlist
            .iter()
            .map(|(t, p)| (t.clone(), p.original.clone()))
            .collect()
    }

    pub fn set_mode(&mut self, mode: SecurityMode) {
        self.mode = mode;
    }

    pub fn mode(&self) -> SecurityMode {
        self.mode
    }

    pub fn set_working_dir(&mut self, dir: &str) {
        self.working_dir = dir.to_string();
    }

    fn is_path_tool(&self, tool: &str) -> bool {
        matches!(tool, "read" | "write" | "edit" | "list_dir")
    }

    fn is_external_path(&self, path_str: &str) -> bool {
        let p = Path::new(path_str);
        if !p.is_absolute() {
            return false;
        }
        let cwd = Path::new(&self.working_dir);
        !p.starts_with(cwd)
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

fn resolve_absolute(path: &str, working_dir: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_string_lossy().to_string()
    } else {
        Path::new(working_dir).join(p).to_string_lossy().to_string()
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
