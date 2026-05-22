pub mod client;
pub mod config;
pub mod tool;

use std::collections::HashMap;

use tool::McpTool;

use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

pub struct McpClientManager {
    pub handles: Vec<client::McpClientHandle>,
    /// Original configs retained so a disconnected server can be
    /// reconnected later via [`reconnect`]. Without this, a
    /// transport that dies mid-session was dead for the rest of the
    /// session — there was nowhere to look up the original
    /// command/args/env to respawn (audit H15). Auto-retry-on-tool-
    /// failure is deferred (requires sharing a swappable Peer across
    /// already-handed-out McpTool instances); this enables manual
    /// recovery in the meantime.
    configs: HashMap<String, config::McpServerConfig>,
}

impl McpClientManager {
    pub async fn connect_all(configs: &HashMap<String, config::McpServerConfig>) -> Self {
        let mut handles = Vec::new();
        for (name, cfg) in configs {
            match client::McpClientHandle::connect(name.clone(), cfg).await {
                Ok(handle) => {
                    tracing::info!("Connected to MCP server '{}'", name);
                    handles.push(handle);
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
            handles,
            configs: configs.clone(),
        }
    }

    /// Reconnect a single MCP server by name using its original
    /// config. Replaces any existing handle for that server. Returns
    /// Err if the server isn't in the manager's config map or the
    /// fresh connect attempt fails. McpTool instances already handed
    /// out hold a `Peer<RoleClient>` clone that will continue
    /// pointing at the dead transport — they need to be rebuilt
    /// via a fresh `collect_tools` call after a successful
    /// reconnect.
    ///
    /// Wired by `/mcp reconnect <name>` (UI slash); auto-reconnect on
    /// tool-call failure is a follow-up that requires sharing a
    /// swappable Peer across handed-out McpTool instances.
    #[allow(dead_code)]
    pub async fn reconnect(&mut self, name: &str) -> anyhow::Result<()> {
        let cfg = self.configs.get(name).cloned().ok_or_else(|| {
            anyhow::anyhow!("no config for MCP server '{name}' — was it registered at startup?")
        })?;
        self.handles.retain(|h| h.server_name != name);
        let handle = client::McpClientHandle::connect(name.to_string(), &cfg)
            .await
            .map_err(|e| anyhow::anyhow!("reconnect to '{name}' failed: {e}"))?;
        self.handles.push(handle);
        Ok(())
    }

    pub async fn collect_tools(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Vec<McpTool> {
        let mut all_tools = Vec::new();
        for handle in &self.handles {
            let peer = handle.peer();
            let server_name = handle.server_name.clone();
            match handle.list_tools().await {
                Ok(tools) => {
                    for definition in tools {
                        all_tools.push(McpTool {
                            server_name: server_name.clone(),
                            definition,
                            peer: peer.clone(),
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

    pub async fn shutdown(self) {
        for handle in self.handles {
            let name = handle.server_name.clone();
            drop(handle);
            tracing::debug!("Disconnected from MCP server '{}'", name);
        }
    }
}
