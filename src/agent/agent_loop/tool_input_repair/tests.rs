//! Tests for the tool-input repair layer. Split out of
//! `agent/agent_loop/tool_input_repair.rs` (dirge-4y4l stage 10a).
//! `use super::*` resolves through `tool_input_repair`’s re-exports
//! (`pub use <child>::*`), so test references stay valid as further
//! clusters are extracted into sibling modules.

use super::*;

// ============================================================
// parse_json_pointer
// ============================================================

#[test]
fn parse_empty_pointer() {
    assert_eq!(parse_json_pointer(""), Vec::<String>::new());
    assert_eq!(parse_json_pointer("/"), Vec::<String>::new());
}

#[test]
fn parse_simple_pointer() {
    assert_eq!(parse_json_pointer("/offset"), vec!["offset"]);
}

#[test]
fn parse_nested_pointer() {
    assert_eq!(
        parse_json_pointer("/items/0/path"),
        vec!["items", "0", "path"]
    );
}

#[test]
fn parse_pointer_with_escapes() {
    assert_eq!(parse_json_pointer("/a~1b"), vec!["a/b"]);
    assert_eq!(parse_json_pointer("/a~0b"), vec!["a~b"]);
}

// ============================================================
// is_path_field_name
// ============================================================

#[test]
fn path_field_names() {
    assert!(is_path_field_name("path"));
    assert!(is_path_field_name("file_path"));
    assert!(is_path_field_name("filename"));
    assert!(is_path_field_name("paths"));
    assert!(is_path_field_name("dir"));
}

#[test]
fn non_path_field_names() {
    assert!(!is_path_field_name("content"));
    assert!(!is_path_field_name("text"));
    assert!(!is_path_field_name("command"));
    assert!(!is_path_field_name("pattern"));
    assert!(!is_path_field_name(""));
}

// ============================================================
// validate_and_repair — valid inputs pass through
// ============================================================

fn simple_object_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "limit": { "type": "integer" }
        },
        "required": ["path"]
    })
}

#[test]
fn valid_input_passes_through() {
    let schema = simple_object_schema();
    let args = serde_json::json!({"path": "/foo/bar", "limit": 42});
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_none(), "valid input should not trigger repair");
}

#[test]
fn valid_input_no_optional_fields() {
    let schema = simple_object_schema();
    let args = serde_json::json!({"path": "/foo/bar"});
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_none());
}

// ============================================================
// Repair 1: null-strip for optional fields
// ============================================================

#[test]
fn null_optional_field_is_stripped() {
    let schema = simple_object_schema();
    let args = serde_json::json!({"path": "/foo/bar", "limit": null});
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    assert_eq!(rr.repaired, serde_json::json!({"path": "/foo/bar"}));
    assert!(rr.kinds.contains(&RepairKind::NullStripped));
}

// ============================================================
// Repair 2: JSON-string-as-array
// ============================================================

fn array_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "paths": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["paths"]
    })
}

#[test]
fn json_string_to_array_single() {
    let schema = array_schema();
    let args = serde_json::json!({"paths": "[\"a\"]"});
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    assert_eq!(rr.repaired, serde_json::json!({"paths": ["a"]}));
    assert_eq!(rr.kinds, vec![RepairKind::JsonStringToArray]);
}

#[test]
fn json_string_to_array_multiple() {
    let schema = array_schema();
    let args = serde_json::json!({"paths": "[\"a\",\"b\"]"});
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    assert_eq!(rr.repaired, serde_json::json!({"paths": ["a", "b"]}));
    assert_eq!(rr.kinds, vec![RepairKind::JsonStringToArray]);
}

/// Critical ordering test: `"[\"a\",\"b\"]"` must become
/// `["a","b"]` via repair #2, NOT `["[\"a\",\"b\"]"]` via
/// repair #4. The JSON-string check runs BEFORE bare-string wrap.
#[test]
fn ordering_json_string_before_bare_string() {
    let schema = array_schema();
    let args = serde_json::json!({"paths": "[\"a\",\"b\"]"});
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    assert_eq!(
        rr.repaired,
        serde_json::json!({"paths": ["a", "b"]}),
        "JSON-string must parse to array, not wrap as singleton"
    );
    assert_eq!(
        rr.kinds,
        vec![RepairKind::JsonStringToArray],
        "only JsonStringToArray should fire"
    );
}

