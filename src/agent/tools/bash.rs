use rig::completion::ToolDefinition;
use rig::tool::Tool;
use tokio::process::Command;
use tokio::time::Duration;

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, BashArgs, PermCheck, ToolError, check_perm};

use crate::sandbox::Sandbox;
#[cfg(feature = "semantic-bash")]
use crate::semantic::adapters::bash;

/// Captured output with stdout + stderr lines preserved in arrival
/// order. Replaces tokio's `Output` (which collects each stream as
/// a separate blob, losing time ordering between them — F12).
#[derive(Debug)]
pub(crate) struct InterleavedOutput {
    /// Lines in the order they arrived from EITHER pipe.
    pub merged: String,
    pub exit_code: i32,
}

/// Spawn `cmd` into its own process group and wait for it,
/// capped at `secs`. On timeout, send SIGKILL to the process
/// group so the whole subprocess tree dies — not just bash. On
/// Windows we fall back to tokio's `kill_on_drop` which signals
/// the direct child only (Windows job objects would be cleaner
/// but require extra deps). F6 + F12 fix.
async fn run_with_timeout(cmd: Command, secs: u64) -> Result<InterleavedOutput, ToolError> {
    use std::process::Stdio;
    let mut cmd = cmd;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `kill_on_drop(true)` ensures the immediate child gets a
    // signal when the tokio future is dropped — necessary for
    // ANY platform's timeout to actually clean up the bash process.
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    {
        // process_group(0) makes the spawned child the leader of a
        // new process group with pgid = pid. Then `killpg(-pid)`
        // reaches every descendant. (tokio's `Command` exposes this
        // natively without needing the std `CommandExt` trait.)
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Msg(format!("failed to spawn: {}", e)))?;
    let pid = child.id();

    // F12: drain stdout + stderr concurrently into a single buffer
    // so the order of lines reflects actual arrival time. The prior
    // implementation (`wait_with_output`) buffered each stream
    // separately and concatenated stdout + stderr at the end, which
    // mis-ordered every command that wrote to both interleaved
    // (e.g. `make`, `npm install`, `cargo build`).
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let drain = async move {
        use tokio::io::AsyncBufReadExt;
        let mut merged = String::new();
        let mut so = stdout.map(tokio::io::BufReader::new);
        let mut se = stderr.map(tokio::io::BufReader::new);
        loop {
            // Decide presence BEFORE constructing futures — the
            // `if` guards on select! borrow `so` and `se`, which
            // would conflict with the futures' mutable borrows.
            let has_so = so.is_some();
            let has_se = se.is_some();
            if !has_so && !has_se {
                break;
            }
            let mut so_buf = String::new();
            let mut se_buf = String::new();
            // Build futures lazily; each is "noop" if its reader
            // is None. We funnel both into `Result<usize>` so the
            // select! arms have matching types.
            let so_fut = async {
                match so.as_mut() {
                    Some(r) => r.read_line(&mut so_buf).await.map(Some),
                    None => Ok::<_, std::io::Error>(None),
                }
            };
            let se_fut = async {
                match se.as_mut() {
                    Some(r) => r.read_line(&mut se_buf).await.map(Some),
                    None => Ok::<_, std::io::Error>(None),
                }
            };
            tokio::select! {
                biased;
                r = so_fut, if has_so => match r {
                    Ok(Some(0)) | Ok(None) | Err(_) => { so = None; }
                    Ok(Some(_)) => merged.push_str(&so_buf),
                },
                r = se_fut, if has_se => match r {
                    Ok(Some(0)) | Ok(None) | Err(_) => { se = None; }
                    Ok(Some(_)) => merged.push_str(&se_buf),
                },
            }
        }
        merged
    };

    let wait = async {
        let merged = drain.await;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((merged, status))
    };

    match tokio::time::timeout(Duration::from_secs(secs), wait).await {
        Ok(Ok((merged, status))) => Ok(InterleavedOutput {
            merged,
            exit_code: status.code().unwrap_or(-1),
        }),
        Ok(Err(e)) => Err(ToolError::Msg(format!("wait failed: {}", e))),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                // SAFETY: killpg with negative pid sends to the
                // process group. SIGKILL is the same on every
                // POSIX platform; libc::pid_t is i32 on every
                // platform dirge supports.
                unsafe {
                    let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            let _ = pid;
            Err(ToolError::Msg(format!("Command timed out after {}s", secs)))
        }
    }
}

pub struct BashTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub sandbox: Sandbox,
    cache: Option<ToolCache>,
}

impl BashTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>, sandbox: Sandbox) -> Self {
        BashTool {
            permission,
            ask_tx,
            sandbox,
            cache: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        sandbox: Sandbox,
        cache: ToolCache,
    ) -> Self {
        BashTool {
            permission,
            ask_tx,
            sandbox,
            cache: Some(cache),
        }
    }
}

