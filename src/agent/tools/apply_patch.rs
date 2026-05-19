use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use std::path::Path;

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
}

impl ApplyPatchTool {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self { permission, ask_tx }
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

    if !original.contains(old_text) {
        return Err(format!("text not found in {}", path));
    }

    let matches: Vec<_> = original.match_indices(old_text).collect();
    if matches.len() > 1 {
        return Err(format!(
            "text matches {} locations in {} — provide more context to make unique",
            matches.len(),
            path
        ));
    }

    let updated = original.replacen(old_text, new_text, 1);
    std::fs::write(path, &updated).map_err(|e| format!("write failed: {}", e))?;
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
                Ok(msg) => results.push(msg),
                Err(e) => {
                    results.push(format!("FAILED: {}", e));
                    break;
                }
            }
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
}
