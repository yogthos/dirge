use indexmap::IndexMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Maximum chars retained per Completed/Failed payload. Prevents a single
/// subagent answer from blowing the parent context.
const MAX_TASK_OUTPUT_CHARS: usize = 3000;

/// Maximum number of tasks retained in the store. When a new task is
/// inserted past this cap, the oldest task by insertion order is evicted
/// (FIFO — `get` does not bump access order). Plenty of headroom for any
/// reasonable session; agents only see ids they themselves spawned.
const STORE_CAPACITY: usize = 32;

/// Thread-safe store for background subagent tasks.
///
/// Tasks persist after completion so the parent agent can look them up by id
/// via the `task_status` tool. Completion events are queued separately for
/// push-style delivery (see [`drain_notifications`]); the agent does not need
/// to poll.
#[derive(Debug, Clone, Default)]
pub struct BackgroundStore {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Tasks keyed by id. Insertion order preserved so the oldest entry can
    /// be evicted when at capacity. Drain does not remove tasks from here;
    /// they remain looked-up-able by `task_status` until LRU eviction.
    tasks: IndexMap<String, BackgroundTask>,
    /// Ids waiting to be delivered as notifications. FIFO. Drained items
    /// disappear from the queue but remain in `tasks`.
    pending: VecDeque<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TaskState {
    Running,
    Completed(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct BackgroundTask {
    pub state: TaskState,
}

/// A completion event ready to be surfaced to the parent agent at its next
/// turn boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskNotification {
    pub id: String,
    pub state: TaskState,
}

impl BackgroundStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a new task in Running state. If the store is at capacity, the
    /// oldest task is evicted to make room. Inserting an existing id replaces
    /// it in place without evicting anything.
    pub fn insert(&self, id: String) {
        let mut inner = self.lock();
        if !inner.tasks.contains_key(&id) && inner.tasks.len() >= STORE_CAPACITY {
            // Evict the oldest by insertion order. shift_remove preserves order
            // of the remaining entries.
            inner.tasks.shift_remove_index(0);
        }
        inner.tasks.insert(
            id,
            BackgroundTask {
                state: TaskState::Running,
            },
        );
    }

    /// Look up the current state of a task without mutating the store.
    pub fn get(&self, id: &str) -> Option<BackgroundTask> {
        self.lock().tasks.get(id).cloned()
    }

    /// Record a terminal state (Completed or Failed) and queue a notification
    /// for delivery. Truncates the payload to MAX_TASK_OUTPUT_CHARS.
    ///
    /// No-op if the id has been evicted from the store. Calling notify with
    /// `TaskState::Running` is also a no-op — Running is the initial state
    /// set by `insert` and not a terminal transition.
    pub fn notify(&self, id: &str, state: TaskState) {
        if matches!(state, TaskState::Running) {
            return;
        }
        let mut inner = self.lock();
        let Some(task) = inner.tasks.get_mut(id) else {
            return;
        };
        task.state = truncate_state(state);
        // Guard against double-notifies enqueuing the same id twice.
        if !inner.pending.iter().any(|existing| existing == id) {
            inner.pending.push_back(id.to_string());
        }
    }

    /// Take all queued notifications and clear the queue. Each notification is
    /// delivered exactly once; subsequent calls return only notifications
    /// arriving after the previous drain.
    pub fn drain_notifications(&self) -> Vec<TaskNotification> {
        let mut inner = self.lock();
        let ids: Vec<String> = inner.pending.drain(..).collect();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(task) = inner.tasks.get(&id) {
                out.push(TaskNotification {
                    id,
                    state: task.state.clone(),
                });
            }
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().tasks.len()
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.lock().pending.len()
    }
}

/// Format pending notifications as a `<system-reminder>` block prepended to
/// the next user prompt. Returns the prompt unchanged when there's nothing
/// pending or no store is provided.
///
/// Drains the queue so each notification is delivered exactly once. The
/// underlying tasks remain in the store and remain looked-up-able by
/// `task_status` until LRU eviction.
pub fn prepend_pending_notifications(prompt: &str, store: Option<&BackgroundStore>) -> String {
    let Some(store) = store else {
        return prompt.to_string();
    };
    let drained = store.drain_notifications();
    if drained.is_empty() {
        return prompt.to_string();
    }

    let mut out = String::with_capacity(prompt.len() + 256);
    out.push_str("<system-reminder>\n");
    out.push_str("The following background tasks finished since your last turn:\n\n");
    for (i, n) in drained.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match &n.state {
            TaskState::Completed(text) => {
                out.push_str(&format!("Task {} (completed):\n{}\n", n.id, text));
            }
            TaskState::Failed(err) => {
                out.push_str(&format!("Task {} (failed):\n{}\n", n.id, err));
            }
            // Running is never queued for notification (notify() rejects it),
            // but treat defensively as "no terminal info to report".
            TaskState::Running => {}
        }
    }
    out.push_str("</system-reminder>\n\n");
    out.push_str(prompt);
    out
}

