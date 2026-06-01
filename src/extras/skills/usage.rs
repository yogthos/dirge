//! Skill usage telemetry sidecar.
//!
//! Port of Hermes's `tools/skill_usage.py`. Tracks per-skill activity
//! counters, lifecycle state, and provenance in a `.usage.json`
//! sidecar file at `.dirge/skills/.usage.json`.
//!
//! Key design decisions from Hermes preserved:
//! - Sidecar, not frontmatter — keeps telemetry out of SKILL.md
//! - Atomic writes via tempfile + rename
//! - File locking for read-modify-write safety
//! - All counter bumps are best-effort — failures never break the
//!   underlying tool call
//! - Provenance filter: only agent-created skills are curator-managed

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::extras::dirge_paths::ProjectPaths;

/// Lifecycle state tracked by the curator.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillState {
    Active,
    Stale,
    Archived,
}

/// Per-skill telemetry record. Port of Hermes's skill_usage.py record shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default)]
    pub use_count: u64,
    #[serde(default)]
    pub view_count: u64,
    #[serde(default)]
    pub patch_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_viewed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_patched_at: Option<String>,
    pub created_at: String,
    #[serde(default = "default_state")]
    pub state: SkillState,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}

fn default_state() -> SkillState {
    SkillState::Active
}

impl SkillUsage {
    fn new(created_by: Option<&str>) -> Self {
        SkillUsage {
            created_by: created_by.map(|s| s.to_string()),
            use_count: 0,
            view_count: 0,
            patch_count: 0,
            last_used_at: None,
            last_viewed_at: None,
            last_patched_at: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            state: SkillState::Active,
            pinned: false,
            archived_at: None,
        }
    }
}

/// Sidecar store for skill telemetry at `.dirge/skills/.usage.json`.
/// Thread-safe via internal locking — all methods take `&mut self`.
#[derive(Clone)]
pub struct UsageStore {
    path: PathBuf,
    lock_path: PathBuf,
    data: HashMap<String, SkillUsage>,
}

impl UsageStore {
    /// Load the usage sidecar, creating an empty store if the file
    /// doesn't exist. Corrupt JSON results in a fresh start (best-effort,
    /// never blocks skill operations).
    pub fn load(paths: &ProjectPaths) -> Result<Self, String> {
        let path = paths.skills_dir().join(".usage.json");
        let lock_path = paths.skills_dir().join(".usage.json.lock");
        let data = read_usage_data(&path);
        Ok(UsageStore {
            path,
            lock_path,
            data,
        })
    }

