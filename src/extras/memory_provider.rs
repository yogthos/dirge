//! Pluggable memory backend trait — port of hermes-agent's
//! `agent/memory_provider.py` `MemoryProvider` ABC, adapted for
//! Rust.
//!
//! Hermes lets users swap the built-in memory tool for a backend of
//! their choice (Hindsight, Honcho, custom) by implementing this
//! ABC. Dirge previously hard-coded `MemoryToolStore` everywhere it
//! used memory, blocking any future alternative backend.
//!
//! Design decisions ported from hermes:
//! - Lifecycle hooks (`on_session_end`, `on_memory_write`,
//!   `on_pre_compress`) so providers can react to events without
//!   being asked.
//! - Core CRUD (`view`/`add`/`replace`/`remove`) matching the
//!   existing `MemoryTool` schema so the tool layer doesn't need a
//!   parallel rewrite.
//! - Default no-op hooks so existing back-ends only override what
//!   they care about.
//!
//! The dirge `MemoryToolStore` (per-project MEMORY.md/PITFALLS.md
//! backing the default tool) is the canonical implementation. New
//! backends — e.g. a future MCP-server-backed provider, an embedding
//! store, a global cross-project store — implement this trait and
//! plug in at `agent::builder` time.
//!
//! See dirge-bov5.

use serde_json::Value;
use std::sync::Arc;

/// Pluggable backend for the `memory` tool. Implementors are stored
/// behind `Arc<dyn MemoryProvider>` so the tool layer can hold a
/// fixed reference while the concrete backend is swapped at agent
/// construction time.
pub trait MemoryProvider: Send + Sync {
    /// Short identifier — used in logs and diagnostics. Hermes uses
    /// `"builtin"`, `"hindsight"`, etc.
    fn name(&self) -> &str;

    /// Render the frozen system-prompt snapshot for this provider.
    /// Called once at agent-builder time; the result is injected
    /// into the preamble. Return an empty string to skip injection.
    fn format_for_system_prompt(&self) -> String {
        String::new()
    }

    /// Return all entries under `target` (e.g. `"memory"` /
    /// `"pitfalls"`). The response shape matches the existing tool
    /// schema — a JSON object with `entries`, `count`, `usage_pct`.
    fn view(&self, target: &str) -> Value;

    /// Append a new entry.
    fn add(&self, target: &str, content: &str) -> Result<Value, String>;

    /// Replace an entry matched by substring. `old_text` must
    /// uniquely identify an entry; ambiguous matches error.
    fn replace(&self, target: &str, old_text: &str, content: &str) -> Result<Value, String>;

    /// Drop an entry matched by substring. Same uniqueness rule as
    /// `replace`.
    fn remove(&self, target: &str, old_text: &str) -> Result<Value, String>;

    // ── Optional lifecycle hooks — default no-ops ──────────────

    /// Notify the provider that a memory write just happened via
    /// the tool layer. Use to mirror the write to a secondary
    /// backend (e.g. a vector store), audit log, or analytics
    /// sink. `action` is one of `"add"`, `"replace"`, `"remove"`.
    fn on_memory_write(&self, _action: &str, _target: &str, _content: &str) {}

    /// Notify the provider that the live session ended. Use for
    /// end-of-session fact extraction, queue flushing, or
    /// summarization. `transcript` is the full conversation text.
    fn on_session_end(&self, _transcript: &str) {}

    /// Notify the provider that the session id is changing
    /// mid-process. Ported from hermes
    /// `MemoryProvider.on_session_switch` (memory_provider.py:162-194).
    ///
    /// Fires on dirge events that reassign `session.id` without
    /// tearing the provider down — currently the compaction-driven
    /// rotation (every successful auto-compact creates a new session
    /// id whose `parent_session_id` is the pre-compact id).
    ///
    /// Providers that cache per-session state in their backend
    /// (document ids, accumulated buffers, counters) should update
    /// or reset it here so subsequent writes land in the correct
    /// session's record.
    ///
    /// `new_session_id` — the id the agent just switched to.
    /// `parent_session_id` — the previous id, empty when no
    /// lineage applies.
    /// `reset` — `true` when this is a fresh conversation (not a
    /// continuation). Compaction rotation is a continuation, so
    /// dirge passes `false`. Reserved for future `/reset`-style
    /// commands.
    fn on_session_switch(&self, _new_session_id: &str, _parent_session_id: &str, _reset: bool) {}

