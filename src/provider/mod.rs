mod build;
pub mod client;
mod dispatch;
mod resolve;
mod run;
mod spawn;
pub mod summarize;

pub use build::*;
pub use dispatch::*;
pub use resolve::*;

use rig::agent::Agent;
use rig::providers::{anthropic, gemini, ollama, openai, openrouter};

use crate::agent::tools::ToolCache;

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

    /// The LoopTool registry built at `build_agent` time. Read by the
    /// escalation/review stream-fn builders in `provider::build` (a
    /// sibling module) to mirror the default loop's tool set.
    pub(crate) fn loop_tools(&self) -> &[std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>] {
        &self.loop_tools
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
