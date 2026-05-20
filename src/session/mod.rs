pub mod storage;

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
        }
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
        let tokens = Self::estimate_tokens(content);
        self.messages.push(SessionMessage {
            role,
            content: CompactString::new(content),
            estimated_tokens: tokens,
            id: new_message_id(),
            timestamp: chrono::Utc::now().timestamp(),
        });
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
        let summary_msg = SessionMessage {
            role: MessageRole::System,
            content: CompactString::from(summary.clone()),
            estimated_tokens: summary_tokens,
            id: new_message_id(),
            timestamp: chrono::Utc::now().timestamp(),
        };

        // Remove summarized messages and insert summary
        self.messages.drain(..first_kept_index);
        self.messages.insert(0, summary_msg);

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
}
