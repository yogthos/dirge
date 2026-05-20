pub mod storage;

use std::collections::HashMap;

use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    pub role: MessageRole,
    pub content: CompactString,
    pub estimated_tokens: u64,
    /// Per-message unique id. Defaulted on deserialize so existing
    /// session files load without migration — they get fresh UUIDs.
    /// Used by P4b to address messages in a node-based session tree;
    /// today's consumers can ignore it.
    #[serde(default = "new_message_id")]
    pub id: CompactString,
    /// Epoch seconds when the message was added. Defaulted to 0 on
    /// deserialize for backward compat; new messages get
    /// `chrono::Utc::now().timestamp()`. Used by the UI to interleave
    /// chat messages with plugin entries by timestamp.
    #[serde(default)]
    pub timestamp: i64,
}

/// Generate a fresh message id. Extracted for `#[serde(default = ...)]`.
fn new_message_id() -> CompactString {
    CompactString::new(Uuid::new_v4().to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    pub summary: CompactString,
    pub first_kept_index: usize,
    pub summarized_count: usize,
    pub token_savings: u64,
    pub created_at: CompactString,
}

/// Single node in the session tree. References a `SessionMessage` by
/// `id` (the id lives both here and on the message itself; we keep
/// the duplication minimal but it gives the tree a self-contained
/// identity if we ever want to detach content).
///
/// `parent` is None for the root node; otherwise it's the previous
/// node on the current branch. `label` is an optional bookmark set
/// via the future `harness/set-label` (P4d) — None for unlabeled
/// nodes, which is the common case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeNode {
    pub id: CompactString,
    pub parent: Option<CompactString>,
    pub timestamp: i64,
    #[serde(default)]
    pub label: Option<String>,
}

/// Node-based session storage. `entries` is keyed by node id; each
/// `SessionMessage` in `Session::messages` has a corresponding entry
/// in `tree.entries` with the same id.
///
/// For the current linear-only use case, `entries` mirrors `messages`
/// as a degenerate chain: root → second → … → leaf. P4c (fork/clone)
/// will introduce branches by letting alternate paths share a parent.
///
/// `leaf_id` points at the current end of the active branch. When new
/// messages are appended (`add_message`), they extend from `leaf_id`
/// and the leaf advances. Forks (P4c) will switch `leaf_id` to a
/// different branch without disturbing the entries map.
///
/// Defaults to empty (`leaf_id = None`, no entries) so pre-P4b
/// session JSON loads cleanly via the serde defaults; `Session::new`
/// + `add_message` initialize it correctly on subsequent appends.
/// Legacy linear sessions are auto-converted on first access via
/// `Session::ensure_tree_initialized`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionTree {
    #[serde(default)]
    pub entries: HashMap<CompactString, TreeNode>,
    #[serde(default)]
    pub leaf_id: Option<CompactString>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionAllowEntry {
    pub tool: String,
    pub pattern: String,
}

/// One plugin-appended entry on the session timeline. Data is treated as
/// opaque by the host — the plugin chose its own format (JSON string,
/// plain text, whatever) and any registered renderer for `custom_type`
/// is responsible for turning it into displayable lines. The host's
/// fallback renderer just dumps the raw data dim.
///
/// `seq` is the host-assigned insertion order; combined with `timestamp`
/// it provides a stable rendering order even when many entries land in
/// the same second.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    pub custom_type: String,
    pub data: String,
    /// Whether to render this entry in the chat (false = silent;
    /// useful for persistent state that shouldn't visually clutter).
    pub display: bool,
    /// Epoch seconds at the time of append.
    pub timestamp: i64,
    /// Monotonic per-session insertion order.
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: CompactString,
    pub name: CompactString,
    pub messages: Vec<SessionMessage>,
    pub compactions: Vec<Compaction>,
    pub created_at: CompactString,
    pub updated_at: CompactString,
    pub total_tokens: u64,
    pub total_cost: f64,
    pub total_estimated_tokens: u64,
    pub context_window: u64,
    pub model: CompactString,
    pub provider: CompactString,
    pub working_dir: CompactString,
    #[serde(default)]
    pub permission_allowlist: Vec<PermissionAllowEntry>,
    /// Plugin-appended entries (bookmarks, telemetry, custom state)
    /// that survive session save/load. Defaulted on deserialize so
    /// pre-P2 session files load without migration.
    #[serde(default)]
    pub extra_entries: Vec<PluginEntry>,
    /// Counter for `PluginEntry::seq`. Defaulted on deserialize for
    /// backward compat; we initialize from `extra_entries.len()` on
    /// load so new appends don't collide with existing seq values.
    #[serde(default)]
    pub next_entry_seq: u64,
    /// Node-based mirror of `messages` enabling future fork/clone /
    /// branch navigation. Each message has a corresponding entry in
    /// `tree.entries` with parent links pointing at the previous
    /// message on the current branch. Defaulted on deserialize for
    /// pre-P4b session files; `ensure_tree_initialized()` rebuilds
    /// the linear chain from `messages` when the loaded tree is
    /// empty but messages aren't.
    #[serde(default)]
    pub tree: SessionTree,
    /// Content store for every `SessionMessage` ever appended to this
    /// session, keyed by message id. `messages` is the projection of
    /// the *current* branch's path; `message_store` keeps content
    /// alive for branches the user isn't currently viewing so
    /// `switch_to_leaf` can re-derive the path. Defaulted on
    /// deserialize for backward compat; `ensure_message_store_initialized`
    /// populates it from `messages` for legacy session files.
    #[serde(default)]
    pub message_store: HashMap<CompactString, SessionMessage>,
}

