//! JSON-RPC 2.0 request/response correlation over a framed transport.
//!
//! Built on top of [`crate::lsp::jsonrpc`]. The client spawns a background
//! task that pumps incoming frames and routes them:
//! - responses (have `id`, no `method`) → resolve the matching pending request
//! - notifications (have `method`, no `id`) → dispatch to registered handlers
//! - server→client requests (have both `id` and `method`) → currently
//!   acknowledged with a null result (we register no client capabilities for
//!   this in v1)
//!
//! Outbound writes serialize through a mutex so multiple concurrent callers
//! don't interleave frames on stdin.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::lsp::jsonrpc::{decode_frame, encode_frame};

/// Failure surfaced to a pending request.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("RPC error {code}: {message}")]
    Server { code: i64, message: String },
    #[error("connection closed before response arrived")]
    ConnectionClosed,
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Handler invoked for an incoming notification. Synchronous for simplicity —
/// dispatch into a channel inside the closure if work needs to happen async.
pub type NotificationHandler = Box<dyn Fn(Value) + Send + Sync>;

type Pending = HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>;

struct Inner {
    next_id: AtomicU64,
    pending: Mutex<Pending>,
    handlers: Mutex<HashMap<String, NotificationHandler>>,
    writer: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    closed: std::sync::atomic::AtomicBool,
}

/// JSON-RPC client. Cheap to clone (just an `Arc`).
#[derive(Clone)]
pub struct RpcClient {
    inner: Arc<Inner>,
}

impl RpcClient {
    /// Create a client over a framed transport. Spawns a background task that
    /// pumps incoming frames; the returned [`JoinHandle`] lets callers await
    /// the reader's exit (it ends when the peer closes the stream).
    pub fn new<R, W>(reader: R, writer: W) -> (Self, JoinHandle<io::Result<()>>)
    where
        R: AsyncBufRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let inner = Arc::new(Inner {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            handlers: Mutex::new(HashMap::new()),
            writer: Mutex::new(Box::new(writer)),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        let client = RpcClient {
            inner: inner.clone(),
        };
        let task = tokio::spawn(read_loop(inner, reader));
        (client, task)
    }

    /// Send a request and await its response. Errors on connection close,
    /// I/O failure, server-side error response, or `timeout` elapsing.
    ///
    /// Tiny race window if a peer close interleaves with a request: the
    /// `closed` check + insert + write are not atomic against the read loop
    /// draining pending entries on EOF. In that case the request waits for
    /// its own timeout rather than failing instantly with `ConnectionClosed`.
    /// Callers should treat both terminations as terminal.
    pub async fn request<P, R>(
        &self,
        method: &str,
        params: P,
        request_timeout: Duration,
    ) -> Result<R, RpcError>
    where
        P: Serialize,
        R: serde::de::DeserializeOwned,
    {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(RpcError::ConnectionClosed);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, tx);

        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": serde_json::to_value(params)?,
        });
        let bytes = serde_json::to_vec(&body)?;
        let send_result = {
            let mut writer = self.inner.writer.lock().await;
            encode_frame(&mut *writer, &bytes).await
        };
        if let Err(e) = send_result {
            // Roll the pending entry back so we don't leak it.
            self.inner.pending.lock().await.remove(&id);
            return Err(RpcError::Io(e));
        }

        let value = match timeout(request_timeout, rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => {
                self.inner.pending.lock().await.remove(&id);
                return Err(RpcError::ConnectionClosed);
            }
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                return Err(RpcError::Timeout(request_timeout));
            }
        };
        Ok(serde_json::from_value(value)?)
    }

    /// Fire-and-forget notification. No id, no response.
    pub async fn notify<P>(&self, method: &str, params: P) -> Result<(), RpcError>
    where
        P: Serialize,
    {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(RpcError::ConnectionClosed);
        }
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": serde_json::to_value(params)?,
        });
        let bytes = serde_json::to_vec(&body)?;
        let mut writer = self.inner.writer.lock().await;
        encode_frame(&mut *writer, &bytes).await?;
        Ok(())
    }

    /// Register a handler for an incoming server notification. Replaces any
    /// previously-registered handler for the same method.
    pub async fn on_notification(&self, method: &str, handler: NotificationHandler) {
        self.inner
            .handlers
            .lock()
            .await
            .insert(method.to_string(), handler);
    }
}

async fn read_loop<R>(inner: Arc<Inner>, mut reader: R) -> io::Result<()>
where
    R: AsyncBufRead + Send + Unpin,
{
    loop {
        let frame = match decode_frame(&mut reader).await {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                // Clean shutdown — peer closed.
                break;
            }
            Err(e) => return Err(e),
        };
        let msg: Value = match serde_json::from_slice(&frame) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("lsp: skipping non-JSON frame: {e}");
                continue;
            }
        };
        dispatch(&inner, msg).await;
    }
    // Stream closed — fail any pending requests and mark closed.
    inner.closed.store(true, Ordering::SeqCst);
    let mut pending = inner.pending.lock().await;
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(RpcError::ConnectionClosed));
    }
    Ok(())
}

