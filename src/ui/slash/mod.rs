use crossterm::style::Color;
use smallvec::SmallVec;

use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::{AnyAgent, AnyClient};
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;
use crate::session::{MessageRole, Session};
use crate::ui::events::render_session;
use crate::ui::input::InputEditor;
use crate::ui::renderer::Renderer;
use crate::ui::theme;

mod cmd_misc;
mod cmd_model;
mod cmd_session;
#[cfg(feature = "git-worktree")]
mod cmd_worktree;

#[inline]
pub(super) fn c_agent() -> Color {
    theme::agent()
}
#[inline]
pub(super) fn c_result() -> Color {
    theme::result()
}
#[inline]
pub(super) fn c_error() -> Color {
    theme::error()
}

/// Bundle of mutable references that slash-command handlers need.
/// Keeps individual handler signatures tractable.
pub(super) struct SlashCtx<'a> {
    pub agent: &'a mut AnyAgent,
    pub client: &'a AnyClient,
    pub renderer: &'a mut Renderer,
    pub session: &'a mut Session,
    pub cli: &'a Cli,
    pub cfg: &'a Config,
    pub context: &'a mut ContextFiles,
    pub show_reasoning: &'a mut bool,
    pub is_running: &'a mut bool,
    pub input: &'a mut InputEditor,
    pub permission: &'a Option<PermCheck>,
    pub ask_tx: &'a Option<AskSender>,
    pub question_tx: &'a Option<crate::agent::tools::question::QuestionSender>,
    pub plan_tx: &'a Option<crate::agent::tools::plan::PlanSwitchSender>,
    pub todo_tools_enabled: &'a mut bool,
    pub bg_store: &'a Option<crate::agent::tools::background::BackgroundStore>,
    pub sandbox: &'a Sandbox,
    #[cfg(feature = "loop")]
    pub loop_state: &'a mut Option<crate::extras::r#loop::LoopState>,
    #[cfg(feature = "mcp")]
    pub mcp_manager: Option<&'a McpClientManager>,
    #[cfg(feature = "semantic")]
    pub semantic_manager: Option<&'a SemanticManager>,
    #[cfg(feature = "lsp")]
    pub lsp_manager: Option<&'a std::sync::Arc<crate::lsp::manager::LspManager>>,
}

/// Walk `cut_idx` forward until the message at that index is a
/// `User` message (or the index reaches `messages.len()`). This
/// guarantees the kept tail after compress starts with a User
/// message, which is what every provider expects after a System
/// summary. If `cut_idx` already points at a User message or is
/// past the end, no change. If no user message exists in the
/// tail, return `messages.len()` — caller surfaces the "nothing to
/// compress" message.
///
/// Matches opencode's `splitTurn` discipline
/// (`session/compaction.ts:161-184`).
fn align_cut_to_user_boundary(
    messages: &[crate::session::SessionMessage],
    cut_idx: usize,
) -> usize {
    let mut i = cut_idx;
    while i < messages.len() && messages[i].role != MessageRole::User {
        i += 1;
    }
    i
}

/// Outcome of `undo_last`. `removed` is the number of messages popped;
/// `had_tool_calls` is set when at least one of the popped messages
/// had tool calls attached — the caller should surface a warning
/// because tool side effects (file writes, bash, MCP calls) are NOT
/// reverted by undo.
#[derive(Debug, Default)]
pub struct UndoOutcome {
    pub removed: usize,
    pub had_tool_calls: bool,
}

pub fn undo_last(session: &mut Session) -> UndoOutcome {
    let len = session.messages.len();
    if len == 0 {
        return UndoOutcome::default();
    }
    let mut outcome = UndoOutcome::default();
    let pop = |session: &mut Session, outcome: &mut UndoOutcome| {
        if let Some(last) = session.messages.last()
            && !last.tool_calls.is_empty()
        {
            outcome.had_tool_calls = true;
        }
        session.pop_last_message();
        outcome.removed += 1;
    };
    // Route through `pop_last_message` so the tree + message_store
    // stay in sync — P4c made direct .messages.pop() incorrect for
    // branched sessions.
    if session.messages[len - 1].role == MessageRole::Assistant {
        pop(session, &mut outcome);
        if session
            .messages
            .last()
            .is_some_and(|m| m.role == MessageRole::User)
        {
            pop(session, &mut outcome);
        }
        return outcome;
    }
    if session.messages[len - 1].role == MessageRole::User {
        pop(session, &mut outcome);
    }
    outcome
}