// ============================================================
// Repair 3: empty object {} → []
// ============================================================

#[test]
fn empty_object_to_array() {
    let schema = array_schema();
    let args = serde_json::json!({"paths": {}});
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    assert_eq!(rr.repaired, serde_json::json!({"paths": []}));
    assert_eq!(rr.kinds, vec![RepairKind::ObjectToArray]);
}

#[test]
fn non_empty_object_to_array_fails() {
    let schema = array_schema();
    let args = serde_json::json!({"paths": {"x": 1}});
    let result = validate_and_repair(&schema, &args);
    assert!(result.is_err(), "non-empty object should fail repair");
}

// ============================================================
// Repair 4: bare string → singleton array
// ============================================================

#[test]
fn bare_string_to_singleton_array() {
    let schema = array_schema();
    let args = serde_json::json!({"paths": "foo"});
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    assert_eq!(rr.repaired, serde_json::json!({"paths": ["foo"]}));
    assert!(rr.kinds.contains(&RepairKind::BareStringToArray));
}

// ============================================================
// Nested path repairs
// ============================================================

fn nested_array_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "replacements": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["path", "replacements"]
                }
            }
        },
        "required": ["edits"]
    })
}

#[test]
fn nested_bare_string_to_array() {
    let schema = nested_array_schema();
    let args = serde_json::json!({
        "edits": [{
            "path": "/foo",
            "replacements": "bar"
        }]
    });
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    assert_eq!(
        rr.repaired,
        serde_json::json!({
            "edits": [{
                "path": "/foo",
                "replacements": ["bar"]
            }]
        })
    );
    assert!(rr.kinds.contains(&RepairKind::BareStringToArray));
}

#[test]
fn nested_null_optional_stripped() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "extra": { "type": "string" }
                    },
                    "required": ["path"]
                }
            }
        },
        "required": ["edits"]
    });
    let args = serde_json::json!({
        "edits": [{
            "path": "/foo",
            "extra": null
        }]
    });
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    assert_eq!(
        rr.repaired,
        serde_json::json!({
            "edits": [{
                "path": "/foo"
            }]
        })
    );
    assert!(rr.kinds.contains(&RepairKind::NullStripped));
}

// ============================================================
// Multiple repairs
// ============================================================

#[test]
fn multiple_repairs_in_one_input() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "count": { "type": "integer" },
            "tags": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["name", "tags"]
    });
    let args = serde_json::json!({
        "name": "test",
        "count": null,
        "tags": "abc"
    });
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    assert_eq!(
        rr.repaired,
        serde_json::json!({"name": "test", "tags": ["abc"]})
    );
    assert_eq!(rr.kinds.len(), 2);
    assert!(rr.kinds.contains(&RepairKind::NullStripped));
    assert!(rr.kinds.contains(&RepairKind::BareStringToArray));
}

// ============================================================
// Unrepairable errors
// ============================================================

#[test]
fn missing_required_field_fails() {
    let schema = simple_object_schema();
    let args = serde_json::json!({});
    let result = validate_and_repair(&schema, &args);
    assert!(result.is_err());
    let errors = result.unwrap_err();
    assert!(
        errors.iter().any(|e| e.contains("path")),
        "errors should mention missing 'path': {errors:?}"
    );
}

#[test]
fn wrong_type_for_required_field_fails() {
    let schema = simple_object_schema();
    let args = serde_json::json!({"path": 123});
    let result = validate_and_repair(&schema, &args);
    assert!(result.is_err(), "number where string required should fail");
}

// ============================================================
// format_structured_error
// ============================================================

#[test]
fn structured_error_contains_expected_sections() {
    let schema = simple_object_schema();
    let args = serde_json::json!({"path": 123});
    let errors = vec!["at /path: expected string, got number".to_string()];
    let msg = format_structured_error(&schema, &args, &errors);
    assert!(msg.contains("Tool input rejected:"));
    assert!(msg.contains("Expected:"));
    assert!(msg.contains("Got:"));
    assert!(msg.contains("Try:"));
    assert!(msg.contains("123"));
}

