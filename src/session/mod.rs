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

impl SessionTree {
    /// Walk from `leaf_id` to root, returning the chain of ids in
    /// chronological order (root first, leaf last). Used to confirm
    /// the tree's current path matches `Session::messages`.
    ///
    /// Returns an empty Vec if `leaf_id` is None or any link is
    /// broken (defensive — a healthy tree always reconstructs).
    pub fn path_from_leaf(&self) -> Vec<&TreeNode> {
        let mut path = Vec::new();
        let mut cursor = self.leaf_id.as_ref();
        while let Some(id) = cursor {
            let Some(node) = self.entries.get(id) else {
                return Vec::new();
            };
            path.push(node);
            cursor = node.parent.as_ref();
        }
        path.reverse();
        path
    }
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
        // Make sure the tree mirrors any messages that were loaded
        // from a pre-P4b session file BEFORE we append the new one —
        // otherwise the rebuild would also re-insert this new
        // message with the wrong parent.
        self.ensure_tree_initialized();
        let tokens = Self::estimate_tokens(content);
        let id = new_message_id();
        let timestamp = chrono::Utc::now().timestamp();
        // Capture the parent NOW, before we touch the leaf — first
        // message in a fresh session has parent=None.
        let parent = self.tree.leaf_id.clone();
        self.messages.push(SessionMessage {
            role,
            content: CompactString::new(content),
            estimated_tokens: tokens,
            id: id.clone(),
            timestamp,
        });
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
        // can prune them from the tree too. Keep the tree consistent
        // with the messages cache.
        let dropped_ids: Vec<CompactString> = self.messages[..first_kept_index]
            .iter()
            .map(|m| m.id.clone())
            .collect();

        // Remove summarized messages and insert summary
        self.messages.drain(..first_kept_index);
        self.messages.insert(0, summary_msg);

        // Mirror into the tree: remove dropped nodes, insert the new
        // summary node as the new root, and re-parent the first
        // kept node to point at the summary.
        self.ensure_tree_initialized();
        for id in &dropped_ids {
            self.tree.entries.remove(id);
        }
        let new_root = TreeNode {
            id: summary_id.clone(),
            parent: None,
            timestamp: summary_ts,
            label: None,
        };
        self.tree.entries.insert(summary_id.clone(), new_root);
        // The new "first kept" message (index 1 in the cache, after
        // the summary at index 0) becomes the summary's child. If
        // every prior message was dropped, the summary is also the
        // new leaf.
        if let Some(first_kept) = self.messages.get(1) {
            if let Some(node) = self.tree.entries.get_mut(&first_kept.id) {
                node.parent = Some(summary_id.clone());
            }
        } else {
            self.tree.leaf_id = Some(summary_id);
        }

        self.compactions.push(Compaction {
            summary: CompactString::from(summary),
            first_kept_index: 1, // The summary is at index 0
            summarized_count,
            token_savings,
            created_at: CompactString::new(chrono::Utc::now().to_rfc3339()),
        });

        // Adjust all compaction first_kept indices for the removed messages
        // (since we never have >1 compaction with the current simple approach, this is fine)
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

    /// `path_from_leaf` walks back to root and reports the chain in
    /// chronological order. Matches the linear `messages` Vec ordering.
    #[test]
    fn path_from_leaf_matches_messages_order() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "a");
        s.add_message(MessageRole::Assistant, "b");
        s.add_message(MessageRole::User, "c");
        let path = s.tree.path_from_leaf();
        let path_ids: Vec<_> = path.iter().map(|n| n.id.as_str()).collect();
        let msg_ids: Vec<_> = s.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(path_ids, msg_ids);
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
}
