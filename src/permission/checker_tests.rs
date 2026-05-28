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

/// M4 (dirge-ojn): the post-flip defaults.
/// - Read-only tools in the builtin-allow list don't prompt.
/// - Mutating / network / code-execution tools fall to the new
///   global Ask default OUTSIDE CWD. (Mutating tools inside the
///   working directory are auto-allowed by the CWD-scoped
///   builtin-allow rule installed alongside the read-only ones
///   — see `path_tool_writes_inside_cwd_auto_allowed` for that
///   path.)
/// - The `--yolo` mode bypass (via `SecurityMode::Yolo`) still
///   short-circuits everything (line 362 of `check_path`).
/// - An explicit user rule overrides the builtin-allow.
#[test]
fn m4_defaults_allow_safe_ask_dangerous() {
    // `working_dir = /tmp` so the dangerous-tool probes below
    // hit /opt/... — outside CWD — and exercise the global Ask
    // default rather than the CWD-scoped allow installer.
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp")),
    );

    // Builtin-allow: read-only tools don't prompt.
    for tool in [
        "read",
        "glob",
        "grep",
        "find_files",
        "list_dir",
        "list_symbols",
        "find_definition",
        "find_callers",
        "find_callees",
        "get_symbol_body",
        "repo_overview",
        "lsp",
        "write_todo_list",
        "task_status",
        "question",
        // dirge-sm9w: memory is auto-allowed in Standard/Accept
        // (scoped to ~/.dirge/memories/, no arbitrary FS access).
        "memory",
    ] {
        let result = checker.check_path(tool, "/tmp/anything.rs");
        assert!(
            matches!(result, CheckResult::Allowed),
            "builtin-allow tool {tool} should Allow without prompting; got {result:?}",
        );
    }

    // Mutating / network / code-execution tools fall to Ask.
    for tool in [
        "write",
        "edit",
        "apply_patch",
        "webfetch",
        "websearch",
        "task",
        "skill",
    ] {
        // Path is OUTSIDE working_dir (/tmp) so the CWD-scoped
        // allow installer does not apply.
        let result = checker.check_path(tool, "/opt/anywhere/anything.rs");
        assert!(
            matches!(result, CheckResult::Ask | CheckResult::Denied(_)),
            "dangerous tool {tool} should Ask or Deny outside CWD by default; got {result:?}",
        );
    }
}

/// F2 (dirge-jlj): write / apply_patch alias to the `edit`
/// permission. `edit: deny` blocks all three uniformly (matches
/// opencode's `EDIT_TOOLS` aliasing). This is enforced at the
/// `enforce` chokepoint, not in the checker — but the underlying
/// rules behavior must be sound, which we exercise here.
#[test]
fn f2_edit_alias_check_path_directly_for_write_and_apply_patch() {
    // The checker itself doesn't alias — that lives in
    // `tools::enforce`. But pin that the checker's `edit`
    // rules behave as the user expects when consulted with
    // the edit tool name.
    use crate::permission::ToolPerm;
    use std::collections::HashMap;

    let mut edit_rules = HashMap::new();
    edit_rules.insert("**".to_string(), Action::Deny);
    let config = PermissionConfig {
        edit: Some(ToolPerm::Granular(edit_rules)),
        ..Default::default()
    };

    let mut checker = PermissionChecker::new(
        &config,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp")),
    );

    // Direct `edit` query: hit the deny rule.
    assert!(matches!(
        checker.check_path("edit", "/tmp/x.rs"),
        CheckResult::Denied(_)
    ));
    // Direct `write` query (no aliasing at checker level): the
    // CWD-scoped builtin-allow rule only fires for paths inside
    // working_dir (/tmp here), so probe an OUTSIDE path to
    // exercise the global Ask default — that's the "write has
    // no user-configured rules and no in-CWD allow" path the
    // checker is asserted on.
    assert!(matches!(
        checker.check_path("write", "/opt/elsewhere/x.rs"),
        CheckResult::Ask
    ));
    // `tools::enforce` is what ties these together. The
    // alias test for that path lives in src/agent/tools/mod.rs
    // (covered indirectly by the bash F1 tests below since
    // write rules drive the redirect-target gate).
}

