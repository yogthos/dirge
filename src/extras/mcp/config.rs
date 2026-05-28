use std::collections::HashMap;

use serde::Deserialize;

/// Per-server MCP configuration. Either a stdio command (`command` +
/// `args` + `env`) or a remote URL (`url` + `headers`).
///
/// Both variants accept `allow_external_paths: bool` (default `false`):
/// when set, MCP tool calls from this server bypass the cwd-external-
/// path guard. Other permission rules (the `mcp_tool` rule table,
/// prompt `deny_tools`, doom-loop detection, etc.) still apply — this
/// flag ONLY toggles the path-outside-cwd refusal for tools whose JSON
/// arguments name absolute or relative paths that resolve outside the
/// working directory. Intended for semantic indexers, project-wide
/// search tools, or any MCP server whose legitimate scope is broader
/// than the current project.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    Command {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        allow_external_paths: bool,
    },
    Url {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        allow_external_paths: bool,
    },
}

impl McpServerConfig {
    /// Whether this server is configured to bypass the cwd-external-
    /// path guard. Defaults to `false` for both variants.
    pub fn allow_external_paths(&self) -> bool {
        match self {
            McpServerConfig::Command {
                allow_external_paths,
                ..
            }
            | McpServerConfig::Url {
                allow_external_paths,
                ..
            } => *allow_external_paths,
        }
    }
}