    /// Write `self.data` to disk atomically WITHOUT acquiring the
    /// lock — the caller (`mutate_locked`) must already hold it.
    /// `acquire_usage_lock` is create-exclusive and NOT reentrant, so
    /// re-acquiring it here would self-deadlock.
    fn write_data(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create usage directory: {e}"))?;
        }
        let content = serde_json::to_string_pretty(&self.data)
            .map_err(|e| format!("Failed to serialize usage: {e}"))?;
        crate::fs_atomic::atomic_write_sync(&self.path, content.as_bytes())
            .map_err(|e| format!("Failed to write usage: {e}"))
    }

    /// dirge-szgc: apply a mutation as one atomic read-modify-write
    /// critical section. Holds the file lock across a fresh reload
    /// from disk, the mutation, and the write — so concurrent
    /// mutations (the curator's `set_state` batch vs a live
    /// skill-tool `record_use` from another turn, or a second
    /// process) can't lose each other's updates.
    ///
    /// Before this, `load` (unlocked) → mutate in-memory → `save`
    /// (locked write) let two handles each start from a stale
    /// snapshot and clobber one another (last writer wins). Now the
    /// mutation always applies to the latest on-disk state.
    /// `self.data` is left holding the just-written state.
    fn mutate_locked<F>(&mut self, f: F) -> Result<(), String>
    where
        F: FnOnce(&mut HashMap<String, SkillUsage>),
    {
        let _lock = acquire_usage_lock(&self.lock_path)?;
        // Reload under the lock so the delta lands on the freshest
        // on-disk state, not the (possibly stale) in-memory snapshot.
        self.data = read_usage_data(&self.path);
        f(&mut self.data);
        self.write_data()
    }

    /// Record a skill creation event.
    pub fn record_create(&mut self, name: &str, created_by: &str) {
        let name = name.to_string();
        let created_by = created_by.to_string();
        if let Err(e) = self.mutate_locked(|data| {
            let entry = data
                .entry(name.clone())
                .or_insert_with(|| SkillUsage::new(Some(&created_by)));
            // If the entry already existed, don't overwrite created_by.
            if entry.created_by.is_none() {
                entry.created_by = Some(created_by.clone());
            }
        }) {
            tracing::debug!(target: "dirge::skills::usage", error = %e, "record_create save failed");
        }
    }

    /// Record a skill use (agent invoked the skill).
    pub fn record_use(&mut self, name: &str) {
        let name = name.to_string();
        if let Err(e) = self.mutate_locked(|data| {
            let entry = data
                .entry(name.clone())
                .or_insert_with(|| SkillUsage::new(None));
            entry.use_count = entry.use_count.saturating_add(1);
            entry.last_used_at = Some(now_iso());
        }) {
            tracing::debug!(target: "dirge::skills::usage", error = %e, "record_use save failed");
        }
    }

    /// Record a skill view (read the skill content).
    pub fn record_view(&mut self, name: &str) {
        let name = name.to_string();
        if let Err(e) = self.mutate_locked(|data| {
            let entry = data
                .entry(name.clone())
                .or_insert_with(|| SkillUsage::new(None));
            entry.view_count = entry.view_count.saturating_add(1);
            entry.last_viewed_at = Some(now_iso());
        }) {
            tracing::debug!(target: "dirge::skills::usage", error = %e, "record_view save failed");
        }
    }

    /// Record a skill patch (content was modified).
    pub fn record_patch(&mut self, name: &str) {
        let name = name.to_string();
        if let Err(e) = self.mutate_locked(|data| {
            let entry = data
                .entry(name.clone())
                .or_insert_with(|| SkillUsage::new(None));
            entry.patch_count = entry.patch_count.saturating_add(1);
            entry.last_patched_at = Some(now_iso());
        }) {
            tracing::debug!(target: "dirge::skills::usage", error = %e, "record_patch save failed");
        }
    }

    /// Set the pinned flag. Pinned skills are exempt from curator transitions.
    #[allow(dead_code)]
    pub fn set_pinned(&mut self, name: &str, pinned: bool) -> Result<(), String> {
        let name = name.to_string();
        self.mutate_locked(|data| {
            let entry = data
                .entry(name.clone())
                .or_insert_with(|| SkillUsage::new(None));
            entry.pinned = pinned;
        })
    }

    /// Set the lifecycle state.
    pub fn set_state(&mut self, name: &str, state: SkillState) -> Result<(), String> {
        let name = name.to_string();
        self.mutate_locked(|data| {
            let entry = data
                .entry(name.clone())
                .or_insert_with(|| SkillUsage::new(None));
            let is_archived = state == SkillState::Archived;
            entry.state = state;
            if is_archived {
                entry.archived_at = Some(now_iso());
            }
        })
    }

    /// Provenance filter: only skills created by the agent are
    /// curator-managed. Bundled/shipped skills have `created_by: None`
    /// or a non-"agent" value.
    pub fn is_agent_created(&self, name: &str) -> bool {
        self.data
            .get(name)
            .and_then(|u| u.created_by.as_deref())
            .map(|c| c == "agent")
            .unwrap_or(false)
    }

    /// Seconds since the most recent activity (max of last_used_at,
    /// last_patched_at). Returns None if the skill has never been used
    /// or patched (just created).
    pub fn activity_age_seconds(&self, name: &str) -> Option<u64> {
        let entry = self.data.get(name)?;
        let newest = [
            entry.last_used_at.as_deref(),
            entry.last_patched_at.as_deref(),
        ]
        .into_iter()
        .flatten()
        .max();
        let ts = newest?;
        let parsed = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
        let now = chrono::Utc::now();
        let age = now.signed_duration_since(parsed);
        Some(age.num_seconds().max(0) as u64)
    }

    /// Get a reference to the usage record for a skill, if it exists.
    pub fn get(&self, name: &str) -> Option<&SkillUsage> {
        self.data.get(name)
    }

    /// Iterate over all skill names tracked in the usage store.
    #[allow(dead_code)]
    pub fn skill_names(&self) -> impl Iterator<Item = &String> {
        self.data.keys()
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Read the usage sidecar from disk into a map. Missing file → empty;
/// corrupt JSON → empty (best-effort, never blocks skill ops). Pure
/// (no lock); callers that need atomicity hold the lock around it.
fn read_usage_data(path: &Path) -> HashMap<String, SkillUsage> {
    if !path.exists() {
        return HashMap::new();
    }
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::debug!(
                target: "dirge::skills::usage",
                error = %e,
                "Corrupt .usage.json — starting fresh"
            );
            HashMap::new()
        }),
        Err(e) => {
            tracing::debug!(
                target: "dirge::skills::usage",
                error = %e,
                "Cannot read .usage.json — starting fresh"
            );
            HashMap::new()
        }
    }
}

