//! Process-execution layer for the bash tool. Split out of
//! `agent/tools/bash.rs` (dirge-4y4l stage 9a): spawns commands into their
//! own process group, drains stdout+stderr in arrival order, and enforces
//! timeouts / process-group cleanup. Used by `BashTool::call` (synchronous,
//! bounded runs via [`run_with_timeout`]) and the background-shell path
//! (detached, streamed runs via [`spawn_streaming_shell`]).

use tokio::process::Command;
use tokio::time::Duration;

use crate::agent::tools::ToolError;

/// Captured output with stdout + stderr lines preserved in arrival
/// order. Replaces tokio's `Output` (which collects each stream as
/// a separate blob, losing time ordering between them — F12).
#[derive(Debug)]
pub(crate) struct InterleavedOutput {
    /// Lines in the order they arrived from EITHER pipe.
    pub merged: String,
    pub exit_code: i32,
}

/// On Unix, SIGKILL the bash process group on drop. Used to clean up
/// grandchildren when the agent task is aborted (Ctrl+C) — tokio's
/// `kill_on_drop` only signals the immediate child, leaving descendants
/// orphaned. Disarmed via [`PgKillGuard::disarm`] on graceful paths
/// (successful completion, timeout — which already calls killpg itself)
/// so we don't double-signal.
#[cfg(unix)]
struct PgKillGuard {
    pid: u32,
    armed: bool,
}

#[cfg(unix)]
impl PgKillGuard {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for PgKillGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: killpg with negative pid sends to the process
        // group. SIGKILL is the same on every POSIX platform;
        // libc::pid_t is i32 on every platform dirge supports. The
        // pid was set by us via `process_group(0)` so we know this
        // group exists and is bash + descendants.
        unsafe {
            let _ = libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

/// Spawn `cmd` into its own process group and wait for it,
/// capped at `secs`. On timeout, send SIGKILL to the process
/// group so the whole subprocess tree dies — not just bash. On
/// Windows we fall back to tokio's `kill_on_drop` which signals
/// the direct child only (Windows job objects would be cleaner
/// but require extra deps). F6 + F12 fix.
pub(super) async fn run_with_timeout(
    cmd: Command,
    secs: u64,
) -> Result<InterleavedOutput, ToolError> {
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
    // pid drives only the Unix process-group kill paths below; on
    // non-unix it's unused — consume it so `-D warnings` passes.
    #[cfg(not(unix))]
    let _ = pid;

    // Drop guard: on Unix, `kill_on_drop(true)` SIGKILLs the immediate
    // bash child when the future is dropped (e.g. user Ctrl+C aborts
    // the agent task) but leaves bash's *descendants* running as
    // grandchildren of pid 1. The timeout branch below already
    // handles this by calling `killpg(-pid, SIGKILL)`; the same is
    // needed for any other drop path. Holding a `PgKillGuard` for
    // the lifetime of the future does that.
    #[cfg(unix)]
    let _pgguard = pid.map(PgKillGuard::new);

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
        // TOOL-7: streaming cap. Previously this loop appended
        // every line to `merged` without bound; a child writing
        // GBs (`cat /dev/urandom | head -c 10G`) would buffer all
        // of it in memory before the post-drain truncation. Stop
        // appending once we cross the cap; keep draining so the
        // child doesn't block on a full pipe buffer (which would
        // hold open the pgid past the timeout).
        const DRAIN_CAP_BYTES: usize = 256 * 1024;
        let mut overflow_bytes: usize = 0;
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
                    Ok(Some(n)) => {
                        if merged.len() < DRAIN_CAP_BYTES {
                            merged.push_str(&so_buf);
                        } else {
                            overflow_bytes = overflow_bytes.saturating_add(n);
                        }
                    },
                },
                r = se_fut, if has_se => match r {
                    Ok(Some(0)) | Ok(None) | Err(_) => { se = None; }
                    Ok(Some(n)) => {
                        if merged.len() < DRAIN_CAP_BYTES {
                            merged.push_str(&se_buf);
                        } else {
                            overflow_bytes = overflow_bytes.saturating_add(n);
                        }
                    },
                },
            }
        }
        if overflow_bytes > 0 {
            if !merged.is_empty() && !merged.ends_with('\n') {
                merged.push('\n');
            }
            merged.push_str(&format!(
                "…[bash output exceeded cap; discarded {} additional bytes streamed after the {}-KiB cap]",
                overflow_bytes,
                DRAIN_CAP_BYTES / 1024,
            ));
        }
        merged
    };

    let wait = async {
        let merged = drain.await;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((merged, status))
    };

    let outcome = tokio::time::timeout(Duration::from_secs(secs), wait).await;
    match outcome {
        Ok(Ok((merged, status))) => {
            // Graceful completion — process group is already gone.
            // Disarm the guard so its Drop doesn't issue a useless
            // SIGKILL against a reaped pgid (worst case: signal
            // races into a PID re-used by the OS).
            #[cfg(unix)]
            {
                let mut g = _pgguard;
                if let Some(ref mut gg) = g {
                    gg.disarm();
                }
            }
            Ok(InterleavedOutput {
                merged,
                exit_code: status.code().unwrap_or(-1),
            })
        }
        Ok(Err(e)) => Err(ToolError::Msg(format!("wait failed: {}", e))),
        Err(_) => {
            // Timeout path already issues the killpg below; disarm
            // the drop guard so we don't double-signal.
            #[cfg(unix)]
            {
                let mut g = _pgguard;
                if let Some(ref mut gg) = g {
                    gg.disarm();
                }
                if let Some(pid) = pid {
                    // SAFETY: killpg with negative pid sends to the
                    // process group. SIGKILL is the same on every
                    // POSIX platform; libc::pid_t is i32 on every
                    // platform dirge supports.
                    unsafe {
                        let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    }
                }
            }
            let _ = pid;
            Err(ToolError::Msg(format!("Command timed out after {}s", secs)))
        }
    }
}

