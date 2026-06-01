//! Unit + integration tests for the bash tool. Split out of
//! `agent/tools/bash.rs` (dirge-4y4l stage 9a). `use super::*` pulls in
//! `BashTool` + `run_with_timeout` (re-imported from `bash::exec`), and
//! `use super::check::*` pulls in the parsing/permission helpers
//! (`check_bash_segments`, `quote_aware_split`, `coarse_*`,
//! `bash_mutation_targets`) extracted to `bash::check` in stage 9b.

use super::check::*;
use super::*;
use tokio::process::Command;
use tokio::time::Duration;

/// End-to-end: `background: true` returns immediately with a shell id,
/// registers the shell in the `BackgroundShellStore`, and streams the
/// command's output into the store's per-shell buffer as it runs.
#[tokio::test]
async fn background_bash_registers_shell_and_streams_output() {
    use crate::agent::tools::BashArgs;
    use crate::agent::tools::bg_shell::{BackgroundShellStore, ShellStatus};

    let store = BackgroundShellStore::new();
    let tool = BashTool::new(None, None, crate::sandbox::Sandbox::new(false))
        .with_shell_store(Some(store.clone()));

    // Unbounded background run (timeout: None) — Claude-Code model.
    let res = tool
        .call(BashArgs {
            command: "echo bg-hello".to_string(),
            timeout: None,
            background: Some(true),
        })
        .await
        .expect("background bash call");
    assert!(
        res.contains("background shell started"),
        "expected an immediate start message, got: {res}"
    );

    // Parse the id out of "… id: <id>(…".
    let id = res
        .split("id: ")
        .nth(1)
        .and_then(|s| s.split(['(', ' ']).next())
        .expect("id in start message")
        .to_string();

    // Poll bash_output's underlying read until the shell exits, and
    // accumulate streamed output.
    let mut out = String::new();
    let mut exited = false;
    for _ in 0..200 {
        if let Some((chunk, status)) = store.read_new(&id) {
            out.push_str(&chunk);
            if !status.is_running() {
                assert!(
                    matches!(status, ShellStatus::Exited(0)),
                    "status: {status:?}"
                );
                exited = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(exited, "background shell should exit");
    assert!(
        out.contains("bg-hello"),
        "expected streamed output, got: {out}"
    );
    assert_eq!(store.running_count(), 0);
}

/// Test helper: build a single op-based rule (tool-agnostic).
#[cfg(feature = "semantic-bash")]
fn rule(
    op: crate::permission::OpSpec,
    pattern: &str,
    effect: crate::permission::Action,
) -> crate::permission::RuleConfig {
    crate::permission::RuleConfig {
        op,
        pattern: pattern.to_string(),
        effect,
        tool: None,
    }
}

/// F6: a timed-out `sleep 9999` (or any long-running command)
/// must actually be killed when the timeout fires. Before this
/// fix, dropping the tokio future left the bash child running
/// orphaned. The test runs `sleep 5` with a 1-second timeout
/// and asserts: (a) we return the timeout error within ~1.5s,
/// (b) the time to return is much less than the requested
/// sleep duration — proving the process was actually killed
/// rather than us racing to read its output.
#[tokio::test]
async fn run_with_timeout_kills_orphaned_child() {
    let start = std::time::Instant::now();
    let cmd = {
        let mut c = Command::new("bash");
        c.arg("-c").arg("sleep 5");
        c
    };
    let result = run_with_timeout(cmd, 1).await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "expected timeout error, got {:?}", result);
    let msg = format!("{:?}", result);
    assert!(
        msg.contains("timed out"),
        "expected 'timed out' in error: {msg}",
    );
    // The timeout fires at 1s; we allow up to 2s slack for
    // CI variance. The KEY assertion is we return well before
    // the 5s sleep would have completed naturally.
    assert!(
        elapsed < Duration::from_secs(3),
        "took too long to return: {:?}",
        elapsed,
    );
}

/// F6: a command that completes under the timeout returns
/// normally — no false-positive kill.
#[tokio::test]
async fn run_with_timeout_returns_output_on_success() {
    let cmd = {
        let mut c = Command::new("bash");
        c.arg("-c").arg("echo hi");
        c
    };
    let out = run_with_timeout(cmd, 5).await.expect("should succeed");
    assert_eq!(out.merged.trim(), "hi");
}

/// F12: stdout + stderr interleave in true arrival order, not
/// stdout-then-stderr. Use a script that pings stderr between
/// stdout writes; the merged output must keep the order.
#[tokio::test]
async fn run_with_timeout_interleaves_stdout_stderr() {
    let cmd = {
        let mut c = Command::new("bash");
        c.arg("-c")
            // Print to alternating streams with small delays so
            // the kernel actually buffers them in order. Without
            // the delay, both lines might land in the same
            // select! poll and ordering becomes about poll bias.
            .arg(
                "echo OUT-A; \
                     sleep 0.05; \
                     echo ERR-1 >&2; \
                     sleep 0.05; \
                     echo OUT-B; \
                     sleep 0.05; \
                     echo ERR-2 >&2",
            );
        c
    };
    let out = run_with_timeout(cmd, 5).await.expect("should succeed");
    let lines: Vec<&str> = out.merged.lines().collect();
    // Pre-F12 we'd see [OUT-A, OUT-B, ERR-1, ERR-2] because
    // stdout was concatenated before stderr. Post-F12 each line
    // appears in arrival order.
    assert_eq!(
        lines,
        vec!["OUT-A", "ERR-1", "OUT-B", "ERR-2"],
        "stdout/stderr should interleave by arrival",
    );
}

/// F10: a `;` inside double quotes is part of the string, not a
/// segment boundary. Before this, the naive splitter produced
/// two segments, the second being `rm -rf /"`, which could
/// match a bash deny rule for `rm`.
#[test]
fn quote_aware_split_keeps_semi_in_double_quotes() {
    let segments = quote_aware_split(r#"echo "; rm -rf /""#);
    assert_eq!(segments.len(), 1);
    assert!(segments[0].contains("rm -rf /"));
}

/// `&&` inside single quotes is literal too.
#[test]
fn quote_aware_split_keeps_compound_in_single_quotes() {
    let segments = quote_aware_split("echo 'a && b'");
    assert_eq!(segments.len(), 1);
}

/// Escaped `;` is literal — `echo \; ls` is ONE command in bash.
#[test]
fn quote_aware_split_respects_backslash_escape() {
    let segments = quote_aware_split(r"echo \; ls");
    assert_eq!(segments.len(), 1, "got: {:?}", segments);
}

/// Real compounds still split correctly into segments.
#[test]
fn quote_aware_split_splits_unquoted_compounds() {
    let segments = quote_aware_split("cmd1 && cmd2; cmd3 || cmd4");
    assert_eq!(segments.len(), 4);
    assert_eq!(segments[0], "cmd1");
    assert_eq!(segments[1], "cmd2");
    assert_eq!(segments[2], "cmd3");
    assert_eq!(segments[3], "cmd4");
}

/// B3-6: background `&` is a segment separator. Distinct from
/// `&&`, which is handled by the earlier 2-byte branch.
#[test]
fn quote_aware_split_splits_background_ampersand() {
    let segments = quote_aware_split("safe_cmd & rm -rf /tmp/x");
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0], "safe_cmd");
    assert_eq!(segments[1], "rm -rf /tmp/x");
}

#[test]
fn quote_aware_split_keeps_logical_and_separate_from_background() {
    // `&&` still binds as a 2-byte compound — must NOT be split
    // as two `&` separators.
    let segments = quote_aware_split("a && b & c");
    assert_eq!(segments, vec!["a", "b", "c"]);
}

/// Regression: bare `|` pipes must split into segments. Before
/// this, a command like `safe_cmd | rm -rf /` was treated as
/// one unit and only `safe_cmd`'s permission rule applied.
#[test]
fn quote_aware_split_splits_on_bare_pipe() {
    let segments = quote_aware_split("safe_cmd | rm -rf /tmp/x");
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].trim(), "safe_cmd");
    assert_eq!(segments[1].trim(), "rm -rf /tmp/x");
}

