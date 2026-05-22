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
    /// Per-provider override for the streaming chunk timeout. Same
    /// units / semantics as the top-level `stream_chunk_timeout_secs`
    /// but takes precedence for this specific provider. Useful when
    /// one custom endpoint (e.g. a self-hosted reasoning model) needs
    /// a generous gap and others should fail faster.
    pub stream_chunk_timeout_secs: Option<u64>,
}

/// Per-provider tuning knobs that apply to a built-in provider name
/// (anthropic, openai, openrouter, gemini, deepseek, glm, ollama).
/// Keyed by `providers.<name>` in config.json. Currently carries
/// only the chunk timeout; expand as more per-provider knobs land.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderSettings {
    /// Override the streaming chunk timeout for this provider.
    /// Precedence: this > top-level `stream_chunk_timeout_secs` >
    /// default 300s. Useful e.g. for Anthropic extended-thinking
    /// runs that legitimately exceed the 5-minute default.
    pub stream_chunk_timeout_secs: Option<u64>,
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

#[cfg(feature = "lsp")]
impl crate::lsp::server::AsExtensionOverride for LspServerConfig {
    fn extensions(&self) -> Option<&[String]> {
        self.extensions.as_deref()
    }
    fn disabled(&self) -> bool {
        self.disabled.unwrap_or(false)
    }
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
    /// Per-built-in-provider tuning. Keyed by provider name
    /// (`anthropic`, `openai`, `openrouter`, `gemini`, `deepseek`,
    /// `glm`, `ollama`). Currently only the chunk timeout; expand
    /// as more knobs land.
    pub providers: Option<HashMap<String, ProviderSettings>>,
    pub permission: Option<serde_json::Value>,
    pub restrictive: Option<bool>,
    pub accept_all: Option<bool>,
    pub yolo: Option<bool>,
    pub sandbox: Option<bool>,
    pub default_permission_mode: Option<String>,
    pub show_tool_details: Option<bool>,
    pub show_edit_diff: Option<bool>,
    pub tool_result_max_chars: Option<usize>,
    /// Cap on tool-result body lines shown by default inside a tool
    /// chamber. Anything past this collapses to a
    /// `↓ N more lines (Ctrl+O to expand)` footer, and the user can
    /// re-print the most recent collapsed result in full via Ctrl+O.
    /// `tool_result_max_chars` still applies on top as a hard
    /// character ceiling for the displayed slice.
    pub tool_result_max_lines: Option<usize>,
    /// Per-chunk read deadline for streaming LLM responses, in seconds.
    /// Default 300s (5 min). Bump higher (600–900) if you use models
    /// with very long reasoning budgets (Claude 3.7 extended thinking,
    /// GPT-5 thinking, etc.) and see false-positive "stream chunk timed
    /// out" errors mid-turn. Set lower if you want faster failure
    /// detection on flaky networks; below ~60s is risky on reasoning
    /// models.
    pub stream_chunk_timeout_secs: Option<u64>,
    pub default_prompt: Option<String>,
    /// UI color theme. Known built-in values: `phosphor` (default,
    /// 80s CRT green) and `plain` (white/cyan).
    ///
    /// Any other value looks for a custom theme file at
    /// `~/.config/dirge/<theme>.theme.json` — see the
    /// `ui::theme` module for the JSON format. Fields not in the
    /// file inherit from the phosphor preset so minimal overrides
    /// work (e.g. just `{"accent": "magenta"}`).
    ///
    /// If neither the built-in name nor the file matches, dirge
    /// falls back to phosphor with a warning rather than refusing
    /// to start.
    pub theme: Option<String>,
    pub tools: Option<ToolsConfig>,
    #[cfg(feature = "lsp")]
    pub lsp: Option<LspConfig>,
    #[cfg(feature = "mcp")]
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,

    /// ACP server config map when compiled with the `acp` feature.
    /// Used by the editor-integration server; dirge's ACP transport
    /// is stdio-only — the TCP / Unix-socket forms live here for
    /// future expansion but are not honored today.
    #[cfg(feature = "acp")]
    pub acp_servers: Option<HashMap<String, AcpServerConfig>>,
}

impl Config {
    pub fn custom_providers_map(&self) -> HashMap<String, CustomProviderConfig> {
        self.custom_providers.clone().unwrap_or_default()
    }

