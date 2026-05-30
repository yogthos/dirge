//! Background-shell registry — the Claude-Code-style model for detached
//! `bash` commands: a command started with `background: true` runs
//! UNBOUNDED (no timeout kill), its output streams into a per-shell
//! buffer the model pulls incrementally via the `bash_output` tool, and
//! it is stopped explicitly via the `kill_shell` tool (or killed en-masse
//! when the session ends).
//!
//! This is distinct from the background-SUBAGENT store
//! (`background.rs`), which uses a push-once completion notification —
//! the wrong fit for long-lived processes (dev servers, watchers) that
//! never "complete" and whose output must be readable while running.
//!
//! Memory model: each entry holds only the UNREAD output. `read_new`
//! returns and clears it, so a model that polls regularly keeps the
//! buffer small; an unread buffer is hard-capped so a never-read flood
//! can't OOM.

use indexmap::IndexMap;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

use crate::agent::tools::ToolError;

/// Process-global background-shell registry. There is exactly one
/// interactive session per process, so the bash tool (which spawns
/// shells), the `bash_output`/`kill_shell` tools, the status bar, and
/// session-end cleanup all share this one instance — avoiding threading
/// it through every builder/UI signature. (Same pattern dirge already
/// uses for the subagent `/kill` abort registry.) Tests inject their own
/// store instead of touching the global, so they stay isolated.
static GLOBAL: std::sync::LazyLock<BackgroundShellStore> =
    std::sync::LazyLock::new(BackgroundShellStore::new);

/// A clone (cheap — `Arc` inside) of the process-global shell registry.
pub fn global() -> BackgroundShellStore {
    GLOBAL.clone()
}

/// Hard cap on a single shell's UNREAD output buffer. Past this the
/// buffer stops growing and a one-time truncation marker is appended;
/// the model should `bash_output` regularly to drain it.
const MAX_UNREAD_BYTES: usize = 1024 * 1024;

/// Max number of shells retained (running + finished-but-unread). Oldest
/// by insertion order is evicted past this — generous for any session.
const STORE_CAPACITY: usize = 32;

/// Max concurrently RUNNING background shells. A runaway model shouldn't
/// be able to spawn unbounded detached processes.
const MAX_CONCURRENT_SHELLS: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub enum ShellStatus {
    Running,
    Exited(i32),
    /// Killed via `kill_shell` / session end.
    Killed,
    /// Failed to spawn or drain (carries the error).
    Failed(String),
}

impl ShellStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, ShellStatus::Running)
    }
    /// One-word label for the model-facing output / `/tasks` listing.
    pub fn label(&self) -> String {
        match self {
            ShellStatus::Running => "running".to_string(),
            ShellStatus::Exited(code) => format!("exited({code})"),
            ShellStatus::Killed => "killed".to_string(),
            ShellStatus::Failed(e) => format!("failed: {e}"),
        }
    }
}

struct ShellEntry {
    /// The command line, for the `/tasks` listing and tool feedback.
    command: String,
    /// Output produced since the last `read_new` (drained on read).
    unread: String,
    /// True once `unread` hit the cap and we stopped appending.
    truncated: bool,
    status: ShellStatus,
    /// Drain task handle. Aborting it drops the child + its
    /// `PgKillGuard`, SIGKILLing the whole process group. `None` once the
    /// shell has reached a terminal state.
    handle: Option<JoinHandle<()>>,
}

/// One row of the `/tasks` listing / model-facing status.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellInfo {
    pub id: String,
    pub command: String,
    pub status: ShellStatus,
}

/// Thread-safe registry of background shells. Cloneable (`Arc` inside) so
/// the bash tool, the `bash_output`/`kill_shell` tools, and the UI all
/// share one instance.
#[derive(Debug, Clone, Default)]
pub struct BackgroundShellStore {
    inner: Arc<Mutex<IndexMap<String, ShellEntry>>>,
}

// Manual Debug for ShellEntry (JoinHandle isn't Debug-friendly to print).
impl std::fmt::Debug for ShellEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellEntry")
            .field("command", &self.command)
            .field("unread_len", &self.unread.len())
            .field("truncated", &self.truncated)
            .field("status", &self.status)
            .field("has_handle", &self.handle.is_some())
            .finish()
    }
}

