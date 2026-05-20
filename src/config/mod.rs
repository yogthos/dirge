use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::session::storage;

#[cfg(feature = "mcp")]
use crate::extras::mcp::config::McpServerConfig;

#[cfg(feature = "acp")]
use crate::extras::acp::config::AcpServerConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct CustomProviderConfig {
    pub provider_type: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub websearch: Option<bool>,
    pub webfetch: Option<bool>,
}

/// Per-server LSP configuration. All fields optional — unspecified fields
/// fall back to the built-in defaults for the given `server_id`.
///
/// Two forms are accepted:
/// - `{ "disabled": true }` to turn off a built-in server entirely.
/// - any subset of `{ command, extensions, env, initialization, disabled }`
///   to override pieces of the default.
#[cfg(feature = "lsp")]
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct LspServerConfig {
    pub command: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub initialization: Option<serde_json::Value>,
    pub disabled: Option<bool>,
}

/// `lsp = true`  → enable built-in servers with default commands.
/// `lsp = false` → disable LSP entirely.
/// `lsp = { server-id = { … } }` → enable defaults, overriding the named
///   servers with the provided config.
#[cfg(feature = "lsp")]
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum LspConfig {
    Enabled(bool),
    Servers(HashMap<String, LspServerConfig>),
}

#[cfg(feature = "lsp")]
impl LspConfig {
    /// `true` when LSP should be on. Defaults to enabled.
    pub fn is_enabled(&self) -> bool {
        match self {
            LspConfig::Enabled(b) => *b,
            LspConfig::Servers(_) => true,
        }
    }

    /// Per-server overrides keyed by server id. Empty when LSP is a bool.
    pub fn server_overrides(&self) -> &HashMap<String, LspServerConfig> {
        match self {
            LspConfig::Enabled(_) => {
                // Empty borrow without allocating per-call.
                static EMPTY: std::sync::OnceLock<HashMap<String, LspServerConfig>> =
                    std::sync::OnceLock::new();
                EMPTY.get_or_init(HashMap::new)
            }
            LspConfig::Servers(map) => map,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub no_tools: Option<bool>,
    pub no_context_files: Option<bool>,
    pub context_window: Option<u64>,
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
    pub max_agent_turns: Option<usize>,
    pub compact_enabled: Option<bool>,
    pub custom_providers: Option<HashMap<String, CustomProviderConfig>>,
    pub permission: Option<serde_json::Value>,
    pub restrictive: Option<bool>,
    pub accept_all: Option<bool>,
    pub yolo: Option<bool>,
    pub sandbox: Option<bool>,
    pub default_permission_mode: Option<String>,
    pub show_tool_details: Option<bool>,
    pub show_edit_diff: Option<bool>,
    pub tool_result_max_chars: Option<usize>,
    pub default_prompt: Option<String>,
    /// UI color theme. Known values: `phosphor` (default, 80s CRT
    /// green) and `plain` (the pre-theme white/cyan look). Unknown
    /// values fall back to `phosphor` with a warning.
    pub theme: Option<String>,
    pub tools: Option<ToolsConfig>,
    #[cfg(feature = "lsp")]
    pub lsp: Option<LspConfig>,
    #[cfg(feature = "mcp")]
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,

    #[cfg(feature = "acp")]
    pub acp_servers: Option<HashMap<String, AcpServerConfig>>,
    #[cfg(feature = "acp")]
    pub acp_host: Option<String>,
    #[cfg(feature = "acp")]
    pub acp_port: Option<u16>,
}

impl Config {
    pub fn custom_providers_map(&self) -> HashMap<String, CustomProviderConfig> {
        self.custom_providers.clone().unwrap_or_default()
    }

    pub fn resolve_context_window(&self) -> u64 {
        self.context_window.unwrap_or(128_000)
    }

    pub fn resolve_reserve_tokens(&self) -> u64 {
        self.reserve_tokens.unwrap_or(16_384)
    }

    pub fn resolve_keep_recent_tokens(&self) -> u64 {
        self.keep_recent_tokens.unwrap_or(20_000)
    }

    pub fn resolve_compact_enabled(&self) -> bool {
        self.compact_enabled.unwrap_or(true)
    }

    pub fn resolve_tool_result_max_chars(&self) -> usize {
        self.tool_result_max_chars.unwrap_or(500)
    }

    pub fn resolve_show_edit_diff(&self) -> bool {
        self.show_edit_diff.unwrap_or(true)
    }
}

pub fn config_file_path() -> PathBuf {
    storage::config_path().join("config.json")
}

pub fn load() -> Config {
    let path = config_file_path();
    #[allow(unused_mut)]
    let mut cfg: Config = if !path.exists() {
        Config::default()
    } else {
        let content = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            eprintln!(
                "error: failed to read config file ({}): {}\n\
                 Fix the file or remove it to use defaults.",
                path.display(),
                e,
            );
            std::process::exit(1);
        });
        serde_json::from_str(&content).unwrap_or_else(|e| {
            eprintln!(
                "error: {} is not a valid config: {}\n\
                 Fix the file or remove it to use defaults.",
                path.display(),
                e,
            );
            std::process::exit(1);
        })
    };

