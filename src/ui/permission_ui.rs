const ALLOW_PLACEHOLDER: &str = "<edit this pattern>";

/// Whether a pattern was returned by `suggest_pattern` as the
/// "empty input — please type a real pattern" placeholder rather
/// than a real glob. Used by the ask-dialog to detect when the
/// user pressed "allow always" on a degenerate input and refuse
/// to store the placeholder as an actual allowlist entry.
pub(crate) fn is_placeholder_pattern(p: &str) -> bool {
    p == ALLOW_PLACEHOLDER
}

/// Find the head (first word) of the first command segment in a bash
/// line that ISN'T a benign navigation/no-op prefix. Used so an
/// allow-always suggestion targets the command that actually needs
/// permission (e.g. `python3` in `cd /x && python3 …`) rather than an
/// already-auto-allowed prefix like `cd`. Returns `None` when every
/// segment is benign (then the caller falls back to the first token).
///
/// Splits on shell segment separators only to locate the head — the
/// goal is just to skip a leading benign command, so a heredoc/quoted
/// body further right is irrelevant (the first significant head appears
/// before it).
fn significant_bash_head(command: &str) -> Option<&str> {
    // Only prefixes that are THEMSELVES auto-allowed by
    // `default_bash_rules` belong here — skipping a prefix that still
    // needs approval would make the suggested pattern miss it and the
    // agent would keep re-prompting on that segment (dirge-9zbd). So
    // `source`/`.` are intentionally NOT here: they execute arbitrary
    // script code and are not auto-allowed, so the suggestion should
    // target them.
    const BENIGN: &[&str] = &[
        "cd", "pushd", "popd", "export", "set", "unset", ":", "true", "env",
    ];
    command
        .split(['&', '|', ';', '\n'])
        .map(str::trim)
        .filter_map(|seg| seg.split_whitespace().next())
        .find(|head| !BENIGN.contains(head))
}

