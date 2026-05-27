//! Git worktree slash commands: /worktree, /wt-merge, /wt-exit.
//!
//! `/wt-merge` and `/wt-exit` defer their work via anyhow sentinel
//! errors (`DEFER_WT_MERGE:` / `DEFER_WT_EXIT:`) that the outer
//! event loop in `ui/mod.rs` parses — preserve those return paths
//! exactly.

use super::{SlashCtx, c_agent, c_error};
use crate::ui::events::render_session;

pub(super) async fn cmd_worktree(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        ctx.renderer.write_line("usage: /worktree <name>", c_error())?;
        return Ok(());
    }
    let name = parts[1].trim();
    let invalid = name.is_empty()
        || name.contains(' ')
        || name.contains('/')
        || name.starts_with('-')
        || name.contains("..")
        || name == "HEAD"
        || name == "@"
        || name.chars().any(|c| {
            c == '\0'
                || c == '~'
                || c == ':'
                || c == '^'
                || c == '?'
                || c == '*'
                || c == '['
                || (c.is_control() && c != '\t')
        });
    if invalid {
        ctx.renderer.write_line(
            "invalid name: use a single word without spaces, slashes, leading '-', '..', or git ref metacharacters (~ : ^ ? * [) — and not 'HEAD' or '@'",
            c_error(),
        )?;
        return Ok(());
    }
    match crate::extras::git_worktree::create(name) {
        Ok((path, _info)) => {
            std::env::set_current_dir(&path)
                .map_err(|e| anyhow::anyhow!("failed to change directory: {}", e))?;
            ctx.session.working_dir = compact_str::CompactString::new(path.to_string_lossy());
            ctx.context.reload();
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
            )
            .await;
            render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
            ctx.renderer.write_line(
                &format!("worktree created: branch '{}' at {}", name, path.display()),
                c_agent(),
            )?;
        }
        Err(e) => {
            ctx.renderer
                .write_line(&format!("failed: {}", e), c_error())?;
        }
    }
    Ok(())
}

pub(super) async fn cmd_wt_merge(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let info = match crate::extras::git_worktree::detect() {
        Some(i) => i,
        None => {
            ctx.renderer
                .write_line("not in a git worktree", c_error())?;
            return Ok(());
        }
    };
    let target = if parts.len() >= 2 {
        parts[1].trim().to_string()
    } else {
        match crate::extras::git_worktree::default_branch(&info.main_repo_path) {
            Some(b) => b,
            None => {
                ctx.renderer.write_line(
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
    ctx.renderer.write_line(
        &format!(
            "merging '{}' into '{}' in {}...",
            info.branch, target, repo_name
        ),
        c_agent(),
    )?;
    Err(anyhow::anyhow!(
        "DEFER_WT_MERGE:{}:{}:{}:{}:{}",
        info.branch,
        target,
        main_path,
        wt_path,
        repo_name
    ))
}

pub(super) async fn cmd_wt_exit(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let info = match crate::extras::git_worktree::detect() {
        Some(i) => i,
        None => {
            ctx.renderer
                .write_line("not in a git worktree", c_error())?;
            return Ok(());
        }
    };
    let force = parts.iter().skip(1).any(|p| *p == "--force" || *p == "-f");
    if !force {
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .output();
        match status {
            Ok(out) if out.status.success() && !out.stdout.is_empty() => {
                let dirty = String::from_utf8_lossy(&out.stdout);
                let line_count = dirty.lines().count();
                ctx.renderer.write_line(
                    &format!(
                        "worktree is dirty ({} uncommitted change{}); refusing to exit. Commit/stash first, or run `/wt-exit --force` to leave it stranded.",
                        line_count,
                        if line_count == 1 { "" } else { "s" },
                    ),
                    c_error(),
                )?;
                return Ok(());
            }
            Ok(_) => {} // clean tree
            Err(e) => {
                ctx.renderer.write_line(
                    &format!(
                        "could not run `git status` to check worktree state ({}); refusing to exit. Pass `--force` to override.",
                        e
                    ),
                    c_error(),
                )?;
                return Ok(());
            }
        }
    }
    let main_path = info.main_repo_path.display();
    ctx.renderer.write_line(
        &format!("returning to main repo at {}", main_path),
        c_agent(),
    )?;
    Err(anyhow::anyhow!(
        "DEFER_WT_EXIT:{}:{}",
        main_path,
        info.worktree_path.display()
    ))
}