/// CWD-scoped builtin-allow for mutating tools: writes inside
/// the working directory are silent, writes outside still
/// prompt. Without this, users had to "allow always" on every
/// first write to each new subdir of their project — partly
/// from the `parent/*` bug, partly from the post-M4 posture of
/// no global allow for write/edit. This test pins both halves
/// (inside-allow + outside-ask) on the same checker instance.
#[test]
fn write_inside_cwd_allowed_outside_cwd_asks() {
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp/proj")),
    );
    for tool in ["write", "edit", "apply_patch"] {
        // Inside CWD: silent.
        assert!(
            matches!(
                checker.check_path(tool, "/tmp/proj/src/main.rs"),
                CheckResult::Allowed
            ),
            "{tool} inside CWD must be auto-allowed",
        );
        // Nested inside CWD: same.
        assert!(
            matches!(
                checker.check_path(tool, "/tmp/proj/src/agent/foo.rs"),
                CheckResult::Allowed
            ),
            "{tool} nested-inside-CWD must be auto-allowed",
        );
        // Outside CWD: prompt.
        assert!(
            matches!(checker.check_path(tool, "/etc/passwd"), CheckResult::Ask),
            "{tool} outside CWD must prompt",
        );
    }
}

/// A user's explicit `write: { "<cwd>/build/**": deny }` must
/// beat the CWD-scoped builtin-allow rule. Last-match-wins is
/// already the documented semantics; this pins it for the new
/// CWD-allow installer specifically.
#[test]
fn user_write_deny_overrides_cwd_builtin_allow() {
    use crate::permission::ToolPerm;
    use std::collections::HashMap;

    let mut write_rules = HashMap::new();
    write_rules.insert("/tmp/proj/build/**".to_string(), Action::Deny);
    let config = PermissionConfig {
        write: Some(ToolPerm::Granular(write_rules)),
        ..Default::default()
    };

    let mut checker = PermissionChecker::new(
        &config,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp/proj")),
    );

    // User's deny beats CWD-allow for the configured subtree.
    assert!(matches!(
        checker.check_path("write", "/tmp/proj/build/out.txt"),
        CheckResult::Denied(_)
    ));
    // Outside the user's deny scope, still allowed via CWD-allow.
    assert!(matches!(
        checker.check_path("write", "/tmp/proj/src/main.rs"),
        CheckResult::Allowed
    ));
}

/// `/cd` mid-session refreshes the CWD-allow rule. After cd from
/// `/tmp/old` to `/tmp/new`, writes inside `/tmp/new` must be
/// auto-allowed AND writes inside `/tmp/old` must NOT be
/// (the old rule must not linger).
#[test]
fn set_working_dir_refreshes_cwd_allow_rule() {
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp/old")),
    );
    // Baseline: old CWD allows.
    assert!(matches!(
        checker.check_path("write", "/tmp/old/foo.rs"),
        CheckResult::Allowed
    ));

    checker.set_working_dir("/tmp/new");

    // New CWD now allowed.
    assert!(matches!(
        checker.check_path("write", "/tmp/new/foo.rs"),
        CheckResult::Allowed
    ));
    // Old CWD no longer auto-allowed — the stale rule was
    // removed, so it falls through to default Ask.
    assert!(
        matches!(
            checker.check_path("write", "/tmp/old/foo.rs"),
            CheckResult::Ask
        ),
        "stale CWD-allow for /tmp/old must be removed after cd",
    );
}

