use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use rmcp::service::{Peer, RoleClient, RunningService, serve_client};
use tokio::process::{ChildStderr, Command};
use tokio::sync::{Mutex, RwLock};

use super::config::McpServerConfig;

/// Co-owned (peer, running_service) pair for one MCP server.
///
/// Every `McpTool` from the same server holds the same
/// `Arc<SharedConnection>`. On reconnect — manager-side OR
/// tool-side — `replace` atomically swaps in a fresh peer +
/// running_service. The OLD `RunningService` drops at the end of
/// the swap, which cancels its cancellation_token, closes the
/// transport, and (for child-process transports) kills the dead
/// child. This was the M-R1 review finding: the prior code did
/// `mem::forget(RunningService)` which leaked the spawned process.
///
/// Lock order to avoid deadlock: always take `running_service`
/// before `peer`. Readers take a single lock at a time.
pub struct SharedConnection {
    /// Kept for debugging / tracing — every error path logs the
    /// server name, so the structured field stays for log
    /// correlation even when no code reads it directly.
    #[allow(dead_code)]
    pub server_name: String,
    peer: RwLock<Peer<RoleClient>>,
    /// `Option` so the consuming-`Drop` of `RunningService::cancel`
    /// can take it out cleanly during shutdown. `None` after the
    /// connection has been explicitly shut down.
    running_service: Mutex<Option<RunningService<RoleClient, ()>>>,
}

impl SharedConnection {
    /// Wrap a freshly-built (peer, running_service) pair. Pub-crate so
    /// the manager's reconnect path can create a SharedConnection for
    /// servers that failed initial connect but succeed later.
    pub(crate) fn new(
        server_name: String,
        peer: Peer<RoleClient>,
        rs: RunningService<RoleClient, ()>,
    ) -> Self {
        Self {
            server_name,
            peer: RwLock::new(peer),
            running_service: Mutex::new(Some(rs)),
        }
    }

    /// Snapshot the current peer. Cheap — `Peer` is an `mpsc::Sender`
    /// wrapper, cloning bumps a refcount.
    pub async fn current_peer(&self) -> Peer<RoleClient> {
        self.peer.read().await.clone()
    }

    /// Atomically swap in a fresh (peer, running_service). The OLD
    /// `RunningService` drops as `_old` falls out of scope, cancelling
    /// its background task + closing its transport.
    pub async fn replace(
        &self,
        new_peer: Peer<RoleClient>,
        new_rs: RunningService<RoleClient, ()>,
    ) {
        // Order: take running_service first (Option swap), then peer
        // (RwLock write). Both consumed before the OLD running_service
        // is dropped so the new one is fully wired before the cleanup
        // signal fires on the old transport.
        let _old = {
            let mut rs_guard = self.running_service.lock().await;
            (*rs_guard).replace(new_rs)
        };
        *self.peer.write().await = new_peer;
        // `_old` drops here. If it was `Some`, that `RunningService`'s
        // `DropGuard` cancels its cancellation_token; the background
        // task observes the cancel + closes the transport; the
        // TokioChildProcess transport's drop reaps the child.
    }

    /// Explicit shutdown — drops the running service synchronously
    /// (via `Drop`'s async-cancellation guard) and renders the
    /// connection dead. Called by `McpClientManager::shutdown`.
    pub async fn shutdown(&self) {
        let mut rs = self.running_service.lock().await;
        rs.take(); // dropping the Some(...) here triggers cleanup
    }
}

/// Upper bound on how long we'll wait for an MCP server to complete
/// initialization. Command-based servers that hang on `initialize`
/// (e.g. waiting for stdin that never comes) would otherwise pin
/// startup indefinitely. 10s is generous for legitimate inits — npm
/// install-on-first-run servers take a few seconds; locally-running
/// binaries respond in <100ms. Past the cap we abort and log. The value
/// is the resolved `[timeouts].mcp_init_secs` (dirge-onlr/4xgd).
///
/// Connect to one MCP server and wrap the connection in a
/// shared, swappable container. Returns the `Arc<SharedConnection>`
/// the manager + every McpTool clone holds.
pub async fn connect(
    server_name: String,
    config: &McpServerConfig,
) -> anyhow::Result<Arc<SharedConnection>> {
    let init_timeout = crate::timeout::Timeouts::get().mcp_init;
    let inner = connect_inner(server_name.clone(), config);
    match tokio::time::timeout(init_timeout, inner).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "MCP server {server_name:?} did not initialize within {}s — skipping",
            init_timeout.as_secs(),
        )),
    }
}

