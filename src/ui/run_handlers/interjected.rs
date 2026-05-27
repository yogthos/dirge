//! `AgentEvent::Interjected` handler extracted from `run_interactive`.
//!
//! Fires when a queued user message hit the runner's per-tool-result
//! interjection probe. Finalizes the partial assistant text, records
//! it (with the pending tool-call entries left in their Interrupted
//! state) on the session, tears the runner down, and immediately
//! drains the interjection queue to launch the next run.

use compact_str::CompactString;
use crossterm::style::Color;
use tokio::sync::mpsc;

use crate::agent::tools::background::BackgroundStore;
use crate::event::AgentEvent;
use crate::provider::AnyAgent;
use crate::session::MessageRole;
use crate::ui::agent_io::persist_turn_to_db;
use crate::ui::colors::{c_agent, c_error};
use crate::ui::run_handlers::RunCtx;
use crate::ui::theme;
use crate::ui::tool_display::close_tool_chamber_if_open;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_interjected(
    ctx: &mut RunCtx<'_>,
    partial_response: CompactString,
    tokens: u64,
    was_reasoning: &mut bool,
    is_running: &mut bool,
    agent: &AnyAgent,
    agent_rx: &mut Option<mpsc::Receiver<AgentEvent>>,
    agent_abort: &mut Option<tokio::task::JoinHandle<()>>,
    agent_interject: &mut Option<mpsc::Sender<()>>,
    agent_cancel: &mut Option<mpsc::Sender<()>>,
    interjection_queue: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    bg_store: &Option<BackgroundStore>,
) -> anyhow::Result<()> {
    *was_reasoning = false;
    close_tool_chamber_if_open(ctx.renderer, ctx.last_tool_name, ctx.tool_chamber_open)?;

    // Finalize whatever assistant text streamed so far so
    // the conversation history reflects what the user saw,
    // not a phantom turn that "never happened".
    if !ctx.response_buf.is_empty() {
        let max_width = ctx.renderer.content_width().saturating_sub(9); // 8-col handle + space
        let mut styled =
            crate::ui::markdown::markdown_to_styled(ctx.response_buf, max_width, c_agent());
        if !styled.is_empty() {
            styled[0].text = CompactString::from(format!("<dirge> {}", styled[0].text));
        }
        if let Some(start) = *ctx.response_start_line {
            ctx.renderer.replace_from(start, styled);
            ctx.renderer.render_viewport()?;
        }
    }
    ctx.renderer.write_line("", Color::White)?;
    ctx.renderer.write_line(
        "(interjected — stopped at last tool-result boundary)",
        theme::dim(),
    )?;
    ctx.renderer.write_line("", Color::White)?;

    // Record the (partial) assistant response in session
    // history. Even truncated, it lets the LLM see what
    // it had said when the user spoke up.
    if !partial_response.is_empty() {
        // Persist the partial turn to session DB
        // before tool_calls_buf is consumed.
        persist_turn_to_db(
            ctx.session,
            ctx.last_user_prompt,
            &partial_response,
            ctx.tool_calls_buf,
        );

        // Phase 3: same structured persistence
        // as the Done branch. Any pending entries
        // (tool calls without a result yet) keep
        // their Interrupted state — the LLM
        // sees [Tool execution was interrupted]
        // tool_result on resume.
        ctx.session.add_message_with_tool_calls(
            MessageRole::Assistant,
            &partial_response,
            std::mem::take(ctx.tool_calls_buf),
        );
        // TODO(cost-tracking): same caveat as the Done
        // branch — `tokens` is an estimate, not actual
        // provider usage. Wire after rig usage plumbing.
        ctx.session.total_tokens = ctx.session.total_tokens.saturating_add(tokens);
    } else {
        // No partial text but maybe pending tool
        // calls — drop them; the session already
        // captured them via prior turns or they
        // were a single-call abort with no text.
        ctx.tool_calls_buf.clear();
    }
    // Run ended (interjection-style) — reset the
    // per-run tool-call counter alongside the
    // other per-run state.
    *ctx.tool_calls_this_run = 0;
    *ctx.agent_line_started = false;
    ctx.response_buf.clear();
    *ctx.response_start_line = None;
    ctx.reasoning_buf.clear();
    *ctx.reasoning_start_line = None;

    if !ctx.cli.no_session
        && let Err(e) = crate::session::storage::save_session(ctx.session)
    {
        ctx.renderer.write_line(
            &format!("warning: failed to save session: {}", e),
            c_error(),
        )?;
    }
    *is_running = false;
    if let Some(h) = agent_abort.take() {
        h.abort();
    }
    *agent_rx = None;
    *agent_interject = None;
    *agent_cancel = None;

    // Drain the queue immediately — it's guaranteed to be
    // non-empty here since the runner only emits this
    // event when the UI signaled an interjection, and the
    // signal is only sent from the queue-push code path.
    if !interjection_queue.lock().unwrap().is_empty() {
        let queued: Vec<String> = interjection_queue.lock().unwrap().drain(..).collect();
        let combined = queued.join("\n\n");
        // No write_user_lines — same reasoning as
        // the idle-drain path above; the loop's
        // UserMessage bridge handles the render.

        ctx.last_user_prompt.clone_from(&combined);
        let history = crate::agent::runner::convert_history(ctx.session);
        ctx.session.add_message(MessageRole::User, &combined);

        let runner = agent.clone().spawn_runner(
            crate::agent::tools::background::prepend_pending_notifications(
                &combined,
                bg_store.as_ref(),
            ),
            history,
            Some(interjection_queue.clone()),
        );
        *agent_rx = Some(runner.event_rx);
        *agent_abort = Some(runner.task);
        *agent_interject = Some(runner.interject_tx);
        *agent_cancel = Some(runner.cancel_tx);
        *is_running = true;
    }
    Ok(())
}
