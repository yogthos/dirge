use std::path::{Path, PathBuf};

use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, PermCheck, ToolError, WriteArgs, check_perm_path};

pub struct WriteTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    plan_file: Option<PathBuf>,
    cache: Option<ToolCache>,
}

impl WriteTool {
    #[allow(dead_code)]
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        plan_file: Option<PathBuf>,
    ) -> Self {
        WriteTool {
            permission,
            ask_tx,
            plan_file,
            cache: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        plan_file: Option<PathBuf>,
        cache: ToolCache,
    ) -> Self {
        WriteTool {
            permission,
            ask_tx,
            plan_file,
            cache: Some(cache),
        }
    }
}

impl Tool for WriteTool {
    const NAME: &'static str = "write";

    type Error = ToolError;
    type Args = WriteArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "write".to_string(),
            description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                    "content": { "type": "string", "description": "Content to write to the file" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn call(&self, args: WriteArgs) -> Result<String, ToolError> {
        check_perm_path(&self.permission, &self.ask_tx, "write", &args.path).await?;

        if let Some(plan) = &self.plan_file {
            let allowed = {
                let path = Path::new(&args.path);
                path == Path::new("PLAN.md") || {
                    let pc = std::fs::canonicalize(path).ok();
                    let pp = std::fs::canonicalize(plan).ok();
                    pc.is_some() && pp.is_some() && pc.as_ref() == pp.as_ref()
                }
            };
            if !allowed {
                return Err(ToolError::Msg(
                    "Plan mode: writes restricted to PLAN.md only. Use /prompt default to exit plan mode."
                        .to_string(),
                ));
            }
        }

        let path = Path::new(&args.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = args.content.len();
        tokio::fs::write(path, &args.content).await?;
        // File mutated → invalidate cached reads/greps/listings for this turn.
        if let Some(ref cache) = self.cache {
            cache.clear();
        }
        Ok(format!("Written {} bytes to {}", bytes, args.path))
    }
}
