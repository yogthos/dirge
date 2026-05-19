use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, PermCheck, ReadArgs, ToolError, check_perm_path};
use crate::lsp::manager::{LspManager, TouchMode};

pub struct ReadTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub cache: Option<ToolCache>,
    /// When set, the tool fires off a `touch_file` to warm the LSP server
    /// so subsequent edits surface diagnostics quickly. Fire-and-forget:
    /// the read tool does not wait or surface diagnostics in its output.
    pub lsp_manager: Option<Arc<LspManager>>,
}

impl ReadTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        ReadTool {
            permission,
            ask_tx,
            cache: None,
            lsp_manager: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        cache: ToolCache,
        lsp_manager: Option<Arc<LspManager>>,
    ) -> Self {
        ReadTool {
            permission,
            ask_tx,
            cache: Some(cache),
            lsp_manager,
        }
    }
}

impl Tool for ReadTool {
    const NAME: &'static str = "read";

    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "read".to_string(),
            description: "Read the contents of a file. Supports text files. Defaults to first 2000 lines. Use offset/limit for large files.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                    "offset": { "type": "integer", "description": "Line number to start from (1-indexed)" },
                    "limit": { "type": "integer", "description": "Maximum number of lines to read" }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        check_perm_path(&self.permission, &self.ask_tx, "read", &args.path).await?;

        let cache_key = format!(
            "read:{}:{}:{}",
            args.path,
            args.offset.unwrap_or(1),
            args.limit.unwrap_or(2000),
        );

        if let Some(ref cache) = self.cache {
            if let Some(cached) = cache.get(&cache_key) {
                return Ok(cached);
            }
        }

        let metadata = tokio::fs::metadata(&args.path).await?;
        let file_size = metadata.len();
        if file_size > 10 * 1024 * 1024 {
            return Err(ToolError::Msg(format!(
                "File too large ({} bytes). Max 10MB.",
                file_size
            )));
        }
        let content = tokio::fs::read_to_string(&args.path).await?;
        let total_lines = content.lines().count();

        let offset = args.offset.unwrap_or(1).max(1) - 1;
        let limit = args.limit.unwrap_or(2000);
        let end = (offset + limit).min(total_lines);

        let width = (total_lines.to_string().len()).max(1);
        let excerpt: String = content
            .lines()
            .skip(offset)
            .take(end - offset)
            .enumerate()
            .map(|(i, line)| format!("{:>width$}: {}", offset + i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        let info = format!(
            "File: {} ({} lines total, showing lines {}-{})\n\n{}",
            args.path,
            total_lines,
            offset + 1,
            end,
            excerpt
        );

        if let Some(ref cache) = self.cache {
            cache.set(&cache_key, info.clone());
        }

        // Fire-and-forget LSP warmup so the server already has the file
        // open by the time the agent edits it (and we can wait_for_push
        // quickly). No diagnostic surfacing on read.
        if let Some(manager) = self.lsp_manager.clone() {
            let path = std::path::PathBuf::from(&args.path);
            tokio::spawn(async move {
                manager.touch_file(&path, TouchMode::Notify).await;
            });
        }

        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    /// Verifies the line-numbering format used in read output.
    /// The model sees this format and must strip "NNN: " prefixes when passing text to edit.
    #[test]
    fn test_line_number_format() {
        let content = "line one\nline two\nline three\n";
        let total_lines = content.lines().count();
        let excerpt: String = content
            .lines()
            .take(3)
            .enumerate()
            .map(|(i, line)| {
                let width = (total_lines.to_string().len()).max(1);
                format!("{:>width$}: {}", i + 1, line)
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(excerpt, "1: line one\n2: line two\n3: line three");
    }
}
