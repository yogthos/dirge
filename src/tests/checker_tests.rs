use crate::permission::checker::{CheckResult, PermissionChecker};
use crate::permission::{Action, OpSpec, PermissionConfig, RuleConfig, SecurityMode};

/// Concise config-rule constructor for tests.
fn rule(op: OpSpec, m: &str, effect: Action) -> RuleConfig {
    RuleConfig {
        op,
        pattern: m.to_string(),
        effect,
        tool: None,
    }
}

fn make_checker(mode: SecurityMode) -> PermissionChecker {
    PermissionChecker::new(
        &PermissionConfig::default(),
        mode,
        Some(std::path::PathBuf::from("/home/user/project")),
    )
}

// --- SecurityMode behavior ---

#[test]
fn yolo_allows_everything() {
    let mut checker = make_checker(SecurityMode::Yolo);
    assert_eq!(checker.check("bash", "rm -rf /"), CheckResult::Allowed);
    assert_eq!(checker.check("write", "/etc/passwd"), CheckResult::Allowed);
}

#[test]
fn restrictive_makes_unconfigured_tool_ask() {
    let mut checker = make_checker(SecurityMode::Restrictive);
    let result = checker.check("some_tool", "any input");
    assert!(matches!(result, CheckResult::Ask));
}

#[test]
fn standard_asks_unknown_tool_with_default() {
    // M4 (dirge-ojn): unconfigured tools now Ask by default (was
    // Allow). The renamed test pins the new contract — anything
    // dirge doesn't ship a builtin-allow rule for AND the user
    // hasn't configured prompts the user.
    let mut checker = make_checker(SecurityMode::Standard);
    let result = checker.check("some_tool", "any input");
    assert!(matches!(result, CheckResult::Ask));
}

#[test]
fn accept_auto_allows_inside_working_dir() {
    let config = PermissionConfig {
        rules: vec![rule(OpSpec::Edit, "**", Action::Ask)],
        ..PermissionConfig::default()
    };
    let mut checker = PermissionChecker::new(
        &config,
        SecurityMode::Accept,
        Some(std::path::PathBuf::from("/home/user/project")),
    );
    let result = checker.check_path("write", "/home/user/project/src/main.rs");
    assert!(matches!(result, CheckResult::Allowed));
}

#[test]
fn accept_asks_for_external_path() {
    let mut checker = make_checker(SecurityMode::Accept);
    let external_path = if cfg!(windows) {
        "D:\\outside\\file.txt"
    } else {
        "/etc/config.conf"
    };
    let result = checker.check_path("write", external_path);
    assert!(
        matches!(result, CheckResult::Ask),
        "expected Ask, got {:?} for path: {}",
        result,
        external_path,
    );
}

// --- Deny rules ---

#[test]
fn deny_rule_blocks_regardless_of_mode() {
    let mut checker = make_checker(SecurityMode::Standard);
    let result = checker.check("bash", "rm -rf /home/user/project");
    assert!(matches!(result, CheckResult::Denied(_)));
}

#[test]
fn deny_rule_not_blocked_by_yolo() {
    let mut checker = make_checker(SecurityMode::Yolo);
    let result = checker.check("bash", "rm -rf /home/user/project");
    assert!(matches!(result, CheckResult::Allowed));
}

/// Regression: deny messages must name the rule pattern that matched
/// so the user knows what to edit. Previously bare
/// `"Blocked by permission rules"` left them with no path forward.
#[test]
fn deny_message_names_the_matching_rule() {
    let mut checker = make_checker(SecurityMode::Standard);
    let result = checker.check("bash", "rm -rf /home/user/project");
    match result {
        CheckResult::Denied(msg) => {
            assert!(
                msg.contains("rm") || msg.contains("rule"),
                "deny message must reference the rule: {msg}",
            );
            assert!(
                !msg.eq("Blocked by permission rules"),
                "deny message must not be generic: {msg}",
            );
        }
        other => panic!("expected Denied; got {other:?}"),
    }
}

