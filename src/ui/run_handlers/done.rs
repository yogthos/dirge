//! `AgentEvent::Done` handler extracted from `run_interactive`.
//!
//! This is the largest handler — it closes a successful turn,
//! finalizes the streamed response, runs the plugin `on-response` /
//! `on-complete` / `prepare-next-run` chain (with optional model
//! swap), auto-compacts when the session crossed the threshold,
//! decides via `decide_post_done_action` whether to launch a
//! follow-up / loop iteration / stop, spawns a background review +
//! curator pass when idle, handles git-worktree return, and finally
//! drains any user interjections queued during the run.
//!
//! Behavior is identical to the original inline body; only the
//! lexical home moved.

use compact_str::CompactString;
use crossterm::style::Color;
use tokio::sync::mpsc;

use crate::agent::tools::background::BackgroundStore;
use crate::agent::tools::plan::PlanSwitchSender;
use crate::agent::tools::question::QuestionSender;
use crate::context::ContextFiles;
use crate::event::AgentEvent;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
#[cfg(feature = "plugin")]
use crate::plugin::PluginManager;
use crate::provider::{AnyAgent, AnyClient};
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;
use crate::session::MessageRole;
use crate::ui::agent_io::persist_turn_to_db;
use crate::ui::avatar;
use crate::ui::colors::{c_agent, c_error};
use crate::ui::events::render_session;
use crate::ui::run_handlers::RunCtx;
use crate::ui::slash::handle_compress;
use crate::ui::theme;
use crate::ui::tool_display::{chamber_bottom, chamber_widths};

/// Optional loop-feature state passed through to `handle_done`.
/// Behind `cfg(feature = "loop")` we hand the real mutable state;
/// without the feature, the placeholder type is `()` so the call
/// site doesn't need to thread a sentinel.
#[cfg(feature = "loop")]
pub(crate) struct LoopBits<'a> {
    pub state: &'a mut Option<crate::extras::r#loop::LoopState>,
    pub label: &'a mut Option<String>,
}

/// Git-worktree return path. Same conditional rationale as `LoopBits`.
#[cfg(feature = "git-worktree")]
pub(crate) struct WorktreeBits<'a> {
    pub return_path: &'a mut Option<String>,
}