async fn connect_inner(
    server_name: String,
    config: &McpServerConfig,
) -> anyhow::Result<Arc<SharedConnection>> {
    let (peer, rs) = raw_connect(&server_name, config).await?;
    Ok(Arc::new(SharedConnection::new(server_name, peer, rs)))
}

/// Build a new `RunningService` + extract its peer, without wrapping
/// in `SharedConnection`. Used by `SharedConnection::replace` callers
/// (manager + tool-side auto-reconnect) which already own the
/// container they want to swap into.
///
/// Does NOT wrap in `MCP_INIT_TIMEOUT` — the caller times out the
/// whole reconnect operation.
pub async fn raw_connect(
    server_name: &str,
    config: &McpServerConfig,
) -> anyhow::Result<(Peer<RoleClient>, RunningService<RoleClient, ()>)> {
    match config {
        McpServerConfig::Command {
            command,
            args,
            env,
            allow_external_paths: _,
        } => {
            let mut cmd = Command::new(command);
            cmd.args(args);
            for (k, v) in env {
                cmd.env(k, v);
            }
            // CRITICAL: capture stderr instead of inheriting it.
            // See lengthy explanation below at the original
            // call site — terminal-query bytes from the child must
            // not reach dirge's stdin via the controlling TTY.
            let (transport, stderr) =
                rmcp::transport::child_process::TokioChildProcess::builder(cmd)
                    .stderr(Stdio::piped())
                    .spawn()?;
            if let Some(child_stderr) = stderr {
                spawn_stderr_forwarder(server_name.to_string(), child_stderr);
            }
            let rs = serve_client((), transport)
                .await
                .map_err(|e| anyhow::anyhow!("MCP connection failed for '{server_name}': {e}"))?;
            let peer = rs.peer().clone();
            Ok((peer, rs))
        }
        McpServerConfig::Url {
            url,
            headers,
            allow_external_paths: _,
        } => {
            let custom_headers = parse_headers(headers)?;
            let cfg = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.as_str())
                .custom_headers(custom_headers);
            type HttpClient = rmcp::transport::StreamableHttpClientTransport<reqwest::Client>;
            let transport = HttpClient::from_config(cfg);
            let rs = serve_client((), transport).await.map_err(|e| {
                anyhow::anyhow!("MCP HTTP connection failed for '{server_name}': {e}")
            })?;
            let peer = rs.peer().clone();
            Ok((peer, rs))
        }
    }
}

/// List the tools the server advertises. Called once at startup
/// (or after manual reconnect) to build the agent's tool registry.
pub async fn list_tools(
    conn: &SharedConnection,
) -> Result<Vec<rmcp::model::Tool>, rmcp::ServiceError> {
    let peer = conn.current_peer().await;
    peer.list_all_tools().await
}

/// Forward an MCP child's stderr line-by-line to dirge's tracing
/// `info` channel (and ultimately to dirge's own stderr, which the
/// user has been seeing under the `[Lattice]` / `[Chiasmus]` etc.
/// prefixes). Strips any control bytes the child emits so its
/// stderr can't paint colors / move the cursor / send queries on
/// the way through. Bytes are forwarded as plain text, prefixed
/// with `[mcp:<server_name>]` so multiple servers are
/// distinguishable.
///
/// The task exits when the child closes stderr (process termination
/// or stream EOF). No explicit cancel — the rmcp ChildWithCleanup
/// Drop kills the child on shutdown, which closes stderr.
/// Per-line byte cap. A buggy / runaway MCP child that writes gigabytes
/// without a newline would otherwise grow dirge's read buffer until OOM.
/// 16 KiB is generous for any real log line; past it we truncate, emit a
/// single marker, and drain the rest of that line. (#5 fix.)
const MAX_STDERR_LINE_BYTES: usize = 16 * 1024;

