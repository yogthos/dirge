//! Apply plugin-issued [`TreeOp`]s to a live [`Session`] (P4d).
//!
//! The plugin worker queues ops on `harness-tree-ops`; the UI loop
//! drains them between events via `PluginManager::drain_tree_ops` and
//! hands the result to [`apply_tree_op`] here.
//!
//! Mirrors pi's `ctx.setLabel` / `ctx.fork` / `ctx.navigateTree` /
//! `ctx.newSession` / `ctx.switchSession` semantics. See `P4d` notes in
//! the README for the user-facing surface.

use compact_str::CompactString;

use crate::plugin::TreeOp;
use crate::session::{MessageRole, Session};
use crate::ui::input::InputEditor;

/// Outcome surfaced to the UI so it can render a status line, redraw
/// the chat, etc. Plain enum (no Display impl — callers format).
#[derive(Debug, PartialEq, Eq)]
pub enum TreeOpEffect {
    /// State change happened; UI should re-render the session and
    /// show the optional confirmation message.
    Applied(String),
    /// Op failed — message describes why. Surface as an error line.
    Failed(String),
    /// Session itself was replaced (new-session / switch-session).
    /// Caller must rebuild the agent + repaint completely.
    SessionReplaced(String),
}

/// Apply one op. Returns the UI-visible effect. Restored editor text
/// (for fork :before / navigate-tree on user messages) is pushed
/// straight into `input`.
pub fn apply_tree_op(op: TreeOp, session: &mut Session, input: &mut InputEditor) -> TreeOpEffect {
    match op {
        TreeOp::SetLabel { id, label } => {
            let cid = CompactString::new(id.clone());
            match session.set_label(&cid, label.clone()) {
                Ok(()) => TreeOpEffect::Applied(match label {
                    Some(l) => format!("[plugin] labeled {} as \"{}\"", short(&id), l),
                    None => format!("[plugin] cleared label on {}", short(&id)),
                }),
                Err(e) => TreeOpEffect::Failed(format!("[plugin] set-label: {}", e)),
            }
        }
        TreeOp::Fork { id, restore_text } => {
            let cid = CompactString::new(id.clone());
            match session.fork_at(&cid) {
                Ok(original) => {
                    if restore_text {
                        input.set_text(&original.content);
                    }
                    TreeOpEffect::Applied(format!("[plugin] forked at {}", short(&id)))
                }
                Err(e) => TreeOpEffect::Failed(format!("[plugin] fork: {}", e)),
            }
        }
        TreeOp::NavigateTree { id } => {
            let cid = CompactString::new(id.clone());
            // Pi's semantics: if the target is a user message, move
            // the leaf to its parent and restore the prompt; for any
            // other role, the target itself becomes the new leaf.
            let role = session.message_store.get(&cid).map(|m| m.role);
            match role {
                None => TreeOpEffect::Failed(format!(
                    "[plugin] navigate-tree: unknown entry {}",
                    short(&id)
                )),
                Some(MessageRole::User) => match session.fork_at(&cid) {
                    Ok(original) => {
                        input.set_text(&original.content);
                        TreeOpEffect::Applied(format!(
                            "[plugin] navigated to user message {} (prompt restored)",
                            short(&id),
                        ))
                    }
                    Err(e) => TreeOpEffect::Failed(format!("[plugin] navigate-tree: {}", e)),
                },
                Some(_) => match session.switch_to_leaf(&cid) {
                    Ok(()) => {
                        TreeOpEffect::Applied(format!("[plugin] navigated to {}", short(&id)))
                    }
                    Err(e) => TreeOpEffect::Failed(format!("[plugin] navigate-tree: {}", e)),
                },
            }
        }
        TreeOp::NewSession { parent } => {
            // Persist the current session before resetting so the user
            // can still recover it via `/sessions`. Failures here are
            // logged but don't block the reset — getting wedged on disk
            // I/O would be worse than losing a session.
            let prev_id = session.id.to_string();
            let parent_id = parent.as_deref().unwrap_or(&prev_id);
            let _ = crate::session::storage::save_session(session);
            session.reset_to_new(Some(parent_id));
            input.set_text("");
            TreeOpEffect::SessionReplaced(format!(
                "[plugin] new session started (parent: {})",
                short(parent_id),
            ))
        }
        TreeOp::SwitchSession { id_prefix } => {
            match crate::session::storage::find_sessions_by_prefix(&id_prefix) {
                Ok(matches) => match matches.len() {
                    0 => TreeOpEffect::Failed(format!(
                        "[plugin] switch-session: no session matching '{}'",
                        id_prefix
                    )),
                    1 => {
                        let _ = crate::session::storage::save_session(session);
                        let loaded = matches.into_iter().next().expect("len == 1");
                        let new_id = loaded.id.clone();
                        *session = loaded;
                        input.set_text("");
                        TreeOpEffect::SessionReplaced(format!(
                            "[plugin] switched to session {}",
                            short(new_id.as_str()),
                        ))
                    }
                    n => {
                        // Surface the first few matches so the plugin
                        // author / user can pick a longer prefix.
                        let ids: Vec<String> = matches
                            .iter()
                            .take(3)
                            .map(|s| short(s.id.as_str()))
                            .collect();
                        let suffix = if n > 3 {
                            format!(" (and {} more)", n - 3)
                        } else {
                            String::new()
                        };
                        TreeOpEffect::Failed(format!(
                            "[plugin] switch-session: prefix '{}' matches {} sessions ({}){}",
                            id_prefix,
                            n,
                            ids.join(", "),
                            suffix,
                        ))
                    }
                },
                Err(e) => TreeOpEffect::Failed(format!("[plugin] switch-session: {}", e)),
            }
        }
    }
}

