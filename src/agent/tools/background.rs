use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Thread-safe store for background subagent tasks.
/// Completed/failed tasks are removed on read to avoid unbounded growth.
#[derive(Debug, Clone, Default)]
pub struct BackgroundStore(Arc<Mutex<HashMap<String, BackgroundTask>>>);

const MAX_TASK_OUTPUT_CHARS: usize = 3000;

#[derive(Debug, Clone)]
pub enum TaskState {
    Running,
    Completed(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct BackgroundTask {
    pub state: TaskState,
}

impl BackgroundStore {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    pub fn insert(&self, id: String) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id,
            BackgroundTask {
                state: TaskState::Running,
            },
        );
    }

    /// Get current task state. Completed/failed tasks are removed on read
    /// so the store doesn't grow unbounded.
    pub fn get(&self, id: &str) -> Option<BackgroundTask> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let task = map.get(id).cloned();
        // Remove completed/failed tasks to prevent unbounded growth
        if let Some(ref t) = task {
            if !matches!(t.state, TaskState::Running) {
                map.remove(id);
            }
        }
        task
    }

    /// Update task state (called by the spawned subagent).
    /// Truncates output to MAX_TASK_OUTPUT_CHARS to avoid context bloat.
    pub fn update(&self, id: &str, state: TaskState) {
        if let Some(task) = self.0.lock().unwrap_or_else(|e| e.into_inner()).get_mut(id) {
            let truncated = match state {
                TaskState::Completed(text) => {
                    let t: String = text.chars().take(MAX_TASK_OUTPUT_CHARS).collect();
                    TaskState::Completed(t)
                }
                TaskState::Failed(err) => {
                    let e: String = err.chars().take(MAX_TASK_OUTPUT_CHARS).collect();
                    TaskState::Failed(e)
                }
                s => s,
            };
            task.state = truncated;
        }
    }
}