/// Byte-at-a-time splitter for MCP child stderr. Accumulates a line until
/// `\n`, capping at `max_line_bytes`. dirge-yb0a: once a line overflows the
/// cap it emits ONE `...[truncated]` marker and then DRAINS (drops) the
/// remaining bytes until the next newline — previously the truncation
/// branch cleared the buffer but fell through to re-accumulate the SAME
/// logical line, emitting a fresh `...[truncated]` chunk every 16 KiB.
struct StderrLineSplitter {
    buf: Vec<u8>,
    draining: bool,
    max_line_bytes: usize,
}

impl StderrLineSplitter {
    fn new(max_line_bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(1024),
            draining: false,
            max_line_bytes,
        }
    }

    /// Feed one byte; returns a completed line ready to emit, if any.
    fn push(&mut self, b: u8) -> Option<Vec<u8>> {
        if b == b'\n' {
            // End of line: emit the accumulated buffer unless we were
            // draining an over-length line (whose marker already went out).
            let line = if self.draining {
                None
            } else {
                Some(std::mem::take(&mut self.buf))
            };
            self.buf.clear();
            self.draining = false;
            return line;
        }
        if self.draining {
            return None; // dropping the remainder of an over-length line
        }
        if self.buf.len() >= self.max_line_bytes {
            self.buf.extend_from_slice(b" ...[truncated]");
            self.draining = true;
            return Some(std::mem::take(&mut self.buf));
        }
        if self.buf.is_empty() && b == b'\r' {
            return None; // strip leading CR (CRLF from a windows-y child)
        }
        self.buf.push(b);
        None
    }

    /// EOF: return any pending partial line (none if mid-drain).
    fn finish(self) -> Option<Vec<u8>> {
        if self.buf.is_empty() {
            None
        } else {
            Some(self.buf)
        }
    }
}

fn spawn_stderr_forwarder(server_name: String, stderr: ChildStderr) {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut splitter = StderrLineSplitter::new(MAX_STDERR_LINE_BYTES);
        let mut byte_buf = [0u8; 4096];
        loop {
            let n = match reader.read(&mut byte_buf).await {
                Ok(0) => break, // EOF
                Ok(n) => n,
                Err(_) => break,
            };
            for &b in &byte_buf[..n] {
                if let Some(line) = splitter.push(b) {
                    emit_mcp_line(&server_name, &line);
                }
            }
        }
        // Flush any pending partial line on EOF.
        if let Some(line) = splitter.finish() {
            emit_mcp_line(&server_name, &line);
        }
    });
}

/// Sanitize and emit one MCP child stderr line through the UI's
/// off-stream notification channel.
///
/// Filter blocks:
///   - C0 controls except `\t` (0x00..=0x1F minus 0x09)
///   - DEL (0x7F)
///   - C1 controls (U+0080..=U+009F) — U+009B is single-byte CSI
///     on terminals in 8-bit mode and behaves identically to
///     `ESC[`, so leaving it through would defeat the sanitizer.
///     Also blocks NEL (U+0085), DCS (U+0090), etc.
///   - Trailing `\r` from CRLF children
///
/// Routes through `ui::notifications::notify_mcp_log` rather than
/// `tracing::warn!` or direct stderr writes — the UI event loop
/// drains the channel and renders via the standard
/// `Renderer::write_line` pipeline. Without this, MCP server logs
/// painted directly on top of the alt-screen UI from raw stderr
/// (e.g. `[Lattice] session closed` overlapping a chamber).
fn emit_mcp_line(server_name: &str, raw: &[u8]) {
    let s = String::from_utf8_lossy(raw);
    // Centralised sanitizer (`ui::ansi`) so MCP / websearch /
    // chat consumers share one definition of "what's a control
    // byte". Block ALL controls — MCP log lines are emitted
    // one-per-row by the UI, so embedded newlines/tabs from a
    // child would split into multiple notifications and rendering
    // becomes inconsistent. Newlines are handled by our
    // `read` loop seeing them as line delimiters; tabs become
    // spaces upstream.
    let sanitized = crate::ui::ansi::strip_controls(&s, crate::ui::ansi::StripPolicy::STRICT);
    // Review #9: previously used `trim().is_empty()` which also
    // dropped legitimate whitespace-only lines (e.g. servers that
    // emit indented continuation lines). Now drop only truly
    // empty post-sanitize lines — `\n`-between-log-groups still
    // collapses since the read loop sees the empty buf and emits
    // nothing.
    if sanitized.is_empty() {
        return;
    }
    crate::ui::notifications::notify_mcp_log(server_name, &sanitized);
}