/// Doom-loop deny message must name the offending tool + call.
#[test]
fn doom_loop_deny_names_the_call() {
    use crate::permission::{Action, PermissionConfig};
    let config = PermissionConfig {
        doom_loop: Some(Action::Deny),
        ..PermissionConfig::default()
    };
    let mut checker = PermissionChecker::new(
        &config,
        SecurityMode::Standard,
        Some(std::path::PathBuf::from("/tmp")),
    );
    // Loop guard: an Ask op (echo hi isn't a default-allowed bash
    // command) retried past the threshold (3) is hard-denied on the
    // 4th identical prompted call. (The new guard never gates an
    // ALLOWED op and always hard-denies a true retry loop, regardless
    // of the legacy `doom_loop` action.)
    assert!(matches!(
        checker.check("bash", "frobnicate xyz"),
        CheckResult::Ask
    ));
    assert!(matches!(
        checker.check("bash", "frobnicate xyz"),
        CheckResult::Ask
    ));
    assert!(matches!(
        checker.check("bash", "frobnicate xyz"),
        CheckResult::Ask
    ));
    let result = checker.check("bash", "frobnicate xyz");
    match result {
        CheckResult::Denied(msg) => {
            assert!(msg.contains("Doom loop"), "must say Doom loop: {msg}");
            assert!(
                msg.contains("bash") && msg.contains("frobnicate"),
                "must name tool + call preview: {msg}",
            );
        }
        other => panic!("expected Denied on the 4th identical Ask; got {other:?}"),
    }
}

// --- Doom loop detection ---

#[test]
fn doom_loop_triggers_after_three_repeated_calls() {
    let mut checker = make_checker(SecurityMode::Standard);
    // Use a command that genuinely Asks (not in the default allow rules).
    // dirge-9zbd: bare `ls` is now auto-Allowed by `ls **` (the rewrite
    // makes ` **` args optional), so it never reaches the doom-loop path.
    checker.check("bash", "frobnicate xyz");
    checker.check("bash", "frobnicate xyz");
    let result = checker.check("bash", "frobnicate xyz");
    assert!(matches!(result, CheckResult::Ask));
}

#[test]
fn doom_loop_does_not_trigger_before_three() {
    let mut checker = make_checker(SecurityMode::Standard);
    // `ls -la` matches the `ls **` default allow rule. Two identical
    // allowed calls stay Allowed — the doom-loop counter only escalates
    // repeated *Ask* calls, not allowed ones. (dirge-9zbd: bare `ls` is
    // also Allowed now, but keep the args form to be explicit.)
    checker.check("bash", "ls -la");
    let result = checker.check("bash", "ls -la");
    assert!(matches!(result, CheckResult::Allowed));
}

#[test]
fn doom_loop_resets_for_different_inputs() {
    let mut checker = make_checker(SecurityMode::Standard);
    checker.check("bash", "ls");
    checker.check("bash", "ls");
    checker.check("bash", "pwd");
    let result = checker.check("bash", "pwd");
    assert!(matches!(result, CheckResult::Allowed));
}

// --- Session allowlist ---

#[test]
fn session_allowlist_bypasses_rules() {
    let mut checker = make_checker(SecurityMode::Restrictive);
    checker.add_session_allowlist("bash".into(), "cargo test **");
    let result = checker.check("bash", "cargo test --all");
    assert!(matches!(result, CheckResult::Allowed));
}

#[test]
fn session_allowlist_is_tool_specific() {
    let mut checker = make_checker(SecurityMode::Restrictive);
    checker.add_session_allowlist("read".into(), "**");
    assert!(matches!(
        checker.check("read", "/etc/passwd"),
        CheckResult::Allowed
    ));
    assert!(matches!(
        checker.check("write", "some/file.txt"),
        CheckResult::Ask
    ));
}

// --- External path detection ---

#[test]
fn external_absolute_path_outside_cwd_is_detected() {
    let mut checker = make_checker(SecurityMode::Standard);
    let external_path = if cfg!(windows) {
        "D:\\outside\\secret.txt"
    } else {
        "/etc/shadow"
    };
    let result = checker.check_path("write", external_path);
    assert!(
        matches!(result, CheckResult::Ask),
        "expected Ask, got {:?}",
        result,
    );
}

#[test]
fn relative_path_is_not_external() {
    let mut checker = make_checker(SecurityMode::Accept);
    let result = checker.check_path("read", "src/lib.rs");
    assert!(matches!(result, CheckResult::Allowed));
}

