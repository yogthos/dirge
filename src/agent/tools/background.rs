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

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_get_returns_running() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        let task = store.get("t1").expect("task present");
        assert!(matches!(task.state, TaskState::Running));
    }

    #[test]
    fn get_on_missing_returns_none() {
        let store = BackgroundStore::new();
        assert!(store.get("nope").is_none());
    }

    // Regression: previously the store grew unbounded across a session because
    // completed/failed tasks were never removed. get() now evicts on read.
    #[test]
    fn regression_get_removes_completed_task() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.update("t1", TaskState::Completed("done".into()));
        assert_eq!(store.len(), 1);

        let first = store.get("t1");
        assert!(matches!(
            first.unwrap().state,
            TaskState::Completed(ref s) if s == "done"
        ));

        // The first read evicts.
        assert_eq!(store.len(), 0);
        assert!(store.get("t1").is_none());
    }

    #[test]
    fn regression_get_removes_failed_task() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.update("t1", TaskState::Failed("boom".into()));

        let first = store.get("t1");
        assert!(matches!(
            first.unwrap().state,
            TaskState::Failed(ref s) if s == "boom"
        ));
        assert_eq!(store.len(), 0);
    }

    // Regression: get() must NOT evict tasks still running, otherwise polling
    // wait=true would lose the task before completion.
    #[test]
    fn regression_get_keeps_running_task() {
        let store = BackgroundStore::new();
        store.insert("t1".into());

        for _ in 0..5 {
            let task = store.get("t1").expect("still present while running");
            assert!(matches!(task.state, TaskState::Running));
        }
        assert_eq!(store.len(), 1);
    }

    // Regression: subagent output was injected verbatim into parent context
    // and blew the window. update() truncates to MAX_TASK_OUTPUT_CHARS.
    #[test]
    fn regression_update_truncates_completed_text() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        let huge = "x".repeat(MAX_TASK_OUTPUT_CHARS * 2);
        store.update("t1", TaskState::Completed(huge));

        let task = store.get("t1").unwrap();
        let TaskState::Completed(text) = task.state else {
            panic!("expected Completed");
        };
        assert_eq!(text.chars().count(), MAX_TASK_OUTPUT_CHARS);
    }

    #[test]
    fn regression_update_truncates_failed_error() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        let huge = "e".repeat(MAX_TASK_OUTPUT_CHARS * 2);
        store.update("t1", TaskState::Failed(huge));

        let task = store.get("t1").unwrap();
        let TaskState::Failed(text) = task.state else {
            panic!("expected Failed");
        };
        assert_eq!(text.chars().count(), MAX_TASK_OUTPUT_CHARS);
    }

    #[test]
    fn update_leaves_short_text_intact() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.update("t1", TaskState::Completed("hello".into()));
        let TaskState::Completed(text) = store.get("t1").unwrap().state else {
            panic!("expected Completed");
        };
        assert_eq!(text, "hello");
    }

    // Truncation uses chars().take(), so multibyte characters count as one each
    // — guards against accidentally switching to bytes-based truncation that
    // would split UTF-8 sequences.
    #[test]
    fn update_truncates_by_chars_not_bytes() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        // Each emoji is 4 bytes; producing MAX*2 chars = MAX*8 bytes.
        let emojis = "🦀".repeat(MAX_TASK_OUTPUT_CHARS * 2);
        store.update("t1", TaskState::Completed(emojis));
        let TaskState::Completed(text) = store.get("t1").unwrap().state else {
            panic!("expected Completed");
        };
        assert_eq!(text.chars().count(), MAX_TASK_OUTPUT_CHARS);
        // Verify no broken UTF-8: re-encoding round-trips.
        assert_eq!(text.as_str(), &"🦀".repeat(MAX_TASK_OUTPUT_CHARS));
    }

    #[test]
    fn update_on_missing_is_noop() {
        let store = BackgroundStore::new();
        store.update("ghost", TaskState::Completed("never inserted".into()));
        assert!(store.get("ghost").is_none());
        assert_eq!(store.len(), 0);
    }

    // The store is Clone + thread-safe (Arc<Mutex<...>>). Clones must see each
    // other's writes — guards against accidentally cloning the inner HashMap.
    #[test]
    fn clones_share_state() {
        let a = BackgroundStore::new();
        let b = a.clone();

        a.insert("t1".into());
        assert!(b.get("t1").is_some());
        // get() on `b` evicted via the shared mutex.
        assert_eq!(a.len(), 1); // still running, still there

        b.update("t1", TaskState::Completed("via clone b".into()));
        let from_a = a.get("t1").unwrap();
        assert!(matches!(from_a.state, TaskState::Completed(_)));
    }
}
