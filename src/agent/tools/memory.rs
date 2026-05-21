use std::path::PathBuf;

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::extras::memory;

pub struct MemoryTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

impl MemoryTool {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self { permission, ask_tx }
    }

    fn mem_dir() -> PathBuf {
        memory::memory_dir(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

#[derive(Deserialize)]
pub struct Args {
    action: String,
    path: Option<String>,
    content: Option<String>,
}

impl Tool for MemoryTool {
    const NAME: &'static str = "memory";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "memory".to_string(),
            description: "Persistent long-term memory. Actions: view [path] (list all or read one), write \"path\" \"content\" (create/update), delete \"path\" (remove). Memories are scoped to the current project.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["view", "write", "delete"],
                        "description": "view: read one or list all; write: create/overwrite; delete: remove a memory file"
                    },
                    "path": {
                        "type": "string",
                        "description": "Memory file path (relative). For view without path, lists all."
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write (required for write action)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "memory", &args.action).await?;

        let dir = Self::mem_dir();

        match args.action.as_str() {
            "view" => {
                if let Some(path) = args.path.as_deref().filter(|p| !p.is_empty()) {
                    let content = memory::read_file(&dir, path).map_err(|e| ToolError::Msg(e))?;
                    Ok(format!("Memory: {}\n\n{}", path, content))
                } else {
                    let files = memory::list_files(&dir).map_err(|e| ToolError::Msg(e))?;
                    if files.is_empty() {
                        Ok("No memories stored.".to_string())
                    } else {
                        Ok(format!(
                            "Memories ({}):\n{}",
                            files.len(),
                            files
                                .iter()
                                .map(|f| format!("  {}", f))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ))
                    }
                }
            }
            "write" => {
                let path = args
                    .path
                    .as_deref()
                    .ok_or_else(|| ToolError::Msg("path required for write".to_string()))?;
                let content = args
                    .content
                    .as_deref()
                    .ok_or_else(|| ToolError::Msg("content required for write".to_string()))?;
                memory::write_file(&dir, path, content).map_err(|e| ToolError::Msg(e))?;
                Ok(format!("Written memory: {}", path))
            }
            "delete" => {
                let path = args
                    .path
                    .as_deref()
                    .ok_or_else(|| ToolError::Msg("path required for delete".to_string()))?;
                memory::delete_file(&dir, path).map_err(|e| ToolError::Msg(e))?;
                Ok(format!("Deleted memory: {}", path))
            }
            _ => Err(ToolError::Msg(format!(
                "Unknown action '{}'. Use view, write, or delete.",
                args.action
            ))),
        }
    }
}
