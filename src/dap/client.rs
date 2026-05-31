//! DAP client transport.
//!
//! Spawns a debug adapter process and communicates via Content-Length framed
//! DAP messages over stdio. Request/response matching is done by `seq` /
//! `request_seq` correlation — DAP uses its own message envelope, not
//! JSON-RPC 2.0.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::dap::framing::{decode_frame, encode_frame};
use crate::dap::types::Capabilities;

// ---------------------------------------------------------------------------
// DAP RPC — lightweight seq→request_seq correlation
// ---------------------------------------------------------------------------

/// Errors surfaced to callers of [`DapRpc::request`].
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("adapter error: {0}")]
    Server(String),
    #[error("connection closed before response arrived")]
    ConnectionClosed,
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>;

/// Notification handler: `Fn(event_body: Value)` called for each incoming
/// event matching a registered `event_type`.
pub type NotificationHandler = Box<dyn Fn(Value) + Send + Sync>;

struct Inner {
    next_seq: AtomicU64,
    pending: Mutex<Pending>,
    handlers: Mutex<HashMap<String, NotificationHandler>>,
    writer: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    closed: std::sync::atomic::AtomicBool,
}

/// Cheaply cloneable handle for issuing DAP requests and registering
/// notification handlers.
#[derive(Clone)]
pub struct DapRpc {
    inner: Arc<Inner>,
}

impl DapRpc {
    pub fn new<R, W>(reader: R, writer: W) -> (Self, JoinHandle<io::Result<()>>)
    where
        R: AsyncBufRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let inner = Arc::new(Inner {
            next_seq: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            handlers: Mutex::new(HashMap::new()),
            writer: Mutex::new(Box::new(writer)),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        let rpc = DapRpc {
            inner: inner.clone(),
        };
        let task = tokio::spawn(read_loop(inner, reader));
        (rpc, task)
    }

    /// Send a DAP request and await its response.
    pub async fn request<P, R>(
        &self,
        command: &str,
        arguments: P,
        request_timeout: Duration,
    ) -> Result<R, RpcError>
    where
        P: Serialize,
        R: serde::de::DeserializeOwned,
    {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(RpcError::ConnectionClosed);
        }
        let seq = self.inner.next_seq.fetch_add(1, Ordering::SeqCst);

        let body = serde_json::json!({
            "type": "request",
            "seq": seq,
            "command": command,
            "arguments": serde_json::to_value(arguments)?,
        });
        let bytes = serde_json::to_vec(&body)?;

        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(seq, tx);

        let send_result = {
            let mut writer = self.inner.writer.lock().await;
            encode_frame(&mut *writer, &bytes).await
        };
        if let Err(e) = send_result {
            self.inner.pending.lock().await.remove(&seq);
            return Err(RpcError::Io(e));
        }

        let value = match timeout(request_timeout, rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => {
                self.inner.pending.lock().await.remove(&seq);
                return Err(RpcError::ConnectionClosed);
            }
            Err(_) => {
                self.inner.pending.lock().await.remove(&seq);
                return Err(RpcError::Timeout(request_timeout));
            }
        };
        Ok(serde_json::from_value(value)?)
    }

    /// Fire-and-forget notification (DAP request without a response).
    pub async fn notify<P>(&self, command: &str, arguments: P) -> Result<(), RpcError>
    where
        P: Serialize,
    {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(RpcError::ConnectionClosed);
        }
        let seq = self.inner.next_seq.fetch_add(1, Ordering::SeqCst);
        let body = serde_json::json!({
            "type": "request",
            "seq": seq,
            "command": command,
            "arguments": serde_json::to_value(arguments)?,
        });
        let bytes = serde_json::to_vec(&body)?;
        let mut writer = self.inner.writer.lock().await;
        encode_frame(&mut *writer, &bytes).await?;
        Ok(())
    }

    /// Register a handler for an incoming DAP event (e.g. "stopped", "output").
    pub async fn on_event(&self, event_type: &str, handler: NotificationHandler) {
        self.inner
            .handlers
            .lock()
            .await
            .insert(event_type.to_string(), handler);
    }
}

