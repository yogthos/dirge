//! Shared helpers used by every `LanguageAdapter` impl.
//!
//! Each adapter previously defined its own `text()`, `range()`,
//! `signature_line()`, and `find_node_at_range()` — 7 copies of the
//! same 4 functions with mild drift between them. This module is the
//! one place all adapters call into.

use tree_sitter::Node;

/// Decode a node's source text. Falls back to `""` on UTF-8 errors
/// (rare in practice — tree-sitter parses bytes, but the source
/// could contain malformed sequences if a file slipped through the
/// binary-detection guard).
///
/// The fallback is logged at `debug` so users running with
/// `--verbose` (which enables `dirge=debug`) see them, while
/// default-log users don't get flooded by them. UTF-8 failures
/// here aren't load-bearing: an empty symbol name is filtered out
/// downstream; this log line exists purely so the issue surfaces
/// when someone is investigating "why is this symbol missing".
pub fn node_text<'a>(node: Node<'a>, source: &'a [u8]) -> &'a str {
    match node.utf8_text(source) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                start = node.start_byte(),
                end = node.end_byte(),
                kind = node.kind(),
                error = %e,
                "semantic: utf8_text failed; falling back to empty string",
            );
            ""
        }
    }
}

/// Walk a node's descendants looking for one whose byte range
/// exactly matches `[start, end)`. Used by `find_callees_in_range`
/// to re-find the target after a fresh parse. Returns `None` if no
/// match exists.
///
/// **Note on ambiguity:** if multiple nested nodes span the same
/// range (e.g., a `function_item` and its inner `block`), this
/// returns the OUTER node (first match in depth-first order). That
/// matches what callers want: a query over the function body is
/// rooted at the function node, not at its inner block — the query
/// itself filters to the relevant constructs.
pub fn find_node_at_range<'a>(n: Node<'a>, start: usize, end: usize) -> Option<Node<'a>> {
    if n.start_byte() == start && n.end_byte() == end {
        return Some(n);
    }
    for i in 0..n.named_child_count() {
        if let Some(c) = n.named_child(i)
            && c.start_byte() <= start
            && c.end_byte() >= end
            && let Some(f) = find_node_at_range(c, start, end)
        {
            return Some(f);
        }
    }
    None
}

/// First line of `node`'s text, capped at 80 display chars (with an
/// ellipsis if longer). Used for symbol signatures — long
/// signatures clutter the list_symbols output and don't help the
/// LLM understand the symbol.
///
/// Note: counts CHARS not bytes (UTF-8-safe), but doesn't account
/// for display width (emoji / CJK). Source code is overwhelmingly
/// ASCII so the simpler char-count is fine.
pub fn signature_first_line(node: Node, source: &[u8]) -> String {
    let text = node_text(node, source);
    let first = text.lines().next().unwrap_or(text);
    if first.chars().count() > 80 {
        let prefix: String = first.chars().take(80).collect();
        format!("{prefix}…")
    } else {
        first.to_string()
    }
}

/// Build a signature from the node's leading prefix up to its body.
/// For function-shaped nodes (`function_definition`, `function_item`,
/// `method_declaration`, …) the body is exposed via a `body` field
/// name; this returns everything before it, trimmed. Falls back to
/// the first-line-capped form when no `body` field exists. Used by
/// adapters whose signature concept is "the declarator + return
/// type, minus the body" — Go, C, C++, Rust.
#[cfg(any(
    feature = "semantic-go",
    feature = "semantic-c",
    feature = "semantic-cpp",
    feature = "semantic-rust"
))]
pub fn signature_up_to_body(node: Node, source: &[u8]) -> String {
    if let Some(body) = node.child_by_field_name("body") {
        return String::from_utf8_lossy(&source[node.start_byte()..body.start_byte()])
            .trim()
            .to_string();
    }
    signature_first_line(node, source)
}

#[cfg(test)]
mod tests {
    /// `ByteRange::from(Node)` produces the right line numbers
    /// (1-based) and byte offsets. Sanity check the shared
    /// converter that replaces 7 copies of the same body.
    #[test]
    #[cfg(feature = "semantic-clojure")]
    fn byte_range_from_node_uses_1_based_lines() {
        use crate::semantic::types::ByteRange;
        use tree_sitter::Parser;

        let mut p = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_clojure::LANGUAGE.into();
        p.set_language(&lang).unwrap();
        let src = "\n(defn foo [] :ok)\n";
        let t = p.parse(src, None).unwrap();
        // First named child of `source` is the defn list_lit.
        let list_lit = t.root_node().named_child(0).unwrap();
        let range = ByteRange::from(list_lit);
        // defn is on line 2 (1-based) after the leading \n.
        assert_eq!(range.start_line, 2);
        assert_eq!(range.end_line, 2);
        assert_eq!(range.start_byte, 1);
    }
}
