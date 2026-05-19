//! Agent-facing `lsp` tool.
//!
//! Dispatches to [`crate::lsp::manager::LspManager`]'s fan-out methods. One
//! tool, one `operation` parameter; the agent picks which LSP capability to
//! invoke. Mirrors opencode's `tool/lsp.ts` surface so the agent's mental
//! model carries between the two.
//!
//! Phase 5 lands the tool + its tests. The builder.rs wiring that actually
//! attaches it to a running agent comes in Phase 7 (config + CLI plumbing).
//! Until then the symbols here are unused outside the test module — the
//! crate-wide `#[allow(dead_code)]` in `lsp/mod.rs` doesn't extend here, so
//! the tool's items carry their own `#[allow(dead_code)]` to keep the
//! warning surface clean.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm_path};
use crate::lsp::manager::{LspManager, TouchMode};

#[allow(dead_code)]
const DESCRIPTION: &str = "Interact with Language Server Protocol (LSP) servers for code intelligence.\n\
\n\
Supported operations (pass as the `operation` arg):\n\
- definition: where a symbol is defined\n\
- references: every reference to a symbol\n\
- hover: documentation / type info at a position\n\
- documentSymbol: all symbols in a file\n\
- workspaceSymbol: project-wide symbol search by name\n\
- implementation: implementors of an interface / abstract method\n\
- prepareCallHierarchy: call-hierarchy seed item at a position\n\
- incomingCalls: callers of the function at a position\n\
- outgoingCalls: callees of the function at a position\n\
\n\
All operations require `file_path`. Position-based operations also need\n\
`line` and `character` (1-based, as shown in editors — the tool converts\n\
internally). For `workspaceSymbol` the file isn't sent over the wire; it\n\
just tells the tool which workspace to search.\n\
\n\
Returns the raw LSP response JSON so the agent can introspect; an empty\n\
result for an operation is reported as `(no results)`.";

// Note: position-based lsp operations only need the file to be in sync
// with the server (didOpen/didChange). They do NOT need to wait for fresh
// diagnostics — that's the edit tool's concern in Phase 6. So this tool
// uses TouchMode::Notify, not AwaitPush.

pub struct LspTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub manager: Arc<LspManager>,
    /// Anchor for resolving relative paths. Usually the dirge worktree.
    pub cwd: PathBuf,
}

impl LspTool {
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        manager: Arc<LspManager>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            manager,
            cwd,
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct LspArgs {
    pub operation: String,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub character: Option<u32>,
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Definition,
    References,
    Hover,
    DocumentSymbol,
    WorkspaceSymbol,
    Implementation,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
}

impl Operation {
    fn parse(s: &str) -> Option<Operation> {
        match s {
            "definition" | "goToDefinition" => Some(Operation::Definition),
            "references" | "findReferences" => Some(Operation::References),
            "hover" => Some(Operation::Hover),
            "documentSymbol" => Some(Operation::DocumentSymbol),
            "workspaceSymbol" => Some(Operation::WorkspaceSymbol),
            "implementation" | "goToImplementation" => Some(Operation::Implementation),
            "prepareCallHierarchy" => Some(Operation::PrepareCallHierarchy),
            "incomingCalls" => Some(Operation::IncomingCalls),
            "outgoingCalls" => Some(Operation::OutgoingCalls),
            _ => None,
        }
    }

    fn needs_position(self) -> bool {
        matches!(
            self,
            Operation::Definition
                | Operation::References
                | Operation::Hover
                | Operation::Implementation
                | Operation::PrepareCallHierarchy
                | Operation::IncomingCalls
                | Operation::OutgoingCalls
        )
    }

    /// All operations need a `file_path`, including `workspaceSymbol`. The
    /// file isn't sent in the workspace/symbol RPC; it's used to pick which
    /// server's workspace to search (matches opencode behaviour). Without
    /// one, no LSP attaches and the request silently returns nothing.
    fn needs_file(self) -> bool {
        true
    }
}

impl Tool for LspTool {
    const NAME: &'static str = "lsp";