impl Session {
    pub fn estimate_tokens(text: &str) -> u64 {
        (text.len() as u64 / 4).max(1)
    }

    pub fn new(provider: &str, model: &str, context_window: u64) -> Self {
        let now = CompactString::new(chrono::Utc::now().to_rfc3339());
        Session {
            id: CompactString::new(Uuid::new_v4().to_string()),
            name: CompactString::new(""),
            messages: Vec::new(),
            compactions: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            total_tokens: 0,
            total_cost: 0.0,
            total_estimated_tokens: 0,
            context_window,
            model: CompactString::new(model),
            provider: CompactString::new(provider),
            working_dir: std::env::current_dir()
                .map(|p| CompactString::new(p.to_string_lossy()))
                .unwrap_or_default(),
            permission_allowlist: Vec::new(),
            extra_entries: Vec::new(),
            next_entry_seq: 0,
            tree: SessionTree::default(),
            message_store: HashMap::new(),
        }
    }

    /// Populate `message_store` from `messages` for legacy session
    /// files that were saved before P4c added the per-id content map.
    /// Idempotent.
    pub fn ensure_message_store_initialized(&mut self) {
        if !self.message_store.is_empty() {
            return;
        }
        for msg in &self.messages {
            self.message_store.insert(msg.id.clone(), msg.clone());
        }
    }

    /// If this session was loaded from a pre-P4b file (or any file
    /// where `tree.entries` is empty but `messages` isn't), build a
    /// linear tree from `messages` so all subsequent appends extend
    /// from the correct leaf. Idempotent and safe to call repeatedly.
    pub fn ensure_tree_initialized(&mut self) {
        if !self.tree.entries.is_empty() || self.messages.is_empty() {
            return;
        }
        let mut prev: Option<CompactString> = None;
        for msg in &self.messages {
            let node = TreeNode {
                id: msg.id.clone(),
                parent: prev.clone(),
                timestamp: msg.timestamp,
                label: None,
            };
            prev = Some(msg.id.clone());
            self.tree.entries.insert(msg.id.clone(), node);
        }
        self.tree.leaf_id = prev;
    }

    /// Run both back-compat initializers as a unit. Use this instead
    /// of calling `ensure_message_store_initialized` and
    /// `ensure_tree_initialized` separately — they're individually
    /// idempotent but the combined invariant ("tree + store both
    /// reflect `messages`") is what every mutation actually depends
    /// on. A panic between two separate calls would leave the
    /// session half-initialized; this helper does both in one shot.
    pub fn ensure_back_compat_initialized(&mut self) {
        self.ensure_message_store_initialized();
        self.ensure_tree_initialized();
    }

    /// Append a plugin entry to this session. Assigns the next
    /// monotonic `seq` so renderers can produce a deterministic
    /// ordering even within a single-second timestamp bucket. Plugins
    /// reach this via `harness/append-entry` (see PluginManager).
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub fn append_plugin_entry(
        &mut self,
        custom_type: impl Into<String>,
        data: impl Into<String>,
        display: bool,
    ) -> &PluginEntry {
        let entry = PluginEntry {
            custom_type: custom_type.into(),
            data: data.into(),
            display,
            timestamp: chrono::Utc::now().timestamp(),
            seq: self.next_entry_seq,
        };
        self.next_entry_seq = self.next_entry_seq.saturating_add(1);
        self.extra_entries.push(entry);
        self.extra_entries.last().expect("just pushed")
    }

    pub fn add_message(&mut self, role: MessageRole, content: &str) {
        // Make sure tree + store mirror any messages that were loaded
        // from a pre-P4b/P4c session file BEFORE we append the new
        // one — otherwise the rebuild would re-insert this new message
        // with the wrong parent.
        self.ensure_back_compat_initialized();
        let tokens = Self::estimate_tokens(content);
        let id = new_message_id();
        let timestamp = chrono::Utc::now().timestamp();
        // Capture the parent NOW, before we touch the leaf — first
        // message in a fresh session has parent=None.
        let parent = self.tree.leaf_id.clone();
        let msg = SessionMessage {
            role,
            content: CompactString::new(content),
            estimated_tokens: tokens,
            id: id.clone(),
            timestamp,
        };
        self.messages.push(msg.clone());
        self.message_store.insert(id.clone(), msg);
        // Mirror into the tree as a node extending from the previous
        // leaf. The first message in a fresh session gets parent=None.
        self.tree.entries.insert(
            id.clone(),
            TreeNode {
                id: id.clone(),
                parent,
                timestamp,
                label: None,
            },
        );
        self.tree.leaf_id = Some(id);
        self.total_estimated_tokens = self.total_estimated_tokens.saturating_add(tokens);
        self.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
    }

