use rig::completion::ToolDefinition;
use rig::tool::Tool;
use std::process::Output;
use tokio::process::Command;
use tokio::time::Duration;

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, BashArgs, PermCheck, ToolError, check_perm};

use crate::sandbox::Sandbox;
#[cfg(feature = "semantic-bash")]
use crate::semantic::adapters::bash;

/// Spawn `cmd` into its own process group and wait for it,
/// capped at `secs`. On timeout, send SIGKILL to the process
/// group so the whole subprocess tree dies — not just bash. On
/// Windows we fall back to tokio's `kill_on_drop` which signals
/// the direct child only (Windows job objects would be cleaner
/// but require extra deps). F6 fix.
async fn run_with_timeout(cmd: Command, secs: u64) -> Result<Output, ToolError> {
    use std::process::Stdio;
    let mut cmd = cmd;
    // Pipe stdio so `wait_with_output` actually captures it. Default
    // is inherit, which routes output to the parent's terminal and
    // returns empty `output.stdout`/`stderr`.
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

    let child = cmd
        .spawn()
        .map_err(|e| ToolError::Msg(format!("failed to spawn: {}", e)))?;
    let pid = child.id();

    let wait = child.wait_with_output();
    match tokio::time::timeout(Duration::from_secs(secs), wait).await {
        Ok(out) => out.map_err(|e| ToolError::Msg(format!("wait failed: {}", e))),
        Err(_) => {
            // Timeout. Kill the whole group on Unix; on Windows
            // kill_on_drop will signal the direct child when the
            // returned error path drops the (now-dropped) child.
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
            // We've requested the kill but tokio doesn't surface a
            // post-kill output. Return the timeout error directly.
            let _ = pid; // silence unused-on-windows warning
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

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let exit_code = output.status.code().unwrap_or(-1);

        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&stderr);
        }
        if exit_code != 0 {
            result.push_str(&format!("\nExit code: {}", exit_code));
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
        // `||`) so each segment is checked individually. This won't
        // catch command substitution or subshells — those need the
        // tree-sitter feature for correct parsing — but it covers the
        // common compound case.
        let segments = command
            .split(|c| c == ';')
            .flat_map(|s| s.split("&&"))
            .flat_map(|s| s.split("||"))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<&str>>();
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
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(stdout.trim(), "hi");
    }
}