/// Repeated `/cd` calls don't accumulate stale CWD-allow rules.
/// Pin that after N cds, only one CWD-allow entry per tool
/// remains (matching the current working_dir).
#[test]
fn set_working_dir_does_not_accumulate_stale_rules() {
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp/a")),
    );
    checker.set_working_dir("/tmp/b");
    checker.set_working_dir("/tmp/c");
    checker.set_working_dir("/tmp/d");

    // Only the most-recent CWD allows.
    for stale in ["/tmp/a/x", "/tmp/b/x", "/tmp/c/x"] {
        assert!(
            matches!(checker.check_path("write", stale), CheckResult::Ask),
            "{stale} should no longer be allowed",
        );
    }
    assert!(matches!(
        checker.check_path("write", "/tmp/d/x"),
        CheckResult::Allowed
    ));
}

/// Degenerate working_dirs (`/`, empty) must NOT install a
/// CWD-allow rule — `/` would generate `/**` which silently
/// allows everything, defeating the "permissive only inside the
/// project" intent.
#[test]
fn cwd_allow_refuses_root_and_empty() {
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/")),
    );
    // `/` cwd: writes anywhere still prompt — no `/**` allow installed.
    assert!(matches!(
        checker.check_path("write", "/etc/passwd"),
        CheckResult::Ask
    ));
    assert!(matches!(
        checker.check_path("write", "/tmp/anything.rs"),
        CheckResult::Ask
    ));
}

/// Working dirs containing glob metacharacters (`*`, `?`, `[`,
/// `{`) must NOT install a CWD-allow rule — the glob compiler
/// would interpret them as wildcards / classes and match
/// unintended paths.
#[test]
fn cwd_allow_refuses_paths_with_glob_metachars() {
    // The test working_dir doesn't have to exist on disk;
    // canonicalize falls back to the literal string for the
    // safety check.
    for dir in ["/tmp/proj-*", "/tmp/p[a-z]", "/tmp/{a,b}"] {
        let mut checker = PermissionChecker::new(
            &PermissionConfig::default(),
            SecurityMode::Standard,
            Some(std::path::PathBuf::from(dir)),
        );
        // A write that would normally land inside the project
        // must still prompt — no rule installed.
        let inside = format!("{}/foo.rs", dir);
        assert!(
            matches!(checker.check_path("write", &inside), CheckResult::Ask),
            "{dir} must not install CWD-allow (glob metachar present)",
        );
    }
}

/// Explicit user rules override the M4 builtin-allow list.
#[test]
fn m4_user_rule_overrides_builtin_allow() {
    use crate::permission::ToolPerm;
    use std::collections::HashMap;

    let mut read_rules = HashMap::new();
    read_rules.insert("/etc/**".to_string(), Action::Deny);
    let config = PermissionConfig {
        read: Some(ToolPerm::Granular(read_rules)),
        ..Default::default()
    };

    let mut checker = PermissionChecker::new(
        &config,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp")),
    );

    // User's explicit deny wins over builtin Allow.
    assert!(matches!(
        checker.check_path("read", "/etc/passwd"),
        CheckResult::Denied(_)
    ));
    // Other paths still hit builtin Allow.
    assert!(matches!(
        checker.check_path("read", "/tmp/safe.txt"),
        CheckResult::Allowed
    ));
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
    tools_map.insert("plugin_xyz".to_string(), ToolPerm::Granular(plugin_rules));

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
    // The CWD-scoped builtin-allow rule for write/edit/apply_patch
    // would otherwise intercept any path under `working_dir` and
    // mask the session-allowlist semantics under test. Pin
    // `working_dir = /cwd-off-test-axis` and probe paths that
    // live elsewhere (`/probe/src/...`) so the CWD-allow rule
    // never matches and the session allowlist alone gates the
    // decision.
    let mut checker = PermissionChecker::new(
        &cfg,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/cwd-off-test-axis")),
    );
    checker.add_session_allowlist("write".to_string(), "/probe/src/*");

    // One-segment hit from the session allowlist.
    assert!(matches!(
        checker.check_path("write", "/probe/src/main.rs"),
        CheckResult::Allowed
    ));
    // Nested path: not in allowlist, falls through to default Ask.
    let nested = checker.check_path("write", "/probe/src/agent/main.rs");
    assert!(
        matches!(nested, CheckResult::Ask),
        "/probe/src/* must not match nested path; got {:?}",
        nested
    );
}

