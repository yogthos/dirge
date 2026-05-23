use std::collections::HashMap;
use std::sync::OnceLock;

use rig::agent::Agent;
use rig::client::CompletionClient;
use rig::completion::{CompletionModel, Message, Prompt};
use rig::providers::{anthropic, gemini, ollama, openai, openrouter};
use rig::streaming::StreamingChat;

use crate::agent::builder;
use crate::agent::prompt;
use crate::agent::runner::{self, AgentRunner};
use crate::agent::tools::ToolCache;
use crate::agent::tools::plan::PlanSwitchSender;
use crate::agent::tools::question::QuestionSender;
use crate::cli::Cli;
use crate::config::{Config, CustomProviderConfig};
use crate::context::ContextFiles;
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
}

pub fn resolve_provider_info(
    name: &str,
    custom_providers: &HashMap<String, CustomProviderConfig>,
) -> Option<ProviderInfo> {
    // Config-declared custom providers win on name collision —
    // user intent always trumps plugin defaults.
    // #2 fix: lowercase-fallback lookup so `--provider My-VLLM` finds
    // a `custom_providers["my-vllm"]` config entry. parse_provider
    // (for built-ins) is already case-insensitive; matching the same
    // convention here removes a silent miss.
    let lower = name.to_ascii_lowercase();
    if let Some(custom) = custom_providers
        .get(name)
        .or_else(|| custom_providers.get(&lower))
    {
        let kind = parse_provider(&custom.provider_type)?;
        return Some(ProviderInfo {
            kind,
            base_url: Some(custom.base_url.clone()),
            api_key_env: custom.api_key_env.clone(),
        });
    }
    // Then plugin-registered providers from `harness/register-provider`.
    // Installed once at startup after plugin load; never mutated again
    // in this process.
    if let Some(custom) = plugin_provider(name).or_else(|| plugin_provider(&lower)) {
        let kind = parse_provider(&custom.provider_type)?;
        return Some(ProviderInfo {
            kind,
            base_url: Some(custom.base_url),
            api_key_env: custom.api_key_env,
        });
    }
    let kind = parse_provider(name)?;
    Some(ProviderInfo {
        kind,
        base_url: None,
        api_key_env: None,
    })
}

/// Process-global map of plugin-registered providers, populated once
/// after plugin load. Stored separately from `cfg.custom_providers`
/// so a `/reload` (future) can swap plugin providers without
/// disturbing the user's persistent config.
static PLUGIN_PROVIDERS: OnceLock<HashMap<String, CustomProviderConfig>> = OnceLock::new();

/// Install the plugin-registered provider map. Only the first call
/// wins (OnceLock semantics) — sufficient for current behavior where
/// plugins re-register every startup and never change at runtime.
/// Returns the installed-or-already-installed map size so callers
/// can log a confirmation.
#[cfg_attr(not(feature = "plugin"), allow(dead_code))]
pub fn install_plugin_providers(map: HashMap<String, CustomProviderConfig>) -> usize {
    let size = map.len();
    let _ = PLUGIN_PROVIDERS.set(map);
    size
}

fn plugin_provider(name: &str) -> Option<CustomProviderConfig> {
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
        _ => &[],
    }
}

fn resolve_api_key(
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
        let conversation = serialize_conversation(messages);

        let prompt = prompt::COMPACTION_PROMPT
            .replace("{conversation}", &conversation)
            .replace("{previous_summary}", previous_summary.unwrap_or("(none)"))
            .replace("{instructions}", instructions.unwrap_or("(none)"));

        let model = self.completion_model(model_name.to_string());
        let response = summarize_with_model(model, prompt).await?;
        Ok(response)
    }
}

async fn summarize_with_model(model: AnyModel, prompt: String) -> anyhow::Result<String> {
    match model {
        AnyModel::OpenRouter(m) => run_summarizer(m, prompt).await,
        AnyModel::OpenAI(m) => run_summarizer(m, prompt).await,
        AnyModel::Anthropic(m) => run_summarizer(m, prompt).await,
        AnyModel::Gemini(m) => run_summarizer(m, prompt).await,
        AnyModel::DeepSeek(m) => run_summarizer(m, prompt).await,
        AnyModel::Glm(m) => run_summarizer(m, prompt).await,
        AnyModel::Ollama(m) => run_summarizer(m, prompt).await,
        AnyModel::Custom(m) => run_summarizer(m, prompt).await,
    }
}

