//! ASCII tree rendering + id-prefix resolution for the `/tree`,
//! `/fork`, `/clone` slash commands. Pure functions over the
//! `Session` tree so the rendering is easy to test without spinning
//! up the UI.

use compact_str::CompactString;

use crate::session::{MessageRole, Session, TreeNode};

/// Display only the first 8 chars of a node id. Full UUIDs are
/// noisy in chat output; the prefix is uniquely-identifiable in any
/// realistic session size.
pub fn short_id(id: &CompactString) -> String {
    id.chars().take(8).collect()
}

/// Resolve a short id prefix the user typed (e.g. "abc12345") to a
/// full node id in the session tree. Returns Err if no match or if
/// the prefix is ambiguous (>1 match). Empty prefix is treated as
/// "no match" so callers handle the "no arg" case first.
pub fn resolve_id_prefix(session: &Session, prefix: &str) -> Result<CompactString, String> {
    if prefix.is_empty() {
        return Err("missing id prefix".to_string());
    }
    let matches: Vec<&CompactString> = session
        .tree
        .entries
        .keys()
        .filter(|id| id.starts_with(prefix))
        .collect();
    match matches.as_slice() {
        [] => Err(format!("no entry id starts with '{}'", prefix)),
        [only] => Ok((*only).clone()),
        many => Err(format!(
            "ambiguous prefix '{}' matches {} entries (try a longer prefix)",
            prefix,
            many.len()
        )),
    }
}

/// Render the session tree as a list of ASCII lines. Walks children
/// of the root depth-first, drawing branch glyphs in the left margin.
/// Marks the current leaf with a `*` and labeled nodes with their
/// label.
///
/// Format:
///   abc12345  user      "first prompt"
///     ↳ def67890  asst   "first response"
///       ↳ * 11223344  user "follow-up" [bookmark]
///     ↳ aa667788  asst   "alternate response (forked)"
pub fn render_tree(session: &Session) -> Vec<String> {
    let mut out = Vec::new();
    if session.tree.entries.is_empty() {
        return out;
    }
    let leaf = session.tree.leaf_id.as_ref();
    // Find roots (parent = None).
    let mut roots: Vec<&TreeNode> = session
        .tree
        .entries
        .values()
        .filter(|n| n.parent.is_none())
        .collect();
    roots.sort_by_key(|n| n.timestamp);
    for root in roots {
        render_subtree(session, root, 0, leaf, &mut out);
    }
    // Phase 4: append a "Summarized branches" section if any
    // forked subtrees were pruned during prior compress / rewind.
    // Each entry shows the parent id (which may itself be pruned by
    // now), the count, and the preview captured at prune time.
    // Without this, users only see "discarded N branches" in the
    // moment but lose access to what was in them — defeats the
    // point of pi-style preservation.
    if !session.branch_summaries.is_empty() {
        out.push(String::new());
        out.push(format!(
            "Summarized branches ({}): pruned during compress/rewind",
            session.branch_summaries.len(),
        ));
        for bs in &session.branch_summaries {
            out.push(format!(
                "  └─ parent {} · {} msg{} · {}",
                short_id(&bs.parent_id),
                bs.message_count,
                if bs.message_count == 1 { "" } else { "s" },
                bs.preview,
            ));
        }
    }
    out
}

fn render_subtree(
    session: &Session,
    node: &TreeNode,
    depth: usize,
    leaf: Option<&CompactString>,
    out: &mut Vec<String>,
) {
    let indent = "  ".repeat(depth);
    let marker = if Some(&node.id) == leaf { "*" } else { " " };
    let role = session
        .message_store
        .get(&node.id)
        .map(|m| role_label(m.role))
        .unwrap_or("???");
    let preview: String = session
        .message_store
        .get(&node.id)
        .map(|m| {
            let s: String = m.content.chars().take(50).collect();
            if m.content.chars().count() > 50 {
                format!("{}…", s)
            } else {
                s
            }
        })
        .unwrap_or_else(|| "(content missing)".to_string());
    let preview_one_line = preview.replace('\n', " ").trim().to_string();
    let label_suffix = node
        .label
        .as_deref()
        .map(|l| format!(" [{}]", l))
        .unwrap_or_default();
    out.push(format!(
        "{}{} {} {}  {:?}{}",
        indent,
        marker,
        short_id(&node.id),
        role,
        preview_one_line,
        label_suffix,
    ));
    // Children, sorted by timestamp so the order is deterministic.
    let mut children: Vec<&TreeNode> = session
        .tree
        .entries
        .values()
        .filter(|n| n.parent.as_ref() == Some(&node.id))
        .collect();
    children.sort_by_key(|n| n.timestamp);
    for child in children {
        render_subtree(session, child, depth + 1, leaf, out);
    }
}