/// Result of an attempted compression. `Compacted` means messages
/// were actually replaced; `NoOp` covers every path that returned
/// without shrinking the session (already-within-limits, nothing to
/// cut, summary too large). Callers driving auto-recovery (the
/// `ContextOverflow` handler) MUST distinguish these — respawning
/// the run against an unchanged history just re-emits the same
/// ContextLength error and loops.
pub enum CompressOutcome {
    Compacted,
    NoOp { reason: &'static str },
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_compress(
    instructions: Option<&str>,
    agent: &mut AnyAgent,
    client: &AnyClient,
    renderer: &mut Renderer,
    session: &mut Session,
    cli: &Cli,
    cfg: &Config,
    context: &mut ContextFiles,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    // Audit followup (companion to C8 LSP fix): question_tx +
    // plan_tx were previously passed as None when this function
    // rebuilt the agent post-compact. Tools that depend on either
    // (the `question` tool, plan-mode switch hooks) silently
    // broke after every auto-compact + manual /compress. Thread
    // the channels through.
    question_tx: &Option<crate::agent::tools::question::QuestionSender>,
    plan_tx: &Option<crate::agent::tools::plan::PlanSwitchSender>,
    bg_store: &Option<crate::agent::tools::background::BackgroundStore>,
    sandbox: &Sandbox,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    #[cfg(feature = "lsp")] lsp_manager: Option<&std::sync::Arc<crate::lsp::manager::LspManager>>,
) -> anyhow::Result<CompressOutcome> {
    renderer.write_line("compressing...", c_agent())?;
    renderer.write_line("", Color::White)?;

    let reserve = cfg.resolve_reserve_tokens();
    let keep_recent = cfg.resolve_keep_recent_tokens();
    let max_tokens = session.context_window.saturating_sub(reserve);

    if session.total_estimated_tokens <= max_tokens {
        renderer.write_line("context within limits, no compression needed", c_agent())?;
        return Ok(CompressOutcome::NoOp {
            reason: "context within limits",
        });
    }

    let mut accumulated = 0u64;
    let mut cut_idx = session.messages.len();
    for (i, msg) in session.messages.iter().enumerate().rev() {
        if accumulated >= keep_recent {
            cut_idx = i + 1;
            break;
        }
        accumulated = accumulated.saturating_add(msg.estimated_tokens);
    }

    // F3: nudge `cut_idx` forward until the first kept message is a
    // User message (or we hit the end). Without this, the kept tail
    // could start with an Assistant message — Anthropic + OpenAI
    // expect alternating user/assistant role order; an assistant
    // following a system summary breaks the role sequence and gets
    // rejected with a 400. opencode handles this via `splitTurn`
    // (`compaction.ts:161-184`).
    cut_idx = align_cut_to_user_boundary(&session.messages, cut_idx);

    if cut_idx == 0 {
        renderer.write_line("nothing to compress (entire context is recent)", c_agent())?;
        return Ok(CompressOutcome::NoOp {
            reason: "entire context is within keep_recent_tokens — lower it to compress further",
        });
    }

    let messages_to_summarize = &session.messages[..cut_idx];
    let previous_summary = session.compactions.last().map(|c| c.summary.as_str());

    // dirge-7tvq: give the memory provider a chance to inject
    // provider-extracted insights into the compression prompt before
    // the to-be-discarded messages are summarized. Returns an empty
    // string for providers that don't override the hook.
    let provider_insights = agent.memory_provider().map(|p| {
        let pre_compress_transcript =
            crate::agent::review::build_transcript_from_slice(messages_to_summarize);
        p.on_pre_compress(&pre_compress_transcript)
    });
    let augmented_instructions: Option<String> = match (instructions, provider_insights) {
        (Some(user), Some(extra)) if !extra.trim().is_empty() => {
            Some(format!("{}\n\nProvider insights:\n{}", user, extra))
        }
        (None, Some(extra)) if !extra.trim().is_empty() => {
            Some(format!("Provider insights:\n{}", extra))
        }
        (Some(user), _) => Some(user.to_string()),
        _ => None,
    };

    let summary = client
        .compress_messages(
            &session.model,
            messages_to_summarize,
            previous_summary,
            augmented_instructions.as_deref(),
        )
        .await?;

    let tokens_before: u64 = messages_to_summarize
        .iter()
        .map(|m| m.estimated_tokens)
        .sum();

    // F13: estimate the summary's own token cost so we can
    // report TRUE net savings instead of just "tokens replaced".
    // A pathological summary longer than the messages it
    // replaces means we just paid more tokens for less context.
    // We still proceed with the compress (the new prefix is the
    // SHAPE the LLM expects), but we want to surface the
    // misfire so the user can adjust `keep_recent_tokens` or
    // their custom compress prompt. opencode validates the
    // summary fits the budget BEFORE issuing the LLM call
    // (`compaction.ts:136-294`); dirge validates AFTER because
    // we don't know the summary's size until the LLM returns.
    let summary_tokens_est = crate::session::Session::estimate_tokens(&summary);
    let net_saved: i64 = tokens_before as i64 - summary_tokens_est as i64;

    // Audit M9: previously the summary was installed via
    // `compress_reporting` BEFORE the net-saved check, so an
    // oversized summary still landed in the session — we only
    // told the user *afterwards*. Refuse to install when the
    // summary would cost more than the messages it replaces; the
    // user can adjust `keep_recent_tokens` / their compress prompt
    // and re-issue. Skipping the install also avoids polluting the
    // session-tree with a node we'd want to revert.
    if net_saved < 0 {
        renderer.write_line(
            &format!(
                "compress aborted — summary ({}t) is LARGER than the {} messages it would replace ({}t); net cost +{}t. Compression rejected. Consider lowering keep_recent_tokens or refining compress instructions, then re-run /compress.",
                summary_tokens_est,
                cut_idx,
                tokens_before,
                -net_saved,
            ),
            c_error(),
        )?;
        return Ok(CompressOutcome::NoOp {
            reason: "summary would be larger than the messages it replaces",
        });
    }

    // `compress_reporting` returns the count of non-active-path
    // tree nodes (sibling branches) pruned. We notify the user
    // about that loss explicitly — without the notification a
    // branched session could silently lose forks during auto-
    // compaction. opencode (`session/compaction.ts:386-396`) drops
    // siblings silently; dirge prefers the explicit notification.
    let pruned_branches = session.compress_reporting(summary, cut_idx, tokens_before);

    let model = client.completion_model(session.model.to_string());
    *agent = crate::provider::build_agent(
        model,
        cli,
        cfg,
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
        Some(session.id.to_string()),
    )
    .await;
    renderer.write_line("prompt cleared (back to default behavior)", c_agent())?;

    render_session(renderer, session, cli, cfg, context)?;
    if pruned_branches > 0 {
        // Tell the user the branched topology shrunk. Without this,
        // they'd notice missing forks in `/tree` without any
        // explanation.
        renderer.write_line(
            &format!(
                "discarded {} forked branch node{} that were rooted in the compressed region",
                pruned_branches,
                if pruned_branches == 1 { "" } else { "s" },
            ),
            c_error(),
        )?;
    }
    // Net-saved is guaranteed non-negative here: the early-return
    // above (audit M9) aborts the compress when the summary would
    // cost more than the messages it replaced.
    {
        renderer.write_line(
            &format!(
                "compressed {} messages (saved ~{} tokens; summary uses {}t)",
                cut_idx, net_saved, summary_tokens_est,
            ),
            c_agent(),
        )?;
    }

    Ok(CompressOutcome::Compacted)
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_slash(
    text: &str,
    agent: &mut AnyAgent,
    client: &AnyClient,
    renderer: &mut Renderer,
    session: &mut Session,
    cli: &Cli,
    cfg: &Config,
    context: &mut ContextFiles,
    show_reasoning: &mut bool,
    is_running: &mut bool,
    input: &mut InputEditor,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    // Audit followup: same threading as ask_tx — every build_agent
    // rebuild inside handle_slash previously passed None for
    // question_tx + plan_tx, silently killing the `question` tool
    // and plan-switch hooks after any rebuild-triggering slash
    // command. Companion to the C8 LSP fix.
    question_tx: &Option<crate::agent::tools::question::QuestionSender>,
    plan_tx: &Option<crate::agent::tools::plan::PlanSwitchSender>,
    todo_tools_enabled: &mut bool,
    bg_store: &Option<crate::agent::tools::background::BackgroundStore>,
    sandbox: &Sandbox,
    #[cfg(feature = "loop")] loop_state: &mut Option<crate::extras::r#loop::LoopState>,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
    // C8 (audit fix): every prior agent-rebuild path (/model,
    // /prompt, /mode, /cd, /worktree, /wt-exit, /regen-prompts,
    // /loop start/stop, /toggle) passed None for lsp_manager into
    // build_agent. The user lost LSP silently after the first such
    // command. Thread the actual manager through.
    #[cfg(feature = "lsp")] lsp_manager: Option<&std::sync::Arc<crate::lsp::manager::LspManager>>,
) -> anyhow::Result<()> {
    let parts: SmallVec<[&str; 3]> = text.trim().splitn(3, ' ').collect();
    let mut ctx = SlashCtx {
        agent,
        client,
        renderer,
        session,
        cli,
        cfg,
        context,
        show_reasoning,
        is_running,
        input,
        permission,
        ask_tx,
        question_tx,
        plan_tx,
        todo_tools_enabled,
        bg_store,
        sandbox,
        #[cfg(feature = "loop")]
        loop_state,
        #[cfg(feature = "mcp")]
        mcp_manager,
        #[cfg(feature = "semantic")]
        semantic_manager,
        #[cfg(feature = "lsp")]
        lsp_manager,
    };
    match parts[0] {
        "/model" => cmd_model::cmd_model(&mut ctx, &parts).await?,
        "/sessions" => cmd_session::cmd_sessions(&mut ctx, &parts).await?,
        "/reasoning" => cmd_model::cmd_reasoning(&mut ctx).await?,
        "/mode" => cmd_model::cmd_mode(&mut ctx, &parts).await?,
        #[cfg(feature = "mcp")]
        "/mcp" => cmd_misc::cmd_mcp(&mut ctx, &parts).await?,
        "/toggle" => cmd_model::cmd_toggle(&mut ctx, &parts).await?,
        "/compress" | "/compact" => {
            // Deferred via sentinel — the outer event loop in
            // `ui/mod.rs` parses the `DEFER_COMPRESS:` prefix and
            // runs `handle_compress` with the freshly-built
            // dependencies.
            let instructions = if parts.len() > 1 {
                Some(parts[1..].join(" "))
            } else {
                None
            };
            let instr_str = instructions.clone().unwrap_or_default();
            return Err(anyhow::anyhow!("DEFER_COMPRESS:{}", instr_str));
        }
        "/loop" => cmd_misc::cmd_loop(&mut ctx, &parts, text).await?,
        "/prompt" => cmd_model::cmd_prompt(&mut ctx, &parts).await?,
        #[cfg(feature = "git-worktree")]
        "/worktree" => cmd_worktree::cmd_worktree(&mut ctx, &parts).await?,
        #[cfg(feature = "git-worktree")]
        "/wt-merge" => return cmd_worktree::cmd_wt_merge(&mut ctx, &parts).await,
        #[cfg(feature = "git-worktree")]
        "/wt-exit" => return cmd_worktree::cmd_wt_exit(&mut ctx, &parts).await,
        "/regen-prompts" => cmd_model::cmd_regen_prompts(&mut ctx).await?,
        "/quit" => return cmd_misc::cmd_quit(&mut ctx).await,
        "/tasks" => cmd_session::cmd_tasks(&mut ctx).await?,
        "/clear" => cmd_session::cmd_clear(&mut ctx).await?,
        "/tree" => cmd_session::cmd_tree(&mut ctx, &parts).await?,
        "/fork" => cmd_session::cmd_fork(&mut ctx, &parts).await?,
        "/clone" => cmd_session::cmd_clone(&mut ctx, &parts).await?,
        "/panel" => cmd_misc::cmd_panel(&mut ctx, &parts).await?,
        "/btw" => cmd_misc::cmd_btw(&mut ctx, &parts).await?,
        "/cd" => cmd_misc::cmd_cd(&mut ctx, text).await?,
        "/undo" => cmd_session::cmd_undo(&mut ctx).await?,
        "/retry" => cmd_session::cmd_retry(&mut ctx).await?,
        "/allow" => cmd_misc::cmd_allow(&mut ctx, &parts, text).await?,
        "/help" => cmd_misc::cmd_help(&mut ctx).await?,
        "/kill" => cmd_misc::cmd_kill(&mut ctx, &parts).await?,
        _ => {
            // If `slash_command_names()` advertised this command
            // but no match arm above caught it, the lists drifted
            // (added to the canonical list without wiring up the
            // dispatch). Emit a loud error here rather than falling
            // through to plugin lookup / "unknown command", so the
            // mistake is obvious in dev/test rather than silently
            // shadowed by either path. Plugin commands by
            // convention don't have a leading `/` in the canonical
            // list, so this won't false-fire on them.
            if is_known_slash_command(parts[0]) {
                ctx.renderer.write_line(
                    &format!(
                        "internal error: {} is listed in slash_command_names() but has no dispatch arm in handle_slash — wire it up or remove from the list",
                        parts[0]
                    ),
                    c_error(),
                )?;
                return Ok(());
            }

            // Fall through to plugin-registered commands. The process-global
            // PluginManager is the same one HookedToolDyn uses, so we don't
            // need to thread an Arc through handle_slash's already long
            // parameter list.
            #[cfg(feature = "plugin")]
            if let Some(pm_arc) = crate::plugin::hook::global() {
                let cmd = parts[0].trim_start_matches('/');
                let args = parts.get(1..).map(|p| p.join(" ")).unwrap_or_default();
                let handler = {
                    let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
                    mgr.list_commands()
                        .into_iter()
                        .find(|(name, _)| name == cmd)
                        .map(|(_, h)| h)
                };
                if let Some(handler_fn) = handler {
                    let result = {
                        let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
                        mgr.invoke_command(&handler_fn, &args)
                    };
                    match result {
                        Ok(Some(text)) => {
                            // Strip ANSI escapes from plugin output to
                            // prevent repaint/screen-manipulation attacks.
                            let safe = crate::ui::ansi::strip_escapes(
                                &text,
                                crate::ui::ansi::StripPolicy::KEEP_NEWLINE,
                            );
                            for line in safe.lines() {
                                ctx.renderer.write_line(line, c_agent())?;
                            }
                        }
                        Ok(None) => {
                            // Handler ran cleanly but had nothing to say — no-op.
                        }
                        Err(e) => {
                            ctx.renderer.write_line(
                                &format!("[plugin] {} failed: {}", cmd, e),
                                c_error(),
                            )?;
                        }
                    }
                    return Ok(());
                }
            }
            ctx.renderer.write_line(
                &format!("unknown command: {} (try /help)", parts[0]),
                c_error(),
            )?;
        }
    }
    Ok(())
}

/// Result of a slash-command tab completion.
#[allow(dead_code)]
pub struct CompletionResult {
    pub new_buffer: String,
    pub new_cursor: usize,
    /// The full sorted command list and the index of the currently-selected
    /// command, so the renderer can show a preview of upcoming items.
    pub all_commands: Vec<&'static str>,
    pub current_index: usize,
}

/// Try to complete the slash command at `cursor` in `buffer`.
/// Returns `Some(CompletionResult)` if completion was possible.
/// Tab cycles through matching commands; when narrowed to one match,
/// subsequent tabs cycle through ALL commands so the user can keep browsing.
#[cfg(feature = "experimental-ui-tab-slash")]
pub fn try_complete(buffer: &str, cursor: usize) -> Option<CompletionResult> {
    if !buffer.starts_with('/') {
        return None;
    }

    let cursor = cursor.min(buffer.len());
    // Find the bounds of the FIRST word in the buffer (the command
    // name). Previously the replacement range was
    // `[word_start..cursor]`, which corrupted the buffer when the
    // cursor sat mid-word: e.g. Tab with cursor at byte 2 of `/mod`
    // produced `/mcpod` (replacement appended to the tail after the
    // cursor). Anchor the replacement to the whole word boundary
    // instead — the cursor can land anywhere inside the command and
    // the result is still well-formed.
    let word_start = 0usize;
    let word_end = buffer.find(char::is_whitespace).unwrap_or(buffer.len());
    let current_word = &buffer[word_start..word_end];

    // Only complete when the cursor is inside (or just after) the
    // first word. Cursor past the first whitespace means the user is
    // typing args, not a command name.
    if cursor > word_end {
        return None;
    }

    let all_commands = builtin_commands();
    let matching: Vec<&str> = all_commands
        .iter()
        .filter(|c| c.starts_with(current_word))
        .copied()
        .collect();

    if matching.is_empty() {
        return None;
    }

    // Once the current word is an exact command name, cycle through ALL
    // commands so the user can keep browsing.  Otherwise stay within the
    // matching prefix subset (e.g. /mod → /mode, /model → repeats).
    let is_exact = all_commands.contains(&current_word);

    let (replacement, current_index) = if is_exact {
        let all_idx = all_commands.iter().position(|c| *c == current_word);
        let next_idx = match all_idx {
            Some(i) => (i + 1) % all_commands.len(),
            None => 0,
        };
        (all_commands[next_idx], next_idx)
    } else {
        let current_idx = matching.iter().position(|c| *c == current_word);
        let next_idx = match current_idx {
            Some(i) => (i + 1) % matching.len(),
            None => 0,
        };
        let cmd = matching[next_idx];
        let all_idx = all_commands.iter().position(|c| *c == cmd).unwrap_or(0);
        (cmd, all_idx)
    };
    // Build the new buffer: prefix (everything before the word —
    // always empty here since word_start==0) + replacement + tail
    // (everything from word_end onward). Cursor lands at the end of
    // the replacement so the user can immediately type args.
    let mut new_buffer = String::with_capacity(replacement.len() + buffer.len() - word_end);
    new_buffer.push_str(replacement);
    new_buffer.push_str(&buffer[word_end..]);
    let new_cursor = replacement.len();
    Some(CompletionResult {
        new_buffer,
        new_cursor,
        all_commands,
        current_index,
    })
}

/// Canonical list of built-in slash commands. **Single source of
/// truth** consulted by:
///   * `builtin_commands()`     — tab completion (feature-gated)
///   * `is_known_slash_command` — handle_slash's "internal error"
///     vs "unknown command" branching in the default arm
///
/// **When you add a new slash command to `handle_slash`'s match
/// arms, add it here too.** A drift in the other direction (listed
/// here but no match arm in `handle_slash`) surfaces at runtime as
/// an explicit `internal error: known command X reached default
/// arm` so the mistake is loud rather than silent.
///
/// Always-compiled (not feature-gated) because `handle_slash`'s
/// default arm consults it regardless of the tab-completion
/// feature.
pub fn slash_command_names() -> Vec<&'static str> {
    let mut cmds = vec![
        "/allow",
        "/btw",
        "/cd",
        "/clear",
        "/clone",
        "/compact",
        "/compress",
        "/fork",
        "/help",
        "/kill",
        "/mode",
        "/model",
        "/panel",
        "/prompt",
        "/quit",
        "/reasoning",
        "/regen-prompts",
        "/retry",
        "/sessions",
        "/tasks",
        "/toggle",
        "/tree",
        "/undo",
    ];
    #[cfg(feature = "git-worktree")]
    {
        cmds.push("/worktree");
        cmds.push("/wt-exit");
        cmds.push("/wt-merge");
    }
    #[cfg(feature = "mcp")]
    cmds.push("/mcp");
    #[cfg(feature = "loop")]
    cmds.push("/loop");
    cmds.sort_unstable();
    cmds
}

/// Returns true if `name` (with leading `/`) is a built-in slash
/// command. Used by `handle_slash`'s default arm to distinguish
/// "command name we should have dispatched but didn't" (internal
/// error) from "command name we don't know about" (plugin fallback /
/// unknown).
pub fn is_known_slash_command(name: &str) -> bool {
    slash_command_names().contains(&name)
}

/// Returns all built-in slash commands (with leading `/`), sorted alphabetically.
#[cfg(feature = "experimental-ui-tab-slash")]
pub fn builtin_commands() -> Vec<&'static str> {
    slash_command_names()
}

/// Format a completion preview string showing upcoming commands.
/// Returns an empty string when `cr` is `None`. The result is shaped
/// to fit within `avail_w` display cells (after the continuation
/// prompt), showing as many upcoming command names as will fit.
#[cfg(feature = "experimental-ui-tab-slash")]
pub fn format_completion_preview(cr: Option<&CompletionResult>, avail_w: usize) -> String {
    let cr = match cr {
        Some(c) => c,
        None => return String::new(),
    };
    if cr.all_commands.is_empty() || avail_w < 4 {
        return String::new();
    }
    let all = &cr.all_commands;
    let start = (cr.current_index + 1) % all.len();
    let mut result = String::new();
    for i in 0..all.len() {
        let cmd = all[(start + i) % all.len()];
        let candidate = if result.is_empty() {
            cmd.to_string()
        } else {
            format!("{result}  {cmd}")
        };
        use unicode_width::UnicodeWidthStr;
        if UnicodeWidthStr::width(candidate.as_str()) > avail_w {
            break;
        }
        result = candidate;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionMessage};

    fn msg(role: MessageRole, content: &str) -> SessionMessage {
        // Re-use Session::add_message to get a real msg with id/timestamp.
        let mut s = Session::new("p", "m", 0);
        s.add_message(role, content);
        s.messages.pop().unwrap()
    }

    /// F3: when the reverse-scan lands on an Assistant message, the
    /// helper advances to the next User so the kept tail begins
    /// with a User message (provider-required role sequence after
    /// the System summary).
    #[test]
    fn align_cut_advances_past_assistant_to_next_user() {
        let messages = vec![
            msg(MessageRole::User, "u0"),
            msg(MessageRole::Assistant, "a0"),
            msg(MessageRole::User, "u1"),
            msg(MessageRole::Assistant, "a1"), // reverse-scan landed here (cut_idx=3)
            msg(MessageRole::User, "u2"),
            msg(MessageRole::Assistant, "a2"),
        ];
        // Initial cut at idx=3 (Assistant). Should advance to 4 (User).
        assert_eq!(align_cut_to_user_boundary(&messages, 3), 4);
    }

    /// User-boundary cut is unchanged.
    #[test]
    fn align_cut_idempotent_when_already_on_user() {
        let messages = vec![
            msg(MessageRole::User, "u0"),
            msg(MessageRole::Assistant, "a0"),
            msg(MessageRole::User, "u1"),
        ];
        assert_eq!(align_cut_to_user_boundary(&messages, 2), 2);
        assert_eq!(align_cut_to_user_boundary(&messages, 0), 0);
    }

    /// Cut past end of array stays past end.
    #[test]
    fn align_cut_past_end_clamps() {
        let messages = vec![msg(MessageRole::User, "u0")];
        assert_eq!(align_cut_to_user_boundary(&messages, 1), 1);
        assert_eq!(align_cut_to_user_boundary(&messages, 5), 5);
    }

    /// No user in the tail (e.g. only system+assistant remain after
    /// the cut). Helper returns `messages.len()`, which is the
    /// "nothing to compress (no clean boundary)" case.
    #[test]
    fn align_cut_returns_end_when_no_user_in_tail() {
        let messages = vec![
            msg(MessageRole::User, "u0"),
            msg(MessageRole::Assistant, "a0"),
            msg(MessageRole::System, "system note"),
            msg(MessageRole::Assistant, "a1"),
        ];
        // cut_idx=2 points at System; no User follows.
        assert_eq!(align_cut_to_user_boundary(&messages, 2), messages.len());
    }

    /// A cut that lands on a System message (e.g. a prior summary)
    /// also advances forward to the next User.
    #[test]
    fn align_cut_skips_system_to_user() {
        let messages = vec![
            msg(MessageRole::System, "prior summary"),
            msg(MessageRole::User, "u0"),
            msg(MessageRole::Assistant, "a0"),
        ];
        assert_eq!(align_cut_to_user_boundary(&messages, 0), 1);
    }

    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn no_completion_without_slash() {
        assert!(try_complete("hello", 5).is_none());
    }

    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn empty_buffer_returns_none() {
        assert!(try_complete("", 0).is_none());
    }

    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn complete_partial_command() {
        let r = try_complete("/mod", 4).unwrap();
        assert_eq!(r.new_buffer, "/mode");
        assert_eq!(r.new_cursor, 5);
    }

    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn cycles_between_partial_matches() {
        let r = try_complete("/mod", 4).unwrap();
        assert!(r.new_buffer.starts_with("/mod"));
    }

    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn cycles_beyond_single_match() {
        let r1 = try_complete("/", 1).unwrap();
        let r2 = try_complete(&r1.new_buffer, r1.new_cursor).unwrap();
        assert_ne!(r1.new_buffer, r2.new_buffer);
        assert!(!r2.new_buffer.is_empty());
        assert!(r2.new_buffer.starts_with('/'));
    }

    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn cycles_from_full_command() {
        let r = try_complete("/btw", 4).unwrap();
        assert_ne!(r.new_buffer, "/btw");
        assert!(r.new_buffer.starts_with('/'));
    }

    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn cycles_through_all_commands() {
        let mut seen = std::collections::HashSet::new();
        let mut buf = "/".to_string();
        let mut cur = 1;
        for _ in 0..100 {
            let result = try_complete(&buf, cur);
            if result.is_none() {
                break;
            }
            let r = result.unwrap();
            buf = r.new_buffer;
            cur = r.new_cursor;
            seen.insert(buf.clone());
        }
        let all = builtin_commands();
        assert_eq!(
            seen.len(),
            all.len(),
            "should cycle through all builtin commands"
        );
    }

    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn unknown_command_returns_none() {
        assert!(try_complete("/nonexistent", 12).is_none());
    }

    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn commands_are_sorted() {
        let cmds = builtin_commands();
        for pair in cmds.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "{} should be before {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn preview_includes_upcoming_commands() {
        let r = try_complete("/", 1).unwrap();
        let all = &r.all_commands;
        let cur = r.current_index;
        let upcoming = &all[(cur + 1)..];
        assert!(
            !upcoming.is_empty(),
            "should have commands after the current one"
        );
    }

    // ============================================================
    // Code-review B1 fix: cursor mid-word produces well-formed
    // buffer
    // ============================================================

    /// Regression: cursor sitting at byte 2 of `/mod` previously
    /// produced `/mcpod` (replacement appended to the tail AFTER
    /// the cursor, with `od` left over from the original word).
    /// The fix anchors replacement to the whole-word boundary
    /// instead, so cursor position inside the command name doesn't
    /// corrupt the buffer — the result is exactly one of the
    /// matching commands, no Frankenstein.
    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn complete_with_cursor_mid_word_produces_clean_buffer() {
        // /mod, cursor at the `o` (byte 2). The new buffer must be
        // exactly a candidate command — not the candidate +
        // residual `od` from the source.
        let r = try_complete("/mod", 2).unwrap();
        let candidates = builtin_commands()
            .into_iter()
            .filter(|c| c.starts_with("/mod"))
            .collect::<Vec<_>>();
        assert!(
            candidates.contains(&r.new_buffer.as_str()),
            "{:?} must be one of the /mod* commands {:?} — no Frankenstein concatenation",
            r.new_buffer,
            candidates,
        );
        assert_eq!(
            r.new_cursor,
            r.new_buffer.len(),
            "cursor should land at end of replacement",
        );
    }

    /// Cursor at byte 0 (Home before Tab) used to produce
    /// `/allow/mod` because the entire buffer was concatenated as
    /// the tail. Verify it now produces a clean replacement —
    /// exactly one of the matching commands, no residual `/mod`.
    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn complete_with_cursor_at_start_produces_clean_buffer() {
        let r = try_complete("/mod", 0).unwrap();
        let candidates = builtin_commands()
            .into_iter()
            .filter(|c| c.starts_with("/mod"))
            .collect::<Vec<_>>();
        assert!(
            candidates.contains(&r.new_buffer.as_str()),
            "{:?} must be a /mod* command (clean replacement, no /mod residual): candidates {:?}",
            r.new_buffer,
            candidates,
        );
    }

    /// Tab on a command with trailing args (e.g. `/mode standard`
    /// with cursor after `/mode`) preserves the args tail.
    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn complete_preserves_trailing_args() {
        let r = try_complete("/mod standard", 4).unwrap();
        assert!(
            r.new_buffer.ends_with(" standard"),
            "args after the command should be preserved: {:?}",
            r.new_buffer
        );
    }

    /// Cursor past the first whitespace means the user is typing
    /// args, not a command name — no completion should fire.
    #[cfg(feature = "experimental-ui-tab-slash")]
    #[test]
    fn no_completion_when_cursor_in_args() {
        // Cursor inside the args portion of "/mode standard".
        let buf = "/mode standard";
        let cursor = buf.len(); // past the space
        assert!(try_complete(buf, cursor).is_none());
    }

    // ============================================================
    // Code-review B2 fix: canonical command list + drift guard
    // ============================================================

    /// `is_known_slash_command` must agree with `slash_command_names`
    /// since the helper just iterates the list. Catches a future
    /// refactor that decouples them (e.g. someone introducing a
    /// second hardcoded match).
    #[test]
    fn is_known_slash_command_agrees_with_canonical_list() {
        for name in slash_command_names() {
            assert!(
                is_known_slash_command(name),
                "{name} is in slash_command_names() but is_known_slash_command rejects it",
            );
        }
        // Spot-check negatives.
        assert!(!is_known_slash_command("/not-a-real-command"));
        assert!(!is_known_slash_command(""));
        assert!(!is_known_slash_command("/"));
    }

    /// The canonical list is sorted (tab completion preview relies
    /// on stable ordering for the cycle direction).
    #[test]
    fn slash_command_names_is_sorted() {
        let cmds = slash_command_names();
        for pair in cmds.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "{} should sort before {}",
                pair[0],
                pair[1]
            );
        }
    }

    /// Pin that the canonical list and `handle_slash`'s actual
    /// match arms agree on the always-on commands. If a name in
    /// the list above is missing from the dispatch tree the user
    /// would hit the new "internal error" arm at runtime; this
    /// duplicates the check in plain test code so a future
    /// maintainer sees the gap before users do.
    ///
    /// This DOES NOT enforce the reverse direction (arm present in
    /// `handle_slash` but missing from `slash_command_names`) — that
    /// would require parsing source. We accept it as the lesser
    /// drift: the only user-visible cost is that the missing
    /// command isn't tab-completable.
    #[test]
    fn always_on_commands_appear_in_canonical_list() {
        // Subset that is unconditionally compiled (no cfg) and
        // therefore must always be present. Cross-checked by hand
        // against the `match parts[0]` arms in `handle_slash`.
        const ALWAYS_ON: &[&str] = &[
            "/allow",
            "/btw",
            "/cd",
            "/clear",
            "/clone",
            "/compact",
            "/compress",
            "/fork",
            "/help",
            "/mode",
            "/model",
            "/panel",
            "/prompt",
            "/quit",
            "/reasoning",
            "/regen-prompts",
            "/retry",
            "/sessions",
            "/tasks",
            "/toggle",
            "/tree",
            "/undo",
        ];
        let list = slash_command_names();
        for name in ALWAYS_ON {
            assert!(
                list.contains(name),
                "{name} must appear in slash_command_names() — it's an always-on dispatch arm in handle_slash",
            );
        }
    }
}
