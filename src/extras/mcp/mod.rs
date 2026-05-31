pub mod client;
pub mod config;
pub mod tool;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tool::McpTool;

use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

pub struct McpClientManager {
    /// Connection state per server, by name. Each `Arc<SharedConnection>`
    /// is the SINGLE owner of its peer + RunningService. Cloned into every
    /// `McpTool` from that server so manual `/mcp reconnect` AND tool-side
    /// auto-reconnect share the same swap target (M-R1 + M-R4 fix).
    connections: HashMap<String, Arc<client::SharedConnection>>,
    /// Per-server reconnect serializer + generation counter. Cloned into
    /// every `McpTool` from that server so concurrent failures dedup
    /// across the whole agent — and survive `collect_tools` being
    /// called multiple times during a session (M-R2 fix).
    reconnect_locks: HashMap<String, Arc<Mutex<u64>>>,
    /// Original configs retained so a disconnected server can be
    /// reconnected later via [`reconnect`] (manual `/mcp reconnect`) OR
    /// the tool-side auto-reconnect path (audit H15).
    configs: HashMap<String, config::McpServerConfig>,
}

impl McpClientManager {
    pub async fn connect_all(configs: &HashMap<String, config::McpServerConfig>) -> Self {
        // Connect to every server CONCURRENTLY. This loop used to await
        // each `client::connect` in turn, so startup paid the SUM of every
        // server's spin-up before the first frame could draw — and each
        // can be seconds (an `npx -y <pkg>` cold start) with a 10s init
        // timeout on top. Running them together means startup waits only
        // for the SLOWEST server instead of all of them in series, which
        // is the dominant contributor to time-to-first-frame (dirge-lvag).
        //
        // `join_all` preserves input order, and the result-handling below
        // stays a sequential pass, so log/insert order and the
        // skip-failed-server behaviour are unchanged — only the network
        // waits overlap.
        let connect_results = futures::future::join_all(configs.iter().map(|(name, cfg)| {
            let name = name.clone();
            async move {
                let result = client::connect(name.clone(), cfg).await;
                (name, result)
            }
        }))
        .await;

        let mut connections = HashMap::new();
        let mut reconnect_locks = HashMap::new();
        for (name, result) in connect_results {
            match result {
                Ok(conn) => {
                    tracing::info!("Connected to MCP server '{}'", name);
                    connections.insert(name.clone(), conn);
                    reconnect_locks.insert(name, Arc::new(Mutex::new(0u64)));
                }
                Err(e) => {
                    // ALSO emit to stderr so users running without
                    // RUST_LOG / --verbose see that an MCP server
                    // failed to register. Without this, configured
                    // tools just silently never appear and the user
                    // has no idea why.
                    tracing::warn!("Failed to connect to MCP server '{}': {e}", name);
                    eprintln!(
                        "warning: MCP server '{}' failed to connect: {}; its tools won't be available this session",
                        name, e,
                    );
                }
            }
        }
        Self {
            connections,
            reconnect_locks,
            configs: configs.clone(),
        }
    }

