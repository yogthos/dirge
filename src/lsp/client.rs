//! High-level LSP client.
//!
//! Layered on [`crate::lsp::rpc`]. Tracks per-file synchronization state
//! (versions, last text sent) and accumulates push/pull diagnostics into
//! merged, deduplicated views.
//!
//! What this module DOES NOT do:
//! - Spawn LSP server processes (that's Phase 4's orchestrator).
//! - Pick which server to attach to a file (Phase 4).
//! - Cancellation on shutdown (covered when the orchestrator owns clients).
//!
//! Construction: call [`LspClient::new`] with an already-initialized
//! [`RpcClient`]. The constructor installs the `textDocument/publishDiagnostics`
//! handler so push diagnostics are accumulated from that moment on.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lsp_types::Diagnostic;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::lsp::language;
use crate::lsp::rpc::{RpcClient, RpcError};
use crate::lsp::uri::{path_to_file_uri_string, uri_to_path};

#[derive(Debug, Default)]
struct FileState {
    /// Last version we sent to the server. LSP wants a monotonically
    /// increasing per-document version on each didChange.
    version: i32,
}

#[derive(Debug, Default)]
struct Inner {
    files: HashMap<PathBuf, FileState>,
    push: HashMap<PathBuf, Vec<Diagnostic>>,
    pull: HashMap<PathBuf, Vec<Diagnostic>>,
    last_push_at: HashMap<PathBuf, Instant>,
}

/// High-level client for a connected LSP server. Cheap to clone.
#[derive(Clone)]
pub struct LspClient {
    rpc: RpcClient,
    inner: Arc<Mutex<Inner>>,
    /// Watch channel bumped on every incoming push. Waiters subscribe and
    /// poll the inner state to detect fresh data for their file of interest.
    push_signal: watch::Sender<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out waiting for diagnostics on {path}")]
    DiagnosticsTimeout { path: PathBuf },
    #[error("server closed before diagnostics arrived")]
    ServerClosed,
}

impl LspClient {
    /// Wrap an initialized [`RpcClient`] in a higher-level LSP client. The
    /// constructor registers a `textDocument/publishDiagnostics` handler that
    /// records pushed diagnostics into the client's state.
    pub async fn new(rpc: RpcClient) -> Self {
        let (signal_tx, _signal_rx) = watch::channel(0u64);
        let inner = Arc::new(Mutex::new(Inner::default()));

        // Register the push handler. Held by the RpcClient internally; it
        // captures clones of `inner` and `signal_tx`.
        let inner_for_handler = Arc::clone(&inner);
        let signal_for_handler = signal_tx.clone();
        rpc.on_notification(
            "textDocument/publishDiagnostics",
            Box::new(move |params: Value| {
                let Some(path) = params
                    .get("uri")
                    .and_then(|v| v.as_str())
                    .and_then(uri_to_path)
                else {
                    return;
                };
                let diagnostics: Vec<Diagnostic> = params
                    .get("diagnostics")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();

                {
                    let mut state = inner_for_handler.lock().unwrap_or_else(|e| e.into_inner());
                    state.push.insert(path.clone(), diagnostics);
                    state.last_push_at.insert(path, Instant::now());
                }
                // Bump the watch to wake any waiters.
                signal_for_handler.send_modify(|v| *v = v.wrapping_add(1));
            }),
        )
        .await;

        Self {
            rpc,
            inner,
            push_signal: signal_tx,
        }
    }