/// F2 write↔edit aliasing: when a user "always allows" a write
/// path, the alias check against "edit" must also match so the
/// most-restrictive merge doesn't re-prompt on every subsequent
/// call. Without this, `enforce()` sees Allowed from write rules
/// but Ask from edit (no session-allowlist entry), and the
/// combined result is Ask — infinite re-prompt loop.
#[test]
fn add_session_allowlist_mirrors_write_to_edit() {
    let mut cfg = PermissionConfig::default();
    cfg.default = Some(Action::Ask);
    let mut checker = PermissionChecker::new(
        &cfg,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/cwd-off-test-axis")),
    );
    checker.add_session_allowlist("write".to_string(), "/probe/src/**");

    // The write tool itself hits the allowlist.
    assert!(matches!(
        checker.check_path("write", "/probe/src/main.rs"),
        CheckResult::Allowed
    ));
    // The edit alias MUST also match — this is what enforce() checks.
    assert!(
        matches!(
            checker.check_path("edit", "/probe/src/main.rs"),
            CheckResult::Allowed,
        ),
        "edit alias must reflect write session-allowlist entry"
    );

    // Reverse direction: "always allow" edit → write must match.
    let mut checker2 = PermissionChecker::new(
        &cfg,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/cwd-off-test-axis")),
    );
    checker2.add_session_allowlist("edit".to_string(), "/probe/src/**");
    assert!(
        matches!(
            checker2.check_path("write", "/probe/src/main.rs"),
            CheckResult::Allowed,
        ),
        "write must reflect edit session-allowlist entry"
    );
    assert!(
        matches!(
            checker2.check_path("apply_patch", "/probe/src/main.rs"),
            CheckResult::Allowed,
        ),
        "apply_patch must reflect edit session-allowlist entry"
    );

    // apply_patch → edit mirroring too.
    let mut checker3 = PermissionChecker::new(
        &cfg,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/cwd-off-test-axis")),
    );
    checker3.add_session_allowlist("apply_patch".to_string(), "/probe/src/**");
    assert!(
        matches!(
            checker3.check_path("edit", "/probe/src/main.rs"),
            CheckResult::Allowed,
        ),
        "edit must reflect apply_patch session-allowlist entry"
    );

    // Via load_session_allowlist too (persisted-session path).
    let mut checker4 = PermissionChecker::new(
        &cfg,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/cwd-off-test-axis")),
    );
    checker4.load_session_allowlist(&[("write".to_string(), "/probe/src/**".to_string())]);
    assert!(
        matches!(
            checker4.check_path("edit", "/probe/src/main.rs"),
            CheckResult::Allowed,
        ),
        "load_session_allowlist must also mirror write→edit"
    );

    // Non-aliased tools are unaffected.
    let mut checker5 = fresh_checker();
    checker5.add_session_allowlist("read".to_string(), "/tmp/**");
    assert!(matches!(
        checker5.check_path("read", "/tmp/foo.txt"),
        CheckResult::Allowed,
    ));
    // read doesn't alias to write/edit.
    assert!(
        !checker5.is_session_allowed("write", "/tmp/foo.txt"),
        "read allowlist entry must not leak to write"
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

/// Regression: the prior bash defaults used exact patterns
/// (`cargo build`, `git status`, etc.) so any flagged
/// invocation re-prompted (`cargo build --release` →
/// no match → Ask). The widened defaults wildcard those AND
/// add the common dev commands users hit constantly. Pin a
/// representative sample so a future tightening can't quietly
/// regress the friction.
#[test]
fn default_bash_rules_cover_common_flagged_invocations() {
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp/proj")),
    );
    for cmd in [
        // The original friction cases.
        "cargo build --release",
        "cargo test --bin dirge --features plugin",
        "cargo fmt --all --check",
        "cargo clippy --all-targets",
        "git status -s",
        "git log --oneline -10",
        // Newly-added safe dev commands.
        "cargo run --release",
        "git add -A",
        "git commit -m \"msg\"",
        "git checkout main",
        "git switch -c feat/foo",
        "git pull --rebase",
        "git fetch origin",
        "git restore --staged file.rs",
        "make test",
        "pytest -x tests/",
        "python3 script.py",
        "node index.js",
        "npx eslint .",
        "npm test -- --coverage",
        "go test ./...",
    ] {
        let result = checker.check("bash", cmd);
        assert!(
            matches!(result, CheckResult::Allowed),
            "{cmd:?} should be auto-allowed by default bash rules; got {result:?}",
        );
    }
}

/// Defense: high-risk operations stay Ask (or Deny) even after
/// the bash defaults were widened. If anyone accidentally adds
/// `npm install **` or similar to the allow list this fires.
#[test]
fn default_bash_rules_keep_high_risk_gated() {
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp/proj")),
    );
    // Destructive / network-side-effect / privilege-escalation
    // commands must NOT be silently allowed.
    for cmd in [
        "git push",
        "git push origin main",
        "git reset --hard",
        "git rebase -i main",
        "git stash drop",
        "npm install lodash",
        "pip install requests",
        "curl http://example.com",
        "wget http://example.com",
        "sudo make install",
    ] {
        let result = checker.check("bash", cmd);
        assert!(
            matches!(result, CheckResult::Ask | CheckResult::Denied(_)),
            "{cmd:?} must NOT be silently allowed; got {result:?}",
        );
    }

    // Hard denies stay hard denies.
    for cmd in [
        "rm -rf /etc",
        "sudo rm -rf /usr",
        "dd if=/dev/zero of=/dev/sda",
    ] {
        let result = checker.check("bash", cmd);
        assert!(
            matches!(result, CheckResult::Denied(_)),
            "{cmd:?} must remain hard-denied; got {result:?}",
        );
    }
}

