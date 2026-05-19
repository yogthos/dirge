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

use std::collections::{HashMap, HashSet};
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

#[derive(Default)]
struct ManagerState {
    clients: HashMap<(PathBuf, String), Arc<ClientEntry>>,
    /// (root, server_id) pairs we've given up on. Avoids hammering a broken
    /// server every time the agent touches a file.
    broken: HashSet<(PathBuf, String)>,
    /// In-flight spawns. The `Notify` is shared with every caller waiting
    /// on the same (root, server_id) so we never spawn two of the same
    /// server in parallel.
    spawning: HashMap<(PathBuf, String), Arc<Notify>>,
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
    pub fn new(spawner: Arc<dyn Spawner>, worktree: impl Into<PathBuf>) -> Self {
        Self {
            spawner,
            worktree: worktree.into(),
            state: Arc::new(Mutex::new(ManagerState::default())),
            servers: Arc::new(server::builtin_servers()),
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
                if state.broken.contains(&key) {
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
        // If another caller is already spawning this key, wait for them
        // instead of racing.
        let wait_for: Option<Arc<Notify>> = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // Re-check cache under the lock — could have landed between
            // the fast-path miss and now.
            if let Some(entry) = state.clients.get(key) {
                return Some(Arc::clone(entry));
            }
            if state.broken.contains(key) {
                return None;
            }
            if let Some(notify) = state.spawning.get(key) {
                Some(Arc::clone(notify))
            } else {
                // We're the spawner. Mark in-flight.
                let notify = Arc::new(Notify::new());
                state.spawning.insert(key.clone(), Arc::clone(&notify));
                None
            }
        };

        if let Some(notify) = wait_for {
            // Safety timeout in case we subscribed after `notify_waiters` fired
            // (Notify doesn't save permits for late subscribers). The cache is
            // authoritative either way — re-check after the wait, regardless
            // of how it terminated.
            let _ = tokio::time::timeout(Duration::from_secs(60), notify.notified()).await;
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = state.clients.get(key) {
                return Some(Arc::clone(entry));
            }
            return None;
        }

        // We're responsible for the spawn.
        let result = self.do_spawn(server, root).await;

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
                    SpawnOutcome::Inserted {
                        arc,
                        notify,
                        slot_missing,
                    }
                }
                Err(e) => {
                    state.broken.insert(key.clone());
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
                if let Err(e) = entry.client.wait_for_push(path, after, timeout).await {
                    tracing::debug!(server = %entry.server_id, path = %path.display(), "wait_for_push: {e}");
                }
            }
        }
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
                    tracing::debug!(server = %entry.server_id, method = %method, "request failed: {e}");
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

impl ClientEntry {
    pub fn server_id(&self) -> &str {
        &self.server_id
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
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
        fail_keys: std::sync::Mutex<HashSet<(String, PathBuf)>>,
        /// Delay each spawn by this much so concurrent calls have a chance
        /// to collide on the dedupe path.
        delay_ms: u64,
    }

    impl CountingSpawner {
        fn new(delay_ms: u64) -> Self {
            Self {
                spawn_calls: StdArc::new(AtomicUsize::new(0)),
                fail_keys: std::sync::Mutex::new(HashSet::new()),
                delay_ms,
            }
        }
        fn count(&self) -> usize {
            self.spawn_calls.load(Ordering::SeqCst)
        }
        fn fail_for(&self, server_id: &str, root: &Path) {
            self.fail_keys
                .lock()
                .unwrap()
                .insert((server_id.to_string(), root.to_path_buf()));
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
                if self.delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
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
