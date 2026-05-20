use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use std::path::Path;

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm_path};

/// Max content size for a single create operation (1MB).
const MAX_CREATE_SIZE: usize = 1_048_576;

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "action")]
pub enum PatchOp {
    #[serde(rename = "create")]
    Create { path: String, content: String },
    #[serde(rename = "update")]
    Update {
        path: String,
        old_text: String,
        new_text: String,
    },
    #[serde(rename = "delete")]
    Delete { path: String },
    #[serde(rename = "rename")]
    Rename { path: String, new_path: String },
}

pub struct ApplyPatchTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    cache: Option<ToolCache>,
}

impl ApplyPatchTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self {
            permission,
            ask_tx,
            cache: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        cache: ToolCache,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            cache: Some(cache),
        }
    }
}

#[derive(Deserialize)]
pub struct ApplyPatchArgs {
    pub operations: Vec<PatchOp>,
}

fn apply_create(path: &str, content: &str) -> Result<String, String> {
    let p = Path::new(path);
    if p.exists() {
        return Err(format!("file already exists: {}", path));
    }
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create parent dir: {}", e))?;
    }
    std::fs::write(p, content).map_err(|e| format!("write failed: {}", e))?;
    Ok(format!("created {}", path))
}

fn apply_update(path: &str, old_text: &str, new_text: &str) -> Result<String, String> {
    let original = std::fs::read_to_string(path).map_err(|e| format!("read failed: {}", e))?;

    // CRLF normalization to match `edit.rs`. The LLM almost always
    // generates `\n` in `old_text` even when the file is CRLF on
    // disk; without normalization the literal substring match fails.
    // We normalize a working copy for matching but preserve the
    // original's line endings on the write-back.
    let crlf = original.contains("\r\n");
    let normalized = if crlf {
        original.replace("\r\n", "\n")
    } else {
        original.clone()
    };
    let needle = old_text.replace("\r\n", "\n");

    if !normalized.contains(&needle) {
        return Err(format!("text not found in {}", path));
    }

    let matches: Vec<_> = normalized.match_indices(&needle).collect();
    if matches.len() > 1 {
        return Err(format!(
            "text matches {} locations in {} — provide more context to make unique",
            matches.len(),
            path
        ));
    }

    let replacement = if crlf {
        new_text.replace("\r\n", "\n")
    } else {
        new_text.to_string()
    };
    let updated_normalized = normalized.replacen(&needle, &replacement, 1);
    // Restore CRLF line endings on write-back so we don't silently
    // re-format the user's file.
    let to_write = if crlf {
        updated_normalized.replace('\n', "\r\n")
    } else {
        updated_normalized
    };
    std::fs::write(path, &to_write).map_err(|e| format!("write failed: {}", e))?;
    Ok(format!("updated {}", path))
}

fn apply_delete(path: &str) -> Result<String, String> {
    std::fs::remove_file(path).map_err(|e| format!("delete failed: {}", e))?;
    Ok(format!("deleted {}", path))
}

fn apply_rename(path: &str, new_path: &str) -> Result<String, String> {
    std::fs::rename(path, new_path).map_err(|e| format!("rename failed: {}", e))?;
    Ok(format!("renamed {} -> {}", path, new_path))
}

impl Tool for ApplyPatchTool {
    const NAME: &'static str = "apply_patch";

