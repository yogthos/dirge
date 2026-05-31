pub mod client;
pub mod summarize;

use std::collections::HashMap;
use std::sync::OnceLock;

use rig::agent::Agent;
use rig::client::CompletionClient;
use rig::completion::{Message, Prompt};
use rig::providers::{anthropic, gemini, ollama, openai, openrouter};

use crate::agent::builder;
use crate::agent::prompt;
use crate::agent::runner::{self, AgentRunner};
use crate::agent::tools::ToolCache;
use crate::agent::tools::plan::PlanSwitchSender;
use crate::agent::tools::question::QuestionSender;
use crate::cli::Cli;
use crate::config::{Config, ProviderEntry};
use crate::context::ContextFiles;
use crate::event::AgentEvent;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;
use crate::session::SessionMessage;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProviderKind {
    OpenRouter,
    OpenAI,
    Anthropic,
    Gemini,
    DeepSeek,
    Glm,
    Ollama,
    Custom,
}

pub fn default_model_for(provider_name: &str) -> &'static str {
    // Per-provider sensible defaults. Without per-provider defaults
    // an unspecified `--model` against OpenAI/Anthropic/Gemini/Ollama
    // would pass `deepseek/deepseek-v4-flash` and the API would reject
    // with a confusing 404. Each provider gets a current-as-of-2026
    // first-class model id; OpenRouter keeps the multi-vendor prefix
    // form since that's what its API expects.
    match parse_provider(provider_name) {
        Some(ProviderKind::OpenAI) => "gpt-4o",
        Some(ProviderKind::Anthropic) => "claude-sonnet-4-6",
        Some(ProviderKind::Gemini) => "gemini-2.0-flash",
        Some(ProviderKind::DeepSeek) => "deepseek-v4-pro",
        Some(ProviderKind::Glm) => "glm-4",
        Some(ProviderKind::Ollama) => "llama3",
        // OpenRouter + Custom + unknown — keep the historical default
        // since OpenRouter wants the `vendor/model` form.
        _ => "deepseek/deepseek-v4-flash",
    }
}

pub fn parse_provider(name: &str) -> Option<ProviderKind> {
    match name.to_lowercase().as_str() {
        "openrouter" => Some(ProviderKind::OpenRouter),
        "openai" => Some(ProviderKind::OpenAI),
        "anthropic" => Some(ProviderKind::Anthropic),
        "gemini" | "google" => Some(ProviderKind::Gemini),
        "deepseek" => Some(ProviderKind::DeepSeek),
        "glm" | "zhipu" => Some(ProviderKind::Glm),
        "ollama" => Some(ProviderKind::Ollama),
        "custom" => Some(ProviderKind::Custom),
        _ => None,
    }
}

pub struct ProviderInfo {
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    /// Literal API key resolved from `entry.api_key` (with `${VAR}`
    /// already expanded). When present, takes precedence over both
    /// `api_key_env` and the standard env-var fallback chain.
    pub api_key_literal: Option<String>,
}

pub fn resolve_provider_info(
    name: &str,
    providers: &HashMap<String, ProviderEntry>,
) -> Option<ProviderInfo> {
    // Config-declared providers win on name collision — user intent
    // always trumps plugin defaults.
    // #2 fix: lowercase-fallback lookup so `--provider My-VLLM` finds
    // a `providers["my-vllm"]` config entry. parse_provider
    // (for built-ins) is already case-insensitive; matching the same
    // convention here removes a silent miss.
    let lower = name.to_ascii_lowercase();
    if let Some(entry) = providers.get(name).or_else(|| providers.get(&lower)) {
        let ptype = Config::provider_type_of(name, entry);
        let kind = parse_provider(&ptype)?;
        // Only enforce URL safety when the entry actually carries
        // a base_url. Built-in providers (e.g. `"deepseek": {}`)
        // legitimately have no base_url — they fall through to the
        // provider's default endpoint.
        if let Some(url) = entry.base_url.as_deref()
            && let Err(err) = validate_custom_provider(name, url, entry.allow_insecure)
        {
            tracing::error!(
                target: "dirge::provider",
                "{err}"
            );
            eprintln!("error: {err}");
            return None;
        }
        let api_key_literal = match entry.resolved_api_key() {
            Some(Ok(k)) => Some(k),
            Some(Err(missing)) => {
                tracing::error!(
                    target: "dirge::provider",
                    "provider '{name}' references env var ${{{missing}}} via api_key but it is unset",
                );
                eprintln!(
                    "error: provider '{name}' references env var ${{{missing}}} via api_key but it is unset"
                );
                None
            }
            None => None,
        };
        return Some(ProviderInfo {
            kind,
            base_url: entry.base_url.clone(),
            api_key_env: entry.api_key_env.clone(),
            api_key_literal,
        });
    }
    // Then plugin-registered providers from `harness/register-provider`.
    // Installed once at startup after plugin load; never mutated again
    // in this process.
    if let Some(entry) = plugin_provider(name).or_else(|| plugin_provider(&lower)) {
        let ptype = Config::provider_type_of(name, &entry);
        let kind = parse_provider(&ptype)?;
        if let Some(url) = entry.base_url.as_deref()
            && let Err(err) = validate_custom_provider(name, url, entry.allow_insecure)
        {
            tracing::error!(
                target: "dirge::provider",
                "{err}"
            );
            eprintln!("error: {err}");
            return None;
        }
        let api_key_literal = match entry.resolved_api_key() {
            Some(Ok(k)) => Some(k),
            Some(Err(missing)) => {
                tracing::error!(
                    target: "dirge::provider",
                    "plugin provider '{name}' references env var ${{{missing}}} via api_key but it is unset",
                );
                eprintln!(
                    "error: plugin provider '{name}' references env var ${{{missing}}} via api_key but it is unset"
                );
                None
            }
            None => None,
        };
        return Some(ProviderInfo {
            kind,
            base_url: entry.base_url,
            api_key_env: entry.api_key_env,
            api_key_literal,
        });
    }
    let kind = parse_provider(name)?;
    Some(ProviderInfo {
        kind,
        base_url: None,
        api_key_env: None,
        api_key_literal: None,
    })
}

/// Built-in provider names — custom/plugin providers are rejected
/// if they collide with one of these. Protects against a malicious
/// plugin that registers "openai" to silently intercept credentials.
const BUILTIN_PROVIDER_NAMES: &[&str] = &[
    "openai",
    "anthropic",
    "gemini",
    "google",
    "deepseek",
    "glm",
    "zhipu",
    "ollama",
    "openrouter",
    "custom",
];

/// Validate a custom/plugin provider's configuration.
/// - Rejects names that collide with built-in providers.
/// - Rejects non-https base_url unless `allow_insecure: true`.
fn validate_custom_provider(
    name: &str,
    base_url: &str,
    allow_insecure: bool,
) -> Result<(), String> {
    let lower = name.to_ascii_lowercase();
    if BUILTIN_PROVIDER_NAMES
        .iter()
        .any(|b| b.eq_ignore_ascii_case(&lower))
    {
        return Err(format!(
            "Custom provider '{}' collides with built-in provider name. \
             Choose a different name.",
            name
        ));
    }
    // URL scheme validation: only https:// is safe by default.
    // http:// sends plaintext over the network — every prompt,
    // file content, and tool result is exposed. Only allow when
    // the user explicitly opts in via `allow_insecure: true`,
    // which is appropriate for local-only proxies (ollama, vllm).
    if !allow_insecure && !base_url.starts_with("https://") {
        return Err(format!(
            "Custom provider '{}' has insecure base_url '{}'. \
             Set allow_insecure: true in config.json if this is a \
             local-only endpoint (e.g. ollama, vllm). All other \
             http:// URLs send your data in plaintext.",
            name, base_url
        ));
    }
    // PROV-1 stretch: when allow_insecure is set AND the base_url is
    // http://, also gate on host shape. Loopback / private-range
    // hosts (the legitimate ollama/vllm/lmstudio case) are silent;
    // a public-looking host with allow_insecure gets a LOUD stderr
    // warning every session so a misconfigured production setup
    // doesn't silently leak conversation content.
    if allow_insecure && base_url.starts_with("http://") && !looks_like_local_host(base_url) {
        eprintln!(
            "  ⚠️  WARNING: custom provider '{}' is using http:// over a NON-LOCAL host: {}\n  Every prompt, file content, and tool result is sent in plaintext.\n  This is allowed because allow_insecure: true is set in config.json,\n  but you should verify this is intentional — the typical allow_insecure\n  use case is loopback (127.0.0.1 / localhost) endpoints like ollama.",
            name, base_url,
        );
    }
    Ok(())
}

/// Quick check whether a base_url's host appears to be a loopback or
/// private-range address. Used by `validate_custom_provider` to
/// decide whether `allow_insecure: true` is benign (local ollama)
/// or alarming (somebody pointing at a public http endpoint). Not
/// a security boundary — `validate_custom_provider` already
/// rejected the dangerous case (http without allow_insecure) before
/// this function runs.
fn looks_like_local_host(base_url: &str) -> bool {
    let scheme_len = if base_url.len() >= 7 && base_url[..7].eq_ignore_ascii_case("http://") {
        7
    } else {
        return false;
    };
    let after = &base_url[scheme_len..];
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let host_and_port = &after[..end];
    let host: &str = if let Some(rest) = host_and_port.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        &rest[..end]
    } else {
        host_and_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_and_port)
    };
    let lower = host.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "localhost" | "ip6-localhost" | "ip6-loopback"
    ) {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
        };
    }
    // `.local` mDNS names are also commonly local-only.
    lower.ends_with(".local")
}

/// Process-global map of plugin-registered providers, populated once
/// after plugin load. Stored separately from `cfg.custom_providers`
/// so a `/reload` (future) can swap plugin providers without
/// disturbing the user's persistent config.
static PLUGIN_PROVIDERS: OnceLock<HashMap<String, ProviderEntry>> = OnceLock::new();

/// Install the plugin-registered provider map. Only the first call
/// wins (OnceLock semantics) — sufficient for current behavior where
/// plugins re-register every startup and never change at runtime.
/// Returns the installed-or-already-installed map size so callers
/// can log a confirmation.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub fn install_plugin_providers(map: HashMap<String, ProviderEntry>) -> usize {
    let size = map.len();
    let _ = PLUGIN_PROVIDERS.set(map);
    size
}

