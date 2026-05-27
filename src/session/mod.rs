pub mod compact;
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

/// State of a tool call attached to an assistant message. Mirrors
/// opencode's `ToolPart.state` (`message-v2.ts:310-320`). The point
/// of preserving state — rather than just "this tool ran" — is so
/// that resumed sessions can emit a paired tool_result block to the
/// LLM even for tool calls that didn't complete (e.g. user hit
/// Ctrl+C mid-execution). Anthropic + OpenAI reject orphan tool_use
/// blocks; we always emit a result, even if its content is an
/// interrupted marker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolCallState {
    /// Tool ran to completion. `result` is the output text the LLM
    /// would see (the same string the UI rendered in the chamber).
    Completed { result: String },
    /// Tool was dispatched but the agent was aborted before its
    /// result came back. Resumed sessions emit a tool_result with
    /// "[Tool execution was interrupted]" so the LLM knows the
    /// effect is undefined.
    Interrupted,
    /// Tool dispatched but the call errored (e.g. permission denied,
    /// runtime panic). `error` is the message the LLM saw.
    Failed { error: String },
}

/// One tool invocation attached to an assistant message. We keep
/// the original call id (rig's `ToolCall.id`) so resumed sessions
/// emit tool_result blocks with the right correlation id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallEntry {
    /// Provider-supplied call id (e.g. `tooluse_abc123` for
    /// Anthropic, `call_xyz` for OpenAI). Used as the
    /// `tool_use_id` / `tool_call_id` correlation on resume.
    pub id: String,
    /// Tool name as the LLM saw it (`bash`, `read`, `mcp_tool:...`).
    pub name: String,
    /// Arguments the LLM sent. JSON value so it round-trips
    /// without re-parsing.
    pub args: serde_json::Value,
    /// Outcome — completed, interrupted, or failed.
    pub state: ToolCallState,
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
    /// Tool calls + results attached to this assistant message.
    /// Empty for User / System messages and for assistants that
    /// didn't invoke any tools. Phase 3 added persistence so
    /// resumed sessions re-emit structured tool_use/tool_result
    /// blocks to the LLM instead of only the assistant's text;
    /// previously the LLM lost all context of prior tool work on
    /// session resume. Defaulted on deserialize for back-compat
    /// with pre-Phase-3 session files.
    #[serde(default)]
    pub tool_calls: Vec<ToolCallEntry>,
}