async fn run_summarizer<M>(model: M, prompt: String) -> anyhow::Result<String>
where
    M: CompletionModel + Clone + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    // Retry loop. The summarizer is invoked by `/compress`, often
    // exactly when the user's context is about to overflow — a single
    // transient 503 or rate-limit would otherwise permanently fail
    // the compaction. Use the same `RecoveryPolicy` shape as the main
    // agent so the user sees consistent retry semantics. No partial
    // output is streamed to the UI for the summarizer, so retry is
    // safe and the response is replaced wholesale on each attempt.
    use crate::agent::recovery::{self, RecoveryPolicy};
    let policy = RecoveryPolicy::default();
    let mut attempts: usize = 0;
    loop {
        let agent = rig::agent::AgentBuilder::new(model.clone())
            .preamble("You are a conversation summarizer.")
            .build();

        let mut stream = agent
            .stream_chat(prompt.clone(), Vec::<Message>::new())
            .multi_turn(1)
            .await;

        let mut response = String::new();
        let mut error: Option<String> = None;
        use futures::StreamExt;
        while let Some(item) = stream.next().await {
            match item {
                Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                    rig::streaming::StreamedAssistantContent::Text(text),
                )) => response.push_str(&text.text),
                Ok(rig::agent::MultiTurnStreamItem::FinalResponse(res)) => {
                    response = res.response().to_string();
                    break;
                }
                Err(e) => {
                    error = Some(e.to_string());
                    break;
                }
                _ => {}
            }
        }

        if let Some(msg) = error {
            let kind = recovery::classify_error(&msg);
            if policy.should_retry(attempts, kind) {
                let delay = policy.backoff_duration_for_msg(attempts, &msg);
                tracing::info!(
                    "summarizer retry {}/{} after {:?} ({:?}): {}",
                    attempts + 1,
                    policy.max_retries(),
                    delay,
                    kind,
                    msg
                );
                tokio::time::sleep(delay).await;
                attempts += 1;
                continue;
            }
            return Err(anyhow::anyhow!("Compression failed: {}", msg));
        }

        if response.is_empty() {
            anyhow::bail!("Compression returned empty response");
        }

        return Ok(response);
    }
}

