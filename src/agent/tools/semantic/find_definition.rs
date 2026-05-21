use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::semantic::SymbolIndex;

pub struct FindDefinitionTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    index: Arc<RwLock<SymbolIndex>>,
}

impl FindDefinitionTool {
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
}

impl Tool for FindDefinitionTool {
    const NAME: &'static str = "find_definition";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "find_definition".to_string(),
            description: "Find where a symbol (function, class, type, etc.) is defined across the project. Uses tree-sitter to locate definitions precisely.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the symbol to find"
                    }
                },
                "required": ["name"]
            }),
        }
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        check_perm(
            &self.permission,
            &self.ask_tx,
            "find_definition",
            &args.name,
        )
        .await?;

        let results = {
            let mut idx = self
                .index
                .write()
                .map_err(|e| ToolError::Msg(format!("Index lock error: {e}")))?;
            idx.ensure_all(
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                None,
            )
            .map_err(ToolError::Msg)?;
            idx.find_definition(&args.name)
                .map_err(ToolError::Msg)?
        };

        if results.is_empty() {
            return Ok(format!("No definition found for '{}'", args.name));
        }

        let mut output = format!(
            "Found {} definition(s) for '{}':\n",
            results.len(),
            args.name
        );
        for (path, sym) in &results {
            output.push_str(&format!(
                "  {}:{} [{}] {}\n",
                path.display(),
                sym.range.start_line,
                sym.kind,
                sym.signature
            ));
        }

        Ok(output)
    }
}
