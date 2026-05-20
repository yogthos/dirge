use rig::completion::ToolDefinition;
use rig::tool::Tool;
use tokio::time::{Duration, timeout};

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, BashArgs, PermCheck, ToolError, check_perm};

use crate::sandbox::Sandbox;
#[cfg(feature = "semantic-bash")]
use crate::semantic::adapters::bash;

pub struct BashTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub sandbox: Sandbox,
    cache: Option<ToolCache>,
}

impl BashTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>, sandbox: Sandbox) -> Self {
        BashTool {
            permission,
            ask_tx,
            sandbox,
            cache: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        sandbox: Sandbox,
        cache: ToolCache,
    ) -> Self {
        BashTool {
            permission,
            ask_tx,
            sandbox,
            cache: Some(cache),
        }
    }
}

impl Tool for BashTool {
    const NAME: &'static str = "bash";

    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description: "Execute a bash command in the current working directory. Returns stdout and stderr.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Bash command to execute" },
                    "timeout": { "type": "integer", "description": "Timeout in seconds (optional)" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: BashArgs) -> Result<String, ToolError> {
        check_bash_segments(&self.permission, &self.ask_tx, &args.command).await?;

        let output = if let Some(secs) = args.timeout {
            if secs == 0 {
                return Err(ToolError::Msg("timeout must be > 0".to_string()));
            }
            timeout(
                Duration::from_secs(secs),
                self.sandbox.wrap_command(&args.command).output(),
            )
            .await
            .map_err(|_| ToolError::Msg("Command timed out".to_string()))?
        } else {
            timeout(
                Duration::from_secs(120),
                self.sandbox.wrap_command(&args.command).output(),
            )
            .await
            .map_err(|_| ToolError::Msg("Command timed out after 120s".to_string()))?
        }?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let exit_code = output.status.code().unwrap_or(-1);

        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&stderr);
        }
        if exit_code != 0 {
            result.push_str(&format!("\nExit code: {}", exit_code));
        }
        // Bash may have mutated the filesystem; conservatively invalidate the
        // per-turn read/grep/list cache.
        if let Some(ref cache) = self.cache {
            cache.clear();
        }
        Ok(result)
    }
}

async fn check_bash_segments(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    command: &str,
) -> Result<(), ToolError> {
    #[cfg(feature = "semantic-bash")]
    {
        let (segments, complex) = bash::parse_bash_segments_full(command)
            .unwrap_or_else(|_| (vec![command.to_string()], false));

        if complex {
            return check_perm(permission, ask_tx, "bash", command).await;
        }

        for segment in &segments {
            check_perm(permission, ask_tx, "bash", segment).await?;
        }
        Ok(())
    }
    #[cfg(not(feature = "semantic-bash"))]
    {
        // Best-effort coarse split when tree-sitter isn't compiled in.
        // Without it, a command like `safe_cmd && rm -rf /` would be
        // checked as a single string against the bash rules and might
        // squeak through if `safe_cmd && rm` doesn't match any deny.
        // Split on the unambiguous compound separators (`&&`, `;`,
        // `||`) so each segment is checked individually. This won't
        // catch command substitution or subshells — those need the
        // tree-sitter feature for correct parsing — but it covers the
        // common compound case.
        let segments = command
            .split(|c| c == ';')
            .flat_map(|s| s.split("&&"))
            .flat_map(|s| s.split("||"))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<&str>>();
        // Flag command substitution / subshell constructs that need a
        // full parser. Surface as one whole-command check so the user
        // sees the unfamiliar form before any segment runs.
        let has_substitution = command.contains("$(")
            || command.contains('`')
            || command.contains("<(")
            || command.contains(">(");
        if has_substitution {
            return check_perm(permission, ask_tx, "bash", command).await;
        }
        for segment in &segments {
            check_perm(permission, ask_tx, "bash", segment).await?;
        }
        Ok(())
    }
}
