use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use rmcp::ServiceError;
use rmcp::model::{CallToolRequestParams, JsonObject, RawContent};
use tokio::sync::Mutex;

use crate::agent::tools::check_perm;
use crate::extras::mcp::client::{SharedConnection, raw_connect};
use crate::extras::mcp::config::McpServerConfig;
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
    /// Shared connection — peer + running_service co-owned with the
    /// manager and every other McpTool from this server. M-R1 review
    /// fix: previously each tool held a bare `Peer<RoleClient>` clone
    /// plus a separately leaked `RunningService`; the new shape keeps
    /// the running_service alive THROUGH the swap so reconnects
    /// don't leak the spawned child process.
    pub connection: Arc<SharedConnection>,
    /// Server config retained so a transport-class failure can
    /// trigger a self-reconnect without going through the manager.
    /// `None` for tools constructed by callers that don't supply
    /// the config (e.g. tests); auto-reconnect is skipped in that
    /// case and a clear error surfaces instead.
    pub config: Option<Arc<McpServerConfig>>,
    /// Per-server lock + generation counter. Multiple in-flight tool
    /// calls failing concurrently all wait on this; the gen lets the
    /// first reconnect mark the swap done so later callers re-read
    /// the peer without redundant reconnects. M-R2 review fix:
    /// constructed once per server at manager startup, not per
    /// `collect_tools` call, so the gen counter is canonical for
    /// the entire process lifetime.
    pub reconnect_lock: Arc<Mutex<u64>>,
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

/// Classify a [`ServiceError`] as transport-class (worth reconnecting)
/// versus everything else (surface as-is). M-R5 review tightening:
/// narrowed from the original aggressive set. Only the two unambiguous
/// transport-failure variants reconnect:
///
/// - `TransportSend` — the underlying writer failed.
/// - `TransportClosed` — the receiver task on our side observed EOF.
///
/// `UnexpectedResponse` (protocol mismatch — server is alive but
/// buggy), `Timeout` (a slow tool legitimately running long), and
/// `Cancelled` (user-driven abort) intentionally fall through to the
/// surface-as-is path. Reconnecting on those would mask real bugs or
/// tear down healthy connections mid-run.
fn is_transport_failure(err: &ServiceError) -> bool {
    matches!(
        err,
        ServiceError::TransportSend(_) | ServiceError::TransportClosed
    )
}

