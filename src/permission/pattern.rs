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
        // dirge-9zbd: command-style patterns match against whole bash
        // command segments, whose arguments routinely contain embedded
        // newlines — e.g. `npx tsx -e "import x\nimport y"`, `python3 -c
        // "...\n..."`, heredoc-ish bodies. Compile them DOTALL so `*`
        // (→ `.*`) spans those newlines, matching shell-glob intent where
        // `*` is any char including `\n`. WITHOUT this, a session grant
        // like `npx *` (`^npx(?: .*)?$`) silently fails to match any
        // multi-line invocation and the agent re-prompts forever despite
        // "allow always". Path patterns stay line-sensitive: filesystem
        // paths don't legitimately contain newlines, and letting `**` span
        // them would weaken path scoping in deny rules.
        let regex = if path_style {
            Regex::new(&regex_str)
        } else {
            regex::RegexBuilder::new(&regex_str)
                .dot_matches_new_line(true)
                .build()
        }
        .unwrap_or_else(|_| Regex::new("^$").unwrap());
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

    /// F3 (dirge-efw): a trailing ` *` in a command-style pattern
    /// makes the args optional. So `ls *` matches BOTH `ls` (no
    /// args) and `ls -la` (with args). Matches opencode's
    /// `util/wildcard.ts:13-15` semantic. Without this, a session
    /// allowlist entry `ls *` re-prompts when the agent next
    /// invokes bare `ls`.
    #[test]
    fn f3_command_trailing_space_star_makes_args_optional() {
        let pat = Pattern::new_command("ls *");
        // With args — same as before.
        assert!(pat.matches("ls -la"));
        assert!(pat.matches("ls /tmp"));
        // Without args — NEW behavior post-F3.
        assert!(pat.matches("ls"));
        // Doesn't over-match a different command that happens to
        // start with `ls`.
        assert!(!pat.matches("lsof"));
        assert!(!pat.matches("less"));
    }

    /// F3 doesn't affect path-style patterns. `src/*` still
    /// matches single-segment files and doesn't span directories.
    /// (Note: `src/` itself matches because `*` accepts empty
    /// segments — pre-existing behavior, orthogonal to F3.)
    #[test]
    fn f3_does_not_relax_path_patterns() {
        let pat = Pattern::new("src/*");
        // With segment — matches.
        assert!(pat.matches("src/main.rs"));
        // Doesn't span directories (existing semantic).
        assert!(!pat.matches("src/agent/main.rs"));
        // Bare `src` (no trailing slash) — pre-F3 behavior:
        // doesn't match because pattern requires the `/`.
        assert!(!pat.matches("src"));
    }

    /// F3: bare `git *` doesn't accidentally swallow other commands.
    #[test]
    fn f3_anchored_to_command_head() {
        let pat = Pattern::new_command("git *");
        assert!(pat.matches("git"));
        assert!(pat.matches("git status"));
        assert!(pat.matches("git diff --name-only"));
        // Not anchored to a prefix; bare `git` matches but
        // `gitk` does not.
        assert!(!pat.matches("gitk"));
        assert!(!pat.matches("egit"));
    }

    /// dirge-9zbd: a trailing ` **` on a command rule makes args optional,
    /// just like ` *`. So `cargo test **` matches BOTH bare `cargo test`
    /// and `cargo test --all`. Before this, `cargo test **` compiled to
    /// `^cargo test .*$` and the bare command re-prompted.
    #[test]
    fn command_double_star_makes_args_optional() {
        let pat = Pattern::new_command("cargo test **");
        assert!(pat.matches("cargo test"), "bare command must match");
        assert!(pat.matches("cargo test --all"));
        assert!(pat.matches("cargo test --all --features x"));
        // Still head-anchored.
        assert!(!pat.matches("cargo testx"));
        assert!(!pat.matches("xcargo test"));
        // `npx **` matches bare `npx` and any args (incl. multi-line).
        let npx = Pattern::new_command("npx **");
        assert!(npx.matches("npx"));
        assert!(npx.matches("npx tsx -e \"a\nb\""));
    }

    /// A `/**`-suffixed COMMAND deny pattern (no space before `**`) keeps
    /// requiring the prefix — the optional-args rewrite must not fire for
    /// it, or `rm -rf /**` would also match bare `rm -rf`.
    #[test]
    fn command_slash_double_star_is_not_made_optional() {
        let pat = Pattern::new_command("rm -rf /**");
        assert!(pat.matches("rm -rf /etc"));
        assert!(pat.matches("rm -rf /"));
        // Must NOT match `rm -rf` with no slash-path (that's a different,
        // non-denied command — e.g. `rm -rf ./local`).
        assert!(!pat.matches("rm -rf"));
        assert!(!pat.matches("rm -rf ./local"));
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

    /// dirge-9zbd: a command-style grant must match a multi-line command.
    /// Models constantly emit `npx tsx -e "<multi-line script>"`,
    /// `python3 -c "...\n..."`, etc. The command regex is DOTALL so `*`
    /// spans the embedded newlines; without it, "allow always" never
    /// sticks and the agent re-prompts on every multi-line invocation.
    #[test]
    fn command_pattern_spans_embedded_newlines() {
        let pat = Pattern::new_command("npx *");
        // Single-line — always worked.
        assert!(pat.matches("npx tsx -e \"console.log(1)\""));
        // Multi-line `-e` script — the exact reported failure.
        let multi = "npx tsx -e \"import { readFileSync } from 'fs';\n\
                     import { runRiggingTest } from './src/index.ts';\n\
                     runRiggingTest();\"";
        assert!(
            pat.matches(multi),
            "command grant must match a multi-line argument, got no match for:\n{multi}"
        );
        // Other common multi-line shapes.
        assert!(Pattern::new_command("python3 *").matches("python3 -c \"import sys\nprint(1)\""));
        assert!(Pattern::new_command("node *").matches("node -e \"const x = 1;\nconsole.log(x)\""));
        // Still anchored to the head — a multi-line command that doesn't
        // start with `npx` must NOT match.
        assert!(!pat.matches("node -e \"x\nnpx y\""));
    }

    /// Path patterns stay line-sensitive (a `\n` in a path is pathological;
    /// `**` must not silently span it and broaden a deny/allow scope).
    #[test]
    fn path_pattern_does_not_span_newlines() {
        let pat = Pattern::new("/etc/**");
        assert!(pat.matches("/etc/passwd"));
        // A newline-bearing "path" must not be swallowed by `**`.
        assert!(!pat.matches("/etc/x\n/home/victim"));
    }

    /// PERM-4: `/etc/**` should match the bare directory and all
    /// content beneath it. Previous behavior required a `/`-suffixed
    /// path, silently missing the directory itself.
    #[test]
    fn trailing_double_star_matches_bare_dir() {
        let pat = Pattern::new("/etc/**");
        assert!(pat.matches("/etc"), "bare directory should match");
        assert!(pat.matches("/etc/passwd"), "child should match");
        assert!(
            pat.matches("/etc/nested/deep/file"),
            "nested child should match",
        );
        // Sibling that shares a prefix must NOT match.
        assert!(!pat.matches("/etcetera/foo"));
    }
}

