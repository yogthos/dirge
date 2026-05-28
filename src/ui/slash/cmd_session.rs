//! Session and history slash command handlers:
//! /sessions, /tree, /fork, /clone, /undo, /retry, /tasks, /clear.

use super::{SlashCtx, c_agent, c_error, c_result, undo_last};
use crate::session::MessageRole;
use crate::ui::events::{format_time, render_session};
use crate::ui::theme;
use crate::ui::tree::{self, short_id};

pub(super) async fn cmd_sessions(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        let sessions = crate::session::storage::find_recent_sessions(20)?;
        if sessions.is_empty() {
            ctx.renderer.write_line("no saved sessions", c_agent())?;
        } else {
            ctx.renderer
                .write_line(&format!("recent sessions ({}):", sessions.len()), c_agent())?;
            for s in &sessions {
                let last = s
                    .messages
                    .last()
                    .map(|m| format!("...{}", &m.content.chars().take(30).collect::<String>()))
                    .unwrap_or_default();
                let time = format_time(&s.updated_at);
                ctx.renderer.write_line(
                    &format!(
                        "  {}  {}  {}msgs  {}  {}",
                        &s.id[..8],
                        time,
                        s.messages.len(),
                        s.model,
                        last
                    ),
                    c_result(),
                )?;
            }
        }
    } else if parts[1] == "delete" && parts.len() >= 3 {
        let prefix = parts[2].trim();
        let sessions = crate::session::storage::find_sessions_by_prefix(prefix)?;
        if sessions.is_empty() {
            ctx.renderer
                .write_line(&format!("no session matching '{}'", prefix), c_agent())?;
        } else if sessions.len() == 1 {
            if let Some(s) = sessions.into_iter().next() {
                let id = s.id.clone();
                let preview = s
                    .messages
                    .last()
                    .map(|m| format!("...{}", &m.content.chars().take(40).collect::<String>()))
                    .unwrap_or_default();
                if let Err(e) = crate::session::storage::delete_session(&id) {
                    ctx.renderer
                        .write_line(&format!("failed to delete: {}", e), c_error())?;
                } else {
                    ctx.renderer.write_line(
                        &format!("deleted session {} {}", &id[..8], preview),
                        c_agent(),
                    )?;
                }
            }
        } else {
            ctx.renderer.write_line(
                &format!("multiple sessions match '{}', be more specific", prefix),
                c_agent(),
            )?;
            for s in &sessions {
                let last = s
                    .messages
                    .last()
                    .map(|m| format!("...{}", &m.content.chars().take(30).collect::<String>()))
                    .unwrap_or_default();
                let time = format_time(&s.updated_at);
                ctx.renderer.write_line(
                    &format!(
                        "  {}  {}  {}msgs  {}  {}",
                        &s.id[..8],
                        time,
                        s.messages.len(),
                        s.model,
                        last
                    ),
                    c_result(),
                )?;
            }
        }
    } else {
        let prefix = parts[1].trim();
        let sessions = crate::session::storage::find_sessions_by_prefix(prefix)?;
        if sessions.is_empty() {
            ctx.renderer
                .write_line(&format!("no session matching '{}'", prefix), c_agent())?;
        } else if sessions.len() == 1 {
            if let Some(s) = sessions.into_iter().next() {
                let msg_count = s.messages.len();
                if let Some(store) = ctx.bg_store.as_ref() {
                    store.cancel_all();
                }
                // dirge-7tvq: fire the outgoing session's
                // on_session_end provider hook BEFORE we replace
                // `*ctx.session` — at this point the old agent
                // still holds the provider keyed to the leaving
                // session. Build a transcript from the live
                // session and dispatch.
                if let Some(provider) = ctx.agent.memory_provider() {
                    let transcript = crate::agent::review::build_transcript(ctx.session);
                    provider.on_session_end(&transcript);
                }
                *ctx.session = s;
                let restored = ctx.session.current_prompt_name.clone();
                if let Some(name) = restored.as_deref()
                    && let Some(p) = ctx.context.prompts.get(name).cloned()
                {
                    ctx.context.current_prompt = Some(p.body.clone());
                    ctx.context.current_prompt_name = Some(name.to_string());
                    ctx.context.current_prompt_deny_tools = p.deny_tools.clone();
                    crate::permission::apply_prompt_deny(
                        ctx.permission,
                        &ctx.context.current_prompt_deny_tools,
                    );
                }
                // dirge-502b: rebuild the agent unconditionally after a
                // session swap, so `SessionSearchTool` picks up the new
                // session id and stops including the live session's
                // own turns. Pre-fix the rebuild was gated on a prompt
                // restore, which left the agent holding the previous
                // session's id (or the very session the model is
                // now in) — exact regression of the bug this branch
                // fixes for the initial-build path.
                let model = ctx.client.completion_model(ctx.session.model.to_string());
                *ctx.agent = crate::provider::build_agent(
                    model,
                    ctx.cli,
                    ctx.cfg,
                    ctx.context,
                    ctx.permission.clone(),
                    ctx.ask_tx.clone(),
                    ctx.question_tx.clone(),
                    ctx.plan_tx.clone(),
                    ctx.bg_store.clone(),
                    #[cfg(feature = "lsp")]
                    ctx.lsp_manager.cloned(),
                    ctx.sandbox.clone(),
                    #[cfg(feature = "mcp")]
                    ctx.mcp_manager,
                    #[cfg(feature = "semantic")]
                    ctx.semantic_manager,
                    Some(ctx.session.id.to_string()),
                )
                .await;
                render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
                let prompt_note = restored
                    .map(|n| format!("; prompt: {}", n))
                    .unwrap_or_default();
                ctx.renderer.write_line(
                    &format!("loaded session ({} msgs{})", msg_count, prompt_note),
                    c_agent(),
                )?;
            }
        } else {
            ctx.renderer
                .write_line(&format!("multiple sessions match '{}':", prefix), c_agent())?;
            for s in &sessions {
                let last = s
                    .messages
                    .last()
                    .map(|m| format!("...{}", &m.content.chars().take(30).collect::<String>()))
                    .unwrap_or_default();
                let time = format_time(&s.updated_at);
                ctx.renderer.write_line(
                    &format!(
                        "  {}  {}  {}msgs  {}  {}",
                        &s.id[..8],
                        time,
                        s.messages.len(),
                        s.model,
                        last
                    ),
                    c_result(),
                )?;
            }
        }
    }
    Ok(())
}

