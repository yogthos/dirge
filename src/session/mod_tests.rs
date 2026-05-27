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

/// User-reported: status line stuck at ~16/128k after a long
/// session because tool args + tool results weren't counted.
/// An assistant message that ran a tool with a 2 KB args blob
/// and got back an 8 KB result should add the COMBINED bytes
/// (text + args + result) / 4 — plus a small per-call overhead
/// — to `total_estimated_tokens`. This test pins the contract.
#[test]
fn tool_call_args_and_results_count_toward_estimate() {
    let mut s = Session::new("p", "m", 0);
    // Baseline: a plain user message.
    s.add_message(MessageRole::User, "hi");
    let baseline = s.total_estimated_tokens;

    // Assistant message with one tool call carrying ~8 KB result.
    let result = "x".repeat(8000);
    let args = serde_json::json!({ "command": "ls -la /very/deep/path" });
    let tc = ToolCallEntry {
        id: "t1".to_string(),
        name: "bash".to_string(),
        args: args.clone(),
        state: ToolCallState::Completed {
            result: result.clone(),
        },
    };
    s.add_message_with_tool_calls(MessageRole::Assistant, "I'll run that.", vec![tc]);

    let delta = s.total_estimated_tokens - baseline;
    // Expected: text ("I'll run that.") + args (~37 chars) +
    // result (8000 chars) + name ("bash") + 16 overhead. All
    // estimated at chars/4. Result alone is 8000/4 = 2000
    // tokens; delta must therefore be ≥ 1900 (allow some slack
    // for estimator rounding differences).
    assert!(
        delta >= 1900,
        "tool result must dominate the estimate: delta = {delta}",
    );
    // Sanity upper bound: not pathologically large either.
    assert!(delta < 3000, "delta = {delta} should be ~2050");
}