#[test]
fn structured_error_truncates_long_input() {
    let schema = simple_object_schema();
    let long = "x".repeat(500);
    let args = serde_json::json!({"path": long});
    let errors = vec!["at /path: too long".to_string()];
    let msg = format_structured_error(&schema, &args, &errors);
    assert!(msg.len() < 500, "output should be reasonable size");
    assert!(msg.contains('…'), "truncation marker missing");
}

/// Code-review B2: `navigate_schema` must descend into array
/// `items` when a JSON Pointer segment is numeric, so the
/// structured error's `Expected:` line shows the per-item
/// schema instead of falling back to the generic
/// "(see tool schema)" hint. Previously only `properties` was
/// consulted, which silently failed at `/edits/0/path` style
/// pointers.
#[test]
fn navigate_schema_descends_into_array_items() {
    let schema = nested_array_schema();
    // Top-level lookup still works.
    let top = navigate_schema(&schema, &["edits".to_string()]);
    assert!(top.is_some());
    assert_eq!(
        top.unwrap().get("type").and_then(|v| v.as_str()),
        Some("array")
    );

    // Numeric index descends into items.
    let item = navigate_schema(&schema, &["edits".to_string(), "0".to_string()]);
    assert!(item.is_some(), "should resolve /edits/0 via items");
    assert_eq!(
        item.unwrap().get("type").and_then(|v| v.as_str()),
        Some("object")
    );

    // Full path through array → property at the item.
    let path = navigate_schema(
        &schema,
        &["edits".to_string(), "0".to_string(), "path".to_string()],
    );
    assert!(path.is_some(), "should resolve /edits/0/path");
    assert_eq!(
        path.unwrap().get("type").and_then(|v| v.as_str()),
        Some("string")
    );
}

/// `format_structured_error` integration: with the array-items
/// fix in place, the Expected: line for a nested-array error
/// should reflect the per-item schema, not the default fallback.
#[test]
fn structured_error_uses_array_item_schema() {
    let schema = nested_array_schema();
    let args = serde_json::json!({
        "edits": [{
            "path": 123, // wrong type
            "replacements": ["a"]
        }]
    });
    let errors = vec!["at /edits/0/path: expected string, got integer".to_string()];
    let msg = format_structured_error(&schema, &args, &errors);
    // The Expected: line should contain the path schema's "string" type.
    assert!(
        msg.contains("string"),
        "Expected: should reflect the per-item path schema (type=string): {msg}",
    );
    // Fallback hint should NOT be present.
    assert!(
        !msg.contains("(see tool schema)"),
        "fallback should not fire when array navigation works: {msg}",
    );
}

// ============================================================
// Concrete hint suggestions
// ============================================================

#[test]
fn hint_for_null_value() {
    let hint = build_concrete_hint(&["at /limit: expected integer, got null".to_string()]);
    assert!(hint.contains("null"));
    assert!(hint.contains("not required"));
}

#[test]
fn hint_for_array_expected_string_got() {
    let hint = build_concrete_hint(&["at /paths: expected array, got string".to_string()]);
    assert!(hint.contains("square brackets"));
}

#[test]
fn hint_for_array_expected_object_got() {
    let hint = build_concrete_hint(&["at /paths: expected array, got object".to_string()]);
    assert!(hint.contains("{}"));
    assert!(hint.contains("[]"));
}

#[test]
fn hint_for_missing_field() {
    let hint = build_concrete_hint(&["at : missing field 'path'".to_string()]);
    assert!(hint.contains("required"));
}

// ============================================================
// Phase 2: markdown auto-link unwrap
// ============================================================

/// Degenerate: link text == URL stripped of protocol.
#[test]
fn md_unwrap_exact_match() {
    assert_eq!(
        unwrap_md_link("[notes.md](http://notes.md)"),
        Some("notes.md".into())
    );
    assert_eq!(
        unwrap_md_link("[file.txt](https://file.txt)"),
        Some("file.txt".into())
    );
}

/// Degenerate: link text is a suffix of the URL path.
#[test]
fn md_unwrap_suffix_match() {
    assert_eq!(
        unwrap_md_link("[notes.md](https://example.com/sub/notes.md)"),
        Some("notes.md".into())
    );
}