fn parse_headers(
    headers: &HashMap<String, String>,
) -> anyhow::Result<HashMap<http::HeaderName, http::HeaderValue>> {
    let mut result = HashMap::new();
    for (name, value) in headers {
        let h_name: http::HeaderName = name
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid header name '{name}': {e}"))?;
        let h_value: http::HeaderValue = value
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid header value for '{name}': {e}"))?;
        result.insert(h_name, h_value);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the splitter over a whole byte stream, collecting the lines it
    /// would emit (as UTF-8-lossy strings for readable assertions).
    fn split(data: &[u8], cap: usize) -> Vec<String> {
        let mut s = StderrLineSplitter::new(cap);
        let mut out = Vec::new();
        for &b in data {
            if let Some(line) = s.push(b) {
                out.push(String::from_utf8_lossy(&line).into_owned());
            }
        }
        if let Some(line) = s.finish() {
            out.push(String::from_utf8_lossy(&line).into_owned());
        }
        out
    }

    #[test]
    fn normal_lines_split_on_newline() {
        assert_eq!(split(b"alpha\nbeta\n", 16), vec!["alpha", "beta"]);
    }

    #[test]
    fn trailing_partial_line_flushed_on_eof() {
        assert_eq!(split(b"alpha\npartial", 16), vec!["alpha", "partial"]);
    }

    #[test]
    fn leading_cr_stripped() {
        // The splitter strips only LEADING CR; a trailing CR before the
        // newline is left for `emit_mcp_line`'s sanitizer to remove.
        assert_eq!(split(b"\rhi\n", 16), vec!["hi"]);
    }

    /// dirge-yb0a: ONE runaway line past the cap emits exactly ONE
    /// `...[truncated]` marker, then drains until the next newline — it does
    /// NOT emit a fresh truncated chunk every `cap` bytes.
    #[test]
    fn overlong_line_truncates_once_then_drains() {
        let mut data = vec![b'x'; 100]; // 100 'x' with cap 8 → would be 12+ chunks if buggy
        data.push(b'\n');
        let lines = split(&data, 8);
        assert_eq!(lines.len(), 1, "exactly one emitted line, got {lines:?}");
        assert!(lines[0].starts_with("xxxxxxxx"));
        assert!(lines[0].ends_with("...[truncated]"));
        assert_eq!(lines[0].matches("[truncated]").count(), 1);
    }

    /// After draining an over-length line, the FOLLOWING line is emitted
    /// cleanly — the overflow does not roll into it.
    #[test]
    fn line_after_overlong_is_clean() {
        let mut data = vec![b'x'; 50];
        data.push(b'\n');
        data.extend_from_slice(b"ok\n"); // shorter than the cap
        let lines = split(&data, 8);
        assert_eq!(lines.len(), 2, "got {lines:?}");
        assert!(lines[0].ends_with("...[truncated]"));
        assert_eq!(lines[1], "ok");
    }

    /// An over-length line that never sees a newline before EOF emits its
    /// single truncation marker and nothing more (no partial-flush dup).
    #[test]
    fn overlong_line_at_eof_emits_once() {
        let data = vec![b'x'; 100];
        let lines = split(&data, 8);
        assert_eq!(lines.len(), 1, "got {lines:?}");
        assert!(lines[0].ends_with("...[truncated]"));
    }
}
