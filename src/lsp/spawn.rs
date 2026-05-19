//! Spawning abstraction for LSP server processes.
//!
//! `Spawner` is a tiny trait so the orchestrator can be tested without
//! launching real `rust-analyzer` / `pyright` / etc. processes in CI. The
//! real implementation ([`ProcessSpawner`]) does the obvious thing with
//! [`tokio::process::Command`]; tests use an in-memory mock that pairs the
//! client with a fake server task over `tokio::io::duplex`.

use std::path::{Path, PathBuf};

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite};

/// Result of a successful spawn: the async I/O halves the [`crate::lsp::rpc::RpcClient`]
/// will consume, the server-specific `initializationOptions` payload, and an
/// opaque guard that holds the child process alive (its `Drop` terminates).
pub struct Spawned {
    pub reader: Box<dyn AsyncBufRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    pub init_options: Value,
    /// Whatever needs to live for the child's lifetime — typically a
    /// `tokio::process::Child` with `kill_on_drop(true)`. Opaque to the
    /// manager; dropped when the client is shut down.
    ///
    /// `Send + Sync` because the manager stores entries behind an `Arc` and
    /// passes them across `tokio::spawn` boundaries; both `Child` and
    /// `JoinHandle` already satisfy this.
    pub guard: Box<dyn std::any::Any + Send + Sync>,
}

/// Launches LSP server processes. Trait so the orchestrator can be unit-
/// tested with in-memory duplex pipes.
pub trait Spawner: Send + Sync + 'static {
    fn spawn<'a>(
        &'a self,
        server_id: &'a str,
        root: &'a Path,
    ) -> BoxFuture<'a, std::io::Result<Spawned>>;
}

/// Default `Spawner` for production. Resolves the binary name via `which`-
/// style PATH lookup and spawns it with stdin/stdout/stderr piped.
///
/// Knows nothing about LSP semantics — the orchestrator (Phase 4) chooses
/// the binary name + args based on the `server_id` and any user config.
pub struct ProcessSpawner {
    /// Maps `server_id` to the binary + args to launch. Populated from the
    /// builtin registry + user config when the manager is constructed.
    commands: std::collections::HashMap<String, ProcessCommand>,
}

#[derive(Clone, Debug)]
pub struct ProcessCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub init_options: Value,
}

impl ProcessSpawner {
    pub fn new(commands: std::collections::HashMap<String, ProcessCommand>) -> Self {
        Self { commands }
    }
}