fn plugin_provider(name: &str) -> Option<ProviderEntry> {
    PLUGIN_PROVIDERS.get().and_then(|m| m.get(name).cloned())
}

fn provider_env_var(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAI => "OPENAI_API_KEY",
        ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
        ProviderKind::Gemini => "GEMINI_API_KEY",
        ProviderKind::DeepSeek => "DEEPSEEK_API_KEY",
        ProviderKind::Glm => "GLM_API_KEY",
        ProviderKind::Ollama => "OLLAMA_API_KEY",
        ProviderKind::OpenRouter => "OPENROUTER_API_KEY",
        ProviderKind::Custom => "CUSTOM_API_KEY",
    }
}

/// Auto-detect provider from environment variables when none is
/// explicitly configured. Returns the provider name string
/// (e.g. "deepseek") for the first matching `*_API_KEY` env var
/// with a non-empty value. Returns `None` if no known key is set.
///
/// Resolution order is fixed (see `PROVIDER_AUTODETECT_ORDER`).
/// When multiple keys are present, the FIRST in that list wins so
/// the behavior is deterministic — important for users who have
/// several keys in their shell environment.
pub fn auto_detect_provider() -> Option<&'static str> {
    auto_detect_provider_from(|name| std::env::var(name).ok())
}

/// Provider candidate list for autodetect. Listed in priority
/// order — first key with a non-empty value wins. Extracted as a
/// module item so tests reference the same source of truth and
/// adding a provider only touches one place.
const PROVIDER_AUTODETECT_ORDER: &[(&str, &str)] = &[
    ("DEEPSEEK_API_KEY", "deepseek"),
    ("OPENAI_API_KEY", "openai"),
    ("ANTHROPIC_API_KEY", "anthropic"),
    ("GEMINI_API_KEY", "gemini"),
    ("GLM_API_KEY", "glm"),
    // Zhipu's canonical env var name for the same provider. Listed
    // after GLM_API_KEY so users with both set get the dirge-
    // primary one; users with only ZHIPU_API_KEY still get glm.
    ("ZHIPU_API_KEY", "glm"),
    ("OLLAMA_API_KEY", "ollama"),
    ("OPENROUTER_API_KEY", "openrouter"),
];

/// Pure helper that drives `auto_detect_provider` from a
/// caller-supplied env lookup. Production calls
/// `auto_detect_provider()` which passes `std::env::var`; tests
/// pass a closure backed by a HashMap so they don't mutate
/// process-wide env vars (which races under parallel `cargo test`).
fn auto_detect_provider_from<F: Fn(&str) -> Option<String>>(env: F) -> Option<&'static str> {
    for (env_var, provider_name) in PROVIDER_AUTODETECT_ORDER {
        if let Some(v) = env(env_var)
            && !v.is_empty()
        {
            return Some(provider_name);
        }
    }
    None
}

/// Per-provider fallback env vars consulted AFTER the primary
/// (returned by `provider_env_var`) and after any explicit
/// `api_key_env_override`. Lets users with the upstream-canonical
/// env var name (e.g. ZHIPU_API_KEY for GLM/Zhipu) skip aliasing.
///
/// Empty for providers with no widely-used alternative; the slice
/// is iterated in order and the first non-empty value wins.
fn provider_env_var_fallbacks(kind: ProviderKind) -> &'static [&'static str] {
    match kind {
        // Zhipu's docs + their official SDKs uniformly use
        // ZHIPU_API_KEY. GLM_API_KEY is dirge's chosen primary
        // (matches the provider name), but accepting the
        // canonical form means users don't have to alias.
        ProviderKind::Glm => &["ZHIPU_API_KEY"],
        // B3-3 (audit fix): Anthropic users on Claude.ai OAuth
        // have ANTHROPIC_OAUTH_TOKEN exported by the official
        // setup tools. Pi (env-api-keys.ts:97-99) treats it as a
        // higher-priority alternative. Without this dirge users
        // had to manually export ANTHROPIC_API_KEY to use the
        // same token.
        ProviderKind::Anthropic => &["ANTHROPIC_OAUTH_TOKEN"],
        // Google's generative-language SDK (and the official
        // gemini-cli) uses GOOGLE_GENERATIVE_AI_API_KEY. dirge's
        // primary GEMINI_API_KEY matches the provider name in the
        // /model command surface; accepting the Google-canonical
        // form means users don't have to alias.
        ProviderKind::Gemini => &["GOOGLE_GENERATIVE_AI_API_KEY", "GOOGLE_API_KEY"],
        _ => &[],
    }
}

pub(crate) fn resolve_api_key(
    kind: ProviderKind,
    api_key_env_override: Option<&str>,
    cli_key: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(key) = cli_key.filter(|k| !k.is_empty()) {
        // Audit C2: the `/proc/*/cmdline` warning now fires at the
        // call site in main.rs where we know which CLI source the
        // key came from. File-sourced and stdin-sourced keys end up
        // here too but those paths don't appear in argv, so no
        // warning is wanted.
        return Ok(key.to_string());
    }

    let env_var = api_key_env_override
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| provider_env_var(kind));

    if let Ok(key) = std::env::var(env_var)
        && !key.is_empty()
    {
        return Ok(key);
    }

    // Provider-specific fallback env vars (e.g. ZHIPU_API_KEY
    // for GLM). Skip if the override was explicit — in that case
    // the user named the env var they want; don't second-guess.
    if api_key_env_override.is_none_or(|s| s.is_empty()) {
        for fallback in provider_env_var_fallbacks(kind) {
            if let Ok(key) = std::env::var(fallback)
                && !key.is_empty()
            {
                return Ok(key);
            }
        }
    }

    if kind == ProviderKind::Ollama {
        return Ok(String::new());
    }

    if kind == ProviderKind::Custom {
        return Ok(String::new());
    }

    let fallbacks = provider_env_var_fallbacks(kind);
    if fallbacks.is_empty() {
        anyhow::bail!(
            "No API key found for {kind:?}. Set the {env_var} environment variable or pass --api-key."
        )
    } else {
        anyhow::bail!(
            "No API key found for {kind:?}. Set {env_var} (or one of: {}) or pass --api-key.",
            fallbacks.join(", ")
        )
    }
}

pub enum AnyClient {
    OpenRouter(openrouter::Client),
    OpenAI(openai::CompletionsClient),
    Anthropic(anthropic::Client),
    Gemini(gemini::Client),
    DeepSeek(openai::CompletionsClient),
    Glm(openai::CompletionsClient),
    Ollama(ollama::Client),
    Custom(openai::CompletionsClient),
}

impl AnyClient {
    pub fn completion_model(&self, name: impl Into<String>) -> AnyModel {
        let name = name.into();
        match self {
            AnyClient::OpenRouter(c) => AnyModel::OpenRouter(c.completion_model(name)),
            AnyClient::OpenAI(c) => AnyModel::OpenAI(c.completion_model(name)),
            AnyClient::Anthropic(c) => AnyModel::Anthropic(c.completion_model(name)),
            AnyClient::Gemini(c) => AnyModel::Gemini(c.completion_model(name)),
            AnyClient::DeepSeek(c) => AnyModel::DeepSeek(c.completion_model(name)),
            AnyClient::Glm(c) => AnyModel::Glm(c.completion_model(name)),
            AnyClient::Ollama(c) => AnyModel::Ollama(c.completion_model(name)),
            AnyClient::Custom(c) => AnyModel::Custom(c.completion_model(name)),
        }
    }

    pub async fn compress_messages(
        &self,
        model_name: &str,
        messages: &[SessionMessage],
        previous_summary: Option<&str>,
        instructions: Option<&str>,
    ) -> anyhow::Result<String> {
        // C6 (audit fix): no more 6000-char truncation. A 300K-token
        // session was previously summarized from ~1500 tokens of
        // content — fidelity collapsed exactly when compaction was
        // most needed. Feed the full prefix; the summarizer model
        // (typically the same model as the agent, or a faster/
        // cheaper sibling with similar context) has plenty of room
        // unless the prefix itself is bigger than the summarizer's
        // window, in which case the summarizer's own context-overflow
        // path surfaces a real error rather than silently lying. Pi
        // and opencode both feed the full prefix.
        let conversation = summarize::serialize_conversation(messages);

        // `/compress <focus>` argument: when the user passes free-form
        // text after the slash command, treat it as a Hermes-style
        // FOCUS TOPIC. The summarizer is asked to allocate ~60-70%
        // of its budget to information related to the topic. Maps
        // hermes-agent/context_compressor.py:1050-1054.
        let instructions_block = match instructions {
            Some(text) if !text.trim().is_empty() => format!(
                "FOCUS TOPIC: \"{}\"\n\
                 The user has requested that this compaction PRIORITISE preserving \
                 all information related to the focus topic above. For content \
                 related to \"{}\", include full detail — exact values, file paths, \
                 command outputs, error messages, and decisions. For content NOT \
                 related to the focus topic, summarise more aggressively. The \
                 focus topic sections should receive roughly 60-70% of the \
                 summary token budget. Even for the focus topic, NEVER preserve \
                 API keys, tokens, passwords, or credentials — use [REDACTED].",
                text.trim(),
                text.trim(),
            ),
            _ => "(none)".to_string(),
        };

        // dirge-u13u: prompt-injection defense. Before we fence the
        // untrusted inputs with our distinctive delimiter pair, scan
        // them for the delimiter itself. If an attacker (via a prior
        // tool output, fetched URL, user paste, etc.) has managed to
        // smuggle the delimiter string in, re-wrapping would let them
        // close our fence and inject instructions outside it. Bail
        // rather than risk it. The warning stays on the operator side
        // (tracing) — we do NOT surface the collision detail to the
        // LLM. The caller treats this `Err` as "skip compaction for
        // this turn".
        let prev_summary_value = previous_summary.unwrap_or("(none)");
        if prompt::input_contains_compaction_delimiter(&[
            &conversation,
            prev_summary_value,
            &instructions_block,
        ]) {
            tracing::warn!(
                "compaction input contains the untrusted-material delimiter — \
                 skipping compaction this turn to avoid prompt-injection risk"
            );
            anyhow::bail!("compaction aborted: input contains reserved delimiter string");
        }

        let prompt = prompt::COMPACTION_PROMPT
            .replace("{conversation}", &conversation)
            .replace("{previous_summary}", prev_summary_value)
            .replace("{instructions}", &instructions_block);

        let model = self.completion_model(model_name.to_string());
        let response = summarize::summarize_with_model(model, prompt).await?;
        // If the summarizer echoed the delimiters into its output,
        // strip them before the summary gets injected into the next
        // turn's system prompt via `rig_history_system_prompt`. A
        // stray delimiter in the system prompt would (a) confuse the
        // next-turn LLM about where the untrusted block ends and
        // (b) trip our collision check on the next compaction.
        Ok(prompt::strip_compaction_delimiters(&response))
    }
}

