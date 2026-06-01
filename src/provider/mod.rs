mod build;
pub mod client;
mod dispatch;
mod resolve;
pub mod summarize;

pub use build::*;
pub use dispatch::*;
pub use resolve::*;

use rig::agent::Agent;
use rig::completion::Message;
use rig::providers::{anthropic, gemini, ollama, openai, openrouter};

use crate::agent::runner::{self, AgentRunner};
use crate::agent::tools::ToolCache;
use crate::event::AgentEvent;

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

    /// The LoopTool registry built at `build_agent` time. Read by the
    /// escalation/review stream-fn builders in `provider::build` (a
    /// sibling module) to mirror the default loop's tool set.
    pub(crate) fn loop_tools(&self) -> &[std::sync::Arc<dyn crate::agent::agent_loop::LoopTool>] {
        &self.loop_tools
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

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