/// `||` must NOT also match the single-`|` arm (already covered
/// by the existing `||` test, but pin the interaction here too).
#[test]
fn quote_aware_split_or_and_pipe_distinct() {
    let segments = quote_aware_split("a || b | c");
    assert_eq!(segments.len(), 3, "got {segments:?}");
    assert_eq!(segments[0].trim(), "a");
    assert_eq!(segments[1].trim(), "b");
    assert_eq!(segments[2].trim(), "c");
}

/// Empty / whitespace-only segments dropped.
#[test]
fn quote_aware_split_drops_empty_segments() {
    let segments = quote_aware_split(";; cmd ;");
    assert_eq!(segments, vec!["cmd"]);
}

/// Mixed: quoted compound + unquoted compound.
#[test]
fn quote_aware_split_mixed_quoted_and_unquoted() {
    let segments = quote_aware_split(r#"echo "a; b" ; ls"#);
    assert_eq!(segments.len(), 2);
    assert!(segments[0].contains("a; b"));
    assert_eq!(segments[1], "ls");
}

// M3 (dirge-6ab) — segment-level bash gating regression tests.
// These pin the "every command in a compound gets checked
// separately" invariant the user asked about
// ("agent runs `git diff && rm -rf /`, what happens?").

/// `git diff && rm -rf /` must be denied — the second segment
/// hits the default `rm -rf /**` deny rule even though the
/// first segment is allowlisted. Pre-this-test, the path was
/// covered by the parser test in `semantic::adapters::bash`,
/// but nothing end-to-end pinned that `check_bash_segments`
/// actually walks the segments through the perm checker.
#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn compound_command_denies_dangerous_segment() {
    use crate::permission::{PermissionConfig, SecurityMode, checker::PermissionChecker};

    let config = PermissionConfig::default();
    let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
    let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

    let result = check_bash_segments(&Some(perm), &None, "git diff && rm -rf /").await;
    assert!(
        result.is_err(),
        "compound: rm segment must hit deny rule even after safe git segment; got {result:?}",
    );
    let msg = format!("{:?}", result);
    assert!(
        msg.contains("denied") || msg.contains("Denied"),
        "expected 'denied' in error: {msg}",
    );
}

