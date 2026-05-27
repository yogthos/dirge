//! Ctrl-F fuzzy search over the chat buffer and the rewind picker /
//! session-truncation helpers.
//!
//! Extracted from `ui/mod.rs`. These all sit AFTER the main
//! `run_interactive` body and only mutate session/picker state — no
//! cross-talk with the event loop other than the call sites.

use crate::session::{MessageRole, Session};
use crate::ui::colors::c_error;
use crate::ui::permission_ui;
use crate::ui::picker::ListPicker;
use crate::ui::renderer::Renderer;
use crate::ui::theme;

/// Whether a pattern was returned by `suggest_pattern` as the
/// "empty input — please type a real pattern" placeholder rather
/// than a real glob. Used by the ask-dialog to detect when the
/// user pressed "allow always" on a degenerate input and refuse
/// to store the placeholder as an actual allowlist entry.
pub(crate) fn is_placeholder_pattern(p: &str) -> bool {
    permission_ui::is_placeholder_pattern(p)
}

pub(crate) fn suggest_pattern(tool: &str, input: &str) -> String {
    permission_ui::suggest_pattern(tool, input)
}

/// Fuzzy match via `nucleo-matcher`, ranked by score descending so
/// the best matches surface first
/// (`maki-ui/src/components/search_modal.rs:147-185`).
/// Previously this was a `to_lowercase().contains()` substring filter
/// — it failed on typos, partial words, and out-of-order keystrokes
/// that fuzzy matching handles naturally.
///
/// Empty / whitespace-only queries clear the result set (same as
/// maki). Matching is case-insensitive with smart-case semantics:
/// lowercase query matches both cases; mixed-case query forces an
/// exact-case match — handled inside `Atom::new` with
/// `CaseMatching::Smart`.
#[cfg(test)]
pub(crate) fn update_search(
    renderer: &Renderer,
    query: &str,
    matches: &mut Vec<usize>,
    selected: &mut usize,
) {
    use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
    use nucleo_matcher::{Config, Matcher, Utf32Str};

    matches.clear();
    *selected = 0;
    if query.trim().is_empty() {
        return;
    }

    let atom = Atom::new(
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );
    let mut matcher = Matcher::new(Config::DEFAULT);
    let lines = renderer.buffer_lines();
    // Collect (line_idx, score) so we can sort by score descending
    // and keep the original buffer positions for Enter-to-scroll.
    let mut scored: Vec<(usize, u16)> = Vec::new();
    let mut buf = Vec::new();
    let mut indices = Vec::new();
    for (idx, text) in lines.iter().enumerate() {
        if text.is_empty() {
            continue;
        }
        buf.clear();
        indices.clear();
        let haystack = Utf32Str::new(text, &mut buf);
        if let Some(score) = atom.indices(haystack, &mut matcher, &mut indices) {
            scored.push((idx, score));
        }
    }
    // Higher score first; tie-break on earlier line for determinism.
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    *matches = scored.into_iter().map(|(idx, _)| idx).collect();
}

pub(crate) fn open_rewind_picker(session: &Session, picker: &mut ListPicker) {
    let prompts: Vec<String> = session
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .rev()
        .take(20)
        .map(|m| {
            let truncated: String = m.content.chars().take(80).collect();
            if truncated.chars().count() >= 80 {
                format!("{}...", truncated)
            } else {
                truncated
            }
        })
        .collect();
    picker.activate("Rewind to:", prompts);
}