    /// Resolve the context window for the active model. Precedence:
    ///   1. explicit `context_window` in config.json
    ///   2. per-model static table (`context_window_for_model`)
    ///   3. 128_000 fallback
    ///
    /// `model` is the resolved model id (after CLI / config / default
    /// resolution). Passing an empty string falls through to (3).
    pub fn resolve_context_window(&self, model: &str) -> u64 {
        if let Some(v) = self.context_window {
            return v;
        }
        context_window_for_model(model).unwrap_or(128_000)
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

    pub fn resolve_tool_result_max_lines(&self) -> usize {
        self.tool_result_max_lines.unwrap_or(4)
    }

    /// Resolve the chunk timeout for the active provider.
    ///
    /// Precedence:
    ///   1. `custom_providers[name].stream_chunk_timeout_secs`
    ///   2. `providers[name].stream_chunk_timeout_secs`
    ///   3. top-level `stream_chunk_timeout_secs`
    ///   4. `DEFAULT_STREAM_CHUNK_TIMEOUT_SECS` (300s)
    ///
    /// Passing an unknown / empty provider name falls through past
    /// (1) and (2) to the top-level / default.
    pub fn resolve_stream_chunk_timeout(&self, provider: &str) -> std::time::Duration {
        // Provider lookup is case-insensitive because `parse_provider`
        // accepts `--provider Anthropic` (#2 fix). Without this, a
        // capitalized CLI / config provider name built the client
        // fine but missed the `providers.anthropic` override silently.
        let lower = provider.to_ascii_lowercase();
        let from_custom = self
            .custom_providers
            .as_ref()
            .and_then(|m| m.get(&lower).or_else(|| m.get(provider)))
            .and_then(|c| c.stream_chunk_timeout_secs);
        let from_provider = self
            .providers
            .as_ref()
            .and_then(|m| m.get(&lower).or_else(|| m.get(provider)))
            .and_then(|p| p.stream_chunk_timeout_secs);
        let secs = from_custom
            .or(from_provider)
            .or(self.stream_chunk_timeout_secs)
            .unwrap_or(crate::agent::runner::DEFAULT_STREAM_CHUNK_TIMEOUT_SECS);
        std::time::Duration::from_secs(secs)
    }

    pub fn resolve_show_edit_diff(&self) -> bool {
        self.show_edit_diff.unwrap_or(true)
    }
}

/// Static per-model context-window table. Returns `None` for unknown
/// models so callers can fall back to a sane default. Matched by
/// case-insensitive substring so a provider-prefixed or
/// version-suffixed id (`openai/gpt-4o`, `claude-3.5-sonnet-20241022`,
/// `deepseek-v4-pro`) still hits the right family. Order matters:
/// the FIRST matching prefix wins — list longer / more-specific
/// keys first.
///
/// Values are the model's documented maximum context (input + output
/// combined where the provider quotes a unified figure). Update as
/// providers extend their context budgets.
pub fn context_window_for_model(model: &str) -> Option<u64> {
    let m = model.to_lowercase();
    // Ordered: most-specific first.
    const TABLE: &[(&str, u64)] = &[
        // DeepSeek
        ("deepseek-v4", 1_000_000),
        ("deepseek-r1", 128_000),
        ("deepseek", 128_000),
        // GLM / ZhipuAI
        ("glm-4.6", 200_000),
        ("glm-4.5", 128_000),
        ("glm-4", 128_000),
        // Anthropic Claude
        ("claude-opus-4-5", 1_000_000),
        ("claude-opus-4-7", 1_000_000),
        ("claude-sonnet-4-5", 1_000_000),
        ("claude-sonnet-4-6", 1_000_000),
        ("claude-opus", 200_000),
        ("claude-sonnet", 200_000),
        ("claude-haiku", 200_000),
        ("claude-3-7", 200_000),
        ("claude-3.5", 200_000),
        ("claude-3", 200_000),
        ("claude", 200_000),
        // OpenAI GPT
        ("gpt-5", 400_000),
        ("gpt-4.1", 1_000_000),
        ("gpt-4o", 128_000),
        ("gpt-4-turbo", 128_000),
        ("gpt-4", 128_000),
        ("o3", 200_000),
        ("o1", 200_000),
        // Google Gemini
        ("gemini-2.0-flash-thinking", 32_000),
        ("gemini-2.5-pro", 2_000_000),
        ("gemini-2.5-flash", 1_000_000),
        ("gemini-2.0-pro", 2_000_000),
        ("gemini-2.0-flash", 1_000_000),
        ("gemini-1.5-pro", 2_000_000),
        ("gemini-1.5-flash", 1_000_000),
        ("gemini-pro", 128_000),
        ("gemini", 128_000),
        // Meta / Llama (via OpenRouter and others)
        ("llama-4", 1_000_000),
        ("llama-3.3", 128_000),
        ("llama-3.1", 128_000),
        ("llama-3", 8_000),
        // Mistral
        ("mistral-large", 128_000),
        ("mistral", 32_000),
        // Qwen
        ("qwen2.5", 128_000),
        ("qwen", 32_000),
    ];
    for (key, window) in TABLE {
        if m.contains(key) {
            return Some(*window);
        }
    }
    None
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

    // Validate `custom_providers` at load time so a typo in
    // `provider_type` surfaces immediately instead of failing at
    // first agent call with a cryptic "unknown provider" deep in the
    // call stack.
    if let Some(providers) = cfg.custom_providers.as_ref() {
        for (name, p) in providers {
            if crate::provider::parse_provider(&p.provider_type).is_none() {
                eprintln!(
                    "error: custom provider {:?} has invalid provider_type {:?}.\n\
                     Must be one of: openrouter, openai, anthropic, gemini,\n\
                     deepseek, glm, ollama, custom.",
                    name, p.provider_type,
                );
                std::process::exit(1);
            }
        }
    }

    #[cfg(feature = "mcp")]
    if cfg.mcp_servers.is_none() {
        // Only auto-register the Exa default when there's actually
        // a non-empty API key. An empty `EXA_API_KEY=""` (e.g. unset
        // via a `.envrc` that intentionally clears it) used to
        // register Exa anyway with an empty header, then every web-
        // search call failed with 401 at first use. Skip cleanly
        // when no usable key is present.
        match std::env::var("EXA_API_KEY") {
            Ok(key) if !key.is_empty() => {
                let mut headers = HashMap::new();
                headers.insert("x-api-key".to_string(), key);
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
            _ => {
                // Key unset or empty — leave mcp_servers as None so
                // the host knows there's nothing to connect to.
            }
        }
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

#[cfg(test)]
mod model_context_tests {
    use super::*;

    /// Per-model table maps common provider/version-prefixed ids to
    /// their published context windows.
    #[test]
    fn known_models_resolve_to_published_windows() {
        for (model, want) in &[
            ("deepseek-v4-pro", 1_000_000),
            ("deepseek/deepseek-v4-flash", 1_000_000),
            ("claude-opus-4-7", 1_000_000),
            ("claude-sonnet-4-6", 1_000_000),
            ("claude-3.5-sonnet-20241022", 200_000),
            ("openai/gpt-4o", 128_000),
            ("gpt-5", 400_000),
            ("gemini-2.5-pro", 2_000_000),
            ("gemini-1.5-flash-002", 1_000_000),
            ("glm-4.6", 200_000),
        ] {
            let got = context_window_for_model(model);
            assert_eq!(
                got,
                Some(*want),
                "model {model} expected {want}, got {got:?}",
            );
        }
    }

    /// Unknown models return `None` so the caller falls back to the
    /// 128k default.
    #[test]
    fn unknown_model_returns_none() {
        assert!(context_window_for_model("totally-fictional-model").is_none());
        assert!(context_window_for_model("").is_none());
    }

    /// Match is case-insensitive — provider ids that uppercase
    /// product names still hit the table.
    #[test]
    fn model_match_is_case_insensitive() {
        assert_eq!(context_window_for_model("Claude-Opus-4-7"), Some(1_000_000));
        assert_eq!(context_window_for_model("DEEPSEEK-V4-PRO"), Some(1_000_000));
    }

    /// Explicit `context_window` in config wins over the model table.
    #[test]
    fn explicit_config_overrides_model_table() {
        let cfg = Config {
            context_window: Some(50_000),
            ..Default::default()
        };
        // deepseek would normally resolve to 1M.
        assert_eq!(cfg.resolve_context_window("deepseek-v4-pro"), 50_000);
    }

    /// Default fallback (no explicit config, unknown model) = 128k.
    #[test]
    fn fallback_default_is_128k() {
        let cfg = Config::default();
        assert_eq!(cfg.resolve_context_window("unknown-model-9000"), 128_000);
    }
}