/// Output redirect targets route through the `write` tool rules
/// (M3 fix to the C4 audit). Pre-fix: `tool="bash"` lookup with a
/// path string, no matching command pattern, fell through to
/// default Allow — `echo hi > /etc/passwd` ran without prompting.
/// Post-fix: routes through write rules.
#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn redirect_target_routes_through_write_rules() {
    use crate::permission::{
        Action, OpSpec, PermissionConfig, SecurityMode, checker::PermissionChecker,
    };

    // Configure edit to deny everywhere; without an explicit
    // rule the M2/M4-pre default is still Allow, so we set an
    // explicit deny to make the test robust against the
    // default-flip.
    let config = PermissionConfig {
        rules: vec![rule(OpSpec::Edit, "/etc/**", Action::Deny)],
        ..Default::default()
    };
    let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
    let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

    let result = check_bash_segments(&Some(perm), &None, "echo hi > /etc/passwd").await;
    assert!(
        result.is_err(),
        "redirect to /etc/passwd should be denied by write rules; got {result:?}",
    );
}

/// Sibling check: a redirect target inside the working directory
/// (non-external) passes the write-rules check. Without this, a
/// regression that over-broadly denied all redirects could pass
/// the negative case above and ship.
///
/// Uses an in-cwd path because the catch-all at
/// `permission/checker.rs:434` upgrades unmatched-Allow to Ask
/// for EXTERNAL paths — so `/tmp/x` (external to the test's cwd
/// of the dirge repo) would test the external-path catch-all,
/// not the write-rules-allow path we want to exercise here.
/// M3 is intentionally tightening external bash-redirects to
/// prompt; this test pins the in-cwd happy path.
// F1 (dirge-dvy) — bash arg-side path checks. Pin that
// file-mutating commands route their positional path args
// through the write rules, independent of the bash command-
// pattern check.

/// `rm /etc/passwd` is denied via write rules even when the
/// user's bash config is otherwise permissive. Pre-F1: the
/// path-side check never ran for arguments (only redirect
/// targets), so a `bash: { "rm *": "allow" }` rule silently
/// allowed `rm /etc/passwd`. Post-F1: the path arg routes
/// through `enforce(write, /etc/passwd)` and the user's
/// write deny rule fires.
#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn rm_arg_path_routes_through_write_rules() {
    use crate::permission::{
        Action, OpSpec, PermissionConfig, SecurityMode, checker::PermissionChecker,
    };

    // Permissive execute: allow `rm *`. Restrictive edit: deny
    // `/etc/**`. Without F1, the execute allow would let
    // `rm /etc/passwd` through.
    let config = PermissionConfig {
        rules: vec![
            rule(OpSpec::Execute, "rm *", Action::Allow),
            rule(OpSpec::Edit, "/etc/**", Action::Deny),
        ],
        ..Default::default()
    };
    let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
    let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

    let result = check_bash_segments(&Some(perm), &None, "rm /etc/passwd").await;
    assert!(
        result.is_err(),
        "rm /etc/passwd must hit write deny rule even when bash rule allows; got {result:?}",
    );
}

/// chmod's FIRST arg (the mode spec like `777` or `u+x`) is
/// NOT treated as a path. Only subsequent positional args go
/// through the write check.
#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn chmod_skips_mode_spec_routes_paths() {
    use crate::permission::{
        Action, OpSpec, PermissionConfig, SecurityMode, checker::PermissionChecker,
    };

    let config = PermissionConfig {
        rules: vec![
            rule(OpSpec::Execute, "chmod *", Action::Allow),
            rule(OpSpec::Edit, "/etc/**", Action::Deny),
        ],
        ..Default::default()
    };
    let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
    let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

    // `777` is the mode spec; it must NOT be treated as a
    // path arg (would resolve to /cwd/777, false-positive).
    // `/etc/passwd` IS a path → should hit write deny.
    let result = check_bash_segments(&Some(perm), &None, "chmod 777 /etc/passwd").await;
    assert!(
        result.is_err(),
        "chmod 777 /etc/passwd: mode skipped, path arg gated; got {result:?}",
    );
}

/// Flags (`-r`, `--recursive`) are correctly skipped when
/// extracting path args.
#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn flags_skipped_when_extracting_paths() {
    use crate::permission::{
        Action, OpSpec, PermissionConfig, SecurityMode, checker::PermissionChecker,
    };

    let config = PermissionConfig {
        rules: vec![
            rule(OpSpec::Execute, "rm *", Action::Allow),
            rule(OpSpec::Edit, "/etc/**", Action::Deny),
        ],
        ..Default::default()
    };
    let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
    let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

    // `-rf` is a flag; `/etc/passwd` is the path. Flag is
    // skipped, path hits deny.
    let result = check_bash_segments(&Some(perm), &None, "rm -rf /etc/passwd").await;
    assert!(
        result.is_err(),
        "rm -rf /etc/passwd: flag skipped, path arg gated; got {result:?}",
    );
}