fn glob_to_regex(pattern: &str, path_style: bool) -> String {
    // F3 (dirge-efw): trailing ` *` becomes ` (?:.*)?$` — opencode's
    // `util/wildcard.ts:13-15` semantic. Lets a session-allowlist
    // pattern like `ls *` match BOTH `ls` (no args) and `ls -la`
    // (with args). Without this rewrite, `ls *` compiles to
    // `^ls .*$` which requires the trailing space, so the user
    // gets re-prompted for bare `ls`.
    //
    // Applies only to command-style patterns (path_style=false).
    // Path patterns like `src/*` legitimately require at least
    // one character after the slash; relaxing those would let
    // `src/` (the directory itself, no file) match a per-file
    // rule. Command tools use shell-style globbing where the
    // optional-trailing-arg semantic is the user expectation.
    // dirge-9zbd: also covers a trailing ` **`. For COMMAND patterns `*`
    // and `**` both compile to `.*`, so `cargo test *` and `cargo test **`
    // are equivalent — the only purpose of this rewrite is making the
    // trailing args OPTIONAL. Without it, `cargo test **` compiled to
    // `^cargo test .*$`, which requires a trailing space, so the BARE
    // command `cargo test` (no args) silently re-prompted — and most
    // `default_bash_rules` entries use the ` **` form.
    if !path_style && !pattern.ends_with("\\ *") {
        if let Some(head) = pattern
            .strip_suffix(" **")
            .or_else(|| pattern.strip_suffix(" *"))
        {
            let head_regex = glob_to_regex_inner(head, path_style);
            return format!("^{head_regex}(?: .*)?$");
        }
    }
    // PERM-4: a user-written `/etc/**` should match BOTH the
    // directory itself and everything beneath it. Default inner
    // semantics emit `^/etc/.*$` for that pattern, which requires
    // a slash + content and silently misses the bare-directory
    // case. Trailing `/**` rewrites the tail to `(?:/.*)?` so both
    // forms hit. Path-style only; command patterns don't have
    // this idiom.
    if path_style && pattern.ends_with("/**") && pattern.len() >= 3 {
        let head = &pattern[..pattern.len() - 3];
        let head_regex = glob_to_regex_inner(head, path_style);
        return format!("^{head_regex}(?:/.*)?$");
    }
    format!("^{}$", glob_to_regex_inner(pattern, path_style))
}

/// Inner glob → regex without the leading `^` and trailing `$`
/// anchors. Separated so the F3 trailing-space-star rewrite can
/// wrap the head independently.
fn glob_to_regex_inner(pattern: &str, path_style: bool) -> String {
    let mut re = String::with_capacity(pattern.len() * 2);
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
    re
}
