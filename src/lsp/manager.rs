//! `LspManager` — lazy-spawn LSP clients per (workspace root, server-id) and
//! fan tool requests out across them.
//!
//! One manager per agent session. Constructed in `main::build_channels`,
//! threaded through `build_agent` and `run_interactive` (the same plumbing
//! pattern used for `BackgroundStore`).
//!
//! What it does in Phase 4:
//! - Walk the built-in [`crate::lsp::server`] registry to find which servers
//!   claim a given file extension.
//! - Resolve workspace roots via each server's `root` function.
//! - Spawn LSP servers on demand, dedupe in-flight spawns (so two parallel
//!   tool calls don't race two `rust-analyzer` processes), cache the result
//!   by `(root, server_id)`.
//! - Track a "broken" set so spawn failures aren't re-attempted.
//! - `touch_file(path, mode)`: open / re-sync the file on every claiming
//!   client so push diagnostics flow.
//! - Fan-out helpers for the LSP request methods the agent tool (Phase 5)
//!   will expose: hover, definition, references, implementation,
//!   document_symbol, workspace_symbol, prepareCallHierarchy + incoming/
//!   outgoingCalls.
//!
//! On `Drop`, every spawned client's `_guard` drops (which kills the child
//! process via `kill_on_drop(true)` on the real spawner).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Diagnostic,
    DocumentSymbolResponse, GotoDefinitionResponse, Hover, Location, WorkspaceSymbolResponse,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::sync::Notify;

use crate::lsp::client::{LspClient, LspError};
use crate::lsp::init::initialize;
use crate::lsp::rpc::RpcClient;
use crate::lsp::server::{self, ServerInfo};
use crate::lsp::spawn::Spawner;
use crate::lsp::uri::path_to_file_uri_string;

/// Time we'll let any non-initialize LSP request take. Generous — LSP servers
/// can be slow on first-touch indexing — but bounded so a stuck server
/// doesn't hold up the agent's turn forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How a [`LspManager::touch_file`] call should handle diagnostics:
/// - `None`: just send didOpen / didChange and return.
/// - `Document(after)`: wait for the next push for this file after the given
///   `Instant`, with a timeout. Used by the edit tool to surface fresh
///   diagnostics in its tool result.
#[derive(Debug, Clone, Copy)]
pub enum TouchMode {
    Notify,
    AwaitPush { after: Instant, timeout: Duration },
}

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("LSP server {server_id:?} failed to spawn: {source}")]
    SpawnFailed {
        server_id: String,
        #[source]
        source: std::io::Error,
    },
    #[error("LSP server {server_id:?} initialize handshake failed: {source}")]
    InitializeFailed {
        server_id: String,
        #[source]
        source: crate::lsp::rpc::RpcError,
    },
    #[error(transparent)]
    Client(#[from] LspError),
}

/// One cached LSP connection.
pub struct ClientEntry {
    client: LspClient,
    server_id: String,
    root: PathBuf,
    /// Holds the child process / fake-server task; drops on manager
    /// shutdown to terminate the connection.
    #[allow(dead_code)]
    guard: Box<dyn std::any::Any + Send + Sync>,
}

/// Tracks a server that's failed (spawn refusal or runtime crash)
/// and how aggressively we should back off before retrying. Without
/// this state, an LSP that crashed once stayed marked-broken for
/// the entire dirge session — the agent kept editing without
/// diagnostics. Exponential backoff with a cap caps the
/// thrash if a server is genuinely uninstallable while still
/// recovering automatically from transient crashes.
#[derive(Debug)]
struct BrokenState {
    last_failure: std::time::Instant,
    attempts: u32,
}

impl BrokenState {
    /// Backoff window for the current attempt count.
    /// 1st failure → 1s, 2nd → 2s, 3rd → 4s … capped at 10 min.
    /// Reads "have we waited long enough to retry yet?"
    ///
    /// Audit C4: was 10s initial which felt sluggish for transient
    /// crashes (LSP server segfault on a malformed parse, OOM
    /// while loading a fresh worktree) — the user expected
    /// diagnostics back within seconds, not tens of seconds. 1s
    /// initial recovers fast; if the server is genuinely broken
    /// (uninstalled binary, persistent config error) the
    /// exponential growth still hits the 10-min cap within ~10
    /// failures and stops thrashing.
    fn backoff(&self) -> std::time::Duration {
        // EXT-12: clamp on the Duration directly. The previous form
        // (`1u64 << exp`) was defensively gated by `exp.min(10)`,
        // but a future edit that lifts the exp cap past 63 would
        // silently overflow before the duration clamp. Computing
        // and clamping on `Duration` removes the shift-overflow
        // footgun and keeps the same 1s → 2s → 4s → … → 10min
        // ladder.
        const CAP: std::time::Duration = std::time::Duration::from_secs(600);
        let attempts = self.attempts.saturating_sub(1);
        // Cap the exponent before the shift so the multiplication
        // can't overflow even if `attempts` becomes pathological.
        let mul = 1u64.checked_shl(attempts.min(20)).unwrap_or(u64::MAX);
        std::time::Duration::from_secs(mul).min(CAP)
    }