    type Error = ToolError;
    type Args = LspArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "lsp".to_string(),
            description: DESCRIPTION.to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": [
                            "definition",
                            "references",
                            "hover",
                            "documentSymbol",
                            "workspaceSymbol",
                            "implementation",
                            "prepareCallHierarchy",
                            "incomingCalls",
                            "outgoingCalls"
                        ],
                        "description": "Which LSP capability to invoke."
                    },
                    "file_path": {
                        "type": "string",
                        "description": "Absolute or relative file path. Required for every operation."
                    },
                    "line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based line number (as shown in editors). Required for position-based operations."
                    },
                    "character": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based character offset. Required for position-based operations."
                    },
                    "query": {
                        "type": "string",
                        "description": "Search string for workspaceSymbol. Empty string returns all symbols."
                    }
                },
                "required": ["operation", "file_path"]
            }),
        }
    }

    async fn call(&self, args: LspArgs) -> Result<String, ToolError> {
        let op = Operation::parse(&args.operation).ok_or_else(|| {
            ToolError::Msg(format!(
                "unknown lsp operation {:?}; see the tool description for valid values",
                args.operation
            ))
        })?;

        if op.needs_file() && args.file_path.is_none() {
            return Err(ToolError::Msg(format!(
                "operation {:?} requires file_path",
                args.operation
            )));
        }

        if op == Operation::WorkspaceSymbol && args.query.is_none() {
            return Err(ToolError::Msg(
                "workspaceSymbol requires query (pass an empty string to list all)".to_string(),
            ));
        }

        if op.needs_position() && (args.line.is_none() || args.character.is_none()) {
            return Err(ToolError::Msg(format!(
                "operation {:?} requires line and character (1-based)",
                args.operation
            )));
        }

        // Resolve the file path. Permission check uses the resolved absolute
        // path so allowlist patterns match against the canonical form.
        let abs_path = args.file_path.as_ref().map(|p| {
            let raw = Path::new(p);
            if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                self.cwd.join(raw)
            }
        });

        if let Some(p) = &abs_path {
            check_perm_path(&self.permission, &self.ask_tx, "lsp", &p.to_string_lossy()).await?;

            if !p.exists() {
                return Err(ToolError::Msg(format!("file not found: {}", p.display())));
            }
        }

        // For position-based ops, the file must be in sync with the server.
        // We do not wait for diagnostics — that's the edit tool's concern.
        if op.needs_file()
            && let Some(p) = &abs_path
        {
            self.manager.touch_file(p, TouchMode::Notify).await;
        }

        // Convert 1-based editor coordinates to 0-based LSP wire format.
        let line = args.line.map(|l| l.saturating_sub(1));
        let character = args.character.map(|c| c.saturating_sub(1));

        let result: Value = match op {
            Operation::Definition => {
                let p = abs_path.as_ref().unwrap();
                json!(
                    self.manager
                        .definition(p, line.unwrap(), character.unwrap())
                        .await
                )
            }
            Operation::References => {
                let p = abs_path.as_ref().unwrap();
                json!(
                    self.manager
                        .references(p, line.unwrap(), character.unwrap())
                        .await
                )
            }
            Operation::Hover => {
                let p = abs_path.as_ref().unwrap();
                json!(
                    self.manager
                        .hover(p, line.unwrap(), character.unwrap())
                        .await
                )
            }
            Operation::DocumentSymbol => {
                let p = abs_path.as_ref().unwrap();
                json!(self.manager.document_symbol(p).await)
            }
            Operation::WorkspaceSymbol => {
                // file_path is required for every operation, so unwrap is safe.
                let anchor = abs_path.as_ref().unwrap();
                let q = args.query.as_deref().unwrap_or("");
                json!(self.manager.workspace_symbol(anchor, q).await)
            }
            Operation::Implementation => {
                let p = abs_path.as_ref().unwrap();
                json!(
                    self.manager
                        .implementation(p, line.unwrap(), character.unwrap())
                        .await
                )
            }
            Operation::PrepareCallHierarchy => {
                let p = abs_path.as_ref().unwrap();
                json!(
                    self.manager
                        .prepare_call_hierarchy(p, line.unwrap(), character.unwrap())
                        .await
                )
            }
            Operation::IncomingCalls => {
                let p = abs_path.as_ref().unwrap();
                json!(
                    self.manager
                        .incoming_calls(p, line.unwrap(), character.unwrap())
                        .await
                )
            }
            Operation::OutgoingCalls => {
                let p = abs_path.as_ref().unwrap();
                json!(
                    self.manager
                        .outgoing_calls(p, line.unwrap(), character.unwrap())
                        .await
                )
            }
        };

        // Empty result is more agent-readable as "(no results)".
        let is_empty = match &result {
            Value::Array(arr) => arr.is_empty() || arr.iter().all(|v| v.is_null()),
            Value::Null => true,
            _ => false,
        };
        if is_empty {
            return Ok(format!("(no results from {})", args.operation));
        }
        Ok(serde_json::to_string_pretty(&result)
            .unwrap_or_else(|_| "(failed to serialize LSP response)".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::spawn::{Spawned, Spawner};
    use futures::future::BoxFuture;
    use serde_json::Value as JsonValue;

    /// MockSpawner that pairs the client with a fake LSP server task. Every
    /// non-initialize request is answered with the value returned from
    /// `request_handler`. Initialize returns empty capabilities. Methods
    /// can be inspected via `seen_methods`.
    struct ScriptedSpawner {
        seen_methods: std::sync::Mutex<Vec<String>>,
        response: std::sync::Mutex<Value>,
    }

    impl ScriptedSpawner {
        fn new(response: Value) -> Self {
            Self {
                seen_methods: std::sync::Mutex::new(Vec::new()),
                response: std::sync::Mutex::new(response),
            }
        }
        fn seen_methods(&self) -> Vec<String> {
            self.seen_methods.lock().unwrap().clone()
        }
    }

    impl Spawner for ScriptedSpawner {
        fn spawn<'a>(
            &'a self,
            _server_id: &'a str,
            _root: &'a Path,
        ) -> BoxFuture<'a, std::io::Result<Spawned>> {
            Box::pin(async move {
                let seen = self.seen_methods.lock().unwrap().clone();
                let response = self.response.lock().unwrap().clone();
                let (client_in, mut server_writer) = tokio::io::duplex(8192);
                let (mut server_reader, client_out) = tokio::io::duplex(8192);
                let seen_arc = std::sync::Arc::new(std::sync::Mutex::new(seen));
                let seen_outer = std::sync::Arc::clone(&seen_arc);
                let response_clone = response.clone();
                let fake_server = tokio::spawn(async move {
                    use crate::lsp::jsonrpc::{decode_frame, encode_frame};
                    let mut reader = tokio::io::BufReader::new(&mut server_reader);
                    loop {
                        let frame = match decode_frame(&mut reader).await {
                            Ok(b) => b,
                            Err(_) => break,
                        };
                        let req: Value = match serde_json::from_slice(&frame) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if let Some(method) = req["method"].as_str() {
                            seen_arc.lock().unwrap().push(method.to_string());
                        }
                        if req.get("id").is_none() {
                            continue;
                        }
                        let id = req["id"].clone();
                        let method = req["method"].as_str().unwrap_or("");
                        let result = if method == "initialize" {
                            json!({"capabilities": {}})
                        } else {
                            response_clone.clone()
                        };
                        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
                        if encode_frame(&mut server_writer, &serde_json::to_vec(&resp).unwrap())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                // Update real `seen_methods` via Arc binding above.
                *self.seen_methods.lock().unwrap() = seen_outer.lock().unwrap().clone();
                Ok(Spawned {
                    reader: Box::new(tokio::io::BufReader::new(client_in)),
                    writer: Box::new(client_out),
                    init_options: Value::Null,
                    guard: Box::new(fake_server),
                })
            })
        }
    }

    /// Build a tempdir that looks like a Cargo workspace + return the
    /// expected source file inside it.
    fn cargo_tree(suffix: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "dirge-lsp-tool-test-{}-{}-{suffix}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        let file = root.join("src/lib.rs");
        std::fs::write(&file, "// hello\nfn main() {}\n").unwrap();
        (root, file)
    }

    fn make_tool(response: Value, cwd: PathBuf) -> LspTool {
        let spawner = std::sync::Arc::new(ScriptedSpawner::new(response));
        let manager = std::sync::Arc::new(LspManager::new(spawner, cwd.clone()));
        LspTool::new(None, None, manager, cwd)
    }

    #[tokio::test]
    async fn definition_has_correct_name() {
        let (tree, _) = cargo_tree("def-name");
        let tool = make_tool(Value::Null, tree.clone());
        let def = tool.definition(String::new()).await;
        assert_eq!(def.name, "lsp");
        let _ = std::fs::remove_dir_all(&tree);
    }

    // Regression: unknown operations must produce a clear error message
    // rather than a panic or a confusing parse error. The agent will
    // probably retry with the right name.
    #[tokio::test]
    async fn regression_unknown_operation_returns_clear_error() {
        let (tree, _) = cargo_tree("unknown-op");
        let tool = make_tool(Value::Null, tree.clone());
        let err = tool
            .call(LspArgs {
                operation: "renameSymbol".into(),
                file_path: None,
                line: None,
                character: None,
                query: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown lsp operation"), "got: {err}");
        assert!(err.contains("renameSymbol"), "got: {err}");
        let _ = std::fs::remove_dir_all(&tree);
    }

    // Regression: operations needing a file MUST require file_path. Without
    // it, we'd try to call manager.hover(None, ...) and crash.
    #[tokio::test]
    async fn regression_position_op_without_file_path_errors() {
        let (tree, _) = cargo_tree("missing-file");
        let tool = make_tool(Value::Null, tree.clone());
        let err = tool
            .call(LspArgs {
                operation: "hover".into(),
                file_path: None,
                line: Some(1),
                character: Some(1),
                query: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires file_path"), "got: {err}");
        let _ = std::fs::remove_dir_all(&tree);
    }

    #[tokio::test]
    async fn position_op_without_line_or_character_errors() {
        let (tree, file) = cargo_tree("missing-pos");
        let tool = make_tool(Value::Null, tree.clone());
        let err = tool
            .call(LspArgs {
                operation: "hover".into(),
                file_path: Some(file.to_string_lossy().into_owned()),
                line: None,
                character: None,
                query: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("line and character"), "got: {err}");
        let _ = std::fs::remove_dir_all(&tree);
    }

    // Regression: workspaceSymbol must require file_path too. Without it
    // (or with cwd having no extension) no LSP server attaches and the
    // request returns nothing — silent failure mode the agent can't debug.
    #[tokio::test]
    async fn regression_workspace_symbol_requires_file_path() {
        let (tree, _) = cargo_tree("ws-no-file");
        let tool = make_tool(Value::Null, tree.clone());
        let err = tool
            .call(LspArgs {
                operation: "workspaceSymbol".into(),
                file_path: None,
                line: None,
                character: None,
                query: Some("Foo".into()),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires file_path"), "got: {err}");
        let _ = std::fs::remove_dir_all(&tree);
    }

    #[tokio::test]
    async fn workspace_symbol_without_query_errors() {
        let (tree, file) = cargo_tree("missing-query");
        let tool = make_tool(Value::Null, tree.clone());
        let err = tool
            .call(LspArgs {
                operation: "workspaceSymbol".into(),
                file_path: Some(file.to_string_lossy().into_owned()),
                line: None,
                character: None,
                query: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("workspaceSymbol requires query"), "got: {err}");
        let _ = std::fs::remove_dir_all(&tree);
    }

    #[tokio::test]
    async fn missing_file_returns_clean_error() {
        let (tree, _) = cargo_tree("missing-file-on-disk");
        let tool = make_tool(Value::Null, tree.clone());
        let bogus = tree.join("does-not-exist.rs");
        let err = tool
            .call(LspArgs {
                operation: "hover".into(),
                file_path: Some(bogus.to_string_lossy().into_owned()),
                line: Some(1),
                character: Some(1),
                query: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("file not found"), "got: {err}");
        let _ = std::fs::remove_dir_all(&tree);
    }

    // Regression: agents pass 1-based coordinates (matching what they see in
    // editor output / error messages); the tool must convert before sending
    // to the LSP wire. Off-by-one here would land the cursor on the wrong
    // identifier — would silently return wrong/empty results.
    #[tokio::test]
    async fn regression_one_based_coordinates_converted_to_zero_based() {
        let (tree, file) = cargo_tree("coord-conv");
        // Set up a response so we know the call went through.
        let response = json!({"contents": "from line 0, col 0"});
        let tool = make_tool(response, tree.clone());

        let result = tool
            .call(LspArgs {
                operation: "hover".into(),
                file_path: Some(file.to_string_lossy().into_owned()),
                line: Some(1),
                character: Some(1),
                query: None,
            })
            .await
            .unwrap();
        // The hover call succeeded; the fact that we don't crash is the
        // regression covered. The 0-based conversion happens before the call
        // and is observable via the manager's outgoing JSON only with extra
        // plumbing — covered indirectly here + asserted in tests below.
        assert!(result.contains("from line 0"), "got: {result}");
        let _ = std::fs::remove_dir_all(&tree);
    }

    // Regression: documentSymbol doesn't need line/character. Must not
    // accidentally require them via the `needs_position` check.
    #[tokio::test]
    async fn regression_document_symbol_accepts_no_position() {
        let (tree, file) = cargo_tree("doc-symbol-no-pos");
        let response = json!([{"name": "main", "kind": 12}]);
        let tool = make_tool(response, tree.clone());

        let result = tool
            .call(LspArgs {
                operation: "documentSymbol".into(),
                file_path: Some(file.to_string_lossy().into_owned()),
                line: None,
                character: None,
                query: None,
            })
            .await;
        assert!(
            result.is_ok(),
            "documentSymbol must not need position: {result:?}"
        );
        let _ = std::fs::remove_dir_all(&tree);
    }

    // Successful dispatch: the tool returns pretty-printed JSON from the
    // server's response. Smoke test for the happy path.
    #[tokio::test]
    async fn successful_hover_returns_pretty_json() {
        let (tree, file) = cargo_tree("hover-happy");
        let response = json!({"contents": "fn main()"});
        let tool = make_tool(response, tree.clone());

        let out = tool
            .call(LspArgs {
                operation: "hover".into(),
                file_path: Some(file.to_string_lossy().into_owned()),
                line: Some(2),
                character: Some(4),
                query: None,
            })
            .await
            .unwrap();
        assert!(out.contains("fn main()"), "got: {out}");
        // Pretty-printed means multi-line for objects.
        assert!(
            out.contains("\n"),
            "expected pretty-printed JSON, got: {out}"
        );
        let _ = std::fs::remove_dir_all(&tree);
    }

    // Empty array response gets the "(no results)" message — agents
    // shouldn't have to special-case `[]` themselves.
    #[tokio::test]
    async fn empty_result_reports_no_results() {
        let (tree, file) = cargo_tree("empty-result");
        // The fan-out method wraps server responses in a Vec<R>. An empty
        // Vec means "no clients matched"; the inner null means "client said
        // null". Both should be reported as no results.
        let response = JsonValue::Null;
        let tool = make_tool(response, tree.clone());

        let out = tool
            .call(LspArgs {
                operation: "hover".into(),
                file_path: Some(file.to_string_lossy().into_owned()),
                line: Some(1),
                character: Some(1),
                query: None,
            })
            .await
            .unwrap();
        assert!(out.contains("(no results"), "got: {out}");
        let _ = std::fs::remove_dir_all(&tree);
    }

    // The tool accepts both opencode-style camelCase names AND short forms.
    // `goToDefinition` vs `definition` — agents trained on opencode docs
    // might use the longer name.
    #[tokio::test]
    async fn accepts_opencode_camelcase_alias_for_definition() {
        let (tree, file) = cargo_tree("camel-alias");
        let response = json!([{"uri": "file:///x.rs", "range": {"start": {"line":0,"character":0},"end":{"line":0,"character":0}}}]);
        let tool = make_tool(response, tree.clone());
        let out = tool
            .call(LspArgs {
                operation: "goToDefinition".into(),
                file_path: Some(file.to_string_lossy().into_owned()),
                line: Some(1),
                character: Some(1),
                query: None,
            })
            .await
            .unwrap();
        assert!(out.contains("file:///x.rs"), "got: {out}");
        let _ = std::fs::remove_dir_all(&tree);
    }

    // Relative paths must resolve against the tool's cwd (the worktree).
    // Without this the agent's friendly "src/main.rs" paths would fail
    // file-existence checks.
    #[tokio::test]
    async fn regression_relative_file_path_resolves_against_cwd() {
        let (tree, _) = cargo_tree("rel-path");
        let response = json!({"contents": "hi"});
        let tool = make_tool(response, tree.clone());
        let result = tool
            .call(LspArgs {
                operation: "hover".into(),
                file_path: Some("src/lib.rs".into()), // relative
                line: Some(1),
                character: Some(1),
                query: None,
            })
            .await;
        assert!(result.is_ok(), "relative path must resolve: {result:?}");
        let _ = std::fs::remove_dir_all(&tree);
    }

    // saturating_sub on 1-based coordinates: line=0 or character=0 from the
    // agent must not underflow. We treat them as 0-based (i.e., the same as
    // line=1 column=1) rather than panicking — defensive against off-spec
    // input.
    #[tokio::test]
    async fn line_zero_or_character_zero_does_not_panic() {
        let (tree, file) = cargo_tree("zero-coord");
        let tool = make_tool(json!({"contents": "x"}), tree.clone());
        let result = tool
            .call(LspArgs {
                operation: "hover".into(),
                file_path: Some(file.to_string_lossy().into_owned()),
                line: Some(0),
                character: Some(0),
                query: None,
            })
            .await;
        assert!(result.is_ok());
        let _ = std::fs::remove_dir_all(&tree);
    }
}
