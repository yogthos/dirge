//! Schema-driven semantic repair. Split out of
//! `agent/agent_loop/tool_input_repair.rs` (dirge-4y4l stage 10b):
//! relational-defaults fill + markdown-auto-link unwrapping, driven by
//! `dirge-hints` / semantic-tag annotations on the JSON Schema.

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

use super::RepairKind;
use super::is_path_field_name;

/// Schema-driven relational-defaults pass. Reads a top-level
/// `dirge-hints.relational` array from the schema:
///
///     "dirge-hints": {
///       "relational": [
///         {"requires": ["offset", "limit"], "defaults": {"offset": 0}}
///       ]
///     }
///
/// Semantic: every entry declares a set of fields that should
/// be present together. When SOME but not all are present (the
/// "partial" case), the missing fields are filled from `defaults`
/// and a `Note:` is appended to `notes` so the model sees the
/// auto-fill. When ALL are present, or ALL are absent, the pass
/// is a no-op — the partial case is the only one that needs
/// repair.
///
/// Phase-2 of `docs/AGENTIC_LOOP_PLAN.md` — replaces the
/// hardcoded `read.rs:190` "Note: offset defaulted to 0" with a
/// schema-driven mechanism every tool can declare.
pub(super) fn apply_relational_defaults(schema: &Value, args: &mut Value, notes: &mut Vec<String>) {
    let Some(relational) = schema
        .get("dirge-hints")
        .and_then(|h| h.get("relational"))
        .and_then(|v| v.as_array())
    else {
        return;
    };
    let Value::Object(obj) = args else {
        return;
    };
    for entry in relational {
        let Some(requires) = entry.get("requires").and_then(|v| v.as_array()) else {
            continue;
        };
        let names: Vec<&str> = requires.iter().filter_map(|v| v.as_str()).collect();
        if names.is_empty() {
            continue;
        }
        let present: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| obj.contains_key(*n))
            .collect();
        // Only fire on the "partial" case — some required fields
        // present, others absent. Full-presence and full-absence
        // are both fine: the schema's own `required` list handles
        // the latter, and the LLM is acting consistently in either.
        if present.is_empty() || present.len() == names.len() {
            continue;
        }
        let defaults = entry.get("defaults").and_then(|d| d.as_object());
        let mut auto_filled: Vec<(String, Value)> = Vec::new();
        for name in &names {
            if obj.contains_key(*name) {
                continue;
            }
            let value = defaults
                .and_then(|d| d.get(*name))
                .cloned()
                .unwrap_or(Value::Null);
            if !value.is_null() {
                obj.insert((*name).to_string(), value.clone());
                auto_filled.push(((*name).to_string(), value));
            }
        }
        if !auto_filled.is_empty() {
            // One Note per relational entry, summarising every
            // field defaulted. Keeps the surface concise even
            // when multiple fields auto-filled together.
            let filled_desc: Vec<String> = auto_filled
                .iter()
                .map(|(n, v)| format!("{n}={v}"))
                .collect();
            let provided: Vec<&str> = present.to_vec();
            notes.push(format!(
                "Note: {} was provided but {} was not — defaulted to {}. To change, retry with all of [{}] set explicitly.",
                provided.join(", "),
                auto_filled
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                filled_desc.join(", "),
                names.join(", "),
            ));
        }
    }
}

// Compile the markdown link regex once.
static MD_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[(.+?)\]\((https?://[^\)]+)\)$").expect("md link regex must compile")
});

/// Try to unwrap a degenerate markdown auto-link.
///
/// Degenerate cases (model leaked chat formatting into a tool arg):
///   `[notes.md](http://notes.md)`     → `notes.md`
///   `[file.txt](https://file.txt)`    → `file.txt`
///   `[src/main.rs](https://example.com/src/main.rs)` → `src/main.rs`
///
/// Non-degenerate (real markdown — text and URL differ semantically):
///   `[click](https://example.com)`    → passes through untouched
///   `[link](http://other.com)`        → passes through untouched
///
/// Returns `Some(unwrapped)` if the value is a degenerate auto-link,
/// or `None` to leave it unchanged.
pub(super) fn unwrap_md_link(value: &str) -> Option<String> {
    let caps = MD_LINK_RE.captures(value)?;
    let link_text = caps.get(1)?.as_str();
    let raw_url = caps.get(2)?.as_str();

    // Strip protocol: "http://foo" or "https://foo" → "foo"
    let url_no_proto = raw_url
        .strip_prefix("http://")
        .or_else(|| raw_url.strip_prefix("https://"))
        .unwrap_or(raw_url);

    // Degenerate case 1: link text exactly equals URL without protocol.
    // e.g. [notes.md](http://notes.md)
    if link_text == url_no_proto {
        return Some(link_text.to_string());
    }

    // Degenerate case 2: link text is a suffix of the URL path.
    // e.g. [notes.md](http://example.com/sub/notes.md)
    if url_no_proto.ends_with(link_text)
        && (url_no_proto.ends_with(&format!("/{link_text}")) || url_no_proto == link_text)
    {
        return Some(link_text.to_string());
    }

    // Real markdown: text and URL are semantically different.
    None
}