    /// Pop the most recent message off the current branch. Used by
    /// `/undo`. Removes from `messages`, `message_store`, and the
    /// tree (entry + leaf rewind). Returns the popped message so the
    /// caller can compute the token rebate.
    ///
    /// Tree pruning: a popped node is only removed from `tree.entries`
    /// if no other node lists it as a parent — that way an active
    /// fork's children stay reachable. In the linear case it's
    /// always safe to remove.
    pub fn pop_last_message(&mut self) -> Option<SessionMessage> {
        self.ensure_back_compat_initialized();
        let msg = self.messages.pop()?;
        // Pull the popped node's parent for leaf rewind. If the tree
        // somehow lacks this node (corruption / external mutation),
        // fall back to the previous message in the linear cache rather
        // than wiping the leaf — wiping would leave the tree dangling
        // when the user pops on a branched session.
        let parent = match self.tree.entries.get(&msg.id) {
            Some(node) => node.parent.clone(),
            None => self.messages.last().map(|m| m.id.clone()),
        };
        self.tree.leaf_id = parent;
        // Only prune the node if nothing else (e.g. a forked branch)
        // refers to it as a parent.
        let still_referenced = self
            .tree
            .entries
            .values()
            .any(|n| n.parent.as_ref() == Some(&msg.id));
        if !still_referenced {
            self.tree.entries.remove(&msg.id);
            self.message_store.remove(&msg.id);
        }
        self.total_estimated_tokens = self
            .total_estimated_tokens
            .saturating_sub(msg.estimated_tokens);
        self.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
        Some(msg)
    }

    /// Switch the active branch to the one ending at `new_leaf_id`.
    /// Rebuilds the `messages` cache by walking from the new leaf
    /// back to root via `tree.entries` and looking up each node's
    /// content in `message_store`.
    ///
    /// Returns Err if `new_leaf_id` isn't in the tree, or if any
    /// node along the path is missing from `message_store` (which
    /// would indicate corruption). On error, leaves the session
    /// state untouched.
    pub fn switch_to_leaf(&mut self, new_leaf_id: &CompactString) -> Result<(), String> {
        self.ensure_back_compat_initialized();
        if !self.tree.entries.contains_key(new_leaf_id) {
            return Err(format!("unknown entry id: {}", new_leaf_id));
        }
        // Walk back to root, collecting IDs.
        let mut chain: Vec<CompactString> = Vec::new();
        let mut cursor: Option<CompactString> = Some(new_leaf_id.clone());
        while let Some(id) = cursor {
            let node = self
                .tree
                .entries
                .get(&id)
                .ok_or_else(|| format!("broken parent link to missing node {}", id))?;
            cursor = node.parent.clone();
            chain.push(id);
        }
        chain.reverse();
        // Validate every node has content before we mutate.
        for id in &chain {
            if !self.message_store.contains_key(id) {
                return Err(format!("missing content for node {}", id));
            }
        }
        // Now rebuild messages + recompute estimated tokens.
        let new_messages: Vec<SessionMessage> = chain
            .iter()
            .map(|id| self.message_store[id].clone())
            .collect();
        let new_total: u64 = new_messages.iter().map(|m| m.estimated_tokens).sum();
        self.messages = new_messages;
        self.total_estimated_tokens = new_total;
        self.tree.leaf_id = Some(new_leaf_id.clone());
        self.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
        Ok(())
    }

    /// Fork the session at `entry_id`. Sets the active leaf to that
    /// entry's *parent* — i.e. position the user just before the
    /// chosen message so the next add_message creates a divergent
    /// branch. Returns the message content (so the UI can restore
    /// it into the input editor for re-editing).
    ///
    /// **Root-node behaviour**: if `entry_id` has no parent (it is
    /// the conversation root), the current `messages` cache is
    /// cleared and `tree.leaf_id` is set to `None` so the next
    /// `add_message` starts a fresh root. The tree's other entries
    /// (sibling branches) are *not* pruned — they remain reachable
    /// via `/tree`.
    ///
    /// Mirrors pi's `ctx.fork(entryId, { position: "before" })`.
    pub fn fork_at(&mut self, entry_id: &CompactString) -> Result<SessionMessage, String> {
        self.ensure_back_compat_initialized();
        let node = self
            .tree
            .entries
            .get(entry_id)
            .ok_or_else(|| format!("unknown entry id: {}", entry_id))?;
        let parent = node.parent.clone();
        let original = self
            .message_store
            .get(entry_id)
            .cloned()
            .ok_or_else(|| format!("missing content for entry {}", entry_id))?;
        match parent {
            Some(parent_id) => {
                self.switch_to_leaf(&parent_id)?;
            }
            None => {
                // Forking at the root: empty current branch entirely.
                self.messages.clear();
                self.total_estimated_tokens = 0;
                self.tree.leaf_id = None;
                self.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
            }
        }
        Ok(original)
    }