#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn redirect_target_allowed_when_write_permits() {
    use crate::permission::{
        Action, OpSpec, PermissionConfig, SecurityMode, checker::PermissionChecker,
    };

    // F2 (dirge-jlj) dissolved: write/edit/apply_patch all map to
    // Operation::Edit, so a single Edit allow rule governs the
    // redirect-target write.
    let config = PermissionConfig {
        rules: vec![rule(OpSpec::Edit, "**", Action::Allow)],
        ..Default::default()
    };
    let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
    let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

    let result = check_bash_segments(&Some(perm), &None, "echo hi > target/test-out.txt").await;
    assert!(
        result.is_ok(),
        "redirect to an explicitly-allowed target should pass; got {result:?}",
    );
}

// dirge-mzs4: /dev/null redirect whitelist. Commands whose only
// filesystem-touching effect is a `/dev/null` redirect are
// auto-allowed — writing to /dev/null discards data with no
// observable side effect, so prompting on that pattern is pure
// noise. Deny rules and the doom-loop detector still fire; the
// only behavioural change is `Ask → Allow` for the bash segment
// check.

/// The `/dev/null` redirect TARGET is auto-allowed (a harmless
/// bit-bucket), so it never adds a prompt of its own. Phase 3
/// behavior change: the COMMAND still needs its own Execute
/// permission — an unfamiliar command redirected to /dev/null
/// still prompts (more correct than the old blanket command
/// soft-allow). So an ALLOWED command (`git status -s`) redirected
/// to /dev/null passes without prompting; the /dev/null target
/// contributes no extra gate.
#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn bash_dev_null_target_adds_no_prompt() {
    use crate::permission::{PermissionConfig, SecurityMode, checker::PermissionChecker};

    // `git status` is a default-allowed bash command; redirecting
    // it to /dev/null must not introduce a prompt.
    let allowed_cases = [
        "git status -s > /dev/null",
        "git status -s 2> /dev/null",
        "git status -s &> /dev/null",
        "git status -s > /dev/null 2>&1",
    ];
    for cmd in &allowed_cases {
        let checker =
            PermissionChecker::new(&PermissionConfig::default(), SecurityMode::Standard, None);
        let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));
        let result = check_bash_segments(&Some(perm), &None, cmd).await;
        assert!(
            result.is_ok(),
            "{cmd:?}: allowed command + /dev/null target must not prompt; got {result:?}",
        );
    }

    // An UNFAMILIAR command redirected to /dev/null still needs
    // command permission → prompts (Err in non-interactive test).
    let checker =
        PermissionChecker::new(&PermissionConfig::default(), SecurityMode::Standard, None);
    let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));
    let result = check_bash_segments(&Some(perm), &None, "unfamiliar_cmd > /dev/null").await;
    assert!(
        result.is_err(),
        "unfamiliar command still needs Execute permission even redirecting to /dev/null; got {result:?}",
    );
}

/// Compound redirects (one to /dev/null, one to a real file) must
/// NOT slip through the whitelist — the real-file destination
/// still routes through the write rules, and the bash segment
/// check still applies. Pre-fix, naively whitelisting any
/// /dev/null mention would let `cmd > file.txt > /dev/null`
/// silently write to file.txt.
#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn bash_redirect_to_file_and_dev_null_still_prompts() {
    use crate::permission::{PermissionConfig, SecurityMode, checker::PermissionChecker};

    let config = PermissionConfig::default();
    let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
    let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

    // No ask_tx is wired, so any `Ask` outcome surfaces as an
    // error from `enforce`. If the whitelist mistakenly applied,
    // this would succeed silently.
    let result = check_bash_segments(
        &Some(perm),
        &None,
        "unfamiliar_cmd > /tmp/dirge-mzs4-real.log 2> /dev/null",
    )
    .await;
    assert!(
        result.is_err(),
        "compound redirect (real file + /dev/null) must NOT auto-allow; got {result:?}",
    );
}

/// Baseline: a command with NO /dev/null redirect and no default
/// allow rule must still prompt. Pins that the whitelist does
/// not bleed into the unredirected case.
#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn bash_other_destination_still_prompts() {
    use crate::permission::{PermissionConfig, SecurityMode, checker::PermissionChecker};

    let config = PermissionConfig::default();
    let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
    let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

    // `unfamiliar_cmd` doesn't match any default bash allow
    // rule. No ask_tx is wired so the `Ask` outcome surfaces
    // as an error. The whitelist is dormant — falls through to
    // the standard enforce path.
    let result =
        check_bash_segments(&Some(perm), &None, "unfamiliar_cmd > /tmp/elsewhere.log").await;
    assert!(
        result.is_err(),
        "non-/dev/null redirect must still prompt; got {result:?}",
    );
}

