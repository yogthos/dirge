use compact_str::CompactString;
use rig::agent::{Agent, AgentBuilder};
use rig::completion::CompletionModel;
use rig::providers::openrouter;
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::prompt::{SYSTEM_PROMPT, TODO_TOOLS_PROMPT};
use crate::agent::tools;
use crate::agent::tools::ToolCache;
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
            )),
            Box::new(tools::WriteTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                plan_file.clone(),
                cache.clone(),
            )),
            Box::new(tools::EditTool::with_cache(
                permission.clone(),
                ask_tx.clone(),
                plan_file.clone(),
                cache.clone(),
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
        ];

        #[allow(unused_mut)]
        let mut builder = builder.tools(base_tools);

        if let Some(pm) = parent_model {
            let task_tool = Box::new(tools::TaskTool::new(permission.clone(), ask_tx.clone(), pm));
            builder = builder.tools(vec![task_tool]);
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