    /// True while we're still inside the backoff window.
    fn still_cooling(&self) -> bool {
        self.last_failure.elapsed() < self.backoff()
    }
}

#[derive(Default)]
struct ManagerState {
    clients: HashMap<(PathBuf, String), Arc<ClientEntry>>,
    /// (root, server_id) pairs that have failed. Each entry tracks
    /// the last failure timestamp + attempt count so we back off
    /// exponentially on repeat failures and clear the entry once
    /// a fresh spawn or request succeeds. Previously a std::collections::HashSet
    /// with no expiration; once a server failed, it was dead for
    /// the rest of the session.
    broken: HashMap<(PathBuf, String), BrokenState>,
    /// In-flight spawns. The `Notify` is shared with every caller waiting
    /// on the same (root, server_id) so we never spawn two of the same
    /// server in parallel.
    spawning: HashMap<(PathBuf, String), Arc<Notify>>,
}

impl ManagerState {
    /// True if the (root, server_id) is broken AND still inside its
    /// backoff window. Entries past their backoff fall through and
    /// are treated as eligible for retry; the caller is responsible
    /// for calling `mark_broken` again if the retry also fails.
    fn is_broken_now(&self, key: &(PathBuf, String)) -> bool {
        self.broken.get(key).is_some_and(|s| s.still_cooling())
    }

    /// Record a failure. Increments the attempt count if the entry
    /// already exists (so backoff escalates), or seeds a fresh
    /// entry on first failure.
    fn mark_broken(&mut self, key: &(PathBuf, String)) {
        let entry = self.broken.entry(key.clone()).or_insert(BrokenState {
            last_failure: std::time::Instant::now(),
            attempts: 0,
        });
        entry.last_failure = std::time::Instant::now();
        entry.attempts = entry.attempts.saturating_add(1);
    }
}

/// One manager per agent session. Cheap to clone.
#[derive(Clone)]
pub struct LspManager {
    spawner: Arc<dyn Spawner>,
    /// Worktree boundary — `server.root` fn walks stop here.
    worktree: PathBuf,
    state: Arc<Mutex<ManagerState>>,
    /// Built-in registry. Cached so per-call code doesn't reallocate.
    servers: Arc<Vec<ServerInfo>>,
}

impl LspManager {
    #[allow(dead_code)]
    pub fn new(spawner: Arc<dyn Spawner>, worktree: impl Into<PathBuf>) -> Self {
        Self::with_servers(spawner, worktree, server::builtin_servers())
    }

    /// Construct an `LspManager` with an explicit server set —
    /// the host calls this when the user has per-server config
    /// overrides (extensions, disabled, etc.). The default `new`
    /// just delegates here with the unmodified builtin list.
    pub fn with_servers(
        spawner: Arc<dyn Spawner>,
        worktree: impl Into<PathBuf>,
        servers: Vec<ServerInfo>,
    ) -> Self {
        Self {
            spawner,
            worktree: worktree.into(),
            state: Arc::new(Mutex::new(ManagerState::default())),
            servers: Arc::new(servers),
        }
    }

    /// Resolve and (lazily) spawn every LSP client that claims `file`. Returns
    /// one `LspClient` per (server_id, root) attached to that file. Empty
    /// when no server claims the extension or every match is in the
    /// broken-set.
    pub async fn get_clients(&self, file: &Path) -> Vec<Arc<ClientEntry>> {
        let ext = file
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        let mut out = Vec::new();

        for server in self.servers.iter() {
            if !server
                .extensions
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&ext))
            {
                continue;
            }
            let Some(root) = (server.root)(file, &self.worktree) else {
                continue;
            };
            let key = (root.clone(), server.id.to_string());

            // Fast-path cache check.
            {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                // Broken servers stay skipped only while inside
                // their backoff window. Once the window elapses we
                // fall through to spawn fresh — `mark_broken`
                // already evicted the dead `clients` entry, so the
                // clients-get below misses and we spawn.
                if state.is_broken_now(&key) {
                    continue;
                }
                if let Some(entry) = state.clients.get(&key) {
                    out.push(Arc::clone(entry));
                    continue;
                }
            }