    type Error = ToolError;
    type Args = ApplyPatchArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".to_string(),
            description: "Apply multiple file operations in a single call. Supports create, update (by exact text match), delete, and rename. Operations execute in order and stop on first failure — prior operations that succeeded remain applied."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "operations": {
                        "type": "array",
                        "description": "Ordered list of file operations to execute",
                        "items": {
                            "type": "object",
                            "properties": {
                                "action": {
                                    "type": "string",
                                    "enum": ["create", "update", "delete", "rename"],
                                    "description": "The type of operation"
                                },
                                "path": {
                                    "type": "string",
                                    "description": "Target file path"
                                },
                                "content": {
                                    "type": "string",
                                    "description": "File content (required for create)"
                                },
                                "old_text": {
                                    "type": "string",
                                    "description": "Exact text to find and replace (required for update)"
                                },
                                "new_text": {
                                    "type": "string",
                                    "description": "Replacement text (required for update)"
                                },
                                "new_path": {
                                    "type": "string",
                                    "description": "New file path (required for rename)"
                                }
                            },
                            "required": ["action", "path"]
                        }
                    }
                },
                "required": ["operations"]
            }),
        }
    }

    async fn call(&self, args: ApplyPatchArgs) -> Result<String, ToolError> {
        if args.operations.is_empty() {
            return Err(ToolError::Msg("no operations provided".to_string()));
        }

        let mut results = Vec::new();

        for op in &args.operations {
            // Permission check for the target path
            match op {
                PatchOp::Create { path, .. }
                | PatchOp::Update { path, .. }
                | PatchOp::Delete { path }
                | PatchOp::Rename { path, .. } => {
                    check_perm_path(&self.permission, &self.ask_tx, "apply_patch", path).await?;
                }
            }
            // Rename also requires permission on the new path
            if let PatchOp::Rename { new_path, .. } = op {
                check_perm_path(&self.permission, &self.ask_tx, "apply_patch", new_path).await?;
            }
            // Validate create content size
            if let PatchOp::Create { content, .. } = op {
                if content.len() > MAX_CREATE_SIZE {
                    results.push(format!(
                        "FAILED: create content exceeds {} bytes ({} bytes provided)",
                        MAX_CREATE_SIZE,
                        content.len()
                    ));
                    break;
                }
            }

            let result = match op {
                PatchOp::Create { path, content } => apply_create(path, content),
                PatchOp::Update {
                    path,
                    old_text,
                    new_text,
                } => apply_update(path, old_text, new_text),
                PatchOp::Delete { path } => apply_delete(path),
                PatchOp::Rename { path, new_path } => apply_rename(path, new_path),
            };

            match result {
                Ok(msg) => {
                    // Record the touched path(s) for the info panel. Rename
                    // adds the *new* path; delete still records the path the
                    // user/agent operated on so the panel reflects the action.
                    match op {
                        PatchOp::Create { path, .. }
                        | PatchOp::Update { path, .. }
                        | PatchOp::Delete { path } => {
                            crate::agent::tools::modified::mark_modified(std::path::Path::new(
                                path,
                            ));
                        }
                        PatchOp::Rename { new_path, .. } => {
                            crate::agent::tools::modified::mark_modified(std::path::Path::new(
                                new_path,
                            ));
                        }
                    }
                    results.push(msg);
                }
                Err(e) => {
                    results.push(format!("FAILED: {}", e));
                    break;
                }
            }
        }

        // Clear the cache once after the batch instead of once per op.
        // Per-op clearing was correct but wasteful — a 5-op batch
        // would clear 5 times. Subsequent tool calls within the same
        // turn now see a single clean cache.
        if let Some(ref cache) = self.cache {
            cache.clear();
        }

        Ok(results.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFile {
        path: String,
    }

    impl TestFile {
        fn new(name: &str) -> Self {
            let path = format!("/tmp/dirge-test-{}", name);
            // Clean up any leftover
            let _ = std::fs::remove_file(&path);
            Self { path }
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn test_create_and_read() {
        let tf = TestFile::new("create-test.txt");
        let result = apply_create(&tf.path, "hello world");
        assert!(result.is_ok());
        let content = std::fs::read_to_string(&tf.path).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_create_existing_file_fails() {
        let tf = TestFile::new("create-exists.txt");
        std::fs::write(&tf.path, "existing").unwrap();
        let result = apply_create(&tf.path, "new");
        assert!(result.is_err());
    }

    #[test]
    fn test_update_text() {
        let tf = TestFile::new("update-test.txt");
        std::fs::write(&tf.path, "before after").unwrap();
        let result = apply_update(&tf.path, "before", "replaced");
        assert!(result.is_ok());
        let content = std::fs::read_to_string(&tf.path).unwrap();
        assert_eq!(content, "replaced after");
    }

    #[test]
    fn test_update_text_not_found() {
        let tf = TestFile::new("update-notfound.txt");
        std::fs::write(&tf.path, "some content").unwrap();
        let result = apply_update(&tf.path, "nonexistent", "replacement");
        assert!(result.is_err());
    }

    #[test]
    fn test_delete_file() {
        let tf = TestFile::new("delete-test.txt");
        std::fs::write(&tf.path, "to delete").unwrap();
        assert!(Path::new(&tf.path).exists());
        let result = apply_delete(&tf.path);
        assert!(result.is_ok());
        assert!(!Path::new(&tf.path).exists());
    }

    #[test]
    fn test_rename_file() {
        let src = TestFile::new("rename-src.txt");
        let dst = "/tmp/dirge-test-rename-dst.txt";
        let _ = std::fs::remove_file(dst);
        std::fs::write(&src.path, "rename me").unwrap();

        let result = apply_rename(&src.path, dst);
        assert!(result.is_ok());
        assert!(!Path::new(&src.path).exists());
        assert!(Path::new(dst).exists());
        let _ = std::fs::remove_file(dst);
    }

    #[tokio::test]
    async fn test_rejects_empty_operations() {
        let tool = ApplyPatchTool::new(None, None);
        let result = tool.call(ApplyPatchArgs { operations: vec![] }).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no operations"));
    }

    #[tokio::test]
    async fn test_definition_has_correct_name() {
        let tool = ApplyPatchTool::new(None, None);
        let def = tool.definition(String::new()).await;
        assert_eq!(def.name, "apply_patch");
    }

    // Regression: update is documented as text-find-and-replace and must reject
    // ambiguous matches rather than silently replacing the first one. Without
    // this guard the agent could clobber wrong code in a file with repeated
    // boilerplate (use statements, similar function bodies, etc.).
    #[test]
    fn regression_update_rejects_multiple_matches() {
        let tf = TestFile::new("update-ambiguous.txt");
        std::fs::write(&tf.path, "foo bar foo baz foo").unwrap();
        let result = apply_update(&tf.path, "foo", "qux");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("3 locations"), "got: {msg}");
        // File should be untouched.
        assert_eq!(
            std::fs::read_to_string(&tf.path).unwrap(),
            "foo bar foo baz foo"
        );
    }

    // Regression: prior to the fix, multi-op patches were documented as
    // "atomic" but in fact left earlier successful ops applied when a later op
    // failed. We now stop on first failure AND the prior ops MUST stay applied
    // (no rollback). The error report must explicitly call out which op failed
    // and ops after the failure must NOT execute.
    #[tokio::test]
    async fn regression_multi_op_stops_on_failure_prior_ops_remain() {
        let a = TestFile::new("multi-op-a.txt");
        let b_existing = TestFile::new("multi-op-b.txt");
        let c_should_not_exist = TestFile::new("multi-op-c.txt");

        // Pre-create B so the second op (create B) fails.
        std::fs::write(&b_existing.path, "already here").unwrap();

        let tool = ApplyPatchTool::new(None, None);
        let result = tool
            .call(ApplyPatchArgs {
                operations: vec![
                    PatchOp::Create {
                        path: a.path.clone(),
                        content: "A content".into(),
                    },
                    PatchOp::Create {
                        path: b_existing.path.clone(),
                        content: "B content".into(),
                    },
                    PatchOp::Create {
                        path: c_should_not_exist.path.clone(),
                        content: "C content".into(),
                    },
                ],
            })
            .await
            .unwrap();

        // A was created.
        assert!(Path::new(&a.path).exists(), "A must remain applied");
        assert_eq!(std::fs::read_to_string(&a.path).unwrap(), "A content");
        // B was not overwritten.
        assert_eq!(
            std::fs::read_to_string(&b_existing.path).unwrap(),
            "already here"
        );
        // C was never attempted.
        assert!(
            !Path::new(&c_should_not_exist.path).exists(),
            "C must not run after failure"
        );
        // Report names both the success and the failure.
        assert!(result.contains("created"), "got: {result}");
        assert!(result.contains("FAILED"), "got: {result}");
    }

    // Regression: create previously had no size cap; the agent could write
    // multi-GB files by accident. 1MB limit must be enforced before touching
    // the filesystem, and the operation must not produce a partial write.
    #[tokio::test]
    async fn regression_create_rejects_oversized_content() {
        let tf = TestFile::new("oversize.txt");
        let too_big = "x".repeat(1_048_577); // 1MB + 1 byte

        let tool = ApplyPatchTool::new(None, None);
        let result = tool
            .call(ApplyPatchArgs {
                operations: vec![PatchOp::Create {
                    path: tf.path.clone(),
                    content: too_big,
                }],
            })
            .await
            .unwrap();

        assert!(result.contains("FAILED"), "got: {result}");
        assert!(result.contains("exceeds"), "got: {result}");
        assert!(
            !Path::new(&tf.path).exists(),
            "no file should exist after size-limit rejection"
        );
    }

    // Right at the limit must succeed; off-by-one boundary check.
    #[tokio::test]
    async fn create_accepts_content_at_size_limit() {
        let tf = TestFile::new("at-limit.txt");
        let at_limit = "x".repeat(1_048_576); // exactly 1MB

        let tool = ApplyPatchTool::new(None, None);
        let result = tool
            .call(ApplyPatchArgs {
                operations: vec![PatchOp::Create {
                    path: tf.path.clone(),
                    content: at_limit,
                }],
            })
            .await
            .unwrap();

        assert!(!result.contains("FAILED"), "got: {result}");
        assert!(Path::new(&tf.path).exists());
        assert_eq!(std::fs::metadata(&tf.path).unwrap().len(), 1_048_576);
    }

    // create_dir_all is called on the parent — confirms nested-path creates work.
    #[test]
    fn create_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("dirge-test-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let nested = dir.join("a/b/c/file.txt");
        let path_str = nested.to_str().unwrap();

        let result = apply_create(path_str, "deep content");
        assert!(result.is_ok());
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "deep content");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_missing_file_returns_err() {
        let path = format!("/tmp/dirge-test-delete-ghost-{}.txt", std::process::id());
        let _ = std::fs::remove_file(&path);
        let result = apply_delete(&path);
        assert!(result.is_err());
    }

    // Multi-op happy path: create + update + rename + delete in sequence,
    // touching different files. Regression-tests that the loop applies each op
    // in declaration order and the report lists each.
    #[tokio::test]
    async fn multi_op_happy_path_executes_in_order() {
        let a = TestFile::new("multi-happy-a.txt");
        let b = TestFile::new("multi-happy-b.txt");
        let renamed = format!(
            "/tmp/dirge-test-multi-happy-renamed-{}.txt",
            std::process::id()
        );
        let _ = std::fs::remove_file(&renamed);

        let tool = ApplyPatchTool::new(None, None);
        let result = tool
            .call(ApplyPatchArgs {
                operations: vec![
                    PatchOp::Create {
                        path: a.path.clone(),
                        content: "hello".into(),
                    },
                    PatchOp::Update {
                        path: a.path.clone(),
                        old_text: "hello".into(),
                        new_text: "HELLO".into(),
                    },
                    PatchOp::Create {
                        path: b.path.clone(),
                        content: "scratch".into(),
                    },
                    PatchOp::Rename {
                        path: a.path.clone(),
                        new_path: renamed.clone(),
                    },
                    PatchOp::Delete {
                        path: b.path.clone(),
                    },
                ],
            })
            .await
            .unwrap();

        assert!(!result.contains("FAILED"), "got: {result}");
        assert!(!Path::new(&a.path).exists()); // renamed away
        assert!(!Path::new(&b.path).exists()); // deleted
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "HELLO");
        let _ = std::fs::remove_file(&renamed);

        // Each successful op contributes a line to the report.
        assert_eq!(
            result.lines().filter(|l| !l.is_empty()).count(),
            5,
            "report: {result}"
        );
    }

    // Regression: PatchOp deserializes via internally-tagged `action` enum.
    // Schema mismatch (e.g. missing `content` for create) must fail at deserialize.
    #[test]
    fn patch_op_deserializes_each_variant() {
        let json = serde_json::json!([
            {"action": "create", "path": "/tmp/x", "content": "hi"},
            {"action": "update", "path": "/tmp/x", "old_text": "a", "new_text": "b"},
            {"action": "delete", "path": "/tmp/x"},
            {"action": "rename", "path": "/tmp/x", "new_path": "/tmp/y"},
        ]);
        let ops: Vec<PatchOp> = serde_json::from_value(json).unwrap();
        assert!(matches!(ops[0], PatchOp::Create { .. }));
        assert!(matches!(ops[1], PatchOp::Update { .. }));
        assert!(matches!(ops[2], PatchOp::Delete { .. }));
        assert!(matches!(ops[3], PatchOp::Rename { .. }));
    }
}