pub(super) async fn cmd_tasks(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let names = ctx.renderer.chat_names();
    if names.len() <= 1 {
        ctx.renderer.write_line(
            "no subagent chats yet — spawn one via the `task` tool.",
            c_result(),
        )?;
    } else {
        ctx.renderer.write_line("chat windows:", c_result())?;
        let active = ctx.renderer.active_chat();
        for (i, name) in names.iter().enumerate() {
            let marker = if i == active { "→" } else { " " };
            ctx.renderer
                .write_line(&format!("  {} [{}] {}", marker, i, name), c_result())?;
        }
        ctx.renderer
            .write_line("  (Ctrl-N / Ctrl-P / Ctrl-X to switch)", theme::dim())?;
    }
    Ok(())
}

pub(super) async fn cmd_clear(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    ctx.session.messages.clear();
    ctx.session.total_estimated_tokens = 0;
    ctx.session.compactions.clear();
    ctx.session.message_store.clear();
    ctx.session.tree.entries.clear();
    ctx.session.tree.leaf_id = None;
    crate::agent::tools::modified::clear_modified();
    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
    Ok(())
}

pub(super) async fn cmd_tree(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    ctx.session.ensure_tree_initialized();
    ctx.session.ensure_message_store_initialized();
    let arg = parts.get(1).copied().unwrap_or("").trim();
    if arg.is_empty() {
        if ctx.session.tree.entries.is_empty() {
            ctx.renderer.write_line("(empty session)", c_agent())?;
        } else {
            for line in tree::render_tree(ctx.session) {
                ctx.renderer.write_line(&line, c_result())?;
            }
        }
    } else {
        match tree::resolve_id_prefix(ctx.session, arg) {
            Ok(id) => {
                if let Err(e) = ctx.session.switch_to_leaf(&id) {
                    ctx.renderer
                        .write_line(&format!("switch failed: {}", e), c_error())?;
                } else {
                    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
                    ctx.renderer
                        .write_line(&format!("switched to leaf {}", short_id(&id)), c_agent())?;
                }
            }
            Err(e) => ctx
                .renderer
                .write_line(&format!("/tree: {}", e), c_error())?,
        }
    }
    Ok(())
}