impl Tool for BashTool {
    const NAME: &'static str = "bash";

    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description: "Execute a bash command in the current working directory. Returns stdout and stderr.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Bash command to execute" },
                    "timeout": { "type": "integer", "description": "Timeout in seconds (optional)" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: BashArgs) -> Result<String, ToolError> {
        check_bash_segments(&self.permission, &self.ask_tx, &args.command).await?;

        // F6: spawn into its own process group so a timeout can
        // SIGKILL the entire subprocess tree, not just the
        // immediate `bash` child. Before this, `pi` would spawn
        // `npm install`, the 120s timeout fired, the future was
        // dropped (taking the tokio `Child` with it), but bash's
        // children — and theirs — kept running orphaned under PID 1.
        // pi (`bash.ts:76-81`) does this via `detached: true` +
        // `killProcessTree(pid)`.
        let secs = args.timeout.unwrap_or(120);
        if secs == 0 {
            return Err(ToolError::Msg("timeout must be > 0".to_string()));
        }
        let output = run_with_timeout(self.sandbox.wrap_command(&args.command), secs).await?;

        // F12: `merged` already contains stdout + stderr in arrival
        // order. Previously we concatenated stdout then stderr,
        // mis-ordering interleaved output.
        let mut result = output.merged;
        if output.exit_code != 0 {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&format!("Exit code: {}", output.exit_code));
        }
        // Bash may have mutated the filesystem; conservatively invalidate the
        // per-turn read/grep/list cache.
        if let Some(ref cache) = self.cache {
            cache.clear();
        }
        Ok(result)
    }
}

async fn check_bash_segments(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    command: &str,
) -> Result<(), ToolError> {
    #[cfg(feature = "semantic-bash")]
    {
        let (segments, complex) = bash::parse_bash_segments_full(command)
            .unwrap_or_else(|_| (vec![command.to_string()], false));

        if complex {
            return check_perm(permission, ask_tx, "bash", command).await;
        }

        for segment in &segments {
            check_perm(permission, ask_tx, "bash", segment).await?;
        }
        Ok(())
    }
    #[cfg(not(feature = "semantic-bash"))]
    {
        // Best-effort coarse split when tree-sitter isn't compiled in.
        // Without it, a command like `safe_cmd && rm -rf /` would be
        // checked as a single string against the bash rules and might
        // squeak through if `safe_cmd && rm` doesn't match any deny.
        // Split on the unambiguous compound separators (`&&`, `;`,
        // `||`) so each segment is checked individually.
        //
        // F10: the splitter now respects shell quoting. The naive
        // `command.split(";")` split inside quoted strings, so
        // `echo "; rm -rf /"` produced segments `echo "` and
        // `rm -rf /"` — the second matched the bash rule for `rm`
        // and could trigger a deny that the user thought was safe.
        // The fixed splitter walks character-by-character and only
        // emits a boundary when not inside `'…'`, `"…"`, or after
        // a backslash escape.
        let segments = quote_aware_split(command);

        // Flag command substitution / subshell constructs that need a
        // full parser. Surface as one whole-command check so the user
        // sees the unfamiliar form before any segment runs.
        let has_substitution = command.contains("$(")
            || command.contains('`')
            || command.contains("<(")
            || command.contains(">(");
        if has_substitution {
            return check_perm(permission, ask_tx, "bash", command).await;
        }
        for segment in &segments {
            check_perm(permission, ask_tx, "bash", segment).await?;
        }
        Ok(())
    }
}

/// Split a shell command on `;`, `&&`, `||` separators that appear
/// OUTSIDE single quotes, double quotes, or backslash escapes.
/// Used only on the no-`semantic-bash` build path — the
/// tree-sitter path delegates to the real bash grammar in
/// `semantic::adapters::bash` and doesn't need this.
///
/// Edge cases:
/// - `echo "; rm"` → one segment (the `;` is quoted).
/// - `echo 'a&&b'` → one segment.
/// - `echo \; ls` → one segment (the `;` is escaped).
/// - `cmd1; cmd2 && cmd3` → three segments, trimmed.
/// - Empty / whitespace-only segments dropped.
#[cfg_attr(feature = "semantic-bash", allow(dead_code))]
fn quote_aware_split(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_backslash = false;

    while i < bytes.len() {
        let b = bytes[i];

        if prev_backslash {
            prev_backslash = false;
            i += 1;
            continue;
        }

        if b == b'\\' && !in_single {
            // Inside single quotes, backslash is literal; otherwise it
            // escapes the next byte.
            prev_backslash = true;
            i += 1;
            continue;
        }

        if !in_double && b == b'\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && b == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }

        if !in_single && !in_double {
            // Check for `&&` and `||` (2-byte) BEFORE single-byte `;`.
            if i + 1 < bytes.len()
                && ((b == b'&' && bytes[i + 1] == b'&') || (b == b'|' && bytes[i + 1] == b'|'))
            {
                push_segment(command, start, i, &mut segments);
                i += 2;
                start = i;
                continue;
            }
            if b == b';' {
                push_segment(command, start, i, &mut segments);
                i += 1;
                start = i;
                continue;
            }
        }

        i += 1;
    }

    push_segment(command, start, bytes.len(), &mut segments);
    segments
}

#[cfg_attr(feature = "semantic-bash", allow(dead_code))]
fn push_segment<'a>(command: &'a str, start: usize, end: usize, out: &mut Vec<&'a str>) {
    if end <= start {
        return;
    }
    let s = command[start..end].trim();
    if !s.is_empty() {
        out.push(s);
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

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
}