fn serialize_conversation(messages: &[SessionMessage]) -> String {
    // C7 (audit fix): include each assistant message's tool calls.
    // Previously only `[role]: content` was emitted; msg.tool_calls
    // (args, results, errors) were invisible to the summarizer. For
    // tool-heavy sessions that's the bulk of where state diverged
    // from "just chat" — after compress the LLM had no record that
    // bash/read/edit ever ran. Pi's compaction prompt includes
    // structured tool I/O (see pi/.../compaction).
    let mut result = String::new();
    for msg in messages {
        let role_tag = match msg.role {
            crate::session::MessageRole::User => "User",
            crate::session::MessageRole::Assistant => "Assistant",
            crate::session::MessageRole::System => "System",
        };
        result.push_str(&format!("[{}]: {}\n", role_tag, msg.content));
        for tc in &msg.tool_calls {
            // Args serialized compactly — the summarizer cares about
            // which tool fired with what shape, not pretty formatting.
            let args_str = serde_json::to_string(&tc.args).unwrap_or_else(|_| "{}".to_string());
            result.push_str(&format!("[Tool: {}({})]\n", tc.name, args_str));
            match &tc.state {
                crate::session::ToolCallState::Completed { result: out } => {
                    // Cap each individual tool output at 2KB to keep
                    // the summarizer prompt bounded — full output for
                    // a 10MB grep would dominate the prompt with low
                    // signal-per-byte. The truncation here is per-tool
                    // (not whole-conversation like the C6 fix removed)
                    // so structure is preserved.
                    const PER_TOOL_CAP: usize = 2048;
                    if out.len() > PER_TOOL_CAP {
                        let trimmed: String = out.chars().take(PER_TOOL_CAP).collect();
                        result.push_str(&format!(
                            "[Result: {} ... (truncated, {} bytes total)]\n",
                            trimmed,
                            out.len()
                        ));
                    } else {
                        result.push_str(&format!("[Result: {}]\n", out));
                    }
                }
                crate::session::ToolCallState::Interrupted => {
                    result.push_str("[Result: <interrupted>]\n");
                }
                crate::session::ToolCallState::Failed { error } => {
                    result.push_str(&format!("[Result: <failed: {}>]\n", error));
                }
            }
        }
        result.push('\n');
    }
    result
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
        macro_rules! btw {
            ($m:expr) => {{
                let agent = rig::agent::AgentBuilder::new($m).preamble(preamble).build();
                Ok(agent.prompt(prompt).await?)
            }};
        }
        match self {
            AnyModel::OpenRouter(m) => btw!(m.clone()),
            AnyModel::OpenAI(m) => btw!(m.clone()),
            AnyModel::Anthropic(m) => btw!(m.clone()),
            AnyModel::Gemini(m) => btw!(m.clone()),
            AnyModel::DeepSeek(m) => btw!(m.clone()),
            AnyModel::Glm(m) => btw!(m.clone()),
            AnyModel::Ollama(m) => btw!(m.clone()),
            AnyModel::Custom(m) => btw!(m.clone()),
        }
    }
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
    ) -> Self {
        AnyAgent {
            inner,
            cache,
            chunk_timeout,
            loop_tools,
            preamble,
        }
    }

    pub async fn run_print(&self, prompt: &str, max_turns: usize) -> anyhow::Result<String> {
        let t = self.chunk_timeout;
        match &self.inner {
            AnyAgentInner::OpenRouter(a) => runner::run_print(a, prompt, max_turns, t).await,
            AnyAgentInner::OpenAI(a) => runner::run_print(a, prompt, max_turns, t).await,
            AnyAgentInner::Anthropic(a) => runner::run_print(a, prompt, max_turns, t).await,
            AnyAgentInner::Gemini(a) => runner::run_print(a, prompt, max_turns, t).await,
            AnyAgentInner::DeepSeek(a) => runner::run_print(a, prompt, max_turns, t).await,
            AnyAgentInner::Glm(a) => runner::run_print(a, prompt, max_turns, t).await,
            AnyAgentInner::Ollama(a) => runner::run_print(a, prompt, max_turns, t).await,
            AnyAgentInner::Custom(a) => runner::run_print(a, prompt, max_turns, t).await,
        }
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

    pub fn spawn_runner(self, prompt: String, history: Vec<Message>) -> AgentRunner {
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

        // Build the StreamFn (4.5h-2 + 4.5h-3 chunk timeout).
        let inner_stream_fn = self.build_stream_fn(tool_defs);
        // Wrap with retry (4.5g) so transient Network / RateLimit
        // errors auto-retry with exponential backoff + Retry-After.
        let stream_fn = retrying_stream_fn(inner_stream_fn, RecoveryPolicy::default());

        // Merge any system-message content from the history
        // (e.g. compaction summary) into the loop's
        // Context.system_prompt. The Agent's preamble (model
        // identity + tool docs) is the base; session-side
        // system messages append.
        let history_preamble = rig_history_system_prompt(&history);
        let system_prompt = if history_preamble.is_empty() {
            self.preamble.clone()
        } else {
            format!("{}\n\n{}", self.preamble, history_preamble)
        };

        // Convert rig history → loop messages (Session-side
        // user/assistant/toolResult shapes).
        let loop_history = rig_history_to_loop_messages(history);

        let mut cfg = LoopSpawnConfig::minimal(stream_fn, prompt);
        cfg.system_prompt = system_prompt;
        cfg.history = loop_history;
        cfg.tools = self.loop_tools;
        cfg.provider_name = Some(provider_name);
        #[cfg(feature = "plugin")]
        {
            cfg.plugin_mgr = crate::plugin::hook::global();
        }
        // steering_queue stays None for h-6 — UI's existing
        // interjection_queue isn't shared yet. interject_tx →
        // signal.cancel() handles the legacy "stop the run"
        // case via the LoopRunner::into_agent_runner adapter.
        // A follow-up UI commit will share the queue so the
        // model observes interjections mid-run.

        let loop_runner = spawn_loop_runner(cfg);
        loop_runner.into_agent_runner()
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
        use crate::agent::agent_loop::rig_stream_fn_from_model_with_provider;
        let chunk_timeout = self.chunk_timeout;
        let provider = Some(self.provider_name().to_string());
        match &self.inner {
            AnyAgentInner::OpenRouter(a) => rig_stream_fn_from_model_with_provider(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
            ),
            AnyAgentInner::OpenAI(a) => rig_stream_fn_from_model_with_provider(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
            ),
            AnyAgentInner::Anthropic(a) => rig_stream_fn_from_model_with_provider(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
            ),
            AnyAgentInner::Gemini(a) => rig_stream_fn_from_model_with_provider(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
            ),
            AnyAgentInner::DeepSeek(a) => rig_stream_fn_from_model_with_provider(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
            ),
            AnyAgentInner::Glm(a) => rig_stream_fn_from_model_with_provider(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
            ),
            AnyAgentInner::Ollama(a) => rig_stream_fn_from_model_with_provider(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
            ),
            AnyAgentInner::Custom(a) => rig_stream_fn_from_model_with_provider(
                (*a.model).clone(),
                tools.clone(),
                Some(chunk_timeout),
                provider,
            ),
        }
    }
}

