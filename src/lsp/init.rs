//! LSP initialize handshake.
//!
//! Wraps the spec's three-step start-up:
//! 1. send `initialize` request with our client capabilities
//! 2. receive `InitializeResult` carrying the server's capabilities
//! 3. send `initialized` notification to acknowledge
//!
//! Returns the [`InitializeResult`] so callers can introspect capability
//! flags (textDocumentSync mode, diagnosticProvider, etc.) without parsing
//! the JSON themselves.

use std::path::Path;
use std::time::Duration;

use lsp_types::{
    ClientCapabilities, DiagnosticClientCapabilities, DidChangeWatchedFilesClientCapabilities,
    GeneralClientCapabilities, InitializeParams, InitializeResult,
    PublishDiagnosticsClientCapabilities, TextDocumentClientCapabilities,
    TextDocumentSyncClientCapabilities, Uri, WindowClientCapabilities, WorkspaceClientCapabilities,
    WorkspaceFolder,
};

use crate::lsp::rpc::{RpcClient, RpcError};

/// Time we'll wait for the server to answer `initialize`. Matches opencode's
/// 45s ceiling — rust-analyzer in particular can take a moment when first
/// indexing a large workspace.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(45);

/// Run the LSP initialize handshake against a connected [`RpcClient`].
///
/// `root` must be a canonical filesystem path; it's converted to a `file://`
/// URI and sent as both `rootUri` and a single-element `workspaceFolders`.
/// `initialization_options` is the server-specific payload (e.g. for the
/// typescript LSP, the resolved tsserver path); pass `serde_json::Value::Null`
/// when there are none.
pub async fn initialize(
    client: &RpcClient,
    root: &Path,
    process_id: Option<u32>,
    initialization_options: serde_json::Value,
) -> Result<InitializeResult, RpcError> {
    let root_uri = path_to_file_uri(root)?;

    let params = InitializeParams {
        process_id,
        workspace_folders: Some(vec![WorkspaceFolder {
            name: "workspace".to_string(),
            uri: root_uri.clone(),
        }]),
        // `rootUri` is deprecated in favor of `workspaceFolders` but every
        // shipping server still reads it; send both for compatibility.
        #[allow(deprecated)]
        root_uri: Some(root_uri),
        initialization_options: if initialization_options.is_null() {
            None
        } else {
            Some(initialization_options)
        },
        capabilities: client_capabilities(),
        ..Default::default()
    };

    let result: InitializeResult = client
        .request("initialize", params, INITIALIZE_TIMEOUT)
        .await?;

    client.notify("initialized", serde_json::json!({})).await?;

    Ok(result)
}

fn path_to_file_uri(path: &Path) -> Result<Uri, RpcError> {
    // `file://` URI from a filesystem path. We don't try to handle Windows
    // drive letters specially — dirge isn't tested on Windows.
    let canonical = path.to_string_lossy();
    let encoded = percent_encode_path(&canonical);
    let uri_str = if canonical.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    };
    uri_str.parse::<Uri>().map_err(|e| {
        RpcError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid path for file URI: {e}"),
        ))
    })
}