pub(crate) fn suggest_pattern(tool: &str, input: &str) -> String {
    // Refuse to suggest a catch-all wildcard for empty / whitespace-
    // only input. A user mis-clicking "(a) allow always" on an empty
    // invocation would otherwise pin an "allow everything for this
    // tool forever" rule into their session. The placeholder string
    // is intentionally not a valid glob — the UI shows it as the
    // suggested pattern, the user edits it before confirming.
    const PLACEHOLDER: &str = ALLOW_PLACEHOLDER;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return PLACEHOLDER.to_string();
    }
    match tool {
        "bash" => {
            // Base the suggestion on the first SIGNIFICANT command, not
            // literally the first token. A compound command is split into
            // a permission claim per segment; benign navigation prefixes
            // like `cd` are already auto-allowed (`default_bash_rules`),
            // so suggesting `cd *` for `cd /x && python3 …` saves a rule
            // that covers nothing — the `python3` segment keeps
            // prompting. Skip the benign prefix and suggest `python3 *`.
            let head = significant_bash_head(trimmed)
                .unwrap_or_else(|| trimmed.split_whitespace().next().unwrap_or(PLACEHOLDER));
            format!("{} *", head)
        }
        // Path-arg tools: suggest a `<parent>/**` glob from the input
        // path. One arm for all of them — previously read/write/edit/
        // list_dir, apply_patch, and the semantic tools each had an
        // identical copy of this body (dirge-t1wh).
        "read" | "write" | "edit" | "list_dir" | "apply_patch" | "list_symbols"
        | "get_symbol_body" | "find_definition" | "find_callers" | "find_callees" => {
            let path = std::path::Path::new(trimmed);
            let parent = path
                .parent()
                .map(|p| p.to_string_lossy())
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            if parent.is_empty() {
                "**".to_string()
            } else {
                format!("{}/**", parent)
            }
        }
        "grep" | "find_files" => {
            let first = trimmed.split_whitespace().next().unwrap_or(PLACEHOLDER);
            format!("{}*", first)
        }
        "mcp_tool" => {
            let mut parts = trimmed.splitn(3, ':');
            let umbrella = parts.next().unwrap_or("");
            let server = parts.next().unwrap_or("");
            if umbrella.eq_ignore_ascii_case("mcp_tool") && !server.is_empty() {
                format!("mcp_tool:{}:*", server)
            } else {
                PLACEHOLDER.to_string()
            }
        }
        "webfetch" => "webfetch:*".to_string(),
        "websearch" => "websearch:*".to_string(),
        "task" | "task_status" | "question" => "**".to_string(),
        "glob" | "repo_overview" | "skill" | "memory" | "write_todo_list" | "lsp" => {
            "**".to_string()
        }
        _ => PLACEHOLDER.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `suggest_pattern` returns a literal placeholder for empty
    /// input. The ask-dialog path that consumes it must detect the
    /// placeholder and refuse to add it as an allowlist entry —
    /// otherwise pressing "a" (allow always) on an empty invocation
    /// would silently store `<edit this pattern>` as a real pattern.
    /// The detection is exposed via `is_placeholder_pattern` so the
    /// dialog code is unit-testable.
    #[test]
    fn placeholder_pattern_is_detectable() {
        let p = suggest_pattern("bash", "");
        assert!(
            is_placeholder_pattern(&p),
            "empty input should yield a detectable placeholder; got {p:?}",
        );
        let p = suggest_pattern("grep", "  \t  ");
        assert!(is_placeholder_pattern(&p));
        // A legit suggestion is NOT flagged as a placeholder.
        let p = suggest_pattern("bash", "cargo test");
        assert!(!is_placeholder_pattern(&p), "real pattern flagged: {p:?}");
    }

    // Whitespace-only or empty input must NOT collapse to a "* *"
    // / "*" wildcard pattern that matches every subsequent call.
    // The audit flagged this as a footgun: a user accidentally
    // hitting "(a) allow always" on an empty bash invocation would
    // permanently auto-allow ALL bash. Now we return a literal
    // placeholder + the user has to type the pattern themselves.
    #[test]
    fn suggest_pattern_refuses_wildcard_on_empty_input() {
        // Bash: empty / whitespace input should NOT yield "* *".
        let p = suggest_pattern("bash", "");
        assert_ne!(p, "* *", "empty bash input must not yield catch-all");
        assert!(
            !p.contains('*'),
            "empty input should not contain wildcards: {p:?}"
        );

        let p = suggest_pattern("bash", "   \t  ");
        assert_ne!(
            p, "* *",
            "whitespace-only bash input must not yield catch-all"
        );
        assert!(
            !p.contains('*'),
            "ws-only input should not contain wildcards: {p:?}"
        );

        // grep / find_files: same — empty must not yield "*"
        let p = suggest_pattern("grep", "");
        assert!(
            !p.contains('*'),
            "empty grep input must not yield wildcard: {p:?}"
        );

        // Unknown tool with empty input shouldn't yield catch-all.
        let p = suggest_pattern("mcp_tool:foo", "");
        assert!(!p.contains('*'), "unknown tool empty input: {p:?}");
    }

    /// A compound command with a benign `cd` prefix must suggest the
    /// SIGNIFICANT command, not `cd *` (which is already auto-allowed and
    /// leaves the real command prompting forever). Regression for the
    /// "permission keeps re-asking" report.
    #[test]
    fn compound_bash_suggests_significant_command_not_cd() {
        assert_eq!(
            suggest_pattern("bash", "cd /tmp/proj && python3 gen.py"),
            "python3 *"
        );
        // Heredoc body (with its own punctuation) doesn't confuse the head pick.
        assert_eq!(
            suggest_pattern(
                "bash",
                "cd src && python3 - <<PY\nwith open('a','w') as f: f.write(x)\nPY"
            ),
            "python3 *"
        );
        // Multiple benign prefixes are all skipped.
        assert_eq!(
            suggest_pattern("bash", "export X=1 && cd app && npm run build"),
            "npm *"
        );
        // A plain significant command is unchanged.
        assert_eq!(suggest_pattern("bash", "cargo test --all"), "cargo *");
        // cd-only (no significant segment) falls back to the first token.
        assert_eq!(suggest_pattern("bash", "cd /tmp"), "cd *");
    }

    /// dirge-9zbd: `source`/`.` execute arbitrary script code and are NOT
    /// auto-allowed, so they must NOT be skipped — the suggestion targets
    /// them, so granting it covers the (otherwise un-allowed) source while
    /// any default-allowed sibling (`python …`) already passes.
    #[test]
    fn source_is_the_suggestion_target_not_skipped() {
        assert_eq!(
            suggest_pattern("bash", "source venv/bin/activate && python app.py"),
            "source *"
        );
        assert_eq!(suggest_pattern("bash", ". ./env.sh && cargo run"), ". *");
        // But genuinely-benign, auto-allowed prefixes ARE still skipped.
        assert_eq!(
            suggest_pattern("bash", "export TOKEN=x && unset Y && mycli run"),
            "mycli *"
        );
    }

    // Non-empty inputs still produce the expected suggestion.
    #[test]
    fn suggest_pattern_works_for_non_empty_inputs() {
        assert_eq!(suggest_pattern("bash", "cargo test --all"), "cargo *");
        assert_eq!(suggest_pattern("grep", "fn foo bar"), "fn*");
    }

    /// User-reported bug: "allow always" on a write inside `src/`
    /// stored `src/*` (single `*`, no slash-spanning), so the next
    /// write under `src/agent/…` re-prompted. Maki's equivalent
    /// (`maki-agent/src/permissions.rs:519`) uses `parent/**`. Pin
    /// that the fix is in place for every path-shaped tool.
    #[test]
    fn suggest_pattern_path_tools_use_recursive_glob() {
        assert_eq!(suggest_pattern("write", "src/main.rs"), "src/**");
        assert_eq!(suggest_pattern("edit", "src/main.rs"), "src/**");
        assert_eq!(
            suggest_pattern("write", "src/agent/tools/foo.rs"),
            "src/agent/tools/**"
        );
        assert_eq!(suggest_pattern("read", "src/main.rs"), "src/**");
        assert_eq!(suggest_pattern("list_dir", "src/agent"), "src/**");
        // Files at the repo root: `Path::parent` is "" — keep the
        // existing `**` fallback so the rule is broad but explicit.
        assert_eq!(suggest_pattern("write", "main.rs"), "**");
    }

    /// User-reported bug: `[a] allow always` on an MCP tool call
    /// silently degraded to `allow once` because the catch-all
    /// `_ => PLACEHOLDER` branch fired for `mcp_tool`. Result: the
    /// permission allowlist never got an entry and every
    /// subsequent call to the same MCP server re-prompted the
    /// user.
    #[test]
    fn suggest_pattern_derives_server_wildcard_for_mcp_tool() {
        let p = suggest_pattern("mcp_tool", "mcp_tool:lattice:lattice_expand");
        assert_eq!(p, "mcp_tool:lattice:*");
        // Multi-segment server names also work.
        let p = suggest_pattern("mcp_tool", "mcp_tool:my-server:do_thing");
        assert_eq!(p, "mcp_tool:my-server:*");
    }

    /// Malformed MCP input (missing colons, wrong umbrella) still
    /// falls through to the placeholder rather than producing a
    /// nonsense pattern.
    #[test]
    fn suggest_pattern_mcp_tool_malformed_input_uses_placeholder() {
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool", "garbage"
        )));
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool",
            "mcp_tool:"
        )));
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool",
            "mcp_tool::"
        )));
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool",
            "wrong:lattice:foo"
        )));
    }
}
