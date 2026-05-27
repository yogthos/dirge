//! `AgentEvent::ContextOverflow` handler extracted from `run_interactive`.
//!
//! Auto-recovery for a context-length error mid-run: persist what
//! streamed so far, run `/compress`, then (only when the compaction
//! actually shrank the session AND no side-effecting tools fired)
//! respawn the same prompt against the compacted history. Tool-side-
//! effect-safety and no-op compactions surface as error rows and
//! leave `is_running` false.

use compact_str::CompactString;
use tokio::sync::mpsc;

use crate::agent::tools::background::BackgroundStore;
use crate::agent::tools::plan::PlanSwitchSender;
use crate::agent::tools::question::QuestionSender;
use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
use crate::event::AgentEvent;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::{AnyAgent, AnyClient};
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;
use crate::ui::agent_io::persist_turn_to_db;
use crate::ui::colors::c_error;
use crate::ui::events::sanitize_output;
use crate::ui::renderer::Renderer;
use crate::ui::run_handlers::RunCtx;
use crate::ui::slash::{CompressOutcome, handle_compress};
use crate::ui::theme;
use crate::ui::tool_display::close_tool_chamber_if_open;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_context_overflow(
    ctx: &mut RunCtx<'_>,
    prompt: CompactString,
    error: CompactString,
    was_reasoning: &mut bool,
    is_running: &mut bool,
    agent: &mut AnyAgent,
    client: &AnyClient,
    context: &mut ContextFiles,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    question_tx: &Option<QuestionSender>,
    plan_tx: &Option<PlanSwitchSender>,
    bg_store: &Option<BackgroundStore>,
    sandbox: &Sandbox,
    agent_rx: &mut Option<mpsc::Receiver<AgentEvent>>,
    agent_abort: &mut Option<tokio::task::JoinHandle<()>>,
    agent_interject: &mut Option<mpsc::Sender<()>>,
    interjection_queue: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    #[cfg(feature = "lsp")] lsp_manager: Option<&std::sync::Arc<crate::lsp::manager::LspManager>>,
) -> anyhow::Result<()> {
    // Audit H17: the streaming run hit a context-
    // length error. Auto-compact then re-spawn with
    // the same prompt against the now-compacted
    // history — opencode-style automatic recovery
    // (compaction.ts:477-558) instead of leaving the
    // user stranded at the error.
    *was_reasoning = false;
    close_tool_chamber_if_open(ctx.renderer, ctx.last_tool_name, ctx.tool_chamber_open)?;
    let safe = sanitize_output(&error);
    ctx.renderer
        .write_line(&format!("context overflow: {}", safe), c_error())?;
    // Persist what we have so far (partial response
    // + tool calls) before tearing down the runner.
    persist_turn_to_db(
        ctx.session,
        ctx.last_user_prompt,
        ctx.response_buf,
        ctx.tool_calls_buf,
    );
    // Tear down the current runner before respawn.
    if let Some(h) = agent_abort.take() {
        h.abort();
    }
    *agent_rx = None;
    *agent_interject = None;
    *ctx.agent_line_started = false;
    ctx.response_buf.clear();
    *ctx.response_start_line = None;
    ctx.reasoning_buf.clear();
    *ctx.reasoning_start_line = None;

    ctx.renderer
        .write_line("▒░ auto-compacting then retrying ░▒", theme::accent())?;
    let compress_result = compress(
        agent,
        client,
        ctx.renderer,
        ctx.session,
        ctx.cli,
        ctx.cfg,
        context,
        permission,
        ask_tx,
        question_tx,
        plan_tx,
        bg_store,
        sandbox,
        #[cfg(feature = "mcp")]
        mcp_manager,
        #[cfg(feature = "semantic")]
        semantic_manager,
        #[cfg(feature = "lsp")]
        lsp_manager,
    )
    .await;

    // Review #1: compress can return Ok WITHOUT
    // shrinking the session (three no-op paths
    // inside `handle_compress`). Respawning
    // against the unchanged history just re-emits
    // ContextOverflow and infinite-loops the
    // auto-recovery. Only respawn on `Compacted`.
    //
    // Review #2: re-issuing the same prompt
    // re-runs any side-effecting tool calls the
    // failed turn already made. The interactive
    // retry loop already refuses to retry when
    // `had_tool_calls`; the auto path used to
    // bypass that safety. We have no direct
    // `had_tool_calls` signal here (the runner
    // emitted ContextOverflow without telling us
    // whether tools fired). Approximate it by
    // comparing `tool_calls_this_run > 0`, which
    // tracks every ToolCall event observed during
    // the failed turn. If any tool ran, surface
    // the error and let the user decide.
    let tools_already_ran = *ctx.tool_calls_this_run > 0;
    // Reset the abort-trailer counter regardless
    // — the failed run is over.
    *ctx.tool_calls_this_run = 0;
    match compress_result {
        Ok(CompressOutcome::Compacted) if !tools_already_ran => {
            // Build history from the compacted session.
            // Drop the trailing User message because
            // it's the prompt we're about to resubmit
            // — otherwise rig would receive it twice.
            let mut history = crate::agent::runner::convert_history(ctx.session);
            if let Some(last) = history.last()
                && matches!(last, rig::completion::Message::User { .. })
            {
                history.pop();
            }
            let prompt_owned = prompt.to_string();
            ctx.last_user_prompt.clone_from(&prompt_owned);
            let prepared_prompt = crate::agent::tools::background::prepend_pending_notifications(
                &prompt_owned,
                bg_store.as_ref(),
            );
            let runner = agent.clone().spawn_runner(
                prepared_prompt,
                history,
                Some(interjection_queue.clone()),
            );
            *agent_rx = Some(runner.event_rx);
            *agent_abort = Some(runner.task);
            *agent_interject = Some(runner.interject_tx);
            *is_running = true;
            // Review #4: collapsed result from the
            // failed run is stale — the user will
            // care about results from the new
            // attempt, not what got truncated
            // before the overflow.
            *ctx.last_collapsed = None;
            ctx.renderer
                .write_line("  ↳ resumed run with compacted history", theme::dim())?;
        }
        Ok(CompressOutcome::Compacted) => {
            // Compacted, but tool side-effects
            // already applied — refusing auto-
            // retry. User can re-issue manually.
            ctx.renderer.write_line(
                "  ↳ context compacted, but the failed run already invoked tools — not auto-retrying. Re-issue your prompt manually if you want to continue.",
                c_error(),
            )?;
            *is_running = false;
            let dropped = interjection_queue.lock().unwrap().len();
            interjection_queue.lock().unwrap().clear();
            if dropped > 0 {
                ctx.renderer.write_line(
                    &format!(
                        "{} queued message{} dropped due to tool-side-effect safety",
                        dropped,
                        if dropped == 1 { "" } else { "s" }
                    ),
                    c_error(),
                )?;
            }
        }
        Ok(CompressOutcome::NoOp { reason }) => {
            ctx.renderer.write_line(
                &format!(
                    "auto-compact made no progress ({reason}); leaving session as-is. Try /compress with stricter instructions, lower keep_recent_tokens, or /clear."
                ),
                c_error(),
            )?;
            *is_running = false;
            let dropped = interjection_queue.lock().unwrap().len();
            interjection_queue.lock().unwrap().clear();
            if dropped > 0 {
                ctx.renderer.write_line(
                    &format!(
                        "{} queued message{} dropped due to compact no-op",
                        dropped,
                        if dropped == 1 { "" } else { "s" }
                    ),
                    c_error(),
                )?;
            }
        }
        Err(ce) => {
            ctx.renderer.write_line(
                &format!(
                    "auto-compact failed ({}); leaving session as-is. Try /compress manually or /clear.",
                    ce
                ),
                c_error(),
            )?;
            *is_running = false;
            let dropped = interjection_queue.lock().unwrap().len();
            interjection_queue.lock().unwrap().clear();
            if dropped > 0 {
                ctx.renderer.write_line(
                    &format!(
                        "{} queued message{} dropped due to compact failure",
                        dropped,
                        if dropped == 1 { "" } else { "s" }
                    ),
                    c_error(),
                )?;
            }
        }
    }
    Ok(())
}

/// Thin wrapper around `handle_compress` so the call site above stays
/// readable; preserves the exact feature-gated parameter list.
#[allow(clippy::too_many_arguments)]
async fn compress(
    agent: &mut AnyAgent,
    client: &AnyClient,
    renderer: &mut Renderer,
    session: &mut crate::session::Session,
    cli: &Cli,
    cfg: &Config,
    context: &mut ContextFiles,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    question_tx: &Option<QuestionSender>,
    plan_tx: &Option<PlanSwitchSender>,
    bg_store: &Option<BackgroundStore>,
    sandbox: &Sandbox,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    #[cfg(feature = "lsp")] lsp_manager: Option<&std::sync::Arc<crate::lsp::manager::LspManager>>,
) -> anyhow::Result<CompressOutcome> {
    handle_compress(
        None,
        agent,
        client,
        renderer,
        session,
        cli,
        cfg,
        context,
        permission,
        ask_tx,
        question_tx,
        plan_tx,
        bg_store,
        sandbox,
        #[cfg(feature = "mcp")]
        mcp_manager,
        #[cfg(feature = "semantic")]
        semantic_manager,
        #[cfg(feature = "lsp")]
        lsp_manager,
    )
    .await
}