fn truncate_state(state: TaskState) -> TaskState {
    match state {
        TaskState::Completed(text) => {
            let t: String = text.chars().take(MAX_TASK_OUTPUT_CHARS).collect();
            TaskState::Completed(t)
        }
        TaskState::Failed(err) => {
            let e: String = err.chars().take(MAX_TASK_OUTPUT_CHARS).collect();
            TaskState::Failed(e)
        }
        s => s,
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
        assert_eq!(task.state, TaskState::Running);
    }

    #[test]
    fn get_on_missing_returns_none() {
        assert!(BackgroundStore::new().get("nope").is_none());
    }

    // Regression: previously get() evicted completed/failed tasks. The new
    // model keeps tasks until eviction by LRU cap, since notifications are
    // delivered out-of-band and task_status is read-only.
    #[test]
    fn regression_get_is_read_only_after_completion() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("done".into()));

        for _ in 0..3 {
            let task = store.get("t1").expect("must remain after read");
            assert_eq!(task.state, TaskState::Completed("done".into()));
        }
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn notify_pushes_completed_to_pending_queue() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("done".into()));
        assert_eq!(store.pending_len(), 1);
    }

    // Regression: subagent output was previously injected verbatim and bloated
    // the parent context. notify() now truncates by chars (UTF-8-safe).
    #[test]
    fn regression_notify_truncates_completed_text() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        let huge = "x".repeat(MAX_TASK_OUTPUT_CHARS * 2);
        store.notify("t1", TaskState::Completed(huge));

        let TaskState::Completed(text) = store.get("t1").unwrap().state else {
            panic!("expected Completed");
        };
        assert_eq!(text.chars().count(), MAX_TASK_OUTPUT_CHARS);
    }

    #[test]
    fn regression_notify_truncates_failed_error() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        let huge = "e".repeat(MAX_TASK_OUTPUT_CHARS * 2);
        store.notify("t1", TaskState::Failed(huge));

        let TaskState::Failed(text) = store.get("t1").unwrap().state else {
            panic!("expected Failed");
        };
        assert_eq!(text.chars().count(), MAX_TASK_OUTPUT_CHARS);
    }

    // Regression: multibyte chars must not be split. Guards against switching
    // to bytes-based truncation.
    #[test]
    fn notify_truncates_by_chars_not_bytes() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        let emojis = "🦀".repeat(MAX_TASK_OUTPUT_CHARS * 2);
        store.notify("t1", TaskState::Completed(emojis));
        let TaskState::Completed(text) = store.get("t1").unwrap().state else {
            panic!("expected Completed");
        };
        assert_eq!(text.chars().count(), MAX_TASK_OUTPUT_CHARS);
        // Re-encoding round-trips: no broken UTF-8 sequences.
        assert_eq!(text.as_str(), &"🦀".repeat(MAX_TASK_OUTPUT_CHARS));
    }

    #[test]
    fn notify_on_missing_id_is_noop() {
        let store = BackgroundStore::new();
        store.notify("ghost", TaskState::Completed("never inserted".into()));
        assert!(store.get("ghost").is_none());
        assert_eq!(store.pending_len(), 0);
    }

    #[test]
    fn notify_with_running_state_is_noop() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Running);
        assert_eq!(store.pending_len(), 0);
        // State unchanged.
        assert_eq!(store.get("t1").unwrap().state, TaskState::Running);
    }

    // Regression: notify() must be idempotent on the pending queue — if a
    // subagent runner accidentally double-notifies, the agent must not see
    // the same completion twice.
    #[test]
    fn regression_double_notify_enqueues_once() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("first".into()));
        store.notify("t1", TaskState::Completed("second".into()));
        assert_eq!(store.pending_len(), 1);
        // The latest state wins.
        let TaskState::Completed(text) = store.get("t1").unwrap().state else {
            panic!("expected Completed");
        };
        assert_eq!(text, "second");
    }

    #[test]
    fn drain_returns_pending_then_empties_queue() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.insert("t2".into());
        store.notify("t1", TaskState::Completed("a".into()));
        store.notify("t2", TaskState::Failed("b".into()));

        let drained = store.drain_notifications();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].id, "t1");
        assert_eq!(drained[0].state, TaskState::Completed("a".into()));
        assert_eq!(drained[1].id, "t2");
        assert_eq!(drained[1].state, TaskState::Failed("b".into()));

        // Drained items don't reappear; tasks remain in the store for lookup.
        assert!(store.drain_notifications().is_empty());
        assert!(store.get("t1").is_some());
        assert!(store.get("t2").is_some());
    }

    #[test]
    fn drain_is_empty_when_nothing_pending() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        // Insert alone doesn't enqueue — only notify() does.
        assert!(store.drain_notifications().is_empty());
    }

    // Regression: previously the store grew unbounded across long sessions.
    // The new bound is an LRU cap that evicts the oldest entry on overflow.
    #[test]
    fn regression_lru_evicts_oldest_at_capacity() {
        let store = BackgroundStore::new();
        for i in 0..STORE_CAPACITY {
            store.insert(format!("t{i}"));
        }
        assert_eq!(store.len(), STORE_CAPACITY);
        // One more push past capacity.
        store.insert("overflow".into());
        assert_eq!(store.len(), STORE_CAPACITY);
        // The oldest (t0) is gone; the newest is retained.
        assert!(store.get("t0").is_none());
        assert!(store.get("overflow").is_some());
        assert!(store.get(&format!("t{}", STORE_CAPACITY - 1)).is_some());
    }

    // Re-inserting an existing id must not trigger eviction — that would
    // surprise callers who happened to reuse an id.
    #[test]
    fn re_insert_existing_id_does_not_evict() {
        let store = BackgroundStore::new();
        for i in 0..STORE_CAPACITY {
            store.insert(format!("t{i}"));
        }
        // Re-insert at capacity: the existing id should just be reset.
        store.insert("t5".into());
        assert_eq!(store.len(), STORE_CAPACITY);
        assert!(store.get("t0").is_some(), "oldest must NOT be evicted");
        assert_eq!(store.get("t5").unwrap().state, TaskState::Running);
    }

    // Regression: a task that's been evicted before notify() runs must not
    // produce a phantom notification.
    #[test]
    fn regression_notify_on_evicted_id_is_noop() {
        let store = BackgroundStore::new();
        for i in 0..STORE_CAPACITY {
            store.insert(format!("t{i}"));
        }
        store.insert("overflow".into()); // evicts t0

        store.notify("t0", TaskState::Completed("late".into()));
        assert_eq!(store.pending_len(), 0);
        assert!(store.drain_notifications().is_empty());
    }

    // The store is Clone + thread-safe; clones must share inner state.
    #[test]
    fn clones_share_state() {
        let a = BackgroundStore::new();
        let b = a.clone();
        a.insert("t1".into());
        b.notify("t1", TaskState::Completed("via b".into()));

        let drained = a.drain_notifications();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "t1");
    }

    // ---- prepend_pending_notifications ----

    #[test]
    fn prepend_passthrough_when_store_is_none() {
        let out = prepend_pending_notifications("hello", None);
        assert_eq!(out, "hello");
    }

    #[test]
    fn prepend_passthrough_when_nothing_pending() {
        let store = BackgroundStore::new();
        store.insert("t1".into()); // running, not pending
        let out = prepend_pending_notifications("hello", Some(&store));
        assert_eq!(out, "hello");
    }

    #[test]
    fn prepend_formats_system_reminder() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("the result".into()));

        let out = prepend_pending_notifications("user msg", Some(&store));
        assert!(out.starts_with("<system-reminder>\n"));
        assert!(out.contains("Task t1 (completed):"));
        assert!(out.contains("the result"));
        assert!(out.contains("</system-reminder>\n\n"));
        assert!(out.ends_with("user msg"));
    }

    #[test]
    fn prepend_includes_failed_tasks() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Failed("kaboom".into()));

        let out = prepend_pending_notifications("user msg", Some(&store));
        assert!(out.contains("Task t1 (failed):"));
        assert!(out.contains("kaboom"));
    }

    // Regression: prepend MUST consume the queue. Calling it twice in
    // succession must not re-deliver the same notifications, otherwise the
    // agent would see the same completion on every turn.
    #[test]
    fn regression_prepend_drains_queue_once() {
        let store = BackgroundStore::new();
        store.insert("t1".into());
        store.notify("t1", TaskState::Completed("once".into()));

        let first = prepend_pending_notifications("msg", Some(&store));
        assert!(first.contains("once"));

        let second = prepend_pending_notifications("msg", Some(&store));
        assert_eq!(second, "msg");
    }

    #[test]
    fn prepend_includes_all_pending_tasks_in_order() {
        let store = BackgroundStore::new();
        for i in 0..3 {
            store.insert(format!("t{i}"));
            store.notify(&format!("t{i}"), TaskState::Completed(format!("r{i}")));
        }
        let out = prepend_pending_notifications("msg", Some(&store));
        // FIFO order preserved.
        let i0 = out.find("Task t0").unwrap();
        let i1 = out.find("Task t1").unwrap();
        let i2 = out.find("Task t2").unwrap();
        assert!(i0 < i1 && i1 < i2);
    }

    // Concurrency smoke: many threads inserting + notifying must not lose
    // notifications. Each thread's task should be drainable from any handle.
    #[test]
    fn concurrent_inserts_and_notifies() {
        let store = BackgroundStore::new();
        let mut handles = Vec::new();
        // Stay below STORE_CAPACITY so nothing gets evicted.
        let n = STORE_CAPACITY;
        for i in 0..n {
            let s = store.clone();
            let id = format!("t{i}");
            handles.push(std::thread::spawn(move || {
                s.insert(id.clone());
                s.notify(&id, TaskState::Completed(format!("done-{i}")));
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let drained = store.drain_notifications();
        assert_eq!(drained.len(), n);
        // Every id appears exactly once.
        let mut ids: Vec<String> = drained.into_iter().map(|n| n.id).collect();
        ids.sort();
        let mut expected: Vec<String> = (0..n).map(|i| format!("t{i}")).collect();
        expected.sort();
        assert_eq!(ids, expected);
    }
}