#[derive(Clone)]
pub enum AnyModel {
    OpenRouter(openrouter::completion::CompletionModel),
    OpenAI(openai::completion::CompletionModel),
    Anthropic(anthropic::completion::CompletionModel),
    Gemini(gemini::completion::CompletionModel),
    DeepSeek(openai::completion::CompletionModel),
    Glm(openai::completion::CompletionModel),
    Ollama(ollama::CompletionModel),
    Custom(openai::completion::CompletionModel),
}

impl AnyModel {
    pub async fn btw_query(&self, prompt: String) -> anyhow::Result<String> {
        let preamble = "Answer the user's question concisely.";
        // PROV-3: wrap the bare one-shot prompt in the same recovery
        // policy used for the main turn loop. Previously a single
        // 503 from the provider killed every `/btw` and subagent
        // (`task` tool) call with no retry. Network + rate-limit
        // failures now get the standard 3-retry exponential backoff;
        // auth / context-length / other still bail immediately.
        use crate::agent::recovery::{RecoveryPolicy, run_with_retry};
        let policy = RecoveryPolicy::default();
        // The retry/backoff loop lives in `run_with_retry` (dirge-6cvc);
        // the macro only exists to dispatch over `AnyModel`'s concrete
        // per-variant model type (each `$m` has a different type).
        macro_rules! one_shot {
            ($m:expr) => {{
                let m = $m.clone();
                run_with_retry(&policy, "btw_query", || {
                    let agent = rig::agent::AgentBuilder::new(m.clone())
                        .preamble(preamble)
                        .build();
                    let prompt = prompt.clone();
                    async move { agent.prompt(prompt).await }
                })
                .await
                .map_err(anyhow::Error::from)
            }};
        }
        match self {
            AnyModel::OpenRouter(m) => one_shot!(m),
            AnyModel::OpenAI(m) => one_shot!(m),
            AnyModel::Anthropic(m) => one_shot!(m),
            AnyModel::Gemini(m) => one_shot!(m),
            AnyModel::DeepSeek(m) => one_shot!(m),
            AnyModel::Glm(m) => one_shot!(m),
            AnyModel::Ollama(m) => one_shot!(m),
            AnyModel::Custom(m) => one_shot!(m),
        }
    }

    /// Phase 4 part 1: build a standalone `StreamFn` from this
    /// model + tool definitions. Used to construct the escalation
    /// route when `ConfigRole::Escalation` resolves to a provider
    /// different from `ConfigRole::Default`. The result is plumbed
    /// into `LoopConfig.escalation_stream_fn` and invoked exactly
    /// once after a repair-exhaustion or tree-sitter failure.
    ///
    /// Tools and chunk timeout are passed in (not extracted) for
    /// symmetry with `AnyAgent::build_stream_fn_with_filter`. The
    /// escalation stream uses the SAME tool definitions as the
    /// default — only the model + provider differ.
    pub fn build_stream_fn(
        &self,
        tools: Vec<rig::completion::ToolDefinition>,
        chunk_timeout: std::time::Duration,
        provider_name: Option<String>,
    ) -> crate::agent::agent_loop::StreamFn {
        use crate::agent::agent_loop::rig_stream_fn_from_model_with_filter;
        match self {
            AnyModel::OpenRouter(m) => rig_stream_fn_from_model_with_filter(
                m.clone(),
                tools,
                Some(chunk_timeout),
                provider_name,
                None,
            ),
            AnyModel::OpenAI(m) => rig_stream_fn_from_model_with_filter(
                m.clone(),
                tools,
                Some(chunk_timeout),
                provider_name,
                None,
            ),
            AnyModel::Anthropic(m) => rig_stream_fn_from_model_with_filter(
                m.clone(),
                tools,
                Some(chunk_timeout),
                provider_name,
                None,
            ),
            AnyModel::Gemini(m) => rig_stream_fn_from_model_with_filter(
                m.clone(),
                tools,
                Some(chunk_timeout),
                provider_name,
                None,
            ),
            AnyModel::DeepSeek(m) => rig_stream_fn_from_model_with_filter(
                m.clone(),
                tools,
                Some(chunk_timeout),
                provider_name,
                None,
            ),
            AnyModel::Glm(m) => rig_stream_fn_from_model_with_filter(
                m.clone(),
                tools,
                Some(chunk_timeout),
                provider_name,
                None,
            ),
            AnyModel::Ollama(m) => rig_stream_fn_from_model_with_filter(
                m.clone(),
                tools,
                Some(chunk_timeout),
                provider_name,
                None,
            ),
            AnyModel::Custom(m) => rig_stream_fn_from_model_with_filter(
                m.clone(),
                tools,
                Some(chunk_timeout),
                provider_name,
                None,
            ),
        }
    }

    /// Return the model identifier string that was passed when
    /// the model was built (`client.completion_model("…")`).
    /// Forwarded to `LoopConfig.model_name` so the
    /// `tool_input_repair` telemetry can record `(model, tool,
    /// repair_kind)`.
    pub fn name(&self) -> String {
        match self {
            AnyModel::OpenRouter(m) => m.model.clone(),
            AnyModel::OpenAI(m) => m.model.clone(),
            AnyModel::Anthropic(m) => m.model.clone(),
            AnyModel::Gemini(m) => m.model.clone(),
            AnyModel::DeepSeek(m) => m.model.clone(),
            AnyModel::Glm(m) => m.model.clone(),
            AnyModel::Ollama(m) => m.model.clone(),
            AnyModel::Custom(m) => m.model.clone(),
        }
    }
}

/// dirge-yai1 — pure-function tool-name filter used by tests to
/// exercise the filter shape `spawn_filtered_runner_with_cache`
/// applies internally. Gated `#[cfg(test)]` because production
/// code uses the inline filter directly.
#[cfg(test)]
pub(crate) fn filter_tool_names<'a>(
    all: impl Iterator<Item = &'a str>,
    allowed: &[&str],
) -> Vec<String> {
    all.filter(|n| allowed.contains(n))
        .map(String::from)
        .collect()
}

#[derive(Clone)]
pub struct AnyAgent {
    inner: AnyAgentInner,
    cache: ToolCache,
    /// Per-chunk read timeout resolved at build_agent time from
    /// config (custom_providers.<n>.stream_chunk_timeout_secs >
    /// providers.<n>.stream_chunk_timeout_secs > top-level
    /// stream_chunk_timeout_secs > 300s default). Carried on the
    /// agent so spawn_runner / run_print don't need to thread it
    /// through every call site.
    chunk_timeout: std::time::Duration,
    /// Phase 4.5h-6: LoopTool registry the new agent_loop path
    /// dispatches against. Built once at `build_agent` time via
    /// `agent::builder::build_loop_tools`. `Vec<Arc<...>>` is
    /// clone-cheap (Arc bump).
    loop_tools: Vec<std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>>,
    /// Phase 4.5h-6: system prompt for the new loop path.
    /// Extracted from the rig Agent's preamble field at build
    /// time (every variant exposes `Agent.preamble: Option<String>`).
    preamble: String,
    /// Model identifier — the same string the user passed via
    /// `--model` or pulled from config. Carried so `spawn_runner`
    /// can forward it into `LoopSpawnConfig::model_name` for the
    /// `tool_input_repair` telemetry's `(model, tool, repair_kind)`
    /// triple. `String::new()` is acceptable — telemetry falls back
    /// to `"unknown"` when the field is empty.
    model_name: String,
    /// Phase-3: dynamic-tool-search opt-in. Resolved from
    /// `config.dynamic_tool_search` at `build_agent` time.
    /// When `true`, `spawn_runner` wires the shared
    /// `tool_def_filter` Arc into both the stream factory (for
    /// per-turn filtering) and (already) into the
    /// `ToolSearchTool` instance in `loop_tools`. Default
    /// `false` — the untouched-by-this-feature path.
    dynamic_tool_search: bool,
    /// Phase-3: per-session loaded-tool set. Allocated by
    /// `build_agent` when `dynamic_tool_search` is on, and
    /// shared with the `ToolSearchTool` instance registered in
    /// `loop_tools`. `spawn_runner` forwards this Arc to the
    /// stream factory so the filter sees the same set the tool
    /// mutates. `None` when the feature is off.
    tool_def_filter: Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
    /// dirge-tpx6: the live `tool_search` registry — the SAME Arc held by
    /// the `ToolSearchTool` in `loop_tools`. `extend_loop_tools` appends
    /// background-injected MCP tools' meta here so they stay search-gated
    /// (discoverable via `tool_search`, hidden until requested) rather
    /// than always-visible. `None` when dynamic_tool_search is off. Only
    /// read on the MCP-injection path.
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    tool_search_registry:
        Option<std::sync::Arc<std::sync::Mutex<Vec<crate::agent::tools::tool_search::ToolMeta>>>>,
    /// Phase 4 part 1: alternate stream function for dual-client
    /// escalation. Constructed at `build_agent` time when
    /// `ConfigRole::Escalation` resolves to a DIFFERENT provider
    /// than `ConfigRole::Default`. `None` keeps the legacy single-
    /// provider behaviour byte-for-byte identical.
    escalation_stream_fn: Option<crate::agent::agent_loop::StreamFn>,
    /// Phase 4 part 1: provider alias for the escalation route.
    /// Forwarded to `LoopConfig.escalation_provider_name` so the
    /// UI's `EscalationActivated` line can show the user which
    /// provider is taking over. `None` when escalation is off.
    escalation_provider_name: Option<String>,
    /// F6 tier 3: bounded LLM critic callback, built at `build_agent`
    /// time when `ConfigRole::Critic` resolves (i.e. `critic_provider`
    /// is configured). Forwarded to `LoopConfig.critic_fn`. `None` = off.
    critic_fn: Option<crate::agent::agent_loop::critic::CriticFn>,
    /// Phase 4 part 2: optional context-depth reminder threshold.
    /// Forwarded to `spawn_runner`, which constructs a fresh
    /// `FileTouchTracker` for each session because the tracker is
    /// per-prompt (`active_task` is the initial prompt).
    context_depth_reminder_threshold: Option<usize>,
    /// dirge-nqr: hard cap on assistant turns per run. Set via
    /// `with_max_turns`. Forwarded to `LoopSpawnConfig.max_turns`
    /// at spawn time. `None` = unlimited (legacy).
    max_turns: Option<usize>,
    /// dirge-z73i: alternate stream_fn for the background-review
    /// path. Built at `build_agent` time when `ConfigRole::Review`
    /// resolves to a different provider than `ConfigRole::Default`.
    /// `None` falls back to the main agent's stream_fn (legacy
    /// behavior; matches the original `spawn_review_runner`).
    review_stream_fn: Option<crate::agent::agent_loop::StreamFn>,
    /// dirge-z73i: provider alias for the review route, surfaced in
    /// the review runner's `LoopConfig.provider_name` so telemetry
    /// records the right backend.
    review_provider_name: Option<String>,
    /// dirge-z73i: model identifier for the review route, surfaced
    /// in the review runner's `LoopConfig.model_name`.
    review_model_name: Option<String>,
    /// dirge-9tfq: per-session background-task store, forwarded into
    /// `LoopSpawnConfig.bg_store` at spawn time so the loop's
    /// `get_followup_messages` hook surfaces subagent completions
    /// without needing the user to re-prompt. `None` when no store
    /// was supplied (tests, `--no-tools`); the followup path stays
    /// disabled in that case (legacy behaviour byte-identical).
    bg_store: Option<crate::agent::tools::background::BackgroundStore>,
    /// dirge-7tvq: memory provider held alongside the agent so
    /// session-lifecycle hooks (`on_session_end`, `on_pre_compress`)
    /// can dispatch through the trait. `None` when no provider was
    /// built (test agents, --no-tools, build failure). The provider
    /// is shared with `MemoryTool` via `Arc` — same instance.
    memory_provider: Option<std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>>,
}

