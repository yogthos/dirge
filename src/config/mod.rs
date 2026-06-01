use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::session::storage;

#[cfg(feature = "mcp")]
use crate::extras::mcp::config::McpServerConfig;

#[cfg(feature = "acp")]
use crate::extras::acp::config::AcpServerConfig;

/// Unified provider declaration. One entry per alias in
/// `config.providers`. The map KEY is the alias the rest of the
/// config (and `provider`, `review_provider`, etc.) refers to.
///
/// `provider_type` is optional: when the alias matches a built-in
/// (anthropic, deepseek, glm, openai, openrouter, gemini, ollama),
/// it's inferred from the key. Set it explicitly only when aliasing
/// a built-in backend under a different name — e.g.
/// `"ollama": { "provider_type": "openai", "base_url": "..." }`
/// aliases the OpenAI-compatible backend under the alias `ollama`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderEntry {
    pub provider_type: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// Name of the env var holding the API key. Kept for backward
    /// compatibility — prefer `api_key` with `${VAR}` interpolation
    /// for clarity.
    pub api_key_env: Option<String>,
    /// API key for this provider. Accepts a literal key OR shell-style
    /// `${ENV_VAR}` interpolation (expanded at use time). Takes
    /// precedence over `api_key_env`. Accepts both `api_key` and
    /// `apiKey` in the JSON.
    #[serde(alias = "apiKey")]
    pub api_key: Option<String>,
    /// Set to true to allow `http://` URLs (insecure). Default false —
    /// only `https://` is accepted. Non-https endpoints send every
    /// prompt, file content, and tool result in plaintext over the
    /// network. Only enable for local-only proxies (ollama, vllm, etc.)
    /// that are NOT reachable from other hosts.
    pub allow_insecure: bool,
    /// Per-provider override for the streaming chunk timeout. Same
    /// units / semantics as the top-level `stream_chunk_timeout_secs`
    /// but takes precedence for this specific provider.
    pub stream_chunk_timeout_secs: Option<u64>,
    /// Per-provider model options. Free-form map; known keys are
    /// honored by the request builder, unknown keys are ignored.
    /// Currently honored: `temperature` (f64, overrides cfg/CLI for
    /// requests routed through this provider).
    pub options: Option<serde_json::Map<String, serde_json::Value>>,
}

impl ProviderEntry {
    /// Resolve the API key declared on this entry, expanding
    /// `${VAR}` interpolation against the process environment.
    /// Returns:
    /// - `Some(Ok(key))` when a literal or successfully-expanded key is available
    /// - `Some(Err(missing_var))` when `${VAR}` is configured but the env var is unset
    /// - `None` when no `api_key` is configured on the entry
    pub fn resolved_api_key(&self) -> Option<Result<String, String>> {
        let raw = self.api_key.as_deref()?;
        if let Some(name) = raw.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
            match std::env::var(name) {
                Ok(v) if !v.is_empty() => Some(Ok(v)),
                _ => Some(Err(name.to_string())),
            }
        } else {
            Some(Ok(raw.to_string()))
        }
    }

    /// `options.temperature` as an f64 when set. Other shapes (string,
    /// integer, missing) return `None`.
    pub fn options_temperature(&self) -> Option<f64> {
        self.options.as_ref()?.get("temperature")?.as_f64()
    }
}

/// Logical role a provider can be assigned to. Used by
/// `Config::resolve_role` to look up the named provider for that
/// role (and fall back to the default for non-default roles).
///
/// `Review`, `Escalation`, `Summarization`, and `Subagent` are
/// declared for the unified role-routing surface; the
/// corresponding call-sites (background review, Phase 4
/// escalation, compaction summarizer, `task` subagent) wire up in
/// follow-up commits. They're tested today via the role
/// resolver but not yet referenced from a runtime path, so
/// `#[allow(dead_code)]` keeps the warning quiet for the
/// config-only PR.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum ConfigRole {
    Default,
    Review,
    Escalation,
    Summarization,
    Subagent,
    Critic,
    Approval,
}

