use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::semantic::SymbolIndex;

pub struct FindCallersTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    index: Arc<RwLock<SymbolIndex>>,
}

impl FindCallersTool {
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
    name: String,
    path: Option<String>,
}

impl Tool for FindCallersTool {
    const NAME: &'static str = "find_callers";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "find_callers".to_string(),
            description: "Find all call sites of a function or method across the project. Searches source files for references, excluding the definition site. Supports all tree-sitter supported languages.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the function/method to find callers of"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in (defaults to current working directory)"
                    }
                },
                "required": ["name"]
            }),
        }
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "find_callers", &args.name).await?;

        let search_path = args
            .path
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let results = {
            let mut idx = self
                .index
                .write()
                .map_err(|e| ToolError::Msg(format!("Index lock error: {e}")))?;
            idx.ensure_all(&search_path, None)
                .map_err(ToolError::Msg)?;
            idx.find_callers(&args.name, &search_path)
                .map_err(ToolError::Msg)?
        };

        if results.is_empty() {
            return Ok(format!("No callers found for '{}'", args.name));
        }

        let total = results.len();
        Ok(format!(
            "{} caller(s) for '{}':\n{}",
            total,
            args.name,
            results.join("\n")
        ))
    }
}