/// A Failed tool call counts the error string too.
#[test]
fn failed_tool_call_counts_error_text() {
    let mut s = Session::new("p", "m", 0);
    let big_err = "y".repeat(4000);
    let tc = ToolCallEntry {
        id: "t1".to_string(),
        name: "bash".to_string(),
        args: serde_json::json!({}),
        state: ToolCallState::Failed {
            error: big_err.clone(),
        },
    };
    s.add_message_with_tool_calls(MessageRole::Assistant, "", vec![tc]);
    assert!(
        s.total_estimated_tokens >= 900,
        "failed-tool error must be counted: got {}",
        s.total_estimated_tokens,
    );
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

/// Regression: a session loaded with populated `extra_entries`
/// but stale `next_entry_seq` (corruption, hand-edit, or a
/// pre-version save) must re-seed the counter so the next
/// `append_plugin_entry` doesn't assign a colliding seq.
/// The Session struct's doc comment claimed this seeding
/// existed; this test pins it.
#[test]
fn ensure_back_compat_reseeds_next_entry_seq_from_stale_value() {
    let mut s = Session::new("openai", "gpt-4", 200_000);
    // Simulate a corrupted/hand-edited session: three plugin
    // entries on disk but next_entry_seq still at 0.
    s.extra_entries.push(PluginEntry {
        seq: 0,
        timestamp: 0,
        display: true,
        custom_type: "bookmark".to_string(),
        data: "a".to_string(),
    });
    s.extra_entries.push(PluginEntry {
        seq: 1,
        timestamp: 0,
        display: true,
        custom_type: "bookmark".to_string(),
        data: "b".to_string(),
    });
    s.extra_entries.push(PluginEntry {
        seq: 2,
        timestamp: 0,
        display: true,
        custom_type: "bookmark".to_string(),
        data: "c".to_string(),
    });
    s.next_entry_seq = 0;
    s.ensure_back_compat_initialized();
    // After back-compat init, seq must be >= 3 so the next
    // append doesn't collide with any existing entry.
    assert!(
        s.next_entry_seq >= 3,
        "next_entry_seq must skip past existing seqs; got {}",
        s.next_entry_seq,
    );
    let added_seq = s.append_plugin_entry("bookmark", "d", true).seq;
    // The new entry's seq must be unique vs existing.
    let seqs: std::collections::HashSet<u64> = s.extra_entries.iter().map(|e| e.seq).collect();
    assert_eq!(seqs.len(), 4, "all seqs must be unique; got {seqs:?}");
    assert!(added_seq >= 3, "new entry's seq must skip past existing");
}

/// `reset_to_new` must clear `permission_allowlist` to avoid
/// leaking "allow always" decisions from a prior conversation
/// into an unrelated fresh session. Least-privilege: each new
/// task starts with no implicit grants.
#[test]
fn reset_to_new_clears_permission_allowlist() {
    let mut s = Session::new("openai", "gpt-4", 200_000);
    s.permission_allowlist.push(PermissionAllowEntry {
        tool: "bash".to_string(),
        pattern: "rm -rf /tmp/foo".to_string(),
    });
    assert!(!s.permission_allowlist.is_empty());
    s.reset_to_new(None);
    assert!(
        s.permission_allowlist.is_empty(),
        "allowlist must reset on new session; got {:?}",
        s.permission_allowlist,
    );
}

/// Repeated `/undo` after a compress must NOT pop the System
/// summary at index 0. Removing it would unanchor the compressed
/// context and the next agent turn would see no history.
#[test]
fn pop_last_message_refuses_to_pop_summary_at_index_zero() {
    let mut s = Session::new("p", "m", 0);
    s.add_message(MessageRole::User, "u1");
    s.add_message(MessageRole::Assistant, "a1");
    s.add_message(MessageRole::User, "u2");
    s.add_message(MessageRole::Assistant, "a2");
    s.compress("summary".to_string(), 4, 100);
    // After compress: messages = [<summary at 0>, ...nothing else,
    // since we drained 4 and kept 0 recent]. Verify the shape.
    assert_eq!(s.messages.len(), 1, "post-compress shape");
    assert_eq!(s.messages[0].role, MessageRole::System);

    // Now /undo style pop. Must NOT remove the summary.
    let popped = s.pop_last_message();
    assert!(
        popped.is_none(),
        "pop must refuse the summary; got {:?}",
        popped,
    );
    assert_eq!(s.messages.len(), 1, "summary must remain");
    assert_eq!(s.messages[0].role, MessageRole::System);
}

/// Popping past the summary by depleting all recent messages
/// also must not remove the summary itself — the user has to
/// `/clear` to reset.
#[test]
fn pop_drains_recent_but_keeps_summary() {
    let mut s = Session::new("p", "m", 0);
    s.add_message(MessageRole::User, "u1");
    s.add_message(MessageRole::Assistant, "a1");
    s.compress("summary".to_string(), 2, 50);
    // Re-add some recent messages after the compress.
    s.add_message(MessageRole::User, "u2");
    s.add_message(MessageRole::Assistant, "a2");
    assert_eq!(s.messages.len(), 3); // [summary, u2, a2]

    // Pop a2, then u2 — both should succeed.
    assert!(s.pop_last_message().is_some());
    assert!(s.pop_last_message().is_some());
    assert_eq!(s.messages.len(), 1);
    assert_eq!(s.messages[0].role, MessageRole::System);

    // One more pop attempt → refused.
    assert!(s.pop_last_message().is_none());
    assert_eq!(s.messages.len(), 1);
}

/// Phase 4 — when compress prunes a sibling subtree, it records
/// a `BranchSummary` capturing what was lost: parent id,
/// message count, preview text. Without this users see
/// "discarded N branches" with no way to know what was in them.
#[test]
fn compress_records_branch_summary_for_pruned_siblings() {
    let mut s = Session::new("p", "m", 0);
    s.add_message(MessageRole::User, "u1");
    let u1_id = s.messages.last().unwrap().id.clone();
    s.add_message(MessageRole::Assistant, "a1");
    s.add_message(MessageRole::User, "u2 keep");
    s.add_message(MessageRole::Assistant, "a2 keep");
    // Graft a sibling pair under u1.
    let sib1_id = CompactString::new("sib_alpha");
    let sib2_id = CompactString::new("sib_beta");
    s.tree.entries.insert(
        sib1_id.clone(),
        TreeNode {
            id: sib1_id.clone(),
            parent: Some(u1_id.clone()),
            timestamp: 100,
            label: Some("explore-alt".to_string()),
        },
    );
    s.tree.entries.insert(
        sib2_id.clone(),
        TreeNode {
            id: sib2_id.clone(),
            parent: Some(sib1_id.clone()),
            timestamp: 200,
            label: None,
        },
    );
    s.message_store.insert(
        sib1_id.clone(),
        SessionMessage {
            role: MessageRole::Assistant,
            content: CompactString::from(
                "let me try a different approach: investigate the foo module first",
            ),
            estimated_tokens: 10,
            id: sib1_id.clone(),
            timestamp: 100,
            tool_calls: Vec::new(),
        },
    );
    s.message_store.insert(
        sib2_id.clone(),
        SessionMessage {
            role: MessageRole::User,
            content: CompactString::from("continue with that"),
            estimated_tokens: 3,
            id: sib2_id.clone(),
            timestamp: 200,
            tool_calls: Vec::new(),
        },
    );

    let baseline = s.branch_summaries.len();
    let pruned = s.compress_reporting("summary".to_string(), 2, 10);
    assert_eq!(pruned, 2, "expected 2 sibling nodes pruned");
    assert_eq!(
        s.branch_summaries.len(),
        baseline + 1,
        "expected 1 branch summary; got {:?}",
        s.branch_summaries,
    );
    let summary = s.branch_summaries.last().unwrap();
    // Parent is u1 (the closest still-active-or-dropped ancestor
    // of the pruned subtree). u1 itself was dropped in this
    // compress, but the summary records the relationship so the
    // user can correlate.
    assert_eq!(summary.parent_id, u1_id);
    assert_eq!(summary.message_count, 2);
    // Preview pulls from the root sibling's content + label.
    assert!(
        summary.preview.contains("explore-alt") || summary.preview.contains("different approach"),
        "preview missing branch info: {:?}",
        summary.preview,
    );
}

/// `reset_to_new` clears `branch_summaries`.
#[test]
fn reset_to_new_clears_branch_summaries() {
    let mut s = Session::new("p", "m", 0);
    s.branch_summaries.push(BranchSummary {
        root_id: CompactString::from("x"),
        parent_id: CompactString::from("y"),
        message_count: 3,
        preview: "lingering branch".to_string(),
        created_at: "2026-01-01".to_string(),
    });
    s.reset_to_new(None);
    assert!(s.branch_summaries.is_empty());
}

/// Phase 3 — tool calls round-trip through serde with default
/// for back-compat. Old session files without the field
/// deserialize into an empty Vec.
#[test]
fn session_message_tool_calls_default_when_field_missing() {
    let json = r#"{
        "role": "assistant",
        "content": "Done.",
        "estimated_tokens": 5
    }"#;
    let msg: SessionMessage = serde_json::from_str(json).unwrap();
    assert!(
        msg.tool_calls.is_empty(),
        "missing field must default to []"
    );
}

