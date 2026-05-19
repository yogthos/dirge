use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use std::time::Duration;

use crate::agent::tools::ToolError;
use crate::agent::tools::background::{BackgroundStore, TaskState};

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
            description: "Look up the state of a background task by id. You usually do NOT need this — completion notifications arrive automatically as a <system-reminder> on your next turn. Use task_status only when you need to re-check a task's status mid-turn or look up a task whose notification you've already consumed. Returns running / completed / failed plus the result text. Set wait=true to block until the task transitions out of running (rarely useful — prefer letting the notification arrive).".to_string(),
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
                        return Err(ToolError::Msg(format!("task not found: {}", args.task_id)));
                    }
                }
            }
        } else {
            match self.bg_store.get(&args.task_id) {
                Some(task) => match &task.state {
                    TaskState::Running => Ok(format!("task_id: {}\nstate: running", args.task_id)),
                    TaskState::Completed(text) => Ok(format!(
                        "task_id: {}\nstate: completed\n\n{}",
                        args.task_id, text
                    )),
                    TaskState::Failed(err) => Ok(format!(
                        "task_id: {}\nstate: failed\n\nerror: {}",
                        args.task_id, err
                    )),
                },
                None => Err(ToolError::Msg(format!("task not found: {}", args.task_id))),
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
        store.notify("test-task", TaskState::Completed("result text".to_string()));
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
        store.notify("test-task", TaskState::Failed("error message".to_string()));
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
            store_clone.notify("test-task", TaskState::Completed("done".to_string()));
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

    // Regression: the description must steer the agent away from polling
    // (notifications now arrive automatically). A future "improvement" that
    // re-introduces "poll until completion" language would silently regress
    // the push-notification UX.
    #[tokio::test]
    async fn definition_discourages_polling() {
        let store = BackgroundStore::new();
        let tool = TaskStatusTool::new(store);
        let def = tool.definition(String::new()).await;
        let desc = def.description.to_lowercase();
        assert!(
            desc.contains("system-reminder") || desc.contains("automatically"),
            "task_status description must reference automatic notification: {}",
            def.description
        );
        assert!(
            desc.contains("do not") || desc.contains("usually") || desc.contains("rarely"),
            "task_status description must discourage routine polling: {}",
            def.description
        );
    }

    // task_status is read-only: repeated lookups return the same payload.
    // Completed tasks are evicted by the store's LRU cap, not by reads, so
    // an agent can re-fetch its own results idempotently. Notification
    // delivery (Phase 3) happens out-of-band via drain_notifications().
    #[tokio::test]
    async fn status_lookup_is_idempotent() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("payload".into()));

        let tool = TaskStatusTool::new(store);
        for _ in 0..3 {
            let result = tool
                .call(TaskStatusArgs {
                    task_id: "t1".into(),
                    wait: None,
                })
                .await
                .unwrap();
            assert!(result.contains("state: completed"));
            assert!(result.contains("payload"));
        }
    }

    // wait=true must also return on failure (not just on completion), and the
    // error text must be surfaced.
    #[tokio::test]
    async fn wait_returns_on_failure() {
        let store = BackgroundStore::new();
        store.insert("t1".into());

        let store_clone = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            store_clone.notify("t1", TaskState::Failed("kaboom".into()));
        });

        let tool = TaskStatusTool::new(store);
        let result = tool
            .call(TaskStatusArgs {
                task_id: "t1".into(),
                wait: Some(true),
            })
            .await
            .unwrap();
        assert!(result.contains("state: failed"));
        assert!(result.contains("kaboom"));
    }

    // wait=true must surface a not-found error promptly rather than loop on
    // an absent task.
    #[tokio::test]
    async fn wait_on_missing_task_errors_promptly() {
        let store = BackgroundStore::new();
        let tool = TaskStatusTool::new(store);

        // Bound the call so a regression to infinite-loop fails the test.
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            tool.call(TaskStatusArgs {
                task_id: "never-existed".into(),
                wait: Some(true),
            }),
        )
        .await
        .expect("must not loop on missing task");

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}