fn role_label(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "asst",
        MessageRole::System => "sys ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    #[test]
    fn resolve_unique_prefix_returns_full_id() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "msg");
        let full_id = s.messages[0].id.clone();
        let prefix: String = full_id.chars().take(8).collect();
        let resolved = resolve_id_prefix(&s, &prefix).unwrap();
        assert_eq!(resolved, full_id);
    }

    #[test]
    fn resolve_empty_prefix_errors() {
        let s = Session::new("p", "m", 0);
        let err = resolve_id_prefix(&s, "").unwrap_err();
        assert!(err.contains("missing id prefix"));
    }

    #[test]
    fn resolve_no_match_errors() {
        let s = Session::new("p", "m", 0);
        let err = resolve_id_prefix(&s, "00000000").unwrap_err();
        assert!(err.contains("no entry id"), "got: {err}");
    }

    /// An empty session renders to no lines (caller handles the
    /// "(empty session)" message itself).
    #[test]
    fn render_tree_empty_session_is_empty() {
        let s = Session::new("p", "m", 0);
        assert!(render_tree(&s).is_empty());
    }

    /// A linear session renders one line per message, indented by
    /// depth. The leaf gets a `*` marker.
    #[test]
    fn render_tree_linear_session() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "first");
        s.add_message(MessageRole::Assistant, "reply");
        let lines = render_tree(&s);
        assert_eq!(lines.len(), 2);
        // Root has no `*` (it's not the leaf).
        assert!(lines[0].contains("user"));
        assert!(!lines[0].contains(" *"));
        // Indented child is the leaf.
        assert!(lines[1].starts_with("  *"));
        assert!(lines[1].contains("asst"));
    }

    /// Forking creates two children of the same parent; both render
    /// at the same indent level beneath their shared root.
    #[test]
    fn render_tree_branched_session() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "Q");
        s.add_message(MessageRole::Assistant, "original");
        let original_assistant_id = s.messages[1].id.clone();
        // Fork at the assistant message — moves leaf to user message.
        s.fork_at(&original_assistant_id).unwrap();
        s.add_message(MessageRole::Assistant, "alternate");
        let alt_id = s.tree.leaf_id.clone().unwrap();
        let lines = render_tree(&s);
        // Three lines total: user (root) + original + alternate.
        assert_eq!(lines.len(), 3);
        // Both assistant lines indent under the user.
        let assistant_lines: Vec<_> = lines.iter().filter(|l| l.contains("asst")).collect();
        assert_eq!(assistant_lines.len(), 2);
        // The leaf marker `*` only appears on the active (alternate)
        // branch.
        let starred: Vec<_> = lines.iter().filter(|l| l.contains(" * ")).collect();
        assert_eq!(starred.len(), 1);
        let alt_prefix = short_id(&alt_id);
        assert!(starred[0].contains(&alt_prefix), "got: {starred:?}");
    }

    /// Labels render as `[label]` suffix on the matching node line.
    #[test]
    fn render_tree_includes_labels() {
        let mut s = Session::new("p", "m", 0);
        s.add_message(MessageRole::User, "milestone");
        let id = s.messages[0].id.clone();
        s.set_label(&id, Some("checkpoint".to_string())).unwrap();
        let lines = render_tree(&s);
        assert!(lines[0].contains("[checkpoint]"), "got: {:?}", lines[0]);
    }
}
