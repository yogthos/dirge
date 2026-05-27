//! Schema flattening — prevent models from dropping deeply nested args.
//!
//! Faithful port of `DeepSeek-Reasonix/src/repair/flatten.ts` (92 lines).
//!
//! DeepSeek (and other models) drop arguments on schemas with >10 leaf
//! parameters or >2 levels of nesting. The fix:
//!
//! 1. `analyze_schema` — detect schemas that need flattening
//! 2. `flatten_schema` — present dot-notation keys to the model
//! 3. `nest_arguments` — re-nest flat args at dispatch time
//!
//! Example: `{user: {profile: {name: string}}}` becomes
//! `{"user.profile.name": string}`

use serde_json::Value;

/// Result of `analyze_schema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlattenDecision {
    pub should_flatten: bool,
    pub leaf_count: usize,
    pub max_depth: usize,
}

/// Walk the schema and decide whether flattening is needed.
/// Port of `analyzeSchema` (flatten.ts:11-24).
pub fn analyze_schema(schema: &Value) -> FlattenDecision {
    let mut leaf_count = 0;
    let mut max_depth = 0;
    walk(schema, 0, &mut leaf_count, &mut max_depth);
    FlattenDecision {
        should_flatten: leaf_count > 10 || max_depth > 2,
        leaf_count,
        max_depth,
    }
}

/// Flatten a nested schema to dot-notation.
/// Port of `flattenSchema` (flatten.ts:26-35).
///
/// Returns the input unchanged if it's not a deep/wide schema.
pub fn flatten_schema(schema: &Value) -> Value {
    debug_assert!(
        schema.get("type").and_then(|v| v.as_str()) == Some("object"),
        "flatten_schema precondition: root schema must have type=object"
    );
    let mut flat_props = serde_json::Map::new();
    let mut required: Vec<String> = Vec::new();
    collect("", schema, &mut flat_props, &mut required, true);
    let required_json: Vec<Value> = required.into_iter().map(Value::String).collect();
    serde_json::json!({
        "type": "object",
        "properties": flat_props,
        "required": required_json,
    })
}

/// Escape dots in property names so `flatten_schema` →
/// `nest_arguments` round-trips correctly even when original keys
/// contain `.` characters.  `\.` → literal dot.
const ESCAPED_DOT: &str = "\\.";
const DOT_PLACEHOLDER: &str = "\x1E";