/// dirge-sm9w: memory tool is auto-approved in Standard mode for
/// all four actions. Writes are scoped to `~/.dirge/memories/`
/// (no arbitrary filesystem access); the agent can only add /
/// replace / remove its own entries. The per-action prompt was
/// friction without security value.
#[test]
fn memory_tool_standard_mode_auto_approved() {
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp")),
    );
    for action in ["view", "add", "replace", "remove"] {
        let result = checker.check("memory", action);
        assert!(
            matches!(result, CheckResult::Allowed),
            "memory.{action} must auto-allow in Standard; got {result:?}",
        );
    }
}

/// dirge-sm9w: Restrictive mode (`-R`) still prompts for memory.
/// Restrictive's contract is "every action confirms" — the
/// builtin Allow rule installed for Standard/Accept must be
/// demoted back to Ask here.
#[test]
fn memory_tool_restrictive_mode_still_prompts() {
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Restrictive,
        Some(std::path::PathBuf::from("/tmp")),
    );
    for action in ["view", "add", "replace", "remove"] {
        let result = checker.check("memory", action);
        assert!(
            matches!(result, CheckResult::Ask),
            "memory.{action} must Ask in Restrictive; got {result:?}",
        );
    }
}

/// dirge-sm9w (regression): Yolo mode short-circuits the entire
/// permission stack — memory must Allow without consulting any
/// rule. This already worked before sm9w; pinned here so the
/// new builtin-allow + Restrictive demotion can't accidentally
/// break it.
#[test]
fn memory_tool_yolo_short_circuits() {
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Yolo,
        Some(std::path::PathBuf::from("/tmp")),
    );
    for action in ["view", "add", "replace", "remove"] {
        let result = checker.check("memory", action);
        assert!(
            matches!(result, CheckResult::Allowed),
            "memory.{action} must Allow in Yolo; got {result:?}",
        );
    }
}