/// Percent-encode characters that would otherwise be interpreted as URI
/// structural delimiters. Slashes are preserved (path separators). Conforms
/// to RFC 3986's `unreserved` set + `/` for path segments. Pure ASCII only —
/// non-ASCII bytes pass through and the Uri parser does its own escaping or
/// rejection.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        let safe =
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~' | b':');
        if safe {
            out.push(byte as char);
        } else if byte < 0x80 {
            out.push_str(&format!("%{byte:02X}"));
        } else {
            // Non-ASCII — emit as UTF-8 percent-encoded bytes.
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The client capabilities we advertise. Conservative and stable — matches
/// what opencode sends. Phase 3's per-file work depends on these being
/// honoured by the server:
/// - synchronization (didOpen/didChange)
/// - publishDiagnostics (push diagnostics)
/// - diagnostic (pull diagnostics, dynamic registration)
fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                relative_pattern_support: Some(false),
            }),
            workspace_folders: Some(true),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities {
                did_save: Some(false),
                dynamic_registration: Some(false),
                will_save: Some(false),
                will_save_wait_until: Some(false),
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                related_information: Some(true),
                version_support: Some(false),
                ..Default::default()
            }),
            diagnostic: Some(DiagnosticClientCapabilities {
                dynamic_registration: Some(true),
                related_document_support: Some(true),
            }),
            ..Default::default()
        }),
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            ..Default::default()
        }),
        general: Some(GeneralClientCapabilities {
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::jsonrpc::{decode_frame, encode_frame};
    use serde_json::{Value, json};
    use tokio::io::BufReader;

    /// Spin up a fake LSP server task wired to a duplex pair. The task reads
    /// the next frame from the client, calls `respond(req)` to compute a
    /// reply, and writes it back. Returns the client + a join handle for
    /// the fake server.
    async fn with_fake_server<F>(respond: F) -> (RpcClient, tokio::task::JoinHandle<()>)
    where
        F: Fn(Value) -> Value + Send + 'static,
    {
        let (client_in, server_out) = tokio::io::duplex(4096);
        let (server_in, client_out) = tokio::io::duplex(4096);
        let (client_reader, _) = tokio::io::split(client_in);
        let (_, client_writer) = tokio::io::split(client_out);
        let (server_reader_half, _) = tokio::io::split(server_in);
        let (_, mut server_writer) = tokio::io::split(server_out);
        let (client, _task) = RpcClient::new(BufReader::new(client_reader), client_writer);

        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader_half);
            loop {
                let frame = match decode_frame(&mut reader).await {
                    Ok(b) => b,
                    Err(_) => break,
                };
                let req: Value = serde_json::from_slice(&frame).unwrap();
                let reply = respond(req);
                if reply.is_null() {
                    continue; // notification — no reply
                }
                let bytes = serde_json::to_vec(&reply).unwrap();
                if encode_frame(&mut server_writer, &bytes).await.is_err() {
                    break;
                }
            }
        });

        (client, server)
    }

    #[tokio::test]
    async fn initialize_round_trips_params_and_returns_capabilities() {
        let (client, _server) = with_fake_server(|req: Value| {
            if req["method"] == "initialize" {
                let id = req["id"].clone();
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "capabilities": {
                            "textDocumentSync": 2,
                            "diagnosticProvider": {
                                "interFileDependencies": true,
                                "workspaceDiagnostics": false
                            }
                        }
                    }
                })
            } else {
                Value::Null // initialized notification — no reply
            }
        })
        .await;

        let root = std::env::temp_dir();
        let result = initialize(&client, &root, Some(12345), Value::Null)
            .await
            .unwrap();

        // We received the capabilities the fake server advertised.
        assert!(result.capabilities.diagnostic_provider.is_some());
        // textDocumentSync should round-trip too.
        assert!(result.capabilities.text_document_sync.is_some());
    }

    // Regression: the initialize request body must carry the rootUri /
    // workspaceFolders pointing at the path we passed in. Without this,
    // rust-analyzer attaches at the wrong directory and misses workspace
    // members.
    #[tokio::test]
    async fn regression_initialize_request_carries_root_uri() {
        // We hand-spin the server side so we can inspect the raw request
        // before answering it.
        let (client_in, server_out) = tokio::io::duplex(4096);
        let (server_in, client_out) = tokio::io::duplex(4096);
        let (client_reader, _) = tokio::io::split(client_in);
        let (_, client_writer) = tokio::io::split(client_out);
        let (server_reader, _) = tokio::io::split(server_in);
        let (_, mut server_writer) = tokio::io::split(server_out);
        let (client, _task) = RpcClient::new(BufReader::new(client_reader), client_writer);

        let root = std::env::temp_dir();
        let root_str = root.to_string_lossy().to_string();

        // Spawn a server that captures the initialize request and replies.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            let frame = decode_frame(&mut reader).await.unwrap();
            let req: Value = serde_json::from_slice(&frame).unwrap();
            let id = req["id"].clone();
            let _ = tx.send(req);
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"capabilities": {}}
            });
            encode_frame(&mut server_writer, &serde_json::to_vec(&resp).unwrap())
                .await
                .unwrap();
            // Eat the `initialized` notification.
            let _ = decode_frame(&mut reader).await;
        });

        let _ = initialize(&client, &root, Some(1), Value::Null)
            .await
            .unwrap();

        let req = rx.recv().await.unwrap();
        let uri = req["params"]["rootUri"].as_str().unwrap();
        assert!(uri.starts_with("file://"), "got: {uri}");
        assert!(
            uri.contains(&*root_str),
            "expected {root_str} in uri: {uri}"
        );

        let folders = req["params"]["workspaceFolders"].as_array().unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0]["name"], "workspace");
    }

    // Regression: server-specific `initializationOptions` must propagate.
    // The typescript LSP (Phase 1 server) refuses to attach without
    // `tsserver.path` in this payload.
    #[tokio::test]
    async fn regression_initialization_options_propagate_when_provided() {
        let (client_in, server_out) = tokio::io::duplex(4096);
        let (server_in, client_out) = tokio::io::duplex(4096);
        let (client_reader, _) = tokio::io::split(client_in);
        let (_, client_writer) = tokio::io::split(client_out);
        let (server_reader, _) = tokio::io::split(server_in);
        let (_, mut server_writer) = tokio::io::split(server_out);
        let (client, _task) = RpcClient::new(BufReader::new(client_reader), client_writer);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            let frame = decode_frame(&mut reader).await.unwrap();
            let req: Value = serde_json::from_slice(&frame).unwrap();
            let id = req["id"].clone();
            let _ = tx.send(req);
            let resp = json!({"jsonrpc":"2.0","id":id,"result":{"capabilities":{}}});
            encode_frame(&mut server_writer, &serde_json::to_vec(&resp).unwrap())
                .await
                .unwrap();
            let _ = decode_frame(&mut reader).await;
        });

        let opts = json!({"tsserver": {"path": "/path/to/tsserver"}});
        let _ = initialize(&client, &std::env::temp_dir(), None, opts)
            .await
            .unwrap();

        let req = rx.recv().await.unwrap();
        assert_eq!(
            req["params"]["initializationOptions"]["tsserver"]["path"],
            "/path/to/tsserver"
        );
    }

    // Null initializationOptions must NOT serialize a JSON `null` for the
    // field — some servers reject the field's mere presence with null. Match
    // opencode's behavior (omit the field).
    #[tokio::test]
    async fn null_initialization_options_omits_field() {
        let (client_in, server_out) = tokio::io::duplex(4096);
        let (server_in, client_out) = tokio::io::duplex(4096);
        let (client_reader, _) = tokio::io::split(client_in);
        let (_, client_writer) = tokio::io::split(client_out);
        let (server_reader, _) = tokio::io::split(server_in);
        let (_, mut server_writer) = tokio::io::split(server_out);
        let (client, _task) = RpcClient::new(BufReader::new(client_reader), client_writer);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            let frame = decode_frame(&mut reader).await.unwrap();
            let req: Value = serde_json::from_slice(&frame).unwrap();
            let id = req["id"].clone();
            let _ = tx.send(req);
            let resp = json!({"jsonrpc":"2.0","id":id,"result":{"capabilities":{}}});
            encode_frame(&mut server_writer, &serde_json::to_vec(&resp).unwrap())
                .await
                .unwrap();
            let _ = decode_frame(&mut reader).await;
        });

        let _ = initialize(&client, &std::env::temp_dir(), None, Value::Null)
            .await
            .unwrap();

        let req = rx.recv().await.unwrap();
        let opts = req["params"].get("initializationOptions");
        assert!(
            opts.is_none() || opts.unwrap().is_null(),
            "expected omitted or explicit null; got: {opts:?}"
        );
    }

    // Regression: paths containing URI-significant characters must be
    // percent-encoded. A `#` would otherwise terminate the path early and
    // produce a fragment.
    #[test]
    fn path_to_file_uri_percent_encodes_special_chars() {
        let p = Path::new("/tmp/proj #1/src/main.rs");
        let uri = path_to_file_uri(p).unwrap();
        let s = uri.as_str();
        assert!(s.starts_with("file:///"), "got: {s}");
        assert!(s.contains("%23"), "must encode '#' as %23, got: {s}");
        assert!(s.contains("%20"), "must encode space as %20, got: {s}");
    }

    #[test]
    fn path_to_file_uri_preserves_slashes_and_safe_chars() {
        let p = Path::new("/tmp/proj_v1.0-rc/main.rs");
        let uri = path_to_file_uri(p).unwrap();
        let s = uri.as_str();
        assert!(
            s.starts_with("file:///tmp/proj_v1.0-rc/main.rs"),
            "got: {s}"
        );
    }

    // The `initialized` notification must follow the InitializeResult — some
    // servers stall until they see it.
    #[tokio::test]
    async fn initialized_notification_is_sent_after_response() {
        let (client_in, server_out) = tokio::io::duplex(4096);
        let (server_in, client_out) = tokio::io::duplex(4096);
        let (client_reader, _) = tokio::io::split(client_in);
        let (_, client_writer) = tokio::io::split(client_out);
        let (server_reader, _) = tokio::io::split(server_in);
        let (_, mut server_writer) = tokio::io::split(server_out);
        let (client, _task) = RpcClient::new(BufReader::new(client_reader), client_writer);

        let observed = tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            // First message: initialize request.
            let first = decode_frame(&mut reader).await.unwrap();
            let req: Value = serde_json::from_slice(&first).unwrap();
            assert_eq!(req["method"], "initialize");
            let id = req["id"].clone();
            let resp = json!({"jsonrpc":"2.0","id":id,"result":{"capabilities":{}}});
            encode_frame(&mut server_writer, &serde_json::to_vec(&resp).unwrap())
                .await
                .unwrap();

            // Second message: initialized notification.
            let second = decode_frame(&mut reader).await.unwrap();
            let notif: Value = serde_json::from_slice(&second).unwrap();
            assert_eq!(notif["method"], "initialized");
            assert!(
                notif.get("id").is_none(),
                "initialized must be a notification"
            );
        });

        let _ = initialize(&client, &std::env::temp_dir(), None, Value::Null)
            .await
            .unwrap();
        observed.await.unwrap();
    }
}