/// F18: a relative path with `..` traversal that escapes the
/// working directory IS external — previously it was treated as
/// internal because is_external_path returned false on
/// `!is_absolute`. In Accept mode that auto-allowed `../../secret`.
#[test]
fn relative_path_escaping_cwd_is_external() {
    // Build a deeper working dir so `../../../escaped` lands ABOVE
    // the working dir but inside a still-existing ancestor.
    let base = std::env::temp_dir().join(format!("dirge-f18-extern-{}", std::process::id()));
    let cwd = base.join("a/b/c"); // 3 levels deep under `base`
    let escaped_dir = base.join("escaped");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&escaped_dir).unwrap();
    let escaped_file = escaped_dir.join("file.rs");
    std::fs::write(&escaped_file, "").unwrap();

    // M4 (dirge-ojn): `read` has a builtin `**: allow` now, so the
    // escape upgrade can't be tested through it (the builtin matches
    // before the catch-all condition checks for `matched.is_empty()`).
    // Use `write` instead — no builtin-allow, so the original F18
    // semantics apply: in-tree write is Ask→Allow under Accept,
    // external write is Ask (no ext_dir rule installed).
    let config = PermissionConfig {
        rules: vec![rule(OpSpec::Edit, "**", Action::Ask)],
        ..PermissionConfig::default()
    };
    let mut checker = PermissionChecker::new(&config, SecurityMode::Accept, Some(cwd.clone()));

    // In-tree relative path → still internal → auto-allow in Accept.
    std::fs::write(cwd.join("local.rs"), "").unwrap();
    let internal = checker.check_path("write", "local.rs");
    assert!(
        matches!(internal, CheckResult::Allowed),
        "in-tree path should auto-allow in Accept: got {:?}",
        internal,
    );

    // `../../../escaped/file.rs` escapes cwd → external → Ask
    // (no external_directory rule configured to allow it).
    let escape = checker.check_path("write", "../../../escaped/file.rs");
    assert!(
        matches!(escape, CheckResult::Ask),
        "escape attempt must surface as Ask in Accept; got {:?}",
        escape,
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// `/dev/null` is a harmless bit-bucket — writes discard data, reads
/// return immediate EOF. It must be allowed for ALL tools without
/// prompting, regardless of security mode (except Yolo which already
/// allows everything). This test pins the post-fix contract: a write
/// to `/dev/null` outside the CWD does NOT trigger Ask/Deny.
#[test]
fn dev_null_is_always_allowed_for_all_tools() {
    let mut checker = make_checker(SecurityMode::Standard);
    // Writes are the crucial case — read-only tools already have `**`
    // builtin-allow. Write/edit/apply_patch only have CWD-scoped
    // allow, so /dev/null would hit the default Ask without this fix.
    for tool in ["write", "edit", "apply_patch", "read", "grep"] {
        let result = checker.check_path(tool, "/dev/null");
        assert!(
            matches!(result, CheckResult::Allowed),
            "{tool} /dev/null must be Allowed in Standard mode; got {result:?}",
        );
    }
    // Accept mode: same contract — no prompt for /dev/null.
    let mut checker_accept = make_checker(SecurityMode::Accept);
    for tool in ["write", "edit", "apply_patch"] {
        let result = checker_accept.check_path(tool, "/dev/null");
        assert!(
            matches!(result, CheckResult::Allowed),
            "{tool} /dev/null must be Allowed in Accept mode; got {result:?}",
        );
    }
    // Restrictive mode: same.
    let mut checker_restr = make_checker(SecurityMode::Restrictive);
    for tool in ["write", "edit", "apply_patch"] {
        let result = checker_restr.check_path(tool, "/dev/null");
        assert!(
            matches!(result, CheckResult::Allowed),
            "{tool} /dev/null must be Allowed in Restrictive mode; got {result:?}",
        );
    }
}

/// Session allowlist entries for path tools must actually take
/// effect on the next check — user bug report that "allow always"
/// didn't stick. This test pins that adding a session-allow entry
/// for a path tool makes the next check_path call return Allowed.
#[test]
fn session_allowlist_takes_effect_for_path_tool_on_next_check() {
    let mut checker = make_checker(SecurityMode::Standard);
    // Use a path that's definitely outside CWD and not special.
    let out_path = if cfg!(windows) {
        "C:\\Windows\\Temp\\test.txt"
    } else {
        "/tmp/allowlist_test.txt"
    };
    let before = checker.check_path("write", out_path);
    assert!(
        matches!(before, CheckResult::Ask),
        "baseline write to {out_path} must Ask; got {before:?}",
    );

    // Simulate "allow always" press: the UI would call
    // suggest_pattern("write", out_path) → parent/**.
    let pattern = if cfg!(windows) {
        "C:\\Windows\\Temp\\**"
    } else {
        "/tmp/**"
    };
    checker.add_session_allowlist("write".to_string(), pattern);

    // Now the same path must be allowed.
    let after = checker.check_path("write", out_path);
    assert!(
        matches!(after, CheckResult::Allowed),
        "after adding session allowlist entry {pattern}, write to {out_path} must be Allowed; got {after:?}",
    );

    // A nested path within the allowed subtree must also match.
    let nested = if cfg!(windows) {
        "C:\\Windows\\Temp\\subdir\\nested.txt"
    } else {
        "/tmp/subdir/nested.txt"
    };
    let nested_result = checker.check_path("write", nested);
    assert!(
        matches!(nested_result, CheckResult::Allowed),
        "nested path {nested} must be Allowed after session allowlist {pattern}; got {nested_result:?}",
    );
}

/// Re-prompt bug: when the user "allow always"es a path tool while the
/// LLM sent a RELATIVE path, `suggest_pattern` stores a relative glob
/// (e.g. `sub/**`). The next check_path always matches against the
/// canonical ABSOLUTE form (via resolve_absolute), so the relative
/// pattern never matched and the user got re-prompted. The fix anchors
/// the canonical-variant twin at the checker's working_dir.
#[test]
fn session_allowlist_relative_pattern_matches_absolute_check_inside_cwd() {
    // Real on-disk working dir so resolve_absolute / canonicalize work.
    let proj = std::env::temp_dir().join(format!(
        "dirge-relpat-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let sub = proj.join("sub");
    std::fs::create_dir_all(&sub).unwrap();

    let mut checker = PermissionChecker::new(
        &PermissionConfig::default(),
        // Restrictive so the in-cwd write isn't auto-allowed by the
        // CWD-scoped builtin rule — forces the test through the
        // session-allowlist path we actually care about.
        SecurityMode::Restrictive,
        Some(proj.clone()),
    );
    checker.set_working_dir(proj.to_str().unwrap());

    // Simulate "allow always" with the RELATIVE pattern that
    // suggest_pattern("write", "sub/file.rs") would produce.
    checker.add_session_allowlist("write".to_string(), "sub/**");

    // Subsequent call arrives as an ABSOLUTE path to a DIFFERENT file
    // in the same subtree (the realistic LLM behavior). Must be allowed
    // without re-prompting.
    let abs = sub.join("other.rs");
    let result = checker.check_path("write", abs.to_str().unwrap());
    assert!(
        matches!(result, CheckResult::Allowed),
        "absolute write to {abs:?} must be Allowed after relative `sub/**` allow-always; got {result:?}",
    );

    // Security boundary: a path OUTSIDE the working directory entirely
    // must still prompt — anchoring the relative `sub/**` pattern at
    // working_dir must not over-allow arbitrary absolute paths.
    let outside = if cfg!(windows) {
        "C:\\Windows\\Temp\\sub\\evil.txt".to_string()
    } else {
        "/tmp/sub/evil.txt".to_string()
    };
    let outside_result = checker.check_path("write", &outside);
    assert!(
        matches!(outside_result, CheckResult::Ask),
        "write outside the working dir must still Ask; the relative `sub/**` allow must anchor at cwd, not match any `/.../sub/*`; got {outside_result:?}",
    );

    let _ = std::fs::remove_dir_all(&proj);
}

// --- Config-driven rules ---

#[test]
fn explicit_granular_rules_take_effect() {
    let config = PermissionConfig {
        rules: vec![
            rule(OpSpec::Read, "*.md", Action::Allow),
            rule(OpSpec::Read, "*.rs", Action::Ask),
        ],
        ..PermissionConfig::default()
    };
    let mut checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
    assert_eq!(checker.check("read", "README.md"), CheckResult::Allowed);
    assert_eq!(checker.check("read", "main.rs"), CheckResult::Ask);
}
