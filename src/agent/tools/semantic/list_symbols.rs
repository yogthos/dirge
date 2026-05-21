use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm_path};
use crate::semantic::SymbolIndex;
use crate::semantic::types::SymbolKind;

pub struct ListSymbolsTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    index: Arc<RwLock<SymbolIndex>>,
}

impl ListSymbolsTool {
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
    path: Option<String>,
    kind: Option<String>,
}

impl Tool for ListSymbolsTool {
    const NAME: &'static str = "list_symbols";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "list_symbols".to_string(),
            description: "List symbols (functions, classes, methods, etc.) in a file or across the project. Parses code with tree-sitter for accurate results. Use this instead of grep when looking for code structure.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path to list symbols from. Omit to list across all indexed files."
                    },
                    "kind": {
                        "type": "string",
                        "description": "Filter by symbol kind: function, class, method, interface, type, or variable"
                    }
                }
            }),
        }
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        // Path-aware check so external_directory rules apply.
        // `args.path` is None when scanning the whole project — pass
        // "." which check_perm_path resolves against the working dir.
        check_perm_path(
            &self.permission,
            &self.ask_tx,
            "list_symbols",
            args.path.as_deref().unwrap_or("."),
        )
        .await?;

        let kind_filter = args.kind.as_deref().and_then(|k| match k {
            "function" => Some(SymbolKind::Function),
            "class" => Some(SymbolKind::Class),
            "method" => Some(SymbolKind::Method),
            "interface" => Some(SymbolKind::Interface),
            "type" => Some(SymbolKind::TypeAlias),
            "variable" => Some(SymbolKind::Variable),
            _ => None,
        });

        let file_path = args.path.as_deref().map(PathBuf::from);

        let results = {
            let mut idx = self
                .index
                .write()
                .map_err(|e| ToolError::Msg(format!("Index lock error: {e}")))?;

            if let Some(ref fp) = file_path {
                idx.ensure_file(fp).map_err(|e| ToolError::Msg(e))?;
            } else {
                idx.ensure_all(
                    &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                    None,
                )
                .map_err(|e| ToolError::Msg(e))?;
            }

            idx.list_symbols(file_path.as_deref(), kind_filter)
                .map_err(|e| ToolError::Msg(e))?
        };

        if results.is_empty() {
            return Ok("No symbols found.".to_string());
        }

        let mut output = String::new();
        for (path, symbols) in &results {
            output.push_str(&format!("## {}\n", path.display()));
            for sym in symbols {
                let class_hint = match &sym.parent_class {
                    Some(c) => format!(" [class: {}]", c),
                    None => String::new(),
                };
                let export_mark = if sym.is_exported { " (exported)" } else { "" };
                output.push_str(&format!(
                    "  {}-{} [{}] {} {} {}{}\n",
                    sym.range.start_line,
                    sym.range.end_line,
                    sym.kind,
                    sym.name,
                    sym.signature,
                    class_hint,
                    export_mark
                ));
            }
        }

        let total_symbols: usize = results.iter().map(|(_, s)| s.len()).sum();
        let total_files = results.len();
        output.push_str(&format!(
            "\n{} symbols across {} files",
            total_symbols, total_files
        ));

        Ok(output)
    }
}
