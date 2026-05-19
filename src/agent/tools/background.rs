use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Thread-safe store for background subagent tasks.
#[derive(Debug, Clone, Default)]
pub struct BackgroundStore(Arc<Mutex<HashMap<String, BackgroundTask>>>);

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
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, BackgroundTask { state: TaskState::Running });
    }

    pub fn get(&self, id: &str) -> Option<BackgroundTask> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    pub fn update(&self, id: &str, state: TaskState) {
        if let Some(task) = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
        {
            task.state = state;
        }
    }
}