/// One VSCode-style key binding: bind a key chord to a named command.
/// `key` is a chord like `"ctrl-t"` / `"pageup"` / `"ctrl-shift-x"`;
/// `command` is one of the rebindable global commands (see
/// `ui::keymap::KeyAction`), or `"none"` to unbind the default on that
/// chord. Parsed by `ui::keymap::Keymap::from_config`.
#[derive(Debug, Clone, Deserialize)]
pub struct KeybindingConfig {
    pub key: String,
    pub command: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub websearch: Option<bool>,
    pub webfetch: Option<bool>,
    /// Phase 3 / part 2: inline output budget for the `bash`
    /// tool. Output at-or-below this size (AND ≤200 lines) is
    /// returned verbatim; anything above is written to
    /// `~/.dirge/transient/<pid>/bash-<unix_ts>.txt` and a head/
    /// tail summary is returned to the model along with a hint
    /// telling it to use the `read` tool to inspect specific
    /// portions. Default 8 KiB. Set to a huge number to disable
    /// the relay; set lower to keep more turns inline-summarized.
    pub bash_output_inline_max_bytes: Option<usize>,
    /// As above but for the `webfetch` tool. Default 8 KiB. The
    /// 10 MiB streaming body cap inside `webfetch` itself is
    /// independent and stays as the in-memory ceiling.
    pub webfetch_output_inline_max_bytes: Option<usize>,
    /// dirge-nmv5: inline output budget for the `task` subagent
    /// tool. Subagent answers larger than this are relayed to
    /// `~/.dirge/transient/<pid>/task-<unix_ts>.txt` and the parent
    /// agent receives a head/tail summary + a `read`-tool hint to
    /// fetch the full payload. Default 8 KiB. Replaces the legacy
    /// 3000-char hard truncation that silently dropped the tail of
    /// large subagent answers.
    pub task_output_inline_max_bytes: Option<usize>,
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
    /// Extensions to ADD to the server's built-in list (additive — does
    /// not replace). e.g. `"extend_extensions": ["janet"]` on
    /// `clojure-lsp` keeps clj/cljs/… and also routes `.janet` files to
    /// it. Accepts `extendExtensions` too.
    #[serde(alias = "extendExtensions")]
    pub extend_extensions: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub initialization: Option<serde_json::Value>,
    pub disabled: Option<bool>,
}

#[cfg(feature = "lsp")]
impl crate::lsp::server::AsExtensionOverride for LspServerConfig {
    fn extensions(&self) -> Option<&[String]> {
        self.extensions.as_deref()
    }
    fn extend_extensions(&self) -> Option<&[String]> {
        self.extend_extensions.as_deref()
    }
    fn disabled(&self) -> bool {
        self.disabled.unwrap_or(false)
    }
}