fn short(s: &str) -> String {
    s.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    fn fresh_input() -> InputEditor {
        InputEditor::new()
    }

    #[test]
    fn set_label_applies_to_existing_node() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "hello");
        let id = s.messages[0].id.to_string();
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::SetLabel {
                id: id.clone(),
                label: Some("milestone".to_string()),
            },
            &mut s,
            &mut input,
        );
        assert!(matches!(effect, TreeOpEffect::Applied(_)));
        let node_label = s
            .tree
            .entries
            .get(&CompactString::new(&id))
            .and_then(|n| n.label.as_deref());
        assert_eq!(node_label, Some("milestone"));
    }

    /// SetLabel with None clears any existing label on the node.
    #[test]
    fn set_label_with_none_clears() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "hi");
        let id = s.messages[0].id.clone();
        s.set_label(&id, Some("old".to_string())).unwrap();
        let mut input = fresh_input();
        apply_tree_op(
            TreeOp::SetLabel {
                id: id.to_string(),
                label: None,
            },
            &mut s,
            &mut input,
        );
        assert_eq!(s.tree.entries[&id].label, None);
    }

    /// Fork with `restore_text=true` pushes the original prompt back
    /// into the editor so the user can re-edit.
    #[test]
    fn fork_with_restore_text_pushes_to_input() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "what's 2+2?");
        s.add_message(MessageRole::Assistant, "4");
        // Fork at the user message — its content should land back in
        // the editor and the assistant reply should be gone from the
        // current branch.
        let user_id = s.messages[0].id.to_string();
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::Fork {
                id: user_id,
                restore_text: true,
            },
            &mut s,
            &mut input,
        );
        assert!(matches!(effect, TreeOpEffect::Applied(_)));
        assert_eq!(input.buffer.as_str(), "what's 2+2?");
        assert!(s.messages.is_empty(), "leaf moved before user msg");
    }

    /// Fork with `restore_text=false` (the :at position) shifts the
    /// leaf but leaves the editor alone.
    #[test]
    fn fork_at_position_does_not_touch_input() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "q1");
        s.add_message(MessageRole::Assistant, "a1");
        let user_id = s.messages[0].id.to_string();
        let mut input = fresh_input();
        input.set_text("user-was-typing");
        apply_tree_op(
            TreeOp::Fork {
                id: user_id,
                restore_text: false,
            },
            &mut s,
            &mut input,
        );
        // Editor untouched.
        assert_eq!(input.buffer.as_str(), "user-was-typing");
    }

    /// Fork with unknown id surfaces a Failed effect, not a panic.
    #[test]
    fn fork_with_unknown_id_returns_failed() {
        let mut s = Session::new("p", "m", 0);
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::Fork {
                id: "ghost".to_string(),
                restore_text: true,
            },
            &mut s,
            &mut input,
        );
        assert!(matches!(effect, TreeOpEffect::Failed(_)));
    }

    /// NavigateTree to a user message restores text + moves to parent
    /// (pi parity).
    #[test]
    fn navigate_tree_user_message_restores_text() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "redo me");
        s.add_message(MessageRole::Assistant, "won't survive");
        let user_id = s.messages[0].id.to_string();
        let mut input = fresh_input();
        apply_tree_op(TreeOp::NavigateTree { id: user_id }, &mut s, &mut input);
        assert_eq!(input.buffer.as_str(), "redo me");
        assert!(s.messages.is_empty());
    }

    /// NavigateTree to a non-user (assistant) message sets that node
    /// as the leaf — no editor restore.
    #[test]
    fn navigate_tree_assistant_message_switches_leaf() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "q");
        s.add_message(MessageRole::Assistant, "a");
        s.add_message(MessageRole::User, "q2");
        let asst_id = s.messages[1].id.clone();
        let mut input = fresh_input();
        input.set_text("hands-off");
        apply_tree_op(
            TreeOp::NavigateTree {
                id: asst_id.to_string(),
            },
            &mut s,
            &mut input,
        );
        assert_eq!(input.buffer.as_str(), "hands-off");
        assert_eq!(s.tree.leaf_id.as_deref(), Some(asst_id.as_str()));
        // messages was rebuilt to the path-from-leaf for the new leaf.
        assert_eq!(s.messages.last().map(|m| m.content.as_str()), Some("a"));
    }

    /// NavigateTree with unknown id surfaces a Failed effect (we look
    /// up the role in message_store first to decide branch vs. switch).
    #[test]
    fn navigate_tree_unknown_id_returns_failed() {
        let mut s = Session::new("p", "m", 0);
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::NavigateTree {
                id: "missing".to_string(),
            },
            &mut s,
            &mut input,
        );
        assert!(matches!(effect, TreeOpEffect::Failed(_)));
    }

    /// NewSession wipes session state and assigns a fresh id; the
    /// effect must be SessionReplaced so the host rebuilds the agent.
    #[test]
    fn new_session_returns_session_replaced() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "stale");
        let old_id = s.id.clone();
        let mut input = fresh_input();
        let effect = apply_tree_op(TreeOp::NewSession { parent: None }, &mut s, &mut input);
        assert!(matches!(effect, TreeOpEffect::SessionReplaced(_)));
        assert!(s.messages.is_empty());
        assert_ne!(s.id, old_id);
    }

    /// SwitchSession with a non-matching prefix returns Failed without
    /// touching the session.
    #[test]
    fn switch_session_unknown_prefix_returns_failed() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "keep me");
        let id_before = s.id.clone();
        let msg_count_before = s.messages.len();
        let mut input = fresh_input();
        let effect = apply_tree_op(
            TreeOp::SwitchSession {
                id_prefix: "zzzzzzzz-nope".to_string(),
            },
            &mut s,
            &mut input,
        );
        // No matching session on disk -> Failed.
        assert!(matches!(effect, TreeOpEffect::Failed(_)));
        // Session untouched.
        assert_eq!(s.id, id_before);
        assert_eq!(s.messages.len(), msg_count_before);
    }
}