            // Either spawn fresh or join an in-flight spawn for this key.
            match self.get_or_spawn(server, &root, &key).await {
                Some(entry) => out.push(entry),
                None => continue,
            }
        }
        out
    }

    /// The dedupe-and-spawn dance, factored out so [`get_clients`] stays
    /// readable. Returns the cached or freshly-spawned entry, or `None` if
    /// spawn/initialize failed (in which case the key is now in `broken`).
    async fn get_or_spawn(
        &self,
        server: &ServerInfo,
        root: &Path,
        key: &(PathBuf, String),
    ) -> Option<Arc<ClientEntry>> {
        // Decide our role under the lock, then RELEASE it before any await
        // (the MutexGuard is not Send and must not cross `.await`).
        enum Slot {
            Wait(Arc<Notify>),
            Spawn(Arc<Notify>),
        }
        let slot = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // Re-check cache under the lock — could have landed between
            // the fast-path miss and now.
            if let Some(entry) = state.clients.get(key) {
                return Some(Arc::clone(entry));
            }
            if state.is_broken_now(key) {
                return None;
            }
            if let Some(notify) = state.spawning.get(key) {
                Slot::Wait(Arc::clone(notify))
            } else {
                // We're the spawner. Mark in-flight and keep the handle.
                let notify = Arc::new(Notify::new());
                state.spawning.insert(key.clone(), Arc::clone(&notify));
                Slot::Spawn(notify)
            }
        };

        // If another caller is already spawning this key, wait for them
        // instead of racing.
        let spawn_notify = match slot {
            Slot::Wait(notify) => {
                // Safety timeout in case we subscribed after `notify_waiters`
                // fired (Notify doesn't save permits for late subscribers).
                // The cache is authoritative either way — re-check after the
                // wait, regardless of how it terminated.
                let _ = tokio::time::timeout(Duration::from_secs(60), notify.notified()).await;
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = state.clients.get(key) {
                    return Some(Arc::clone(entry));
                }
                return None;
            }
            Slot::Spawn(notify) => notify,
        };

        // dirge-gt8c: arm a Drop guard so a cancelled future (Ctrl-C during a
        // cold spawn) releases the slot + wakes waiters instead of orphaning
        // it. The normal completion path below disarms it.
        let mut slot_guard = SpawnSlotGuard {
            state: Arc::clone(&self.state),
            key: key.clone(),
            notify: Arc::clone(&spawn_notify),
            armed: true,
        };

        // We're responsible for the spawn.
        let result = self.do_spawn(server, root).await;

        // Reached completion under our own control — disarm; the two-phase
        // cleanup below removes the slot and notifies waiters itself.
        slot_guard.armed = false;

        // Two-phase: update state under the lock, then log + signal outside.
        // Slot-disappeared warning is captured for emit after lock release.
        let outcome = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let notify = state.spawning.remove(key);
            let slot_missing = notify.is_none();
            let notify = notify.unwrap_or_else(|| Arc::new(Notify::new()));
            match result {
                Ok(entry) => {
                    let arc = Arc::new(entry);
                    state.clients.insert(key.clone(), Arc::clone(&arc));
                    // Clear any prior failure record — a successful
                    // respawn means the user fixed whatever was
                    // wrong (server installed, path repaired). The
                    // attempt counter should not survive recovery.
                    state.broken.remove(key);
                    SpawnOutcome::Inserted {
                        arc,
                        notify,
                        slot_missing,
                    }
                }
                Err(e) => {
                    state.mark_broken(key);
                    SpawnOutcome::Failed {
                        err: e,
                        notify,
                        slot_missing,
                    }
                }
            }
        };

        match outcome {
            SpawnOutcome::Inserted {
                arc,
                notify,
                slot_missing,
            } => {
                if slot_missing {
                    tracing::warn!(
                        "lsp: spawning slot for {:?} disappeared before spawn finished",
                        key
                    );
                }
                notify.notify_waiters();
                Some(arc)
            }
            SpawnOutcome::Failed {
                err,
                notify,
                slot_missing,
            } => {
                if slot_missing {
                    tracing::warn!(
                        "lsp: spawning slot for {:?} disappeared before spawn failed",
                        key
                    );
                }
                tracing::warn!(
                    server = %server.id,
                    root = %root.display(),
                    "lsp: spawn failed: {err}"
                );
                notify.notify_waiters();
                None
            }
        }
    }

    /// The actual spawn + initialize handshake.
    async fn do_spawn(
        &self,
        server: &ServerInfo,
        root: &Path,
    ) -> Result<ClientEntry, ManagerError> {
        let spawned =
            self.spawner
                .spawn(server.id, root)
                .await
                .map_err(|e| ManagerError::SpawnFailed {
                    server_id: server.id.to_string(),
                    source: e,
                })?;
        let crate::lsp::spawn::Spawned {
            reader,
            writer,
            init_options,
            guard,
        } = spawned;

        let buf_reader = BufReader::new(reader);
        let (rpc, _reader_task) = RpcClient::new(buf_reader, writer);
        let _ = initialize(&rpc, root, None, init_options)
            .await
            .map_err(|e| ManagerError::InitializeFailed {
                server_id: server.id.to_string(),
                source: e,
            })?;

        let client = LspClient::new(rpc).await;
        Ok(ClientEntry {
            client,
            server_id: server.id.to_string(),
            root: root.to_path_buf(),
            guard,
        })
    }

    /// Open or re-sync `path` on every claiming client. Caller passes
    /// [`TouchMode::Notify`] when they just want the server to know about
    /// the file, or [`TouchMode::AwaitPush`] to additionally block until a
    /// fresh push arrives. Errors from individual clients are logged but
    /// not surfaced — the agent should always make progress.
    pub async fn touch_file(&self, path: &Path, mode: TouchMode) {
        let clients = self.get_clients(path).await;
        for entry in clients {
            let send_at = Instant::now();
            match entry.client.notify_open(path).await {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(server = %entry.server_id, path = %path.display(), "notify_open failed: {e}");
                    continue;
                }
            }
            if let TouchMode::AwaitPush { after, timeout } = mode {
                let after = std::cmp::max(after, send_at);
                // B3-10 (audit fix): race push against textDocument/
                // diagnostic pull so lazy servers (clojure-lsp,
                // jdtls, clangd cold-start) still surface errors
                // instead of reporting clean diagnostics on
                // timeout. Pull silently falls back to push-only
                // when unsupported; net behaviour for push-only
                // servers is identical.
                if let Err(e) = entry
                    .client
                    .wait_for_push_or_pull(path, after, timeout)
                    .await
                {
                    tracing::debug!(server = %entry.server_id, path = %path.display(), "wait_for_push_or_pull: {e}");
                }
            }
        }
    }

    /// Snapshot of the currently-active LSP clients as `(server_id, root)`
    /// pairs. Used by the info panel to surface what's been spawned and
    /// where it's rooted. Includes both healthy and broken-but-cached
    /// clients (broken-set entries appear in `broken_servers`).
    pub fn active_servers(&self) -> Vec<(String, PathBuf)> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .clients
            .values()
            .map(|e| (e.server_id.clone(), e.root.clone()))
            .collect()
    }

    /// Servers we've marked broken (failed to spawn or crashed). Useful
    /// for the info panel to show degraded LSP status alongside the
    /// healthy ones.
    pub fn broken_servers(&self) -> Vec<(String, PathBuf)> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Only surface entries that are STILL inside their backoff
        // window. Entries past their backoff are conceptually
        // "eligible to retry" — the panel would mislead the user
        // by showing them as broken when the next file touch will
        // re-spawn.
        state
            .broken
            .iter()
            .filter(|(_, s)| s.still_cooling())
            .map(|((root, id), _)| (id.clone(), root.clone()))
            .collect()
    }

    /// Fan-out `textDocument/didClose` across every attached
    /// client for every file each one has open. Called once at
    /// session shutdown so LSP servers can release per-file
    /// state — without this, a long session that touches dozens
    /// of files leaks server-side parse trees / diagnostic caches
    /// for the lifetime of the server process.
    ///
    /// Best-effort: a server that's already gone won't be
    /// re-contacted, and individual `notify_close` failures are
    /// swallowed inside the client.
    pub async fn close_all_files(&self) {
        let entries: Vec<_> = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.clients.values().cloned().collect()
        };
        for entry in entries {
            entry.client.close_all().await;
        }
    }

    /// Diagnostics for a single file, merged across attached clients.
    /// Returns `None` when no client tracks the file (so the caller can
    /// distinguish "no diagnostics" from "untracked" and fall back to a
    /// canonical-path lookup). Avoids cloning the whole project diagnostic
    /// map the way [`all_diagnostics`](Self::all_diagnostics) does — O(one
    /// file) instead of O(all files).
    // Consumed by the LSP plugin harness (`lsp::harness::run_query`, reached
    // only under `cfg(feature = "plugin")`); dead in a no-plugin build like
    // the Windows `windows-default` set, where `-D warnings` would fail.
    #[allow(dead_code)]
    pub fn diagnostics_for(&self, file: &Path) -> Option<Vec<Diagnostic>> {
        let entries: Vec<_> = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.clients.values().cloned().collect()
        };
        let mut merged: Vec<Diagnostic> = Vec::new();
        let mut tracked = false;
        for entry in entries {
            let diags = entry.client.diagnostics_for(file);
            if !diags.is_empty() {
                tracked = true;
                merged.extend(diags);
            }
        }
        tracked.then_some(merged)
    }

    /// Aggregated diagnostics across all attached clients. Same key/dedupe
    /// semantics as [`LspClient::all_diagnostics`].
    pub fn all_diagnostics(&self) -> HashMap<PathBuf, Vec<Diagnostic>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entries: Vec<_> = state.clients.values().cloned().collect();
        drop(state);

        let mut merged: HashMap<PathBuf, Vec<Diagnostic>> = HashMap::new();
        for entry in entries {
            for (path, diags) in entry.client.all_diagnostics() {
                merged.entry(path).or_default().extend(diags);
            }
        }
        merged
    }

    // ---- Fan-out helpers for the agent's `lsp` tool (Phase 5) ----

    pub async fn hover(&self, file: &Path, line: u32, character: u32) -> Vec<Hover> {
        self.request_all(
            file,
            "textDocument/hover",
            position_params(file, line, character),
        )
        .await
    }

    pub async fn definition(
        &self,
        file: &Path,
        line: u32,
        character: u32,
    ) -> Vec<GotoDefinitionResponse> {
        self.request_all(
            file,
            "textDocument/definition",
            position_params(file, line, character),
        )
        .await
    }

    pub async fn references(&self, file: &Path, line: u32, character: u32) -> Vec<Vec<Location>> {
        let mut params = position_params(file, line, character);
        params["context"] = json!({"includeDeclaration": true});
        self.request_all(file, "textDocument/references", params)
            .await
    }

    pub async fn implementation(
        &self,
        file: &Path,
        line: u32,
        character: u32,
    ) -> Vec<GotoDefinitionResponse> {
        self.request_all(
            file,
            "textDocument/implementation",
            position_params(file, line, character),
        )
        .await
    }

    pub async fn document_symbol(&self, file: &Path) -> Vec<DocumentSymbolResponse> {
        let params = json!({
            "textDocument": { "uri": path_to_file_uri_string(file) }
        });
        self.request_all(file, "textDocument/documentSymbol", params)
            .await
    }

    pub async fn workspace_symbol(
        &self,
        anchor_file: &Path,
        query: &str,
    ) -> Vec<WorkspaceSymbolResponse> {
        let params = json!({ "query": query });
        self.request_all(anchor_file, "workspace/symbol", params)
            .await
    }

    pub async fn prepare_call_hierarchy(
        &self,
        file: &Path,
        line: u32,
        character: u32,
    ) -> Vec<Vec<CallHierarchyItem>> {
        self.request_all(
            file,
            "textDocument/prepareCallHierarchy",
            position_params(file, line, character),
        )
        .await
    }

    pub async fn incoming_calls(
        &self,
        file: &Path,
        line: u32,
        character: u32,
    ) -> Vec<Vec<CallHierarchyIncomingCall>> {
        self.call_hierarchy(file, line, character, "callHierarchy/incomingCalls")
            .await
    }

    pub async fn outgoing_calls(
        &self,
        file: &Path,
        line: u32,
        character: u32,
    ) -> Vec<Vec<CallHierarchyOutgoingCall>> {
        self.call_hierarchy(file, line, character, "callHierarchy/outgoingCalls")
            .await
    }

    async fn call_hierarchy<R: serde::de::DeserializeOwned + Default>(
        &self,
        file: &Path,
        line: u32,
        character: u32,
        method: &str,
    ) -> Vec<R> {
        let clients = self.get_clients(file).await;
        let mut out = Vec::new();
        for entry in clients {
            let prepared: Vec<CallHierarchyItem> = match entry
                .client
                .rpc()
                .request(
                    "textDocument/prepareCallHierarchy",
                    position_params(file, line, character),
                    REQUEST_TIMEOUT,
                )
                .await
            {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(first) = prepared.first() else {
                continue;
            };
            match entry
                .client
                .rpc()
                .request(method, json!({ "item": first }), REQUEST_TIMEOUT)
                .await
            {
                Ok(v) => out.push(v),
                Err(_) => continue,
            }
        }
        out
    }

    /// Fan a single `request<P, R>` out across every client claiming `file`.
    /// Per-client errors are swallowed and logged — the agent gets the
    /// successful responses; one slow / broken server doesn't poison the
    /// whole result.
    async fn request_all<P, R>(&self, file: &Path, method: &str, params: P) -> Vec<R>
    where
        P: Serialize + Clone,
        R: serde::de::DeserializeOwned,
    {
        let clients = self.get_clients(file).await;
        let mut out = Vec::new();
        for entry in clients {
            match entry
                .client
                .rpc()
                .request(method, params.clone(), REQUEST_TIMEOUT)
                .await
            {
                Ok(v) => out.push(v),
                Err(e) => {
                    // Distinguish transport-level death (server
                    // crashed) from LSP-level errors (e.g. unknown
                    // symbol). Only the former should evict the
                    // client + flag as broken; the latter just
                    // means this specific request failed but the
                    // server's still alive and we should keep
                    // talking to it.
                    let is_dead = matches!(
                        e,
                        crate::lsp::rpc::RpcError::ConnectionClosed
                            | crate::lsp::rpc::RpcError::Io(_)
                    );
                    if is_dead {
                        let key = (entry.root.clone(), entry.server_id.clone());
                        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                        // Only evict if the still-cached entry is
                        // the same instance — concurrent calls
                        // might have already replaced it with a
                        // fresh spawn.
                        if let Some(cached) = state.clients.get(&key)
                            && Arc::ptr_eq(cached, &entry)
                        {
                            state.clients.remove(&key);
                        }
                        state.mark_broken(&key);
                        tracing::warn!(
                            server = %entry.server_id,
                            method = %method,
                            "LSP server died ({e}); will retry after backoff",
                        );
                    } else {
                        tracing::debug!(
                            server = %entry.server_id,
                            method = %method,
                            "request failed: {e}",
                        );
                    }
                }
            }
        }
        out
    }
}

