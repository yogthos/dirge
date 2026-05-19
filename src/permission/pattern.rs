use regex::Regex;

#[derive(Debug, Clone)]
pub struct Pattern {
    regex: Regex,
    #[allow(dead_code)]
    pub original: String,
}

impl Pattern {
    /// Filesystem-style glob: `*` matches one path segment (no `/`); `**`
    /// matches any depth. Use for path tools (`read`, `write`, `edit`,
    /// `list_dir`).
    pub fn new(pattern: &str) -> Self {
        Self::compile(pattern, /* path_style */ true)
    }

    /// Shell-style glob for non-path inputs: `*` matches any chars including
    /// `/`. Use for `bash` command patterns, `grep`/`find_files` patterns,
    /// and other tools where the input isn't a filesystem path.
    ///
    /// Without this, a user pattern like `cd *` (suggested by the harness
    /// for `bash` after the user accepts "allow always") would NOT match
    /// `cd /Users/foo/bar` because `[^/]*` stops at the first slash.
    pub fn new_command(pattern: &str) -> Self {
        Self::compile(pattern, /* path_style */ false)
    }

    fn compile(pattern: &str, path_style: bool) -> Self {
        let expanded = expand_home(pattern);
        let regex_str = glob_to_regex(&expanded, path_style);
        let regex = Regex::new(&regex_str).unwrap_or_else(|_| Regex::new("^$").unwrap());
        Pattern {
            regex,
            original: pattern.to_string(),
        }
    }

    pub fn matches(&self, input: &str) -> bool {
        self.regex.is_match(input)
    }
}

fn expand_home(pattern: &str) -> String {
    if pattern == "~" || pattern == "$HOME" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().to_string();
        }
        return pattern.to_string();
    }
    if let Some(rest) = pattern.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}/{}", home.to_string_lossy(), rest);
        }
        return pattern.to_string();
    }
    if let Some(rest) = pattern.strip_prefix("$HOME/")
        && let Some(home) = dirs::home_dir()
    {
        return format!("{}/{}", home.to_string_lossy(), rest);
    }
    pattern.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression: `cd *` saved via "allow always" must match the user's NEXT
    // `cd /absolute/path` command. The original bug was filesystem-glob
    // semantics applied to a shell-command pattern: `*` compiled to `[^/]*`,
    // refusing to cross slashes. Allowlist entries for bash never fired and
    // the agent re-prompted on every command.
    #[test]
    fn regression_command_pattern_cd_star_matches_path_arg() {
        let pat = Pattern::new_command("cd *");
        assert!(pat.matches("cd /Users/yogthos/src/work/foo"));
        assert!(pat.matches("cd /Users/yogthos/src/work/foo && git diff"));
        assert!(pat.matches("cd foo"));
    }

    #[test]
    fn regression_command_pattern_anchors_to_start() {
        // Don't over-rotate: `cd *` shouldn't match commands that merely
        // contain `cd ` somewhere later.
        let pat = Pattern::new_command("cd *");
        assert!(!pat.matches("xcd foo"));
        assert!(!pat.matches("echo cd foo"));
    }

    #[test]
    fn path_pattern_star_still_excludes_slash() {
        let pat = Pattern::new("src/*");
        assert!(pat.matches("src/main.rs"));
        // Single segment only — `*` doesn't span directory boundaries.
        assert!(!pat.matches("src/agent/main.rs"));
    }

    #[test]
    fn path_pattern_double_star_spans_directories() {
        let pat = Pattern::new("src/**");
        assert!(pat.matches("src/main.rs"));
        assert!(pat.matches("src/agent/main.rs"));
        assert!(pat.matches("src/agent/tools/foo.rs"));
    }

    #[test]
    fn command_pattern_question_mark_matches_any_char() {
        let pat = Pattern::new_command("file.?");
        assert!(pat.matches("file.a"));
        // For commands, `?` is unrestricted.
        assert!(pat.matches("file./"));
    }

    #[test]
    fn path_pattern_question_mark_excludes_slash() {
        let pat = Pattern::new("file.?");
        assert!(pat.matches("file.a"));
        assert!(!pat.matches("file./"));
    }

    #[test]
    fn home_expansion_works_for_both_styles() {
        if let Some(home) = dirs::home_dir() {
            let expected = format!("{}/foo/bar", home.display());
            assert!(Pattern::new("~/foo/*").matches(&expected));
            assert!(Pattern::new_command("~/foo/*").matches(&expected));
        }
    }

    // Regex metachars in pattern text must be escaped, not interpreted.
    #[test]
    fn special_chars_are_escaped() {
        let pat = Pattern::new_command("npm test (unit)");
        assert!(pat.matches("npm test (unit)"));
        // Without escaping, `(unit)` would be a regex group and not require
        // the literal parens.
        assert!(!pat.matches("npm test unit"));
    }
}

fn glob_to_regex(pattern: &str, path_style: bool) -> String {
    let mut re = String::with_capacity(pattern.len() * 2);
    re.push('^');
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else if path_style {
                    re.push_str("[^/]*");
                } else {
                    re.push_str(".*");
                }
            }
            '?' if path_style => re.push_str("[^/]"),
            '?' => re.push('.'),
            '.' => re.push_str("\\."),
            '\\' => re.push_str("\\\\"),
            '(' | ')' | '[' | ']' | '{' | '}' | '+' | '^' | '$' | '|' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    re
}
