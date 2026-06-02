use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// LOOP-3: stat a path and return a `mtime_ns:size` suffix suitable
/// for splicing into a cache key. External writes (LSP, IDE,
/// plugin-spawned bash, MCP tool) change one or both, so any
/// previously cached entry under the old suffix is automatically
/// unreachable on the next read. Failure (file missing, perms)
/// returns `"0:0"` which intentionally won't collide with a real
/// stat — the next call resolves to a fresh entry.
pub fn fs_stamp(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) => {
            let nanos = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{}:{}", nanos, m.len())
        }
        Err(_) => "0:0".to_string(),
    }
}

/// Like `fs_stamp` but uses cwd when the path is empty or doesn't
/// resolve to a stat-able target. Useful for tools that operate on
/// a directory and use the directory's mtime as the cache-validity
/// signal (e.g. `list_dir`, `find_files`).
pub fn fs_stamp_or_cwd(path: &str) -> String {
    let p = if path.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    } else {
        std::path::PathBuf::from(path)
    };
    fs_stamp(&p)
}

struct CacheEntry {
    value: String,
    generation: u64,
}

#[derive(Clone)]
pub struct ToolCache {
    entries: Arc<Mutex<HashMap<String, CacheEntry>>>,
    generation: Arc<AtomicU64>,
    /// Read-before-edit gate (ported from vix `session_read_gate.go`): the
    /// set of canonical paths the model has read this session. `edit`/
    /// `apply_patch`-update refuse a file not in this set so a change can't be
    /// built against unread (hallucinated/stale) content. Lives here because
    /// `ToolCache` is the shared per-session handle already threaded into
    /// read/edit/write/apply_patch. Deliberately NOT touched by `clear()` —
    /// it's session-lifetime tracking, independent of the content cache's
    /// per-turn generation.
    read_files: Arc<Mutex<HashSet<PathBuf>>>,
}

impl Default for ToolCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCache {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            generation: Arc::new(AtomicU64::new(0)),
            read_files: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Record that `path` (canonical) has been read this session. Called by
    /// `read` and on a successful `edit`/`write`/`apply_patch` (a successful
    /// mutation leaves the model with accurate knowledge of on-disk content).
    pub fn mark_read(&self, path: &Path) {
        self.read_files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_path_buf());
    }

    /// Whether `path` (canonical) has been read this session. The edit gate
    /// blocks when this is false.
    pub fn has_been_read(&self, path: &Path) -> bool {
        self.read_files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(path)
    }

    pub fn get(&self, key: &str) -> Option<String> {
        let current_gen = self.generation.load(Ordering::Relaxed);
        let guard = self.entries.lock().unwrap();
        match guard.get(key) {
            Some(e) if e.generation == current_gen => Some(e.value.clone()),
            _ => None,
        }
    }

    pub fn set(&self, key: &str, value: String) {
        // Race note (audit H8): the generation is read with `Relaxed`
        // then the entries mutex is taken to insert. A concurrent
        // `clear` could increment the generation between the load
        // and the insert, leaving the just-inserted entry tagged
        // with a stale generation. That's benign — `get` re-checks
        // `e.generation == current_gen` and returns `None` for any
        // entry whose generation doesn't match the live counter, so
        // a stale-generation entry is unreachable and will be
        // overwritten on the next `set` for the same key. Not worth
        // the cost of holding the mutex across the generation read.
        let current_gen = self.generation.load(Ordering::Relaxed);
        self.entries.lock().unwrap().insert(
            key.to_string(),
            CacheEntry {
                value,
                generation: current_gen,
            },
        );
    }

    pub fn clear(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.entries.lock().unwrap().clear();
    }

    /// Test/diagnostic helper: are these two `ToolCache` handles
    /// backed by the same underlying entries Arc?
    ///
    /// `ToolCache: Clone` shares the inner Arc, so a clone returns
    /// `true`; a freshly constructed cache returns `false`. Used
    /// by `provider::mod_tests` to assert that the Phase 4 background
    /// review runner gets an isolated cache (dirge-7ls regression).
    #[allow(dead_code)]
    pub(crate) fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.entries, &other.entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_hit_and_miss() {
        let cache = ToolCache::new();
        assert!(cache.get("key1").is_none());
        cache.set("key1", "value1".to_string());
        assert_eq!(cache.get("key1"), Some("value1".to_string()));
    }

    #[test]
    fn test_cache_clear_invalidates_entries() {
        let cache = ToolCache::new();
        cache.set("key1", "value1".to_string());
        cache.clear();
        assert!(cache.get("key1").is_none());
    }

    #[test]
    fn test_cache_clone_shares_state() {
        let cache1 = ToolCache::new();
        let cache2 = cache1.clone();
        cache1.set("shared", "data".to_string());
        assert_eq!(cache2.get("shared"), Some("data".to_string()));
    }

    #[test]
    fn test_clear_in_one_clone_affects_other() {
        let cache1 = ToolCache::new();
        let cache2 = cache1.clone();
        cache1.set("x", "y".to_string());
        cache2.clear();
        assert!(cache1.get("x").is_none());
    }

    #[test]
    fn read_gate_tracks_and_reports() {
        let cache = ToolCache::new();
        let p = Path::new("/tmp/dirge-read-gate.rs");
        assert!(!cache.has_been_read(p), "unread by default");
        cache.mark_read(p);
        assert!(cache.has_been_read(p), "marked read");
        assert!(
            !cache.has_been_read(Path::new("/tmp/other.rs")),
            "only the marked path"
        );
    }

    #[test]
    fn read_set_survives_clear() {
        // `clear()` invalidates the content cache (per-turn) but MUST NOT drop
        // the session read-set, or every post-edit `clear` would re-block edits.
        let cache = ToolCache::new();
        let p = Path::new("/tmp/dirge-read-survive.rs");
        cache.mark_read(p);
        cache.clear();
        assert!(cache.has_been_read(p), "read-set persists across clear");
    }

    #[test]
    fn read_set_shared_across_clones() {
        // Tools each get a clone of the session cache; a read via one tool
        // must satisfy the gate checked by another.
        let cache1 = ToolCache::new();
        let cache2 = cache1.clone();
        let p = Path::new("/tmp/dirge-read-shared.rs");
        cache1.mark_read(p);
        assert!(cache2.has_been_read(p), "read-set shares the inner Arc");
    }
}