#[derive(Clone)]
pub(crate) enum AnyAgentInner {
    OpenRouter(Agent<openrouter::completion::CompletionModel>),
    OpenAI(Agent<openai::completion::CompletionModel>),
    Anthropic(Agent<anthropic::completion::CompletionModel>),
    Gemini(Agent<gemini::completion::CompletionModel>),
    DeepSeek(Agent<openai::completion::CompletionModel>),
    Glm(Agent<openai::completion::CompletionModel>),
    Ollama(Agent<ollama::CompletionModel>),
    Custom(Agent<openai::completion::CompletionModel>),
}

impl AnyAgent {
    pub fn new(
        inner: AnyAgentInner,
        cache: ToolCache,
        chunk_timeout: std::time::Duration,
        loop_tools: Vec<std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>>,
        preamble: String,
        model_name: String,
    ) -> Self {
        AnyAgent {
            inner,
            cache,
            chunk_timeout,
            loop_tools,
            preamble,
            model_name,
            dynamic_tool_search: false,
            tool_def_filter: None,
            tool_search_registry: None,
            escalation_stream_fn: None,
            escalation_provider_name: None,
            critic_fn: None,
            context_depth_reminder_threshold: None,
            max_turns: None,
            review_stream_fn: None,
            review_provider_name: None,
            review_model_name: None,
            bg_store: None,
            memory_provider: None,
        }
    }

    /// dirge-x949: append tools to the live loop registry. Background
    /// MCP loading uses this to inject server tools after the agent was
    /// built (and the UI drawn) without them — the next
    /// `clone().spawn_runner` forwards the grown registry to the loop
    /// dispatch and the request's tool-definition list. Cheap: each
    /// entry is an `Arc` bump.
    ///
    /// dirge-ffwa/tpx6: when `dynamic_tool_search` is on, the request only
    /// ships tool defs whose names are in the shared loaded-set, and the
    /// model discovers the rest via `tool_search` over a registry snapshot
    /// taken at BUILD time — before MCP connected. A late-injected tool is
    /// in neither place, so it would be both undiscoverable and filtered
    /// out of every request (uncallable). Fix: append its meta to the live
    /// `tool_search` registry so the model can DISCOVER it via
    /// `tool_search` (and `tool_search` then marks it loaded on demand) —
    /// keeping it search-gated, exactly like a build-time MCP tool, rather
    /// than force-loading it into every request. No-op when
    /// dynamic_tool_search is off (registry is `None`).
    #[cfg(feature = "mcp")]
    pub fn extend_loop_tools(
        &mut self,
        more: Vec<std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>>,
    ) {
        if let Some(registry) = &self.tool_search_registry {
            let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            for t in &more {
                reg.push(crate::agent::tools::tool_search::meta_from_loop_tool(
                    t.as_ref(),
                ));
            }
        }
        self.loop_tools.extend(more);
    }

    /// dirge-7tvq: install the `MemoryProvider` used for this session
    /// so lifecycle hooks (`on_session_end`, `on_pre_compress`) can
    /// dispatch through the trait. Called by `build_agent` once the
    /// provider has been constructed. Idempotent — repeated calls
    /// replace the held Arc.
    pub fn with_memory_provider(
        mut self,
        provider: std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>,
    ) -> Self {
        self.memory_provider = Some(provider);
        self
    }

    /// dirge-7tvq: accessor for the held memory provider. Used by
    /// lifecycle call sites (session swap, compaction) to fire the
    /// trait hooks. Returns `None` for test agents and `--no-tools`
    /// runs where no provider was constructed.
    pub fn memory_provider(
        &self,
    ) -> Option<&std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>> {
        self.memory_provider.as_ref()
    }

    /// dirge-9tfq: install the per-session background-task store so
    /// `spawn_runner` can wire the subagent-completion follow-up
    /// hook into the agent loop. Called by `build_agent` whenever a
    /// `BackgroundStore` was provided (production interactive paths;
    /// not test / `--no-tools`). Idempotent — repeated calls replace
    /// the stored handle but keep the Arc-internal state in the
    /// shared store unchanged.
    pub fn with_bg_store(
        mut self,
        store: crate::agent::tools::background::BackgroundStore,
    ) -> Self {
        self.bg_store = Some(store);
        self
    }

    /// dirge-z73i: install a dedicated stream_fn for the
    /// background-review path. Called from `build_agent` only when
    /// `ConfigRole::Review` resolves to a different alias than
    /// `ConfigRole::Default`. `spawn_review_runner` picks this up
    /// and routes review work through the alternate provider/model.
    pub fn with_review_route(
        mut self,
        stream_fn: crate::agent::agent_loop::StreamFn,
        provider_name: String,
        model_name: String,
    ) -> Self {
        self.review_stream_fn = Some(stream_fn);
        self.review_provider_name = Some(provider_name);
        self.review_model_name = Some(model_name);
        self
    }

    /// dirge-nqr: install the per-run assistant-turn cap. `None`
    /// clears any previous cap (unlimited). Forwarded to
    /// `LoopSpawnConfig.max_turns` at spawn time.
    pub fn with_max_turns(mut self, max_turns: Option<usize>) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Phase 4 part 1: wire the dual-client escalation route.
    /// Called by `build_agent` only when `ConfigRole::Escalation`
    /// resolves to a different provider than `ConfigRole::Default`.
    /// Pass both the StreamFn and the provider alias so
    /// `spawn_runner` can plumb them through to `LoopSpawnConfig`.
    pub fn with_escalation(
        mut self,
        stream_fn: crate::agent::agent_loop::StreamFn,
        provider_name: String,
    ) -> Self {
        self.escalation_stream_fn = Some(stream_fn);
        self.escalation_provider_name = Some(provider_name);
        self
    }

    /// F6 tier 3: attach the bounded LLM critic. Called by `build_agent`
    /// only when `ConfigRole::Critic` resolves (`critic_provider` set).
    pub fn with_critic(mut self, critic_fn: crate::agent::agent_loop::critic::CriticFn) -> Self {
        self.critic_fn = Some(critic_fn);
        self
    }

    /// Phase 4 part 2: enable the context-depth reminder system
    /// with the given consecutive-turn threshold. Called by
    /// `build_agent` only when `config.context_depth_reminder_threshold`
    /// is `Some`. Carrying the threshold (rather than a tracker
    /// instance) lets `spawn_runner` build a fresh tracker per
    /// session seeded with the initial prompt.
    pub fn with_context_depth_reminder(mut self, threshold: usize) -> Self {
        self.context_depth_reminder_threshold = Some(threshold);
        self
    }

    /// Phase-3: enable the dynamic-tool-search path for sessions
    /// spawned from this agent. `filter` is the shared Arc
    /// already wired into the `ToolSearchTool` registered in
    /// `loop_tools` (so the tool's mutations and the request
    /// filter see the SAME set). Caller (build_agent) reads
    /// `config.dynamic_tool_search`; when off, this method
    /// isn't called and the legacy path runs untouched.
    pub fn with_dynamic_tool_search(
        mut self,
        filter: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
        registry: std::sync::Arc<std::sync::Mutex<Vec<crate::agent::tools::tool_search::ToolMeta>>>,
    ) -> Self {
        self.dynamic_tool_search = true;
        self.tool_def_filter = Some(filter);
        self.tool_search_registry = Some(registry);
        self
    }