enum SpawnOutcome {
    Inserted {
        arc: Arc<ClientEntry>,
        notify: Arc<Notify>,
        slot_missing: bool,
    },
    Failed {
        err: ManagerError,
        notify: Arc<Notify>,
        slot_missing: bool,
    },
}

/// dirge-gt8c: RAII guard held by the caller responsible for a spawn.
/// If that caller's future is dropped mid-spawn (e.g. Ctrl-C during a
/// cold rust-analyzer / jdtls startup), the in-flight `spawning` slot
/// would otherwise be orphaned — every later caller then blocks on the
/// stale `Notify`, times out, and gets `None`, leaving the server
/// permanently unspawnable for the session. On drop the guard removes
/// the slot and wakes any waiters so the next request re-attempts the
/// spawn. The normal completion path disarms the guard (`armed = false`)
/// because it removes + notifies the slot itself.
struct SpawnSlotGuard {
    state: Arc<Mutex<ManagerState>>,
    key: (PathBuf, String),
    notify: Arc<Notify>,
    armed: bool,
}

impl Drop for SpawnSlotGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.spawning.remove(&self.key);
        }
        // Wake waiters AFTER releasing the lock — they re-check the cache,
        // find no client, and one of them becomes the new spawner.
        self.notify.notify_waiters();
    }
}

