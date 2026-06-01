//! Validate-then-repair orchestration. Split out of
//! `agent/agent_loop/tool_input_repair.rs` (dirge-4y4l stage 10b):
//! validates tool args against the JSON Schema and applies targeted
//! repairs at each failing path (null-strip, array coercion, md-link
//! unwrap, relational defaults).

use serde_json::Value;

use super::semantic::{apply_relational_defaults, unwrap_md_links_in_args};
use super::{RepairKind, RepairResult};

/// Pre-validate the arguments against the JSON Schema. If valid,
/// returns `Ok(None)`. If invalid, attempts targeted repairs at
/// each failing path. Returns `Ok(Some(RepairResult))` if repairs
/// succeeded, or `Err(Vec<String>)` with validation errors if
/// repairs could not fix the input.
pub fn validate_and_repair(
    schema: &Value,
    args: &Value,
) -> Result<Option<RepairResult>, Vec<String>> {
    let compiled = match jsonschema::validator_for(schema) {
        Ok(v) => v,
        Err(e) => {
            return Err(vec![format!("Schema compilation failed: {e}")]);
        }
    };

    let mut repaired = args.clone();
    let mut applied_kinds: Vec<RepairKind> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // dirge-7bwx: truncation pre-pass previously lived here.
    // Hoisted to `run.rs::apply_truncation_repair`, called between
    // scavenge merge and storm filter (Reasonix parity at
    // `repair/index.ts:88-121`). If args still arrive here as
    // `Value::String` it means the upstream closer hard-fellback
    // or wasn't run (test paths bypassing the loop); the
    // validate-and-return-Err contract below surfaces the failure
    // to the dispatcher, matching Reasonix's "leave the original
    // truncated args untouched" invariant at
    // `repair/index.ts:93-102`. The standalone
    // `try_truncation_repair` helper was removed — direct callers
    // can use `repair_truncated_json` instead.

    // 1. Content normalizers (run regardless of validation status).
    //    These fix well-known model output quirks that don't cause
    //    schema errors (e.g. md auto-links in path fields).
    //    Null-strip: remove null-valued optional keys. The recursive
    //    helper handles both Object and Array roots and walks into
    //    nested containers via the schema's properties / items.
    strip_null_recursive(&mut repaired, schema, &mut applied_kinds);

    // 2. Unwrap degenerate markdown auto-links in path fields.
    repaired = unwrap_md_links_in_args(schema, &repaired, &mut applied_kinds);

    // 2.5 Relational defaults (Phase-2). Fill missing fields when
    //     the schema's `dirge-hints.relational` declares
    //     "these belong together"; surface a Note: per fill.
    apply_relational_defaults(schema, &mut repaired, &mut notes);

    // 3. Validate. If the input is valid (possibly after content fixes),
    //    return it — no shape repair needed.
    //    Collect errors into strings first so we can release the
    //    immutable borrow on `repaired` before mutating it.
    let validation_errors: Vec<(String, String)> = compiled
        .iter_errors(&repaired)
        .map(|e| (e.instance_path().to_string(), e.to_string()))
        .collect();
    if validation_errors.is_empty() {
        if applied_kinds.is_empty() && notes.is_empty() {
            return Ok(None);
        }
        return Ok(Some(RepairResult {
            repaired,
            kinds: applied_kinds,
            notes,
        }));
    }

    // 4. Walk each validation error and attempt targeted shape repair.
    for (path_str, complaint) in &validation_errors {
        apply_repair_at_value(&mut repaired, path_str, complaint, &mut applied_kinds);
    }

    // 5. Re-validate.
    let remaining: Vec<_> = compiled.iter_errors(&repaired).collect();
    if remaining.is_empty() {
        Ok(Some(RepairResult {
            repaired,
            kinds: applied_kinds,
            notes,
        }))
    } else {
        let final_errors: Vec<String> = remaining
            .iter()
            .map(|e| format!("at {}: {e}", e.instance_path()))
            .collect();
        Err(final_errors)
    }
}