impl BackgroundShellStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compile-time cap on concurrently running shells.
    pub fn max_concurrent() -> usize {
        MAX_CONCURRENT_SHELLS
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, IndexMap<String, ShellEntry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a freshly-started shell in `Running` state. Evicts the
    /// oldest entry if at capacity.
    pub fn register(&self, id: String, command: String) {
        let mut map = self.lock();
        if !map.contains_key(&id) && map.len() >= STORE_CAPACITY {
            map.shift_remove_index(0);
        }
        map.insert(
            id,
            ShellEntry {
                command,
                unread: String::new(),
                truncated: false,
                status: ShellStatus::Running,
                handle: None,
            },
        );
    }

    /// Attach the drain-task handle so `kill`/`kill_all` can abort it.
    pub fn attach_handle(&self, id: &str, handle: JoinHandle<()>) {
        let mut map = self.lock();
        match map.get_mut(id) {
            // Only keep the handle while the shell is still tracked AND
            // running; a race where the drain finished first leaves the
            // terminal status intact and just drops the (already-done)
            // handle.
            Some(e) if e.status.is_running() => e.handle = Some(handle),
            _ => handle.abort(),
        }
    }

    /// Append a chunk of streamed output. No-op for an unknown id.
    /// Bounded: once the unread buffer hits the cap, further output is
    /// dropped with a one-time marker (the model should poll to drain).
    pub fn append(&self, id: &str, chunk: &str) {
        let mut map = self.lock();
        let Some(e) = map.get_mut(id) else {
            return;
        };
        if e.unread.len() + chunk.len() <= MAX_UNREAD_BYTES {
            e.unread.push_str(chunk);
        } else if !e.truncated {
            e.truncated = true;
            let room = MAX_UNREAD_BYTES.saturating_sub(e.unread.len());
            // Push a UTF-8-safe prefix of the chunk up to the cap, then a marker.
            let mut take = room.min(chunk.len());
            while take > 0 && !chunk.is_char_boundary(take) {
                take -= 1;
            }
            e.unread.push_str(&chunk[..take]);
            e.unread.push_str(
                "\n…[background shell output exceeded the unread-buffer cap; call bash_output more often to drain it]",
            );
        }
    }

    /// Record a terminal status and drop the drain handle. No-op if the
    /// id is unknown or already terminal (first terminal wins, so a
    /// `kill` racing a natural exit doesn't clobber the real exit code).
    pub fn finish(&self, id: &str, status: ShellStatus) {
        let mut map = self.lock();
        if let Some(e) = map.get_mut(id)
            && e.status.is_running()
        {
            e.status = status;
            e.handle = None;
        }
    }

    /// Return output produced since the last read (clearing it) plus the
    /// current status. `None` if the id is unknown.
    pub fn read_new(&self, id: &str) -> Option<(String, ShellStatus)> {
        let mut map = self.lock();
        let e = map.get_mut(id)?;
        let out = std::mem::take(&mut e.unread);
        e.truncated = false;
        Some((out, e.status.clone()))
    }

    /// Kill a running shell by id: abort its drain task (which drops the
    /// child and SIGKILLs the process group) and mark it `Killed`.
    /// Returns true if a running shell was found and killed.
    pub fn kill(&self, id: &str) -> bool {
        let mut map = self.lock();
        let Some(e) = map.get_mut(id) else {
            return false;
        };
        if !e.status.is_running() {
            return false;
        }
        if let Some(h) = e.handle.take() {
            h.abort();
        }
        e.status = ShellStatus::Killed;
        true
    }

    /// Number of shells currently running (drives the status bar).
    pub fn running_count(&self) -> usize {
        self.lock()
            .values()
            .filter(|e| e.status.is_running())
            .count()
    }

    /// Snapshot of every tracked shell, newest last.
    pub fn list(&self) -> Vec<ShellInfo> {
        self.lock()
            .iter()
            .map(|(id, e)| ShellInfo {
                id: id.clone(),
                command: e.command.clone(),
                status: e.status.clone(),
            })
            .collect()
    }

    /// Abort every running shell — called on session swap / shutdown so
    /// detached processes don't outlive the session that started them.
    pub fn kill_all(&self) {
        let mut map = self.lock();
        for e in map.values_mut() {
            if e.status.is_running() {
                if let Some(h) = e.handle.take() {
                    h.abort();
                }
                e.status = ShellStatus::Killed;
            }
        }
    }
}

// ── Model-facing tools ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct BashOutputArgs {
    pub id: String,
}

/// `bash_output` — read output produced by a background shell since the
/// last read, plus its current status. Mirrors Claude Code's BashOutput.
pub struct BashOutputTool {
    store: BackgroundShellStore,
}

impl BashOutputTool {
    pub fn new(store: BackgroundShellStore) -> Self {
        Self { store }
    }
}

