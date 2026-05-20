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
