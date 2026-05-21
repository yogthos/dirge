pub mod client;
pub mod config;
pub mod tool;

use std::collections::HashMap;

use tool::McpTool;

use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

pub struct McpClientManager {
    pub handles: Vec<client::McpClientHandle>,
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
        Self { handles }
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