/// Generate a fresh message id. Extracted for `#[serde(default = ...)]`.
pub(crate) fn new_message_id() -> CompactString {
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

/// Lightweight record of a forked subtree that was pruned during
/// compress / rewind. Phase 4: pi-style preservation
/// (`packages/coding-agent/src/core/branch-summarization.ts`) at
/// metadata-only granularity — no LLM summary call. Captures
/// enough info (count + preview + parent id) that the user can
/// find pruned branches in `/tree` and understand what was lost.
/// A future Phase 4b could generate full LLM summaries; the schema
/// is forward-compatible.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BranchSummary {
    /// Id of the branch's root node (the topmost node of the
    /// pruned subtree). Kept for debugging — the node itself no
    /// longer exists in `tree.entries`.
    pub root_id: CompactString,
    /// Id of the still-present parent that the branch hung off.
    /// May itself have been pruned in the same compress (e.g.
    /// when both parent and sibling subtree get dropped because
    /// the parent was the dropped active-path message).
    pub parent_id: CompactString,
    /// How many nodes were in the pruned subtree.
    pub message_count: usize,
    /// Human-readable preview: branch label (if any) + first
    /// chars of the root message's content. Shown in `/tree`.
    pub preview: String,
    /// RFC3339 timestamp of when the prune happened.
    pub created_at: String,
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

/// Current session-file schema version. Bump when adding fields
/// that REQUIRE the new code to read correctly (rare — most
/// field additions use `#[serde(default)]` and are
/// forward-compatible). Loaders compare this against the
/// session's stored value: equal or higher is fine (we'll just
/// see defaults for fields we don't recognize); strictly lower
/// triggers a migration shim.
pub const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// F8: schema version of this session file. Defaulted to 0 for
    /// pre-F8 session files (which omit the field entirely);
    /// `load_session` runs migrations from `schema_version` →
    /// `SCHEMA_VERSION` after deserialize. New sessions get
    /// `SCHEMA_VERSION` via `Session::new`.
    #[serde(default)]
    pub schema_version: u32,
    pub id: CompactString,
    pub name: CompactString,
    pub messages: Vec<SessionMessage>,
    pub compactions: Vec<Compaction>,
    pub created_at: CompactString,
    pub updated_at: CompactString,
    // TODO(cost-tracking): `total_tokens` and `total_cost` are placeholders.
    // Currently `total_tokens` accumulates the same heuristic estimate that
    // already lives in `total_estimated_tokens` (`AgentEvent::Done` emits
    // estimated_tokens because no provider integration has been wired
    // through rig to extract actual usage). `total_cost` is never advanced
    // past 0.0 because no per-provider pricing table exists. Both fields
    // serialize for forward-compat so when actual provider usage lands
    // they can be populated without a schema bump.
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
    /// Phase 4 — metadata records for forked subtrees that were
    /// pruned during compress / rewind. Surfaces in `/tree` so the
    /// user can see what branches were dropped. Defaulted on
    /// deserialize for back-compat with pre-Phase-4 session files.
    #[serde(default)]
    pub branch_summaries: Vec<BranchSummary>,
    /// Active prompt name (e.g. "code", "plan", "review"). Persisted
    /// with the session so resuming via `-c` / `/sessions <id>`
    /// restores the same prompt the user had active. Defaulted to
    /// `None` for backward compat with pre-feature session files.
    #[serde(default)]
    pub current_prompt_name: Option<String>,
    /// Batch2-3 (audit fix): file mtime at load time. Used by
    /// `save_session` to detect concurrent writes — if the on-disk
    /// file has a newer mtime than this when we go to save, a
    /// second dirge instance wrote to the same session. We then
    /// divert our write to a `<id>.conflict-<ts>.json` sibling so
    /// neither side loses data. `None` on fresh sessions (no file
    /// on disk yet) — save always wins in that case.
    #[serde(skip)]
    pub loaded_mtime: Option<std::time::SystemTime>,

    /// SESS-15: when the on-disk file's `schema_version` exceeded
    /// this binary's `SCHEMA_VERSION` at load time, store the file
    /// version here. `save_session` refuses to overwrite (the
    /// older dirge would silently zero out the newer fields,
    /// permanently losing data the newer version cared about).
    /// `None` for fresh sessions and ones loaded at-or-below our
    /// schema. Runtime-only — never serialized.
    #[serde(skip)]
    pub loaded_from_newer_version: Option<u64>,
}

impl Session {
    pub fn estimate_tokens(text: &str) -> u64 {
        (text.len() as u64 / 4).max(1)
    }

    /// Estimate the token cost of a single `SessionMessage` using
    /// the SAME logic as `add_message_with_tool_calls`. Used by the
    /// schema-v2 migration to repair the under-counted
    /// `estimated_tokens` field on sessions saved before commit
    /// 9a044ce.
    pub fn estimate_message_tokens(msg: &SessionMessage) -> u64 {
        let mut tokens = Self::estimate_tokens(&msg.content);
        for tc in &msg.tool_calls {
            tokens = tokens
                .saturating_add(Self::estimate_tokens(&tc.args.to_string()))
                .saturating_add(Self::estimate_tokens(&tc.name))
                .saturating_add(16);
            match &tc.state {
                ToolCallState::Completed { result } => {
                    tokens = tokens.saturating_add(Self::estimate_tokens(result));
                }
                ToolCallState::Failed { error } => {
                    tokens = tokens.saturating_add(Self::estimate_tokens(error));
                }
                ToolCallState::Interrupted => {
                    tokens = tokens.saturating_add(8);
                }
            }
        }
        tokens
    }