// False positive for `await_holding_lock`: the plugin-manager guard
// IS held during dispatch calls (sync) but is explicitly `drop(mgr)`-ed
// at line 209 BEFORE the `build_agent(...).await` at line 236, and
// re-acquired in a new scope at line 263. Clippy can't trace the
// `drop()` so it flags the outer `let mut mgr` as held across the
// await even though it isn't.
// `unused_mut` allowed: `response`'s `mut` is consumed only by the
// plugin-gated `message-end` rewrite, so non-plugin builds see it
// as unused.
#[allow(clippy::too_many_arguments, clippy::await_holding_lock, unused_mut)]
pub(crate) async fn handle_done(
    ctx: &mut RunCtx<'_>,
    // dirge-lsoq: `mut` so the `message-end` plugin hook can rewrite
    // the finalized assistant text before it is stored/persisted.
    mut response: CompactString,
    tokens: u64,
    cost: f64,
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
    agent_cancel: &mut Option<mpsc::Sender<()>>,
    interjection_queue: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    #[cfg(feature = "lsp")] lsp_manager: Option<&std::sync::Arc<crate::lsp::manager::LspManager>>,
    #[cfg(feature = "plugin")] plugin_manager: Option<
        &std::sync::Arc<std::sync::Mutex<PluginManager>>,
    >,
    #[cfg(feature = "loop")] loop_bits: LoopBits<'_>,
    #[cfg(feature = "git-worktree")] worktree_bits: WorktreeBits<'_>,
) -> anyhow::Result<()> {
    *was_reasoning = false;
    // A successful turn must not leave a chamber
    // half-painted. If anything slipped through
    // — show_details=false skipping the body, an
    // in-flight Ask the user resolved with a path
    // that didn't reach the bottom paint, etc. —
    // close with a plain chamber bottom (not the
    // `⚠ tool denied · aborted` wording, which
    // would mislead the user about an otherwise-
    // successful run).
    if *ctx.tool_chamber_open {
        // Same drop-or-close logic as
        // close_tool_chamber_passive: if no
        // body content was added since the
        // TOP was painted (result never
        // arrived from the agent — MCP timeout,
        // network blip, agent loop bug), drop
        // the chamber entirely instead of
        // leaving an empty box on screen.
        // Otherwise close with a bottom border.
        let drop_chamber = match (*ctx.chamber_top_start, *ctx.chamber_top_end) {
            (Some(_), Some(end)) => ctx.renderer.buffer_len() == end,
            _ => false,
        };
        if drop_chamber {
            if let Some(start) = *ctx.chamber_top_start {
                ctx.renderer.replace_from(start, Vec::new());
            }
        } else {
            let (frame_w, _) = chamber_widths(ctx.renderer);
            ctx.renderer
                .write_line(&chamber_bottom(frame_w), theme::dim())?;
        }
        *ctx.tool_chamber_open = false;
        *ctx.chamber_top_start = None;
        *ctx.chamber_top_end = None;
    }
    *ctx.last_tool_name = None;
    ctx.renderer.set_avatar_state(avatar::AvatarState::Done);
    #[cfg(feature = "experimental-ui-terminal-tab")]
    ctx.renderer.set_last_tool_name("");

    #[allow(unused_mut, unused_variables)]
    let mut plugin_followup: Option<String> = None;
    #[cfg(feature = "plugin")]
    if let Some(pm) = plugin_manager {
        let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
        match mgr.dispatch(
            "on-response",
            &format!(
                "@{{:response \"{}\"}}",
                crate::plugin::escape_janet_string(&response)
            ),
        ) {
            Ok(results) if !results.is_empty() => {
                for line in &results {
                    // Sanitize plugin output (ANSI injection defense).
                    let safe = crate::ui::events::sanitize_output(line);
                    ctx.renderer
                        .write_line(&format!("[plugin] {}", safe), theme::dim())?;
                }
                plugin_followup = Some(results.join("\n"));
            }
            Ok(_) => {}
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("[plugin] on-response error: {e}"), c_error())?;
            }
        }
        // Check for pending prompts queued by on-response
        if let Some(pending) = mgr.take_pending_prompt() {
            plugin_followup = Some(pending);
        }
        // dirge-lsoq: fire `message-end` so a plugin can rewrite the
        // finalized assistant text via `harness/rewrite-message`. The
        // text already streamed to the screen; this rewrites what is
        // STORED + persisted (session DB, store_response), enabling
        // post-hoc redaction/annotation of stored history.
        match mgr.dispatch(
            "message-end",
            &format!(
                "@{{:message \"{}\"}}",
                crate::plugin::escape_janet_string(&response)
            ),
        ) {
            Ok(_) => {
                if let Some(rewritten) = mgr.take_message_rewrite() {
                    response = compact_str::CompactString::new(&rewritten);
                }
            }
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("[plugin] message-end error: {e}"), c_error())?;
            }
        }
        mgr.store_response(&response);
        // Fire on-complete after on-response so
        // plugins can react to "turn fully done."
        // Previously this hook was in HOOK_NAMES
        // (so plugins defining it got auto-aliased)
        // but no host site dispatched — silent fail.
        match mgr.dispatch("on-complete", "@{}") {
            Ok(_) => {}
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("[plugin] on-complete error: {e}"), c_error())?;
            }
        }
        // Fire `prepare-next-run` so plugins can
        // signal session-level state changes for
        // the next run. Closes the gap vs pi's
        // `prepareNextTurn` for the auto-apply
        // piece: when `harness-next-model` is
        // set, the agent is rebuilt with the new
        // model RIGHT HERE so the next user
        // prompt runs against it without
        // requiring `/model X`.
        //
        // Scope difference vs pi: pi fires
        // `prepareNextTurn` between TURNS within
        // a single agent run (and can swap model
        // mid-stream). dirge fires
        // `prepare-next-run` only between RUNS
        // (after Done). Mid-stream swap requires
        // breaking rig's multi-turn stream and
        // restarting with a new agent — that
        // would lose partial assistant state, so
        // we keep the swap at run boundaries.
        match mgr.dispatch("prepare-next-run", "@{}") {
            Ok(_) => {}
            Err(e) => {
                ctx.renderer
                    .write_line(&format!("[plugin] prepare-next-run error: {e}"), c_error())?;
            }
        }
        let pending_next_model = mgr.take_pending_next_model();
        // Release the plugin-manager guard before any `.await` below —
        // `std::sync::MutexGuard` is `!Send`, and the agent rebuild
        // path awaits a future. Re-acquire after the await for the
        // final `set harness-response nil`.
        drop(mgr);
        if let Some(next_model) = pending_next_model {
            // Validate: empty string is a
            // misconfiguration. Don't replace the
            // active model with nothing.
            let trimmed = next_model.trim();
            if !trimmed.is_empty() && trimmed != ctx.session.model.as_str() {
                let new_model_compact = CompactString::new(trimmed);
                let model_obj = client.completion_model(new_model_compact.to_string());
                *agent = crate::provider::build_agent(
                    model_obj,
                    ctx.cli,
                    ctx.cfg,
                    context,
                    permission.clone(),
                    ask_tx.clone(),
                    question_tx.clone(),
                    plan_tx.clone(),
                    bg_store.clone(),
                    #[cfg(feature = "lsp")]
                    lsp_manager.cloned(),
                    sandbox.clone(),
                    #[cfg(feature = "mcp")]
                    mcp_manager,
                    #[cfg(feature = "semantic")]
                    semantic_manager,
                    Some(ctx.session.id.to_string()),
                )
                .await;
                let old_model = ctx.session.model.clone();
                ctx.session.model = new_model_compact.clone();
                ctx.session.provider = ctx.cli.resolve_provider(ctx.cfg);
                // Re-resolve context window for
                // the new model — mirrors the
                // `/model` slash behavior so a
                // 128k→1M jump (or vice versa)
                // updates the status indicator.
                let new_ctx = ctx.cfg.resolve_context_window(new_model_compact.as_str());
                if new_ctx != ctx.session.context_window {
                    ctx.session.context_window = new_ctx;
                }
                ctx.renderer.write_line(
                    &format!(
                        "[plugin] swapped model: {} → {}",
                        old_model, new_model_compact,
                    ),
                    c_agent(),
                )?;
            }
        }
        // Clear `harness-response` so the next hook
        // doesn't see stale text from this turn. Re-acquire the
        // lock here since we released it above to satisfy
        // `clippy::await_holding_lock`.
        {
            let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
            let _ = mgr.eval("(set harness-response nil)");
        }
    }

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
    } else if !*ctx.agent_line_started {
        ctx.renderer.write("<dirge> ", c_agent())?;
    }

    ctx.renderer.write_line("", Color::White)?;
    ctx.renderer.write_line("", Color::White)?;
    // Phase 3: persist structured tool calls
    // alongside the assistant text so the next
    // resume sees the full tool_use/tool_result
    // pairs in convert_history.
    ctx.session.add_message_with_tool_calls(
        MessageRole::Assistant,
        &response,
        std::mem::take(ctx.tool_calls_buf),
    );
    // TODO(cost-tracking): `tokens` here is the heuristic
    // estimate (text.len()/4) and `cost` is always 0.0 —
    // these accumulate into placeholder fields and won't
    // reflect actual provider usage / billing until we
    // pipe rig's `FinalResponse.usage()` through into
    // `AgentEvent::Done`. Kept as no-op-ish additions so
    // the wiring is in place when real values arrive.
    ctx.session.total_tokens = ctx.session.total_tokens.saturating_add(tokens);
    ctx.session.total_cost += cost;
    // Run ended cleanly — reset the per-run tool-
    // call counter so the next user submission
    // starts at zero. Mirrored in the Interjected
    // branch + both abort paths below.
    *ctx.tool_calls_this_run = 0;
    *ctx.agent_line_started = false;
    ctx.response_buf.clear();
    *ctx.response_start_line = None;
    ctx.reasoning_buf.clear();
    *ctx.reasoning_start_line = None;

    #[cfg(feature = "loop")]
    let loop_running = loop_bits.state.as_ref().is_some_and(|ls| ls.active);
    #[cfg(not(feature = "loop"))]
    let loop_running = false;

    if !loop_running
        && ctx.cfg.resolve_compact_enabled()
        && ctx
            .session
            .needs_compaction(ctx.cfg.resolve_reserve_tokens())
        && !ctx.cli.no_session
    {
        // Auto-compact failure used to render as a
        // single dim red line that scrolled past
        // unnoticed — users kept typing into an
        // over-full context and saw mysterious
        // context-length errors next turn. Frame
        // the warning so it visibly stops the eye
        // and tells the user what to do next.
        ctx.renderer
            .write_line("▒░ auto-compacting context ░▒", theme::accent())?;
        let compress_result = handle_compress(
            None,
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
        if let Err(e) = compress_result {
            ctx.renderer.write_line(
                "╭─ ⚠ AUTO-COMPACT FAILED ─────────────────────────────╮",
                c_error(),
            )?;
            // Cap the cause length so a sprawling
            // multi-line error doesn't blow out the
            // box's visual rhythm. The full error
            // is still in the agent's recovery
            // path; this is for the user-facing
            // hint only.
            let cause = {
                let s = e.to_string().replace('\n', " ");
                if s.chars().count() > 64 {
                    let mut out: String = s.chars().take(63).collect();
                    out.push('…');
                    out
                } else {
                    s
                }
            };
            ctx.renderer
                .write_line(&format!("│ cause: {}", cause), c_error())?;
            ctx.renderer.write_line(
                "│ context is over the threshold — replies may start",
                c_error(),
            )?;
            ctx.renderer
                .write_line("│ hitting context-length errors. Try /compress", c_error())?;
            ctx.renderer.write_line(
                "│ manually, /clear to start fresh, or restart with",
                c_error(),
            )?;
            ctx.renderer
                .write_line("│ a larger context_window in config.", c_error())?;
            ctx.renderer.write_line(
                "╰─────────────────────────────────────────────────────╯",
                c_error(),
            )?;
        }
    }

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

    #[cfg(feature = "plugin")]
    let followup_for_decision = plugin_followup.clone();
    #[cfg(not(feature = "plugin"))]
    let followup_for_decision: Option<String> = None;

    #[cfg(feature = "loop")]
    let (loop_active, loop_should_stop) = loop_bits
        .state
        .as_ref()
        .map(|ls| (ls.active, ls.active && ls.should_stop()))
        .unwrap_or((false, false));
    #[cfg(not(feature = "loop"))]
    let (loop_active, loop_should_stop) = (false, false);

    let action = crate::plugin::decide_post_done_action(
        followup_for_decision,
        loop_active,
        loop_should_stop,
    );

    match action {
        crate::plugin::PostDoneAction::Followup(text) => {
            let followup_prompt = text + "\n\nContinue.";
            ctx.last_user_prompt.clone_from(&followup_prompt);
            let runner = agent.clone().spawn_runner(
                crate::agent::tools::background::prepend_pending_notifications(
                    &followup_prompt,
                    bg_store.as_ref(),
                ),
                crate::agent::runner::convert_history(ctx.session),
                Some(interjection_queue.clone()),
            );
            *agent_rx = Some(runner.event_rx);
            *agent_abort = Some(runner.task);
            *agent_interject = Some(runner.interject_tx);
            *agent_cancel = Some(runner.cancel_tx);
            *is_running = true;
        }
        crate::plugin::PostDoneAction::LoopStop =>
        {
            #[cfg(feature = "loop")]
            if let Some(ls) = loop_bits.state.as_mut() {
                ctx.renderer.write_line(
                    &format!("[loop] max iterations ({}) reached, stopping", ls.iteration),
                    c_agent(),
                )?;
                ls.active = false;
                *loop_bits.label = None;
            }
        }
        crate::plugin::PostDoneAction::LoopIter =>
        {
            #[cfg(feature = "loop")]
            if let Some(ls) = loop_bits.state.as_mut() {
                let summary: String = response.chars().take(200).collect();
                ls.last_summary = Some(summary);
                ls.iteration += 1;
                let prompt = ls.build_prompt();
                ctx.last_user_prompt.clone_from(&prompt);
                let runner = agent.clone().spawn_runner(
                    crate::agent::tools::background::prepend_pending_notifications(
                        &prompt,
                        bg_store.as_ref(),
                    ),
                    Vec::new(),
                    Some(interjection_queue.clone()),
                );
                *agent_rx = Some(runner.event_rx);
                *agent_abort = Some(runner.task);
                *agent_interject = Some(runner.interject_tx);
                *agent_cancel = Some(runner.cancel_tx);
                *is_running = true;
                *loop_bits.label = Some(ls.iteration_label());
                ctx.renderer.write_line(
                    &format!("[loop] launching {}", ls.iteration_label()),
                    c_agent(),
                )?;
            }
        }
        crate::plugin::PostDoneAction::Idle => {}
    }

    // Phase 4: spawn background review when the
    // session is truly idle (no plugin followup,
    // loop iteration, or worktree cleanup claimed
    // the next turn). Fire-and-forget — the review
    // runs in a tokio task and never blocks the user.
    if !*is_running {
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let paths = crate::extras::dirge_paths::ProjectPaths::new(&cwd);

        // Persist the completed turn to the SQLite
        // session DB for future search. Uses a
        // stable session id so messages from the
        // same interactive session are grouped.
        // Includes tool names + results for FTS5.
        persist_turn_to_db(
            ctx.session,
            ctx.last_user_prompt,
            &response,
            ctx.tool_calls_buf,
        );

        let transcript = crate::agent::review::build_transcript(ctx.session);

        // dirge-ba0m: unified post-session learning orchestrator.
        // Replaces the three independent fire-and-forget spawns
        // (background review + skills curator + memory curator)
        // that used to race here. The orchestrator runs them
        // strictly in order inside ONE detached task so a skill
        // the review creates is flushed before the curator reads
        // it, and the three LLM runners never fire concurrently.
        // Still fire-and-forget — the user's turn never waits.
        crate::agent::post_session::spawn_post_session(
            agent.clone(),
            paths,
            transcript,
            ctx.session.id.to_string(),
        );
    }

    #[cfg(feature = "git-worktree")]
    if let Some(main_path) = worktree_bits.return_path.take() {
        match std::env::set_current_dir(&main_path) {
            Ok(()) => {
                ctx.session.working_dir = compact_str::CompactString::new(&main_path);
                // Re-anchor the permission checker to the main repo after
                // merging back from the worktree, else the CWD write-allow
                // stays pointed at the removed worktree and main-repo writes
                // prompt. Same contract as /cd and worktree create/exit.
                if let Some(perm) = permission
                    && let Ok(mut guard) = perm.lock()
                {
                    guard.set_working_dir(&ctx.session.working_dir);
                }
                context.reload();
                let model = client.completion_model(ctx.session.model.to_string());
                *agent = crate::provider::build_agent(
                    model,
                    ctx.cli,
                    ctx.cfg,
                    context,
                    permission.clone(),
                    ask_tx.clone(),
                    question_tx.clone(),
                    plan_tx.clone(),
                    bg_store.clone(),
                    #[cfg(feature = "lsp")]
                    lsp_manager.cloned(),
                    sandbox.clone(),
                    #[cfg(feature = "mcp")]
                    mcp_manager,
                    #[cfg(feature = "semantic")]
                    semantic_manager,
                    Some(ctx.session.id.to_string()),
                )
                .await;
                render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, context)?;
                ctx.renderer.write_line(
                    &format!("merged and returned to main repo at {}", main_path),
                    c_agent(),
                )?;
            }
            Err(e) => {
                ctx.renderer.write_line(
                    &format!("warning: failed to change back to main repo: {}", e),
                    c_error(),
                )?;
            }
        }
    }

    // Drain the interjection queue once the run is fully
    // idle (no plugin follow-up, loop iteration, or worktree
    // cleanup grabbed the next turn). Concatenate all
    // queued messages as a single new user turn and kick
    // off another run against the now-stable agent/cwd.
    if !*is_running && !interjection_queue.lock().unwrap().is_empty() {
        let queued: Vec<String> = interjection_queue.lock().unwrap().drain(..).collect();
        let combined = queued.join("\n\n");
        // No write_user_lines here — the loop's
        // MessageStart{User} → AgentEvent::UserMessage
        // bridge will render the user's text once,
        // post-stripping the system-reminder block.
        // Calling write_user_lines here would
        // duplicate the render (see commit 7584bdf
        // for the original regular-input fix).

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