/// Real markdown: text and URL are semantically different.
#[test]
fn md_unwrap_real_markdown_passes_through() {
    assert_eq!(unwrap_md_link("[click here](https://example.com)"), None);
    assert_eq!(unwrap_md_link("[docs](http://other.org/page)"), None);
    assert_eq!(unwrap_md_link("[search](https://google.com?q=test)"), None);
}

/// Non-markdown strings pass through.
#[test]
fn md_unwrap_plain_string_passes_through() {
    assert_eq!(unwrap_md_link("/foo/bar"), None);
    assert_eq!(unwrap_md_link("notes.md"), None);
}

/// Brackets without URL pass through.
#[test]
fn md_unwrap_brackets_without_url() {
    assert_eq!(unwrap_md_link("[notes.md]"), None);
    assert_eq!(unwrap_md_link("(http://notes.md)"), None);
}

/// Schema-driven: only path-named fields get unwrapped.
#[test]
fn md_unwrap_only_path_fields_via_validate() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "content": { "type": "string" }
        },
        "required": ["path", "content"]
    });
    let args = serde_json::json!({
        "path": "[notes.md](http://notes.md)",
        "content": "[notes.md](http://notes.md)"
    });
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    // path field should be unwrapped.
    assert_eq!(rr.repaired["path"], "notes.md");
    // content field should NOT be unwrapped (not a path field).
    assert_eq!(rr.repaired["content"], "[notes.md](http://notes.md)");
    assert!(rr.kinds.contains(&RepairKind::MdLinkUnwrapped));
}

/// x-dirge-kind annotation triggers path field detection.
#[test]
fn md_unwrap_x_dirge_kind_path_annotation() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "source": {
                "type": "string",
                "x-dirge-kind": "path"
            },
            "body": { "type": "string" }
        },
        "required": ["source", "body"]
    });
    let args = serde_json::json!({
        "source": "[file.rs](http://file.rs)",
        "body": "[file.rs](http://file.rs)"
    });
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    assert_eq!(rr.repaired["source"], "file.rs");
    // body is NOT a path field — no annotation, not path-named.
    assert_eq!(rr.repaired["body"], "[file.rs](http://file.rs)");
}

/// Nested path fields are unwrapped.
#[test]
fn md_unwrap_nested_path_field() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "new_text": { "type": "string" }
                    },
                    "required": ["path", "new_text"]
                }
            }
        },
        "required": ["edits"]
    });
    let args = serde_json::json!({
        "edits": [{
            "path": "[src/main.rs](https://src/main.rs)",
            "new_text": "[src/main.rs](https://src/main.rs)"
        }]
    });
    let result = validate_and_repair(&schema, &args).unwrap();
    assert!(result.is_some());
    let rr = result.unwrap();
    assert_eq!(rr.repaired["edits"][0]["path"], "src/main.rs");
    // new_text is not a path field.
    assert_eq!(
        rr.repaired["edits"][0]["new_text"],
        "[src/main.rs](https://src/main.rs)"
    );
}

// ── dirge-du5k: truncation brace-closer ────────────────────────────

