use std::collections::HashMap;
use std::process::Stdio;

use rmcp::service::{RoleClient, RunningService, serve_client};
use tokio::io::AsyncBufReadExt;
use tokio::process::{ChildStderr, Command};

use super::config::McpServerConfig;

pub struct McpClientHandle {
    pub server_name: String,
    pub running_service: RunningService<RoleClient, ()>,
}

/// Upper bound on how long we'll wait for an MCP server to complete
/// initialization. Command-based servers that hang on `initialize`
/// (e.g. waiting for stdin that never comes) would otherwise pin
/// startup indefinitely. 10s is generous for legitimate inits — npm
/// install-on-first-run servers take a few seconds; locally-running
/// binaries respond in <100ms. Past the cap we abort and log.
const MCP_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl McpClientHandle {
    pub async fn connect(server_name: String, config: &McpServerConfig) -> anyhow::Result<Self> {
        // Wrap the entire connect in a timeout so a wedged server
        // doesn't block startup forever. Returns a clean
        // "init timeout" error past the cap.
        let inner = Self::connect_inner(server_name.clone(), config);
        match tokio::time::timeout(MCP_INIT_TIMEOUT, inner).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "MCP server {server_name:?} did not initialize within {}s — skipping",
                MCP_INIT_TIMEOUT.as_secs(),
            )),
        }
    }

    async fn connect_inner(server_name: String, config: &McpServerConfig) -> anyhow::Result<Self> {
        match config {
            McpServerConfig::Command { command, args, env } => {
                let mut cmd = Command::new(command);
                cmd.args(args);
                for (k, v) in env {
                    cmd.env(k, v);
                }
                // CRITICAL: capture stderr instead of inheriting it.
                // rmcp's default `TokioChildProcess::new` uses
                // `Stdio::inherit()` for stderr, which gives the MCP
                // server (and its descendants) direct access to
                // dirge's controlling terminal. If the server (or
                // any library it uses) emits terminal queries — OSC
                // 11 for bg-color detection, `\x1b[c` for DA1,
                // `\x1b[6n` for CPR — those queries reach the
                // terminal, which replies via the TTY's INPUT side
                // (dirge's stdin). Crossterm's event parser doesn't
                // recognize those reply shapes, so the bytes sit in
                // the OS stdin buffer until exit, when the shell
                // inherits them and renders the literal escape
                // payload as visible garbage at the prompt.
                //
                // Pipe stderr instead. The child's logs are still
                // surfaced — we line-read them and forward to
                // dirge's stderr via tracing — but the child no
                // longer has a route to send escape queries that
                // can elicit a reply on dirge's stdin.
                let (transport, stderr) =
                    rmcp::transport::child_process::TokioChildProcess::builder(cmd)
                        .stderr(Stdio::piped())
                        .spawn()?;
                if let Some(child_stderr) = stderr {
                    spawn_stderr_forwarder(server_name.clone(), child_stderr);
                }
                let running_service = serve_client((), transport).await.map_err(|e| {
                    anyhow::anyhow!("MCP connection failed for '{server_name}': {e}")
                })?;
                Ok(Self {
                    server_name,
                    running_service,
                })
            }
            McpServerConfig::Url { url, headers } => {
                let custom_headers = parse_headers(headers)?;
                let cfg = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.as_str())
                    .custom_headers(custom_headers);
                type HttpClient = rmcp::transport::StreamableHttpClientTransport<reqwest::Client>;
                let transport = HttpClient::from_config(cfg);
                let running_service = serve_client((), transport).await.map_err(|e| {
                    anyhow::anyhow!("MCP HTTP connection failed for '{server_name}': {e}")
                })?;
                Ok(Self {
                    server_name,
                    running_service,
                })
            }
        }
    }

    pub fn peer(&self) -> rmcp::service::Peer<RoleClient> {
        self.running_service.peer().clone()
    }

    pub async fn list_tools(&self) -> Result<Vec<rmcp::model::Tool>, rmcp::ServiceError> {
        self.running_service.peer().list_all_tools().await
    }
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
fn spawn_stderr_forwarder(server_name: String, stderr: ChildStderr) {
    tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // Drop any control / escape bytes so a child can't
            // smuggle terminal queries through the forwarder.
            // Strip everything below 0x20 except `\t`, plus any
            // standalone ESC (0x1b). The `\x1b]` / `\x1b[` query
            // shapes get neutralized here.
            let sanitized: String = line
                .chars()
                .filter(|c| {
                    let b = *c as u32;
                    b == 0x09 || (0x20..0x7f).contains(&b) || b >= 0x80
                })
                .collect();
            tracing::info!(target: "dirge::mcp", server = %server_name, "{}", sanitized);
        }
    });
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