    /// Reconnect a single MCP server by name using its original config.
    /// Updates the existing `SharedConnection` in place via `replace`,
    /// so every `McpTool` clone from that server picks up the new
    /// transport on its next call.
    ///
    /// Wired by `/mcp reconnect <name>` (UI slash) for the manual case.
    /// `McpTool` self-reconnects on its own via the same swap path
    /// on transport-class failures.
    #[allow(dead_code)]
    pub async fn reconnect(&mut self, name: &str) -> anyhow::Result<()> {
        let cfg = self.configs.get(name).cloned().ok_or_else(|| {
            anyhow::anyhow!("no config for MCP server '{name}' — was it registered at startup?")
        })?;
        let conn = self.connections.get(name).cloned();

        let (new_peer, new_rs) = client::raw_connect(name, &cfg)
            .await
            .map_err(|e| anyhow::anyhow!("reconnect to '{name}' failed: {e}"))?;

        if let Some(conn) = conn {
            // Swap into the existing shared container so previously-
            // handed-out McpTool clones see the new peer.
            conn.replace(new_peer, new_rs).await;
        } else {
            // No prior connection (server failed to start originally).
            // Create a fresh shared container + start a fresh
            // reconnect lock.
            let conn = Arc::new(client::SharedConnection::new(
                name.to_string(),
                new_peer,
                new_rs,
            ));
            self.connections.insert(name.to_string(), conn);
            self.reconnect_locks
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(0u64)));
        }
        Ok(())
    }

    pub async fn collect_tools(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Vec<McpTool> {
        let mut all_tools = Vec::new();
        for (server_name, conn) in &self.connections {
            let cfg = self.configs.get(server_name).cloned().map(Arc::new);
            // Reconnect lock from the manager's persistent map. Cloning
            // the Arc bumps the refcount; every McpTool from this
            // server (across this AND any future collect_tools call)
            // shares one canonical lock + gen counter.
            let reconnect_lock = self
                .reconnect_locks
                .get(server_name)
                .cloned()
                .unwrap_or_else(|| Arc::new(Mutex::new(0u64)));
            match client::list_tools(conn).await {
                Ok(tools) => {
                    for definition in tools {
                        all_tools.push(McpTool {
                            server_name: server_name.clone(),
                            definition,
                            connection: Arc::clone(conn),
                            config: cfg.clone(),
                            reconnect_lock: reconnect_lock.clone(),
                            permission: permission.clone(),
                            ask_tx: ask_tx.clone(),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to list tools from MCP server '{}': {e}",
                        server_name,
                    );
                    eprintln!(
                        "warning: MCP server '{}' connected but list_tools failed: {}; \
                         its tools won't be available this session",
                        server_name, e,
                    );
                }
            }
        }
        all_tools
    }

    /// Snapshot the current set of (server_name, shared_connection)
    /// pairs. Cheap — clones an `Arc` per server. Used by the
    /// `/mcp` slash command and the info panel to enumerate the
    /// live connections without holding any lock across the await
    /// points that follow (e.g. `list_tools`).
    pub fn connections_snapshot(&self) -> Vec<(String, Arc<client::SharedConnection>)> {
        self.connections
            .iter()
            .map(|(name, conn)| (name.clone(), Arc::clone(conn)))
            .collect()
    }

    pub async fn shutdown(self) {
        for (name, conn) in self.connections {
            conn.shutdown().await;
            tracing::debug!("Disconnected from MCP server '{}'", name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn bogus_server() -> config::McpServerConfig {
        // A command that can't spawn → `connect` returns Err fast (no
        // 10s timeout), letting us exercise connect_all without a real
        // MCP server.
        config::McpServerConfig::Command {
            command: "dirge-nonexistent-mcp-binary".to_string(),
            args: vec![],
            env: HashMap::new(),
            allow_external_paths: false,
        }
    }

    /// dirge-lvag: parallelizing connect_all must preserve the
    /// skip-failed-server contract — a server that fails to connect is
    /// dropped (no live connection) but its config is retained so it can
    /// still be `/mcp reconnect`-ed, and the other servers are unaffected.
    #[tokio::test]
    async fn connect_all_skips_failed_servers_and_retains_configs() {
        let mut configs = HashMap::new();
        configs.insert("bogus-a".to_string(), bogus_server());
        configs.insert("bogus-b".to_string(), bogus_server());

        let mgr = McpClientManager::connect_all(&configs).await;

        // Both failed → no live connections, but every config is kept
        // (the manager is the source of truth for later reconnects).
        assert_eq!(mgr.connections.len(), 0, "failed servers must not register");
        assert_eq!(mgr.reconnect_locks.len(), 0);
        assert_eq!(mgr.configs.len(), 2, "configs retained for /mcp reconnect");
        assert!(mgr.connections_snapshot().is_empty());
    }

    /// Empty config set → an empty, well-formed manager (no panic, no
    /// stray entries).
    #[tokio::test]
    async fn connect_all_empty_config_is_empty_manager() {
        let mgr = McpClientManager::connect_all(&HashMap::new()).await;
        assert!(mgr.connections.is_empty());
        assert!(mgr.configs.is_empty());
    }
}