    pub async fn run_print(
        &self,
        prompt: &str,
        max_turns: usize,
        output_format: crate::cli::OutputFormat,
    ) -> anyhow::Result<String> {
        // dirge-nqr: honor the cap explicitly even if the agent was
        // built with a different one. `run_print` is the headless
        // entry point — callers explicitly pass the cap they want.
        let agent = self.clone().with_max_turns(Some(max_turns));
        let start_instant = std::time::Instant::now();
        let session_id = runner::uuid_v4_simple();
        let mut num_turns: u32 = 0;
        let suppress_inline = !matches!(output_format, crate::cli::OutputFormat::Text);

        // Plugin `on-prompt` dispatch. Headless modes (--print, --loop)
        // previously skipped this — plugins that mutate the user prompt
        // or block it never fired in CI/script contexts.
        let effective_prompt: String = {
            #[cfg(feature = "plugin")]
            {
                if let Some(pm_arc) = crate::plugin::hook::global() {
                    let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
                    runner::resolve_prompt_with_hooks(prompt, &mut mgr)
                } else {
                    prompt.to_string()
                }
            }
            #[cfg(not(feature = "plugin"))]
            {
                prompt.to_string()
            }
        };

        // StreamJson init event — fires once at startup so downstream
        // tools can pick up cwd/session/model before any turns stream.
        if matches!(output_format, crate::cli::OutputFormat::StreamJson) {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            runner::emit_stream_json_event(serde_json::json!({
                "type": "system",
                "subtype": "init",
                "cwd": cwd,
                "session_id": session_id,
                "tools": Vec::<String>::new(),
                "model": "",
            }));
        }

        // Wire through the new agent_loop path: clone the agent (cheap
        // — Arc internals + refcounts), spawn a runner, and drain the
        // event channel collecting text. Use the max_turns-stamped
        // `agent` from above so the cap is honored.
        let runner = agent.spawn_runner(effective_prompt.clone(), Vec::new(), None);
        let task = runner.task;
        let mut event_rx = runner.event_rx;

        let mut full_response = String::new();
        let mut had_output = false;

        while let Some(event) = event_rx.recv().await {
            match event {
                AgentEvent::Token(text) => {
                    full_response.push_str(&text);
                    if !suppress_inline {
                        let safe = crate::ui::ansi::strip_controls(
                            &text,
                            crate::ui::ansi::StripPolicy::KEEP_NEWLINE,
                        );
                        print!("{safe}");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    had_output = true;
                }
                AgentEvent::Done { response, .. } => {
                    // `Done.response` is the authoritative full text.
                    full_response = response.to_string();
                    break;
                }
                AgentEvent::Error(err) => {
                    if had_output {
                        println!();
                    }
                    eprintln!("Error: {}", err);
                    let _ = task.await;
                    return Err(anyhow::anyhow!("{}", err));
                }
                AgentEvent::TurnEnd { .. } => {
                    num_turns += 1;
                }
                AgentEvent::SystemNotice { content } => {
                    // dirge-originated runtime notice (e.g. the
                    // max-agent-turns cap). Headless drives output from
                    // events, so surface it to stderr — otherwise a
                    // truncated run looks like a clean success to a
                    // `--print` consumer.
                    if had_output {
                        println!();
                    }
                    eprintln!("{}", content);
                }
                // Plugin-driven model swap after last run puts the
                // request in the mgr; caller drains via
                // take_pending_next_model().
                _ => {}
            }
        }

        // Await the spawned task to catch any panics.
        let _ = task.await;

        // Plugin `on-response` + `on-complete` + `prepare-next-run`
        // dispatch. Headless modes previously skipped these.
        #[cfg(feature = "plugin")]
        if let Some(pm_arc) = crate::plugin::hook::global() {
            let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
            let result = runner::apply_response_hooks(&full_response, &mut mgr);
            if let Some(replacement) = result.replacement {
                if suppress_inline {
                    full_response = replacement;
                } else {
                    println!();
                    println!("[plugin replace-result]");
                    let safe = crate::ui::ansi::strip_controls(
                        &replacement,
                        crate::ui::ansi::StripPolicy::KEEP_NEWLINE,
                    );
                    println!("{safe}");
                    full_response = replacement;
                }
            }
        }

        match output_format {
            crate::cli::OutputFormat::Text => {
                println!();
            }
            crate::cli::OutputFormat::Json => {
                let result = serde_json::json!({
                    "type": "result",
                    "subtype": "success",
                    "is_error": false,
                    "duration_ms": start_instant.elapsed().as_millis() as u64,
                    "num_turns": num_turns,
                    "result": full_response.clone(),
                    "session_id": session_id,
                    "total_cost_usd": 0.0,
                });
                if let Ok(s) = serde_json::to_string(&result) {
                    println!("{}", s);
                }
            }
            crate::cli::OutputFormat::StreamJson => {
                runner::emit_stream_json_event(serde_json::json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": full_response.clone()}],
                    },
                    "session_id": session_id,
                }));
                runner::emit_stream_json_event(serde_json::json!({
                    "type": "result",
                    "subtype": "success",
                    "is_error": false,
                    "duration_ms": start_instant.elapsed().as_millis() as u64,
                    "num_turns": num_turns,
                    "result": full_response.clone(),
                    "session_id": session_id,
                    "total_cost_usd": 0.0,
                }));
            }
        }
        Ok(full_response)
    }

    /// Phase 4.5h-6 cutover: route through the new agent_loop
    /// path. Composes 4.5a (rig stream), 4.5b (rig tool adapter,
    /// done at build time via build_loop_tools), 4.5c (event
    /// bridge), 4.5d (plugin hooks from the global manager),
    /// 4.5g (retry wrapper around the stream), and emits
    /// `AgentEvent`s on the existing `AgentRunner` shape so UI /
    /// ACP callsites work unchanged.
    ///
    /// Returns immediately with `AgentRunner`; the loop runs on
    /// a spawned tokio task.
    /// Return the provider name as a static string (matches the
    /// CLI / config naming: "openai", "anthropic", ..., "glm",
    /// "ollama", "openrouter", "custom"). Used to populate
    /// `LoopConfig.provider_name` so the `getApiKey` hook
    /// receives the canonical name (code review #2).
    pub fn provider_name(&self) -> &'static str {
        match &self.inner {
            AnyAgentInner::OpenRouter(_) => "openrouter",
            AnyAgentInner::OpenAI(_) => "openai",
            AnyAgentInner::Anthropic(_) => "anthropic",
            AnyAgentInner::Gemini(_) => "gemini",
            AnyAgentInner::DeepSeek(_) => "deepseek",
            AnyAgentInner::Glm(_) => "glm",
            AnyAgentInner::Ollama(_) => "ollama",
            AnyAgentInner::Custom(_) => "custom",
        }
    }

    /// Internal accessor for the agent's tool result cache.
    /// Exposed `pub(crate)` so tests in `provider::mod_tests`
    /// can assert cache-isolation invariants (e.g. dirge-7ls:
    /// the background-review runner must NOT share this Arc).
    #[allow(dead_code)]
    pub(crate) fn cache(&self) -> &ToolCache {
        &self.cache
    }

    pub fn spawn_runner(
        self,
        prompt: String,
        history: Vec<Message>,
        steering_queue: Option<
            std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
        >,
    ) -> AgentRunner {
        use crate::agent::agent_loop::{
            LoopSpawnConfig, loop_tool_to_rig_definition, retrying_stream_fn,
            rig_history_system_prompt, rig_history_to_loop_messages, spawn_loop_runner,
        };
        use crate::agent::recovery::RecoveryPolicy;

        self.cache.clear();

        let provider_name = self.provider_name().to_string();

        // Convert tool registry → rig ToolDefinitions for the
        // request builder, and keep the registry itself for the
        // loop's dispatch.
        let tool_defs: Vec<rig::completion::ToolDefinition> = self
            .loop_tools
            .iter()
            .map(|t| loop_tool_to_rig_definition(t.as_ref()))
            .collect();

        // Phase-3: per-session loaded-tool set was allocated at
        // `build_agent` time (when `dynamic_tool_search` is on)
        // and the SAME Arc was passed both to the
        // `ToolSearchTool` registered in `self.loop_tools` and
        // stored on `self.tool_def_filter`. The factory reads it
        // per-request; the tool inserts into it on execute.
        // `None` keeps the legacy path.
        let tool_def_filter = self.tool_def_filter.clone();

        // Build the StreamFn (4.5h-2 + 4.5h-3 chunk timeout).
        let inner_stream_fn = self.build_stream_fn_with_filter(tool_defs, tool_def_filter.clone());
        // Wrap with retry (4.5g) so transient Network / RateLimit
        // errors auto-retry with exponential backoff + Retry-After.
        let stream_fn = retrying_stream_fn(inner_stream_fn, RecoveryPolicy::default());

        // Merge any system-message content from the history
        // (e.g. compaction summary) into the loop's
        // Context.system_prompt. The Agent's preamble (model
        // identity + tool docs) is the base; session-side
        // system messages append.
        let history_preamble = rig_history_system_prompt(&history);
        // `mut` is consumed only by the plugin-gated append below.
        #[cfg_attr(not(feature = "plugin"), allow(unused_mut))]
        let mut system_prompt = if history_preamble.is_empty() {
            self.preamble.clone()
        } else {
            format!("{}\n\n{}", self.preamble, history_preamble)
        };

        // dirge-wqxj: fire the `before-agent-start` plugin hook with
        // the assembled system prompt. A plugin may call
        // `harness/append-system-prompt` to add project/team context
        // to the preamble before the agent starts. Append-only — the
        // model-identity + tool-docs preamble is preserved.
        #[cfg(feature = "plugin")]
        if let Some(pm) = crate::plugin::hook::global() {
            let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
            let ctx = format!(
                "@{{:system-prompt \"{}\"}}",
                crate::plugin::escape_janet_string(&system_prompt)
            );
            match mgr.dispatch("before-agent-start", &ctx) {
                Ok(_) => {
                    if let Some(append) = mgr.take_system_prompt_append() {
                        let append = append.trim();
                        if !append.is_empty() {
                            system_prompt = format!("{system_prompt}\n\n{append}");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "dirge::plugin",
                        error = %e,
                        "before-agent-start hook error — system prompt left unchanged",
                    );
                }
            }
        }

        // Convert rig history → loop messages (Session-side
        // user/assistant/toolResult shapes).
        let loop_history = rig_history_to_loop_messages(history);

        let mut cfg = LoopSpawnConfig::minimal(stream_fn, prompt.clone());
        cfg.system_prompt = system_prompt;
        cfg.history = loop_history;
        cfg.tools = self.loop_tools;
        cfg.provider_name = Some(provider_name);
        cfg.model_name = if self.model_name.is_empty() {
            None
        } else {
            Some(self.model_name.clone())
        };
        cfg.steering_queue = steering_queue;
        cfg.tool_def_filter = tool_def_filter;
        cfg.dynamic_tool_search = self.dynamic_tool_search;
        // Phase 4 part 1: thread the escalation route — when set,
        // the loop's `stream_assistant_response` swaps to this
        // StreamFn for the call immediately following a repair or
        // tree-sitter failure. `escalation_stream_fn=None` keeps
        // the legacy single-provider path byte-for-byte identical.
        cfg.escalation_stream_fn = self.escalation_stream_fn.clone();
        cfg.escalation_provider_name = self.escalation_provider_name.clone();
        // Phase 4 part 2: build a fresh `FileTouchTracker` per
        // session seeded with the current prompt as the active
        // task. `None` keeps the feature off — byte-identical to
        // today.
        cfg.file_touch_tracker = self
            .context_depth_reminder_threshold
            .map(|t| crate::agent::agent_loop::context_depth::FileTouchTracker::new(t, prompt));
        // F6: pre-finalization verifier gate, always on (baked-in). Nudges
        // to verify before finishing when code was edited but not run.
        cfg.verifier = Some(crate::agent::agent_loop::verifier::VerifierGate::new());
        // F6 tier 3: thread the bounded critic (only Some when
        // critic_provider is configured). `None` → no critic.
        cfg.critic_fn = self.critic_fn.clone();
        // dirge-nqr: forward the per-run turn cap. `None` keeps the
        // legacy unlimited behavior.
        cfg.max_turns = self.max_turns;
        // dirge-9tfq: forward the BackgroundStore so the spawn pipeline
        // installs a `get_followup_messages` hook that drains pending
        // subagent completions at the outer-loop boundary. `None`
        // (no-tools / test paths) leaves the hook unset and the loop
        // behaves byte-identically to pre-9tfq.
        cfg.bg_store = self.bg_store.clone();
        // dirge-h5tv: thread the memory provider into the loop so
        // auto-compaction can fire on_pre_compress. `None` paths
        // (no provider attached) keep legacy no-op behavior.
        cfg.memory_provider = self.memory_provider.clone();
        #[cfg(feature = "plugin")]
        {
            cfg.plugin_mgr = crate::plugin::hook::global();
        }

        let loop_runner = spawn_loop_runner(cfg);
        loop_runner.into_agent_runner()
    }

    /// Spawn a review runner with only memory + skill tools.
    /// Used by background review (Phase 4) to create a restricted
    /// agent that can only write to project memory and skills.
    ///
    /// dirge-7ls: the review runner gets its OWN `ToolCache` rather
    /// than reusing the main agent's. Even though today's
    /// memory/skill tools don't touch the cache directly, any
    /// future tool added to the review allow-list (or any future
    /// invalidation hook like `cache.clear()` on memory writes)
    /// must not pollute the main agent's cache mid-session.
    /// `subagents/task` is deliberately NOT changed — subagents
    /// share with their parent by design.
    pub fn spawn_review_runner(
        &self,
        prompt: String,
        transcript: String,
    ) -> crate::agent::runner::AgentRunner {
        let (runner, _isolated_cache) =
            self.spawn_review_runner_with_cache(prompt, transcript, ToolCache::new());
        runner
    }

    /// dirge-yai1 — skill-only fork used by the curator's
    /// umbrella-consolidation pass. The curator prompt instructs
    /// the model to only use `skill`, but a tool-level filter is
    /// stronger than a prompt-level guard. Same isolation /
    /// retry / stream-fn selection as `spawn_review_runner`.
    pub fn spawn_curator_runner(
        &self,
        prompt: String,
        transcript: String,
    ) -> crate::agent::runner::AgentRunner {
        let (runner, _isolated_cache) =
            self.spawn_filtered_runner_with_cache(prompt, transcript, ToolCache::new(), &["skill"]);
        runner
    }

    /// dirge-mo0w PR-2: memory-only forked runner for the memory
    /// curator's LLM consolidation pass. Inverse of
    /// `spawn_curator_runner` — same forked-runner pattern, but
    /// the tool allow-list is `&["memory"]` so the consolidation
    /// pass can ONLY add/replace/remove memory entries, not write
    /// skills. The model literally cannot reach skill-write tools
    /// even if the prompt-level guard slips.
    pub fn spawn_memory_curator_runner(
        &self,
        prompt: String,
        transcript: String,
    ) -> crate::agent::runner::AgentRunner {
        let (runner, _isolated_cache) = self.spawn_filtered_runner_with_cache(
            prompt,
            transcript,
            ToolCache::new(),
            &["memory"],
        );
        runner
    }

    /// Internal review-runner constructor with an explicit
    /// caller-supplied cache. Returns the cache alongside the
    /// runner so tests can assert cache isolation via
    /// `ToolCache::shares_storage_with` against `self.cache()`
    /// (dirge-7ls regression test). Callers in production code
    /// should use `spawn_review_runner`, which passes
    /// `ToolCache::new()` here.
    pub(crate) fn spawn_review_runner_with_cache(
        &self,
        prompt: String,
        transcript: String,
        review_cache: ToolCache,
    ) -> (crate::agent::runner::AgentRunner, ToolCache) {
        // dirge-yai1: delegate to the parameterized helper so the
        // curator can reuse the same machinery with a skill-only
        // filter without duplicating the body.
        self.spawn_filtered_runner_with_cache(
            prompt,
            transcript,
            review_cache,
            &["memory", "skill"],
        )
    }

    /// dirge-yai1: forked-runner factory parameterized by the tool
    /// allow-list. `spawn_review_runner_with_cache` calls in with
    /// `&["memory", "skill"]`; the curator pass calls in with
    /// `&["skill"]` so the model literally cannot write memory
    /// entries even if the prompt-level guard slips. Same cache
    /// isolation, same retry policy, same stream-fn selection as
    /// the original review runner.
    pub(crate) fn spawn_filtered_runner_with_cache(
        &self,
        prompt: String,
        transcript: String,
        review_cache: ToolCache,
        allowed_tools: &[&str],
    ) -> (crate::agent::runner::AgentRunner, ToolCache) {
        use crate::agent::agent_loop::{
            LoopSpawnConfig, loop_tool_to_rig_definition, retrying_stream_fn, spawn_loop_runner,
        };
        use crate::agent::recovery::RecoveryPolicy;

        // Hard guard against accidental sharing: if a caller
        // somehow passes the parent's cache, the regression test
        // would fail — but defense-in-depth, debug_assert that
        // the passed cache is distinct from the parent's.
        debug_assert!(
            !review_cache.shares_storage_with(&self.cache),
            "spawn_filtered_runner_with_cache: review cache must not share storage with the main agent's cache (dirge-7ls)"
        );

        // Filter to the caller-supplied allow-list.
        let review_tools: Vec<std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>> = self
            .loop_tools
            .iter()
            .filter(|t| allowed_tools.contains(&t.name()))
            .cloned()
            .collect();

        let tool_defs: Vec<rig::completion::ToolDefinition> = review_tools
            .iter()
            .map(|t| loop_tool_to_rig_definition(t.as_ref()))
            .collect();

        // dirge-z73i: prefer the explicit review_stream_fn when the
        // user configured `review_provider` to point at a different
        // alias than `provider`. Falls back to the main agent's
        // stream_fn so unconfigured sessions keep the legacy behavior
        // byte-for-byte.
        let (inner_stream_fn, provider_name_for_review, model_name_for_review) =
            if let Some(rfn) = self.review_stream_fn.clone() {
                (
                    rfn,
                    self.review_provider_name
                        .clone()
                        .unwrap_or_else(|| self.provider_name().to_string()),
                    self.review_model_name.clone(),
                )
            } else {
                (
                    self.build_stream_fn(tool_defs),
                    self.provider_name().to_string(),
                    if self.model_name.is_empty() {
                        None
                    } else {
                        Some(self.model_name.clone())
                    },
                )
            };
        let stream_fn = retrying_stream_fn(inner_stream_fn, RecoveryPolicy::default());

        let full_prompt = format!(
            "{}\n\n<session_transcript>\n{}\n</session_transcript>",
            prompt, transcript
        );

        let mut cfg = LoopSpawnConfig::minimal(stream_fn, full_prompt);
        cfg.system_prompt = self.preamble.clone();
        cfg.tools = review_tools;
        cfg.provider_name = Some(provider_name_for_review);
        cfg.model_name = model_name_for_review;

        let loop_runner = spawn_loop_runner(cfg);
        (loop_runner.into_agent_runner(), review_cache)
    }

    /// Phase 4.5h-2: produce a `StreamFn` from this agent's
    /// underlying `CompletionModel`, threading the supplied tool
    /// definitions. Used by the new loop path (`spawn_loop_runner`)
    /// to drive a real LLM through the ported agent_loop.
    ///
    /// Dispatch is a match over `AnyAgentInner`; each variant
    /// extracts its provider-specific `Arc<M>` and threads it
    /// through `rig_stream_fn_from_model::<M>`. The Arc deref +
    /// clone is cheap (refcount bump on the inner Arc, then a
    /// CompletionModel clone — rig's models are themselves
    /// Arc-internal in most provider impls).
    ///
    /// Tool definitions are passed in (not extracted from
    /// `agent.tools`) because the new path uses the LoopTool
    /// registry as the source of truth — phase 4.5h-4 builds
    /// that registry alongside the rig Agent. Callers convert
    /// each `Arc<dyn LoopTool>` to a rig `ToolDefinition` via
    /// `agent_loop::loop_tool_to_rig_definition` before calling
    /// this method.
    pub fn build_stream_fn(
        &self,
        tools: Vec<rig::completion::ToolDefinition>,
    ) -> crate::agent::agent_loop::StreamFn {
        self.build_stream_fn_with_filter(tools, None)
    }

    /// Phase-3 dynamic-tool-search variant. When
    /// `tool_def_filter` is `Some`, the per-request tool list is
    /// filtered to the always-on set + names present in the
    /// shared loaded set (plus `tool_search`). When `None`, the
    /// behavior is byte-for-byte identical to the legacy
    /// `build_stream_fn`.
    pub fn build_stream_fn_with_filter(
        &self,
        tools: Vec<rig::completion::ToolDefinition>,
        tool_def_filter: Option<
            std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
        >,
    ) -> crate::agent::agent_loop::StreamFn {
        use crate::agent::agent_loop::rig_stream_fn_from_model_with_filter;
        let chunk_timeout = self.chunk_timeout;
        let provider = Some(self.provider_name().to_string());
        match &self.inner {
            AnyAgentInner::OpenRouter(a) => rig_stream_fn_from_model_with_filter(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
                tool_def_filter,
            ),
            AnyAgentInner::OpenAI(a) => rig_stream_fn_from_model_with_filter(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
                tool_def_filter,
            ),
            AnyAgentInner::Anthropic(a) => rig_stream_fn_from_model_with_filter(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
                tool_def_filter,
            ),
            AnyAgentInner::Gemini(a) => rig_stream_fn_from_model_with_filter(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
                tool_def_filter,
            ),
            AnyAgentInner::DeepSeek(a) => rig_stream_fn_from_model_with_filter(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
                tool_def_filter,
            ),
            AnyAgentInner::Glm(a) => rig_stream_fn_from_model_with_filter(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
                tool_def_filter,
            ),
            AnyAgentInner::Ollama(a) => rig_stream_fn_from_model_with_filter(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
                tool_def_filter,
            ),
            AnyAgentInner::Custom(a) => rig_stream_fn_from_model_with_filter(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
                tool_def_filter,
            ),
        }
    }
}