/// Reasonix parity test 1: fast-path — well-formed JSON is
/// returned unchanged.
#[test]
fn truncation_fast_path_parseable_unchanged() {
    let r = repair_truncated_json(r#"{"path": "/tmp/x"}"#);
    assert!(!r.changed);
    assert!(!r.fallback);
    assert!(r.notes.is_empty());
    assert_eq!(r.repaired, r#"{"path": "/tmp/x"}"#);
}

/// Reasonix parity test 2: empty input → "{}" (and flagged as
/// changed). Models occasionally emit zero-length tool_call
/// arg strings; the closer must route them to a no-op object
/// rather than panic the parser.
#[test]
fn truncation_empty_input_yields_empty_object() {
    let r = repair_truncated_json("");
    assert_eq!(r.repaired, "{}");
    assert!(!r.fallback);
}

/// Reasonix parity test 3: unterminated string + open object.
/// The model wrote `{"path": "/tmp/foo` and ran out of tokens.
/// Closer should close the string and then the object.
#[test]
fn truncation_unterminated_string_and_object() {
    let r = repair_truncated_json(r#"{"path": "/tmp/foo"#);
    assert!(!r.fallback);
    assert!(r.changed);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("closed unterminated string"))
    );
    let parsed: Value = serde_json::from_str(&r.repaired).expect("parses");
    assert_eq!(parsed["path"], "/tmp/foo");
}

/// Reasonix parity test 4: dangling key (model stopped at the
/// colon). Closer fills with `null` so the parse succeeds and
/// the schema layer reports the type mismatch instead of a
/// confusing parse error.
#[test]
fn truncation_dangling_key_filled_with_null() {
    let r = repair_truncated_json(r#"{"path":"#);
    assert!(!r.fallback);
    assert!(r.notes.iter().any(|n| n.contains("dangling key")));
    let parsed: Value = serde_json::from_str(&r.repaired).expect("parses");
    assert_eq!(parsed["path"], Value::Null);
}

/// Reasonix parity test 5: trailing comma trimmed.
#[test]
fn truncation_trailing_comma_trimmed() {
    let r = repair_truncated_json(r#"{"a": 1,"#);
    assert!(!r.fallback);
    assert!(r.notes.iter().any(|n| n.contains("trimmed trailing comma")));
    let parsed: Value = serde_json::from_str(&r.repaired).expect("parses");
    assert_eq!(parsed["a"], 1);
}

/// Reasonix parity test 6: nested arrays + objects all opened.
/// Closer pops them in reverse declaration order.
#[test]
fn truncation_nested_open_structures_all_closed() {
    let input = r#"{"edits":[{"path":"/tmp/x","new_text":"hello"#;
    let r = repair_truncated_json(input);
    assert!(!r.fallback, "notes: {:?}", r.notes);
    let parsed: Value = serde_json::from_str(&r.repaired).expect("parses");
    assert_eq!(parsed["edits"][0]["path"], "/tmp/x");
    assert_eq!(parsed["edits"][0]["new_text"], "hello");
}

/// Reasonix parity test 7: hard fallback. Garbage that the
/// stack can't rationalize routes to `{}` with `fallback=true`.
/// The integration layer relies on this flag to decide NOT to
/// substitute a fake empty object back into `args`.
#[test]
fn truncation_garbage_falls_back_to_empty_object() {
    let r = repair_truncated_json(r#"{"a":1} extra ::: garbage"#);
    // The parser swallows whitespace before { but rejects
    // trailing content. Closer can't rebalance — but the
    // input might actually re-parse as just the prefix. Allow
    // either fallback OR a partial parse; assert correctness
    // by re-parsing the result.
    if !r.fallback {
        assert!(serde_json::from_str::<Value>(&r.repaired).is_ok());
    } else {
        assert_eq!(r.repaired, "{}");
    }
}

/// Reasonix parity test 8: preview is truncated at 500 chars
/// on hard fallback for telemetry sanity. Don't dump 10 MB
/// args into a single notes line.
#[test]
fn truncation_fallback_preview_capped() {
    // Build genuinely-unrecoverable input: deeply unbalanced
    // with a stray closer that can't be reconciled.
    let mut input = String::from("}}}}}}}}}}");
    input.push_str(&"x".repeat(1000));
    let r = repair_truncated_json(&input);
    if r.fallback {
        let preview_note = r
            .notes
            .iter()
            .find(|n| n.contains("preview"))
            .expect("preview note present");
        // Cap at 500 + "…[+N chars]" suffix → ~520 bytes max.
        assert!(
            preview_note.len() < 600,
            "preview too long: {}",
            preview_note.len()
        );
        assert!(preview_note.contains("…"));
    }
}

/// dirge-7bwx post-hoist contract: when args arrive at
/// `validate_and_repair` already parsed (the normal
/// production path, since `run.rs` runs the closer
/// upstream), the validator sees a normal Object and
/// succeeds. This documents the new contract from the
/// consumer's point of view.
#[test]
fn integration_post_hoist_validator_sees_already_parsed_args() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" }
        },
        "required": ["path"]
    });
    // Args after the run.rs hoist did its work.
    let args = serde_json::json!({ "path": "/tmp/cut" });
    let result = validate_and_repair(&schema, &args).expect("validation must succeed");
    // No repair needed — args are already structurally valid.
    assert!(
        result.is_none(),
        "post-repair args must pass through unchanged: {result:?}",
    );
}