/// Strip null-valued keys from an object when the schema marks
/// the property as optional (not in `required`).
fn strip_null_optionals(
    obj: &mut serde_json::Map<String, Value>,
    schema: &Value,
    kinds: &mut Vec<RepairKind>,
) {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let properties = schema.get("properties");

    // Collect keys to remove (can't remove while iterating).
    let to_remove: Vec<String> = obj
        .iter()
        .filter(|(k, v)| v.is_null() && !required.contains(&k.as_str()))
        .map(|(k, _)| k.clone())
        .collect();

    for key in to_remove {
        obj.remove(&key);
        kinds.push(RepairKind::NullStripped);
    }

    // Recursively strip nulls in nested objects and arrays.
    for (key, value) in obj.iter_mut() {
        let child_schema = properties.and_then(|p| p.get(key));
        if let Value::Object(child) = value
            && let Some(cs) = child_schema
        {
            strip_null_optionals(child, cs, kinds);
        }
        if let Value::Array(arr) = value {
            let items_schema = child_schema.and_then(|cs| cs.get("items"));
            for item in arr.iter_mut() {
                if let Value::Object(child_obj) = item
                    && let Some(is) = items_schema
                {
                    strip_null_optionals(child_obj, is, kinds);
                }
            }
        }
    }
}

/// Walk the value tree for null-stripping at deeper levels (inside arrays).
fn strip_null_recursive(value: &mut Value, schema: &Value, kinds: &mut Vec<RepairKind>) {
    match value {
        Value::Object(obj) => {
            strip_null_optionals(obj, schema, kinds);
        }
        Value::Array(arr) => {
            let item_schema = schema.get("items");
            for item in arr.iter_mut() {
                if let Some(is) = item_schema {
                    strip_null_recursive(item, is, kinds);
                }
            }
        }
        _ => {}
    }
}

/// Walk to the value at a JSON Pointer path within `root` and attempt
/// the four shape repairs at that location.
fn apply_repair_at_value(
    root: &mut Value,
    path: &str,
    complaint: &str,
    kinds: &mut Vec<RepairKind>,
) {
    let parts = parse_json_pointer(path);
    if parts.is_empty() {
        try_repairs_at_value(root, complaint, kinds);
        return;
    }
    apply_repair_at_parts(root, &parts, 0, complaint, kinds);
}

fn apply_repair_at_parts(
    value: &mut Value,
    parts: &[String],
    idx: usize,
    complaint: &str,
    kinds: &mut Vec<RepairKind>,
) {
    if idx >= parts.len() {
        try_repairs_at_value(value, complaint, kinds);
        return;
    }

    let part = &parts[idx];
    match value {
        Value::Object(obj) => {
            if let Some(child) = obj.get_mut(part) {
                apply_repair_at_parts(child, parts, idx + 1, complaint, kinds);
            }
        }
        Value::Array(arr) => {
            if let Ok(i) = part.parse::<usize>()
                && let Some(child) = arr.get_mut(i)
            {
                apply_repair_at_parts(child, parts, idx + 1, complaint, kinds);
            }
        }
        _ => {}
    }
}

/// Apply shape repairs to the specific value node.
/// Repairs in exact order:
/// 1. JSON-string-as-array  (MUST be before bare-string-to-array)
/// 2. Empty-object-to-array
/// 3. Bare-string-to-singleton-array
fn try_repairs_at_value(value: &mut Value, complaint: &str, kinds: &mut Vec<RepairKind>) {
    let lower = complaint.to_lowercase();

    // 1. JSON-string-as-array
    if (lower.contains("array") || lower.contains("string"))
        && let Value::String(s) = value
    {
        let trimmed = s.trim();
        if trimmed.starts_with('[')
            && trimmed.ends_with(']')
            && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
            && parsed.is_array()
        {
            *value = parsed;
            kinds.push(RepairKind::JsonStringToArray);
            return;
        }
    }

    // 2. Empty-object-to-array
    if lower.contains("array")
        && let Value::Object(obj) = value
        && obj.is_empty()
    {
        *value = Value::Array(vec![]);
        kinds.push(RepairKind::ObjectToArray);
        return;
    }

    // 3. Bare-string-to-singleton-array
    if lower.contains("array")
        && let Value::String(s) = value.clone()
    {
        *value = Value::Array(vec![Value::String(s)]);
        kinds.push(RepairKind::BareStringToArray);
    }
}

/// Parse "/foo/0/bar~1baz" into ["foo", "0", "bar/baz"].
pub(super) fn parse_json_pointer(path: &str) -> Vec<String> {
    if path.is_empty() || path == "/" {
        return vec![];
    }
    path.trim_start_matches('/')
        .split('/')
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect()
}