/// MCP tool permission trust model — read before assuming an MCP
/// tool obeys the same rules as a built-in:
///
/// All MCP tool calls route through `check_perm` with the umbrella
/// tool name `"mcp_tool"` and a perm key shaped
/// `mcp_tool:<server>:<name>`. They do NOT alias to dirge built-ins
/// — an MCP server exporting `edit_file` / `write` / `bash` is
/// gated by `mcp_tool` rules, NOT by the user's `edit:` / `write:` /
/// `bash:` rules.
///
/// Concretely, if the user configures:
///
///   "permission": {
///     "edit":     { "/etc/**": "deny" },
///     "mcp_tool": { "*":       "allow" }
///   }
///
/// …a built-in `edit` of `/etc/passwd` is denied, but an MCP-exported
/// `edit_file` call against `/etc/passwd` runs unprompted. To gate
/// MCP-exported edits, pin the qualified form:
///
///   "permission": {
///     "mcp_tool": {
///       "mcp_tool:fs:edit_file": "ask"
///     }
///   }
///
/// Prompt frontmatter `deny_tools` IS cross-checked against the
/// concrete MCP tool name (PERM-7 — handled inside
/// `PermissionChecker::check` plus the explicit `any_prompt_denied`
/// probe below), so plan-mode `deny_tools: [edit]` does block an
/// MCP-exported `edit`. Built-in tool *rule tables* don't alias.
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
        let connection = Arc::clone(&self.connection);
        let config = self.config.clone();
        let reconnect_lock = self.reconnect_lock.clone();
        let permission = self.permission.clone();
        let ask_tx = self.ask_tx.clone();

        Box::pin(async move {
            // Adversarial-review finding #1: MCP tools pass the
            // umbrella name `"mcp_tool"` to `check_perm`, which
            // means a prompt's `deny_tools: [edit]` would NOT match
            // an MCP server's `edit` tool — the literal string
            // comparison inside `is_prompt_denied` never sees the
            // concrete name. Probe explicitly for the concrete
            // name, the qualified `mcp_tool:server:name` form, and
            // the umbrella `mcp_tool`; any match denies before the
            // call leaves dirge.
            if let Some(perm) = permission.as_ref() {
                let qualified = format!("mcp_tool:{}:{}", server_name, tool_name);
                let denied = {
                    let guard = perm.lock().unwrap_or_else(|e| e.into_inner());
                    guard.any_prompt_denied(&[tool_name.as_str(), qualified.as_str(), "mcp_tool"])
                };
                if denied {
                    return Err(ToolError::ToolCallError(Box::new(McpToolError(format!(
                        "MCP tool {}::{} is denied by the active prompt's `deny_tools` frontmatter. Switch with `/prompt <other>` to use it.",
                        server_name, tool_name,
                    )))));
                }
            }
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

            // dirge-mgub: per-server cwd guard. By default MCP tool
            // calls whose JSON args name paths outside the working
            // directory are refused — this matches the trust model
            // of the built-in file tools (read/write/edit anchored to
            // cwd). Set `allow_external_paths: true` on the server's
            // config to opt out for that ONE server; deny rules,
            // doom-loop, and prompt deny-lists still apply because
            // they ran BEFORE this check.
            //
            // Heuristic: scan top-level args fields named `path` /
            // `file_path` / `file` / `directory` / `dir` / `cwd`
            // (scalar), or `paths` (array) — same key set used by the
            // context-depth tracker. Anything resolving outside the
            // working directory blocks the call with a clear message.
            let allow_external = config
                .as_ref()
                .map(|c| c.allow_external_paths())
                .unwrap_or(false);
            if let Some(perm) = permission.as_ref()
                && let Some(args_obj) = arguments.as_ref()
                && let Some(p) = first_external_path(perm, args_obj, allow_external)
            {
                return Err(ToolError::ToolCallError(Box::new(McpToolError(format!(
                    "MCP tool {server_name}::{tool_name} refused: path {p:?} is outside the working directory. \
                     Set `allow_external_paths: true` on the `{server_name}` server config to permit external paths for this server."
                )))));
            }

            let params = arguments
                .map(|a| CallToolRequestParams::new(tool_name.clone()).with_arguments(a))
                .unwrap_or_else(|| CallToolRequestParams::new(tool_name.clone()));

            // MCP tool calls go over JSON-RPC to a spawned server
            // process. If the server hangs (deadlock, infinite
            // loop, lost stdin pipe), the await never resolves and
            // the agent turn stalls indefinitely. Cap at 120s to
            // match `bash`'s default timeout — anything longer is
            // clearly broken on the server side.
            //
            // The cap is a TOTAL budget for the whole try-reconnect-
            // retry cycle (M-R3 review fix), not per-attempt. Worst
            // case the user waits 120s for everything; previously the
            // budget was 240s = 2 × 120s.
            const MCP_CALL_BUDGET: Duration = Duration::from_secs(120);
            let started = Instant::now();

            let result = match try_call_with_reconnect(
                &server_name,
                &connection,
                config.as_deref(),
                &reconnect_lock,
                params,
                started,
                MCP_CALL_BUDGET,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    return Err(ToolError::ToolCallError(Box::new(McpToolError(e))));
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

/// Try `peer.call_tool` once; on transport-class failure, swap the
/// shared connection for a freshly-reconnected one and retry exactly
/// once. Tool-level errors (server returned an error response) and
/// non-transport ServiceErrors surface verbatim — reconnecting
/// wouldn't help.
///
/// `started` + `total_budget` define the deadline for the WHOLE
/// try-reconnect-retry cycle (M-R3 fix). Each `call_once` invocation
/// receives whatever budget remains, so the worst-case latency
/// matches the prior single-attempt timeout.
///
/// The reconnect_lock + gen counter serializes concurrent callers
/// failing against the same dead transport: the first reconnects,
/// later callers see the bumped gen and skip the redundant work.
/// Config is required for the reconnect path; without it the
/// transport error surfaces immediately.
async fn try_call_with_reconnect(
    server_name: &str,
    connection: &Arc<SharedConnection>,
    config: Option<&McpServerConfig>,
    reconnect_lock: &Arc<Mutex<u64>>,
    params: CallToolRequestParams,
    started: Instant,
    total_budget: Duration,
) -> Result<rmcp::model::CallToolResult, String> {
    // Snapshot the generation BEFORE the first call so we can detect
    // after-the-fact reconnects below.
    let gen_before = *reconnect_lock.lock().await;

    let remaining = remaining_budget(started, total_budget);
    let first = call_once(server_name, connection, params.clone(), remaining).await;
    let err = match first {
        Ok(r) => return Ok(r),
        Err(e) => e,
    };

    // Non-transport error → surface as-is.
    let Some(svc_err) = err.as_service_error() else {
        return Err(err.message);
    };
    if !is_transport_failure(svc_err) {
        return Err(err.message);
    }

    // Transport failure. Without config we can't reconnect.
    let Some(cfg) = config else {
        return Err(format!(
            "{}\n(auto-reconnect unavailable — no config retained for server '{}')",
            err.message, server_name,
        ));
    };

    // Lock and reconnect (or skip if another caller beat us).
    {
        let mut gen_guard = reconnect_lock.lock().await;
        if *gen_guard == gen_before {
            tracing::warn!(
                target: "dirge::mcp",
                server = %server_name,
                "transport failure detected — attempting auto-reconnect",
            );
            // Bound the reconnect at the remaining budget so a wedged
            // server doesn't burn the whole thing without leaving any
            // for the retry call.
            let reconnect_budget = remaining_budget(started, total_budget);
            let reconnect_result =
                tokio::time::timeout(reconnect_budget, raw_connect(server_name, cfg)).await;
            match reconnect_result {
                Ok(Ok((new_peer, new_rs))) => {
                    connection.replace(new_peer, new_rs).await;
                    *gen_guard += 1;
                    tracing::info!(
                        target: "dirge::mcp",
                        server = %server_name,
                        "MCP server reconnected after transport failure",
                    );
                }
                Ok(Err(e)) => {
                    return Err(format!(
                        "{}\n(auto-reconnect to '{}' also failed: {})",
                        err.message, server_name, e,
                    ));
                }
                Err(_) => {
                    return Err(format!(
                        "{}\n(auto-reconnect to '{}' timed out within the {}s budget)",
                        err.message,
                        server_name,
                        total_budget.as_secs(),
                    ));
                }
            }
        }
        // else: another caller already reconnected; just retry with
        // the (newer) peer.
    }

    // Second attempt with the fresh peer.
    let remaining = remaining_budget(started, total_budget);
    if remaining.is_zero() {
        return Err(format!(
            "MCP tool {}::{} budget ({}s) exhausted before retry",
            server_name,
            params.name,
            total_budget.as_secs(),
        ));
    }
    match call_once(server_name, connection, params, remaining).await {
        Ok(r) => Ok(r),
        Err(e) => Err(format!(
            "{}\n(reconnected but the retry also failed)",
            e.message,
        )),
    }
}

/// Time left in the budget. Returns `Duration::ZERO` (NOT a negative)
/// when the deadline has passed; `tokio::time::timeout(ZERO, _)` then
/// fires immediately and surfaces the budget-exhausted state.
fn remaining_budget(started: Instant, total: Duration) -> Duration {
    total.saturating_sub(started.elapsed())
}

/// Tagged error for `try_call_with_reconnect` — distinguishes
/// transport failures (worth retrying) from tool-level errors
/// (surface as-is).
struct CallErr {
    message: String,
    service_error: Option<ServiceError>,
}

impl CallErr {
    fn as_service_error(&self) -> Option<&ServiceError> {
        self.service_error.as_ref()
    }
}

async fn call_once(
    server_name: &str,
    connection: &Arc<SharedConnection>,
    params: CallToolRequestParams,
    timeout: Duration,
) -> Result<rmcp::model::CallToolResult, CallErr> {
    let tool_name = params.name.to_string();
    // Snapshot the current peer. Held briefly across the read-lock;
    // the actual call doesn't hold the lock so another caller can
    // swap the peer (manager-side or tool-side reconnect) without
    // blocking on us.
    let peer = connection.current_peer().await;
    match tokio::time::timeout(timeout, peer.call_tool(params)).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(svc_err)) => {
            let msg = format!("MCP tool error ({server_name}::{tool_name}): {svc_err}");
            Err(CallErr {
                message: msg,
                service_error: Some(svc_err),
            })
        }
        Err(_) => Err(CallErr {
            message: format!(
                "MCP tool {server_name}::{tool_name} timed out after {}s",
                timeout.as_secs(),
            ),
            service_error: Some(ServiceError::Timeout { timeout }),
        }),
    }
}

/// Per-server cwd-external-path guard (dirge-mgub).
///
/// Returns `Some(path)` for the FIRST path-shaped argument that
/// resolves outside the working directory when external paths are NOT
/// allowed; `None` otherwise (either the server opted into external
/// paths, or every extracted path stays inside cwd, or no paths were
/// extracted at all).
///
/// Pulled out of `McpTool::call` so the guard can be unit-tested
/// without standing up a live MCP server.
pub(crate) fn first_external_path(
    perm: &PermCheck,
    args: &JsonObject,
    allow_external: bool,
) -> Option<String> {
    if allow_external {
        return None;
    }
    let paths = extract_arg_paths(args);
    if paths.is_empty() {
        return None;
    }
    let guard = perm.lock().unwrap_or_else(|e| e.into_inner());
    paths.into_iter().find(|p| guard.is_external_path(p))
}

/// Best-effort extraction of path-shaped arguments from an MCP tool
/// call's JSON object. Used by the cwd-external-path guard
/// (dirge-mgub) to decide whether the call wants to touch the
/// filesystem outside the working directory.
///
/// Scans the TOP LEVEL of the args object for:
///   - scalar fields named `path`, `file_path`, `file`, `directory`,
///     `dir`, `cwd` → one path each
///   - the array field `paths` → one path per string element
///
/// Returns paths in declaration order; duplicates are preserved
/// (the caller short-circuits on the first external hit anyway).
/// Empty strings are filtered out — they're never a legitimate
/// filesystem reference and would canonicalize to `working_dir`
/// itself, falsely classifying as internal.
fn extract_arg_paths(args: &JsonObject) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in ["path", "file_path", "file", "directory", "dir", "cwd"] {
        if let Some(s) = args.get(key).and_then(|v| v.as_str())
            && !s.is_empty()
        {
            out.push(s.to_string());
        }
    }
    if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str()
                && !s.is_empty()
            {
                out.push(s.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classification matrix for `is_transport_failure`. M-R5 review
    /// tightening: ONLY the two unambiguous transport-failure
    /// variants reconnect. `UnexpectedResponse`, `Timeout`,
    /// `McpError`, `Cancelled` surface as-is — previously
    /// `UnexpectedResponse`+`Timeout` reconnected too, which would
    /// tear down healthy connections on a slow tool or a buggy
    /// server reply.
    #[test]
    fn is_transport_failure_classifies_correctly() {
        // Transport-class → reconnect
        assert!(is_transport_failure(&ServiceError::TransportClosed));

        // Non-transport → surface as-is.
        assert!(!is_transport_failure(&ServiceError::UnexpectedResponse));
        assert!(!is_transport_failure(&ServiceError::Timeout {
            timeout: Duration::from_secs(1),
        }));
        let mcp_err = rmcp::ErrorData::new(
            rmcp::model::ErrorCode::INTERNAL_ERROR,
            "the tool refused",
            None,
        );
        assert!(!is_transport_failure(&ServiceError::McpError(mcp_err)));
        assert!(!is_transport_failure(&ServiceError::Cancelled {
            reason: Some("user".into()),
        }));
    }

    /// `remaining_budget` decays as time passes and saturates at
    /// zero past the deadline (no negative durations / underflow).
    #[test]
    fn remaining_budget_decays_and_saturates() {
        let now = Instant::now();
        let total = Duration::from_millis(100);
        // Fresh start — full budget available.
        let r1 = remaining_budget(now, total);
        assert!(r1 > Duration::from_millis(90));
        // Past the deadline — saturates to ZERO, not negative.
        std::thread::sleep(Duration::from_millis(110));
        let r2 = remaining_budget(now, total);
        assert_eq!(r2, Duration::ZERO);
    }

    // ── dirge-mgub: per-server external-path guard ────────────────

    use crate::agent::tools::check_perm;
    use crate::permission::{
        Action, PermissionConfig, SecurityMode, ToolPerm, checker::PermissionChecker,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex as StdMutex};

    /// Build a PermissionChecker anchored at a temporary directory
    /// the test fully controls. `extra_rules` lets a caller install
    /// per-tool rules (e.g. a `mcp_tool` deny) on top of the default
    /// config. Returns `(perm, cwd_string)` so tests can craft path
    /// arguments relative to (or escaping) the cwd.
    fn mk_perm(extra_rules: PermissionConfig) -> (PermCheck, String) {
        let cwd = std::env::temp_dir().join(format!(
            "dirge-mgub-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&cwd).expect("create temp cwd");
        let checker =
            PermissionChecker::new(&extra_rules, SecurityMode::Standard, Some(cwd.clone()));
        let perm: PermCheck = Arc::new(StdMutex::new(checker));
        (perm, cwd.to_string_lossy().into_owned())
    }

    /// dirge-mgub test 1: default behavior (`allow_external_paths=
    /// false`) refuses MCP tool args naming a path outside the
    /// working directory. The guard returns `Some(path)` so the
    /// caller surfaces a refusal error.
    #[test]
    fn mcp_allow_external_paths_default_false_blocks_external() {
        let (perm, _cwd) = mk_perm(PermissionConfig::default());
        // An absolute path that is NOT inside the temp cwd. `/etc`
        // is sufficiently far outside any reasonable temp root.
        let args: JsonObject =
            serde_json::from_str(r#"{"path": "/etc/passwd"}"#).expect("parse args");

        let hit = first_external_path(&perm, &args, false);
        assert_eq!(
            hit.as_deref(),
            Some("/etc/passwd"),
            "default config must flag an external path; got {hit:?}",
        );
    }

    /// dirge-mgub test 2: opting into `allow_external_paths=true`
    /// permits the same call by skipping the cwd-external-path check
    /// entirely. The guard returns `None` regardless of how far the
    /// path sits outside cwd.
    #[test]
    fn mcp_allow_external_paths_true_permits_external() {
        let (perm, _cwd) = mk_perm(PermissionConfig::default());
        let args: JsonObject =
            serde_json::from_str(r#"{"path": "/etc/passwd", "paths": ["/var/log/system.log"]}"#)
                .expect("parse args");

        let hit = first_external_path(&perm, &args, true);
        assert!(
            hit.is_none(),
            "allow_external_paths=true must skip the cwd guard; got {hit:?}",
        );
    }

    /// dirge-mgub test 3: `allow_external_paths=true` ONLY toggles
    /// the cwd guard — it does NOT bypass deny rules. A `mcp_tool`
    /// rule that denies the qualified call still fires through the
    /// normal `check_perm` path (which runs BEFORE the guard in
    /// `McpTool::call`), so a deny rule + `allow_external_paths`
    /// + an external path still results in refusal.
    #[tokio::test]
    async fn mcp_allow_external_paths_does_not_bypass_deny_rules() {
        // Install a deny rule that matches the qualified MCP key
        // `mcp_tool:indexer:scan`.
        let mut mcp_rules: HashMap<String, Action> = HashMap::new();
        mcp_rules.insert("mcp_tool:indexer:*".to_string(), Action::Deny);
        let config = PermissionConfig {
            mcp_tool: Some(ToolPerm::Granular(mcp_rules)),
            ..Default::default()
        };
        let (perm, _cwd) = mk_perm(config);
        let args: JsonObject =
            serde_json::from_str(r#"{"path": "/etc/passwd"}"#).expect("parse args");

        // First arm: deny rule fires through check_perm regardless
        // of the external-path flag. McpTool::call routes through
        // check_perm("mcp_tool", "mcp_tool:indexer:scan") BEFORE
        // the path guard; a deny here aborts the call before the
        // guard ever runs.
        let perm_key = "mcp_tool:indexer:scan".to_string();
        let result = check_perm(&Some(perm.clone()), &None, "mcp_tool", &perm_key).await;
        assert!(
            result.is_err(),
            "deny rule must block the call even when allow_external_paths=true",
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("denied") || msg.contains("Deny") || msg.contains("Blocked"),
            "expected deny message, got {msg:?}",
        );

        // Second arm: confirm the guard ITSELF still respects the
        // allow_external_paths=true opt-out. (Defense in depth —
        // both checks must agree the flag is path-scoped only.)
        let guard_hit = first_external_path(&perm, &args, true);
        assert!(
            guard_hit.is_none(),
            "guard must skip cwd-check on allow_external_paths=true; got {guard_hit:?}",
        );
    }

    /// Empty / no-path arg objects do not produce false positives.
    /// MCP tools without filesystem args (e.g. a search query) must
    /// pass the guard even with `allow_external_paths=false`.
    #[test]
    fn mcp_external_path_guard_skips_argless_calls() {
        let (perm, _cwd) = mk_perm(PermissionConfig::default());
        let args: JsonObject = serde_json::from_str(r#"{"query": "needle"}"#).expect("parse args");
        assert!(first_external_path(&perm, &args, false).is_none());
    }

    /// In-cwd paths (relative or absolute) pass the guard. Only
    /// paths that resolve OUTSIDE cwd should trigger refusal.
    #[test]
    fn mcp_external_path_guard_permits_in_cwd_paths() {
        let (perm, cwd) = mk_perm(PermissionConfig::default());
        let abs_in = format!("{cwd}/inside.txt");
        let args = serde_json::json!({
            "path": abs_in,
            "paths": ["./relative-inside.rs"],
        });
        let obj = args.as_object().unwrap().clone();
        assert!(first_external_path(&perm, &obj, false).is_none());
    }

    /// `allow_external_paths` is round-trip-deserializable on both
    /// `Command` and `Url` variants of `McpServerConfig`, and
    /// defaults to `false` when omitted.
    #[test]
    fn mcp_server_config_allow_external_paths_round_trip() {
        let cmd_default: McpServerConfig = serde_json::from_str(r#"{"command": "x"}"#).unwrap();
        assert!(!cmd_default.allow_external_paths());

        let cmd_true: McpServerConfig =
            serde_json::from_str(r#"{"command": "x", "allow_external_paths": true}"#).unwrap();
        assert!(cmd_true.allow_external_paths());

        let url_default: McpServerConfig = serde_json::from_str(r#"{"url": "https://x"}"#).unwrap();
        assert!(!url_default.allow_external_paths());

        let url_true: McpServerConfig =
            serde_json::from_str(r#"{"url": "https://x", "allow_external_paths": true}"#).unwrap();
        assert!(url_true.allow_external_paths());
    }
}
