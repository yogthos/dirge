use super::*;
use crate::permission::{Action, OpSpec, PermissionConfig, RuleConfig};

/// Concise config-rule constructor for tests.
fn rule(op: OpSpec, m: &str, effect: Action) -> RuleConfig {
    RuleConfig {
        op,
        pattern: m.to_string(),
        effect,
        tool: None,
    }
}

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
        // skill is auto-allowed on the same rationale (scoped to the
        // agent's skills dir; Restrictive still demotes its writes).
        "skill",
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
    let config = PermissionConfig {
        rules: vec![rule(OpSpec::Edit, "**", Action::Deny)],
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
    // F2 dissolution: write/edit/apply_patch now share Operation::Edit,
    // so the `edit: { "**": deny }` rule governs ALL THREE — a write
    // anywhere (even outside cwd) is denied by the same `**` rule. The
    // old aliasing special-case is gone; this falls out of the shared
    // operation.
    assert!(matches!(
        checker.check_path("write", "/opt/elsewhere/x.rs"),
        CheckResult::Denied(_)
    ));
    assert!(matches!(
        checker.check_path("apply_patch", "/opt/elsewhere/x.rs"),
        CheckResult::Denied(_)
    ));
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
    let config = PermissionConfig {
        rules: vec![rule(OpSpec::Edit, "/tmp/proj/build/**", Action::Deny)],
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
    let config = PermissionConfig {
        rules: vec![rule(OpSpec::Read, "/etc/**", Action::Deny)],
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

/// Op-based rules govern any tool, including ones with no dedicated
/// handling (a plugin tool → `Operation::Other`) and network tools.
/// Last-match-wins, and the op selects which calls a rule covers.
#[test]
fn op_rules_govern_arbitrary_and_network_tools() {
    let config = PermissionConfig {
        rules: vec![
            // A network rule denying websearch.
            rule(OpSpec::Network, "**", Action::Deny),
            // A catch-all denying the unknown plugin tool's input.
            RuleConfig {
                op: OpSpec::Any,
                pattern: "dangerous".to_string(),
                effect: Action::Deny,
                tool: Some("plugin_xyz".to_string()),
            },
        ],
        ..Default::default()
    };

    let mut checker = PermissionChecker::new(
        &config,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp")),
    );

    // Unknown plugin tool, narrowed by `tool`.
    assert!(matches!(
        checker.check("plugin_xyz", "dangerous"),
        CheckResult::Denied(_)
    ));
    // Network op rule governs websearch.
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
    // Set question (a Meta op) to Ask explicitly so Accept's
    // coercion path is exercised.
    let config = PermissionConfig {
        rules: vec![rule(OpSpec::Meta, "*", Action::Ask)],
        ..Default::default()
    };
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
    let config = PermissionConfig {
        rules: vec![rule(OpSpec::Mcp, "mcp_tool:lattice:*", Action::Allow)],
        ..Default::default()
    };
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

// dirge-fdvw: `/allow remove <n>` must revoke the grant from the ENGINE
// allowlist (the runtime source of truth that SessionAllowlistPolicy
// reads), not just the display list. Pre-fix, removal left the engine
// entry intact, so a "revoked" grant kept auto-allowing.
#[test]
fn remove_session_allowlist_revokes_engine_grant() {
    let mut cfg = PermissionConfig::default();
    cfg.default = Some(Action::Ask);
    let mut checker = PermissionChecker::new(
        &cfg,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/cwd-off-test-axis")),
    );
    checker.add_session_allowlist("write".to_string(), "/probe/src/**");
    assert!(
        matches!(
            checker.check_path("write", "/probe/src/main.rs"),
            CheckResult::Allowed
        ),
        "grant should auto-allow before removal"
    );

    let removed = checker.remove_session_allowlist_at(0);
    assert!(removed.is_some(), "removal should return the removed entry");

    assert!(
        !matches!(
            checker.check_path("write", "/probe/src/main.rs"),
            CheckResult::Allowed
        ),
        "removed grant must NOT still auto-allow via the engine allowlist",
    );
    assert!(
        !checker.is_session_allowed("write", "/probe/src/main.rs"),
        "engine allowlist must no longer report the grant"
    );
}

// dirge-x0l1: the permission config rejects unknown fields so a stale
// or typo'd config surfaces loudly instead of silently degrading to a
// permissive default.
#[test]
fn permission_config_rejects_unknown_fields() {
    // A valid op-based config parses.
    let ok: Result<PermissionConfig, _> =
        serde_json::from_str(r#"{"*":"ask","rules":[{"op":"edit","match":"**","effect":"deny"}]}"#);
    assert!(ok.is_ok(), "valid config must parse: {ok:?}");

    // A legacy per-tool key (or any typo) is rejected.
    let legacy: Result<PermissionConfig, _> = serde_json::from_str(r#"{"edit":{"**":"deny"}}"#);
    assert!(
        legacy.is_err(),
        "legacy/unknown permission key must be rejected, got {legacy:?}"
    );

    // A rule with an unknown field (`pattern` instead of `match`) is rejected.
    let bad_rule: Result<PermissionConfig, _> =
        serde_json::from_str(r#"{"rules":[{"op":"edit","pattern":"**","effect":"deny"}]}"#);
    assert!(
        bad_rule.is_err(),
        "rule with unknown field must be rejected, got {bad_rule:?}"
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
    assert!(crate::permission::engine::pattern_for_tool("bash", "cd *").matches("cd /a/b/c"));
    assert!(!crate::permission::engine::pattern_for_tool("read", "cd *").matches("cd /a/b/c"));
    assert!(crate::permission::engine::pattern_for_tool("read", "cd *").matches("cd file"));
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
        // dirge-9zbd: general interpreters run arbitrary (possibly remote)
        // code, so they prompt once instead of being pre-trusted.
        "python3 script.py",
        "python -c \"import os; os.system('x')\"",
        "node index.js",
        "node -e \"require('child_process').exec('x')\"",
        "npx eslint .",
        "npx some-remote-tool",
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
/// Restrictive's contract is "every WRITE action confirms" —
/// the builtin Allow rule installed for Standard/Accept must
/// be demoted back to Ask for `add`/`replace`/`remove`, but
/// the read action (`view`) follows the read-only convention
/// (read, grep, list_symbols, … all pass through as Allow in
/// restrictive — restrictive gates side effects, not
/// observation).
#[test]
fn memory_tool_restrictive_mode_writes_prompt_reads_allow() {
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Restrictive,
        Some(std::path::PathBuf::from("/tmp")),
    );
    // view is a read — passes through.
    let result = checker.check("memory", "view");
    assert!(
        matches!(result, CheckResult::Allowed),
        "memory.view must Allow in Restrictive (it's a read); got {result:?}",
    );
    // add/replace/remove are writes — demoted to Ask.
    for action in ["add", "replace", "remove"] {
        let result = checker.check("memory", action);
        assert!(
            matches!(result, CheckResult::Ask),
            "memory.{action} must Ask in Restrictive (it's a write); got {result:?}",
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

// ── dirge-yevn regression coverage ──────────────────────────────
//
// Complaint #1: Accept mode is documented as "auto-approves inside
// the working directory" but users still get permission dialogs on
// edit/write for files INSIDE cwd. The CWD-scoped builtin-allow
// rule installer uses `canonicalize_for_cache` so its pattern is
// the canonical form (e.g. /private/tmp/proj on macOS where /tmp
// symlinks to /private/tmp). But `check_path` matches both the
// canonical AND the literal raw path against installed rules — and
// `is_external_path` ALREADY handles the symlink-root case for
// path classification. The Accept-mode short-circuit, however,
// is shaped: `match base { Ask => ... if !external ... Allow }`.
// If `base = Ask` (e.g. due to a user-supplied catch-all `*: ask`
// rule that wins last-match-wins), the short-circuit fires. But
// when the cwd-allow rule is present AND no other rule intercepts,
// `base` is already Allow.  The actually-reported friction in
// Accept mode appears when the user lands inside the CWD via a
// symlink (working_dir is the symlink path, but the canonicalized
// path comes back as the realpath form, or vice versa) — in that
// case the CWD-allow pattern (`canonical/**`) and the path under
// check don't both line up.

/// Complaint #1, direct repro: in Accept mode, an edit to a file
/// inside the working directory must be auto-allowed without any
/// permission dialog. This is the documented contract.
#[test]
fn accept_mode_auto_approves_edit_inside_cwd() {
    let dir = std::env::temp_dir().join(format!("dirge-yevn-edit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Accept,
        Some(dir.clone()),
    );
    let target = dir.join("src").join("foo.rs");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "").unwrap();

    let result = checker.check_path("edit", target.to_str().unwrap());
    assert!(
        matches!(result, CheckResult::Allowed),
        "Accept mode must auto-approve edit inside cwd; got {:?}",
        result,
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Complaint #1, direct repro: Accept mode must auto-approve
/// write inside cwd. Same shape as the edit test — pin both
/// since the F2 alias merge in `enforce` makes write go through
/// both rule sets.
#[test]
fn accept_mode_auto_approves_write_inside_cwd() {
    let dir = std::env::temp_dir().join(format!("dirge-yevn-write-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Accept,
        Some(dir.clone()),
    );

    // Existing file
    let existing = dir.join("src").join("foo.rs");
    std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
    std::fs::write(&existing, "").unwrap();
    assert!(
        matches!(
            checker.check_path("write", existing.to_str().unwrap()),
            CheckResult::Allowed
        ),
        "Accept mode must auto-approve write to existing file inside cwd",
    );

    // New file (path does not yet exist on disk — common case)
    let newfile = dir.join("src").join("new-file.rs");
    let result = checker.check_path("write", newfile.to_str().unwrap());
    assert!(
        matches!(result, CheckResult::Allowed),
        "Accept mode must auto-approve write to new file inside cwd; got {:?}",
        result,
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Complaint #1 with the symlinked-cwd shape: the working_dir is a
/// SYMLINK to a real directory. The user passes file paths in
/// whichever form (symlinked or real). Either way, the file is
/// inside the project — Accept mode must auto-approve.
///
/// This is the realistic macOS scenario: `/tmp` symlinks to
/// `/private/tmp`. If a user starts dirge with `--accept-all` in
/// `/tmp/proj`, but the LLM/agent ends up passing
/// `/private/tmp/proj/...` (canonicalized), the check must still
/// say Allow.
#[test]
fn accept_mode_auto_approves_edit_inside_cwd_through_symlink() {
    // Build:  /tmp/dirge-yevn-symlink-<pid>/real/        (real cwd)
    //         /tmp/dirge-yevn-symlink-<pid>/link  ->  real
    // Then check writes via BOTH the symlink form and the real form.
    let root = std::env::temp_dir().join(format!("dirge-yevn-symlink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let real = root.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = root.join("link");

    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&real, &link).unwrap();

    // Start dirge with cwd = the symlink form (what a user types).
    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Accept,
        Some(link.clone()),
    );

    // A file inside the project, addressed via the symlink path.
    let src = real.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let via_link = link.join("src").join("foo.rs");
    std::fs::write(real.join("src").join("foo.rs"), "").unwrap();

    let result = checker.check_path("edit", via_link.to_str().unwrap());
    assert!(
        matches!(result, CheckResult::Allowed),
        "edit via symlinked cwd path must Allow in Accept mode; got {:?}",
        result,
    );

    // Same file addressed via the canonical (real) path.
    let via_real = real.join("src").join("foo.rs");
    let result = checker.check_path("edit", via_real.to_str().unwrap());
    assert!(
        matches!(result, CheckResult::Allowed),
        "edit via canonical cwd path must Allow in Accept mode; got {:?}",
        result,
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Complaint #2, direct repro: pick "Allow always" for a folder
/// (folder-glob pattern `/folder/**`), and the next write into the
/// SAME folder must hit the session allowlist — no re-prompt.
#[test]
fn allow_always_folder_persists_in_session_allowlist() {
    let mut cfg = PermissionConfig::default();
    cfg.default = Some(Action::Ask);
    // Use a working_dir OUTSIDE the test paths so the CWD-allow
    // installer doesn't intercept and mask the session allowlist
    // under test.
    let mut checker = PermissionChecker::new(
        &cfg,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/cwd-off-test-axis")),
    );

    // User picks "Allow always" for /project/src/foo.rs. The UI
    // calls `suggest_pattern` which yields `/project/src/**`,
    // then `add_session_allowlist`.
    checker.add_session_allowlist("write".to_string(), "/project/src/**");

    // First write that triggered the dialog — same path — must Allow.
    assert!(matches!(
        checker.check_path("write", "/project/src/foo.rs"),
        CheckResult::Allowed
    ));
    // Subsequent writes to the SAME folder must Allow.
    assert!(matches!(
        checker.check_path("write", "/project/src/bar.rs"),
        CheckResult::Allowed
    ));
}

/// Complaint #2, the exact user-reported behavior: after picking
/// "Allow always" for a write in `<cwd>/src/foo.rs`, the next
/// write to `<cwd>/src/bar.rs` must also Allow — no re-prompt.
/// The session allowlist entry is `<cwd>/src/**`.
#[test]
fn allow_always_folder_covers_subsequent_writes_to_same_folder() {
    let mut cfg = PermissionConfig::default();
    cfg.default = Some(Action::Ask);
    let mut checker = PermissionChecker::new(
        &cfg,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/cwd-off-test-axis")),
    );
    checker.add_session_allowlist("write".to_string(), "/project/src/**");

    // First write that the user dialog approved.
    assert!(matches!(
        checker.check_path("write", "/project/src/foo.rs"),
        CheckResult::Allowed
    ));
    // The complaint: same folder, different file → must NOT re-prompt.
    let result = checker.check_path("write", "/project/src/bar.rs");
    assert!(
        matches!(result, CheckResult::Allowed),
        "subsequent write to same folder must hit allowlist (no re-prompt); got {:?}",
        result,
    );
    // And nested writes (deeper subdirs of the same folder) also Allow.
    let result = checker.check_path("write", "/project/src/agent/baz.rs");
    assert!(
        matches!(result, CheckResult::Allowed),
        "nested write under allowlisted folder must Allow; got {:?}",
        result,
    );

    // Edit / apply_patch must also Allow (write↔edit alias).
    assert!(matches!(
        checker.check_path("edit", "/project/src/bar.rs"),
        CheckResult::Allowed
    ));
    assert!(matches!(
        checker.check_path("apply_patch", "/project/src/bar.rs"),
        CheckResult::Allowed
    ));
}

/// Complaint #2 with symlinked working dir: the user picks "Allow
/// always" for a write inside `<symlinked-cwd>/src/`. The
/// pattern stored is whatever `suggest_pattern` derived from the
/// input the LLM passed. The NEXT write to the same folder may
/// arrive in the CANONICAL form (after canonicalization upstream)
/// or vice versa — the session-allowlist check must still hit.
#[test]
fn allow_always_folder_resolves_through_symlinks() {
    let root = std::env::temp_dir().join(format!(
        "dirge-yevn-allowlist-symlink-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let real = root.join("real");
    let src = real.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let link = root.join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&real, &link).unwrap();

    let mut cfg = PermissionConfig::default();
    cfg.default = Some(Action::Ask);
    // Put the cwd somewhere OFF the test paths so the CWD-allow
    // rule doesn't mask the allowlist behavior under test.
    let mut checker = PermissionChecker::new(
        &cfg,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/cwd-off-test-axis")),
    );

    // User approves a write to a file addressed via the symlink.
    // `suggest_pattern` for `<link>/src/foo.rs` would derive
    // `<link>/src/**`.
    let approved_input = link.join("src").join("foo.rs");
    let approved_parent = approved_input.parent().unwrap();
    let approved_pattern = format!("{}/**", approved_parent.display());
    checker.add_session_allowlist("write".to_string(), &approved_pattern);

    // Original file (created so canonicalize succeeds) addressed
    // via the CANONICAL (real) form — must Allow despite the
    // allowlist pattern being keyed on the symlink form.
    let probe_existing = real.join("src").join("foo.rs");
    std::fs::write(&probe_existing, "").unwrap();
    let via_real = real.join("src").join("foo.rs");
    let result = checker.check_path("write", via_real.to_str().unwrap());
    assert!(
        matches!(result, CheckResult::Allowed),
        "write to canonical-form path must Allow via symlinked allowlist entry; got {:?}",
        result,
    );

    // And the original symlinked form still allows.
    let via_link = link.join("src").join("foo.rs");
    let result = checker.check_path("write", via_link.to_str().unwrap());
    assert!(
        matches!(result, CheckResult::Allowed),
        "write to symlinked-form path must Allow via symlinked allowlist entry; got {:?}",
        result,
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ============================================================
// Regression coverage for the 6 reported permission/UX issues.
// Triage (2026-05) found 5 already fixed on main (write /dev/null,
// memory transparency, in-cwd read/write, the escape-cwd security
// boundary, bash sticky-allow); only `skill` transparency was a
// genuine gap. These tests lock in all six so none regress.
// ============================================================
mod reported_permission_ux_regressions {
    use super::*;

    fn checker_in(dir: &str) -> PermissionChecker {
        PermissionChecker::new(
            &PermissionConfig::default(),
            SecurityMode::Standard,
            Some(std::path::PathBuf::from(dir)),
        )
    }

    // Issue #1: write to /dev/null should not prompt.
    #[test]
    fn probe_write_dev_null_allowed() {
        let mut c = checker_in("/tmp");
        assert!(
            matches!(c.check_path("write", "/dev/null"), CheckResult::Allowed),
            "write /dev/null should be Allowed, got {:?}",
            c.check_path("write", "/dev/null")
        );
    }

    // Issue #2: memory operations should not prompt.
    #[test]
    fn probe_memory_actions_allowed() {
        for action in ["view", "add", "replace", "remove"] {
            let mut c = checker_in("/tmp");
            assert!(
                matches!(c.check("memory", action), CheckResult::Allowed),
                "memory {action} should be Allowed, got {:?}",
                c.check("memory", action)
            );
        }
    }

    // Issue #4: skill operations should not prompt in Standard mode.
    // Read keywords arrive bare (load/list); write actions arrive as
    // `{action}:{name}` (create/edit/patch).
    #[test]
    fn probe_skill_actions_allowed() {
        for action in ["load", "list", "create:foo", "edit:foo", "patch:foo"] {
            let mut c = checker_in("/tmp");
            assert!(
                matches!(c.check("skill", action), CheckResult::Allowed),
                "skill {action} should be Allowed, got {:?}",
                c.check("skill", action)
            );
        }
    }

    // Restrictive mode keeps its "every write confirms" contract:
    // skill read actions stay transparent, but create/edit/patch
    // demote back to Ask (mirrors memory's view-vs-mutate split).
    #[test]
    fn probe_skill_restrictive_demotes_writes_not_reads() {
        let mk = || {
            PermissionChecker::new(
                &PermissionConfig::default(),
                SecurityMode::Restrictive,
                Some(std::path::PathBuf::from("/tmp")),
            )
        };
        for read in ["load", "list"] {
            assert!(
                matches!(mk().check("skill", read), CheckResult::Allowed),
                "skill {read} must stay Allowed under Restrictive, got {:?}",
                mk().check("skill", read)
            );
        }
        for write in ["create:foo", "edit:foo", "patch:foo", "delete:foo"] {
            assert!(
                matches!(mk().check("skill", write), CheckResult::Ask),
                "skill {write} must demote to Ask under Restrictive, got {:?}",
                mk().check("skill", write)
            );
        }
    }

    // Issue #5 (ROOT CAUSE): git worktrees are created as SIBLINGS
    // (`../name`, see git_worktree::create), OUTSIDE the original
    // repo. When dirge switches into a worktree it MUST call
    // set_working_dir, or the in-cwd write-allow glob stays anchored
    // to the original repo and EVERY write inside the worktree is
    // classified external -> prompts on every edit. This pins the
    // re-anchoring contract the worktree cwd-change sites must honor
    // (cmd_worktree create, wt-exit, and merge-return in done.rs).
    #[test]
    fn probe_worktree_sibling_switch_reanchors_cwd_allow() {
        let mut c = checker_in("/repo/main");
        // Before the switch: a write in the sibling worktree is external.
        assert!(
            matches!(
                c.check_path("write", "/repo/wt/src/foo.rs"),
                CheckResult::Ask
            ),
            "pre-switch: sibling-worktree write should be external/Ask",
        );
        // Switching into the worktree (what the fixed UI must do).
        c.set_working_dir("/repo/wt");
        assert!(
            matches!(
                c.check_path("write", "/repo/wt/src/foo.rs"),
                CheckResult::Allowed
            ),
            "post-switch: in-worktree write must be auto-allowed",
        );
        // And the old repo is now external — no privilege carry-over.
        assert!(
            matches!(
                c.check_path("write", "/repo/main/src/x.rs"),
                CheckResult::Ask
            ),
            "post-switch: old-repo write should now be external/Ask",
        );
    }

    // Issue #5a: reads inside the project folder should not prompt.
    #[test]
    fn probe_read_inside_cwd_allowed() {
        let mut c = checker_in("/tmp/proj");
        assert!(
            matches!(
                c.check_path("read", "/tmp/proj/src/main.rs"),
                CheckResult::Allowed
            ),
            "read inside cwd should be Allowed"
        );
    }

    // Issue #5b: writes inside the project folder should not prompt.
    #[test]
    fn probe_write_inside_cwd_allowed() {
        let mut c = checker_in("/tmp/proj");
        let r = c.check_path("write", "/tmp/proj/src/main.rs");
        assert!(
            matches!(r, CheckResult::Allowed),
            "write inside cwd should be Allowed, got {:?}",
            r
        );
    }

    // SECURITY BOUNDARY: writes OUTSIDE cwd must still prompt.
    #[test]
    fn probe_write_outside_cwd_still_asks() {
        let mut c = checker_in("/tmp/proj");
        let r = c.check_path("write", "/etc/evil.conf");
        assert!(
            matches!(r, CheckResult::Ask),
            "write OUTSIDE cwd must still Ask, got {:?}",
            r
        );
    }

    // Issue #6: sticky-allow — after always-allowing `cargo *`,
    // a similar cargo command should not re-prompt.
    #[test]
    fn probe_sticky_allow_bash_similar_command() {
        let mut c = checker_in("/tmp");
        c.add_session_allowlist("bash".to_string(), "cargo *");
        assert!(
            matches!(
                c.check("bash", "cargo test --bin dirge"),
                CheckResult::Allowed
            ),
            "sticky-allow cargo * should match cargo test --bin dirge"
        );
        assert!(
            matches!(c.check("bash", "cargo build"), CheckResult::Allowed),
            "sticky-allow cargo * should match cargo build"
        );
    }

    // Issue #3 (real cause): parallel-tool prompt coalescing. After
    // the user "allow always"-es one queued tool call, the sibling
    // calls already parked on a permission decision should be
    // auto-resolved against the new allowlist instead of re-flashing
    // the Alert avatar. `session_allows_now` is the side-effect-free
    // probe the UI uses for that — it must match the same raw-vs-path
    // and relative-vs-absolute forms the real checks accept.
    #[test]
    fn probe_session_allows_now_coalesces_after_allow_always() {
        // Raw tool (bash): a `cargo *` allow-always covers a queued
        // sibling `cargo build`, but not an unrelated `git status`.
        let mut c = checker_in("/tmp");
        assert!(
            !c.session_allows_now("bash", "cargo build"),
            "nothing allowed yet"
        );
        c.add_session_allowlist("bash".to_string(), "cargo *");
        assert!(
            c.session_allows_now("bash", "cargo build"),
            "queued cargo sibling must be coalesced after allow-always",
        );
        assert!(
            !c.session_allows_now("bash", "git status"),
            "unrelated queued command must still prompt",
        );

        // Path tool: a relative `sub/**` allow-always must match an
        // absolute probe to a sibling file in the same subtree, while
        // a path outside the working dir must NOT be coalesced.
        let proj = std::env::temp_dir().join(format!("dirge-coalesce-{}", std::process::id()));
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        let mut pc = PermissionChecker::new(
            &PermissionConfig::default(),
            SecurityMode::Standard,
            Some(proj.clone()),
        );
        pc.set_working_dir(proj.to_str().unwrap());
        pc.add_session_allowlist("write".to_string(), "sub/**");
        let abs = proj.join("sub/other.rs");
        assert!(
            pc.session_allows_now("write", abs.to_str().unwrap()),
            "absolute sibling write must be coalesced by relative allow-always",
        );
        assert!(
            !pc.session_allows_now("write", "/etc/evil.conf"),
            "path outside the working dir must not be coalesced",
        );
        let _ = std::fs::remove_dir_all(&proj);
    }
}

/// `/why` / `explain`: the rendered trace names the final effect, the
/// deciding policy, and the applicable policies' votes.
#[test]
fn explain_renders_decision_trace() {
    let checker = PermissionChecker::new(
        &PermissionConfig::default(),
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp/proj")),
    );
    // An unfamiliar bash command falls to the default → Ask.
    let report = checker.explain("bash", "frobnicate --hard", false);
    assert!(report.contains("frobnicate"), "names the input: {report}");
    assert!(report.contains("Ask"), "shows the effect: {report}");
    assert!(
        report.contains("default"),
        "names the deciding policy: {report}"
    );

    // A read is transparently allowed by the builtin-allow policy.
    let report = checker.explain("read", "/tmp/proj/src/main.rs", true);
    assert!(report.contains("Allow"), "read allowed: {report}");
    assert!(
        report.contains("builtin-allow"),
        "names builtin-allow as decider: {report}",
    );
}