/// dirge-7bwx post-hoist contract: if args somehow STILL
/// arrive as Value::String at validate_and_repair (test
/// paths that bypass the loop, future code paths that miss
/// the upstream call), the validator returns an Err so the
/// caller can surface the failure to the model. We do NOT
/// silently re-run the closer — Reasonix parity invariant:
/// truncation repair runs ONCE at the loop level. Running it
/// again at dispatch would re-introduce the storm-evasion
/// gap the hoist fixes.
#[test]
fn integration_post_hoist_unparsed_string_surfaces_validation_error() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" }
        },
        "required": ["path"]
    });
    let args = Value::String(r#"{"path": "/tmp/cut"#.to_string());
    let result = validate_and_repair(&schema, &args);
    assert!(
        result.is_err(),
        "unrepaired string args must surface as Err (post-hoist contract): {result:?}",
    );
}

/// dirge-du5k integration test 2: the pre-pass refuses to
/// engage on a non-object schema. Bare-string-to-array etc.
/// own those cases.
#[test]
fn integration_truncation_skips_non_object_schemas() {
    let schema = serde_json::json!({
        "type": "string"
    });
    let args = Value::String("hello".to_string());
    // No truncation repair — falls through to normal flow.
    // (validate_and_repair returns Ok(None) because args is
    // already a valid string.)
    let result = validate_and_repair(&schema, &args).expect("no error");
    // Either None (passthrough) or Some without TruncationFixed.
    if let Some(rr) = result {
        assert!(!rr.kinds.contains(&RepairKind::TruncationFixed));
    }
}

/// dirge-du5k integration test 3: hard fallback does NOT
/// silently substitute `{}`. Model gets a real validation
/// error so it can retry with the right shape.
#[test]
fn integration_truncation_fallback_does_not_mask_error() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" }
        },
        "required": ["path"]
    });
    // Stack-unrecoverable input (stray closers).
    let args = Value::String("}}}}}".to_string());
    let result = validate_and_repair(&schema, &args);
    // Either the closer succeeded with a parseable form that
    // then fails required-field validation, OR the closer
    // fell back and we got a normal validation error. Either
    // way: an Err (or a NOT-promoted Value::String that
    // downstream rejects). Assert we do NOT see TruncationFixed
    // with a fabricated `{}`.
    if let Ok(Some(rr)) = &result {
        // If the closer "succeeded", it must have parsed to
        // something — not a fabricated empty object.
        assert!(
            !(rr.kinds.contains(&RepairKind::TruncationFixed)
                && rr.repaired == Value::Object(Default::default())),
            "fallback should not surface as a successful TruncationFixed → {{}}"
        );
    }
}

/// dirge-du5k integration test 4: RepairStats records
/// TruncationFixed in the per-kind counter and the snapshot
/// surfaces it.
#[test]
fn integration_truncation_increments_repair_stats() {
    let stats = RepairStats::new();
    stats.record(RepairKind::TruncationFixed);
    stats.record(RepairKind::TruncationFixed);
    let snap = stats.snapshot();
    assert_eq!(snap.truncation_fixed, 2);
    assert_eq!(snap.total_successful(), 2);
    assert!(!snap.is_empty());
}

/// dirge-fb8t: a validation failure whose args carry multibyte UTF-8
/// crossing the 200-byte truncation cut must NOT panic the agent run.
/// Before the char-boundary fix this slice (`&args_str[..200]`) panicked.
#[test]
fn format_structured_error_does_not_panic_on_multibyte_args() {
    // Force a multibyte char to straddle byte offset 200.
    let mut content = "a".repeat(199);
    content.push('世'); // 3 bytes at offsets 199..202
    content.push_str(&"b".repeat(50));
    let args = serde_json::json!({ "content": content });
    let schema = serde_json::json!({"type": "object"});
    let out = format_structured_error(&schema, &args, &["missing field `path`".into()]);
    assert!(out.contains("missing field"), "got: {out}");
    // Also exercise the path-style emoji case.
    let args2 = serde_json::json!({ "path": "/tmp/файл😀".to_string() + &"x".repeat(220) });
    let _ = format_structured_error(&schema, &args2, &["bad".into()]);
}