async fn dispatch(inner: &Arc<Inner>, msg: Value) {
    let id = msg.get("id").and_then(|v| v.as_u64());
    let method = msg.get("method").and_then(|v| v.as_str()).map(String::from);

    match (id, method) {
        (Some(id), None) => {
            // Response to one of our requests.
            let sender = inner.pending.lock().await.remove(&id);
            if let Some(sender) = sender {
                let result = if let Some(err) = msg.get("error") {
                    let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                    let message = err
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no message)")
                        .to_string();
                    Err(RpcError::Server { code, message })
                } else {
                    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = sender.send(result);
            }
        }
        (None, Some(method)) => {
            // Server-initiated notification.
            let handlers = inner.handlers.lock().await;
            if let Some(handler) = handlers.get(&method) {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                handler(params);
            }
        }
        (Some(id), Some(_method)) => {
            // Server-to-client request. We don't advertise any client-side
            // request capabilities yet; acknowledge with a null result so the
            // server doesn't hang.
            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": Value::Null,
            });
            if let Ok(bytes) = serde_json::to_vec(&response) {
                let mut writer = inner.writer.lock().await;
                let _ = encode_frame(&mut *writer, &bytes).await;
            }
        }
        (None, None) => {
            tracing::warn!("lsp: ignoring frame without id or method");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    /// Build a client whose I/O is wired to two duplex pipes, and return
    /// "server-side" halves that the test can use to read what the client
    /// sent and to send back responses.
    fn pair() -> (
        RpcClient,
        JoinHandle<io::Result<()>>,
        tokio::io::ReadHalf<tokio::io::DuplexStream>, // server reads what the client sent
        tokio::io::WriteHalf<tokio::io::DuplexStream>, // server writes; client reads
    ) {
        let (client_in, server_out) = tokio::io::duplex(4096); // client reads <- server writes
        let (server_in, client_out) = tokio::io::duplex(4096); // client writes -> server reads
        let (client_reader, _) = tokio::io::split(client_in);
        let (_, client_writer) = tokio::io::split(client_out);
        let (server_reader, _) = tokio::io::split(server_in);
        let (_, server_writer) = tokio::io::split(server_out);
        let (client, task) = RpcClient::new(BufReader::new(client_reader), client_writer);
        (client, task, server_reader, server_writer)
    }

    async fn read_client_frame<R>(reader: &mut R) -> Value
    where
        R: tokio::io::AsyncReadExt + Unpin + tokio::io::AsyncBufRead,
    {
        let bytes = decode_frame(reader).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn request_round_trips_and_resolves_with_result() {
        let (client, _task, server_reader, mut server_writer) = pair();
        let mut server_reader = BufReader::new(server_reader);

        let req_task = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .request::<_, Value>("ping", json!({"q": 1}), Duration::from_secs(2))
                    .await
            }
        });

        let req = read_client_frame(&mut server_reader).await;
        assert_eq!(req["method"], "ping");
        assert_eq!(req["params"]["q"], 1);
        let id = req["id"].as_u64().unwrap();

        // Server side: respond with the same id.
        let resp = json!({"jsonrpc": "2.0", "id": id, "result": {"pong": true}});
        let bytes = serde_json::to_vec(&resp).unwrap();
        encode_frame(&mut server_writer, &bytes).await.unwrap();

        let got = req_task.await.unwrap().unwrap();
        assert_eq!(got, json!({"pong": true}));
    }

    #[tokio::test]
    async fn request_returns_server_error_when_response_has_error() {
        let (client, _task, server_reader, mut server_writer) = pair();
        let mut server_reader = BufReader::new(server_reader);

        let req_task = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .request::<_, Value>("explode", json!({}), Duration::from_secs(2))
                    .await
            }
        });

        let req = read_client_frame(&mut server_reader).await;
        let id = req["id"].as_u64().unwrap();
        let resp = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "method not found"}
        });
        encode_frame(&mut server_writer, &serde_json::to_vec(&resp).unwrap())
            .await
            .unwrap();

        let err = req_task.await.unwrap().unwrap_err();
        match err {
            RpcError::Server { code, message } => {
                assert_eq!(code, -32601);
                assert!(message.contains("method not found"));
            }
            other => panic!("expected Server error, got {other:?}"),
        }
    }

    // Regression: multiple in-flight requests must each get correlated to
    // their own response by id. If the dispatch routed by order rather than
    // id, out-of-order server responses would resolve the wrong future.
    #[tokio::test]
    async fn regression_in_flight_requests_correlated_by_id() {
        let (client, _task, server_reader, mut server_writer) = pair();
        let mut server_reader = BufReader::new(server_reader);

        let a = tokio::spawn({
            let c = client.clone();
            async move {
                c.request::<_, Value>("op", json!({"n": 1}), Duration::from_secs(2))
                    .await
            }
        });
        let b = tokio::spawn({
            let c = client.clone();
            async move {
                c.request::<_, Value>("op", json!({"n": 2}), Duration::from_secs(2))
                    .await
            }
        });

        // Read both requests; respond in reverse order.
        let req1 = read_client_frame(&mut server_reader).await;
        let req2 = read_client_frame(&mut server_reader).await;
        let id1 = req1["id"].as_u64().unwrap();
        let id2 = req2["id"].as_u64().unwrap();

        let resp2 = json!({"jsonrpc":"2.0","id":id2,"result":{"answer":"second"}});
        encode_frame(&mut server_writer, &serde_json::to_vec(&resp2).unwrap())
            .await
            .unwrap();
        let resp1 = json!({"jsonrpc":"2.0","id":id1,"result":{"answer":"first"}});
        encode_frame(&mut server_writer, &serde_json::to_vec(&resp1).unwrap())
            .await
            .unwrap();

        let got_a = a.await.unwrap().unwrap();
        let got_b = b.await.unwrap().unwrap();
        assert_eq!(got_a["answer"], "first");
        assert_eq!(got_b["answer"], "second");
    }

    // Regression: a request whose server never replies must time out cleanly
    // rather than block the caller forever.
    #[tokio::test]
    async fn regression_request_times_out_when_no_response() {
        let (client, _task, _server_reader, _server_writer) = pair();
        let err = client
            .request::<_, Value>("never", json!({}), Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Timeout(_)));
    }

    // Regression: when the timeout fires, the pending-entry for that id must
    // be removed from the table — otherwise the entry leaks and a late
    // response would still attempt to resolve a dropped channel.
    #[tokio::test]
    async fn regression_timeout_clears_pending_entry() {
        let (client, _task, _server_reader, _server_writer) = pair();
        let _ = client
            .request::<_, Value>("never", json!({}), Duration::from_millis(20))
            .await;
        let pending = client.inner.pending.lock().await;
        assert!(pending.is_empty(), "pending must be empty after timeout");
    }

    #[tokio::test]
    async fn notify_sends_payload_without_id() {
        let (client, _task, server_reader, _server_writer) = pair();
        let mut server_reader = BufReader::new(server_reader);

        client
            .notify("textDocument/didOpen", json!({"path": "x.rs"}))
            .await
            .unwrap();
        let frame = read_client_frame(&mut server_reader).await;
        assert_eq!(frame["method"], "textDocument/didOpen");
        assert_eq!(frame["params"]["path"], "x.rs");
        assert!(frame.get("id").is_none(), "notifications must not carry id");
    }

    #[tokio::test]
    async fn server_notification_dispatches_to_registered_handler() {
        let (client, _task, _server_reader, mut server_writer) = pair();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        client
            .on_notification(
                "textDocument/publishDiagnostics",
                Box::new(move |params| {
                    let _ = tx.send(params);
                }),
            )
            .await;

        // Server pushes a notification.
        let note = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///x.rs", "diagnostics": []},
        });
        encode_frame(&mut server_writer, &serde_json::to_vec(&note).unwrap())
            .await
            .unwrap();

        let got = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("handler must fire within timeout")
            .unwrap();
        assert_eq!(got["uri"], "file:///x.rs");
    }

    // Regression: a server-initiated request (id + method) must be
    // acknowledged with a null result so the server doesn't hang waiting
    // for the client's reply. v1 doesn't advertise any client capabilities
    // that would actually receive these.
    #[tokio::test]
    async fn regression_server_request_acknowledged_with_null_result() {
        let (client, _task, server_reader, mut server_writer) = pair();
        let mut server_reader = BufReader::new(server_reader);

        let server_req = json!({
            "jsonrpc": "2.0",
            "id": 999,
            "method": "workspace/configuration",
            "params": {},
        });
        encode_frame(
            &mut server_writer,
            &serde_json::to_vec(&server_req).unwrap(),
        )
        .await
        .unwrap();

        let reply = read_client_frame(&mut server_reader).await;
        assert_eq!(reply["id"], 999);
        assert!(reply["result"].is_null());
        // No error key on a successful ack.
        assert!(reply.get("error").is_none());

        // Keep the client alive past the assertion.
        drop(client);
    }

    // Regression: when the peer drops the stream, all in-flight requests
    // must resolve with ConnectionClosed so callers don't hang.
    #[tokio::test]
    async fn regression_in_flight_requests_fail_on_peer_close() {
        let (client, task, _server_reader, server_writer) = pair();

        let pending = tokio::spawn({
            let c = client.clone();
            async move {
                c.request::<_, Value>("op", json!({}), Duration::from_secs(2))
                    .await
            }
        });

        // Drop the server-side writer → client's read loop hits EOF.
        drop(server_writer);
        // Drain the reader half so the reader task makes progress.
        let _ = task.await;

        let err = pending.await.unwrap().unwrap_err();
        assert!(matches!(err, RpcError::ConnectionClosed));
    }

    // After the peer closes, subsequent requests must fail fast rather than
    // re-attempting and hanging on a dead writer.
    #[tokio::test]
    async fn request_after_close_returns_closed_error() {
        let (client, task, _server_reader, server_writer) = pair();
        drop(server_writer);
        let _ = task.await;

        let err = client
            .request::<_, Value>("op", json!({}), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::ConnectionClosed));
    }
}