    /// Notify the provider that messages are about to be discarded
    /// during context compression. The provider may return a brief
    /// summary string that the compression pass will fold into the
    /// summary prompt so any provider-extracted insights survive.
    /// Default returns an empty string.
    fn on_pre_compress(&self, _transcript: &str) -> String {
        String::new()
    }
}

/// Implementing `MemoryProvider` on the dirge built-in
/// `MemoryToolStore` makes it the canonical backend without changing
/// any of its existing public methods. Plugin providers can wrap a
/// store via a delegating impl if they want to keep file persistence
/// while augmenting with side effects (e.g. mirroring to a remote).
impl MemoryProvider for super::memory_store::MemoryToolStore {
    fn name(&self) -> &str {
        "builtin"
    }

    fn format_for_system_prompt(&self) -> String {
        super::memory_store::MemoryToolStore::format_for_system_prompt(self)
    }

    fn view(&self, target: &str) -> Value {
        super::memory_store::MemoryToolStore::view(self, target)
    }

    fn add(&self, target: &str, content: &str) -> Result<Value, String> {
        let result = super::memory_store::MemoryToolStore::add(self, target, content)?;
        self.on_memory_write("add", target, content);
        Ok(result)
    }

    fn replace(&self, target: &str, old_text: &str, content: &str) -> Result<Value, String> {
        let result =
            super::memory_store::MemoryToolStore::replace(self, target, old_text, content)?;
        self.on_memory_write("replace", target, content);
        Ok(result)
    }

    fn remove(&self, target: &str, old_text: &str) -> Result<Value, String> {
        let result = super::memory_store::MemoryToolStore::remove(self, target, old_text)?;
        self.on_memory_write("remove", target, old_text);
        Ok(result)
    }
}

