use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use std::time::Duration;

use crate::agent::tools::background::{BackgroundStore, TaskState};
use crate::agent::tools::ToolError;

pub struct TaskStatusTool {
    bg_store: BackgroundStore,
}

impl TaskStatusTool {
    pub fn new(bg_store: BackgroundStore) -> Self {
        Self { bg_store }
    }
}

#[derive(Deserialize)]
pub struct TaskStatusArgs {
    pub task_id: String,
    #[serde(default)]
    pub wait: Option<bool>,
}

impl Tool for TaskStatusTool {
    const NAME: &'static str = "task_status";

    type Error = ToolError;
    type Args = TaskStatusArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "task_status".to_string(),
            description: "Check the status of a background task. Returns the task state (running/completed/failed) and, if completed, the result. Set wait=true to block until the task finishes.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID returned by the task tool with background=true"
                    },
                    "wait": {
                        "type": "boolean",
                        "description": "Block until the task completes (default: false)"
                    }
                },
                "required": ["task_id"]
            }),
        }
    }

    async fn call(&self, args: TaskStatusArgs) -> Result<String, ToolError> {
        let wait = args.wait.unwrap_or(false);

        if wait {
            loop {
                let state = self.bg_store.get(&args.task_id);
                match state {
                    Some(task) => match &task.state {
                        TaskState::Running => {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            continue;
                        }
                        TaskState::Completed(text) => {
                            return Ok(format!(
                                "task_id: {}\nstate: completed\n\n{}",
                                args.task_id, text
                            ));
                        }
                        TaskState::Failed(err) => {
                            return Ok(format!(
                                "task_id: {}\nstate: failed\n\nerror: {}",
                                args.task_id, err
                            ));
                        }
                    },
                    None => {
                        return Err(ToolError::Msg(format!(
                            "task not found: {}",
                            args.task_id
                        )));
                    }
                }
            }
        } else {
            match self.bg_store.get(&args.task_id) {
                Some(task) => match &task.state {
                    TaskState::Running => Ok(format!(
                        "task_id: {}\nstate: running",
                        args.task_id
                    )),
                    TaskState::Completed(text) => Ok(format!(
                        "task_id: {}\nstate: completed\n\n{}",
                        args.task_id, text
                    )),
                    TaskState::Failed(err) => Ok(format!(
                        "task_id: {}\nstate: failed\n\nerror: {}",
                        args.task_id, err
                    )),
                },
                None => Err(ToolError::Msg(format!(
                    "task not found: {}",
                    args.task_id
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_task_status_not_found() {
        let store = BackgroundStore::new();
        let tool = TaskStatusTool::new(store);
        let result = tool
            .call(TaskStatusArgs {
                task_id: "nonexistent".to_string(),
                wait: None,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_task_status_running() {
        let store = BackgroundStore::new();
        store.insert("test-task".to_string());
        let tool = TaskStatusTool::new(store);
        let result = tool
            .call(TaskStatusArgs {
                task_id: "test-task".to_string(),
                wait: None,
            })
            .await
            .unwrap();
        assert!(result.contains("state: running"));
    }

    #[tokio::test]
    async fn test_task_status_completed() {
        let store = BackgroundStore::new();
        store.insert("test-task".to_string());
        store.update("test-task", TaskState::Completed("result text".to_string()));
        let tool = TaskStatusTool::new(store);
        let result = tool
            .call(TaskStatusArgs {
                task_id: "test-task".to_string(),
                wait: None,
            })
            .await
            .unwrap();
        assert!(result.contains("state: completed"));
        assert!(result.contains("result text"));
    }

    #[tokio::test]
    async fn test_task_status_failed() {
        let store = BackgroundStore::new();
        store.insert("test-task".to_string());
        store.update("test-task", TaskState::Failed("error message".to_string()));
        let tool = TaskStatusTool::new(store);
        let result = tool
            .call(TaskStatusArgs {
                task_id: "test-task".to_string(),
                wait: None,
            })
            .await
            .unwrap();
        assert!(result.contains("state: failed"));
        assert!(result.contains("error message"));
    }

    #[tokio::test]
    async fn test_task_status_wait_completed() {
        let store = BackgroundStore::new();
        store.insert("test-task".to_string());

        // Update to completed after a short delay
        let store_clone = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            store_clone.update("test-task", TaskState::Completed("done".to_string()));
        });

        let tool = TaskStatusTool::new(store);
        let result = tool
            .call(TaskStatusArgs {
                task_id: "test-task".to_string(),
                wait: Some(true),
            })
            .await
            .unwrap();
        assert!(result.contains("state: completed"));
        assert!(result.contains("done"));
    }

    #[tokio::test]
    async fn test_definition_has_correct_name() {
        let store = BackgroundStore::new();
        let tool = TaskStatusTool::new(store);
        let def = tool.definition(String::new()).await;
        assert_eq!(def.name, "task_status");
    }
}
