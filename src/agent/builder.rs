use compact_str::CompactString;
use rig::agent::{Agent, AgentBuilder};
use rig::completion::CompletionModel;
use rig::providers::openrouter;
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::prompt::{SYSTEM_PROMPT, TODO_TOOLS_PROMPT};
use crate::agent::tools;
use crate::agent::tools::ToolCache;
use crate::agent::tools::background::BackgroundStore;
use crate::agent::tools::plan::PlanSwitchSender;
use crate::agent::tools::question::QuestionSender;
use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::AnyModel;
use crate::sandbox::Sandbox;
#[cfg(feature = "semantic")]
use crate::semantic::SemanticManager;
use crate::skill::{self, Skill};

#[allow(dead_code)]
pub type ZAgent = Agent<openrouter::CompletionModel>;

pub async fn build_agent_inner<M: CompletionModel + 'static>(
    model: M,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    question_tx: Option<QuestionSender>,
    plan_tx: Option<PlanSwitchSender>,
    bg_store: Option<BackgroundStore>,
    sandbox: Sandbox,
    parent_model: Option<AnyModel>,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "semantic")] semantic_manager: Option<&SemanticManager>,
) -> (Agent<M>, ToolCache) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let skills: Arc<[Skill]> = Arc::from(
        tokio::task::spawn_blocking(move || skill::discover_skills(&cwd))
            .await
            .unwrap_or_default(),
    );

    let plan_prompts: &[&str] = &["plan", "review", "review-security"];
    let plan_file: Option<PathBuf> = context
        .current_prompt_name
        .as_deref()
        .filter(|name| plan_prompts.contains(name))
        .map(|_| {
            std::env::current_dir()
                .unwrap_or_else(|_| ".".into())
                .join("PLAN.md")
        });
    let mut preamble = SYSTEM_PROMPT.to_string();
    preamble.push('\n');
    preamble.push_str(TODO_TOOLS_PROMPT);
    if let Some(agents) = &context.agents {
        preamble.push_str("\n\n");
        preamble.push_str(agents);
    }

    if let Some(prompt) = &context.current_prompt {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(prompt);
    }

    if let Ok(cwd) = std::env::current_dir() {
        let cwd_str = cwd.display();
        preamble.push_str(&format!("\n\nCurrent working directory: {}", cwd_str));
    }

    preamble.push_str(&format!("\nOS: {}", std::env::consts::OS));

    if let Ok(shell) = std::env::var("SHELL") {
        preamble.push_str(&format!("\nShell: {}", shell));
    }

    let git_branch = tokio::task::spawn_blocking(|| {
        std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !branch.is_empty() {
                        Some(branch)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
    })
    .await
    .unwrap_or(None);

    if let Some(branch) = git_branch {
        preamble.push_str(&format!("\nGit branch: {}", branch));
    }

    // Inject mode-specific reminders
    if let Some(prompt_name) = &context.current_prompt_name {
        let plan_exists = std::env::current_dir()
            .unwrap_or_else(|_| ".".into())
            .join("PLAN.md")
            .exists();
        append_mode_reminder(&mut preamble, prompt_name, plan_exists);
    }

    let mut builder = AgentBuilder::new(model).preamble(&preamble);

    let max_tokens = cli.resolve_max_tokens(cfg);
    builder = builder.max_tokens(max_tokens);

    let max_turns = cli.resolve_max_agent_turns(cfg);
    builder = builder.default_max_turns(max_turns);

    if let Some(temp) = cli.temperature {
        let clamped = temp.clamp(0.0, 2.0);
        builder = builder.temperature(clamped);
    }

    if cli.resolve_no_tools(cfg) {
        (builder.build(), ToolCache::new())
    } else {
        let cache = ToolCache::new();

        let base_tools: Vec<Box<dyn rig::tool::ToolDyn>> = vec![
            Box::new(tools::ReadTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
                // Phase 7 will populate this from build_channels.
                None,
            )),
            Box::new(tools::WriteTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                plan_file.clone(),
                cache.clone(),
                None,
            )),
            Box::new(tools::EditTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                plan_file.clone(),
                cache.clone(),
                None,
            )),
            Box::new(tools::BashTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                sandbox.clone(),
                cache.clone(),
            )),
            Box::new(tools::GrepTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
            )),
            Box::new(tools::FindFilesTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
            )),
            Box::new(tools::GlobTool::new(permission.clone(), ask_tx.clone())),
            Box::new(tools::ListDirTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                cache.clone(),
            )),
            Box::new(tools::WriteTodoList::new(
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(tools::SkillTool::new(
                Arc::clone(&skills),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(tools::MemoryTool::new(permission.clone(), ask_tx.clone())),
            Box::new(tools::ApplyPatchTool::new(
                permission.clone(),
                ask_tx.clone(),
            )),
        ];

        let question_tool = question_tx
            .map(|tx| Box::new(tools::QuestionTool::new(tx)) as Box<dyn rig::tool::ToolDyn>);

        let plan_tools = plan_tx.map(|tx| {
            let enter =
                Box::new(tools::PlanEnterTool::new(tx.clone())) as Box<dyn rig::tool::ToolDyn>;
            let exit = Box::new(tools::PlanExitTool::new(tx)) as Box<dyn rig::tool::ToolDyn>;
            vec![enter, exit]
        });

        // Web tools: gated on config + env var
        let websearch_enabled = cfg
            .tools
            .as_ref()
            .and_then(|t| t.websearch)
            .unwrap_or(false)
            || std::env::var("WEBSEARCH_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false);
        let webfetch_enabled = cfg.tools.as_ref().and_then(|t| t.webfetch).unwrap_or(false);

        let websearch_tool = websearch_enabled
            .then(|| std::env::var("EXA_API_KEY").ok())
            .flatten()
            .map(|key| {
                Box::new(tools::WebSearchTool::new(
                    permission.clone(),
                    ask_tx.clone(),
                    key,
                )) as Box<dyn rig::tool::ToolDyn>
            });
        let webfetch_tool = webfetch_enabled.then(|| {
            Box::new(tools::WebFetchTool::new(permission.clone(), ask_tx.clone()))
                as Box<dyn rig::tool::ToolDyn>
        });

        #[allow(unused_mut)]
        let mut builder = builder.tools(base_tools);

        if let Some(qt) = question_tool {
            builder = builder.tools(vec![qt]);
        }

        if let Some(pt) = plan_tools {
            builder = builder.tools(pt);
        }

        if let Some(ws) = websearch_tool {
            builder = builder.tools(vec![ws]);
        }

        if let Some(wf) = webfetch_tool {
            builder = builder.tools(vec![wf]);
        }

        if let (Some(pm), Some(store)) = (parent_model, bg_store) {
            let task_tool = Box::new(tools::TaskTool::new(
                permission.clone(),
                ask_tx.clone(),
                pm,
                store.clone(),
            ));
            let status_tool =
                Box::new(tools::TaskStatusTool::new(store)) as Box<dyn rig::tool::ToolDyn>;
            builder = builder.tools(vec![task_tool, status_tool]);
        }

        #[cfg(feature = "mcp")]
        if let Some(manager) = &mcp_manager {
            let mcp_tools = manager
                .collect_tools(permission.clone(), ask_tx.clone())
                .await;
            if !mcp_tools.is_empty() {
                let dyn_tools: Vec<Box<dyn rig::tool::ToolDyn>> = mcp_tools
                    .into_iter()
                    .map(|t| Box::new(t) as Box<dyn rig::tool::ToolDyn>)
                    .collect();
                builder = builder.tools(dyn_tools);
            }
        }

        #[cfg(feature = "semantic")]
        if let Some(manager) = &semantic_manager {
            let sem_tools = manager.tools(permission.clone(), ask_tx.clone());
            if !sem_tools.is_empty() {
                builder = builder.tools(sem_tools);
            }
        }

        (builder.build(), cache)
    }
}

#[allow(dead_code)]
pub fn create_client(api_key: Option<&str>) -> anyhow::Result<openrouter::Client> {
    let key = api_key
        .map(CompactString::new)
        .or_else(|| {
            std::env::var("OPENROUTER_API_KEY")
                .ok()
                .map(CompactString::new)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No API key found. Set OPENROUTER_API_KEY environment variable or pass --api-key."
            )
        })?;
    Ok(openrouter::Client::new(String::from(key))?)
}

/// Append a mode-specific reminder to `preamble` based on the active prompt
/// name. `plan_exists` reports whether `PLAN.md` is present in CWD — only
/// consulted for the `code` mode reminder. Unknown prompt names produce no
/// reminder so custom prompts don't accidentally pick up plan/review semantics.
pub(crate) fn append_mode_reminder(preamble: &mut String, prompt_name: &str, plan_exists: bool) {
    match prompt_name {
        "plan" => {
            preamble.push_str("\n\n---\n\nYou are now in PLAN mode. Create a detailed implementation plan. Save it to PLAN.md in the current directory. Analyze the task, break it into concrete steps, consider edge cases and trade-offs. Do NOT write any code or run any commands until the user reviews and approves the plan.");
        }
        "review" | "review-security" => {
            preamble.push_str("\n\n---\n\nYou are now in REVIEW mode. Review the code or plan carefully. Identify bugs, security issues, performance problems, and design flaws. Be thorough and specific. Provide actionable feedback.");
        }
        "code" if plan_exists => {
            preamble.push_str(
                "\n\n---\n\nA plan file exists at PLAN.md. Execute the plan step by step. Write and test code following the plan. Report progress after each step. The plan is your guide — follow it closely.",
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod reminder_tests {
    use super::append_mode_reminder;

    #[test]
    fn plan_mode_injects_plan_reminder() {
        let mut p = String::from("base");
        append_mode_reminder(&mut p, "plan", false);
        assert!(p.contains("PLAN mode"));
        assert!(p.contains("PLAN.md"));
        assert!(p.contains("Do NOT write any code"));
    }

    #[test]
    fn review_modes_inject_review_reminder() {
        for mode in &["review", "review-security"] {
            let mut p = String::from("base");
            append_mode_reminder(&mut p, mode, false);
            assert!(p.contains("REVIEW mode"), "mode={mode}");
            assert!(p.contains("Identify bugs"), "mode={mode}");
        }
    }

    // Regression: the `code` reminder must only appear when PLAN.md exists.
    // Without that guard every code-mode session would have a stale "execute
    // the plan" instruction even with no plan written.
    #[test]
    fn regression_code_mode_reminder_requires_plan_md() {
        let mut p_with = String::from("base");
        append_mode_reminder(&mut p_with, "code", true);
        assert!(p_with.contains("plan file exists"));

        let mut p_without = String::from("base");
        append_mode_reminder(&mut p_without, "code", false);
        assert_eq!(p_without, "base", "no reminder must be added");
    }

    // Unknown prompts (custom user prompts) must produce no reminder so the
    // plan/review semantics don't bleed into other modes.
    #[test]
    fn unknown_prompt_name_appends_nothing() {
        let mut p = String::from("base");
        append_mode_reminder(&mut p, "my-custom-prompt", true);
        assert_eq!(p, "base");
    }

    // Each reminder is prefixed by the section separator so it visually
    // detaches from the prior prompt — regression-guards the leading "\n\n---".
    #[test]
    fn reminders_use_section_separator() {
        let mut p = String::new();
        append_mode_reminder(&mut p, "plan", false);
        assert!(p.starts_with("\n\n---\n\n"), "got: {p:?}");
    }
}
