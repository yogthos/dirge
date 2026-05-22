use std::borrow::Cow;
use std::fmt;

use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use rmcp::model::{CallToolRequestParams, JsonObject, RawContent};
use rmcp::service::{Peer, RoleClient};

use crate::agent::tools::check_perm;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

#[derive(Debug)]
pub struct McpToolError(pub String);

impl fmt::Display for McpToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for McpToolError {}

pub struct McpTool {
    pub server_name: String,
    pub definition: rmcp::model::Tool,
    pub peer: Peer<RoleClient>,
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

impl ToolDyn for McpTool {
    fn name(&self) -> String {
        self.definition.name.to_string()
    }

    fn definition(&self, _prompt: String) -> WasmBoxedFuture<'_, ToolDefinition> {
        let name = self.definition.name.to_string();
        let description = self
            .definition
            .description
            .clone()
            .unwrap_or(Cow::from(""))
            .to_string();
        // MCP servers that don't ship an `inputSchema` would
        // serialize as `null`, which violates rig's expectation of
        // an object. Substitute an empty object so the tool stays
        // usable (the LLM just won't have a hint that args are
        // expected, but it can still call the tool with no params).
        let parameters = serde_json::to_value(&self.definition.input_schema)
            .ok()
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| serde_json::json!({}));
        Box::pin(async move {
            ToolDefinition {
                name,
                description,
                parameters,
            }
        })
    }

    fn call(&self, args: String) -> WasmBoxedFuture<'_, Result<String, ToolError>> {
        let server_name = self.server_name.clone();
        let tool_name = self.definition.name.to_string();
        let peer = self.peer.clone();
        let permission = self.permission.clone();
        let ask_tx = self.ask_tx.clone();

        Box::pin(async move {
            let perm_key = format!("mcp_tool:{server_name}:{tool_name}");
            check_perm(&permission, &ask_tx, "mcp_tool", &perm_key)
                .await
                .map_err(|e| ToolError::ToolCallError(Box::new(McpToolError(e.to_string()))))?;

            // Malformed JSON used to silently default to `None` via
            // `unwrap_or_default()` — the MCP server got an empty
            // argument set and the agent saw a confusing "missing
            // required field" error from the server instead of a
            // dirge-side parse error. Surface the parse failure
            // distinctly so the agent can fix its tool call.
            //
            // Empty / whitespace-only args is treated as the explicit
            // no-arguments case (matches rig's default tool-call
            // shape when the LLM omits the arguments object).
            let trimmed = args.trim();
            let arguments: Option<JsonObject> = if trimmed.is_empty() {
                None
            } else {
                match serde_json::from_str::<JsonObject>(trimmed) {
                    Ok(obj) => Some(obj),
                    Err(e) => {
                        return Err(ToolError::ToolCallError(Box::new(McpToolError(format!(
                            "MCP tool {}::{}: malformed JSON arguments ({e}). Got: {trimmed:.200}",
                            server_name, tool_name,
                        )))));
                    }
                }
            };
            let params = arguments
                .map(|a| CallToolRequestParams::new(tool_name.clone()).with_arguments(a))
                .unwrap_or_else(|| CallToolRequestParams::new(tool_name.clone()));

            // MCP tool calls go over JSON-RPC to a spawned server
            // process. If the server hangs (deadlock, infinite
            // loop, lost stdin pipe), the await never resolves and
            // the agent turn stalls indefinitely. Cap at 120s to
            // match `bash`'s default timeout — anything longer is
            // clearly broken on the server side. The error message
            // names the server + tool so the user can identify
            // which MCP server is misbehaving.
            const MCP_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
            let result = match tokio::time::timeout(MCP_CALL_TIMEOUT, peer.call_tool(params)).await
            {
                Ok(r) => r.map_err(|e| {
                    ToolError::ToolCallError(Box::new(McpToolError(format!(
                        "MCP tool error ({}::{}): {e}",
                        server_name, tool_name,
                    ))))
                })?,
                Err(_) => {
                    return Err(ToolError::ToolCallError(Box::new(McpToolError(format!(
                        "MCP tool {}::{} timed out after {}s",
                        server_name,
                        tool_name,
                        MCP_CALL_TIMEOUT.as_secs(),
                    )))));
                }
            };

            if result.is_error.unwrap_or(false) {
                let error_msg = result
                    .content
                    .iter()
                    .filter_map(|c| match &c.raw {
                        RawContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let msg = if error_msg.is_empty() {
                    "MCP tool returned an error".to_string()
                } else {
                    error_msg
                };
                return Err(ToolError::ToolCallError(Box::new(McpToolError(msg))));
            }

            // Cap aggregate MCP result at 256 KiB before it
            // reaches LLM context. A misbehaving MCP server
            // returning a 200 KB+ blob would otherwise flood
            // every subsequent turn until compaction. The cap
            // matches the bash output cap below; tools wanting
            // larger payloads should chunk or return resource
            // URIs.
            const MCP_RESULT_CAP_BYTES: usize = 256 * 1024;
            let mut content = String::new();
            let mut truncated = false;
            for item in result.content {
                if truncated {
                    break;
                }
                let chunk: String = match item.raw {
                    RawContent::Text(t) => t.text,
                    RawContent::Image(img) => {
                        format!("data:{};base64,{}", img.mime_type, img.data)
                    }
                    RawContent::Resource(r) => match r.resource {
                        rmcp::model::ResourceContents::TextResourceContents { text, .. } => text,
                        rmcp::model::ResourceContents::BlobResourceContents { blob, .. } => blob,
                    },
                    _ => continue,
                };
                let remaining = MCP_RESULT_CAP_BYTES.saturating_sub(content.len());
                if chunk.len() <= remaining {
                    content.push_str(&chunk);
                } else {
                    // Find a UTF-8 char boundary at or below
                    // `remaining` so we don't slice through a
                    // multi-byte codepoint.
                    let mut cut = remaining;
                    while cut > 0 && !chunk.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    content.push_str(&chunk[..cut]);
                    truncated = true;
                }
            }
            if truncated {
                content.push_str(&format!(
                    "\n…[MCP result truncated at {} bytes — {}::{} returned more]",
                    MCP_RESULT_CAP_BYTES, server_name, tool_name,
                ));
            }
            Ok(content)
        })
    }
}