pub fn create_client(
    provider_name: &str,
    api_key: Option<&str>,
    providers: &HashMap<String, ProviderEntry>,
) -> anyhow::Result<AnyClient> {
    client::create_client(provider_name, api_key, providers)
}

// Arity matches `build_agent_inner` — explicit DI signature kept
// grep-able, refactoring into a struct is tracked separately.
#[allow(clippy::too_many_arguments)]
pub async fn build_agent(
    model: AnyModel,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    question_tx: Option<QuestionSender>,
    plan_tx: Option<PlanSwitchSender>,
    bg_store: Option<crate::agent::tools::background::BackgroundStore>,
    #[cfg(feature = "lsp")] lsp_manager: Option<std::sync::Arc<crate::lsp::manager::LspManager>>,
    sandbox: Sandbox,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    // Live session id forwarded to SessionSearchTool so the model's
    // session_search calls exclude the current session. See dirge-502b.
    session_id: Option<String>,
) -> AnyAgent {
    let parent_model = model.clone();
    // Resolve the per-provider chunk timeout once here so every
    // spawn_runner / run_print call on the resulting agent uses the
    // same value. Provider name comes from the resolved CLI / config
    // (already factored into resolve_provider above the call site).
    let provider_name = cli.resolve_provider(cfg);
    let chunk_timeout = cfg.resolve_stream_chunk_timeout(&provider_name);
    // Capture the model identifier before `match model` consumes
    // it — forwarded into `AnyAgent.model_name` so `spawn_runner`
    // can plumb it through to the `tool_input_repair` telemetry.
    let model_name = parent_model.name();

    macro_rules! build_inner {
        ($m:expr, $variant:ident) => {{
            // Clone params before consuming them in
            // build_agent_inner so build_loop_tools has fresh
            // copies. PermCheck / AskSender / Sandbox / Arc<...>
            // are all Clone-cheap.
            let permission_for_loop = permission.clone();
            let ask_tx_for_loop = ask_tx.clone();
            let question_tx_for_loop = question_tx.clone();
            let plan_tx_for_loop = plan_tx.clone();
            let bg_store_for_loop = bg_store.clone();
            let sandbox_for_loop = sandbox.clone();
            let parent_model_for_loop = Some(parent_model.clone());
            #[cfg(feature = "lsp")]
            let lsp_for_loop = lsp_manager.clone();

            let (agent, cache, memory_provider) = builder::build_agent_inner(
                $m,
                cli,
                cfg,
                context,
                permission,
                ask_tx,
                question_tx.clone(),
                plan_tx.clone(),
                bg_store.clone(),
                #[cfg(feature = "lsp")]
                lsp_manager.clone(),
                sandbox.clone(),
                Some(parent_model.clone()),
                #[cfg(feature = "mcp")]
                mcp_manager,
                #[cfg(feature = "semantic")]
                semantic_manager,
                session_id.clone(),
            )
            .await;

            // Phase 4.5h-6: also build the LoopTool registry the
            // new agent_loop path dispatches against. Tools share
            // the same cache as the rig path (tool result
            // dedup) — though after h-6 the rig path no longer
            // runs, so this is effectively single-owner.
            //
            // Phase-3: build_loop_tools returns `(tools,
            // tool_def_filter)`. When `cfg.dynamic_tool_search`
            // is on, `tool_def_filter` is `Some` and a
            // `ToolSearchTool` has been registered inside `tools`
            // with the same Arc.
            let (loop_tools, dyn_search) = builder::build_loop_tools(
                cache.clone(),
                permission_for_loop,
                ask_tx_for_loop,
                question_tx_for_loop,
                plan_tx_for_loop,
                bg_store_for_loop,
                #[cfg(feature = "lsp")]
                lsp_for_loop,
                sandbox_for_loop,
                parent_model_for_loop,
                #[cfg(feature = "mcp")]
                mcp_manager,
                #[cfg(feature = "semantic")]
                semantic_manager,
                cli,
                cfg,
                session_id.clone(),
            )
            .await;

            // Phase 4.5h-6: extract the rig Agent's preamble so
            // the new path can pass it as Context.system_prompt.
            // rig's Agent has `preamble: Option<String>` public.
            // Phase-3: when dynamic-tool-search is on, append a
            // one-liner nudge so the model knows to call
            // `tool_search` before reaching for unknown tools.
            let mut preamble = agent.preamble.clone().unwrap_or_default();
            if dyn_search.is_some() {
                if !preamble.is_empty() {
                    preamble.push_str("\n\n");
                }
                preamble.push_str(crate::agent::prompt::DYNAMIC_TOOL_SEARCH_PROMPT);
            }

            let mut agent = AnyAgent::new(
                AnyAgentInner::$variant(agent),
                cache,
                chunk_timeout,
                loop_tools,
                preamble,
                model_name.clone(),
            );
            // dirge-7tvq: attach the memory provider so session-end
            // and pre-compress hooks can dispatch through the trait.
            if let Some(provider) = memory_provider {
                agent = agent.with_memory_provider(provider);
            }
            if let Some(ds) = dyn_search {
                agent.with_dynamic_tool_search(ds.filter, ds.registry)
            } else {
                agent
            }
        }};
    }

    let mut agent = match model {
        AnyModel::OpenRouter(m) => build_inner!(m, OpenRouter),
        AnyModel::OpenAI(m) => build_inner!(m, OpenAI),
        AnyModel::Anthropic(m) => build_inner!(m, Anthropic),
        AnyModel::Gemini(m) => build_inner!(m, Gemini),
        AnyModel::DeepSeek(m) => build_inner!(m, DeepSeek),
        AnyModel::Glm(m) => build_inner!(m, Glm),
        AnyModel::Ollama(m) => build_inner!(m, Ollama),
        AnyModel::Custom(m) => build_inner!(m, Custom),
    };

    // Phase 4 part 1 — dual-client escalation wiring.
    //
    // When the user has configured `escalation_provider` AND it
    // resolves to a DIFFERENT (alias, entry) than `ConfigRole::Default`,
    // build a second StreamFn that the loop will swap to for ONE call
    // after a repair-exhaustion or tree-sitter syntactic failure.
    //
    // The escalation route reuses:
    //   - The same tool definitions as the default loop (we just
    //     need a different model behind them).
    //   - The same chunk timeout — escalation should not be
    //     stricter or laxer than the default for stream chunk
    //     health.
    //
    // If `escalation_provider` is configured but the alias doesn't
    // resolve to a present entry AND isn't a built-in (this means
    // `resolve_role` returns None), surface an error rather than
    // silently disabling — the user asked for a feature and we
    // owe them a clear failure mode.
    if cfg.escalation_provider.is_some() {
        let default_role = cfg.resolve_role(crate::config::ConfigRole::Default);
        let escalation_role = cfg.resolve_role(crate::config::ConfigRole::Escalation);
        match (default_role, escalation_role) {
            (Some((default_alias, _)), Some((escalation_alias, escalation_entry))) => {
                // Equal aliases (case-insensitive) → escalation
                // has no effect; skip the duplicate client.
                if default_alias.eq_ignore_ascii_case(&escalation_alias) {
                    tracing::debug!(
                        target: "dirge::provider",
                        alias = %escalation_alias,
                        "escalation provider equals default; skipping duplicate client construction",
                    );
                } else {
                    match build_escalation_stream_fn(
                        &escalation_alias,
                        &escalation_entry,
                        &cfg.providers_map(),
                        chunk_timeout,
                        &agent.loop_tools,
                    ) {
                        Ok(stream_fn) => {
                            agent = agent.with_escalation(stream_fn, escalation_alias.clone());
                            tracing::info!(
                                target: "dirge::provider",
                                alias = %escalation_alias,
                                "dual-client escalation wired",
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                target: "dirge::provider",
                                alias = %escalation_alias,
                                error = %e,
                                "failed to construct escalation client; running without escalation",
                            );
                            eprintln!(
                                "warning: escalation_provider '{}' configured but client build failed: {}",
                                escalation_alias, e
                            );
                        }
                    }
                }
            }
            (_, None) => {
                // escalation_provider was set but resolve_role
                // returned None — alias doesn't name a present
                // entry and isn't a built-in. Hard-fail loudly per
                // the plan: don't silently disable.
                let alias = cfg.escalation_provider.clone().unwrap_or_default();
                tracing::error!(
                    target: "dirge::provider",
                    alias = %alias,
                    "escalation_provider configured but alias does not resolve to a known provider",
                );
                eprintln!(
                    "error: escalation_provider '{}' is configured but does not match any entry \
                     in `providers` or any built-in (anthropic/openai/deepseek/glm/gemini/ollama/openrouter). \
                     Either add it under `providers` or remove the `escalation_provider` setting.",
                    alias
                );
            }
            (None, _) => {
                // Default itself isn't resolvable — let the
                // caller's "no provider" error path handle it.
            }
        }
    }

    // F6 tier 3 — bounded critic wiring. Opt-in: only when the user has
    // set `critic_provider`. `resolve_role(Critic)` has no default
    // fallback, so an unset provider means no critic (no cost).
    if cfg.critic_provider.is_some() {
        match cfg.resolve_role(crate::config::ConfigRole::Critic) {
            Some((alias, entry)) => match build_critic_fn(&alias, &entry, &cfg.providers_map()) {
                Ok(critic_fn) => {
                    agent = agent.with_critic(critic_fn);
                    tracing::info!(target: "dirge::provider", alias = %alias, "in-loop critic wired");
                }
                Err(e) => {
                    tracing::error!(target: "dirge::provider", alias = %alias, error = %e, "failed to build critic client; running without critic");
                    eprintln!(
                        "warning: critic_provider '{alias}' configured but client build failed: {e}"
                    );
                }
            },
            None => {
                let alias = cfg.critic_provider.clone().unwrap_or_default();
                eprintln!(
                    "error: critic_provider '{alias}' is configured but does not match any entry \
                     in `providers` or any built-in. Either add it under `providers` or remove \
                     the `critic_provider` setting."
                );
            }
        }
    }

    // Phase 4 part 2 — context-depth reminder wiring.
    if let Some(threshold) = cfg.resolve_context_depth_threshold() {
        agent = agent.with_context_depth_reminder(threshold);
    }

    // dirge-9tfq — install the BackgroundStore on the agent so
    // `spawn_runner` can thread it into `LoopSpawnConfig.bg_store`,
    // wiring the subagent-completion follow-up path. Done after
    // the variant-dispatch `build_inner!` macro so every variant
    // gets the store. When `bg_store` is `None` (test paths,
    // `--no-tools`) the agent skips the wiring entirely.
    if let Some(store) = bg_store.as_ref() {
        agent = agent.with_bg_store(store.clone());
    }

    // dirge-z73i — background-review route wiring.
    //
    // When the user has configured `review_provider` AND it
    // resolves to a different (alias, entry) than `ConfigRole::Default`,
    // build a review-specific stream_fn so `spawn_review_runner` runs
    // through the configured cheaper / smarter model.
    //
    // Same equality short-circuit as escalation: if the resolved
    // alias equals the default, skip the duplicate client (the
    // fallback inside `spawn_review_runner_with_cache` produces an
    // identical request).
    if cfg.review_provider.is_some() {
        let default_role = cfg.resolve_role(crate::config::ConfigRole::Default);
        let review_role = cfg.resolve_role(crate::config::ConfigRole::Review);
        match (default_role, review_role) {
            (Some((default_alias, _)), Some((review_alias, review_entry))) => {
                if default_alias.eq_ignore_ascii_case(&review_alias) {
                    tracing::debug!(
                        target: "dirge::provider",
                        alias = %review_alias,
                        "review provider equals default; skipping duplicate client construction",
                    );
                } else {
                    match build_review_stream_fn(
                        &review_alias,
                        &review_entry,
                        &cfg.providers_map(),
                        chunk_timeout,
                        &agent.loop_tools,
                    ) {
                        Ok((stream_fn, model_name)) => {
                            agent = agent.with_review_route(
                                stream_fn,
                                review_alias.clone(),
                                model_name,
                            );
                            tracing::info!(
                                target: "dirge::provider",
                                alias = %review_alias,
                                "review-provider route wired",
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                target: "dirge::provider",
                                alias = %review_alias,
                                "failed to build review stream_fn: {e}",
                            );
                            eprintln!(
                                "error: failed to build review stream_fn for '{}': {}",
                                review_alias, e
                            );
                        }
                    }
                }
            }
            (_, None) => {
                let alias = cfg.review_provider.as_deref().unwrap_or("(unset)");
                tracing::warn!(
                    target: "dirge::provider",
                    alias = %alias,
                    "review_provider configured but alias does not resolve to a known provider",
                );
                eprintln!(
                    "error: review_provider '{}' is configured but does not match any entry \
                     in `providers` or any built-in. Either add it under `providers` or \
                     remove the `review_provider` setting.",
                    alias
                );
            }
            (None, _) => {
                // Default not resolvable — caller's "no provider"
                // error path handles it.
            }
        }
    }

    // dirge-nqr — per-run assistant-turn cap. CLI `--max-agent-turns`
    // > config `max_agent_turns` > default 100 (matches the existing
    // `cli::resolve_max_agent_turns` precedence). Always set: the
    // loop already had an implicit cap inherited from the legacy rig
    // builder; this wires it through the agent_loop path so `run_print`
    // and the interactive flow both honor it.
    agent = agent.with_max_turns(Some(cli.resolve_max_agent_turns(cfg)));

    agent
}