/// Per-plugin settings under the config `plugins` object, keyed by plugin
/// name (the directory name or the `.janet` file stem under a plugin search
/// dir). Both fields default to "unset"; the host treats that as
/// enabled + not auto-started, so existing setups load every plugin as
/// before.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PluginSettings {
    /// Load this plugin? Default true. `false` skips loading it entirely.
    pub enabled: Option<bool>,
    /// Passed to the plugin (via `harness/plugin-config`) so it can
    /// self-engage at startup instead of waiting for a trigger. Plugin-
    /// specific: e.g. `backpressured` engages its loop when this is true.
    pub auto_start: Option<bool>,
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
    /// Unified provider map. Keyed by alias; the alias is what
    /// `provider` / `review_provider` / `escalation_provider` /
    /// `summarization_provider` / `subagent_provider` reference.
    /// Each entry's `provider_type` defaults to the alias key
    /// when omitted.
    pub providers: Option<HashMap<String, ProviderEntry>>,
    /// Per-plugin settings, keyed by plugin name (the directory name or
    /// the `.janet` file stem under a plugin search dir). Absent entry =
    /// enabled, not auto-started (backward compatible).
    pub plugins: Option<HashMap<String, PluginSettings>>,
    pub permission: Option<serde_json::Value>,
    pub restrictive: Option<bool>,
    pub accept_all: Option<bool>,
    pub yolo: Option<bool>,
    pub sandbox: Option<bool>,
    pub default_permission_mode: Option<String>,
    pub show_tool_details: Option<bool>,
    pub show_edit_diff: Option<bool>,
    /// Preferred default pane layout for the TUI: a `|`/`,`/space-
    /// separated subset of `left`, `main`, `right` (e.g.
    /// `"left|main|right"`, `"main"`, `"main|right"`). The main pane is
    /// always shown; this picks which side panels appear at startup. The
    /// `/display` command overrides it at runtime. Absent → both side
    /// panels follow the automatic width-based behavior.
    pub display: Option<String>,
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
    /// Optional provider to use for background review at session end.
    /// When not set, the review fork reuses the main session's provider.
    pub review_provider: Option<String>,
    /// Optional provider for escalation (Phase 4 future hook).
    pub escalation_provider: Option<String>,
    /// Optional provider for context summarization / compaction.
    pub summarization_provider: Option<String>,
    /// Optional provider for sub-agents (`task` tool).
    pub subagent_provider: Option<String>,
    /// Optional provider for the F6 in-loop critic (tier 3). When set,
    /// the verifier escalates to a bounded LLM critique at finalization
    /// on substantive runs. Unset (default) = no critic, no cost.
    pub critic_provider: Option<String>,
    /// dirge-0g6i: optional provider for LLM auto-approval. When set, a
    /// permission prompt is routed to this model (with a safety prompt)
    /// which replies ALLOW/DENY instead of asking the human. Unset
    /// (default) = human prompts as usual. See docs/permissions.md.
    pub approval_provider: Option<String>,
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
    /// VSCode-style key-binding overrides for the global command keys.
    /// Each entry binds a chord to a command (see `KeybindingConfig`);
    /// applied over the built-in defaults by `ui::keymap`.
    pub keybindings: Option<Vec<KeybindingConfig>>,
    pub tools: Option<ToolsConfig>,

    /// Phase-3 (`docs/AGENTIC_LOOP_PLAN.md`): when true, ship only
    /// `tool_search` + a small always-on set in the per-turn tool
    /// defs, and let the model discover the rest via
    /// `tool_search(query)`. Default `false` — preserves the
    /// "ship every tool every turn" path. Useful on long sessions
    /// with MCP-heavy toolsets (≈30% token savings).
    pub dynamic_tool_search: Option<bool>,

    /// Phase 4 part 2 (`docs/AGENTIC_LOOP_PLAN.md`): consecutive-turn
    /// threshold for the context-depth reminder system. `None`
    /// (default) keeps the feature OFF — long sessions get no
    /// reminders. Recommended value: 8. Set lower for tighter
    /// re-focusing; higher to silence the reminder for routine
    /// multi-step refactors.
    pub context_depth_reminder_threshold: Option<usize>,
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
    /// Snapshot of the unified providers map. Empty when not set.
    pub fn providers_map(&self) -> HashMap<String, ProviderEntry> {
        self.providers.clone().unwrap_or_default()
    }

    /// Whether the plugin named `name` should be loaded. Default true —
    /// only an explicit `"enabled": false` skips it.
    pub fn plugin_enabled(&self, name: &str) -> bool {
        self.plugins
            .as_ref()
            .and_then(|m| m.get(name))
            .and_then(|s| s.enabled)
            .unwrap_or(true)
    }

    /// Whether the plugin named `name` requested auto-start. Default false.
    pub fn plugin_auto_start(&self, name: &str) -> bool {
        self.plugins
            .as_ref()
            .and_then(|m| m.get(name))
            .and_then(|s| s.auto_start)
            .unwrap_or(false)
    }

    /// Phase 4 part 2: resolve the context-depth reminder
    /// threshold. Trivially returns the field — encapsulated as a
    /// method so future callers don't see the `Option` directly
    /// and so we can add validation (e.g. clamp to >= 1) without
    /// changing every consumer.
    pub fn resolve_context_depth_threshold(&self) -> Option<usize> {
        // Clamp to a minimum of 2: a threshold of 0 or 1 would
        // emit a reminder on the very first tool call, which
        // defeats the purpose.
        self.context_depth_reminder_threshold.map(|t| t.max(2))
    }

    /// Resolve a logical role to `(alias, entry)`. For non-default
    /// roles, falls back to `self.provider` when no role-specific
    /// assignment is configured. Returns `None` only when neither
    /// the role nor the default provider names a present entry,
    /// AND the alias doesn't match a built-in.
    pub fn resolve_role(&self, role: ConfigRole) -> Option<(String, ProviderEntry)> {
        let providers = self.providers.as_ref();
        let role_name: Option<&str> = match role {
            ConfigRole::Default => self.provider.as_deref(),
            ConfigRole::Review => self.review_provider.as_deref().or(self.provider.as_deref()),
            ConfigRole::Escalation => self
                .escalation_provider
                .as_deref()
                .or(self.provider.as_deref()),
            ConfigRole::Summarization => self
                .summarization_provider
                .as_deref()
                .or(self.provider.as_deref()),
            ConfigRole::Subagent => self
                .subagent_provider
                .as_deref()
                .or(self.provider.as_deref()),
            // No fallback to the default provider: the critic is opt-in,
            // so it resolves only when `critic_provider` is explicitly set.
            ConfigRole::Critic => self.critic_provider.as_deref(),
            // Likewise opt-in: auto-approval resolves only when
            // `approval_provider` is explicitly set (no default fallback).
            ConfigRole::Approval => self.approval_provider.as_deref(),
        };
        let alias = role_name?.to_string();
        if let Some(map) = providers
            && let Some(entry) = map
                .get(&alias)
                .or_else(|| map.get(&alias.to_ascii_lowercase()))
        {
            return Some((alias, entry.clone()));
        }
        // Alias names a built-in but no explicit entry: synthesize a
        // default entry so callers don't have to special-case.
        if crate::provider::parse_provider(&alias).is_some() {
            return Some((alias, ProviderEntry::default()));
        }
        None
    }

    /// Resolve the provider_type for an entry — the entry's
    /// explicit value when set, otherwise the alias (lowercased)
    /// which must match a built-in.
    pub fn provider_type_of(name: &str, entry: &ProviderEntry) -> String {
        entry
            .provider_type
            .clone()
            .unwrap_or_else(|| name.to_ascii_lowercase())
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

    /// Phase-3: dynamic-tool-search opt-in. Default off.
    pub fn resolve_dynamic_tool_search(&self) -> bool {
        self.dynamic_tool_search.unwrap_or(false)
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
    ///   1. `providers[name].stream_chunk_timeout_secs`
    ///   2. top-level `stream_chunk_timeout_secs`
    ///   3. `DEFAULT_STREAM_CHUNK_TIMEOUT_SECS` (300s)
    ///
    /// Passing an unknown / empty provider name falls through past
    /// (1) to the top-level / default.
    pub fn resolve_stream_chunk_timeout(&self, provider: &str) -> std::time::Duration {
        // Provider lookup is case-insensitive because `parse_provider`
        // accepts `--provider Anthropic` (#2 fix). Without this, a
        // capitalized CLI / config provider name built the client
        // fine but missed the `providers.anthropic` override silently.
        let lower = provider.to_ascii_lowercase();
        let from_provider = self
            .providers
            .as_ref()
            .and_then(|m| m.get(provider).or_else(|| m.get(&lower)))
            .and_then(|p| p.stream_chunk_timeout_secs);
        let secs = from_provider
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
/// Read `EXA_API_KEY`, trimming whitespace and treating empty as unset.
/// Single source so every consumer (web-search tool, MCP auto-register,
/// the builder) applies the same trim/empty policy (dirge-3xqe).
pub fn exa_api_key() -> Option<String> {
    std::env::var("EXA_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

fn web_env_true(k: &str) -> bool {
    std::env::var(k)
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

/// Whether the websearch tool is enabled: config `tools.websearch`
/// (default true) OR `WEBSEARCH_ENABLED`. Single source for the
/// precedence duplicated across the two builder paths (dirge-f8oe).
pub fn websearch_enabled(cfg: &Config) -> bool {
    cfg.tools.as_ref().and_then(|t| t.websearch).unwrap_or(true)
        || web_env_true("WEBSEARCH_ENABLED")
}

/// Whether the webfetch tool is enabled: config `tools.webfetch`
/// (default true) OR `WEBFETCH_ENABLED`.
pub fn webfetch_enabled(cfg: &Config) -> bool {
    cfg.tools.as_ref().and_then(|t| t.webfetch).unwrap_or(true) || web_env_true("WEBFETCH_ENABLED")
}

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

        // Reject legacy config shape BEFORE deserialising. The old
        // shape used top-level `model`, `review_model`, and
        // `custom_providers`; all three have moved into
        // `providers.<alias>.{model,...}` (with role assignments via
        // `review_provider`, etc.). Surface a clear migration hint
        // rather than silently dropping fields.
        if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(obj) = raw.as_object()
        {
            const LEGACY: &[&str] = &["custom_providers", "model", "review_model"];
            let found: Vec<&str> = LEGACY
                .iter()
                .copied()
                .filter(|k| obj.contains_key(*k))
                .collect();
            if !found.is_empty() {
                eprintln!(
                    "error: legacy config keys found in {}: {:?}",
                    path.display(),
                    found,
                );
                eprintln!("Migrate to the unified `providers` map:");
                eprintln!("  - top-level `model`         -> `providers.<active-provider>.model`");
                eprintln!("  - `custom_providers.X`      -> `providers.X`");
                eprintln!("  - top-level `review_model`  -> `providers.<review-provider>.model`");
                eprintln!(
                    "Then optionally set `review_provider`, `escalation_provider`, \
                     `summarization_provider`, `subagent_provider`."
                );
                std::process::exit(2);
            }
        }

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

    // Validate `providers` at load time so a typo in
    // `provider_type` (or an alias that doesn't match a built-in
    // and has no explicit provider_type) surfaces immediately
    // instead of failing at first agent call with a cryptic
    // "unknown provider" deep in the call stack.
    if let Some(providers) = cfg.providers.as_ref() {
        for (name, p) in providers {
            let ptype = Config::provider_type_of(name, p);
            if crate::provider::parse_provider(&ptype).is_none() {
                eprintln!(
                    "error: provider {:?} has invalid provider_type {:?}.\n\
                     Either the alias must match a built-in (openrouter, openai,\n\
                     anthropic, gemini, deepseek, glm, ollama, custom) or set\n\
                     `provider_type` explicitly to one of those.",
                    name, ptype,
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
        match exa_api_key() {
            Some(key) => {
                let mut headers = HashMap::new();
                headers.insert("x-api-key".to_string(), key);
                let mut defaults = HashMap::new();
                defaults.insert(
                    "Exa Web Search".to_string(),
                    McpServerConfig::Url {
                        url: "https://mcp.exa.ai/mcp".to_string(),
                        headers,
                        allow_external_paths: false,
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

    /// dirge-99ic: `plugins.<name>.{enabled, auto_start}` toggles, with
    /// enabled defaulting to true (plugins load unless explicitly off).
    #[test]
    fn plugin_toggles_parse_with_enabled_default_true() {
        let cfg: Config = serde_json::from_str(
            r#"{
                "plugins": {
                    "backpressured": {"enabled": true, "auto_start": true},
                    "nrepl": {"enabled": false},
                    "noisy": {"auto_start": true}
                }
            }"#,
        )
        .unwrap();

        assert!(cfg.plugin_enabled("backpressured"));
        assert!(cfg.plugin_auto_start("backpressured"));

        assert!(!cfg.plugin_enabled("nrepl"));
        assert!(!cfg.plugin_auto_start("nrepl"));

        // enabled omitted → defaults to true; auto_start honored.
        assert!(cfg.plugin_enabled("noisy"));
        assert!(cfg.plugin_auto_start("noisy"));

        // Absent entry → enabled, not auto-started.
        assert!(cfg.plugin_enabled("unlisted"));
        assert!(!cfg.plugin_auto_start("unlisted"));

        // No `plugins` block at all → everything loads (backward compat).
        let empty: Config = serde_json::from_str("{}").unwrap();
        assert!(empty.plugin_enabled("anything"));
        assert!(!empty.plugin_auto_start("anything"));
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
        let cfg: Config = serde_json::from_str(r#"{"provider": "deepseek"}"#).unwrap();
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

#[cfg(test)]
mod provider_role_tests {
    use super::*;

    fn cfg_with_providers(json: &str) -> Config {
        serde_json::from_str(json).expect("parses")
    }

    #[test]
    fn resolve_role_default_returns_provider_entry() {
        let cfg = cfg_with_providers(
            r#"{
                "provider": "deepseek",
                "providers": { "deepseek": { "model": "deepseek-v4-pro" } }
            }"#,
        );
        let (name, entry) = cfg.resolve_role(ConfigRole::Default).unwrap();
        assert_eq!(name, "deepseek");
        assert_eq!(entry.model.as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn resolve_role_review_falls_back_to_default_provider() {
        // No review_provider set — review should fall back to the
        // active provider's entry.
        let cfg = cfg_with_providers(
            r#"{
                "provider": "deepseek",
                "providers": { "deepseek": { "model": "deepseek-v4-pro" } }
            }"#,
        );
        let (name, entry) = cfg.resolve_role(ConfigRole::Review).unwrap();
        assert_eq!(name, "deepseek");
        assert_eq!(entry.model.as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn resolve_role_review_uses_explicit_assignment() {
        let cfg = cfg_with_providers(
            r#"{
                "provider": "deepseek",
                "review_provider": "glm",
                "providers": {
                    "deepseek": { "model": "deepseek-v4-pro" },
                    "glm": { "model": "glm-4.6" }
                }
            }"#,
        );
        let (name, entry) = cfg.resolve_role(ConfigRole::Review).unwrap();
        assert_eq!(name, "glm");
        assert_eq!(entry.model.as_deref(), Some("glm-4.6"));
    }

    #[test]
    fn provider_type_of_returns_explicit_value_when_set() {
        let entry = ProviderEntry {
            provider_type: Some("openai".to_string()),
            ..Default::default()
        };
        assert_eq!(Config::provider_type_of("ollama", &entry), "openai");
    }

    #[test]
    fn provider_type_of_falls_back_to_alias_when_unset() {
        let entry = ProviderEntry::default();
        assert_eq!(Config::provider_type_of("deepseek", &entry), "deepseek");
        // Lowercases so `Anthropic` alias still parses as built-in.
        assert_eq!(Config::provider_type_of("Anthropic", &entry), "anthropic");
    }

    #[test]
    fn providers_map_returns_clone() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": { "deepseek": { "model": "x" } }
            }"#,
        );
        let map = cfg.providers_map();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("deepseek"));
    }

    #[test]
    fn providers_map_empty_when_unset() {
        let cfg = Config::default();
        assert!(cfg.providers_map().is_empty());
    }

    /// New unified shape (matches the target documented in the
    /// refactor): a `providers` map with mixed built-in entries
    /// (just a `model`) and aliased entries (`provider_type` +
    /// `base_url`) parses cleanly and round-trips through
    /// `resolve_role` / `provider_type_of`.
    #[test]
    fn new_shape_with_aliased_ollama_parses() {
        let cfg = cfg_with_providers(
            r#"{
                "provider": "deepseek",
                "providers": {
                    "deepseek": { "model": "deepseek-v4-pro" },
                    "ollama": {
                        "provider_type": "openai",
                        "base_url": "http://127.0.0.1:11434/v1"
                    }
                }
            }"#,
        );
        let (name, entry) = cfg.resolve_role(ConfigRole::Default).unwrap();
        assert_eq!(name, "deepseek");
        assert_eq!(entry.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(Config::provider_type_of("deepseek", &entry), "deepseek");

        let ollama = cfg.providers_map().get("ollama").cloned().unwrap();
        assert_eq!(Config::provider_type_of("ollama", &ollama), "openai");
        assert_eq!(
            ollama.base_url.as_deref(),
            Some("http://127.0.0.1:11434/v1")
        );
    }

    /// `api_key` accepts both snake_case and `apiKey` camelCase. A literal
    /// passes through; a `${VAR}` form expands against the env at call
    /// time.
    #[test]
    fn api_key_literal_passes_through() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": { "glm": { "api_key": "sk-literal" } }
            }"#,
        );
        let entry = cfg.providers_map().get("glm").cloned().unwrap();
        assert_eq!(
            entry.resolved_api_key().and_then(|r| r.ok()),
            Some("sk-literal".to_string())
        );
    }

    #[test]
    fn api_key_camel_case_alias_parses() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": { "glm": { "apiKey": "sk-camel" } }
            }"#,
        );
        let entry = cfg.providers_map().get("glm").cloned().unwrap();
        assert_eq!(entry.api_key.as_deref(), Some("sk-camel"));
    }

    #[test]
    fn api_key_env_interpolation_expands() {
        // SAFETY: tests in this module are inside the same process so
        // setting an env var is racy across threads. Use a uniquely-
        // named var so a concurrent test doesn't observe ours.
        let var = "DIRGE_TEST_API_KEY_EXPAND";
        unsafe { std::env::set_var(var, "sk-from-env") };
        let cfg = cfg_with_providers(&format!(
            r#"{{
                "providers": {{ "glm": {{ "api_key": "${{{var}}}" }} }}
            }}"#
        ));
        let entry = cfg.providers_map().get("glm").cloned().unwrap();
        assert_eq!(
            entry.resolved_api_key().and_then(|r| r.ok()),
            Some("sk-from-env".to_string())
        );
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn api_key_env_interpolation_reports_missing_var() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": { "glm": { "api_key": "${DIRGE_TEST_MISSING_VAR_NEVER_SET}" } }
            }"#,
        );
        let entry = cfg.providers_map().get("glm").cloned().unwrap();
        let err = entry.resolved_api_key().unwrap().unwrap_err();
        assert_eq!(err, "DIRGE_TEST_MISSING_VAR_NEVER_SET");
    }

    #[test]
    fn api_key_none_when_unset() {
        let entry = ProviderEntry::default();
        assert!(entry.resolved_api_key().is_none());
    }

    /// `options.temperature` is honored as f64. Other types in the
    /// same slot return None.
    #[test]
    fn options_temperature_f64() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": { "glm": { "options": { "temperature": 0.2 } } }
            }"#,
        );
        let entry = cfg.providers_map().get("glm").cloned().unwrap();
        assert_eq!(entry.options_temperature(), Some(0.2));
    }

    #[test]
    fn options_temperature_missing_or_wrong_shape() {
        let cfg = cfg_with_providers(
            r#"{
                "providers": {
                    "no-options":  {},
                    "wrong-shape": { "options": { "temperature": "hot" } }
                }
            }"#,
        );
        assert_eq!(
            cfg.providers_map()
                .get("no-options")
                .unwrap()
                .options_temperature(),
            None
        );
        assert_eq!(
            cfg.providers_map()
                .get("wrong-shape")
                .unwrap()
                .options_temperature(),
            None
        );
    }

    /// Legacy `model` at top level is detected before deserialization.
    /// `load()` reads from disk so we can't drive it directly here;
    /// we verify the detection predicate the same way `load()` does.
    #[test]
    fn legacy_model_key_detected() {
        let raw: serde_json::Value =
            serde_json::from_str(r#"{"model": "deepseek-v4-pro"}"#).unwrap();
        let obj = raw.as_object().unwrap();
        let legacy = ["custom_providers", "model", "review_model"];
        let found: Vec<&str> = legacy
            .iter()
            .copied()
            .filter(|k| obj.contains_key(*k))
            .collect();
        assert_eq!(found, vec!["model"]);
    }

    #[test]
    fn legacy_custom_providers_key_detected() {
        let raw: serde_json::Value =
            serde_json::from_str(r#"{"custom_providers": {"x": {}}}"#).unwrap();
        let obj = raw.as_object().unwrap();
        let legacy = ["custom_providers", "model", "review_model"];
        let found: Vec<&str> = legacy
            .iter()
            .copied()
            .filter(|k| obj.contains_key(*k))
            .collect();
        assert_eq!(found, vec!["custom_providers"]);
    }
}