    #[cfg(feature = "mcp")]
    if cfg.mcp_servers.is_none() {
        let mut headers = HashMap::new();
        if let Some(key) = std::env::var("EXA_API_KEY").ok() {
            headers.insert("x-api-key".to_string(), key);
        }
        let mut defaults = HashMap::new();
        defaults.insert(
            "Exa Web Search".to_string(),
            McpServerConfig::Url {
                url: "https://mcp.exa.ai/mcp".to_string(),
                headers,
            },
        );
        cfg.mcp_servers = Some(defaults);
    }

    cfg
}

#[cfg(all(test, feature = "lsp"))]
mod tests {
    use super::*;

    #[test]
    fn lsp_config_parses_as_bool() {
        let cfg: Config = serde_json::from_str(r#"{"lsp": true}"#).unwrap();
        assert!(cfg.lsp.unwrap().is_enabled());

        let cfg: Config = serde_json::from_str(r#"{"lsp": false}"#).unwrap();
        assert!(!cfg.lsp.unwrap().is_enabled());
    }

    #[test]
    fn lsp_config_parses_as_per_server_map() {
        let raw = r#"{
            "lsp": {
                "rust": { "command": ["my-rust-analyzer", "--my-arg"] },
                "typescript": { "disabled": true }
            }
        }"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let overrides = cfg.lsp.as_ref().unwrap().server_overrides();
        assert_eq!(overrides.len(), 2);
        assert_eq!(
            overrides["rust"].command.as_ref().unwrap(),
            &vec!["my-rust-analyzer".to_string(), "--my-arg".to_string()]
        );
        assert_eq!(overrides["typescript"].disabled, Some(true));
    }

    // Regression: when lsp is omitted entirely, default is "enabled with
    // built-in commands" — the CLI's resolve_lsp_enabled handles that.
    // Config-side, an absent value parses to `None`.
    #[test]
    fn absent_lsp_config_is_none() {
        let cfg: Config = serde_json::from_str(r#"{"model": "foo"}"#).unwrap();
        assert!(cfg.lsp.is_none());
    }

    // Regression: a config that mixes overrides for valid server ids
    // (rust) with disabled-only entries (typescript) must parse cleanly.
    #[test]
    fn lsp_config_mixes_command_and_disabled_entries() {
        let raw = r#"{
            "lsp": {
                "rust": { "command": ["rust-analyzer"], "env": {"RUST_LOG": "info"} },
                "typescript": { "disabled": true }
            }
        }"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let overrides = cfg.lsp.as_ref().unwrap().server_overrides();
        assert!(overrides["rust"].command.is_some());
        assert_eq!(
            overrides["rust"]
                .env
                .as_ref()
                .unwrap()
                .get("RUST_LOG")
                .unwrap(),
            "info"
        );
        assert_eq!(overrides["typescript"].disabled, Some(true));
    }
}