impl Spawner for ProcessSpawner {
    fn spawn<'a>(
        &'a self,
        server_id: &'a str,
        root: &'a Path,
    ) -> BoxFuture<'a, std::io::Result<Spawned>> {
        Box::pin(async move {
            let cmd = self.commands.get(server_id).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no spawn command configured for LSP server {server_id:?}"),
                )
            })?;

            let mut command = tokio::process::Command::new(&cmd.program);
            command
                .args(&cmd.args)
                .current_dir(root)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            for (k, v) in &cmd.env {
                command.env(k, v);
            }

            let mut child = command.spawn().map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("failed to spawn LSP server {server_id:?}: {e}"),
                )
            })?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| std::io::Error::other("LSP server stdin pipe unavailable"))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| std::io::Error::other("LSP server stdout pipe unavailable"))?;

            // Drain stderr in the background. LSP servers chatter on stderr
            // (rust-analyzer logs there) and a full pipe would block the
            // child after ~64 KB.
            if let Some(stderr) = child.stderr.take() {
                let server_id = server_id.to_string();
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt;
                    let mut reader = tokio::io::BufReader::new(stderr);
                    let mut buf = String::new();
                    loop {
                        buf.clear();
                        match reader.read_line(&mut buf).await {
                            Ok(0) => break, // EOF
                            Ok(_) => tracing::debug!(server = %server_id, "{}", buf.trim_end()),
                            Err(_) => break,
                        }
                    }
                });
            }

            let reader: Box<dyn AsyncBufRead + Send + Unpin> =
                Box::new(tokio::io::BufReader::new(stdout));
            let writer: Box<dyn AsyncWrite + Send + Unpin> = Box::new(stdin);

            Ok(Spawned {
                reader,
                writer,
                init_options: cmd.init_options.clone(),
                guard: Box::new(child),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock spawner that pairs the client with a fake LSP server task over
    /// duplex pipes. The fake task responds to `initialize` with empty
    /// capabilities and any other request with `result: null`.
    pub(crate) struct MockSpawner {
        spawn_calls: std::sync::Mutex<Vec<(String, PathBuf)>>,
        fail_for: std::sync::Mutex<std::collections::HashSet<String>>,
    }

    impl MockSpawner {
        pub fn new() -> Self {
            Self {
                spawn_calls: std::sync::Mutex::new(Vec::new()),
                fail_for: std::sync::Mutex::new(std::collections::HashSet::new()),
            }
        }

        pub fn fail_when_server_id(&self, server_id: &str) {
            self.fail_for.lock().unwrap().insert(server_id.to_string());
        }

        pub fn calls(&self) -> Vec<(String, PathBuf)> {
            self.spawn_calls.lock().unwrap().clone()
        }
    }

    impl Spawner for MockSpawner {
        fn spawn<'a>(
            &'a self,
            server_id: &'a str,
            root: &'a Path,
        ) -> BoxFuture<'a, std::io::Result<Spawned>> {
            Box::pin(async move {
                self.spawn_calls
                    .lock()
                    .unwrap()
                    .push((server_id.to_string(), root.to_path_buf()));

                if self.fail_for.lock().unwrap().contains(server_id) {
                    return Err(std::io::Error::other(format!(
                        "mock spawn refused for {server_id}"
                    )));
                }

                let (client_in, mut server_writer) = tokio::io::duplex(8192);
                let (mut server_reader, client_out) = tokio::io::duplex(8192);

                // Fake server task: respond to anything with a sensible reply.
                let fake_server = tokio::spawn(async move {
                    use crate::lsp::jsonrpc::{decode_frame, encode_frame};
                    use serde_json::json;
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
                            continue; // notification — no reply
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

    #[tokio::test]
    async fn mock_spawner_responds_to_initialize() {
        use crate::lsp::init::initialize;
        use crate::lsp::rpc::RpcClient;
        use tokio::io::BufReader;

        let s = MockSpawner::new();
        let spawned = s.spawn("rust", Path::new("/tmp")).await.unwrap();
        // BufReader<Box<dyn AsyncBufRead>> doesn't make sense — the inner
        // reader already implements AsyncBufRead. Use the boxed reader
        // directly.
        let reader = BufReader::new(spawned.reader);
        let (rpc, _) = RpcClient::new(reader, spawned.writer);
        let result = initialize(&rpc, Path::new("/tmp"), Some(1), spawned.init_options).await;
        assert!(result.is_ok(), "initialize should succeed: {result:?}");
    }

    #[tokio::test]
    async fn mock_spawner_records_calls() {
        let s = MockSpawner::new();
        let _ = s.spawn("rust", Path::new("/tmp")).await.unwrap();
        let _ = s.spawn("typescript", Path::new("/tmp/proj")).await.unwrap();
        let calls = s.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "rust");
        assert_eq!(calls[1].0, "typescript");
    }

    #[tokio::test]
    async fn mock_spawner_can_fail_on_command() {
        let s = MockSpawner::new();
        s.fail_when_server_id("rust");
        // Can't `.unwrap_err()` directly because `Spawned` isn't `Debug`.
        match s.spawn("rust", Path::new("/tmp")).await {
            Ok(_) => panic!("expected spawn to fail"),
            Err(e) => assert!(e.to_string().contains("refused")),
        }
    }
}