/// Round-trip: write a message WITH tool_calls, read back, fields intact.
#[test]
fn session_message_tool_calls_roundtrip() {
    let mut s = Session::new("p", "m", 0);
    let calls = vec![
        ToolCallEntry {
            id: "tc_1".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"cmd": "ls"}),
            state: ToolCallState::Completed {
                result: "file1\nfile2".to_string(),
            },
        },
        ToolCallEntry {
            id: "tc_2".to_string(),
            name: "read".to_string(),
            args: serde_json::json!({"path": "/tmp/x"}),
            state: ToolCallState::Interrupted,
        },
    ];
    s.add_message_with_tool_calls(MessageRole::Assistant, "Let me check.", calls.clone());

    let blob = serde_json::to_string(&s).unwrap();
    let s2: Session = serde_json::from_str(&blob).unwrap();
    let last = s2.messages.last().unwrap();
    assert_eq!(last.tool_calls.len(), 2);
    assert_eq!(last.tool_calls[0].id, "tc_1");
    assert!(matches!(
        last.tool_calls[0].state,
        ToolCallState::Completed { .. },
    ));
    assert!(matches!(
        last.tool_calls[1].state,
        ToolCallState::Interrupted,
    ));
}

/// Convert history materializes prior tool calls as structured
/// rig Message blocks (Assistant with ToolCall content +
/// User with ToolResult content). Without this, resumed sessions
/// lose tool-call context and the LLM may re-call the same
/// tools. Matches opencode's `message-v2.ts:630-899` pattern.
#[test]
fn convert_history_emits_tool_use_and_tool_result_blocks() {
    let mut s = Session::new("p", "m", 0);
    s.add_message(MessageRole::User, "list files");
    s.add_message_with_tool_calls(
        MessageRole::Assistant,
        "Here:",
        vec![ToolCallEntry {
            id: "tc_42".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"cmd": "ls"}),
            state: ToolCallState::Completed {
                result: "a\nb".to_string(),
            },
        }],
    );

    let history = crate::agent::runner::convert_history(&s);
    // Expect: User("list files"), Assistant(text + tool_use),
    // User(tool_result). 3 messages total.
    assert_eq!(history.len(), 3, "history shape: {:#?}", history);

    // Last is a User with tool_result content carrying the id.
    match &history[2] {
        rig::completion::Message::User { content } => {
            let s = format!("{:?}", content);
            assert!(s.contains("tc_42"), "tool_result missing call id: {s}");
            // Debug format escapes newlines, so check the
            // escaped form. The underlying string still has the
            // real newline; this is just an assertion-side
            // formatting consideration.
            assert!(
                s.contains("a\\nb") || s.contains("a\nb"),
                "tool_result missing output: {s}",
            );
        }
        other => panic!("expected User tool_result message; got {other:?}"),
    }

    // Middle is Assistant with both text and a ToolCall.
    match &history[1] {
        rig::completion::Message::Assistant { content, .. } => {
            let s = format!("{:?}", content);
            assert!(s.contains("tc_42"), "tool_use missing id: {s}");
            assert!(s.contains("\"bash\""), "tool_use missing name: {s}");
        }
        other => panic!("expected Assistant message; got {other:?}"),
    }
}

