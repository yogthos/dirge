use compact_str::CompactString;
use crossterm::style::Color;
use smallvec::SmallVec;

use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::SecurityMode;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::{AnyAgent, AnyClient};
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;
use crate::session::{MessageRole, Session};
use crate::ui::events::{format_time, render_session};
use crate::ui::input::InputEditor;
use crate::ui::renderer::Renderer;
use crate::ui::theme;
use crate::ui::tree::{self, short_id};

#[inline]
fn c_agent() -> Color {
    theme::agent()
}
#[inline]
fn c_result() -> Color {
    theme::result()
}
#[inline]
fn c_error() -> Color {
    theme::error()
}

pub fn undo_last(session: &mut Session) -> usize {
    let len = session.messages.len();
    if len == 0 {
        return 0;
    }
    // Route through `pop_last_message` so the tree + message_store
    // stay in sync — P4c made direct .messages.pop() incorrect for
    // branched sessions.
    if session.messages[len - 1].role == MessageRole::Assistant {
        session.pop_last_message();
        if session
            .messages
            .last()
            .is_some_and(|m| m.role == MessageRole::User)
        {
            session.pop_last_message();
            return 2;
        }
        return 1;
    }
    if session.messages[len - 1].role == MessageRole::User {
        session.pop_last_message();
        return 1;
    }
    0
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
    bg_store: &Option<crate::agent::tools::background::BackgroundStore>,
    sandbox: &Sandbox,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
) -> anyhow::Result<()> {
    renderer.write_line("compressing...", c_agent())?;
    renderer.write_line("", Color::White)?;

    let reserve = cfg.resolve_reserve_tokens();
    let keep_recent = cfg.resolve_keep_recent_tokens();
    let max_tokens = session.context_window.saturating_sub(reserve);

    if session.total_estimated_tokens <= max_tokens {
        renderer.write_line("context within limits, no compression needed", c_agent())?;
        return Ok(());
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

    if cut_idx == 0 {
        renderer.write_line("nothing to compress (entire context is recent)", c_agent())?;
        return Ok(());
    }

    let messages_to_summarize = &session.messages[..cut_idx];
    let previous_summary = session.compactions.last().map(|c| c.summary.as_str());

    let summary = client
        .compress_messages(
            &session.model,
            messages_to_summarize,
            previous_summary,
            instructions,
        )
        .await?;

    let tokens_before: u64 = messages_to_summarize
        .iter()
        .map(|m| m.estimated_tokens)
        .sum();

    session.compress(summary, cut_idx, tokens_before);

    let model = client.completion_model(session.model.to_string());
    *agent = crate::provider::build_agent(
        model,
        cli,
        cfg,
        context,
        permission.clone(),
        ask_tx.clone(),
        None,
        None,
        bg_store.clone(),
        #[cfg(feature = "lsp")]
        None,
        sandbox.clone(),
        #[cfg(feature = "mcp")]
        mcp_manager,
        #[cfg(feature = "semantic")]
        semantic_manager,
    )
    .await;
    renderer.write_line("prompt cleared (back to default behavior)", c_agent())?;

    render_session(renderer, session, cli, cfg, context)?;
    renderer.write_line(
        &format!(
            "compressed {} messages (saved ~{} tokens)",
            cut_idx, tokens_before,
        ),
        c_agent(),
    )?;

    Ok(())
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
    todo_tools_enabled: &mut bool,
    bg_store: &Option<crate::agent::tools::background::BackgroundStore>,
    sandbox: &Sandbox,
    #[cfg(feature = "loop")] loop_state: &mut Option<crate::extras::r#loop::LoopState>,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
) -> anyhow::Result<()> {
    let parts: SmallVec<[&str; 3]> = text.trim().splitn(3, ' ').collect();
    match parts[0] {
        "/model" => {
            if parts.len() < 2 {
                renderer.write_line(&format!("current model: {}", session.model), c_agent())?;
            } else {
                let new_model = CompactString::new(parts[1].trim());
                let model = client.completion_model(new_model.to_string());
                *agent = crate::provider::build_agent(
                    model,
                    cli,
                    cfg,
                    context,
                    permission.clone(),
                    ask_tx.clone(),
                    None,
                    None,
                    bg_store.clone(),
                    #[cfg(feature = "lsp")]
                    None,
                    sandbox.clone(),
                    #[cfg(feature = "mcp")]
                    mcp_manager,
                    #[cfg(feature = "semantic")]
                    semantic_manager,
                )
                .await;
                session.model = new_model.clone();
                session.provider = cli.resolve_provider(cfg);
                renderer.write_line(&format!("switched to model: {}", new_model), c_agent())?;
            }
        }
        "/sessions" => {
            if parts.len() < 2 {
                let sessions = crate::session::storage::find_recent_sessions(20)?;
                if sessions.is_empty() {
                    renderer.write_line("no saved sessions", c_agent())?;
                } else {
                    renderer
                        .write_line(&format!("recent sessions ({}):", sessions.len()), c_agent())?;
                    for s in &sessions {
                        let last = s
                            .messages
                            .last()
                            .map(|m| {
                                format!("...{}", &m.content.chars().take(30).collect::<String>())
                            })
                            .unwrap_or_default();
                        let time = format_time(&s.updated_at);
                        renderer.write_line(
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
                    renderer.write_line(&format!("no session matching '{}'", prefix), c_agent())?;
                } else if sessions.len() == 1 {
                    if let Some(s) = sessions.into_iter().next() {
                        let id = s.id.clone();
                        let preview = s
                            .messages
                            .last()
                            .map(|m| {
                                format!("...{}", &m.content.chars().take(40).collect::<String>())
                            })
                            .unwrap_or_default();
                        if let Err(e) = crate::session::storage::delete_session(&id) {
                            renderer.write_line(&format!("failed to delete: {}", e), c_error())?;
                        } else {
                            renderer.write_line(
                                &format!("deleted session {} {}", &id[..8], preview),
                                c_agent(),
                            )?;
                        }
                    }
                } else {
                    renderer.write_line(
                        &format!("multiple sessions match '{}', be more specific", prefix),
                        c_agent(),
                    )?;
                    for s in &sessions {
                        let last = s
                            .messages
                            .last()
                            .map(|m| {
                                format!("...{}", &m.content.chars().take(30).collect::<String>())
                            })
                            .unwrap_or_default();
                        let time = format_time(&s.updated_at);
                        renderer.write_line(
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
                    renderer.write_line(&format!("no session matching '{}'", prefix), c_agent())?;
                } else if sessions.len() == 1 {
                    if let Some(s) = sessions.into_iter().next() {
                        let msg_count = s.messages.len();
                        *session = s;
                        render_session(renderer, session, cli, cfg, context)?;
                        renderer.write_line(
                            &format!("loaded session ({} msgs)", msg_count),
                            c_agent(),
                        )?;
                    }
                } else {
                    renderer
                        .write_line(&format!("multiple sessions match '{}':", prefix), c_agent())?;
                    for s in &sessions {
                        let last = s
                            .messages
                            .last()
                            .map(|m| {
                                format!("...{}", &m.content.chars().take(30).collect::<String>())
                            })
                            .unwrap_or_default();
                        let time = format_time(&s.updated_at);
                        renderer.write_line(
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
        }
        "/reasoning" => {
            *show_reasoning = !*show_reasoning;
            renderer.write_line(
                &format!(
                    "reasoning visibility: {}",
                    if *show_reasoning { "on" } else { "off" }
                ),
                c_agent(),
            )?;
        }
        "/mode" => {
            let current_mode = permission
                .as_ref()
                .map(|p| p.lock().unwrap_or_else(|e| e.into_inner()).mode())
                .unwrap_or(SecurityMode::Standard);

            if parts.len() < 2 {
                renderer.write_line("security mode:", c_agent())?;
                renderer.write_line(&format!("  current: {}", current_mode), c_result())?;
                renderer.write_line("", c_agent())?;
                renderer.write_line(
                    "  /mode standard      use configured permission rules",
                    c_result(),
                )?;
                renderer
                    .write_line("  /mode restrictive   default all tools to ask", c_result())?;
                renderer.write_line(
                    "  /mode accept        auto-accept within working directory",
                    c_result(),
                )?;
                renderer.write_line(
                    "  /mode yolo          auto-accept ALL operations",
                    c_result(),
                )?;
                renderer.write_line("", c_agent())?;
            } else {
                match parts[1] {
                    "standard" => {
                        if let Some(p) = permission {
                            p.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .set_mode(SecurityMode::Standard);
                            renderer.write_line("security mode: standard", c_agent())?;
                        } else {
                            renderer.write_line("permission system not active", c_error())?;
                        }
                    }
                    "restrictive" => {
                        if let Some(p) = permission {
                            p.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .set_mode(SecurityMode::Restrictive);
                            renderer.write_line("security mode: restrictive", c_agent())?;
                        } else {
                            renderer.write_line("permission system not active", c_error())?;
                        }
                    }
                    "accept" => {
                        if let Some(p) = permission {
                            p.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .set_mode(SecurityMode::Accept);
                            renderer.write_line(
                                "security mode: accept (auto-allow within CWD)",
                                c_agent(),
                            )?;
                        } else {
                            renderer.write_line("permission system not active", c_error())?;
                        }
                    }
                    "yolo" => {
                        if let Some(p) = permission {
                            p.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .set_mode(SecurityMode::Yolo);
                            renderer.write_line(
                                "security mode: YOLO (all operations allowed)",
                                c_agent(),
                            )?;
                        } else {
                            renderer.write_line("permission system not active", c_error())?;
                        }
                    }
                    _ => {
                        renderer.write_line(&format!("unknown mode: {}", parts[1]), c_error())?;
                    }
                }
            }
        }
        #[cfg(feature = "mcp")]
        "/mcp" => {
            let Some(mgr) = mcp_manager else {
                renderer.write_line("no MCP servers configured", c_agent())?;
                return Ok(());
            };
            if mgr.handles.is_empty() {
                renderer.write_line("no MCP servers connected", c_agent())?;
            } else if parts.len() == 1 {
                renderer.write_line("MCP servers:", c_agent())?;
                for handle in &mgr.handles {
                    match handle.list_tools().await {
                        Ok(tools) => {
                            renderer.write_line(
                                &format!("  {} ({} tools)", handle.server_name, tools.len()),
                                c_result(),
                            )?;
                        }
                        Err(e) => {
                            renderer.write_line(
                                &format!("  {} (error: {})", handle.server_name, e),
                                c_error(),
                            )?;
                        }
                    }
                }
            } else {
                let name = parts[1].trim();
                if let Some(handle) = mgr.handles.iter().find(|h| h.server_name == name) {
                    match handle.list_tools().await {
                        Ok(tools) => {
                            if tools.is_empty() {
                                renderer.write_line(
                                    &format!("server '{}' has no tools", name),
                                    c_agent(),
                                )?;
                            } else {
                                renderer.write_line(&format!("tools on '{}':", name), c_agent())?;
                                for tool in &tools {
                                    let desc = tool.description.as_deref().unwrap_or("");
                                    renderer.write_line(
                                        &format!("  {}  {}", tool.name, desc),
                                        c_result(),
                                    )?;
                                }
                            }
                        }
                        Err(e) => {
                            renderer.write_line(
                                &format!("error listing tools on '{}': {}", name, e),
                                c_error(),
                            )?;
                        }
                    }
                } else {
                    renderer.write_line(&format!("unknown MCP server: '{}'", name), c_error())?;
                }
            }
        }
        "/toggle" => {
            if parts.len() < 2 {
                renderer.write_line("usage: /toggle <feature> [on|off]", c_agent())?;
                renderer.write_line("features:", c_agent())?;
                renderer.write_line(
                    &format!("  todo  {}", if *todo_tools_enabled { "on" } else { "off" }),
                    c_result(),
                )?;
            } else {
                let new_state = match parts.get(2).copied() {
                    Some("on") => true,
                    Some("off") => false,
                    Some(other) => {
                        renderer.write_line(
                            &format!("invalid: '{}', use on or off", other),
                            c_error(),
                        )?;
                        return Ok(());
                    }
                    None => !*todo_tools_enabled,
                };
                if new_state == *todo_tools_enabled {
                    renderer.write_line(
                        &format!(
                            "todo tools already {}",
                            if new_state { "on" } else { "off" }
                        ),
                        c_agent(),
                    )?;
                } else {
                    *todo_tools_enabled = new_state;
                    let model = client.completion_model(session.model.to_string());
                    *agent = crate::provider::build_agent(
                        model,
                        cli,
                        cfg,
                        context,
                        permission.clone(),
                        ask_tx.clone(),
                        None,
                        None,
                        bg_store.clone(),
                        #[cfg(feature = "lsp")]
                        None,
                        sandbox.clone(),
                        #[cfg(feature = "mcp")]
                        mcp_manager,
                        #[cfg(feature = "semantic")]
                        semantic_manager,
                    )
                    .await;
                    renderer.write_line(
                        &format!(
                            "todo tools: {}",
                            if *todo_tools_enabled { "on" } else { "off" }
                        ),
                        c_agent(),
                    )?;
                }
            }
        }
        "/compress" | "/compact" => {
            let instructions = if parts.len() > 1 {
                Some(parts[1..].join(" "))
            } else {
                None
            };
            let instr_str = instructions.clone().unwrap_or_default();
            return Err(anyhow::anyhow!("DEFER_COMPRESS:{}", instr_str));
        }
        "/loop" => {
            #[cfg(feature = "loop")]
            {
                if parts.len() < 2 || (parts.len() >= 2 && parts[1] == "status") {
                    if let Some(ls) = loop_state {
                        let status = if ls.active { "active" } else { "stopped" };
                        renderer.write_line(
                            &format!(
                                "loop {}: {} ({})",
                                status,
                                ls.iteration_label(),
                                ls.plan_file.display()
                            ),
                            c_agent(),
                        )?;
                    } else {
                        renderer.write_line("no active loop", c_agent())?;
                        renderer.write_line("usage: /loop <prompt>  |  /loop stop", c_result())?;
                    }
                } else if parts[1] == "stop" {
                    if let Some(ls) = loop_state {
                        ls.active = false;
                        renderer.write_line("loop stopped", c_agent())?;
                    } else {
                        renderer.write_line("no active loop", c_agent())?;
                    }
                } else {
                    let prompt = parts[1..].join(" ");
                    if prompt.is_empty() {
                        renderer.write_line("usage: /loop <prompt>", c_error())?;
                        return Ok(());
                    }
                    let plan_file = std::path::PathBuf::from("LOOP_PLAN.md");
                    let ls = crate::extras::r#loop::LoopState::new(prompt, plan_file, None, None);
                    *loop_state = Some(ls);
                    renderer.write_line(
                        "loop started — iteration 1 will run after this message",
                        c_agent(),
                    )?;
                }
            }
            #[cfg(not(feature = "loop"))]
            {
                renderer.write_line(
                    "/loop requires the 'loop' feature: cargo build --features loop",
                    c_error(),
                )?;
            }
        }
        "/prompt" => {
            let mut sorted: Vec<&String> = context.prompts.keys().collect();
            sorted.sort();
            if parts.len() < 2 {
                if sorted.is_empty() {
                    renderer.write_line("no prompts available", c_agent())?;
                } else {
                    let current = context.current_prompt_name.as_deref().unwrap_or("(none)");
                    renderer.write_line(
                        &format!("available prompts (current: {}):", current),
                        c_agent(),
                    )?;
                    for name in &sorted {
                        renderer.write_line(&format!("  {}", name), c_result())?;
                    }
                    renderer.write_line("", c_agent())?;
                    renderer.write_line("usage: /prompt <name>  |  /prompt default", c_result())?;
                }
            } else if parts[1] == "default" {
                if context.current_prompt.is_none() {
                    renderer.write_line("no active prompt to clear", c_agent())?;
                } else {
                    context.current_prompt = None;
                    context.current_prompt_name = None;
                    let model = client.completion_model(session.model.to_string());
                    *agent = crate::provider::build_agent(
                        model,
                        cli,
                        cfg,
                        context,
                        permission.clone(),
                        ask_tx.clone(),
                        None,
                        None,
                        bg_store.clone(),
                        #[cfg(feature = "lsp")]
                        None,
                        sandbox.clone(),
                        #[cfg(feature = "mcp")]
                        mcp_manager,
                        #[cfg(feature = "semantic")]
                        semantic_manager,
                    )
                    .await;
                }
            } else {
                let name = parts[1].trim();
                if let Some(content) = context.prompts.get(name) {
                    context.current_prompt = Some(content.clone());
                    context.current_prompt_name = Some(name.to_string());
                    let model = client.completion_model(session.model.to_string());
                    *agent = crate::provider::build_agent(
                        model,
                        cli,
                        cfg,
                        context,
                        permission.clone(),
                        ask_tx.clone(),
                        None,
                        None,
                        bg_store.clone(),
                        #[cfg(feature = "lsp")]
                        None,
                        sandbox.clone(),
                        #[cfg(feature = "mcp")]
                        mcp_manager,
                        #[cfg(feature = "semantic")]
                        semantic_manager,
                    )
                    .await;
                    renderer.write_line(&format!("active prompt: {}", name), c_agent())?;
                } else {
                    renderer.write_line(&format!("unknown prompt: '{}'", name), c_error())?;
                    if !sorted.is_empty() {
                        renderer.write_line("available prompts:", c_agent())?;
                        for p in &sorted {
                            renderer.write_line(&format!("  {}", p), c_result())?;
                        }
                    }
                }
            }
        }
        #[cfg(feature = "git-worktree")]
        "/worktree" => {
            if parts.len() < 2 {
                renderer.write_line("usage: /worktree <name>", c_error())?;
                return Ok(());
            }
            let name = parts[1].trim();
            if name.is_empty() || name.contains(' ') || name.contains('/') {
                renderer.write_line(
                    "invalid name: use a single word without spaces or slashes",
                    c_error(),
                )?;
                return Ok(());
            }

            match crate::extras::git_worktree::create(name) {
                Ok((path, _info)) => {
                    std::env::set_current_dir(&path)
                        .map_err(|e| anyhow::anyhow!("failed to change directory: {}", e))?;
                    session.working_dir = compact_str::CompactString::new(path.to_string_lossy());
                    context.reload();
                    let model = client.completion_model(session.model.to_string());
                    *agent = crate::provider::build_agent(
                        model,
                        cli,
                        cfg,
                        context,
                        permission.clone(),
                        ask_tx.clone(),
                        None,
                        None,
                        bg_store.clone(),
                        #[cfg(feature = "lsp")]
                        None,
                        sandbox.clone(),
                        #[cfg(feature = "mcp")]
                        mcp_manager,
                        #[cfg(feature = "semantic")]
                        semantic_manager,
                    )
                    .await;
                    render_session(renderer, session, cli, cfg, context)?;
                    renderer.write_line(
                        &format!("worktree created: branch '{}' at {}", name, path.display()),
                        c_agent(),
                    )?;
                }
                Err(e) => {
                    renderer.write_line(&format!("failed: {}", e), c_error())?;
                }
            }
        }
        #[cfg(feature = "git-worktree")]
        "/wt-merge" => {
            let info = match crate::extras::git_worktree::detect() {
                Some(i) => i,
                None => {
                    renderer.write_line("not in a git worktree", c_error())?;
                    return Ok(());
                }
            };
            let target = if parts.len() >= 2 {
                parts[1].trim().to_string()
            } else {
                match crate::extras::git_worktree::default_branch(&info.main_repo_path) {
                    Some(b) => b,
                    None => {
                        renderer.write_line(
                            "no target branch specified and couldn't detect main/master",
                            c_error(),
                        )?;
                        return Ok(());
                    }
                }
            };
            let repo_name = crate::extras::git_worktree::repo_name(&info.main_repo_path);
            let main_path = info.main_repo_path.display();
            let wt_path = info.worktree_path.display();
            renderer.write_line(
                &format!(
                    "merging '{}' into '{}' in {}...",
                    info.branch, target, repo_name
                ),
                c_agent(),
            )?;
            return Err(anyhow::anyhow!(
                "DEFER_WT_MERGE:{}:{}:{}:{}:{}",
                info.branch,
                target,
                main_path,
                wt_path,
                repo_name
            ));
        }
        #[cfg(feature = "git-worktree")]
        "/wt-exit" => {
            let info = match crate::extras::git_worktree::detect() {
                Some(i) => i,
                None => {
                    renderer.write_line("not in a git worktree", c_error())?;
                    return Ok(());
                }
            };
            let main_path = info.main_repo_path.display();
            renderer.write_line(
                &format!("returning to main repo at {}", main_path),
                c_agent(),
            )?;
            return Err(anyhow::anyhow!(
                "DEFER_WT_EXIT:{}:{}",
                main_path,
                info.worktree_path.display()
            ));
        }
        "/regen-prompts" => match crate::context::prompts::regen() {
            Ok(()) => {
                context.prompts = crate::context::prompts::load();
                renderer.write_line("default prompts regenerated", c_agent())?;
            }
            Err(e) => {
                renderer.write_line(&format!("failed to regenerate prompts: {}", e), c_error())?;
            }
        },
        "/quit" => {
            *is_running = false;
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "quit").into());
        }
        "/clear" => {
            session.messages.clear();
            session.total_estimated_tokens = 0;
            session.compactions.clear();
            // Drop branch state too so a fresh /clear truly starts over.
            // Without this, /tree would still show the cleared messages
            // as a dead-end branch.
            session.message_store.clear();
            session.tree.entries.clear();
            session.tree.leaf_id = None;
            crate::agent::tools::modified::clear_modified();
            render_session(renderer, session, cli, cfg, context)?;
        }
        "/tree" => {
            // No-arg: print an ASCII view of the tree.
            // <id-prefix>: switch the active branch to the leaf matching
            //              the given id prefix (no need to type full UUID).
            session.ensure_tree_initialized();
            session.ensure_message_store_initialized();
            let arg = parts.get(1).copied().unwrap_or("").trim();
            if arg.is_empty() {
                if session.tree.entries.is_empty() {
                    renderer.write_line("(empty session)", c_agent())?;
                } else {
                    for line in tree::render_tree(session) {
                        renderer.write_line(&line, c_result())?;
                    }
                }
            } else {
                match tree::resolve_id_prefix(session, arg) {
                    Ok(id) => {
                        if let Err(e) = session.switch_to_leaf(&id) {
                            renderer.write_line(&format!("switch failed: {}", e), c_error())?;
                        } else {
                            render_session(renderer, session, cli, cfg, context)?;
                            renderer.write_line(
                                &format!("switched to leaf {}", short_id(&id)),
                                c_agent(),
                            )?;
                        }
                    }
                    Err(e) => renderer.write_line(&format!("/tree: {}", e), c_error())?,
                }
            }
        }
        "/fork" => {
            // /fork [id-prefix] — branch off from the parent of the
            // chosen message, and pop the original prompt back into
            // the editor so the user can re-edit and retry.
            // Default target: the most recent user message on the
            // current branch (i.e. "redo last prompt").
            session.ensure_tree_initialized();
            session.ensure_message_store_initialized();
            let arg = parts.get(1).copied().unwrap_or("").trim();
            let target_id = if arg.is_empty() {
                // Default to the last User message on the current path.
                let last_user = session
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
                tree::resolve_id_prefix(session, arg)
            };
            match target_id {
                Ok(id) => match session.fork_at(&id) {
                    Ok(original) => {
                        input.set_text(&original.content);
                        render_session(renderer, session, cli, cfg, context)?;
                        renderer.write_line(
                            &format!(
                                "forked at {} — original prompt restored to editor",
                                short_id(&id)
                            ),
                            c_agent(),
                        )?;
                    }
                    Err(e) => renderer.write_line(&format!("/fork: {}", e), c_error())?,
                },
                Err(e) => renderer.write_line(&format!("/fork: {}", e), c_error())?,
            }
        }
        "/clone" => {
            // /clone <id-prefix> — make the chosen entry the leaf
            // without restoring its content into the editor. Useful
            // for jumping to a labeled bookmark or comparing branches.
            session.ensure_tree_initialized();
            session.ensure_message_store_initialized();
            let arg = parts.get(1).copied().unwrap_or("").trim();
            if arg.is_empty() {
                renderer.write_line("usage: /clone <id-prefix>", c_error())?;
            } else {
                match tree::resolve_id_prefix(session, arg) {
                    Ok(id) => match session.clone_at(&id) {
                        Ok(()) => {
                            render_session(renderer, session, cli, cfg, context)?;
                            renderer.write_line(
                                &format!("cloned path through {}", short_id(&id)),
                                c_agent(),
                            )?;
                        }
                        Err(e) => renderer.write_line(&format!("/clone: {}", e), c_error())?,
                    },
                    Err(e) => renderer.write_line(&format!("/clone: {}", e), c_error())?,
                }
            }
        }
        "/panel" => {
            use crate::ui::renderer::PanelMode;
            let arg = parts.get(1).copied().unwrap_or("").trim();
            let new_mode = match arg {
                "" => None,
                "on" => Some(PanelMode::On),
                "off" => Some(PanelMode::Off),
                "auto" => Some(PanelMode::Auto),
                other => {
                    renderer.write_line(
                        &format!("unknown /panel mode '{}' (use on|off|auto)", other),
                        c_error(),
                    )?;
                    return Ok(());
                }
            };
            if let Some(mode) = new_mode {
                renderer.set_panel_mode(mode);
                // Force a full repaint so layout / clipping recomputes at
                // the new width immediately, not on next event.
                renderer.render_viewport()?;
            }
            let current = renderer.panel_mode();
            let visible = renderer.panel_visible();
            renderer.write_line(
                &format!(
                    "panel mode: {:?} (currently {})",
                    current,
                    if visible { "shown" } else { "hidden" }
                ),
                c_agent(),
            )?;
        }
        "/btw" => {
            let query = parts.get(1..).map(|p| p.join(" ")).unwrap_or_default();
            if query.is_empty() {
                renderer.write_line("usage: /btw <question>", c_error())?;
            } else {
                let model = client.completion_model(session.model.to_string());
                renderer.write_line(&format!("btw: {}", query), Color::DarkGrey)?;
                match model.btw_query(query).await {
                    Ok(response) => {
                        renderer.write_line("", Color::White)?;
                        let max_width = renderer.line_width();
                        let styled = crate::ui::markdown::markdown_to_styled(&response, max_width);
                        for span in styled {
                            renderer.write(&span.text, span.color)?;
                        }
                        renderer.write_line("", Color::White)?;
                    }
                    Err(e) => {
                        renderer.write_line(&format!("btw error: {}", e), c_error())?;
                    }
                }
            }
        }
        "/cd" => {
            let target = parts.get(1).copied().unwrap_or("");
            let path = if target.is_empty() {
                dirs::home_dir().unwrap_or_default()
            } else if let Some(rest) = target.strip_prefix('~') {
                let mut home = dirs::home_dir().unwrap_or_default();
                home.push(rest.trim_start_matches('/'));
                home
            } else {
                std::path::PathBuf::from(target)
            };
            match std::env::set_current_dir(&path) {
                Ok(()) => {
                    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                    session.working_dir =
                        compact_str::CompactString::new(canonical.to_string_lossy());
                    if let Some(perm) = permission {
                        if let Ok(mut guard) = perm.lock() {
                            guard.set_working_dir(&session.working_dir);
                        }
                    }
                    context.reload();
                    let model = client.completion_model(session.model.to_string());
                    *agent = crate::provider::build_agent(
                        model,
                        cli,
                        cfg,
                        context,
                        permission.clone(),
                        ask_tx.clone(),
                        None,
                        None,
                        bg_store.clone(),
                        #[cfg(feature = "lsp")]
                        None,
                        sandbox.clone(),
                        #[cfg(feature = "mcp")]
                        mcp_manager,
                        #[cfg(feature = "semantic")]
                        semantic_manager,
                    )
                    .await;
                    render_session(renderer, session, cli, cfg, context)?;
                    renderer.write_line(
                        &format!("changed directory to {}", session.working_dir),
                        c_agent(),
                    )?;
                }
                Err(e) => {
                    renderer.write_line(&format!("cd: {}", e), c_error())?;
                }
            }
        }
        "/undo" => {
            let removed = undo_last(session);
            if removed > 0 {
                render_session(renderer, session, cli, cfg, context)?;
                renderer.write_line(&format!("removed {} message(s)", removed), c_agent())?;
            } else {
                renderer.write_line("nothing to undo", c_agent())?;
            }
        }
        "/retry" => {
            let last_user = session
                .messages
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::User)
                .cloned();
            match last_user {
                Some(msg) => {
                    // Pop the trailing assistant response (if any) so a
                    // /retry doesn't leave the failed reply in the
                    // session — the agent would otherwise see its own
                    // bad answer as context on the retry.
                    let _ = undo_last(session);
                    input.buffer = msg.content.clone();
                    input.cursor = msg.content.len();
                    render_session(renderer, session, cli, cfg, context)?;
                    renderer.write_line("edit last message and press Enter to retry", c_agent())?;
                }
                None => {
                    renderer.write_line("no previous message to retry", c_agent())?;
                }
            }
        }
        "/help" => {
            renderer.write_line("commands:", c_agent())?;
            renderer.write_line("  /model [name]          show or switch model", c_result())?;
            renderer.write_line("  /sessions              list recent sessions", c_result())?;
            renderer.write_line(
                "  /sessions <id>         load a session (by ID prefix)",
                c_result(),
            )?;
            renderer.write_line("  /sessions delete <id>  delete a session", c_result())?;
            renderer.write_line(
                "  /reasoning             toggle reasoning visibility",
                c_result(),
            )?;
            renderer.write_line(
                "  /mode                  show/change security mode",
                c_result(),
            )?;
            renderer.write_line(
                "  /mode <mode>           set mode (standard|restrictive|accept|yolo)",
                c_result(),
            )?;
            #[cfg(feature = "mcp")]
            {
                let _ = renderer.write_line(
                    "  /mcp                   list MCP servers and tools",
                    c_result(),
                );
                let _ = renderer.write_line(
                    "  /mcp <server>          list tools of an MCP server",
                    c_result(),
                );
            }
            renderer.write_line(
                "  /clear                 clear screen + reset tree",
                c_result(),
            )?;
            renderer.write_line(
                "  /tree                  show the session tree (use /tree <id-prefix> to switch branches)",
                c_result(),
            )?;
            renderer.write_line(
                "  /fork [id-prefix]      branch off at the chosen message (default: last user message)",
                c_result(),
            )?;
            renderer.write_line(
                "  /clone <id-prefix>     switch to the branch ending at the chosen entry",
                c_result(),
            )?;
            renderer.write_line(
                "  /panel [on|off|auto]   toggle right-hand info panel",
                c_result(),
            )?;
            renderer.write_line(
                "  /cd [path]             change working directory",
                c_result(),
            )?;
            renderer.write_line(
                "  /btw <question>        ask a quick question (no tools, doesn't affect session)",
                c_result(),
            )?;
            renderer.write_line("  /undo                  undo last exchange", c_result())?;
            renderer.write_line("  /retry                 retry last prompt", c_result())?;
            renderer.write_line(
                "  /compress [/compact]   compress conversation history",
                c_result(),
            )?;
            renderer.write_line(
                "  /compress [instr]      compress with custom instructions",
                c_result(),
            )?;
            #[cfg(feature = "loop")]
            {
                let _ = renderer.write_line(
                    "  /loop [prompt]         start iterative coding loop",
                    c_result(),
                );
                let _ = renderer.write_line("  /loop stop             stop the loop", c_result());
            }
            #[cfg(not(feature = "loop"))]
            {
                let _ = renderer.write_line(
                    "  /loop [prompt]         start iterative coding loop (req. 'loop' feature)",
                    c_result(),
                );
            }
            renderer.write_line(
                "  /prompt                list available prompts",
                c_result(),
            )?;
            renderer.write_line("  /prompt <name>         activate a prompt", c_result())?;
            renderer.write_line("  /prompt default        clear active prompt", c_result())?;
            renderer.write_line(
                "  /regen-prompts        restore built-in prompts to global dir",
                c_result(),
            )?;
            #[cfg(feature = "git-worktree")]
            {
                let _ = renderer.write_line(
                    "  /worktree <name>       create a git worktree on <name> branch and cd into it",
                    c_result(),
                );
                let _ = renderer.write_line(
                    "  /wt-merge [branch]     merge worktree branch into [branch] (default: main/master)",
                    c_result(),
                );
                let _ = renderer.write_line(
                    "  /wt-exit               exit worktree and return to main repo",
                    c_result(),
                );
            }
            renderer.write_line("  /quit                  exit dirge", c_result())?;
            renderer.write_line("  /help                  show this message", c_result())?;
            renderer.write_line("", c_agent())?;
            renderer.write_line("keys:", c_agent())?;
            renderer.write_line("  PgUp/PgDn             scroll chat history", c_result())?;
            renderer.write_line("  Home/End               jump to top/bottom", c_result())?;
            renderer.write_line(
                "  @<query>               file picker (Tab/Enter select, Esc cancel)",
                c_result(),
            )?;
            renderer.write_line(
                "  mouse drag             select text (copies to clipboard on release)",
                c_result(),
            )?;
            renderer.write_line(
                "  Esc (while selected)   clear selection (no copy)",
                c_result(),
            )?;
            renderer.write_line("  Ctrl+R                 toggle reasoning", c_result())?;
            renderer.write_line("  Ctrl+C / Ctrl+D        interrupt/quit", c_result())?;
            renderer.write_line(
                "  Ctrl+X                 drop last queued interjection",
                c_result(),
            )?;
            renderer.write_line(
                "  (type while agent runs to queue a follow-up message)",
                c_result(),
            )?;
            renderer.write_line("  mouse scroll           scroll chat", c_result())?;

            // Plugin-registered commands, if any. Listed last so they sit
            // visually after the built-ins and the keybindings.
            #[cfg(feature = "plugin")]
            if let Some(pm_arc) = crate::plugin::hook::global() {
                let cmds = {
                    let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
                    mgr.list_commands()
                };
                if !cmds.is_empty() {
                    renderer.write_line("", c_agent())?;
                    renderer.write_line("plugin commands:", c_agent())?;
                    for (cmd, handler) in cmds {
                        renderer
                            .write_line(&format!("  /{:<20} -> {}", cmd, handler), c_result())?;
                    }
                }
            }
        }
        _ => {
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
                            for line in text.lines() {
                                renderer.write_line(line, c_agent())?;
                            }
                        }
                        Ok(None) => {
                            // Handler ran cleanly but had nothing to say — no-op.
                        }
                        Err(e) => {
                            renderer.write_line(
                                &format!("[plugin] {} failed: {}", cmd, e),
                                c_error(),
                            )?;
                        }
                    }
                    return Ok(());
                }
            }
            renderer.write_line(
                &format!("unknown command: {} (try /help)", parts[0]),
                c_error(),
            )?;
        }
    }
    Ok(())
}