pub(super) async fn cmd_fork(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    ctx.session.ensure_tree_initialized();
    ctx.session.ensure_message_store_initialized();
    let arg = parts.get(1).copied().unwrap_or("").trim();
    let target_id = if arg.is_empty() {
        let last_user = ctx
            .session
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.id.clone());
        match last_user {
            Some(id) => Ok(id),
            None => Err("no user message on current branch".to_string()),
        }
    } else {
        tree::resolve_id_prefix(ctx.session, arg)
    };
    match target_id {
        Ok(id) => match ctx.session.fork_at(&id) {
            Ok(original) => {
                ctx.input.set_text(&original.content);
                render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
                ctx.renderer.write_line(
                    &format!(
                        "forked at {} — original prompt restored to editor",
                        short_id(&id)
                    ),
                    c_agent(),
                )?;
            }
            Err(e) => ctx
                .renderer
                .write_line(&format!("/fork: {}", e), c_error())?,
        },
        Err(e) => ctx
            .renderer
            .write_line(&format!("/fork: {}", e), c_error())?,
    }
    Ok(())
}

pub(super) async fn cmd_clone(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    ctx.session.ensure_tree_initialized();
    ctx.session.ensure_message_store_initialized();
    let arg = parts.get(1).copied().unwrap_or("").trim();
    if arg.is_empty() {
        ctx.renderer
            .write_line("usage: /clone <id-prefix>", c_error())?;
    } else {
        match tree::resolve_id_prefix(ctx.session, arg) {
            Ok(id) => match ctx.session.clone_at(&id) {
                Ok(()) => {
                    render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
                    ctx.renderer
                        .write_line(&format!("cloned path through {}", short_id(&id)), c_agent())?;
                }
                Err(e) => ctx
                    .renderer
                    .write_line(&format!("/clone: {}", e), c_error())?,
            },
            Err(e) => ctx
                .renderer
                .write_line(&format!("/clone: {}", e), c_error())?,
        }
    }
    Ok(())
}

pub(super) async fn cmd_undo(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let outcome = undo_last(ctx.session);
    if outcome.removed > 0 {
        render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
        ctx.renderer.write_line(
            &format!("removed {} message(s)", outcome.removed),
            c_agent(),
        )?;
        if outcome.had_tool_calls {
            ctx.renderer.write_line(
                "warning: tool side effects (file writes, bash, MCP) were NOT reverted",
                c_error(),
            )?;
        }
    } else {
        ctx.renderer.write_line("nothing to undo", c_agent())?;
    }
    Ok(())
}

pub(super) async fn cmd_retry(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let last_user = ctx
        .session
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .cloned();
    match last_user {
        Some(msg) => {
            let mut guard = ctx.session.messages.len();
            while let Some(last) = ctx.session.messages.last() {
                let was_user = last.role == MessageRole::User;
                ctx.session.pop_last_message();
                if was_user {
                    break;
                }
                guard = guard.saturating_sub(1);
                if guard == 0 {
                    break;
                }
            }
            ctx.input.buffer = msg.content.clone();
            ctx.input.cursor = msg.content.len();
            render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
            ctx.renderer
                .write_line("edit last message and press Enter to retry", c_agent())?;
        }
        None => {
            ctx.renderer
                .write_line("no previous message to retry", c_agent())?;
        }
    }
    Ok(())
}