    /// Clone the path through `entry_id`: switch the active leaf to
    /// that entry without removing or restoring anything else.
    /// Mirrors pi's `ctx.fork(entryId, { position: "at" })`.
    pub fn clone_at(&mut self, entry_id: &CompactString) -> Result<(), String> {
        self.switch_to_leaf(entry_id)
    }

    /// Set or clear a label on a tree node. Used by
    /// `harness/set-label` (P4d) and by `/bookmark`-style commands.
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub fn set_label(
        &mut self,
        entry_id: &CompactString,
        label: Option<String>,
    ) -> Result<(), String> {
        // Mirror the other mutation methods — keep tree + store in
        // lockstep even though set_label only touches the tree, in
        // case a future label-aware code path inspects the store.
        self.ensure_back_compat_initialized();
        let node = self
            .tree
            .entries
            .get_mut(entry_id)
            .ok_or_else(|| format!("unknown entry id: {}", entry_id))?;
        node.label = label;
        self.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
        Ok(())
    }

    /// Reset the session in place: assign a fresh id, clear messages,
    /// tree, message store, compactions, plugin entries, and counters.
    /// Preserves model/provider/context_window/working_dir so the
    /// caller doesn't have to rebuild the agent. Used by
    /// `harness/new-session` (P4d) — mirrors pi's `ctx.newSession()`
    /// without the file-replacement step (dirge persists in place).
    ///
    /// If `parent_session` is provided, the previous session id is
    /// recorded as `name` for lineage; pi stores this in a session
    /// header field. We piggyback on `name` to avoid bumping the
    /// session schema for one optional field.
    #[cfg_attr(not(feature = "plugin"), allow(dead_code))]
    pub fn reset_to_new(&mut self, parent_session: Option<&str>) {
        let now = CompactString::new(chrono::Utc::now().to_rfc3339());
        self.id = CompactString::new(Uuid::new_v4().to_string());
        if let Some(parent) = parent_session {
            self.name = CompactString::new(format!("parent:{}", parent));
        } else {
            self.name = CompactString::new("");
        }
        self.messages.clear();
        self.compactions.clear();
        self.extra_entries.clear();
        self.next_entry_seq = 0;
        self.message_store.clear();
        self.tree = SessionTree::default();
        self.total_tokens = 0;
        self.total_cost = 0.0;
        self.total_estimated_tokens = 0;
        self.created_at = now.clone();
        self.updated_at = now;
        // Note: model/provider/context_window/working_dir/permission_allowlist
        // preserved so the host can keep the same agent runtime.
    }

    pub fn needs_compaction(&self, reserve_tokens: u64) -> bool {
        if self.context_window == 0 {
            return false;
        }
        self.total_estimated_tokens > self.context_window.saturating_sub(reserve_tokens)
    }

    pub fn compacted_context(&self) -> (Option<&str>, usize) {
        match self.compactions.last() {
            Some(c) => (Some(c.summary.as_str()), c.first_kept_index),
            None => (None, 0),
        }
    }