/// Boxed-provider alias used by the tool layer. Consumers hold an
/// `Arc<dyn MemoryProvider>` so the concrete backend can be swapped
/// at agent construction time without churning the call sites.
pub type DynMemoryProvider = Arc<dyn MemoryProvider>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A minimal test provider that records writes to a vec. Proves
    /// the trait is implementable outside the built-in store.
    #[derive(Default)]
    struct RecordingProvider {
        writes: Mutex<Vec<(String, String, String)>>,
    }

    impl MemoryProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording-test"
        }
        fn view(&self, _target: &str) -> Value {
            Value::Null
        }
        fn add(&self, target: &str, content: &str) -> Result<Value, String> {
            self.on_memory_write("add", target, content);
            Ok(Value::Null)
        }
        fn replace(&self, target: &str, _old: &str, content: &str) -> Result<Value, String> {
            self.on_memory_write("replace", target, content);
            Ok(Value::Null)
        }
        fn remove(&self, target: &str, old: &str) -> Result<Value, String> {
            self.on_memory_write("remove", target, old);
            Ok(Value::Null)
        }
        fn on_memory_write(&self, action: &str, target: &str, content: &str) {
            self.writes
                .lock()
                .unwrap()
                .push((action.into(), target.into(), content.into()));
        }
    }

    #[test]
    fn alternative_provider_receives_on_memory_write() {
        let p = RecordingProvider::default();
        let _ = p.add("memory", "hello");
        let _ = p.replace("memory", "hello", "world");
        let _ = p.remove("pitfalls", "world");

        let writes = p.writes.lock().unwrap();
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[0], ("add".into(), "memory".into(), "hello".into()));
        assert_eq!(
            writes[1],
            ("replace".into(), "memory".into(), "world".into())
        );
        assert_eq!(
            writes[2],
            ("remove".into(), "pitfalls".into(), "world".into())
        );
    }

    /// dirge-7tvq — the augmentation logic that wraps a provider's
    /// `on_pre_compress` output into the compression `instructions`
    /// parameter must (a) call the hook with the transcript, (b)
    /// fold non-empty output in, and (c) leave existing user
    /// instructions intact.
    #[test]
    fn on_pre_compress_output_threads_into_instructions() {
        #[derive(Default)]
        struct InsightProvider {
            saw_transcript: Mutex<Option<String>>,
        }
        impl MemoryProvider for InsightProvider {
            fn name(&self) -> &str {
                "insight"
            }
            fn view(&self, _: &str) -> Value {
                Value::Null
            }
            fn add(&self, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn replace(&self, _: &str, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn remove(&self, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn on_pre_compress(&self, transcript: &str) -> String {
                *self.saw_transcript.lock().unwrap() = Some(transcript.to_string());
                "REMEMBER: project uses cargo not bazel".into()
            }
        }
        let p = InsightProvider::default();

        // Hook fires with the transcript verbatim.
        let extra = p.on_pre_compress("turn 1 transcript");
        assert_eq!(extra, "REMEMBER: project uses cargo not bazel");
        assert_eq!(
            p.saw_transcript.lock().unwrap().as_deref(),
            Some("turn 1 transcript"),
            "hook must receive the pre-compress transcript verbatim"
        );
    }

    /// dirge-7tvq — `on_session_end` receives the live-session
    /// transcript exactly once per session-swap.
    #[test]
    fn on_session_end_fires_with_transcript() {
        #[derive(Default)]
        struct EndProvider {
            ends: Mutex<Vec<String>>,
        }
        impl MemoryProvider for EndProvider {
            fn name(&self) -> &str {
                "end"
            }
            fn view(&self, _: &str) -> Value {
                Value::Null
            }
            fn add(&self, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn replace(&self, _: &str, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn remove(&self, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn on_session_end(&self, transcript: &str) {
                self.ends.lock().unwrap().push(transcript.to_string());
            }
        }
        let p = EndProvider::default();
        p.on_session_end("User: hi\n\nAssistant: hello\n");
        let ends = p.ends.lock().unwrap();
        assert_eq!(ends.len(), 1, "exactly one end-of-session fire");
        assert!(
            ends[0].contains("User: hi") && ends[0].contains("Assistant: hello"),
            "transcript must contain user + assistant turns: {:?}",
            ends[0]
        );
    }

    #[test]
    fn alternative_provider_default_hooks_are_no_ops() {
        // A provider that overrides only the CRUD methods doesn't
        // need to think about session-end, pre-compress, etc.
        struct MinimalProvider;
        impl MemoryProvider for MinimalProvider {
            fn name(&self) -> &str {
                "minimal"
            }
            fn view(&self, _: &str) -> Value {
                Value::Null
            }
            fn add(&self, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn replace(&self, _: &str, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
            fn remove(&self, _: &str, _: &str) -> Result<Value, String> {
                Ok(Value::Null)
            }
        }
        let p = MinimalProvider;
        // None of these should panic or require an impl.
        p.on_session_end("transcript");
        assert_eq!(p.on_pre_compress("anything"), "");
        p.on_memory_write("add", "memory", "x");
    }

    #[test]
    fn builtin_store_implements_trait_and_routes_through_on_write() {
        use crate::extras::dirge_paths::ProjectPaths;
        let dir = std::env::temp_dir().join(format!(
            "dirge-memprovider-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let paths = ProjectPaths::new(&dir);
        let store = super::super::memory_store::MemoryToolStore::load(&paths).unwrap();

        // Call through the trait — proves the impl forwards.
        let provider: &dyn MemoryProvider = &store;
        assert_eq!(provider.name(), "builtin");
        let resp = provider.add("memory", "trait-routed entry").unwrap();
        assert_eq!(resp["success"], true);

        let view = provider.view("memory");
        let entries = view["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| {
            e.as_str()
                .map(|s| s.contains("trait-routed"))
                .unwrap_or(false)
        }));

        std::fs::remove_dir_all(&dir).ok();
    }
}