/// Interrupted tool calls must be emitted as tool_result with
/// an "[interrupted]" marker, NOT skipped. Anthropic + OpenAI
/// reject orphan tool_use blocks; opencode handles this
/// (`message-v2.ts:848-857`) by emitting an error tool_result.
#[test]
fn convert_history_pairs_interrupted_tool_calls_with_error_marker() {
    let mut s = Session::new("p", "m", 0);
    s.add_message_with_tool_calls(
        MessageRole::Assistant,
        "About to bash...",
        vec![ToolCallEntry {
            id: "tc_99".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"cmd": "sleep 9999"}),
            state: ToolCallState::Interrupted,
        }],
    );

    let history = crate::agent::runner::convert_history(&s);
    // 2 messages: Assistant(text + tool_use) + User(tool_result-interrupted).
    assert_eq!(history.len(), 2);
    let last_str = format!("{:?}", &history[1]);
    assert!(
        last_str.contains("tc_99"),
        "interrupted result must reference call id: {last_str}",
    );
    assert!(
        last_str.contains("interrupted") || last_str.contains("Interrupted"),
        "interrupted result must say so: {last_str}",
    );
}

/// Phase 2 — compress drops a parent that has a sibling branch
/// underneath. The sibling subtree must also be pruned;
/// otherwise its nodes have `parent` pointing at a removed id
/// and the tree is dangling. Returns the count of pruned
/// non-active nodes so the host can surface a "discarded N
/// branches" notification.
#[test]
fn compress_prunes_sibling_branches_rooted_at_dropped_messages() {
    let mut s = Session::new("p", "m", 0);
    // Linear active branch: u1 → a1 → u2 → a2.
    s.add_message(MessageRole::User, "u1");
    let u1_id = s.messages.last().unwrap().id.clone();
    s.add_message(MessageRole::Assistant, "a1");
    s.add_message(MessageRole::User, "u2");
    s.add_message(MessageRole::Assistant, "a2");
    // Manually graft a sibling branch under u1: sib1 (child of
    // u1) → sib2 (child of sib1). These live in tree.entries +
    // message_store but NOT in `messages` (different branch).
    let sib1_id = CompactString::new("sib1");
    let sib2_id = CompactString::new("sib2");
    s.tree.entries.insert(
        sib1_id.clone(),
        TreeNode {
            id: sib1_id.clone(),
            parent: Some(u1_id.clone()),
            timestamp: 0,
            label: None,
        },
    );
    s.tree.entries.insert(
        sib2_id.clone(),
        TreeNode {
            id: sib2_id.clone(),
            parent: Some(sib1_id.clone()),
            timestamp: 0,
            label: None,
        },
    );
    s.message_store.insert(
        sib1_id.clone(),
        SessionMessage {
            role: MessageRole::Assistant,
            content: CompactString::from("sib1"),
            estimated_tokens: 1,
            id: sib1_id.clone(),
            timestamp: 0,
            tool_calls: Vec::new(),
        },
    );
    s.message_store.insert(
        sib2_id.clone(),
        SessionMessage {
            role: MessageRole::Assistant,
            content: CompactString::from("sib2"),
            estimated_tokens: 1,
            id: sib2_id.clone(),
            timestamp: 0,
            tool_calls: Vec::new(),
        },
    );

    // Compress drops u1+a1 (first 2 messages). Sibling branch
    // is rooted at u1 (a dropped id), so sib1 + sib2 must be
    // pruned from tree + store along with the dropped messages.
    let pruned = s.compress_reporting("summary".to_string(), 2, 10);

    // u1, a1 (linear), sib1, sib2 (sibling) all gone from tree.
    assert!(!s.tree.entries.contains_key(&u1_id), "u1 still in tree");
    assert!(!s.tree.entries.contains_key(&sib1_id), "sib1 still in tree");
    assert!(!s.tree.entries.contains_key(&sib2_id), "sib2 still in tree");
    assert!(
        !s.message_store.contains_key(&sib1_id),
        "sib1 still in store",
    );
    assert!(
        !s.message_store.contains_key(&sib2_id),
        "sib2 still in store",
    );

    // Report: 2 sibling branch nodes were pruned (sib1, sib2).
    // The linear u1+a1 drops aren't counted as "branches" since
    // they were on the active path.
    assert_eq!(pruned, 2, "expected 2 sibling nodes pruned, got {pruned}",);
}

/// When compress drops messages but there are NO sibling branches,
/// the report says 0 sibling nodes pruned. Confirms the counter
/// isn't accidentally counting active-path nodes.
#[test]
fn compress_reports_zero_pruned_when_no_siblings() {
    let mut s = Session::new("p", "m", 0);
    s.add_message(MessageRole::User, "u1");
    s.add_message(MessageRole::Assistant, "a1");
    s.add_message(MessageRole::User, "u2");
    s.add_message(MessageRole::Assistant, "a2");
    let pruned = s.compress_reporting("summary".to_string(), 2, 10);
    assert_eq!(pruned, 0);
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