    pub fn compress(&mut self, summary: String, first_kept_index: usize, token_savings: u64) {
        // Bounds check — callers compute `first_kept_index` from a
        // reverse-scan of `messages` so it should always be in range,
        // but a buggy/racy caller could pass out-of-bounds. Clamp
        // rather than panic on `drain(..)` so a misuse degrades to
        // "summarize everything" instead of a hard crash.
        let first_kept_index = first_kept_index.min(self.messages.len());
        let summarized_count = first_kept_index;

        // Subtract the saved tokens from estimated total
        self.total_estimated_tokens = self.total_estimated_tokens.saturating_sub(token_savings);
        // Add back estimated tokens for the summary itself
        let summary_tokens = Self::estimate_tokens(&summary);
        self.total_estimated_tokens = self.total_estimated_tokens.saturating_add(summary_tokens);

        // Insert a System message with the summary at the boundary
        let summary_id = new_message_id();
        let summary_ts = chrono::Utc::now().timestamp();
        let summary_msg = SessionMessage {
            role: MessageRole::System,
            content: CompactString::from(summary.clone()),
            estimated_tokens: summary_tokens,
            id: summary_id.clone(),
            timestamp: summary_ts,
        };

        // Collect the IDs of the messages we're about to drop so we
        // can prune them from the tree and store too.
        let dropped_ids: Vec<CompactString> = self.messages[..first_kept_index]
            .iter()
            .map(|m| m.id.clone())
            .collect();
        let dropped_set: std::collections::HashSet<CompactString> =
            dropped_ids.iter().cloned().collect();

        // Remove summarized messages and insert summary
        self.messages.drain(..first_kept_index);
        self.messages.insert(0, summary_msg.clone());

        // Mirror into the tree + store: remove dropped nodes, insert
        // the new summary node as the new root, and re-parent the
        // first kept node to point at the summary.
        self.ensure_back_compat_initialized();
        for id in &dropped_ids {
            self.tree.entries.remove(id);
            self.message_store.remove(id);
        }
        let new_root = TreeNode {
            id: summary_id.clone(),
            parent: None,
            timestamp: summary_ts,
            label: None,
        };
        self.tree.entries.insert(summary_id.clone(), new_root);
        self.message_store.insert(summary_id.clone(), summary_msg);
        // The new "first kept" message (index 1 in the cache, after
        // the summary at index 0) becomes the summary's child. If
        // every prior message was dropped, the summary is also the
        // new leaf.
        if let Some(first_kept) = self.messages.get(1) {
            if let Some(node) = self.tree.entries.get_mut(&first_kept.id) {
                node.parent = Some(summary_id.clone());
            }
        } else {
            self.tree.leaf_id = Some(summary_id.clone());
        }
        // Re-point the tree leaf if the previous leaf was one of the
        // pruned nodes (e.g. a branched session compressed the branch
        // that owned the leaf). Without this the leaf dangles at an
        // id that no longer exists in `tree.entries`.
        let leaf_dropped = self
            .tree
            .leaf_id
            .as_ref()
            .map(|id| dropped_set.contains(id))
            .unwrap_or(false);
        if leaf_dropped {
            // Anchor the leaf to the new first-kept message, or to the
            // summary if nothing else survived.
            self.tree.leaf_id = self
                .messages
                .get(1)
                .map(|m| m.id.clone())
                .or(Some(summary_id.clone()));
        }

        // On compress we replace every prior compaction record with a
        // single fresh one. `Compaction::first_kept_index` is meant to
        // mark the message-index boundary for the *latest* compaction
        // only — keeping a stale list of records from earlier compresses
        // makes their indices meaningless after subsequent drains. The
        // latest summary IS the conversation prefix; older summaries
        // are folded into it via `previous_summary` in the LLM context.
        self.compactions.clear();
        self.compactions.push(Compaction {
            summary: CompactString::from(summary),
            first_kept_index: 1, // The summary is at index 0
            summarized_count,
            token_savings,
            created_at: CompactString::new(chrono::Utc::now().to_rfc3339()),
        });

        self.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// New messages get a fresh UUID and a non-zero timestamp. P4a's
    /// whole point.
    #[test]
    fn add_message_populates_id_and_timestamp() {
        let mut s = Session::new("openrouter", "test/model", 0);
        s.add_message(MessageRole::User, "hi");
        let m = &s.messages[0];
        assert!(!m.id.is_empty(), "id should be assigned");
        // The id has UUID v4 shape: 36 chars with hyphens.
        assert_eq!(m.id.chars().count(), 36);
        assert_eq!(m.id.matches('-').count(), 4);
        // Timestamp should be roughly "now" (within a couple of
        // seconds of the test). Anything > 1700000000 (Nov 2023) means
        // chrono::Utc::now() actually ran.
        assert!(m.timestamp > 1_700_000_000, "got {}", m.timestamp);
    }

    /// Each message gets a distinct id, so consumers can address them
    /// uniquely. Important for P4b's parent-link addressing.
    #[test]
    fn each_message_gets_a_unique_id() {
        let mut s = Session::new("p", "m", 0);
        for i in 0..50 {
            s.add_message(MessageRole::User, &format!("msg {i}"));
        }
        let mut ids: Vec<_> = s
            .messages
            .iter()
            .map(|m| m.id.as_str().to_string())
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 50, "ids should be unique across 50 messages");
    }