impl Tool for BashOutputTool {
    const NAME: &'static str = "bash_output";
    type Error = ToolError;
    type Args = BashOutputArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "bash_output".to_string(),
            description: "Read new output from a background shell (one started with bash(background=true)). Returns the output produced since your last call plus the shell's status (running / exited(code) / killed / failed). Poll this to follow a long-running command; call kill_shell to stop it.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The background shell id returned by bash(background=true)." }
                },
                "required": ["id"]
            }),
        }
    }

    async fn call(&self, args: BashOutputArgs) -> Result<String, ToolError> {
        match self.store.read_new(&args.id) {
            Some((out, status)) => {
                let body = if out.is_empty() {
                    "(no new output)".to_string()
                } else {
                    out
                };
                Ok(format!(
                    "[shell {} — {}]\n{}",
                    args.id,
                    status.label(),
                    body
                ))
            }
            None => Err(ToolError::Msg(format!(
                "no background shell with id {:?} (it may have been evicted)",
                args.id
            ))),
        }
    }
}

#[derive(Deserialize)]
pub struct KillShellArgs {
    pub id: String,
}

/// `kill_shell` — stop a running background shell by id (SIGKILLs its
/// process group). Mirrors Claude Code's KillShell / TaskStop.
pub struct KillShellTool {
    store: BackgroundShellStore,
}

impl KillShellTool {
    pub fn new(store: BackgroundShellStore) -> Self {
        Self { store }
    }
}

impl Tool for KillShellTool {
    const NAME: &'static str = "kill_shell";
    type Error = ToolError;
    type Args = KillShellArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "kill_shell".to_string(),
            description: "Stop a running background shell (one started with bash(background=true)) by id. Kills the whole process group. No-op if it already exited.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The background shell id to kill." }
                },
                "required": ["id"]
            }),
        }
    }

    async fn call(&self, args: KillShellArgs) -> Result<String, ToolError> {
        if self.store.kill(&args.id) {
            Ok(format!("killed background shell {}", args.id))
        } else {
            Ok(format!(
                "no running background shell with id {:?} (already exited or unknown)",
                args.id
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_new_drains_unread_and_reports_status() {
        let s = BackgroundShellStore::new();
        s.register("a".into(), "sleep 1".into());
        s.append("a", "line1\n");
        s.append("a", "line2\n");
        let (out, st) = s.read_new("a").unwrap();
        assert_eq!(out, "line1\nline2\n");
        assert_eq!(st, ShellStatus::Running);
        // Second read sees only new output.
        s.append("a", "line3\n");
        let (out2, _) = s.read_new("a").unwrap();
        assert_eq!(out2, "line3\n");
        // Nothing new.
        let (out3, _) = s.read_new("a").unwrap();
        assert_eq!(out3, "");
    }

    #[test]
    fn read_new_unknown_id_is_none() {
        let s = BackgroundShellStore::new();
        assert!(s.read_new("nope").is_none());
    }

    #[test]
    fn finish_sets_terminal_and_first_terminal_wins() {
        let s = BackgroundShellStore::new();
        s.register("a".into(), "x".into());
        assert_eq!(s.running_count(), 1);
        s.finish("a", ShellStatus::Exited(0));
        let (_, st) = s.read_new("a").unwrap();
        assert_eq!(st, ShellStatus::Exited(0));
        assert_eq!(s.running_count(), 0);
        // A late kill/finish does not clobber the recorded exit.
        s.finish("a", ShellStatus::Killed);
        assert_eq!(s.read_new("a").unwrap().1, ShellStatus::Exited(0));
    }

    #[test]
    fn kill_marks_killed_only_when_running() {
        let s = BackgroundShellStore::new();
        s.register("a".into(), "x".into());
        assert!(s.kill("a"));
        assert_eq!(s.read_new("a").unwrap().1, ShellStatus::Killed);
        // Already terminal → kill is a no-op.
        assert!(!s.kill("a"));
        assert!(!s.kill("unknown"));
    }

    #[test]
    fn unread_buffer_is_capped() {
        let s = BackgroundShellStore::new();
        s.register("a".into(), "flood".into());
        let chunk = "x".repeat(100_000);
        for _ in 0..20 {
            s.append("a", &chunk);
        }
        let (out, _) = s.read_new("a").unwrap();
        assert!(out.len() <= MAX_UNREAD_BYTES + 200, "len was {}", out.len());
        assert!(out.contains("exceeded the unread-buffer cap"));
    }

    #[test]
    fn list_reports_all_shells() {
        let s = BackgroundShellStore::new();
        s.register("a".into(), "cmd-a".into());
        s.register("b".into(), "cmd-b".into());
        s.finish("b", ShellStatus::Exited(1));
        let rows = s.list();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].command, "cmd-a");
        assert_eq!(rows[1].status, ShellStatus::Exited(1));
    }
}