/// Deny rules still fire even for /dev/null-redirected commands.
/// `rm -rf / > /dev/null` must be denied by the default
/// `rm -rf /**` rule — the dev/null whitelist must NOT bypass
/// the deny gate.
#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn bash_dev_null_does_not_bypass_deny_rules() {
    use crate::permission::{PermissionConfig, SecurityMode, checker::PermissionChecker};

    let config = PermissionConfig::default();
    let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
    let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

    let result = check_bash_segments(&Some(perm), &None, "rm -rf / > /dev/null").await;
    assert!(
        result.is_err(),
        "dev/null redirect must not bypass `rm -rf /**` deny; got {result:?}",
    );
}

/// In a compound (`&&`-separated) statement, the dev/null
/// soft-allow applies ONLY to the segment with the /dev/null
/// redirect — other segments still go through the normal
/// gate. `unfamiliar_cmd > /dev/null && other_unfamiliar_cmd`
/// auto-allows the first but prompts on the second.
#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn bash_dev_null_per_segment_scope() {
    use crate::permission::{PermissionConfig, SecurityMode, checker::PermissionChecker};

    let config = PermissionConfig::default();
    let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
    let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

    let result = check_bash_segments(
        &Some(perm),
        &None,
        "unfamiliar_cmd > /dev/null && other_unfamiliar_cmd",
    )
    .await;
    assert!(
        result.is_err(),
        "second segment without /dev/null redirect must still prompt; got {result:?}",
    );
}

// dirge-sb2n — bash file-mutation propagation. Files created /
// deleted / renamed via bash must surface in the MODIFIED panel the
// same way write/edit/apply_patch do.

/// Heredoc create (`cat > voxel.html <<'EOF' … EOF`) — the exact
/// shape that prompted this fix — yields the redirect target so it
/// can be marked modified.
#[cfg(feature = "semantic-bash")]
#[test]
fn bash_mutation_targets_heredoc_create() {
    let cmd = "cat > voxel.html <<'EOF'\n<html></html>\nEOF";
    let t = bash_mutation_targets(cmd);
    assert!(t.iter().any(|p| p == "voxel.html"), "got {t:?}");
}

/// Plain output redirect creates a file → tracked.
#[cfg(feature = "semantic-bash")]
#[test]
fn bash_mutation_targets_redirect_create() {
    let t = bash_mutation_targets("echo hi > notes.txt");
    assert!(t.iter().any(|p| p == "notes.txt"), "got {t:?}");
}

/// `rm` delete → the deleted path is tracked.
#[cfg(feature = "semantic-bash")]
#[test]
fn bash_mutation_targets_rm_delete() {
    let t = bash_mutation_targets("rm -rf build/old.o");
    assert!(t.iter().any(|p| p == "build/old.o"), "got {t:?}");
}

/// `mv` rename → both source and destination are tracked.
#[cfg(feature = "semantic-bash")]
#[test]
fn bash_mutation_targets_mv_rename() {
    let t = bash_mutation_targets("mv a.txt b.txt");
    assert!(t.iter().any(|p| p == "a.txt"), "src missing, got {t:?}");
    assert!(t.iter().any(|p| p == "b.txt"), "dst missing, got {t:?}");
}

/// End-to-end: a `BashTool::call` that creates a file via redirect
/// records the (canonicalized) path in the shared modified tracker,
/// so it appears in the MODIFIED panel. Uses a unique absolute path
/// and asserts membership only, so it's robust to other tests
/// sharing the global `MODIFIED_FILES` set.
#[cfg(feature = "semantic-bash")]
#[tokio::test]
async fn bash_create_propagates_to_modified_tracker() {
    use crate::agent::tools::BashArgs;
    let dir = std::env::temp_dir().join("dirge-sb2n-bash-create");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("created-by-bash.txt");
    let _ = std::fs::remove_file(&file);

    let tool = BashTool::new(None, None, crate::sandbox::Sandbox::new(false));
    tool.call(BashArgs {
        command: format!("echo hi > {}", file.display()),
        timeout: None,
        background: None,
    })
    .await
    .expect("bash create");

    let canonical = std::fs::canonicalize(&file).expect("file should exist");
    let recent = crate::agent::tools::modified::recent(256);
    assert!(
        recent.contains(&canonical),
        "bash-created file should be tracked; looking for {canonical:?} in {recent:?}",
    );
    let _ = std::fs::remove_file(&file);
}

// ============================================================
// dirge-9zbd — deterministic bash permission-gating corpus.
//
// These pin the END-TO-END gating for the kinds of commands models
// actually emit: compound `&&`/`|`/`;`/`||`, `cd` into another
// project, and multi-line `-e`/`-c` scripts. No LLM involved — pure
// deterministic rule evaluation. The headline invariant: picking
// "allow always" (the pattern the UI suggests) MUST make that exact
// command stop prompting. That invariant was silently broken for
// every multi-line command (the regex wasn't DOTALL) and for
// compounds whose benign prefix wasn't auto-allowed.
// ============================================================
#[cfg(feature = "semantic-bash")]
mod gating_corpus {
    use super::*;
    use crate::permission::{PermissionConfig, SecurityMode, checker::PermissionChecker};
    use std::sync::{Arc, Mutex};