    /// Pre-P4a session JSON (no `id`, no `timestamp`) deserializes
    /// without error and gets a fresh id + zero timestamp by default.
    /// Critical for backward compat — users have existing sessions.
    #[test]
    fn legacy_session_json_loads_with_defaults() {
        // Note: no `id` or `timestamp` keys on the message.
        let legacy = r#"{
            "id": "abc",
            "name": "",
            "messages": [{
                "role": "user",
                "content": "hi",
                "estimated_tokens": 1
            }],
            "compactions": [],
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "total_tokens": 0,
            "total_cost": 0.0,
            "total_estimated_tokens": 1,
            "context_window": 0,
            "model": "x",
            "provider": "p",
            "working_dir": ""
        }"#;
        let s: Session = serde_json::from_str(legacy).expect("legacy session should load");
        assert_eq!(s.messages.len(), 1);
        let m = &s.messages[0];
        // serde default fired — id gets a fresh UUID, timestamp gets 0.
        assert_eq!(m.id.chars().count(), 36);
        assert_eq!(m.timestamp, 0);
        // Other fields preserved.
        assert_eq!(m.content, "hi");
    }

    /// Modern session JSON with id+timestamp serializes and round-trips
    /// without losing either field.
    #[test]
    fn session_serde_roundtrip_preserves_id_and_timestamp() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "hello");
        let original_id = s.messages[0].id.clone();
        let original_ts = s.messages[0].timestamp;
        let serialized = serde_json::to_string(&s).unwrap();
        let restored: Session = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored.messages[0].id, original_id);
        assert_eq!(restored.messages[0].timestamp, original_ts);
    }

    /// `compress` inserts a synthetic system summary message; it must
    /// also get a fresh id + current timestamp like any other message.
    #[test]
    fn compress_summary_message_has_id_and_timestamp() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "earlier");
        s.add_message(MessageRole::Assistant, "reply");
        s.compress("compacted context".to_string(), 2, 10);
        // After compress, index 0 is the summary system message.
        let m = &s.messages[0];
        assert!(matches!(m.role, MessageRole::System));
        assert_eq!(m.id.chars().count(), 36);
        assert!(m.timestamp > 0);
    }

    // --- P4b: tree storage --------------------------------------------

    /// A fresh session has no tree entries; `add_message` builds the
    /// chain, with each node's parent pointing at the previous one.
    #[test]
    fn add_message_extends_tree_chain() {
        let mut s = Session::new("p", "m", 0);
        assert!(s.tree.entries.is_empty());
        assert!(s.tree.leaf_id.is_none());

        s.add_message(MessageRole::User, "first");
        let first_id = s.messages[0].id.clone();
        assert_eq!(s.tree.entries.len(), 1);
        assert_eq!(s.tree.leaf_id.as_ref(), Some(&first_id));
        assert_eq!(s.tree.entries[&first_id].parent, None);

        s.add_message(MessageRole::Assistant, "second");
        let second_id = s.messages[1].id.clone();
        assert_eq!(s.tree.entries.len(), 2);
        assert_eq!(s.tree.leaf_id.as_ref(), Some(&second_id));
        // Second node's parent is the first node.
        assert_eq!(s.tree.entries[&second_id].parent.as_ref(), Some(&first_id));
    }

    /// Pre-P4b session JSON (no `tree` field) deserializes with an
    /// empty tree; `ensure_tree_initialized` rebuilds the linear
    /// chain from `messages` on first access. Critical backward
    /// compat — users have existing sessions on disk.
    #[test]
    fn legacy_session_initializes_tree_from_messages() {
        // Pre-P4b session JSON has no `tree` key.
        let legacy = r#"{
            "id": "abc",
            "name": "",
            "messages": [
                {"role": "user", "content": "a", "estimated_tokens": 1,
                 "id": "msg-1", "timestamp": 100},
                {"role": "assistant", "content": "b", "estimated_tokens": 1,
                 "id": "msg-2", "timestamp": 101}
            ],
            "compactions": [],
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "total_tokens": 0,
            "total_cost": 0.0,
            "total_estimated_tokens": 2,
            "context_window": 0,
            "model": "x",
            "provider": "p",
            "working_dir": ""
        }"#;
        let mut s: Session = serde_json::from_str(legacy).unwrap();
        assert!(s.tree.entries.is_empty(), "tree should default empty");
        // The first call to ensure_tree_initialized builds the chain.
        s.ensure_tree_initialized();
        assert_eq!(s.tree.entries.len(), 2);
        assert_eq!(s.tree.leaf_id.as_deref(), Some("msg-2"));
        assert_eq!(s.tree.entries["msg-1"].parent, None);
        assert_eq!(s.tree.entries["msg-2"].parent.as_deref(), Some("msg-1"));
    }

    /// Modern session JSON serializes both `messages` AND `tree`,
    /// and the tree round-trips intact.
    #[test]
    fn session_serde_roundtrip_preserves_tree() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "hi");
        s.add_message(MessageRole::Assistant, "yo");
        let orig_leaf = s.tree.leaf_id.clone();
        let serialized = serde_json::to_string(&s).unwrap();
        let restored: Session = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored.tree.entries.len(), 2);
        assert_eq!(restored.tree.leaf_id, orig_leaf);
    }

    /// `compress` prunes the dropped messages from the tree, inserts
    /// the new summary as the root, and re-parents the first kept
    /// message to point at the summary.
    #[test]
    fn compress_rebuilds_tree_with_summary_root() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "ancient-1");
        s.add_message(MessageRole::Assistant, "ancient-2");
        s.add_message(MessageRole::User, "kept");
        // Compress the first two messages.
        s.compress("compacted".to_string(), 2, 10);
        // The kept message and the new summary are the only nodes.
        assert_eq!(s.tree.entries.len(), 2);
        let summary_id = s.messages[0].id.clone();
        let kept_id = s.messages[1].id.clone();
        // Summary is the new root.
        assert_eq!(s.tree.entries[&summary_id].parent, None);
        // Kept message points at the summary.
        assert_eq!(s.tree.entries[&kept_id].parent.as_ref(), Some(&summary_id));
        // Leaf is the kept message.
        assert_eq!(s.tree.leaf_id.as_ref(), Some(&kept_id));
    }

    /// Repeated `ensure_tree_initialized` calls are no-ops once
    /// initialized — idempotent and cheap.
    #[test]
    fn ensure_tree_initialized_is_idempotent() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "msg");
        let snapshot = s.tree.entries.len();
        let snapshot_leaf = s.tree.leaf_id.clone();
        s.ensure_tree_initialized();
        s.ensure_tree_initialized();
        s.ensure_tree_initialized();
        assert_eq!(s.tree.entries.len(), snapshot);
        assert_eq!(s.tree.leaf_id, snapshot_leaf);
    }

    // --- P4c: message store + branch operations -----------------------

    /// `add_message` populates the message_store keyed by id so
    /// every message's content survives branch switches.
    #[test]
    fn add_message_populates_message_store() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "hello");
        s.add_message(MessageRole::Assistant, "world");
        assert_eq!(s.message_store.len(), 2);
        let first_id = s.messages[0].id.clone();
        assert_eq!(s.message_store[&first_id].content, "hello");
    }

    /// `pop_last_message` removes from messages, store, and the tree
    /// (since no other branch refers to the popped node yet).
    #[test]
    fn pop_last_message_removes_from_all_three() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "a");
        s.add_message(MessageRole::Assistant, "b");
        let popped_id = s.messages[1].id.clone();
        let popped = s.pop_last_message().unwrap();
        assert_eq!(popped.content, "b");
        assert_eq!(s.messages.len(), 1);
        assert!(!s.message_store.contains_key(&popped_id));
        assert!(!s.tree.entries.contains_key(&popped_id));
        // Leaf moved back.
        assert_eq!(s.tree.leaf_id.as_ref(), Some(&s.messages[0].id));
    }

    /// Forking at an entry that has another active child preserves
    /// the original branch's content in the store, so the user can
    /// `switch_to_leaf` back to it later.
    #[test]
    fn fork_at_preserves_original_branch_content() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "ask-1");
        s.add_message(MessageRole::Assistant, "ans-1");
        let original_leaf = s.tree.leaf_id.clone().unwrap();
        // Fork at the assistant message — moves leaf to its parent (the
        // user message), so the next add_message creates a sibling
        // branch starting from that user message.
        let assistant_id = s.messages[1].id.clone();
        let user_id = s.messages[0].id.clone();
        let original_msg = s.fork_at(&assistant_id).unwrap();
        assert_eq!(original_msg.content, "ans-1");
        // Active path is just the user message now.
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.tree.leaf_id, Some(user_id.clone()));
        // Tree still contains the original assistant node (preserved
        // for switch-back).
        assert!(s.tree.entries.contains_key(&assistant_id));
        assert!(s.message_store.contains_key(&assistant_id));

        // Add a new assistant reply — creates a sibling.
        s.add_message(MessageRole::Assistant, "ans-2-alternate");
        let new_leaf = s.tree.leaf_id.clone().unwrap();
        assert_ne!(new_leaf, original_leaf);
        // user_id has two children now.
        let children: Vec<_> = s
            .tree
            .entries
            .values()
            .filter(|n| n.parent.as_ref() == Some(&user_id))
            .collect();
        assert_eq!(children.len(), 2);
    }

    /// `switch_to_leaf` rebuilds the messages cache from the new
    /// branch's path, with token accounting recomputed.
    #[test]
    fn switch_to_leaf_rebuilds_messages_and_tokens() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "u1");
        s.add_message(MessageRole::Assistant, "a-original");
        let original_leaf = s.tree.leaf_id.clone().unwrap();
        let user_id = s.messages[0].id.clone();
        s.fork_at(&s.messages[1].id.clone()).unwrap();
        s.add_message(MessageRole::Assistant, "a-alternate-and-much-longer");
        let alt_leaf = s.tree.leaf_id.clone().unwrap();
        let alt_tokens = s.total_estimated_tokens;

        // Switch back to the original branch.
        s.switch_to_leaf(&original_leaf).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[1].content, "a-original");
        assert_eq!(s.tree.leaf_id, Some(original_leaf.clone()));
        let original_tokens = s.total_estimated_tokens;

        // Switch back to the alternate.
        s.switch_to_leaf(&alt_leaf).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[1].content, "a-alternate-and-much-longer");
        assert_eq!(s.total_estimated_tokens, alt_tokens);
        assert_ne!(original_tokens, alt_tokens);
        // user_id is still common ancestor.
        assert_eq!(s.messages[0].id, user_id);
    }

    /// Switching to an unknown leaf surfaces an error without
    /// touching session state.
    #[test]
    fn switch_to_unknown_leaf_returns_err() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "msg");
        let original_leaf = s.tree.leaf_id.clone();
        let bogus = CompactString::new("nonexistent-id");
        let err = s.switch_to_leaf(&bogus).unwrap_err();
        assert!(err.contains("nonexistent-id"), "got: {err}");
        // State unchanged.
        assert_eq!(s.tree.leaf_id, original_leaf);
    }

    /// `clone_at` is `switch_to_leaf` semantically — the active path
    /// becomes the path through the chosen entry, but no editor
    /// restore happens (the caller decides about UI state).
    #[test]
    fn clone_at_is_switch_to_leaf() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "u1");
        s.add_message(MessageRole::Assistant, "a-original");
        let original = s.tree.leaf_id.clone().unwrap();
        s.fork_at(&s.messages[1].id.clone()).unwrap();
        s.add_message(MessageRole::Assistant, "a-alternate");
        // clone_at the original leaf reactivates that branch.
        s.clone_at(&original).unwrap();
        assert_eq!(s.tree.leaf_id, Some(original));
        assert_eq!(s.messages[1].content, "a-original");
    }

    /// `set_label` annotates a tree node with a bookmark string.
    /// Read back via `tree.entries[id].label`.
    #[test]
    fn set_label_attaches_to_node() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "milestone");
        let id = s.messages[0].id.clone();
        s.set_label(&id, Some("checkpoint-1".to_string())).unwrap();
        assert_eq!(s.tree.entries[&id].label.as_deref(), Some("checkpoint-1"));
        // Clearing the label sets it back to None.
        s.set_label(&id, None).unwrap();
        assert_eq!(s.tree.entries[&id].label, None);
    }

    /// Legacy session JSON with messages but no message_store
    /// populates the store on first ensure call. Critical backward
    /// compat — users have existing sessions on disk.
    #[test]
    fn ensure_message_store_initialized_backfills_from_messages() {
        let legacy = r#"{
            "id": "abc",
            "name": "",
            "messages": [
                {"role": "user", "content": "a", "estimated_tokens": 1,
                 "id": "msg-1", "timestamp": 100},
                {"role": "assistant", "content": "b", "estimated_tokens": 1,
                 "id": "msg-2", "timestamp": 101}
            ],
            "compactions": [],
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "total_tokens": 0,
            "total_cost": 0.0,
            "total_estimated_tokens": 2,
            "context_window": 0,
            "model": "x",
            "provider": "p",
            "working_dir": ""
        }"#;
        let mut s: Session = serde_json::from_str(legacy).unwrap();
        assert!(s.message_store.is_empty());
        s.ensure_message_store_initialized();
        assert_eq!(s.message_store.len(), 2);
        assert_eq!(
            s.message_store
                .get(&CompactString::new("msg-1"))
                .map(|m| m.content.as_str()),
            Some("a")
        );
    }

    /// `reset_to_new` keeps model/provider/working_dir but wipes
    /// everything else and assigns a fresh id. Mirrors pi's
    /// newSession() in-place.
    #[test]
    fn reset_to_new_clears_content_but_keeps_runtime_metadata() {
        let mut s = Session::new("openai", "gpt-4", 200_000);
        s.add_message(MessageRole::User, "old prompt");
        s.add_message(MessageRole::Assistant, "old reply");
        s.append_plugin_entry("bookmark", "stale", true);
        s.compactions.push(Compaction {
            summary: CompactString::from("dropped"),
            first_kept_index: 0,
            summarized_count: 1,
            token_savings: 0,
            created_at: CompactString::from("2024-01-01"),
        });
        let original_id = s.id.clone();

        s.reset_to_new(None);

        assert_ne!(s.id, original_id, "id must change");
        assert!(s.messages.is_empty());
        assert!(s.message_store.is_empty());
        assert!(s.tree.entries.is_empty());
        assert!(s.tree.leaf_id.is_none());
        assert!(s.compactions.is_empty());
        assert!(s.extra_entries.is_empty());
        assert_eq!(s.next_entry_seq, 0);
        assert_eq!(s.total_estimated_tokens, 0);
        // Runtime metadata preserved.
        assert_eq!(s.model, "gpt-4");
        assert_eq!(s.provider, "openai");
        assert_eq!(s.context_window, 200_000);
        // Lineage left blank when no parent passed.
        assert_eq!(s.name, "");
    }

    /// When given a parent session id, `reset_to_new` records it
    /// in `name` so the prior session is reachable via session search.
    #[test]
    fn reset_to_new_records_parent_lineage() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "x");
        s.reset_to_new(Some("00000000-prev"));
        assert_eq!(s.name, "parent:00000000-prev");
    }
}
