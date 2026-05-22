use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct CacheEntry {
    value: String,
    generation: u64,
}

#[derive(Clone)]
pub struct ToolCache {
    entries: Arc<Mutex<HashMap<String, CacheEntry>>>,
    generation: Arc<AtomicU64>,
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
        }
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
}