    /// Open or update a file with the server. Reads the current file
    /// contents, sends `textDocument/didOpen` on first contact or
    /// `textDocument/didChange` thereafter (with a bumped version).
    /// Returns the version sent.
    ///
    /// File I/O goes through `tokio::fs` so the orchestrator can fan
    /// `touch_file` out across multiple clients without blocking the runtime
    /// thread.
    pub async fn notify_open(&self, path: &Path) -> Result<i32, LspError> {
        let abs = match tokio::fs::canonicalize(path).await {
            Ok(p) => p,
            Err(_) => path.to_path_buf(),
        };
        let text = tokio::fs::read_to_string(&abs).await?;
        let uri = path_to_file_uri_string(&abs);

        let is_first_open;
        let version;
        {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // Treat "never tracked" as the open-once case. We can't peek at
            // the entry under a separate borrow without map-key contention,
            // so probe with contains_key first.
            is_first_open = !state.files.contains_key(&abs);
            let entry = state.files.entry(abs.clone()).or_default();
            if is_first_open {
                entry.version = 0;
                version = 0;
            } else {
                entry.version += 1;
                version = entry.version;
            }
        }

        if is_first_open {
            let language_id = language::language_for_path(&abs);
            self.rpc
                .notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id,
                            "version": version,
                            "text": text,
                        }
                    }),
                )
                .await?;
        } else {
            self.rpc
                .notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "version": version,
                        },
                        // Always send full-document changes for simplicity.
                        // Per the LSP spec, full-sync (sync=1) servers expect
                        // this shape; incremental-sync (sync=2) servers also
                        // accept it. We may switch on initialize.sync later.
                        "contentChanges": [{ "text": text }],
                    }),
                )
                .await?;
        }
        Ok(version)
    }

    /// Merged + deduplicated diagnostics for a single file. Combines push
    /// (server-volunteered) and pull (server requested explicitly) state.
    pub fn diagnostics_for(&self, path: &Path) -> Vec<Diagnostic> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let push = state.push.get(path).cloned().unwrap_or_default();
        let pull = state.pull.get(path).cloned().unwrap_or_default();
        dedupe(push.into_iter().chain(pull))
    }

    /// All merged + deduplicated diagnostics across every tracked file.
    /// Empty entries are pruned. Useful for the write/edit tool's
    /// project-wide diagnostic block.
    pub fn all_diagnostics(&self) -> HashMap<PathBuf, Vec<Diagnostic>> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut paths: HashSet<PathBuf> = state.push.keys().cloned().collect();
        paths.extend(state.pull.keys().cloned());
        drop(state);

        let mut result = HashMap::new();
        for p in paths {
            let merged = self.diagnostics_for(&p);
            if !merged.is_empty() {
                result.insert(p, merged);
            }
        }
        result
    }

    /// Block until a push for `path` arrives with a timestamp strictly later
    /// than `after`, or `timeout` elapses. Stale pushes (already in state at
    /// call time) do NOT satisfy the wait — only a fresh arrival counts.
    pub async fn wait_for_push(
        &self,
        path: &Path,
        after: Instant,
        timeout: Duration,
    ) -> Result<(), LspError> {
        let deadline = Instant::now() + timeout;
        let mut rx = self.push_signal.subscribe();
        loop {
            if self.has_fresh_push(path, after) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(LspError::DiagnosticsTimeout {
                    path: path.to_path_buf(),
                });
            }
            match tokio::time::timeout(remaining, rx.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => return Err(LspError::ServerClosed),
                Err(_) => {
                    return Err(LspError::DiagnosticsTimeout {
                        path: path.to_path_buf(),
                    });
                }
            }
        }
    }

    fn has_fresh_push(&self, path: &Path, after: Instant) -> bool {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state
            .last_push_at
            .get(path)
            .map(|t| *t > after)
            .unwrap_or(false)
    }

    /// For Phase 4: handles to internals so the orchestrator can fan out
    /// requests directly to `rpc`.
    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }
}