    /// Fresh Standard-mode checker with a FIXED synthetic working dir
    /// so external-path classification is deterministic wherever the
    /// suite runs. (None of the corpus commands touch real files.)
    fn checker() -> Arc<Mutex<PermissionChecker>> {
        let config = PermissionConfig::default();
        let c = PermissionChecker::new(
            &config,
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/work/proj")),
        );
        Arc::new(Mutex::new(c))
    }

    /// Default gating, no grant. `Ok` = auto-allowed, `Err` = the
    /// command would prompt (Ask) or is denied — there's no `ask_tx`
    /// so an Ask surfaces as `Err`.
    async fn gated(cmd: &str) -> bool {
        check_bash_segments(&Some(checker()), &None, cmd)
            .await
            .is_ok()
    }

    /// The full "allow always" round-trip: suggest the pattern the UI
    /// would (`suggest_pattern`), store it as the session would
    /// (`add_session_allowlist`), then re-check the SAME command.
    /// Returns whether the command is now allowed.
    async fn grant_then_recheck(cmd: &str) -> bool {
        let perm = checker();
        let pat = crate::ui::permission_ui::suggest_pattern("bash", cmd);
        perm.lock()
            .unwrap_or_else(|e| e.into_inner())
            .add_session_allowlist("bash".to_string(), &pat);
        check_bash_segments(&Some(perm), &None, cmd).await.is_ok()
    }

    /// The exact screenshot command: `cd <external> && npx tsx -e
    /// "<multi-line script>"`. `npx` runs arbitrary remote code, so it
    /// is NOT default-allowed — it must prompt ONCE. The bug was that
    /// "allow always" (`npx *`) then never matched because the regex
    /// wasn't DOTALL; with the fix the grant sticks on the multi-line
    /// command. (`cd` to the external project is auto-allowed.)
    #[tokio::test]
    async fn reported_multiline_npx_compound_prompts_then_grant_sticks() {
        let cmd = "cd /Users/yogthos/src/rignet && npx tsx -e \"\
                import { readFileSync } from 'fs';\n\
                import { runRiggingTest } from './src/index.ts';\n\
                runRiggingTest();\"";
        assert!(
            !gated(cmd).await,
            "npx runs arbitrary code — it must prompt the first time"
        );
        assert!(
            grant_then_recheck(cmd).await,
            "ALLOW-ALWAYS MUST STICK on the multi-line compound (the reported bug)"
        );
    }

    /// Arbitrary-code interpreters prompt once, then the "allow always"
    /// grant must stick — including for multi-line `-e`/`-c` scripts,
    /// the exact class the newline bug broke.
    #[tokio::test]
    async fn multiline_interpreter_scripts_prompt_then_grant_sticks() {
        for cmd in [
            "npx tsx -e \"console.log(1)\"",
            "npx tsx -e \"const a = 1;\nconsole.log(a)\"",
            "node -e \"const x = 1;\nconsole.log(x)\"",
            "python3 -c \"import sys\nprint(sys.argv)\"",
            "python -c \"x = 1\nprint(x)\"",
        ] {
            assert!(
                !gated(cmd).await,
                "interpreter must prompt (not default-allowed): {cmd:?}"
            );
            assert!(
                grant_then_recheck(cmd).await,
                "allow-always must stick on multi-line interpreter cmd: {cmd:?}"
            );
        }
    }

    /// Compounds whose every segment is default-allowed auto-allow —
    /// across `&&`, `|`, `;`, `||`.
    #[tokio::test]
    async fn all_default_compounds_auto_allowed() {
        for cmd in [
            "git add . && git commit -m \"msg\"",
            "cargo fmt && cargo test",
            "cd subdir && npm run build",
            "ls -la | grep foo",
            "cat a.txt; echo done",
            "cargo build || echo failed",
            "export RUST_LOG=debug && cargo test",
            "pushd app && npm run build && popd",
        ] {
            assert!(
                gated(cmd).await,
                "all-default compound must auto-allow: {cmd:?}"
            );
        }
    }

    /// THE INVARIANT: a non-default command (including multi-line and
    /// compound-with-benign-prefix) must FIRST prompt, then stop
    /// prompting once "allow always" stores the suggested pattern.
    #[tokio::test]
    async fn allow_always_sticks_for_custom_commands() {
        for cmd in [
            "mycli run --fast",
            // Multi-line — the DOTALL case end-to-end.
            "mycli gen -e \"line1\nline2\nline3\"",
            // Compound: benign (auto-allowed) prefix + custom multi-line.
            "cd /some/external/project && mycli build -e \"a\nb\"",
            "export TOKEN=x && mycli deploy",
        ] {
            assert!(
                !gated(cmd).await,
                "expected an initial prompt (not in defaults): {cmd:?}"
            );
            assert!(
                grant_then_recheck(cmd).await,
                "ALLOW-ALWAYS MUST STICK — command still prompts after grant: {cmd:?}"
            );
        }
    }