pub(crate) fn rewind_session(
    session: &mut Session,
    idx: usize,
    renderer: &mut Renderer,
) -> anyhow::Result<()> {
    let user_indices: Vec<usize> = session
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == MessageRole::User)
        .map(|(i, _)| i)
        .collect();

    let target = user_indices.len().saturating_sub(idx + 1);
    if let Some(&msg_idx) = user_indices.get(target) {
        let removed = session.messages.len() - msg_idx;
        // Collect ids of the messages we're dropping BEFORE truncate
        // so we can also prune them from `tree.entries` and
        // `message_store`. Without this, the tree references
        // orphaned ids (no content in store), and subsequent
        // fork/clone/switch-to-leaf operations silently fail or
        // corrupt the session.
        let dropped_ids: Vec<_> = session.messages[msg_idx..]
            .iter()
            .map(|m| m.id.clone())
            .collect();
        session.messages.truncate(msg_idx);

        // Sibling-branch prune (Phase 2). Same logic as compress —
        // walk descendants of dropped ids and remove any forked
        // subtrees rooted on them. Active-path messages (still in
        // `session.messages` after truncate) are excluded.
        let dropped_set: std::collections::HashSet<_> = dropped_ids.iter().cloned().collect();
        let active_ids: std::collections::HashSet<_> =
            session.messages.iter().map(|m| m.id.clone()).collect();
        let mut to_prune = dropped_set.clone();
        loop {
            let new_ids: Vec<_> = session
                .tree
                .entries
                .iter()
                .filter(|(id, node)| {
                    !to_prune.contains(*id)
                        && !active_ids.contains(*id)
                        && node
                            .parent
                            .as_ref()
                            .map(|p| to_prune.contains(p))
                            .unwrap_or(false)
                })
                .map(|(id, _)| id.clone())
                .collect();
            if new_ids.is_empty() {
                break;
            }
            for id in new_ids {
                to_prune.insert(id);
            }
        }
        let pruned_siblings = to_prune.len().saturating_sub(dropped_set.len());

        // Phase 4: capture BranchSummary entries for each pruned
        // sibling subtree BEFORE removing nodes. Same algorithm as
        // `Session::compress_reporting` — root of a subtree is a
        // node in `to_prune` whose direct parent was in
        // `dropped_set` (the closest dropped-path ancestor). One
        // summary per subtree root, walking descendants for the
        // count.
        let now_rfc = chrono::Utc::now().to_rfc3339();
        let mut subtree_summaries: Vec<crate::session::BranchSummary> = Vec::new();
        for id in &to_prune {
            if dropped_set.contains(id) {
                continue;
            }
            let node = match session.tree.entries.get(id) {
                Some(n) => n,
                None => continue,
            };
            let parent = match &node.parent {
                Some(p) => p,
                None => continue,
            };
            if !dropped_set.contains(parent) {
                continue;
            }
            let mut count = 0usize;
            let mut stack = vec![id.clone()];
            while let Some(cur) = stack.pop() {
                if !to_prune.contains(&cur) {
                    continue;
                }
                count += 1;
                for (child_id, child_node) in session.tree.entries.iter() {
                    if child_node.parent.as_ref() == Some(&cur) {
                        stack.push(child_id.clone());
                    }
                }
            }
            let label_prefix = node
                .label
                .as_deref()
                .map(|l| format!("[{}] ", l))
                .unwrap_or_default();
            let body_preview = session
                .message_store
                .get(id)
                .map(|m| {
                    let s: String = m.content.chars().take(80).collect();
                    if m.content.chars().count() > 80 {
                        format!("{}…", s)
                    } else {
                        s
                    }
                })
                .unwrap_or_default();
            subtree_summaries.push(crate::session::BranchSummary {
                root_id: id.clone(),
                parent_id: parent.clone(),
                message_count: count,
                preview: format!("{}{}", label_prefix, body_preview),
                created_at: now_rfc.clone(),
            });
        }
        session.branch_summaries.extend(subtree_summaries);

        for id in &to_prune {
            session.tree.entries.remove(id);
            session.message_store.remove(id);
        }

        // Re-anchor `leaf_id` to the new tail (or None if everything
        // was dropped). Previously the leaf was left pointing at a
        // dropped id, which made `/tree` show a phantom branch.
        session.tree.leaf_id = session.messages.last().map(|m| m.id.clone());
        session.total_estimated_tokens = session.messages.iter().map(|m| m.estimated_tokens).sum();
        renderer.write_line(&format!("rewound {} message(s)", removed), theme::accent())?;
        if pruned_siblings > 0 {
            renderer.write_line(
                &format!(
                    "discarded {} forked branch node{} rooted in the rewound region",
                    pruned_siblings,
                    if pruned_siblings == 1 { "" } else { "s" },
                ),
                c_error(),
            )?;
        }
    }
    Ok(())
}