fn dedupe<I: IntoIterator<Item = Diagnostic>>(items: I) -> Vec<Diagnostic> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for d in items {
        // Matches opencode's dedupe key: range + severity + code + source +
        // message. Including `code` is load-bearing — two clippy lints on
        // the same line with the same message but different codes (e.g.
        // `needless_clone` vs `redundant_clone`) must not collapse.
        let key = match serde_json::to_string(&(
            &d.range,
            d.severity,
            &d.code,
            d.source.as_deref(),
            &d.message,
        )) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if seen.insert(key) {
            out.push(d);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::jsonrpc::{decode_frame, encode_frame};
    use lsp_types::{DiagnosticSeverity, NumberOrString, Position, Range};
    use serde_json::json;
    use tokio::io::BufReader;
    use tokio::sync::mpsc;

    /// Build an LspClient wired to an in-memory peer. Returns the client, a
    /// JoinHandle for the reader, and the server-side channels:
    /// - `from_client`: requests/notifications the client sent to the server
    /// - `to_client_tx`: send raw JSON frames toward the client
    async fn pair() -> (
        LspClient,
        mpsc::UnboundedReceiver<Value>,
        mpsc::UnboundedSender<Value>,
    ) {
        let (client_in, server_out) = tokio::io::duplex(8192);
        let (server_in, client_out) = tokio::io::duplex(8192);
        let (client_reader, _) = tokio::io::split(client_in);
        let (_, client_writer) = tokio::io::split(client_out);
        let (server_reader, _) = tokio::io::split(server_in);
        let (_, mut server_writer) = tokio::io::split(server_out);
        let (rpc, _task) = RpcClient::new(BufReader::new(client_reader), client_writer);
        let client = LspClient::new(rpc).await;

        let (from_client_tx, from_client) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            loop {
                let frame = match decode_frame(&mut reader).await {
                    Ok(b) => b,
                    Err(_) => break,
                };
                let v: Value = serde_json::from_slice(&frame).unwrap();
                if from_client_tx.send(v).is_err() {
                    break;
                }
            }
        });

        let (to_client_tx, mut to_client_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            while let Some(msg) = to_client_rx.recv().await {
                let bytes = serde_json::to_vec(&msg).unwrap();
                if encode_frame(&mut server_writer, &bytes).await.is_err() {
                    break;
                }
            }
        });

        (client, from_client, to_client_tx)
    }

    /// Creates a tempfile named `dirge-lsp-client-test-<pid>-<nanos>-<suffix>.rs`
    /// so language detection picks up the right languageId.
    fn tempfile_with(suffix: &str, contents: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "dirge-lsp-client-test-{}-{}-{suffix}.rs",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::write(&p, contents).unwrap();
        p
    }

    fn diag(line: u32, msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 0 },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String("E0001".to_string())),
            code_description: None,
            source: Some("rustc".to_string()),
            message: msg.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    #[tokio::test]
    async fn notify_open_first_call_sends_did_open_with_version_zero() {
        let (client, mut from_client, _to) = pair().await;
        let path = tempfile_with("first-open", "fn main() {}\n");
        let v = client.notify_open(&path).await.unwrap();
        assert_eq!(v, 0);

        let frame = from_client.recv().await.unwrap();
        assert_eq!(frame["method"], "textDocument/didOpen");
        assert_eq!(frame["params"]["textDocument"]["version"], 0);
        assert_eq!(frame["params"]["textDocument"]["languageId"], "rust");
        assert_eq!(frame["params"]["textDocument"]["text"], "fn main() {}\n");
        std::fs::remove_file(&path).ok();
    }

    // Regression: subsequent notify_open calls must emit didChange (NOT
    // another didOpen) with a bumped version. Sending didOpen twice for the
    // same uri causes rust-analyzer to error out.
    #[tokio::test]
    async fn regression_subsequent_notify_open_sends_did_change_with_bumped_version() {
        let (client, mut from_client, _to) = pair().await;
        let path = tempfile_with("subsequent-open", "fn main() {}\n");
        let v0 = client.notify_open(&path).await.unwrap();
        let _first = from_client.recv().await.unwrap();

        // Mutate the file so the change is meaningful.
        std::fs::write(&path, "fn main() { let _ = 1; }\n").unwrap();
        let v1 = client.notify_open(&path).await.unwrap();
        assert_eq!(v0, 0);
        assert_eq!(v1, 1);

        let second = from_client.recv().await.unwrap();
        assert_eq!(second["method"], "textDocument/didChange");
        assert_eq!(second["params"]["textDocument"]["version"], 1);
        assert_eq!(
            second["params"]["contentChanges"][0]["text"],
            "fn main() { let _ = 1; }\n"
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn notify_open_reads_current_file_contents() {
        let (client, mut from_client, _to) = pair().await;
        let path = tempfile_with("read-contents", "hello world\n");
        client.notify_open(&path).await.unwrap();
        let frame = from_client.recv().await.unwrap();
        assert_eq!(frame["params"]["textDocument"]["text"], "hello world\n");
        std::fs::remove_file(&path).ok();
    }

    // Push diagnostic flow: server fires a textDocument/publishDiagnostics
    // notification; LspClient records it; diagnostics_for returns it.
    // The URI we receive is decoded back to the same path string we'd send,
    // so query with the same form (no canonicalize() — that could cross a
    // symlink boundary like macOS's /tmp → /private/tmp).
    #[tokio::test]
    async fn push_diagnostic_lands_in_state() {
        let (client, _from, to_client) = pair().await;
        let path = tempfile_with("push-state", "");
        let uri = path_to_file_uri_string(&path);
        let d = diag(3, "unused variable");

        to_client
            .send(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": uri,
                    "diagnostics": [d.clone()]
                }
            }))
            .unwrap();

        // Give the handler a moment to run.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let got = client.diagnostics_for(&path);
        assert_eq!(got.len(), 1, "got: {got:?}");
        assert_eq!(got[0].message, "unused variable");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn diagnostics_for_unknown_file_returns_empty() {
        let (client, _from, _to) = pair().await;
        let got = client.diagnostics_for(Path::new("/tmp/never-touched.rs"));
        assert!(got.is_empty());
    }

    // Regression: two diagnostics with identical range/severity/message/source
    // but DIFFERENT codes must NOT dedupe. The agent needs to see both lints
    // when (say) two clippy rules fire on the same expression.
    #[tokio::test]
    async fn regression_dedupe_preserves_diagnostics_with_distinct_codes() {
        let (client, _from, _to) = pair().await;
        let path = PathBuf::from("/tmp/dedupe-codes-test.rs");

        let mut a = diag(5, "this is suspicious");
        a.code = Some(NumberOrString::String("clippy::needless_clone".to_string()));
        let mut b = diag(5, "this is suspicious");
        b.code = Some(NumberOrString::String(
            "clippy::redundant_clone".to_string(),
        ));

        {
            let mut state = client.inner.lock().unwrap();
            state.push.insert(path.clone(), vec![a.clone(), b.clone()]);
        }

        let merged = client.diagnostics_for(&path);
        assert_eq!(
            merged.len(),
            2,
            "different `code` must keep both: {merged:?}"
        );
    }

    // Regression: identical diagnostics from push + pull (or from two
    // overlapping push notifications) must dedupe so the UI doesn't show
    // every error twice.
    #[tokio::test]
    async fn regression_merged_diagnostics_dedupe_identical_entries() {
        let (client, _from, _to) = pair().await;
        let path = PathBuf::from("/tmp/dedupe-test.rs");
        let same = diag(1, "same error");
        let different = diag(2, "different error");

        {
            let mut state = client.inner.lock().unwrap();
            state
                .push
                .insert(path.clone(), vec![same.clone(), different.clone()]);
            state.pull.insert(path.clone(), vec![same.clone()]);
        }

        let merged = client.diagnostics_for(&path);
        assert_eq!(merged.len(), 2, "duplicates should collapse: {merged:?}");
        // Both unique items preserved.
        assert!(merged.iter().any(|d| d.message == "same error"));
        assert!(merged.iter().any(|d| d.message == "different error"));
    }

    #[tokio::test]
    async fn all_diagnostics_aggregates_every_tracked_file() {
        let (client, _from, _to) = pair().await;
        {
            let mut state = client.inner.lock().unwrap();
            state
                .push
                .insert(PathBuf::from("/tmp/a.rs"), vec![diag(1, "a-err")]);
            state
                .pull
                .insert(PathBuf::from("/tmp/b.rs"), vec![diag(2, "b-err")]);
        }
        let all = client.all_diagnostics();
        assert_eq!(all.len(), 2);
        assert!(all.contains_key(&PathBuf::from("/tmp/a.rs")));
        assert!(all.contains_key(&PathBuf::from("/tmp/b.rs")));
    }

    #[tokio::test]
    async fn all_diagnostics_prunes_empty_entries() {
        let (client, _from, _to) = pair().await;
        {
            let mut state = client.inner.lock().unwrap();
            state.push.insert(PathBuf::from("/tmp/empty.rs"), vec![]);
        }
        assert!(client.all_diagnostics().is_empty());
    }

    // Regression: wait_for_push must resolve when a fresh push arrives, NOT
    // on a push that landed before `after`. The "freshness" timestamp is the
    // entire point of the API — without it, the agent's wait_for_diagnostics
    // call after a write would resolve immediately on the previous turn's
    // stale diagnostics.
    #[tokio::test]
    async fn regression_wait_for_push_ignores_stale_arrivals() {
        let (client, _from, to_client) = pair().await;
        let path = PathBuf::from("/tmp/wait-stale.rs");
        let uri = path_to_file_uri_string(&path);

        // Land a push BEFORE we mark `after`.
        to_client
            .send(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": { "uri": uri, "diagnostics": [diag(1, "stale")] }
            }))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let after = Instant::now();
        // Should time out — no push arrives after `after`.
        let res = client
            .wait_for_push(&path, after, Duration::from_millis(80))
            .await;
        assert!(matches!(res, Err(LspError::DiagnosticsTimeout { .. })));
    }

    #[tokio::test]
    async fn wait_for_push_resolves_on_fresh_arrival() {
        let (client, _from, to_client) = pair().await;
        let path = PathBuf::from("/tmp/wait-fresh.rs");
        let uri = path_to_file_uri_string(&path);
        let after = Instant::now();

        // Schedule a push 50ms later.
        let to_client2 = to_client.clone();
        let uri2 = uri.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            to_client2
                .send(json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": { "uri": uri2, "diagnostics": [diag(2, "fresh")] }
                }))
                .unwrap();
        });

        let res = client
            .wait_for_push(&path, after, Duration::from_secs(1))
            .await;
        assert!(res.is_ok(), "expected push to satisfy wait: {res:?}");
        let got = client.diagnostics_for(&path);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].message, "fresh");
    }

    #[tokio::test]
    async fn wait_for_push_times_out_on_no_arrival() {
        let (client, _from, _to_client) = pair().await;
        let path = PathBuf::from("/tmp/wait-never.rs");
        let after = Instant::now();
        let res = client
            .wait_for_push(&path, after, Duration::from_millis(80))
            .await;
        assert!(matches!(res, Err(LspError::DiagnosticsTimeout { .. })));
    }
}
