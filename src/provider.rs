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
    if let Some(custom) = custom_providers.get(name) {
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
    if let Some(custom) = plugin_provider(name) {
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

fn resolve_api_key(
    kind: ProviderKind,
    api_key_env_override: Option<&str>,
    cli_key: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(key) = cli_key.filter(|k| !k.is_empty()) {
        tracing::warn!(
            "API key provided via --api-key is visible in process listings (/proc/*/cmdline). Use the {} environment variable instead.",
            provider_env_var(kind)
        );
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

    if kind == ProviderKind::Ollama {
        return Ok(String::new());
    }

    if kind == ProviderKind::Custom {
        return Ok(String::new());
    }

    anyhow::bail!(
        "No API key found for {kind:?}. Set the {env_var} environment variable or pass --api-key."
    )
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
        let conversation = serialize_conversation(messages);
        let conversation = if conversation.len() > 6000 {
            let truncated: String = conversation.chars().take(6000).collect();
            let mut result = truncated;
            result.push_str("\n\n... [truncated]");
            result
        } else {
            conversation
        };

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
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let agent = rig::agent::AgentBuilder::new(model)
        .preamble("You are a conversation summarizer.")
        .build();

    let mut stream = agent
        .stream_chat(prompt, Vec::<Message>::new())
        .multi_turn(1)
        .await;

    let mut response = String::new();
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
            Err(e) => return Err(anyhow::anyhow!("Compression failed: {}", e)),
            _ => {}
        }
    }

    if response.is_empty() {
        anyhow::bail!("Compression returned empty response");
    }

    Ok(response)
}

fn serialize_conversation(messages: &[SessionMessage]) -> String {
    let mut result = String::new();
    for msg in messages {
        let role_tag = match msg.role {
            crate::session::MessageRole::User => "User",
            crate::session::MessageRole::Assistant => "Assistant",
            crate::session::MessageRole::System => "System",
        };
        result.push_str(&format!("[{}]: {}\n\n", role_tag, msg.content));
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
    pub fn new(inner: AnyAgentInner, cache: ToolCache) -> Self {
        AnyAgent { inner, cache }
    }

    pub async fn run_print(&self, prompt: &str, max_turns: usize) -> anyhow::Result<String> {
        match &self.inner {
            AnyAgentInner::OpenRouter(a) => runner::run_print(a, prompt, max_turns).await,
            AnyAgentInner::OpenAI(a) => runner::run_print(a, prompt, max_turns).await,
            AnyAgentInner::Anthropic(a) => runner::run_print(a, prompt, max_turns).await,
            AnyAgentInner::Gemini(a) => runner::run_print(a, prompt, max_turns).await,
            AnyAgentInner::DeepSeek(a) => runner::run_print(a, prompt, max_turns).await,
            AnyAgentInner::Glm(a) => runner::run_print(a, prompt, max_turns).await,
            AnyAgentInner::Ollama(a) => runner::run_print(a, prompt, max_turns).await,
            AnyAgentInner::Custom(a) => runner::run_print(a, prompt, max_turns).await,
        }
    }

    pub fn spawn_runner(self, prompt: String, history: Vec<Message>) -> AgentRunner {
        self.cache.clear();
        match self.inner {
            AnyAgentInner::OpenRouter(a) => runner::spawn_agent(a, prompt, history, self.cache),
            AnyAgentInner::OpenAI(a) => runner::spawn_agent(a, prompt, history, self.cache),
            AnyAgentInner::Anthropic(a) => runner::spawn_agent(a, prompt, history, self.cache),
            AnyAgentInner::Gemini(a) => runner::spawn_agent(a, prompt, history, self.cache),
            AnyAgentInner::DeepSeek(a) => runner::spawn_agent(a, prompt, history, self.cache),
            AnyAgentInner::Glm(a) => runner::spawn_agent(a, prompt, history, self.cache),
            AnyAgentInner::Ollama(a) => runner::spawn_agent(a, prompt, history, self.cache),
            AnyAgentInner::Custom(a) => runner::spawn_agent(a, prompt, history, self.cache),
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

    macro_rules! build_inner {
        ($m:expr, $variant:ident) => {{
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
            AnyAgent::new(AnyAgentInner::$variant(agent), cache)
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