/// Acquire an exclusive file lock on the usage sidecar lock file.
/// Uses create-exclusive semantics with PID-based staleness detection
/// (same pattern as memory_store.rs).
fn acquire_usage_lock(lock_path: &PathBuf) -> Result<UsageLock, String> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create lock directory: {e}"))?;
    }
    for attempt in 0..50 {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut f) => {
                let pid = std::process::id().to_string();
                let _ = std::io::Write::write_all(&mut f, pid.as_bytes());
                return Ok(UsageLock {
                    path: lock_path.clone(),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Check staleness on first attempt only.
                if attempt == 0
                    && let Ok(content) = std::fs::read_to_string(lock_path)
                {
                    let pid: Result<u32, _> = content.trim().parse();
                    if let Ok(pid) = pid {
                        // pid feeds only the Unix liveness check; unused
                        // on non-unix (no kill(2) probe there).
                        #[cfg(not(unix))]
                        let _ = pid;
                        // Check if process is still alive.
                        #[cfg(unix)]
                        {
                            unsafe {
                                if libc::kill(pid as i32, 0) != 0 {
                                    let _ = std::fs::remove_file(lock_path);
                                    continue;
                                }
                            }
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                return Err(format!("Failed to acquire usage lock: {e}"));
            }
        }
    }
    Err("Timed out waiting for usage file lock".to_string())
}

struct UsageLock {
    path: PathBuf,
}

impl Drop for UsageLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project() -> (ProjectPaths, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "dirge-usage-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = ProjectPaths::new(&dir);
        (paths, dir)
    }

    #[test]
    fn load_empty_usage_store() {
        let (paths, _dir) = temp_project();
        let store = UsageStore::load(&paths).unwrap();
        assert!(store.data.is_empty());
    }

    #[test]
    fn record_create_sets_created_by() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_create("my-skill", "agent");
        assert_eq!(
            store.data.get("my-skill").unwrap().created_by.as_deref(),
            Some("agent")
        );
    }

    #[test]
    fn record_use_bumps_counter() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_use("my-skill");
        store.record_use("my-skill");
        assert_eq!(store.data.get("my-skill").unwrap().use_count, 2);
        assert!(store.data.get("my-skill").unwrap().last_used_at.is_some());
    }

    #[test]
    fn record_view_bumps_counter() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_view("my-skill");
        assert_eq!(store.data.get("my-skill").unwrap().view_count, 1);
    }

    #[test]
    fn record_patch_bumps_counter() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_patch("my-skill");
        store.record_patch("my-skill");
        assert_eq!(store.data.get("my-skill").unwrap().patch_count, 2);
    }

    #[test]
    fn is_agent_created_filters_correctly() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_create("agent-skill", "agent");
        store.record_create("bundled-skill", "bundled");

        assert!(store.is_agent_created("agent-skill"));
        assert!(!store.is_agent_created("bundled-skill"));
        assert!(!store.is_agent_created("nonexistent"));
    }

    #[test]
    fn null_created_by_is_not_agent_created() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        // Skills created via record_use without record_create get None created_by.
        store.record_use("unknown-origin");
        assert!(!store.is_agent_created("unknown-origin"));
    }

    #[test]
    fn set_pinned_and_state() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_create("my-skill", "agent");
        store.set_pinned("my-skill", true).unwrap();
        assert!(store.get("my-skill").unwrap().pinned);

        store.set_state("my-skill", SkillState::Archived).unwrap();
        assert_eq!(store.get("my-skill").unwrap().state, SkillState::Archived);
        assert!(store.get("my-skill").unwrap().archived_at.is_some());
    }

    #[test]
    fn activity_age_seconds_returns_correct_diff() {
        let (paths, _dir) = temp_project();
        let mut store = UsageStore::load(&paths).unwrap();
        store.record_use("my-skill");
        let age = store.activity_age_seconds("my-skill");
        assert!(age.is_some());
        assert!(age.unwrap() < 5, "activity age should be under 5 seconds");
    }

    #[test]
    fn roundtrip_save_and_reload() {
        let (paths, _dir) = temp_project();
        {
            let mut store = UsageStore::load(&paths).unwrap();
            // Each mutation persists atomically (mutate_locked) — no
            // explicit save needed.
            store.record_create("test-skill", "agent");
            store.record_use("test-skill");
            store.record_patch("test-skill");
        }
        // Reload from disk.
        let store2 = UsageStore::load(&paths).unwrap();
        let entry = store2.get("test-skill").unwrap();
        assert_eq!(entry.created_by.as_deref(), Some("agent"));
        assert_eq!(entry.use_count, 1);
        assert_eq!(entry.patch_count, 1);
    }

    /// dirge-szgc regression: a mutation through one handle must NOT
    /// clobber a concurrent mutation made through another handle that
    /// started from a stale snapshot. This is the lost-update TOCTOU
    /// — simulated deterministically with two store handles rather
    /// than real threads.
    #[test]
    fn concurrent_mutations_do_not_lose_updates() {
        let (paths, _dir) = temp_project();

        // Handle A creates the skill and bumps use_count to 1.
        let mut a = UsageStore::load(&paths).unwrap();
        a.record_create("x", "agent");
        a.record_use("x"); // disk: use_count=1

        // Handle B loads a snapshot now (sees use_count=1, view_count=0).
        let mut b = UsageStore::load(&paths).unwrap();
        assert_eq!(b.get("x").unwrap().use_count, 1);

        // Meanwhile, handle A bumps use_count again → disk use_count=2.
        a.record_use("x");

        // Handle B, still holding its stale snapshot (use_count=1),
        // records a VIEW. Pre-fix, B would write its snapshot and
        // clobber A's second bump (use_count back to 1). Post-fix,
        // record_view reloads under the lock and sees use_count=2.
        b.record_view("x");

        // Fresh read from disk: BOTH A's bumps AND B's view survived.
        let c = UsageStore::load(&paths).unwrap();
        let entry = c.get("x").unwrap();
        assert_eq!(
            entry.use_count, 2,
            "handle A's second use bump must not be lost by handle B's write",
        );
        assert_eq!(
            entry.view_count, 1,
            "handle B's view must be recorded on top of the latest state",
        );
    }

    #[test]
    fn corrupt_json_recovers_gracefully() {
        let (paths, _dir) = temp_project();
        std::fs::create_dir_all(paths.skills_dir()).unwrap();
        std::fs::write(paths.skills_dir().join(".usage.json"), "not valid json{{{").unwrap();

        let store = UsageStore::load(&paths).unwrap();
        assert!(
            store.data.is_empty(),
            "corrupt JSON should result in empty store"
        );
    }
}
