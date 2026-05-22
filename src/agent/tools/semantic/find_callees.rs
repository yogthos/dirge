use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm_path};
use crate::semantic::SymbolIndex;

pub struct FindCalleesTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    index: Arc<RwLock<SymbolIndex>>,
}

impl FindCalleesTool {
    pub fn new(
        index: Arc<RwLock<SymbolIndex>>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            index,
        }
    }
}

#[derive(Deserialize)]
pub struct Args {
    path: String,
    name: String,
}

impl Tool for FindCalleesTool {
    const NAME: &'static str = "find_callees";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "find_callees".to_string(),
            description: "Find all functions/methods called by a given symbol (its callees). Uses tree-sitter to extract call expressions from the symbol body. Supports TypeScript, Python, and more.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file containing the symbol"
                    },
                    "name": {
                        "type": "string",
                        "description": "Name of the function/method to analyze"
                    }
                },
                "required": ["path", "name"]
            }),
        }
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        // `args.path` is a real file path; use the path-aware
        // permission check so external_directory rules apply.
        check_perm_path(&self.permission, &self.ask_tx, "find_callees", &args.path).await?;

        let file_path = PathBuf::from(&args.path);

        let callees = {
            let mut idx = self
                .index
                .write()
                .map_err(|e| ToolError::Msg(format!("Index lock error: {e}")))?;
            idx.find_callees(&file_path, &args.name)
                .map_err(ToolError::Msg)?
        };

        if callees.is_empty() {
            return Ok(format!(
                "No callees found for '{}' in {}",
                args.name, args.path
            ));
        }

        Ok(format!(
            "Callees of '{}' ({} calls):\n{}",
            args.name,
            callees.len(),
            callees
                .iter()
                .map(|c| format!("  {}", c))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}