/// Phase 4 part 1: build a standalone StreamFn for the escalation
/// route. Constructs a fresh `AnyClient` for the alias, builds an
/// `AnyModel` against it using either the entry's `model` field or
/// the provider's default, then wraps with the same tool defs as
/// the main loop.
fn build_escalation_stream_fn(
    alias: &str,
    entry: &ProviderEntry,
    providers: &HashMap<String, ProviderEntry>,
    chunk_timeout: std::time::Duration,
    loop_tools: &[std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>],
) -> anyhow::Result<crate::agent::agent_loop::StreamFn> {
    use crate::agent::agent_loop::loop_tool_to_rig_definition;
    let client = create_client(alias, None, providers)?;
    let model_name = entry
        .model
        .clone()
        .unwrap_or_else(|| default_model_for(alias).to_string());
    let model = client.completion_model(model_name);
    let tool_defs: Vec<rig::completion::ToolDefinition> = loop_tools
        .iter()
        .map(|t| loop_tool_to_rig_definition(t.as_ref()))
        .collect();
    Ok(model.build_stream_fn(tool_defs, chunk_timeout, Some(alias.to_string())))
}

/// F6 tier 3: build the bounded-critic callback. Constructs a fresh
/// client for the critic alias and returns a [`CriticFn`] that runs one
/// completion (via `summarize_with_model`) per call. No tools — the
/// critic only reads a transcript and returns a verdict.
fn build_critic_fn(
    alias: &str,
    entry: &ProviderEntry,
    providers: &HashMap<String, ProviderEntry>,
) -> anyhow::Result<crate::agent::agent_loop::critic::CriticFn> {
    let client = std::sync::Arc::new(create_client(alias, None, providers)?);
    let model_name = entry
        .model
        .clone()
        .unwrap_or_else(|| default_model_for(alias).to_string());
    Ok(std::sync::Arc::new(move |prompt: String| {
        let client = client.clone();
        let model_name = model_name.clone();
        Box::pin(async move {
            let model = client.completion_model(model_name);
            summarize::summarize_with_model(model, prompt).await
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>>
    }))
}

/// dirge-0g6i: build the LLM auto-approval evaluator from a resolved
/// `approval_provider`. Mirrors [`build_critic_fn`] — same client + model
/// resolution and the SAME shared one-shot helper
/// (`summarize::oneshot_with_model`) — but with the approval system
/// preamble and a verdict parser. Returns an `ApprovalFn` the permission
/// chokepoint calls instead of prompting the human.
pub fn build_approval_fn(
    alias: &str,
    entry: &ProviderEntry,
    providers: &HashMap<String, ProviderEntry>,
) -> anyhow::Result<crate::permission::approval::ApprovalFn> {
    use crate::permission::approval::{
        ApprovalDecision, ApprovalRequest, EVALUATOR_PREAMBLE, build_evaluator_prompt,
        parse_decision,
    };
    let client = std::sync::Arc::new(create_client(alias, None, providers)?);
    let model_name = entry
        .model
        .clone()
        .unwrap_or_else(|| default_model_for(alias).to_string());
    Ok(std::sync::Arc::new(move |req: ApprovalRequest| {
        let client = client.clone();
        let model_name = model_name.clone();
        Box::pin(async move {
            let model = client.completion_model(model_name);
            let prompt = build_evaluator_prompt(&req);
            let raw = summarize::oneshot_with_model(model, EVALUATOR_PREAMBLE, prompt).await?;
            Ok::<ApprovalDecision, anyhow::Error>(parse_decision(&raw))
        })
            as std::pin::Pin<
                Box<dyn std::future::Future<Output = anyhow::Result<ApprovalDecision>> + Send>,
            >
    }))
}

/// dirge-z73i: build a stream_fn for the background-review path,
/// routed through `ConfigRole::Review`. Only the memory + skill tools
/// are baked into the request — the review fork's `loop_tools` is
/// filtered to the same set in `spawn_review_runner_with_cache`,
/// so the model sees a tool catalog that matches what the dispatcher
/// will actually accept. Returns `(stream_fn, model_name)` so the
/// caller can stash the model identifier alongside the stream_fn for
/// telemetry (`LoopConfig.model_name`).
fn build_review_stream_fn(
    alias: &str,
    entry: &ProviderEntry,
    providers: &HashMap<String, ProviderEntry>,
    chunk_timeout: std::time::Duration,
    loop_tools: &[std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>],
) -> anyhow::Result<(crate::agent::agent_loop::StreamFn, String)> {
    use crate::agent::agent_loop::loop_tool_to_rig_definition;
    let client = create_client(alias, None, providers)?;
    let model_name = entry
        .model
        .clone()
        .unwrap_or_else(|| default_model_for(alias).to_string());
    let model = client.completion_model(model_name.clone());
    // Review path uses ONLY memory + skill — match what
    // `spawn_review_runner_with_cache` puts in `cfg.tools` so
    // the request body and the dispatcher agree.
    let tool_defs: Vec<rig::completion::ToolDefinition> = loop_tools
        .iter()
        .filter(|t| {
            let n = t.name();
            n == "memory" || n == "skill"
        })
        .map(|t| loop_tool_to_rig_definition(t.as_ref()))
        .collect();
    let stream_fn = model.build_stream_fn(tool_defs, chunk_timeout, Some(alias.to_string()));
    Ok((stream_fn, model_name))
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