    /// `source`/`.` run arbitrary script code: NOT auto-allowed, and
    /// the suggestion targets them (not a later segment), so granting
    /// makes the whole `source x && <default-allowed-cmd>` pass. Paired
    /// with a project-scoped `cargo test` (auto-allowed) so the only
    /// gate is `source` — granting `source *` must clear it.
    #[tokio::test]
    async fn source_is_gated_but_grant_sticks() {
        let cmd = "source ./env.sh && cargo test";
        assert!(!gated(cmd).await, "source must prompt by default");
        assert!(
            grant_then_recheck(cmd).await,
            "granting the suggested `source *` must make the command pass"
        );
    }

    /// Security: denies and dangerous segments are NOT unlocked by an
    /// "allow always" on a sibling segment.
    #[tokio::test]
    async fn dangerous_segments_stay_gated_even_after_grant() {
        for cmd in [
            "rm -rf /",
            "npx foo && rm -rf /",
            "cargo build && sudo rm -rf /var",
        ] {
            assert!(!gated(cmd).await, "must not auto-allow: {cmd:?}");
            assert!(
                !grant_then_recheck(cmd).await,
                "allow-always must NOT unlock a denied/dangerous segment: {cmd:?}"
            );
        }
    }

    /// Operators inside quotes are literal — the dangerous text must
    /// stay part of one safe command, not split into its own claim.
    #[tokio::test]
    async fn quoted_operators_do_not_split_into_claims() {
        // The `&&` and `rm -rf /` are inside the echo string.
        assert!(
            gated("echo \"a && rm -rf /\"").await,
            "quoted operator is literal — echo is allowed as one segment"
        );
    }

    /// dirge-7l5i: a `cd` to an EXTERNAL dir followed by a RELATIVE
    /// redirect target must be classified out-of-project and prompt —
    /// not silently auto-allowed by resolving the target against the
    /// static project root. (`echo` is allowed, so the ONLY gate here
    /// is the redirect target's classification.)
    #[tokio::test]
    async fn cd_outside_project_gates_relative_redirect() {
        assert!(
            !gated("cd /etc && echo pwned > passwd").await,
            "cd /etc + relative `> passwd` writes /etc/passwd — must prompt"
        );
        // In-project cd + relative write stays auto-allowed.
        assert!(
            gated("cd subdir && echo ok > out.txt").await,
            "in-project cd + relative write is in-tree, stays allowed"
        );
        // No cd: a plain relative in-project write is allowed as before.
        assert!(
            gated("echo ok > local.txt").await,
            "plain in-project relative write stays allowed"
        );
        // Absolute external redirect was already gated; still is.
        assert!(
            !gated("echo pwned > /etc/passwd").await,
            "absolute external redirect must prompt"
        );
    }

    // --- dirge-0g6i: LLM auto-approval at the enforce chokepoint. The
    // evaluator lives on the checker (no global), so each test wires
    // its own stub and stays isolated.

    use crate::permission::approval::{ApprovalDecision, ApprovalFn, ApprovalRequest};
    use std::future::Future;
    use std::pin::Pin;

    fn checker_with_approval(stub: ApprovalFn) -> Arc<Mutex<PermissionChecker>> {
        let config = PermissionConfig::default();
        let mut c = PermissionChecker::new(
            &config,
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/work/proj")),
        );
        c.set_approval_fn(stub);
        Arc::new(Mutex::new(c))
    }

    fn approve_always() -> ApprovalFn {
        std::sync::Arc::new(|_req: ApprovalRequest| {
            Box::pin(async { Ok(ApprovalDecision::Allow) })
                as Pin<Box<dyn Future<Output = anyhow::Result<ApprovalDecision>> + Send>>
        })
    }

    /// Evaluator ALLOW auto-approves an otherwise-prompting command
    /// (no `ask_tx` needed).
    #[tokio::test]
    async fn approval_provider_allows_a_prompting_command() {
        let perm = checker_with_approval(approve_always());
        // `npx foo` is not default-allowed → would Ask; evaluator allows.
        assert!(
            check_bash_segments(&Some(perm), &None, "npx foo")
                .await
                .is_ok(),
            "evaluator ALLOW must auto-approve"
        );
    }

    /// Evaluator DENY rejects with the reason, never falling through to
    /// a human prompt.
    #[tokio::test]
    async fn approval_provider_denies_with_reason() {
        let stub: ApprovalFn = std::sync::Arc::new(|_req: ApprovalRequest| {
            Box::pin(async { Ok(ApprovalDecision::Deny("writes outside project".into())) })
                as Pin<Box<dyn Future<Output = anyhow::Result<ApprovalDecision>> + Send>>
        });
        let perm = checker_with_approval(stub);
        let res = check_bash_segments(&Some(perm), &None, "npx foo").await;
        assert!(res.is_err(), "evaluator DENY must reject");
        assert!(
            format!("{res:?}").contains("writes outside project"),
            "rejection must carry the evaluator's reason: {res:?}"
        );
    }

