//! Agent construction and auxiliary-route wiring.
//!
//! Split out of `provider/mod.rs` (dirge-4y4l): the dependency-injection
//! seam that turns a resolved [`AnyModel`] + config into a fully wired
//! [`AnyAgent`], plus the standalone stream-fn / callback builders for
//! the escalation, critic, approval, and background-review routes. The
//! `AnyAgent` type and its methods live in the parent module; this file
//! only orchestrates the builders.

use std::collections::HashMap;

use crate::agent::builder;
use crate::cli::Cli;
use crate::config::{Config, ProviderEntry};
use crate::context::ContextFiles;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;

use crate::agent::tools::plan::PlanSwitchSender;
use crate::agent::tools::question::QuestionSender;

use super::{
    AnyAgent, AnyAgentInner, AnyClient, AnyModel, client, default_model_for_entry, summarize,
};

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
                        agent.loop_tools(),
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
                        agent.loop_tools(),
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
        .unwrap_or_else(|| default_model_for_entry(alias, entry).to_string());
    let model = client.completion_model(model_name);
    let tool_defs: Vec<rig::completion::ToolDefinition> = loop_tools
        .iter()
        .map(|t| loop_tool_to_rig_definition(t.as_ref()))
        .collect();
    Ok(model.build_stream_fn(tool_defs, chunk_timeout, Some(alias.to_string())))
}

/// F6 tier 3: build the bounded-critic callback. Constructs a fresh
/// client for the critic alias and returns a [`CriticFn`] that runs one
/// completion (via `summarize::oneshot_with_model`, with the critic's own
/// role preamble + telemetry label) per call. No tools — the critic only
/// reads a transcript and returns a verdict.
fn build_critic_fn(
    alias: &str,
    entry: &ProviderEntry,
    providers: &HashMap<String, ProviderEntry>,
) -> anyhow::Result<crate::agent::agent_loop::critic::CriticFn> {
    let client = std::sync::Arc::new(create_client(alias, None, providers)?);
    let model_name = entry
        .model
        .clone()
        .unwrap_or_else(|| default_model_for_entry(alias, entry).to_string());
    Ok(std::sync::Arc::new(move |prompt: String| {
        let client = client.clone();
        let model_name = model_name.clone();
        Box::pin(async move {
            let model = client.completion_model(model_name);
            // Distinct retry/telemetry label + a role-appropriate system
            // preamble (the critic's response FORMAT still rides in the
            // prompt body, next to the transcript).
            summarize::oneshot_with_model(
                model,
                "critic",
                crate::agent::agent_loop::critic::CRITIC_PREAMBLE,
                prompt,
            )
            .await
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
        .unwrap_or_else(|| default_model_for_entry(alias, entry).to_string());
    Ok(std::sync::Arc::new(move |req: ApprovalRequest| {
        let client = client.clone();
        let model_name = model_name.clone();
        Box::pin(async move {
            let model = client.completion_model(model_name);
            let prompt = build_evaluator_prompt(&req);
            let raw = summarize::oneshot_with_model(model, "approval", EVALUATOR_PREAMBLE, prompt)
                .await?;
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
        .unwrap_or_else(|| default_model_for_entry(alias, entry).to_string());
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