pub fn create_client(
    provider_name: &str,
    api_key: Option<&str>,
    custom_providers: &HashMap<String, CustomProviderConfig>,
) -> anyhow::Result<AnyClient> {
    let info = resolve_provider_info(provider_name, custom_providers).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown provider: {}. Supported providers: openrouter, openai, anthropic, gemini, deepseek, glm, ollama, custom",
            provider_name
        )
    })?;

    let key = resolve_api_key(info.kind, info.api_key_env.as_deref(), api_key)?;

    let base_url = match info.kind {
        ProviderKind::DeepSeek => Some(
            std::env::var("DEEPSEEK_BASE_URL")
                .unwrap_or_else(|_| "https://api.deepseek.com/v1".to_string()),
        ),
        ProviderKind::Glm => Some(
            std::env::var("GLM_BASE_URL")
                .unwrap_or_else(|_| "https://open.bigmodel.cn/api/coding/paas/v4".to_string()),
        ),
        ProviderKind::Custom => info
            .base_url
            .or_else(|| std::env::var("CUSTOM_BASE_URL").ok()),
        _ => info.base_url,
    };

    match info.kind {
        ProviderKind::OpenAI => {
            let mut b = openai::CompletionsClient::builder().api_key(&key);
            if let Some(base_url) = &base_url {
                b = b.base_url(base_url);
            }
            Ok(AnyClient::OpenAI(b.build()?))
        }
        ProviderKind::Anthropic => {
            let mut b = anthropic::Client::builder().api_key(&key);
            if let Some(base_url) = &base_url {
                b = b.base_url(base_url);
            }
            Ok(AnyClient::Anthropic(b.build()?))
        }
        ProviderKind::Gemini => {
            let mut b = gemini::Client::builder().api_key(&key);
            if let Some(base_url) = &base_url {
                b = b.base_url(base_url);
            }
            Ok(AnyClient::Gemini(b.build()?))
        }
        ProviderKind::DeepSeek => {
            let b = openai::CompletionsClient::builder()
                .api_key(&key)
                .base_url(base_url.as_deref().unwrap_or("https://api.deepseek.com/v1"));
            Ok(AnyClient::DeepSeek(b.build()?))
        }
        ProviderKind::Glm => {
            let b = openai::CompletionsClient::builder().api_key(&key).base_url(
                base_url
                    .as_deref()
                    .unwrap_or("https://open.bigmodel.cn/api/coding/paas/v4"),
            );
            Ok(AnyClient::Glm(b.build()?))
        }
        ProviderKind::Ollama => {
            let key: ollama::OllamaApiKey = key.as_str().into();
            let mut b = ollama::Client::builder().api_key(key);
            if let Some(base_url) = &base_url {
                b = b.base_url(base_url);
            }
            Ok(AnyClient::Ollama(b.build()?))
        }
        ProviderKind::OpenRouter => {
            let mut b = openrouter::Client::builder().api_key(&key);
            if let Some(base_url) = &base_url {
                b = b.base_url(base_url);
            }
            Ok(AnyClient::OpenRouter(b.build()?))
        }
        ProviderKind::Custom => {
            let base_url = base_url.ok_or_else(|| {
                anyhow::anyhow!(
                    "CUSTOM_BASE_URL environment variable must be set for custom provider"
                )
            })?;
            let b = openai::CompletionsClient::builder()
                .api_key(&key)
                .base_url(&base_url);
            Ok(AnyClient::Custom(b.build()?))
        }
    }
}

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
) -> AnyAgent {
    let parent_model = model.clone();
    // Resolve the per-provider chunk timeout once here so every
    // spawn_runner / run_print call on the resulting agent uses the
    // same value. Provider name comes from the resolved CLI / config
    // (already factored into resolve_provider above the call site).
    let provider_name = cli.resolve_provider(cfg);
    let chunk_timeout = cfg.resolve_stream_chunk_timeout(&provider_name);

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

            let (agent, cache) = builder::build_agent_inner(
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
            )
            .await;

            // Phase 4.5h-6: also build the LoopTool registry the
            // new agent_loop path dispatches against. Tools share
            // the same cache as the rig path (tool result
            // dedup) — though after h-6 the rig path no longer
            // runs, so this is effectively single-owner.
            let loop_tools = builder::build_loop_tools(
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
            )
            .await;

            // Phase 4.5h-6: extract the rig Agent's preamble so
            // the new path can pass it as Context.system_prompt.
            // rig's Agent has `preamble: Option<String>` public.
            let preamble = agent.preamble.clone().unwrap_or_default();

            AnyAgent::new(
                AnyAgentInner::$variant(agent),
                cache,
                chunk_timeout,
                loop_tools,
                preamble,
            )
        }};
    }

    match model {
        AnyModel::OpenRouter(m) => build_inner!(m, OpenRouter),
        AnyModel::OpenAI(m) => build_inner!(m, OpenAI),
        AnyModel::Anthropic(m) => build_inner!(m, Anthropic),
        AnyModel::Gemini(m) => build_inner!(m, Gemini),
        AnyModel::DeepSeek(m) => build_inner!(m, DeepSeek),
        AnyModel::Glm(m) => build_inner!(m, Glm),
        AnyModel::Ollama(m) => build_inner!(m, Ollama),
        AnyModel::Custom(m) => build_inner!(m, Custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build an env-lookup closure backed by a HashMap. Avoids
    /// mutating process-wide env vars — `std::env::set_var` is
    /// thread-unsafe and the previous test suite raced under
    /// parallel `cargo test`, producing intermittent failures.
    fn mock_env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn auto_detect_returns_none_when_no_vars_set() {
        assert_eq!(auto_detect_provider_from(mock_env(&[])), None);
    }

    #[test]
    fn auto_detect_finds_deepseek_when_key_set() {
        let env = mock_env(&[("DEEPSEEK_API_KEY", "sk-test-123")]);
        assert_eq!(auto_detect_provider_from(env), Some("deepseek"));
    }

    #[test]
    fn auto_detect_finds_openai_when_key_set() {
        let env = mock_env(&[("OPENAI_API_KEY", "sk-test-456")]);
        assert_eq!(auto_detect_provider_from(env), Some("openai"));
    }

    #[test]
    fn auto_detect_skips_empty_var() {
        let env = mock_env(&[("DEEPSEEK_API_KEY", ""), ("OPENAI_API_KEY", "sk-test-789")]);
        assert_eq!(auto_detect_provider_from(env), Some("openai"));
    }

    #[test]
    fn auto_detect_returns_first_match_in_order() {
        let env = mock_env(&[("DEEPSEEK_API_KEY", "sk-ds"), ("OPENAI_API_KEY", "sk-oai")]);
        assert_eq!(auto_detect_provider_from(env), Some("deepseek"));
    }

    /// Cover every provider in the autodetect list — guards
    /// against accidentally dropping or reordering an entry.
    #[test]
    fn auto_detect_each_provider_in_isolation() {
        for &(env_var, expected) in PROVIDER_AUTODETECT_ORDER {
            let env = mock_env(&[(env_var, "sk-x")]);
            assert_eq!(
                auto_detect_provider_from(env),
                Some(expected),
                "env_var={env_var}",
            );
        }
    }

    /// `ZHIPU_API_KEY` alone resolves to glm provider — Zhipu's
    /// canonical env-var name doesn't require users to alias.
    #[test]
    fn auto_detect_zhipu_api_key_resolves_to_glm() {
        let env = mock_env(&[("ZHIPU_API_KEY", "fake-zhipu-key")]);
        assert_eq!(auto_detect_provider_from(env), Some("glm"));
    }

    /// When BOTH GLM_API_KEY and ZHIPU_API_KEY are set, the
    /// dirge-primary GLM_API_KEY wins (it's earlier in
    /// PROVIDER_AUTODETECT_ORDER). The fallback only fires when
    /// the primary is absent.
    #[test]
    fn auto_detect_glm_api_key_wins_over_zhipu_when_both_set() {
        let env = mock_env(&[("GLM_API_KEY", "primary"), ("ZHIPU_API_KEY", "fallback")]);
        // Both map to "glm" so the answer is the same kind, but
        // this guards against a future reordering breaking the
        // primary-first invariant. We can't observe WHICH var
        // resolve_api_key picked from auto_detect alone — that's
        // tested below.
        assert_eq!(auto_detect_provider_from(env), Some("glm"));
    }

    /// `provider_env_var_fallbacks` lists ZHIPU_API_KEY for GLM
    /// and nothing else for other providers.
    #[test]
    fn fallback_list_is_glm_specific() {
        assert_eq!(
            provider_env_var_fallbacks(ProviderKind::Glm),
            &["ZHIPU_API_KEY"]
        );
        for kind in [
            ProviderKind::OpenAI,
            ProviderKind::Anthropic,
            ProviderKind::Gemini,
            ProviderKind::DeepSeek,
            ProviderKind::OpenRouter,
            ProviderKind::Ollama,
            ProviderKind::Custom,
        ] {
            assert!(
                provider_env_var_fallbacks(kind).is_empty(),
                "no fallback expected for {kind:?}",
            );
        }
    }

    // ============================================================
    // Phase 4.5h-2: AnyAgent::build_stream_fn dispatch tests
    // ============================================================

    /// Build a real `AnyAgent` from an openai-shaped client +
    /// model. The Client::new doesn't connect (no network until
    /// the first request), so this works in unit tests.
    ///
    /// Use `completions_api()` to get the chat-completion model
    /// (the variant `AnyAgentInner::OpenAI` holds); the default
    /// `completion_model` on a fresh `Client` returns the
    /// responses-api model, which is a different type.
    fn build_openai_any_agent() -> AnyAgent {
        use rig::providers::openai;
        let client = openai::Client::new("test-key")
            .expect("openai Client::new should work")
            .completions_api();
        let model = client.completion_model("gpt-4o");
        let agent = rig::agent::AgentBuilder::new(model).build();
        AnyAgent::new(
            AnyAgentInner::OpenAI(agent),
            ToolCache::new(),
            std::time::Duration::from_secs(300),
            Vec::new(),    // loop_tools — empty for test fixture
            String::new(), // preamble — empty for test fixture
        )
    }

    /// `build_stream_fn` returns a `Send + Sync + 'static`
    /// `StreamFn` for the OpenAI variant. Compile-time check —
    /// if the bounds don't match the type would fail to
    /// construct.
    #[test]
    fn build_stream_fn_returns_send_sync_static() {
        fn assert_send_sync_static<T: Send + Sync + 'static>(_: &T) {}
        let agent = build_openai_any_agent();
        let stream_fn = agent.build_stream_fn(vec![]);
        assert_send_sync_static(&stream_fn);
    }

    /// `build_stream_fn` is callable as a `Fn` (multi-call) —
    /// the loop invokes it once per turn. Verify by calling
    /// twice and checking both invocations produce streams.
    #[tokio::test]
    async fn build_stream_fn_is_multi_callable() {
        use crate::agent::agent_loop::LlmContext;
        use crate::agent::agent_loop::tool::AbortSignal;
        use futures::stream::StreamExt;

        let agent = build_openai_any_agent();
        let stream_fn = agent.build_stream_fn(vec![]);

        // Call once with an empty context — should emit an
        // Error event (no prompt) without panicking.
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: vec![],
        };
        let mut s = stream_fn(
            ctx,
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        );
        let first = s.next().await;
        assert!(first.is_some(), "first call should produce events");

        // Call again — same closure, same Arc, fresh stream.
        let ctx2 = LlmContext {
            system_prompt: String::new(),
            messages: vec![],
        };
        let mut s2 = stream_fn(
            ctx2,
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        );
        let second = s2.next().await;
        assert!(second.is_some(), "second call should also produce events");
    }

    /// All 8 `AnyAgentInner` variants compile through
    /// `build_stream_fn` — the match arms cover the full enum,
    /// and the bounds on `rig_stream_fn_from_model<M>` are
    /// satisfied by each provider's `CompletionModel`.
    ///
    /// This test exists primarily as a compile-time
    /// canary: if a future provider variant gets added to
    /// `AnyAgentInner` without a matching arm in
    /// `build_stream_fn`, the build breaks. Runtime
    /// dispatch is exercised by the OpenAI-backed tests
    /// above.
    #[test]
    fn build_stream_fn_covers_all_variants_compile_time() {
        // Just constructs one variant and calls
        // build_stream_fn; the rest are validated by the
        // match-arm exhaustiveness check at compile time.
        let agent = build_openai_any_agent();
        let _ = agent.build_stream_fn(vec![]);
    }

    // --- C6/C7: compaction prefix is full + includes tool calls -----

    use crate::session::{MessageRole, SessionMessage, ToolCallEntry, ToolCallState};
    use compact_str::CompactString;

    fn sm(role: MessageRole, content: &str, tool_calls: Vec<ToolCallEntry>) -> SessionMessage {
        SessionMessage {
            role,
            content: CompactString::from(content),
            estimated_tokens: 0,
            id: CompactString::from("test-id"),
            timestamp: 0,
            tool_calls,
        }
    }

    /// C7: assistant tool calls land in the serialized form with
    /// args + result. Previously they were dropped entirely so the
    /// summarizer saw only `[Assistant]: <text>` with no record
    /// that bash/read/edit ever ran.
    #[test]
    fn serialize_conversation_includes_tool_calls() {
        let msgs = vec![
            sm(MessageRole::User, "list rust files", vec![]),
            sm(
                MessageRole::Assistant,
                "I'll find them.",
                vec![ToolCallEntry {
                    id: "call_1".into(),
                    name: "find_files".into(),
                    args: serde_json::json!({"pattern": "*.rs"}),
                    state: ToolCallState::Completed {
                        result: "src/main.rs\nsrc/lib.rs".into(),
                    },
                }],
            ),
        ];
        let out = serialize_conversation(&msgs);
        assert!(out.contains("[User]"), "missing role tag: {out}");
        assert!(
            out.contains("[Tool: find_files("),
            "missing tool call line: {out}"
        );
        assert!(
            out.contains("src/main.rs"),
            "missing tool result content: {out}"
        );
    }

    /// C7: interrupted + failed tool calls also surface.
    #[test]
    fn serialize_conversation_marks_interrupted_and_failed() {
        let msgs = vec![sm(
            MessageRole::Assistant,
            "trying",
            vec![
                ToolCallEntry {
                    id: "a".into(),
                    name: "bash".into(),
                    args: serde_json::json!({"command": "sleep 9999"}),
                    state: ToolCallState::Interrupted,
                },
                ToolCallEntry {
                    id: "b".into(),
                    name: "read".into(),
                    args: serde_json::json!({"path": "/missing"}),
                    state: ToolCallState::Failed {
                        error: "no such file".into(),
                    },
                },
            ],
        )];
        let out = serialize_conversation(&msgs);
        assert!(out.contains("<interrupted>"), "got: {out}");
        assert!(out.contains("<failed: no such file>"), "got: {out}");
    }

    /// C7 bound: a single tool result over the per-tool cap (2KB)
    /// truncates with a marker, preserving structure of the rest
    /// of the conversation.
    #[test]
    fn serialize_conversation_truncates_huge_tool_results() {
        let big: String = "x".repeat(5000);
        let msgs = vec![sm(
            MessageRole::Assistant,
            "huge",
            vec![ToolCallEntry {
                id: "c".into(),
                name: "grep".into(),
                args: serde_json::json!({"pattern": "."}),
                state: ToolCallState::Completed { result: big },
            }],
        )];
        let out = serialize_conversation(&msgs);
        assert!(
            out.contains("(truncated, 5000 bytes total)"),
            "expected truncation marker; got: {out}"
        );
    }

    /// C6: a long full-conversation prefix is NOT truncated by the
    /// caller-side 6000-char cap any more. compress_messages no
    /// longer slices `conversation`; the full string reaches the
    /// summarizer. Regression test the unchanged-passthrough via
    /// serialize_conversation's length on a large input.
    #[test]
    fn serialize_conversation_returns_full_prefix() {
        let msgs: Vec<SessionMessage> = (0..200)
            .map(|i| sm(MessageRole::Assistant, &format!("turn {i}"), vec![]))
            .collect();
        let out = serialize_conversation(&msgs);
        // 200 turns × ~10 chars each = ~2000 chars; below the old
        // 6000 cap but the principle still holds: the function is
        // a pure mapper, no length cap. Confirm by checking the
        // last turn is present.
        assert!(out.contains("turn 199"), "tail must be present: {out}");
        assert!(out.contains("turn 0"), "head must be present: {out}");
    }
}