    /// A hard deny is final — auto-approval only intercepts Ask, so an
    /// allow-everything evaluator cannot unlock `rm -rf /`.
    #[tokio::test]
    async fn approval_provider_cannot_override_a_hard_deny() {
        let perm = checker_with_approval(approve_always());
        assert!(
            check_bash_segments(&Some(perm), &None, "rm -rf /")
                .await
                .is_err(),
            "a hard deny must not be reachable by the approval evaluator"
        );
    }

    /// The evaluator receives the full command + a per-claim resource
    /// summary so it can judge compounds precisely.
    #[tokio::test]
    async fn approval_provider_receives_command_and_resources() {
        let seen: Arc<Mutex<Option<(String, usize)>>> = Arc::new(Mutex::new(None));
        let seen2 = seen.clone();
        let stub: ApprovalFn = std::sync::Arc::new(move |req: ApprovalRequest| {
            *seen2.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((req.command.clone(), req.resources.len()));
            Box::pin(async { Ok(ApprovalDecision::Allow) })
                as Pin<Box<dyn Future<Output = anyhow::Result<ApprovalDecision>> + Send>>
        });
        let perm = checker_with_approval(stub);
        // Two prompting segments → aggregate Ask → evaluator sees both.
        let _ = check_bash_segments(&Some(perm), &None, "npx foo && mycli bar").await;
        let (cmd, n) = seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("evaluator should have been called");
        assert_eq!(cmd, "npx foo && mycli bar");
        assert!(
            n >= 2,
            "both command segments should be summarized; got {n}"
        );
    }
}

// ── dirge-9bqy: coarse redirect/mutation gating (no-semantic-bash) ──

#[cfg(not(feature = "semantic-bash"))]
#[test]
fn coarse_redirect_targets_extracts_external_write() {
    // Absolute out-of-tree redirect target is captured.
    assert_eq!(
        coarse_redirect_targets("echo x > /etc/passwd"),
        vec!["/etc/passwd".to_string()]
    );
    // Append + clobber operators.
    assert_eq!(
        coarse_redirect_targets("cmd >> /var/log/x"),
        vec!["/var/log/x".to_string()]
    );
    assert_eq!(
        coarse_redirect_targets("cmd >| out.txt"),
        vec!["out.txt".to_string()]
    );
    // fd-prefixed redirect (`2>`).
    assert_eq!(
        coarse_redirect_targets("cmd 2> err.log"),
        vec!["err.log".to_string()]
    );
    // A literal `>` inside quotes is NOT a redirect (no false positive).
    assert!(coarse_redirect_targets("echo \">notaredirect\"").is_empty());
    // fd duplication `1>&2` captures no file target.
    assert!(coarse_redirect_targets("cmd 1>&2").is_empty());
}

#[cfg(not(feature = "semantic-bash"))]
#[test]
fn coarse_mutation_paths_extracts_targets() {
    assert_eq!(
        coarse_mutation_paths("rm -rf /tmp/x"),
        vec!["/tmp/x".to_string()]
    );
    assert_eq!(
        coarse_mutation_paths("cp a b"),
        vec!["a".to_string(), "b".to_string()]
    );
    // `dd` only contributes its `of=` operand.
    assert_eq!(
        coarse_mutation_paths("dd if=/dev/zero of=/etc/wipe bs=1"),
        vec!["/etc/wipe".to_string()]
    );
    // A `/bin/`-prefixed mutator is still recognized by basename.
    assert_eq!(
        coarse_mutation_paths("/bin/rm /etc/hosts"),
        vec!["/etc/hosts".to_string()]
    );
    // Non-mutators contribute nothing.
    assert!(coarse_mutation_paths("echo hello").is_empty());
}

/// End-to-end on the no-semantic build: a redirect to an out-of-tree
/// path produces an Edit claim against an EXTERNAL resource, so the
/// external-dir gate fires instead of the write riding through ungated.
#[cfg(not(feature = "semantic-bash"))]
#[tokio::test]
async fn coarse_external_redirect_is_gated() {
    use crate::permission::engine::classify_path;
    // The coarse target resolves to the absolute out-of-tree path …
    let targets = coarse_redirect_targets("echo pwned > /etc/passwd");
    assert_eq!(targets, vec!["/etc/passwd".to_string()]);
    // … and classify_path marks it outside any plausible project root.
    let r = classify_path("/etc/passwd", "/home/user/project");
    match r {
        crate::permission::engine::types::Resource::Path { in_cwd, .. } => {
            assert!(!in_cwd, "/etc/passwd must classify as outside the cwd");
        }
        other => panic!("expected a Path resource, got {other:?}"),
    }
}