async fn read_loop<R>(inner: Arc<Inner>, mut reader: R) -> io::Result<()>
where
    R: AsyncBufRead + Send + Unpin,
{
    loop {
        let frame = match decode_frame(&mut reader).await {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        };
        let msg: Value = match serde_json::from_slice(&frame) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("dap: skipping non-JSON frame: {e}");
                continue;
            }
        };
        dispatch(&inner, msg).await;
    }
    inner.closed.store(true, Ordering::SeqCst);
    let mut pending = inner.pending.lock().await;
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(RpcError::ConnectionClosed));
    }
    Ok(())
}

async fn dispatch(inner: &Arc<Inner>, msg: Value) {
    let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "response" => {
            let request_seq = msg.get("request_seq").and_then(|v| v.as_u64());
            if let Some(seq) = request_seq {
                let sender = inner.pending.lock().await.remove(&seq);
                if let Some(sender) = sender {
                    let result = if msg.get("success").and_then(|v| v.as_bool()) == Some(true) {
                        Ok(msg.get("body").cloned().unwrap_or(Value::Null))
                    } else {
                        let message = msg
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(no message)")
                            .to_string();
                        Err(RpcError::Server(message))
                    };
                    let _ = sender.send(result);
                }
            }
        }
        "event" => {
            let event_type = msg.get("event").and_then(|v| v.as_str()).unwrap_or("");
            let handlers = inner.handlers.lock().await;
            if let Some(handler) = handlers.get(event_type) {
                let body = msg.get("body").cloned().unwrap_or(Value::Null);
                handler(body);
            }
        }
        _ => {
            tracing::warn!("dap: unexpected message type {msg_type:?}, ignoring");
        }
    }
}

// ---------------------------------------------------------------------------
// DAP client — wraps DapRpc with process lifecycle
// ---------------------------------------------------------------------------

/// Handle to a running debug adapter process.
pub struct DapClient {
    /// Held so `kill_on_drop` works when the client goes out of scope.
    /// `None` only in tests (where RPC runs over duplex channels).
    _child: Option<tokio::process::Child>,
    pub(crate) rpc: DapRpc,
    /// Task draining adapter stderr to tracing.
    _stderr_task: JoinHandle<()>,
    pub capabilities: Mutex<Option<Capabilities>>,
    pub adapter_name: String,
}

