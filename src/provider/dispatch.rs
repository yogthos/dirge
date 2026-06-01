//! Concrete `rig` client/model dispatch enums.
//!
//! Split out of `provider/mod.rs` (dirge-4y4l): [`AnyClient`] and
//! [`AnyModel`] erase the per-provider `rig` client/model types behind
//! a single enum so the rest of the codebase dispatches uniformly. The
//! agent-building wiring that constructs these lives in the parent
//! module; here we only hold the enums plus the operations that fan out
//! over their variants (model construction, one-shot prompts, stream-fn
//! building, conversation compaction).

use rig::client::CompletionClient;
use rig::completion::Prompt;
use rig::providers::{anthropic, gemini, ollama, openai, openrouter};

use crate::agent::prompt;
use crate::session::SessionMessage;

use super::summarize;

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