/// Spawn `cmd` detached and stream its output into the background-shell
/// store as it arrives. Returns the drain-task `JoinHandle` so the store
/// can abort it (which drops the child + its `PgKillGuard`, SIGKILLing the
/// whole process group). `timeout` is optional: `None` runs unbounded
/// (the Claude-Code model — for dev servers / watchers); `Some(secs)`
/// auto-kills after that long. The task records a terminal `ShellStatus`
/// on the store when it ends.
///
/// Process-group cleanup is Unix-only (same as `run_with_timeout`): on
/// Windows there's no `process_group`/`PgKillGuard`, so aborting the task
/// signals only the immediate child via `kill_on_drop` — a backgrounded
/// process's grandchildren can be orphaned. Background shells are an
/// inherently Unix-oriented feature; Windows is best-effort.
pub(super) fn spawn_streaming_shell(
    cmd: Command,
    store: crate::agent::tools::bg_shell::BackgroundShellStore,
    id: String,
    timeout: Option<u64>,
) -> tokio::task::JoinHandle<()> {
    use crate::agent::tools::bg_shell::ShellStatus;
    use std::process::Stdio;
    tokio::spawn(async move {
        let mut cmd = cmd;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                store.finish(&id, ShellStatus::Failed(format!("failed to spawn: {e}")));
                return;
            }
        };
        let pid = child.id();
        // Unix-only process-group kill paths use pid; consume on non-unix.
        #[cfg(not(unix))]
        let _ = pid;
        #[cfg(unix)]
        let mut pgguard = pid.map(PgKillGuard::new);

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // Stream stdout + stderr line-by-line into the store in arrival
        // order. The closure borrows `store`/`id`, so keep it scoped.
        let drain = {
            let store = store.clone();
            let id = id.clone();
            async move {
                use tokio::io::AsyncBufReadExt;
                let mut so = stdout.map(tokio::io::BufReader::new);
                let mut se = stderr.map(tokio::io::BufReader::new);
                loop {
                    let has_so = so.is_some();
                    let has_se = se.is_some();
                    if !has_so && !has_se {
                        break;
                    }
                    let mut so_buf = String::new();
                    let mut se_buf = String::new();
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
                            Ok(Some(_)) => store.append(&id, &so_buf),
                        },
                        r = se_fut, if has_se => match r {
                            Ok(Some(0)) | Ok(None) | Err(_) => { se = None; }
                            Ok(Some(_)) => store.append(&id, &se_buf),
                        },
                    }
                }
            }
        };

        let wait = async {
            drain.await;
            child.wait().await
        };

        let status = match timeout {
            Some(secs) => match tokio::time::timeout(Duration::from_secs(secs), wait).await {
                Ok(Ok(st)) => ShellStatus::Exited(st.code().unwrap_or(-1)),
                Ok(Err(e)) => ShellStatus::Failed(e.to_string()),
                Err(_) => {
                    // Timed out: SIGKILL the whole group, then mark killed.
                    #[cfg(unix)]
                    if let Some(pid) = pid {
                        unsafe {
                            let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                        }
                    }
                    let _ = pid;
                    ShellStatus::Failed(format!("auto-killed after {secs}s timeout"))
                }
            },
            None => match wait.await {
                Ok(st) => ShellStatus::Exited(st.code().unwrap_or(-1)),
                Err(e) => ShellStatus::Failed(e.to_string()),
            },
        };
        // We only reach here when the task was NOT aborted: the process
        // has already exited (natural) or been SIGKILLed (timeout). Disarm
        // the guard so its Drop doesn't re-signal a now-reaped (possibly
        // OS-recycled) process-group id — matching `run_with_timeout`. On
        // abort (kill_shell / kill_all) we never get here, so the guard
        // fires on drop and kills the group — which is how kill works.
        #[cfg(unix)]
        if let Some(g) = pgguard.as_mut() {
            g.disarm();
        }
        store.finish(&id, status);
    })
}
