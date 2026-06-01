use rig::completion::ToolDefinition;
use rig::tool::Tool;

mod check;
mod exec;
use exec::{run_with_timeout, spawn_streaming_shell};

use crate::agent::agent_loop::tool_input_repair::with_contract_hint;
use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, BashArgs, PermCheck, ToolError};

use crate::sandbox::Sandbox;

pub struct BashTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub sandbox: Sandbox,
    cache: Option<ToolCache>,
    /// Shared background-shell registry. When present, `background: true`
    /// runs the command detached (unbounded) and tracks it here so the
    /// model can read its output (`bash_output`) and stop it
    /// (`kill_shell`). When absent (e.g. some headless paths) `background`
    /// degrades gracefully to synchronous execution.
    shell_store: Option<crate::agent::tools::bg_shell::BackgroundShellStore>,
}

impl BashTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>, sandbox: Sandbox) -> Self {
        BashTool {
            permission,
            ask_tx,
            sandbox,
            cache: None,
            shell_store: None,
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
            shell_store: None,
        }
    }

    /// Inject the shared background-shell registry so `background: true`
    /// commands run detached. Chainable; `None` leaves the tool
    /// synchronous-only.
    pub fn with_shell_store(
        mut self,
        shell_store: Option<crate::agent::tools::bg_shell::BackgroundShellStore>,
    ) -> Self {
        self.shell_store = shell_store;
        self
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
            description: with_contract_hint(
                "bash",
                "Execute a bash command in the current working directory. Returns stdout and stderr.",
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Bash command to execute" },
                    "timeout": { "type": "integer", "description": "Timeout in seconds (optional; default 120, or 600 when background)" },
                    "background": { "type": "boolean", "description": "Run detached and unbounded: returns immediately with a shell id (does NOT block the turn). Use for long-running commands — dev servers, watch builds, tails. Read its accumulated output with the bash_output tool (pass the id; poll it to follow progress) and stop it with kill_shell (pass the id). Output is NOT auto-delivered. If `timeout` is set, the shell is auto-killed after that many seconds; otherwise it runs until it exits or you kill it." }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: BashArgs) -> Result<String, ToolError> {
        // Strip control characters from the command string before
        // it reaches bash. The LLM can embed raw escape sequences
        // and C0 controls in tool arguments; a bare BEL or ESC in
        // a `bash -c` argument would be interpreted by the shell
        // (or passed through to child processes that write to
        // /dev/tty, bypassing our pipe capture). Keep \n (multi-
        // line scripts via `-c`) and \t (indentation).
        let command =
            crate::ui::ansi::strip_escapes(&args.command, crate::ui::ansi::StripPolicy::KEEP_BOTH);
        check::check_bash_segments(&self.permission, &self.ask_tx, &command).await?;

        // F6: spawn into its own process group so a timeout can
        // SIGKILL the entire subprocess tree, not just the
        // immediate `bash` child. Before this, `pi` would spawn
        // `npm install`, the 120s timeout fired, the future was
        // dropped (taking the tokio `Child` with it), but bash's
        // children — and theirs — kept running orphaned under PID 1.
        // pi (`bash.ts:76-81`) does this via `detached: true` +
        // `killProcessTree(pid)`.
        let background = args.background.unwrap_or(false);

        // Detached/background path (Claude-Code model): spawn UNBOUNDED,
        // register in the shell store, and return immediately with an id.
        // The model reads output with `bash_output` and stops it with
        // `kill_shell`. `timeout`, if given, becomes an auto-kill-after-N.
        // Degrades to synchronous if no shell store was injected.
        if background && let Some(store) = &self.shell_store {
            use crate::agent::tools::bg_shell::BackgroundShellStore;
            if let Some(t) = args.timeout
                && t == 0
            {
                return Err(ToolError::Msg("timeout must be > 0".to_string()));
            }
            let running = store.running_count();
            let cap = BackgroundShellStore::max_concurrent();
            if running >= cap {
                return Err(ToolError::Msg(format!(
                    "background shell cap reached ({running}/{cap} running). Stop one with kill_shell, or run inline (background=false).",
                )));
            }
            let id = uuid::Uuid::new_v4().to_string();
            store.register(id.clone(), command.clone());
            // A backgrounded command may mutate the filesystem while it
            // runs; conservatively drop the per-turn read/grep/list cache.
            if let Some(ref cache) = self.cache {
                cache.clear();
            }
            let wrapped = self.sandbox.wrap_command(&command);
            let handle = spawn_streaming_shell(wrapped, store.clone(), id.clone(), args.timeout);
            store.attach_handle(&id, handle);
            let timeout_note = match args.timeout {
                Some(t) => format!(" (auto-killed after {t}s)"),
                None => " (runs until it exits or you kill it)".to_string(),
            };
            return Ok(format!(
                "background shell started — id: {id}{timeout_note}. Read its output with bash_output (id: \"{id}\") and stop it with kill_shell (id: \"{id}\"). Output is NOT pushed to you — poll bash_output.",
            ));
        }

        // Background requested but no store wired (headless): fall back to
        // a bounded synchronous run.
        // dirge-onlr/4xgd: single source — resolved [timeouts] config.
        let secs = args
            .timeout
            .unwrap_or(crate::timeout::Timeouts::get().bash.as_secs());
        if secs == 0 {
            return Err(ToolError::Msg("timeout must be > 0".to_string()));
        }

        let output = run_with_timeout(self.sandbox.wrap_command(&command), secs).await?;

        // F12: `merged` already contains stdout + stderr in arrival
        // order. Previously we concatenated stdout then stderr,
        // mis-ordering interleaved output.
        let mut result = output.merged;
        // Cap raw bash output before it enters LLM context. The
        // streaming drain-loop above already enforces an in-memory
        // ceiling at 256 KiB (TOOL-7) so the cap below is normally
        // a no-op — kept as belt-and-braces in case the drain loop
        // ever races. 256 KiB ≈ 65k tokens worst-case, already well
        // above any sensible single-command output.
        const BASH_OUTPUT_CAP_BYTES: usize = 256 * 1024;
        result = crate::agent::tools::head_cap(result, BASH_OUTPUT_CAP_BYTES, "bash output");
        // Bash may have mutated the filesystem; conservatively invalidate the
        // per-turn read/grep/list cache.
        if let Some(ref cache) = self.cache {
            cache.clear();
        }

        // dirge-sb2n: surface files this command created / edited /
        // deleted in the MODIFIED panel. write/edit/apply_patch already
        // call `mark_modified`; bash bypassed it entirely, so heredoc
        // creates (`cat > voxel.html <<'EOF'`), `rm` deletes and `mv`
        // renames never propagated. Reuse the same path extractors the
        // permission layer runs. Only mark on success so a failed
        // command doesn't record phantom edits.
        #[cfg(feature = "semantic-bash")]
        if output.exit_code == 0 {
            check::mark_bash_mutations(self.permission.as_ref(), &command);
        }

        // Phase 3 / part 2: hand the (post-cap) buffer to the
        // disk-backed-output relay. Below the inline budget the
        // relay is a no-op and the exit-code line is appended
        // inline; above the budget we write the full output to
        // `~/.dirge/transient/<pid>/bash-<ts>.txt` and return a
        // head/tail summary plus a `read`-tool hint. No envelope:
        // bash output is local, not external content.
        let exit_note = if output.exit_code != 0 {
            format!("Exit code: {}", output.exit_code)
        } else {
            String::new()
        };
        let outcome = crate::agent::tools::output_relay::relay_if_large("bash", result, &exit_note);
        Ok(outcome.text)
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests;
