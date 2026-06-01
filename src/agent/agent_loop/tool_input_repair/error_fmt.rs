//! Structured validation-error formatting. Split out of
//! `agent/agent_loop/tool_input_repair.rs` (dirge-4y4l stage 10b):
//! turns a validation failure into a model-readable retry hint
//! (Expected / Got / Try).

use serde_json::Value;

use super::validate::parse_json_pointer;

/// Produce a model-readable retry hint from a validation failure.
///
/// Format:
/// ```text
/// Tool input rejected: <plain English summary>
/// Expected: <schema slice>
/// Got:      <truncated value>
/// Try:      <one concrete hint>
/// ```
pub fn format_structured_error(schema: &Value, args: &Value, errors: &[String]) -> String {
    let summary = errors.join("; ");
    let args_str = serde_json::to_string(args).unwrap_or_default();
    let truncated = if args_str.len() > 200 {
        format!("{}…", crate::text::head(&args_str, 200))
    } else {
        args_str
    };

    let schema_hint = extract_schema_hint(schema, errors);
    let concrete_hint = build_concrete_hint(errors);

    format!(
        "Tool input rejected: {summary}\n\
         Expected: {schema_hint}\n\
         Got:      {truncated}\n\
         Try:      {concrete_hint}"
    )
}

fn extract_schema_hint(schema: &Value, errors: &[String]) -> String {
    for err in errors {
        if let Some(path_start) = err.strip_prefix("at /") {
            let path = path_start.split(':').next().unwrap_or(path_start).trim();
            let parts = parse_json_pointer(&format!("/{path}"));
            if let Some(prop_schema) = navigate_schema(schema, &parts) {
                return serde_json::to_string(prop_schema)
                    .unwrap_or_else(|_| "(schema unavailable)".into());
            }
        }
    }
    "(see tool schema)".into()
}

/// Walk a JSON Schema along a parsed JSON Pointer path. Each path
/// segment is either an object property (looked up via `properties`)
/// or a numeric array index (descended via `items`). Returns the
/// schema node at the requested path, or `None` if any segment can't
/// be resolved.
///
/// Tested via `navigate_schema_descends_into_array_items` —
/// a `/edits/0/path` style pointer reaches the per-item `path`
/// schema rather than falling back to the default "(see tool
/// schema)" hint.
pub(super) fn navigate_schema<'a>(schema: &'a Value, parts: &[String]) -> Option<&'a Value> {
    let mut current = schema;
    for part in parts {
        if part.parse::<usize>().is_ok() {
            // Numeric index — the parent schema must describe an
            // array; descend into its `items`.
            current = current.get("items")?;
        } else {
            current = current.get("properties")?.get(part)?;
        }
    }
    Some(current)
}

pub(super) fn build_concrete_hint(errors: &[String]) -> String {
    for err in errors {
        let lower = err.to_lowercase();
        if lower.contains("null") {
            return "Remove the null value — the field is not required".into();
        }
        if lower.contains("array") && lower.contains("string") {
            return "Wrap the value in square brackets to make it an array".into();
        }
        if lower.contains("array") && lower.contains("object") {
            return "Replace {} with [] (empty array)".into();
        }
        if lower.contains("array") {
            return "The value should be an array, e.g. wrap it in square brackets".into();
        }
        if lower.contains("missing") {
            return "Make sure all required fields are present".into();
        }
    }
    "Check the tool schema and retry with valid arguments".into()
}

/// Detect whether a field name looks like a filesystem path.
/// Used by Phase 2 (markdown auto-link unwrap).
pub fn is_path_field_name(key: &str) -> bool {
    matches!(key, "path" | "file_path" | "filename" | "paths" | "dir")
}