impl DapClient {
    /// Spawn the adapter process given a command + args, wire up stdio, and
    /// return a [`DapClient`] ready for the initialization handshake.
    pub async fn spawn_stdio(
        adapter_name: &str,
        program: &Path,
        args: &[String],
        cwd: &Path,
    ) -> io::Result<Self> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("adapter stdin pipe unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("adapter stdout pipe unavailable"))?;

        // Drain stderr in the background.
        let stderr_task = if let Some(stderr) = child.stderr.take() {
            let name = adapter_name.to_string();
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stderr);
                let mut buf = String::new();
                loop {
                    buf.clear();
                    match reader.read_line(&mut buf).await {
                        Ok(0) => break,
                        Ok(_) => tracing::debug!(adapter = %name, "{}", buf.trim_end()),
                        Err(_) => break,
                    }
                }
            })
        } else {
            tokio::spawn(std::future::ready(()))
        };

        let reader = tokio::io::BufReader::new(stdout);
        let (rpc, _read_task) = DapRpc::new(reader, stdin);

        Ok(Self {
            _child: Some(child),
            rpc,
            _stderr_task: stderr_task,
            capabilities: Mutex::new(None),
            adapter_name: adapter_name.to_string(),
        })
    }

    /// Send a DAP request and await its response.
    pub async fn request<P, R>(
        &self,
        command: &str,
        arguments: P,
        timeout_dur: Duration,
    ) -> Result<R, RpcError>
    where
        P: Serialize,
        R: serde::de::DeserializeOwned,
    {
        self.rpc.request(command, arguments, timeout_dur).await
    }

    /// Send a fire-and-forget notification.
    pub async fn notify<P>(&self, command: &str, arguments: P) -> Result<(), RpcError>
    where
        P: Serialize,
    {
        self.rpc.notify(command, arguments).await
    }

    /// Register a handler for an incoming DAP event.
    pub async fn on_event(&self, event_type: &str, handler: NotificationHandler) {
        self.rpc.on_event(event_type, handler).await
    }

    /// Test-only: construct a DapClient pre-wired to a DapRpc without
    /// spawning a real process. The adapter runs over duplex channels.
    #[cfg(test)]
    pub(crate) fn from_rpc(rpc: DapRpc, adapter_name: &str) -> Self {
        Self {
            _child: None,
            rpc,
            _stderr_task: tokio::spawn(std::future::ready(())),
            capabilities: Mutex::new(None),
            adapter_name: adapter_name.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake DAP adapter that:
    /// 1. Reads a single initialize request
    /// 2. Sends back a capabilities response
    /// 3. Then sends a "stopped" event after a small delay
    async fn fake_adapter(
        client_reader: &mut (impl AsyncBufRead + Unpin),
        client_writer: &mut (impl AsyncWrite + Unpin),
    ) {
        // Read the initialize request.
        let frame = decode_frame(client_reader).await.unwrap();
        let msg: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(msg["type"], "request");
        let seq = msg["seq"].as_u64().unwrap();

        // Send initialize response.
        let resp = serde_json::json!({
            "type": "response",
            "seq": 1,
            "request_seq": seq,
            "success": true,
            "command": "initialize",
            "body": {
                "supportsConfigurationDoneRequest": true,
                "supportsFunctionBreakpoints": false,
            }
        });
        encode_frame(client_writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        // Send a stopped event.
        let evt = serde_json::json!({
            "type": "event",
            "seq": 2,
            "event": "stopped",
            "body": {
                "reason": "entry",
                "threadId": 1,
            }
        });
        encode_frame(client_writer, &serde_json::to_vec(&evt).unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn request_response_roundtrip() {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, mut server_write) = tokio::io::split(server_side);

        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _task) = DapRpc::new(client_reader, client_write);

        let adapter = tokio::spawn(async move {
            fake_adapter(
                &mut tokio::io::BufReader::new(server_read),
                &mut server_write,
            )
            .await;
        });

        let response: Value = rpc
            .request(
                "initialize",
                serde_json::json!({ "adapterID": "test" }),
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(response["supportsConfigurationDoneRequest"], true);
        assert_eq!(response["supportsFunctionBreakpoints"], false);
        adapter.await.unwrap();
    }

    #[tokio::test]
    async fn event_handler_receives_stopped() {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, mut server_write) = tokio::io::split(server_side);

        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _task) = DapRpc::new(client_reader, client_write);

        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel();
        rpc.on_event(
            "stopped",
            Box::new(move |body| {
                let _ = evt_tx.send(body);
            }),
        )
        .await;

        let adapter = tokio::spawn(async move {
            fake_adapter(
                &mut tokio::io::BufReader::new(server_read),
                &mut server_write,
            )
            .await;
        });

        // This will also consume the initialize request (which the fake expects)
        let _: Value = rpc
            .request("initialize", serde_json::json!({}), Duration::from_secs(5))
            .await
            .unwrap();

        let event_body = evt_rx.recv().await.unwrap();
        assert_eq!(event_body["reason"], "entry");
        assert_eq!(event_body["threadId"], 1);
        adapter.await.unwrap();
    }

    #[tokio::test]
    async fn request_timeout() {
        let (client_side, _server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, _task) = DapRpc::new(client_reader, client_write);

        // No adapter on the other end — should time out.
        let err = rpc
            .request::<_, Value>("launch", serde_json::json!({}), Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Timeout(_)));
    }

    #[tokio::test]
    async fn connection_closed_on_eof() {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_side);
        let client_reader = tokio::io::BufReader::new(client_read);
        let (rpc, task) = DapRpc::new(client_reader, client_write);

        // Drop the server side immediately → EOF.
        drop(server_side);

        // The read loop will shut down. Wait for it.
        let _ = task.await;

        let err = rpc
            .request::<_, Value>("launch", serde_json::json!({}), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::ConnectionClosed));
    }

    // -------------------------------------------------------------------
    // Integration: full lifecycle against mock Python adapter
    // -------------------------------------------------------------------

    /// Path to the mock adapter, relative to the repo root.
    fn mock_adapter_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("mock_dap_adapter.py")
    }

    /// End-to-end test: spawn the mock adapter process, run the full
    /// initialize→launch→setBreakpoints→configurationDone→threads→
    /// stackTrace→scopes→variables→evaluate→continue→terminate→disconnect
    /// lifecycle.
    #[tokio::test]
    async fn full_lifecycle_against_mock_adapter() {
        use crate::dap::types::{
            Capabilities, ContinueResponse, EvaluateResponse, ScopesResponse,
            SetBreakpointsResponse, StackTraceResponse, ThreadsResponse, VariablesResponse,
        };

        let program = mock_adapter_path();
        assert!(
            program.exists(),
            "mock adapter must exist at {}",
            program.display()
        );

        let client = super::DapClient::spawn_stdio(
            "mock",
            std::path::Path::new("python3"),
            &[program.to_string_lossy().to_string()],
            std::path::Path::new("."),
        )
        .await
        .expect("mock adapter should spawn");

        // 1. initialize
        let caps: Capabilities = client
            .request(
                "initialize",
                serde_json::json!({
                    "adapterID": "mock",
                    "clientID": "dirge-test",
                    "linesStartAt1": true,
                    "columnsStartAt1": true,
                    "pathFormat": "path"
                }),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("initialize should succeed");
        assert!(
            caps.supports_configuration_done_request.unwrap_or(false),
            "mock adapter should support configurationDoneRequest"
        );

        // 2. launch
        client
            .request::<_, Value>(
                "launch",
                serde_json::json!({"program": "/tmp/test.py", "stopOnEntry": true}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("launch should succeed");

        // 3. setBreakpoints
        let bp_response: SetBreakpointsResponse = client
            .request(
                "setBreakpoints",
                serde_json::json!({
                    "source": {"path": "/tmp/test.py"},
                    "breakpoints": [{"line": 10}]
                }),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("setBreakpoints should succeed");
        assert_eq!(bp_response.breakpoints.len(), 1);
        assert!(bp_response.breakpoints[0].verified);

        // 4. configurationDone
        client
            .request::<_, Value>(
                "configurationDone",
                serde_json::json!({}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("configurationDone should succeed");

        // 5. threads
        let threads_response: ThreadsResponse = client
            .request(
                "threads",
                serde_json::json!({}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("threads should succeed");
        assert!(!threads_response.threads.is_empty());
        let thread_id = threads_response.threads[0].id;

        // 6. stackTrace
        let st_response: StackTraceResponse = client
            .request(
                "stackTrace",
                serde_json::json!({"threadId": thread_id, "levels": 10}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("stackTrace should succeed");
        assert!(!st_response.stack_frames.is_empty());
        let frame_id = st_response.stack_frames[0].id;

        // 7. scopes
        let scopes_response: ScopesResponse = client
            .request(
                "scopes",
                serde_json::json!({"frameId": frame_id}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("scopes should succeed");
        assert!(!scopes_response.scopes.is_empty());
        let variables_ref = scopes_response.scopes[0].variables_reference;

        // 8. variables
        let vars_response: VariablesResponse = client
            .request(
                "variables",
                serde_json::json!({"variablesReference": variables_ref}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("variables should succeed");
        assert!(!vars_response.variables.is_empty());

        // 9. evaluate
        let eval_response: EvaluateResponse = client
            .request(
                "evaluate",
                serde_json::json!({"expression": "1 + 1", "frameId": frame_id, "context": "repl"}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("evaluate should succeed");
        assert_eq!(eval_response.result, "2");

        // 10. continue
        let continue_response: ContinueResponse = client
            .request(
                "continue",
                serde_json::json!({"threadId": thread_id}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("continue should succeed");
        assert!(continue_response.all_threads_continued.unwrap_or(true));

        // 11. terminate
        client
            .request::<_, Value>(
                "terminate",
                serde_json::json!({}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("terminate should succeed");

        // 12. disconnect
        client
            .request::<_, Value>(
                "disconnect",
                serde_json::json!({"terminateDebuggee": true}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("disconnect should succeed");
    }

    // -------------------------------------------------------------------
    // Per-adapter smoke: each test spawns a real adapter, runs
    // initialize→launch→terminate→disconnect lifecycle with a
    // minimal program. Skips gracefully if the adapter binary is
    // missing from $PATH.
    // -------------------------------------------------------------------

    const SMOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    /// Skip the current test with a clear message.
    macro_rules! skip_if_missing {
        ($which:expr, $adapter:expr) => {
            if which::which($which).is_err() {
                eprintln!(
                    "SKIP: {} smoke test — {} not found on PATH",
                    $adapter, $which
                );
                return;
            }
        };
        ($which:expr, $adapter:expr, $detail:expr) => {
            if which::which($which).is_err() {
                eprintln!(
                    "SKIP: {} smoke test — {} not found on PATH ({})",
                    $adapter, $which, $detail
                );
                return;
            }
        };
    }

    /// Python smoke: spawn debugpy, verify initialize handshake succeeds.
    #[tokio::test]
    async fn smoke_debugpy_python() {
        skip_if_missing!("python3", "debugpy");

        let check = std::process::Command::new("python3")
            .args(["-c", "import debugpy"])
            .output();
        if check.map_or(true, |o| !o.status.success()) {
            eprintln!(
                "SKIP: debugpy smoke test — debugpy module not installed (pip install debugpy)"
            );
            return;
        }

        let client = super::DapClient::spawn_stdio(
            "debugpy",
            std::path::Path::new("python3"),
            &["-m".to_string(), "debugpy.adapter".to_string()],
            std::path::Path::new("."),
        )
        .await
        .expect("debugpy adapter should spawn");

        let caps: Capabilities = client
            .request(
                "initialize",
                serde_json::json!({
                    "adapterID": "debugpy",
                    "clientID": "dirge-smoke",
                    "linesStartAt1": true,
                    "columnsStartAt1": true,
                    "pathFormat": "path",
                    "locale": "en-us"
                }),
                SMOKE_TIMEOUT,
            )
            .await
            .expect("debugpy initialize should succeed");
        assert!(
            caps.supports_configuration_done_request.unwrap_or(false),
            "debugpy should support configurationDoneRequest"
        );

        // Clean shutdown.
        let _ = client
            .request::<_, Value>(
                "disconnect",
                serde_json::json!({"terminateDebuggee": false}),
                SMOKE_TIMEOUT,
            )
            .await;
    }

    /// C smoke via lldb-dap: verify initialize handshake succeeds.
    #[tokio::test]
    async fn smoke_lldb_dap_c() {
        skip_if_missing!("lldb-dap", "lldb-dap");

        let client = super::DapClient::spawn_stdio(
            "lldb-dap",
            std::path::Path::new("lldb-dap"),
            &[],
            std::path::Path::new("."),
        )
        .await
        .expect("lldb-dap adapter should spawn");

        let caps: Capabilities = client
            .request(
                "initialize",
                serde_json::json!({
                    "adapterID": "lldb",
                    "clientID": "dirge-smoke",
                    "linesStartAt1": true,
                    "columnsStartAt1": true,
                    "pathFormat": "path",
                    "locale": "en-us"
                }),
                SMOKE_TIMEOUT,
            )
            .await
            .expect("lldb-dap initialize should succeed");

        // Check a native-debugger capability.
        assert!(
            caps.supports_configuration_done_request.unwrap_or(false),
            "lldb-dap should support configurationDoneRequest"
        );

        // Clean shutdown.
        let _ = client
            .request::<_, Value>(
                "disconnect",
                serde_json::json!({"terminateDebuggee": false}),
                SMOKE_TIMEOUT,
            )
            .await;
    }

    /// Full launch→stop-on-entry→continue→terminate with debugpy + test_program.py.
    #[tokio::test]
    async fn smoke_debugpy_launch_test_program() {
        skip_if_missing!("python3", "debugpy");

        let check = std::process::Command::new("python3")
            .args(["-c", "import debugpy"])
            .output();
        if check.map_or(true, |o| !o.status.success()) {
            eprintln!("SKIP: debugpy not installed");
            return;
        }

        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("tests")
            .join("dap")
            .join("fixtures")
            .join("test_program.py");
        assert!(
            fixture.exists(),
            "test_program.py must exist at {}",
            fixture.display()
        );

        let client = super::DapClient::spawn_stdio(
            "debugpy",
            std::path::Path::new("python3"),
            &["-m".to_string(), "debugpy.adapter".to_string()],
            std::path::Path::new("."),
        )
        .await
        .expect("debugpy adapter should spawn");

        // Register events before initialize so we don't miss any.
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel();
        client
            .on_event(
                "stopped",
                Box::new(move |body: serde_json::Value| {
                    let _ = evt_tx.send(body);
                }),
            )
            .await;
        client
            .on_event("output", Box::new(|_: serde_json::Value| {}))
            .await;
        client
            .on_event("terminated", Box::new(|_: serde_json::Value| {}))
            .await;

        // 1. initialize
        let caps: crate::dap::types::Capabilities = client
            .request(
                "initialize",
                serde_json::json!({
                    "adapterID": "debugpy",
                    "clientID": "dirge-smoke",
                    "linesStartAt1": true,
                    "columnsStartAt1": true,
                    "pathFormat": "path",
                    "locale": "en-us"
                }),
                SMOKE_TIMEOUT,
            )
            .await
            .expect("initialize should succeed");
        assert!(caps.supports_configuration_done_request.unwrap_or(false));

        // 2. launch with stopOnEntry (notify to avoid deadlock —
        // debugpy won't respond until configurationDone is sent)
        client
            .notify(
                "launch",
                &serde_json::json!({
                    "program": fixture.to_string_lossy(),
                    "stopOnEntry": true,
                    "console": "internalConsole"
                }),
            )
            .await
            .expect("launch notify should succeed");

        // 3. configurationDone
        client
            .request::<_, serde_json::Value>(
                "configurationDone",
                serde_json::json!({}),
                SMOKE_TIMEOUT,
            )
            .await
            .expect("configurationDone should succeed");

        // 4. Wait for stopped event (stopOnEntry)
        let stopped = tokio::time::timeout(SMOKE_TIMEOUT, evt_rx.recv())
            .await
            .expect("timed out waiting for stopped event")
            .expect("adapter disconnected before stopped event");
        assert_eq!(stopped["reason"], "entry", "expected stop-on-entry");

        // 5. continue
        client
            .request::<_, serde_json::Value>(
                "continue",
                serde_json::json!({"threadId": stopped["threadId"]}),
                SMOKE_TIMEOUT,
            )
            .await
            .expect("continue should succeed");

        // 6. terminate
        client
            .request::<_, serde_json::Value>("terminate", serde_json::json!({}), SMOKE_TIMEOUT)
            .await
            .expect("terminate should succeed");

        // 7. disconnect
        client
            .request::<_, serde_json::Value>(
                "disconnect",
                serde_json::json!({"terminateDebuggee": true}),
                SMOKE_TIMEOUT,
            )
            .await
            .expect("disconnect should succeed");
    }
}
