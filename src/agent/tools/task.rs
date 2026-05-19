use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use uuid::Uuid;

use crate::agent::tools::background::{BackgroundStore, TaskState};
use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::provider::AnyModel;

pub struct TaskTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    model: AnyModel,
    bg_store: BackgroundStore,
}

impl TaskTool {
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        model: AnyModel,
        bg_store: BackgroundStore,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            model,
            bg_store,
        }
    }
}

#[derive(Deserialize)]
pub struct Args {
    pub prompt: String,
    #[serde(default)]
    pub background: Option<bool>,
}

impl Tool for TaskTool {
    const NAME: &'static str = "task";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        let description = "Spawn a subagent to handle a specific subtask. The subagent runs as a one-shot query (no tools) and returns its result inline. Use for research, analysis, or planning subtasks that don't require file access. Set background=true to run asynchronously — use task_status to poll for the result."
            .to_string();

        let properties = serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Task description for the subagent"
                },
                "background": {
                    "type": "boolean",
                    "description": "Run asynchronously (default: false). When true, returns a task_id immediately for use with task_status."
                }
            },
            "required": ["prompt"]
        });

        ToolDefinition {
            name: "task".to_string(),
            description,
            parameters: properties,
        }
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "task", &args.prompt).await?;

        let background = args.background.unwrap_or(false);

        if background {
            let task_id = Uuid::new_v4().to_string();
            self.bg_store.insert(task_id.clone());

            let model = self.model.clone();
            let prompt = args.prompt;
            let store = self.bg_store.clone();
            let tid = task_id.clone();

            tokio::spawn(async move {
                let result = model
                    .btw_query(format!(
                        "You are a subagent working on a specific subtask. Complete it thoroughly.\n\nTask: {}",
                        prompt
                    ))
                    .await;
                store.notify(
                    &tid,
                    match result {
                        Ok(text) => TaskState::Completed(text),
                        Err(e) => TaskState::Failed(e.to_string()),
                    },
                );
            });

            Ok(format!(
                "background task started\n\ntask_id: {}\nstate: running\n\nUse task_status to check progress.",
                task_id
            ))
        } else {
            let result = self
                .model
                .btw_query(format!(
                    "You are a subagent working on a specific subtask. Complete it thoroughly.\n\nTask: {}",
                    args.prompt
                ))
                .await
                .map_err(|e| ToolError::Msg(format!("Subagent error: {}", e)))?;

            Ok(result)
        }
    }
}