/// Semantic tag for a single property, read from
/// `dirge-hints.semantic` (preferred) or `x-dirge-kind`
/// (back-compat). Drives the repair layer's targeted fixes —
/// e.g. `AbsolutePath` triggers md-link unwrap. Phase-2 of
/// `docs/AGENTIC_LOOP_PLAN.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticTag {
    /// Filesystem path that must be absolute. Subject to md-link
    /// unwrap; the call-site tool ALSO enforces `is_absolute()`.
    AbsolutePath,
    /// Filesystem path that may be relative. Subject to md-link
    /// unwrap. No absolute-path enforcement.
    RelativePath,
}

/// Read the semantic tag for a single property's schema node.
/// New schemas declare `dirge-hints: {"semantic": "absolute_path"}`.
/// Older schemas use `x-dirge-kind: "path"` — treated as
/// `AbsolutePath` for backward compatibility (every existing
/// tagged tool requires absolute paths). The field-name
/// heuristic (`path`/`paths`/`dir`/`filename`/`file_path`) still
/// applies as a fallback for tools that haven't been migrated.
fn extract_semantic_tag(key: &str, prop_schema: &Value) -> Option<SemanticTag> {
    // Preferred: dirge-hints.semantic
    if let Some(sem) = prop_schema
        .get("dirge-hints")
        .and_then(|h| h.get("semantic"))
        .and_then(|v| v.as_str())
    {
        return match sem {
            "absolute_path" => Some(SemanticTag::AbsolutePath),
            "relative_path" => Some(SemanticTag::RelativePath),
            _ => None,
        };
    }
    // Back-compat: x-dirge-kind: "path"
    if prop_schema.get("x-dirge-kind").and_then(|v| v.as_str()) == Some("path") {
        return Some(SemanticTag::AbsolutePath);
    }
    // Fallback: field-name heuristic. Any path-shaped name implies
    // absolute (every tool with these names today requires one).
    if is_path_field_name(key) {
        return Some(SemanticTag::AbsolutePath);
    }
    None
}

/// `true` when the property is path-shaped (either flavour).
fn is_path_field(key: &str, prop_schema: &Value) -> bool {
    matches!(
        extract_semantic_tag(key, prop_schema),
        Some(SemanticTag::AbsolutePath) | Some(SemanticTag::RelativePath)
    )
}

/// Walk args in parallel with the schema, applying `unwrap_md_link`
/// to every string value that lands in a path-typed field.
pub(super) fn unwrap_md_links_in_args(
    schema: &Value,
    args: &Value,
    kinds: &mut Vec<RepairKind>,
) -> Value {
    let mut result = args.clone();

    if let Value::Object(ref mut out) = result {
        let props = schema.get("properties");
        for (key, val) in out.iter_mut() {
            let prop_schema = props.and_then(|p| p.get(key));
            if let Some(ps) = prop_schema {
                if is_path_field(key, ps)
                    && let Value::String(s) = val
                    && let Some(unwrapped) = unwrap_md_link(s)
                {
                    *val = Value::String(unwrapped);
                    kinds.push(RepairKind::MdLinkUnwrapped);
                }
                // Recurse into nested objects.
                if let Value::Object(_) = val {
                    *val = unwrap_md_links_in_args(ps, val, kinds);
                }
                // Recurse into arrays whose items may contain path fields.
                if let Value::Array(arr) = val {
                    let items = ps.get("items");
                    for item in arr.iter_mut() {
                        if let Some(is) = items {
                            *item = unwrap_md_links_in_args(is, item, kinds);
                        }
                    }
                }
            }
        }
    }

    result
}