/// Re-nest flat dot-notation args back into the original nested shape.
/// Handles escaped dots in property names (produced by `flatten_schema`).
/// Port of `nestArguments` (flatten.ts:37-43).
pub fn nest_arguments(flat_args: &Value) -> Value {
    match flat_args {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                let path: Vec<String> = split_flat_key(key);
                set_by_path(
                    &mut out,
                    path.iter().map(|s| s.as_str()).collect(),
                    value.clone(),
                );
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Split a flat key on `.`, respecting `\\.` escape sequences.
fn split_flat_key(key: &str) -> Vec<String> {
    // Replace escaped dots with a placeholder, split on dot, restore.
    let with_placeholder = key.replace(ESCAPED_DOT, DOT_PLACEHOLDER);
    with_placeholder
        .split('.')
        .map(|s| s.replace(DOT_PLACEHOLDER, "."))
        .collect()
}

// ---- internal helpers ----

/// Walk the schema tree, counting leaves and tracking max depth.
/// Port of `walk` (flatten.ts:45-61).
#[allow(clippy::collapsible_if)]
fn walk(schema: &Value, depth: usize, leaf_count: &mut usize, max_depth: &mut usize) {
    let ty = schema.get("type").and_then(|v| v.as_str()).unwrap_or("");

    if ty == "object" {
        if let Some(props) = schema.get("properties").and_then(|v| v.as_object()) {
            for child in props.values() {
                walk(child, depth + 1, leaf_count, max_depth);
            }
            return;
        }
    }

    if ty == "array" {
        if let Some(items) = schema.get("items") {
            walk(items, depth + 1, leaf_count, max_depth);
            return;
        }
    }

    // Leaf: any non-object, non-array type.
    *leaf_count += 1;
    if depth > *max_depth {
        *max_depth = depth;
    }
}

/// Collect dot-path → leaf-schema mappings.
/// Port of `collect` (flatten.ts:63-82).
#[allow(clippy::collapsible_if)]
fn collect(
    prefix: &str,
    schema: &Value,
    out: &mut serde_json::Map<String, Value>,
    required: &mut Vec<String>,
    is_root_required: bool,
) {
    let ty = schema.get("type").and_then(|v| v.as_str()).unwrap_or("");

    if ty == "object" {
        if let Some(props) = schema.get("properties").and_then(|v| v.as_object()) {
            let required_set: Vec<&str> = schema
                .get("required")
                .and_then(|r| r.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            for (key, child) in props {
                let escaped_key = key.replace('.', ESCAPED_DOT);
                let next_prefix = if prefix.is_empty() {
                    escaped_key
                } else {
                    format!("{prefix}.{escaped_key}")
                };
                let child_required = is_root_required && required_set.contains(&key.as_str());
                collect(&next_prefix, child, out, required, child_required);
            }
            return;
        }
    }

    // Treat anything non-object (including arrays) as a leaf.
    out.insert(prefix.to_string(), schema.clone());
    if is_root_required {
        required.push(prefix.to_string());
    }
}

/// Set a value at a dot-path inside a nested JSON object.
/// Port of `setByPath` (flatten.ts:84-92).
///
/// Gracefully handles conflicting flat/nested keys: if an intermediate
/// path segment is already a non-object value, it is overwritten with
/// an object so the deeper key can nest inside (the model sent both a
/// leaf value and a subtree at the same path prefix).
fn set_by_path(target: &mut serde_json::Map<String, Value>, path: Vec<&str>, value: Value) {
    // LOOP-1: handle empty path gracefully — an adversarial flattened
    // tool input can produce an empty path slice, which would
    // underflow `path.len() - 1` below.
    if path.is_empty() {
        tracing::warn!("schema_flatten: set_by_path called with empty path — skipping");
        return;
    }
    let mut cur = target;
    let last = path.len() - 1;
    for (i, key) in path.iter().enumerate() {
        if i == last {
            cur.insert(key.to_string(), value.clone());
        } else {
            let needs_object = cur.get(&key.to_string()).map_or(true, |v| !v.is_object());
            if needs_object {
                // Conflicting or missing intermediate — overwrite with object.
                if cur.get(&key.to_string()).is_some() {
                    tracing::warn!(
                        "schema_flatten: key \"{key}\" was a non-object, overwriting to nest deeper keys"
                    );
                }
                cur.insert(key.to_string(), Value::Object(serde_json::Map::new()));
            }
            // LOOP-1: replace the .expect with a graceful skip.
            // If the key cannot be obtained as an object (race
            // between the check and the get_mut, or an adversarial
            // schema that inserts a non-object between our insert
            // and read), skip instead of panicking.
            cur = match cur
                .get_mut(&key.to_string())
                .and_then(|v| v.as_object_mut())
            {
                Some(obj) => obj,
                None => {
                    tracing::warn!(
                        "schema_flatten: key \"{key}\" could not be resolved as object, skipping subtree"
                    );
                    return;
                }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // analyze_schema — ported from flatten.test.ts
    // ============================================================

    #[test]
    fn does_not_flatten_flat_shallow_schemas() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "number"},
            }
        });
        let d = analyze_schema(&schema);
        assert!(!d.should_flatten);
    }

    #[test]
    fn flags_deep_schemas() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {
                        "middle": {
                            "type": "object",
                            "properties": {
                                "leaf": {"type": "string"}
                            }
                        }
                    }
                }
            }
        });
        let d = analyze_schema(&schema);
        assert!(d.should_flatten);
        assert!(d.max_depth > 2);
    }

    #[test]
    fn flags_wide_schemas_over_10_leaves() {
        let mut props = serde_json::Map::new();
        for i in 0..12 {
            props.insert(format!("p{i}"), serde_json::json!({"type": "string"}));
        }
        let schema = serde_json::json!({
            "type": "object",
            "properties": props,
        });
        let d = analyze_schema(&schema);
        assert!(d.should_flatten);
        assert_eq!(d.leaf_count, 12);
    }

    // ============================================================
    // flatten_schema / nest_arguments roundtrip
    // ============================================================

    #[test]
    fn flattens_nested_schema_and_renests_arguments() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["user"],
            "properties": {
                "user": {
                    "type": "object",
                    "required": ["profile"],
                    "properties": {
                        "profile": {
                            "type": "object",
                            "required": ["name"],
                            "properties": {
                                "name": {"type": "string"},
                                "age": {"type": "integer"},
                            }
                        }
                    }
                }
            }
        });

        let flat = flatten_schema(&schema);
        assert!(flat["properties"].get("user.profile.name").is_some());
        assert!(flat["properties"].get("user.profile.age").is_some());

        let req: Vec<&str> = flat["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(req, vec!["user.profile.name"]);

        let nested = nest_arguments(&serde_json::json!({
            "user.profile.name": "alice",
            "user.profile.age": 30,
        }));
        assert_eq!(
            nested,
            serde_json::json!({
                "user": {
                    "profile": {
                        "name": "alice",
                        "age": 30,
                    }
                }
            })
        );
    }

    // ============================================================
    // Additional edge cases
    // ============================================================

    #[test]
    fn undefined_schema_does_not_flatten() {
        let d = analyze_schema(&serde_json::json!({}));
        assert!(!d.should_flatten);
        // Empty object with no type field falls through to leaf counting
        // (same as Reasonix — `undefined !== "object"`).
    }

    #[test]
    fn array_items_are_leaves_for_flattening() {
        // Arrays are treated as leaf nodes — we don't descend into them
        // for flattening purposes (same as Reasonix).
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "tags": {
                    "type": "array",
                    "items": {"type": "string"}
                }
            }
        });
        let d = analyze_schema(&schema);
        assert!(!d.should_flatten);
        assert_eq!(d.leaf_count, 1); // tags array is one leaf
    }

    #[test]
    fn nest_arguments_preserves_non_object_input() {
        assert_eq!(nest_arguments(&Value::Null), Value::Null);
        assert_eq!(
            nest_arguments(&Value::String("hello".into())),
            Value::String("hello".into())
        );
        assert_eq!(nest_arguments(&Value::Array(vec![])), Value::Array(vec![]));
    }

    #[test]
    fn nest_arguments_handles_deep_paths() {
        let nested = nest_arguments(&serde_json::json!({
            "a.b.c.d": "deep",
            "a.b.c.e": 42,
        }));
        assert_eq!(
            nested,
            serde_json::json!({
                "a": {
                    "b": {
                        "c": {
                            "d": "deep",
                            "e": 42,
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn dots_in_property_names_roundtrip() {
        // Property names with literal dots must survive flatten/renest.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "user.name": {"type": "string"},
                "profile": {
                    "type": "object",
                    "properties": {
                        "a.b": {"type": "string"}
                    }
                }
            }
        });
        let flat = flatten_schema(&schema);
        // "user.name" should be escaped as "user\\.name" in flat schema.
        assert!(flat["properties"].get("user\\.name").is_some());
        // Nested should be "profile.a\\.b".
        assert!(flat["properties"].get("profile.a\\.b").is_some());

        // Renest: the flat key "user\\.name" recreates {"user.name": value}
        let nested = nest_arguments(&serde_json::json!({
            "user\\.name": "alice",
            "profile.a\\.b": "hello",
        }));
        assert_eq!(
            nested,
            serde_json::json!({
                "user.name": "alice",
                "profile": {
                    "a.b": "hello",
                }
            })
        );
    }

    #[test]
    fn set_by_path_handles_conflicting_flat_and_nested_keys() {
        // Model sends both "a.b" as string and "a.b.c" as value.
        // Should not panic; deeper key overwrites intermediate.
        let result = nest_arguments(&serde_json::json!({
            "a.b": "string_value",
            "a.b.c": "deeper",
        }));
        assert_eq!(
            result,
            serde_json::json!({
                "a": {
                    "b": {
                        "c": "deeper",
                    }
                }
            })
        );
    }
}