impl ClientEntry {
    #[allow(dead_code)]
    pub fn server_id(&self) -> &str {
        &self.server_id
    }
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }
    #[allow(dead_code)]
    pub fn client(&self) -> &LspClient {
        &self.client
    }
}

fn position_params(file: &Path, line: u32, character: u32) -> Value {
    json!({
        "textDocument": { "uri": path_to_file_uri_string(file) },
        "position": { "line": line, "character": character },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::spawn::Spawned;
    use futures::future::BoxFuture;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fixture: spawn counter + the actual mock from `spawn::tests`.
    struct CountingSpawner {
        spawn_calls: StdArc<AtomicUsize>,
        fail_keys: std::sync::Mutex<std::collections::HashSet<(String, PathBuf)>>,
        /// Delay each spawn by this much so concurrent calls have a chance
        /// to collide on the dedupe path. Atomic so tests can lengthen it
        /// (to park a spawn for cancellation) then shorten it for a retry.
        delay_ms: std::sync::atomic::AtomicU64,
    }

    impl CountingSpawner {
        fn new(delay_ms: u64) -> Self {
            Self {
                spawn_calls: StdArc::new(AtomicUsize::new(0)),
                fail_keys: std::sync::Mutex::new(std::collections::HashSet::new()),
                delay_ms: std::sync::atomic::AtomicU64::new(delay_ms),
            }
        }
        fn count(&self) -> usize {
            self.spawn_calls.load(Ordering::SeqCst)
        }
        fn set_delay(&self, ms: u64) {
            self.delay_ms.store(ms, Ordering::SeqCst);
        }
        fn fail_for(&self, server_id: &str, root: &Path) {
            self.fail_keys
                .lock()
                .unwrap()
                .insert((server_id.to_string(), root.to_path_buf()));
        }
        fn clear_failures(&self) {
            self.fail_keys.lock().unwrap().clear();
        }
    }

    impl Spawner for CountingSpawner {
        fn spawn<'a>(
            &'a self,
            server_id: &'a str,
            root: &'a Path,
        ) -> BoxFuture<'a, std::io::Result<Spawned>> {
            Box::pin(async move {
                self.spawn_calls.fetch_add(1, Ordering::SeqCst);
                let delay = self.delay_ms.load(Ordering::SeqCst);
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                if self
                    .fail_keys
                    .lock()
                    .unwrap()
                    .contains(&(server_id.to_string(), root.to_path_buf()))
                {
                    return Err(std::io::Error::other("forced fail"));
                }

                // Build a minimal duplex pair with a fake server that
                // answers `initialize` and ignores everything else.
                let (client_in, mut server_writer) = tokio::io::duplex(8192);
                let (mut server_reader, client_out) = tokio::io::duplex(8192);
                let fake_server = tokio::spawn(async move {
                    use crate::lsp::jsonrpc::{decode_frame, encode_frame};
                    let mut reader = tokio::io::BufReader::new(&mut server_reader);
                    loop {
                        let frame = match decode_frame(&mut reader).await {
                            Ok(b) => b,
                            Err(_) => break,
                        };
                        let req: Value = match serde_json::from_slice(&frame) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if req.get("id").is_none() {
                            continue;
                        }
                        let id = req["id"].clone();
                        let method = req["method"].as_str().unwrap_or("");
                        let result = if method == "initialize" {
                            json!({"capabilities": {}})
                        } else {
                            Value::Null
                        };
                        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
                        if encode_frame(&mut server_writer, &serde_json::to_vec(&resp).unwrap())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                Ok(Spawned {
                    reader: Box::new(tokio::io::BufReader::new(client_in)),
                    writer: Box::new(client_out),
                    init_options: Value::Null,
                    guard: Box::new(fake_server),
                })
            })
        }
    }

    /// Build a tempdir that looks like a Cargo workspace so the `rust`
    /// server's `root` fn succeeds against files inside it.
    fn cargo_tree(suffix: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "dirge-lsp-manager-test-{}-{}-{suffix}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "// hello\n").unwrap();
        root
    }

    #[tokio::test]
    async fn first_call_spawns_and_caches() {
        let tree = cargo_tree("first-spawn");
        let spawner = StdArc::new(CountingSpawner::new(0));
        let manager = LspManager::new(spawner.clone(), tree.clone());
        let file = tree.join("src/lib.rs");

        let clients = manager.get_clients(&file).await;
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].server_id(), "rust");
        assert_eq!(spawner.count(), 1);

        // Second call: cache hit, no extra spawn.
        let clients2 = manager.get_clients(&file).await;
        assert_eq!(clients2.len(), 1);
        assert_eq!(spawner.count(), 1);

        std::fs::remove_dir_all(&tree).ok();
    }

    /// dirge-gt8c: if the spawner's future is dropped mid-spawn (Ctrl-C
    /// during a cold rust-analyzer/jdtls startup), the in-flight slot must
    /// be released and waiters woken — otherwise that server is
    /// permanently unspawnable for the session. Asserts (1) the slot is
    /// freed after cancellation and (2) a subsequent call re-spawns and
    /// succeeds rather than hanging on the orphaned `Notify`.
    #[tokio::test]
    async fn cancelled_spawn_releases_slot_for_retry() {
        let tree = cargo_tree("cancel-retry");
        // Long cold-spawn delay so we can cancel while it's in-flight.
        let spawner = StdArc::new(CountingSpawner::new(10_000));
        let manager = LspManager::new(spawner.clone(), tree.clone());
        let file = tree.join("src/lib.rs");

        // Start the spawn, drive it until it parks in the slow do_spawn
        // await, then drop the future (simulating an abort).
        {
            let fut = manager.get_clients(&file);
            tokio::pin!(fut);
            let _ = tokio::time::timeout(Duration::from_millis(250), &mut fut).await;
            // `fut` dropped at end of block → SpawnSlotGuard fires.
        }
        // Let the guard's lock/notify settle.
        tokio::task::yield_now().await;
        assert_eq!(spawner.count(), 1, "first spawn should have been attempted");

        // The in-flight slot must be gone.
        {
            let state = manager.state.lock().unwrap_or_else(|e| e.into_inner());
            assert!(
                state.spawning.is_empty(),
                "cancelled spawn must release its in-flight slot, got {:?}",
                state.spawning.keys().collect::<Vec<_>>()
            );
            assert!(
                state.broken.is_empty(),
                "a cancelled (not failed) spawn must not mark the server broken",
            );
        }

        // A retry must actually re-spawn and succeed — proving the server
        // is NOT permanently disabled. Shorten the delay so it completes.
        spawner.set_delay(0);
        let clients = manager.get_clients(&file).await;
        assert_eq!(clients.len(), 1, "retry after cancellation must succeed");
        assert_eq!(clients[0].server_id(), "rust");
        assert_eq!(spawner.count(), 2, "retry must spawn afresh");

        std::fs::remove_dir_all(&tree).ok();
    }

    // Regression: two concurrent get_clients calls for the same file must
    // result in exactly ONE spawn. Without dedupe, every parallel tool call
    // would race two `rust-analyzer` processes for the same workspace.
    #[tokio::test]
    async fn regression_concurrent_get_clients_only_spawns_once() {
        let tree = cargo_tree("concurrent-spawn");
        let spawner = StdArc::new(CountingSpawner::new(40));
        let manager = LspManager::new(spawner.clone(), tree.clone());
        let file = tree.join("src/lib.rs");

        let a = {
            let manager = manager.clone();
            let file = file.clone();
            tokio::spawn(async move { manager.get_clients(&file).await })
        };
        let b = {
            let manager = manager.clone();
            let file = file.clone();
            tokio::spawn(async move { manager.get_clients(&file).await })
        };
        let c = {
            let manager = manager.clone();
            let file = file.clone();
            tokio::spawn(async move { manager.get_clients(&file).await })
        };
        let r_a = a.await.unwrap();
        let r_b = b.await.unwrap();
        let r_c = c.await.unwrap();
        assert_eq!(r_a.len(), 1);
        assert_eq!(r_b.len(), 1);
        assert_eq!(r_c.len(), 1);
        assert_eq!(
            spawner.count(),
            1,
            "must dedupe inflight spawns; got {}",
            spawner.count()
        );

        std::fs::remove_dir_all(&tree).ok();
    }

    // Regression: a spawn that fails must mark the (root, server_id) as
    // broken so future calls don't re-attempt. Hammering a misconfigured
    // server every tool call would be expensive and noisy.
    #[tokio::test]
    async fn regression_failed_spawn_marks_broken_and_no_retry() {
        let tree = cargo_tree("broken");
        let spawner = StdArc::new(CountingSpawner::new(0));
        let root_canon = tree.canonicalize().unwrap();
        spawner.fail_for("rust", &root_canon);
        let manager = LspManager::new(spawner.clone(), tree.clone());
        let file = tree.join("src/lib.rs");

        let first = manager.get_clients(&file).await;
        assert!(
            first.is_empty(),
            "first attempt should fail to produce a client"
        );
        assert_eq!(spawner.count(), 1);

        // Second call: must NOT retry — broken-set blocks.
        let second = manager.get_clients(&file).await;
        assert!(second.is_empty());
        assert_eq!(spawner.count(), 1, "must not retry broken servers");

        std::fs::remove_dir_all(&tree).ok();
    }

    /// A failed spawn STILL blocks retries within the backoff
    /// window — but after the backoff elapses, a subsequent call
    /// retries. Pin this by directly poking the `last_failure`
    /// timestamp backwards so the test doesn't need to sleep.
    #[tokio::test]
    async fn failed_spawn_retries_after_backoff_elapses() {
        let tree = cargo_tree("broken-retry");
        let spawner = StdArc::new(CountingSpawner::new(0));
        let root_canon = tree.canonicalize().unwrap();
        spawner.fail_for("rust", &root_canon);
        let manager = LspManager::new(spawner.clone(), tree.clone());
        let file = tree.join("src/lib.rs");

        // First failure marks broken with a 10s backoff.
        let _ = manager.get_clients(&file).await;
        assert_eq!(spawner.count(), 1);

        // Manually expire the backoff window — pretend last failure
        // was 30s ago so the 10s backoff has elapsed.
        {
            let mut state = manager.state.lock().unwrap();
            let key = (root_canon.clone(), "rust".to_string());
            if let Some(s) = state.broken.get_mut(&key) {
                s.last_failure = std::time::Instant::now()
                    .checked_sub(std::time::Duration::from_secs(30))
                    .expect("clock supports 30s backwards");
            }
        }

        // Second call (still failing) should retry now.
        let _ = manager.get_clients(&file).await;
        assert_eq!(
            spawner.count(),
            2,
            "backoff expired — should attempt respawn",
        );
        // Attempt count should now be 2, so backoff doubles to 2s
        // (audit C4 retuned initial from 10s → 1s; 2nd attempt = 2s).
        {
            let state = manager.state.lock().unwrap();
            let key = (root_canon, "rust".to_string());
            let entry = state.broken.get(&key).expect("still broken");
            assert_eq!(entry.attempts, 2, "attempts must escalate");
            assert_eq!(
                entry.backoff(),
                std::time::Duration::from_secs(2),
                "backoff escalates exponentially",
            );
        }

        std::fs::remove_dir_all(&tree).ok();
    }

    /// Successful respawn clears the broken record so the next
    /// failure starts fresh at attempt=1 (not continuing escalation).
    #[tokio::test]
    async fn successful_respawn_clears_broken_record() {
        let tree = cargo_tree("broken-recover");
        let spawner = StdArc::new(CountingSpawner::new(0));
        let root_canon = tree.canonicalize().unwrap();
        spawner.fail_for("rust", &root_canon);
        let manager = LspManager::new(spawner.clone(), tree.clone());
        let file = tree.join("src/lib.rs");

        // Fail once.
        let _ = manager.get_clients(&file).await;

        // Stop forcing failures, manually expire backoff, retry.
        spawner.clear_failures();
        {
            let mut state = manager.state.lock().unwrap();
            let key = (root_canon.clone(), "rust".to_string());
            if let Some(s) = state.broken.get_mut(&key) {
                s.last_failure = std::time::Instant::now()
                    .checked_sub(std::time::Duration::from_secs(30))
                    .expect("clock supports 30s backwards");
            }
        }
        let _ = manager.get_clients(&file).await;

        // Broken record must be cleared after success.
        let state = manager.state.lock().unwrap();
        let key = (root_canon, "rust".to_string());
        assert!(
            !state.broken.contains_key(&key),
            "successful respawn must clear broken record",
        );

        std::fs::remove_dir_all(&tree).ok();
    }

    #[tokio::test]
    async fn no_server_claims_extension_returns_empty() {
        let tree = cargo_tree("no-server");
        let spawner = StdArc::new(CountingSpawner::new(0));
        let manager = LspManager::new(spawner.clone(), tree.clone());
        let file = tree.join("notes.unknown");

        let clients = manager.get_clients(&file).await;
        assert!(clients.is_empty());
        assert_eq!(
            spawner.count(),
            0,
            "must not spawn for unsupported extensions"
        );

        std::fs::remove_dir_all(&tree).ok();
    }

    #[tokio::test]
    async fn touch_file_calls_notify_open_on_attached_client() {
        let tree = cargo_tree("touch");
        let spawner = StdArc::new(CountingSpawner::new(0));
        let manager = LspManager::new(spawner.clone(), tree.clone());
        let file = tree.join("src/lib.rs");

        manager.touch_file(&file, TouchMode::Notify).await;
        // The notify_open call should have ticked the file's version on the
        // underlying client.
        let entries = manager.get_clients(&file).await;
        assert_eq!(entries.len(), 1);

        std::fs::remove_dir_all(&tree).ok();
    }

    // Manager drop cascades through client guards. We can't directly assert
    // the spawned tokio task aborts, but we can assert dropping the manager
    // releases its state lock (no deadlock on shutdown).
    #[tokio::test]
    async fn manager_drop_does_not_deadlock() {
        let tree = cargo_tree("drop");
        let spawner = StdArc::new(CountingSpawner::new(0));
        let manager = LspManager::new(spawner.clone(), tree.clone());
        let file = tree.join("src/lib.rs");
        let _ = manager.get_clients(&file).await;
        drop(manager);
        // If we got here, drop completed without deadlocking on the state mutex.
        std::fs::remove_dir_all(&tree).ok();
    }
}