    /// Recompute every message's `estimated_tokens` + the session's
    /// `total_estimated_tokens` using the current accounting (which
    /// includes tool args + tool results). Schema-v2 migration path
    /// — pre-9a044ce session files have under-counted values from
    /// the old text-only logic; this brings them up to date.
    pub fn recompute_all_estimates(&mut self) {
        for msg in self.messages.iter_mut() {
            msg.estimated_tokens = Self::estimate_message_tokens(msg);
        }
        // Mirror into the message_store too — the tree-backed copy
        // would otherwise carry the old values.
        for (id, m) in self.message_store.iter_mut() {
            if let Some(canonical) = self.messages.iter().find(|x| &x.id == id) {
                m.estimated_tokens = canonical.estimated_tokens;
            } else {
                m.estimated_tokens = Self::estimate_message_tokens(m);
            }
        }
        self.total_estimated_tokens = self.messages.iter().map(|m| m.estimated_tokens).sum();
    }

    pub fn new(provider: &str, model: &str, context_window: u64) -> Self {
        let now = CompactString::new(chrono::Utc::now().to_rfc3339());
        Session {
            schema_version: SCHEMA_VERSION,
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
            branch_summaries: Vec::new(),
            current_prompt_name: None,
            loaded_mtime: None,
            loaded_from_newer_version: None,
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
        self.ensure_next_entry_seq_initialized();
    }

    /// Recover `next_entry_seq` for sessions that have populated
    /// `extra_entries` but a stale or default `next_entry_seq`
    /// counter. The struct's doc comment promises this seeding, but
    /// it was never actually wired up — a pre-versioned session,
    /// corrupted file, or hand-edited JSON could end up with
    /// `extra_entries = [seq=0, seq=1, seq=2]` and `next_entry_seq
    /// = 0`, causing the next `append_plugin_entry` to assign seq=0
    /// and collide with the existing entry.
    ///
    /// Seed `next_entry_seq` to `max(existing seqs + 1, len)` so
    /// future appends always advance.
    fn ensure_next_entry_seq_initialized(&mut self) {
        if self.extra_entries.is_empty() {
            return;
        }
        let max_seq = self.extra_entries.iter().map(|e| e.seq).max().unwrap_or(0);
        let needed = max_seq
            .saturating_add(1)
            .max(self.extra_entries.len() as u64);
        if self.next_entry_seq < needed {
            self.next_entry_seq = needed;
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
        self.add_message_with_tool_calls(role, content, Vec::new());
    }

    /// Same as `add_message` but attaches structured tool-call
    /// entries to the new message. Used by the runner to persist
    /// assistant turns that invoked tools so `convert_history`
    /// can re-emit structured tool_use/tool_result blocks on
    /// session resume. Empty `tool_calls` is equivalent to the
    /// plain `add_message`.
    pub fn add_message_with_tool_calls(
        &mut self,
        role: MessageRole,
        content: &str,
        tool_calls: Vec<ToolCallEntry>,
    ) {
        // Make sure tree + store mirror any messages that were loaded
        // from a pre-P4b/P4c session file BEFORE we append the new
        // one — otherwise the rebuild would re-insert this new message
        // with the wrong parent.
        self.ensure_back_compat_initialized();
        // User-reported under-count: status line stuck at ~16k/128k
        // after a long session. Root cause was that this estimate only
        // counted the assistant TEXT — not the tool args the LLM sent
        // nor the tool RESULT text the LLM re-saw on every subsequent
        // turn. `convert_history` re-emits structured tool_use /
        // tool_result blocks containing both, so the model's actual
        // context grows by exactly those bytes too. Estimate the FULL
        // serialized assistant turn here so the status indicator
        // tracks what the model is actually carrying.
        let mut tokens = Self::estimate_tokens(content);
        for tc in &tool_calls {
            // Tool args: roughly serialize_json(args).len() / 4.
            tokens = tokens
                .saturating_add(Self::estimate_tokens(&tc.args.to_string()))
                // Plus the tool name + a few framing bytes
                // (`tool_use_id`, `<tool_use>` framing). Approximate
                // at 16 tokens of overhead per call — empirically
                // matches what providers report for the wrapper.
                .saturating_add(Self::estimate_tokens(&tc.name))
                .saturating_add(16);
            match &tc.state {
                ToolCallState::Completed { result } => {
                    tokens = tokens.saturating_add(Self::estimate_tokens(result));
                }
                ToolCallState::Failed { error } => {
                    tokens = tokens.saturating_add(Self::estimate_tokens(error));
                }
                ToolCallState::Interrupted => {
                    // Re-emitted as the fixed string "[Tool execution
                    // was interrupted]" on resume — small constant.
                    tokens = tokens.saturating_add(8);
                }
            }
        }
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
            tool_calls,
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
        // Refuse to pop a compaction summary. After compress, the
        // System message at index 0 anchors all prior context for
        // the next agent turn; removing it would silently lose the
        // entire compressed history. Repeated `/undo` past the
        // recent messages must stop here. The user can `/clear` to
        // reset entirely.
        if let Some(last) = self.messages.last()
            && self.messages.len() == 1
            && last.role == MessageRole::System
        {
            return None;
        }
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
        // SESS-1: cycle detection. If entries contain a cycle
        // (malformed JSON, tampered file, or a bug), the walk
        // loops forever. Cap hops and use a visited set.
        let mut chain: Vec<CompactString> = Vec::new();
        let mut cursor: Option<CompactString> = Some(new_leaf_id.clone());
        let mut visited = std::collections::HashSet::new();
        let mut hops = 0usize;
        const MAX_HOPS: usize = 10_000;
        while let Some(id) = cursor {
            if hops >= MAX_HOPS {
                return Err(format!(
                    "cycle or excessive depth in session tree (>{} hops from leaf {})",
                    MAX_HOPS, new_leaf_id
                ));
            }
            hops += 1;
            if !visited.insert(id.clone()) {
                return Err(format!("cycle detected in session tree at node {}", id));
            }
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
        // Least privilege: clear `permission_allowlist` too. The
        // previous "preserved across reset" behavior let "allow
        // always" grants for one task (e.g. `bash cargo *` from a
        // testing session) silently transfer to an unrelated fresh
        // session. Each new session re-asks for permissions.
        self.permission_allowlist.clear();
        // Phase 4: branch summaries are per-session — wipe with
        // everything else so a fresh session doesn't inherit
        // phantom branch records from the prior one.
        self.branch_summaries.clear();
        // SESS-10: clear the file-conflict mtime and active prompt
        // name. A reset session has no on-disk file yet (the new id
        // doesn't exist), so a stale `loaded_mtime` from the prior
        // session would confuse the concurrent-writer check on the
        // first save. `current_prompt_name` is session-scoped — the
        // prompt the user was running before reset shouldn't carry
        // over implicitly.
        self.loaded_mtime = None;
        self.current_prompt_name = None;
        // SESS-15: reset_to_new generates a fresh id, so we own
        // the on-disk file under that id; the newer-schema flag
        // belonged to the prior id's file.
        self.loaded_from_newer_version = None;
        // Note: model/provider/context_window/working_dir preserved
        // so the host can keep the same agent runtime.
    }

    pub fn needs_compaction(&self, reserve_tokens: u64) -> bool {
        compact::needs_compaction(
            self.total_estimated_tokens,
            self.context_window,
            reserve_tokens,
        )
    }

    pub fn compacted_context(&self) -> (Option<&str>, usize) {
        compact::compacted_context(&self.compactions, self.messages.len())
    }

    /// Legacy wrapper retained for tests that don't care about the
    /// pruned-siblings count. New callers should use
    /// `compress_reporting` to surface a "discarded N branches"
    /// notification to the user when sibling subtrees are dropped.
    #[cfg(test)]
    pub fn compress(&mut self, summary: String, first_kept_index: usize, token_savings: u64) {
        compact::compress(self, summary, first_kept_index, token_savings);
    }

    /// Same as `compress` but returns the number of NON-active-path
    /// nodes that were pruned from the tree because their ancestor
    /// chain rooted at a dropped message. Active-path message drops
    /// aren't counted — they're expected and already visible to the
    /// user as the conversation history shrinking. The host uses this
    /// count to surface a "discarded N forked branches" notification
    /// (matches opencode's drop-with-truncation pattern from
    /// `session/compaction.ts:386-396` — sibling branches outside the
    /// preserved tail are gone, full stop).
    pub fn compress_reporting(
        &mut self,
        summary: String,
        first_kept_index: usize,
        token_savings: u64,
    ) -> usize {
        compact::compress_reporting(self, summary, first_kept_index, token_savings)
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
