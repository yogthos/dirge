use rig::completion::ToolDefinition;
use rig::tool::Tool;

mod exec;
use exec::{run_with_timeout, spawn_streaming_shell};

use crate::agent::agent_loop::tool_input_repair::with_contract_hint;
use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, BashArgs, PermCheck, ToolError, enforce_request};

use crate::sandbox::Sandbox;
#[cfg(feature = "semantic-bash")]
use crate::semantic::adapters::bash;

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
        check_bash_segments(&self.permission, &self.ask_tx, &command).await?;

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
            mark_bash_mutations(self.permission.as_ref(), &command);
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

/// dirge-sb2n: paths a bash command mutates — output-redirect targets
/// (`> f`, `cat > f <<'EOF'`) plus the positional args of file-mutating
/// commands (`rm`/`mv`/`cp`/`touch`/…). Reuses the same tree-sitter
/// extractors the permission layer runs (`extract_redirect_targets` +
/// `extract_mutation_paths`) so there's no second parser to keep in sync.
#[cfg(feature = "semantic-bash")]
fn bash_mutation_targets(command: &str) -> Vec<String> {
    let mut targets = bash::extract_redirect_targets(command);
    targets.extend(bash::extract_mutation_paths(command));
    targets
}

/// dirge-sb2n: record each path a successful bash command touched into
/// the shared modified-files tracker so it shows up in the MODIFIED
/// panel, the same way write/edit/apply_patch do. Relative paths are
/// resolved against the agent's working dir (from the permission
/// checker) so they canonicalize to the same absolute path the other
/// tools record. `/dev/*` and `/proc/*` redirect targets are skipped —
/// they're not real edits.
#[cfg(feature = "semantic-bash")]
fn mark_bash_mutations(permission: Option<&PermCheck>, command: &str) {
    let base = permission.map(|p| {
        let g = p.lock().unwrap_or_else(|e| e.into_inner());
        std::path::PathBuf::from(g.working_dir())
    });
    for target in bash_mutation_targets(command) {
        if target.starts_with("/dev/") || target.starts_with("/proc/") {
            continue;
        }
        let p = std::path::Path::new(&target);
        let abs = match &base {
            Some(b) if p.is_relative() => b.join(p),
            _ => p.to_path_buf(),
        };
        crate::agent::tools::modified::mark_modified(&abs);
    }
}

/// dirge-7l5i: lexically resolve `.`/`..`/`.` path components without
/// touching the filesystem (so it works for not-yet-created targets).
#[cfg(feature = "semantic-bash")]
fn normalize_lexical(p: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        use std::path::Component;
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// dirge-7l5i: fold the targets of leading `cd`/`pushd` segments onto
/// `base` to get the effective cwd a later relative target is written
/// against. Best-effort, quote-trimming; conservatively applies ALL `cd`s
/// in the compound (so the effective dir is the last one).
#[cfg(feature = "semantic-bash")]
fn fold_cd_dirs(base: &str, segments: &[String]) -> String {
    let mut dir = std::path::PathBuf::from(base);
    for seg in segments {
        let mut it = seg.split_whitespace();
        let head = it.next().unwrap_or("");
        if head == "cd" || head == "pushd" {
            if let Some(target) = it.find(|a| !a.starts_with('-')) {
                let t = target.trim_matches(['"', '\'']);
                if t.is_empty() {
                    continue;
                }
                let tp = std::path::Path::new(t);
                if tp.is_absolute() {
                    dir = tp.to_path_buf();
                } else {
                    dir = normalize_lexical(&dir.join(tp));
                }
            }
        }
    }
    dir.to_string_lossy().into_owned()
}

/// dirge-7l5i: resolve a redirect/mutation target to an absolute path
/// against the (cd-adjusted) effective dir; absolute targets pass through.
#[cfg(feature = "semantic-bash")]
fn resolve_target(effective_dir: &str, target: &str) -> String {
    let p = std::path::Path::new(target);
    if p.is_absolute() {
        target.to_string()
    } else {
        normalize_lexical(&std::path::Path::new(effective_dir).join(p))
            .to_string_lossy()
            .into_owned()
    }
}

async fn check_bash_segments(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    command: &str,
) -> Result<(), ToolError> {
    // ATOMIC bash authorization (Phase 3): one bash invocation becomes
    // ONE multi-claim AccessRequest — an Execute claim per command
    // segment plus an Edit claim per redirect target / mutation path —
    // authorized as a unit so the whole command is allowed/denied/
    // prompted ONCE, not gate-by-gate. (Replaces the old per-call
    // `enforce` loop that could fire several sequential prompts.)
    //
    // Semantics preserved by the engine, not bespoke code here:
    //   - each compound segment is checked, so `git diff && rm -rf /`
    //     denies on the `rm` segment (Execute deny rule);
    //   - redirect/mutation targets route through Edit (the write rules
    //     + external-dir gate apply, closing the C4 audit gap);
    //   - `/dev/null` targets are auto-allowed by BuiltinAllow on the
    //     Edit claim — but the command itself still needs Execute
    //     permission, so an UNFAMILIAR `cmd > /dev/null` still prompts
    //     (more correct than the old blanket command soft-allow).
    let Some(perm) = permission else {
        return Ok(()); // no checker (ACP / --no-tools) → pass through
    };
    let mode = {
        let g = perm.lock().unwrap_or_else(|e| e.into_inner());
        g.mode()
    };
    use crate::permission::engine::types::{AccessRequest, Claim, Operation, Resource};
    let cmd_claim = |seg: &str| {
        Claim::new(
            Operation::Execute,
            Resource::Command {
                raw: seg.to_string(),
                head: seg.split_whitespace().next().unwrap_or("").to_string(),
            },
        )
    };
    let mut claims: Vec<Claim> = Vec::new();

    #[cfg(feature = "semantic-bash")]
    {
        let working_dir = {
            let g = perm.lock().unwrap_or_else(|e| e.into_inner());
            g.working_dir().to_string()
        };
        // /dev/null detection lives solely in `classify_path` (the Path
        // resource's `dev_null` field, consulted by BuiltinAllow) — so
        // we just split into plain segments here. The old
        // `parse_bash_segments_with_dev_null` computed a parallel
        // per-segment flag that was discarded (dirge-v0b6).
        let (segments, complex) = bash::parse_bash_segments_full(command)
            .unwrap_or_else(|_| (vec![command.to_string()], false));
        // dirge-7l5i: a leading `cd`/`pushd` changes the cwd BEFORE a later
        // RELATIVE redirect/mutation target is written. Resolve relative
        // targets against that cd'd directory, then classify the resulting
        // ABSOLUTE path against the project root. Without this,
        // `cd /etc && echo x > passwd` classified `passwd` as
        // `<project>/passwd` (in-tree → auto-allowed) while bash actually
        // wrote `/etc/passwd` — an out-of-tree write with no prompt.
        // Conservative: all cd targets fold to one effective dir, so a
        // write-then-cd ordering may over-prompt (safe direction).
        let effective_dir = fold_cd_dirs(&working_dir, &segments);
        let path_claim = |target: &str| {
            let resolved = resolve_target(&effective_dir, target);
            Claim::new(
                Operation::Edit,
                crate::permission::engine::classify_path(&resolved, &working_dir),
            )
        };
        if complex {
            // Subshell / command substitution / etc.: tree-sitter
            // declined to split — check the whole command as one
            // Execute claim so the user confirms the unfamiliar shape.
            claims.push(cmd_claim(command));
        } else {
            for segment in &segments {
                claims.push(cmd_claim(segment));
            }
        }
        // PERM-6 / C4 / F1: redirect targets AND file-mutating command
        // path args (rm/cp/mv/mkdir/touch/chmod/…) both route through
        // Edit so write deny-lists + the external-dir gate govern them.
        for target in bash::extract_redirect_targets(command) {
            claims.push(path_claim(&target));
        }
        for path in bash::extract_mutation_paths(command) {
            claims.push(path_claim(&path));
        }
    }
    #[cfg(not(feature = "semantic-bash"))]
    {
        // Coarse, quote-aware split when tree-sitter isn't compiled in;
        // command-substitution / heredoc / ANSI-C quoting are checked as
        // one whole-command claim.
        let has_substitution = command.contains("$(")
            || command.contains('`')
            || command.contains("<(")
            || command.contains(">(")
            || command.contains("$'")
            || command.contains("<<");
        if has_substitution {
            claims.push(cmd_claim(command));
        } else {
            for segment in quote_aware_split(command) {
                claims.push(cmd_claim(segment));
            }
            // dirge-9bqy: route redirect/mutation targets through Edit so
            // the write deny-lists + external-dir gate govern them, same as
            // the semantic-bash path. Relative targets classify against the
            // project root (no `cd`-folding here — that refinement is
            // semantic-only; absolute out-of-tree writes are the gap that
            // matters and are caught). Skipped on `has_substitution` since
            // a whole-command claim already forces confirmation there.
            let working_dir = {
                let g = perm.lock().unwrap_or_else(|e| e.into_inner());
                g.working_dir().to_string()
            };
            let path_claim = |target: &str| {
                Claim::new(
                    Operation::Edit,
                    crate::permission::engine::classify_path(target, &working_dir),
                )
            };
            for target in coarse_redirect_targets(command) {
                claims.push(path_claim(&target));
            }
            for path in coarse_mutation_paths(command) {
                claims.push(path_claim(&path));
            }
        }
    }

    if claims.is_empty() {
        claims.push(cmd_claim(command));
    }

    let req = AccessRequest {
        tool: "bash".to_string(),
        claims,
        mode,
        display_input: command.to_string(),
    };
    enforce_request(permission, ask_tx, req).await
}

/// Split a shell command on `;`, `&&`, `||` separators that appear
/// OUTSIDE single quotes, double quotes, or backslash escapes.
/// Used only on the no-`semantic-bash` build path — the
/// tree-sitter path delegates to the real bash grammar in
/// `semantic::adapters::bash` and doesn't need this.
///
/// Edge cases:
/// - `echo "; rm"` → one segment (the `;` is quoted).
/// - `echo 'a&&b'` → one segment.
/// - `echo \; ls` → one segment (the `;` is escaped).
/// - `cmd1; cmd2 && cmd3` → three segments, trimmed.
/// - Empty / whitespace-only segments dropped.
#[cfg_attr(feature = "semantic-bash", allow(dead_code))]
fn quote_aware_split(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_backslash = false;

    while i < bytes.len() {
        let b = bytes[i];

        if prev_backslash {
            prev_backslash = false;
            i += 1;
            continue;
        }

        if b == b'\\' && !in_single {
            // Inside single quotes, backslash is literal; otherwise it
            // escapes the next byte.
            prev_backslash = true;
            i += 1;
            continue;
        }

        if !in_double && b == b'\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && b == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }

        if !in_single && !in_double {
            // Check for `&&` and `||` (2-byte) BEFORE single-byte `;`/`|`/`&`.
            if i + 1 < bytes.len()
                && ((b == b'&' && bytes[i + 1] == b'&') || (b == b'|' && bytes[i + 1] == b'|'))
            {
                push_segment(command, start, i, &mut segments);
                i += 2;
                start = i;
                continue;
            }
            if b == b';' {
                push_segment(command, start, i, &mut segments);
                i += 1;
                start = i;
                continue;
            }
            // Pipe `|` (single-byte) — must be checked AFTER `||`
            // above. Without this, a command like `safe_cmd | rm
            // -rf /` was treated as one segment and only `safe_cmd`'s
            // permission rule applied; the destructive RHS rode in
            // unchecked. The semantic-bash tree-sitter path correctly
            // splits pipelines; this fallback didn't.
            if b == b'|' {
                push_segment(command, start, i, &mut segments);
                i += 1;
                start = i;
                continue;
            }
            // B3-6 (audit fix): background `&` (single-byte) — must
            // be checked AFTER `&&` above. Without this,
            // `safe_cmd & rm -rf /` rode through with only the LHS
            // matching a permission rule; the backgrounded LHS plus
            // unchecked RHS would both execute.
            if b == b'&' {
                push_segment(command, start, i, &mut segments);
                i += 1;
                start = i;
                continue;
            }
        }

        i += 1;
    }

    push_segment(command, start, bytes.len(), &mut segments);
    segments
}

#[cfg_attr(feature = "semantic-bash", allow(dead_code))]
fn push_segment<'a>(command: &'a str, start: usize, end: usize, out: &mut Vec<&'a str>) {
    if end <= start {
        return;
    }
    let s = command[start..end].trim();
    if !s.is_empty() {
        out.push(s);
    }
}

/// dirge-9bqy: coarse redirect-target scan for the no-`semantic-bash`
/// build. Without tree-sitter we still must not let `echo x > /etc/passwd`
/// write outside the project ungated. Walks the command outside single/
/// double quotes and, on a `>`/`>>` operator (a leading fd digit or `&`
/// has already been consumed as a normal byte), captures the next
/// whitespace-delimited token as a write target. Quote-aware so a literal
/// `>` inside a string is not treated as a redirect. Exotic forms
/// (process substitution, `{fd}>`) never reach here — the caller routes
/// `$(`/`` ` ``/`<(`/`>(`/`$'`/`<<` to a whole-command claim first.
#[cfg(not(feature = "semantic-bash"))]
fn coarse_redirect_targets(command: &str) -> Vec<String> {
    let bytes = command.as_bytes();
    let mut targets = Vec::new();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\\' => i += 2, // skip the escaped byte
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'>' => {
                i += 1;
                if i < bytes.len() && bytes[i] == b'>' {
                    i += 1; // append `>>`
                }
                if i < bytes.len() && bytes[i] == b'|' {
                    i += 1; // clobber `>|`
                }
                while i < bytes.len() && (bytes[i] as char).is_whitespace() {
                    i += 1;
                }
                let start = i;
                while i < bytes.len() {
                    let t = bytes[i];
                    if (t as char).is_whitespace()
                        || matches!(t, b';' | b'|' | b'&' | b'>' | b'<' | b'(' | b')')
                    {
                        break;
                    }
                    i += 1;
                }
                if i > start {
                    let tok = command[start..i].trim_matches(['"', '\'']);
                    if !tok.is_empty() {
                        targets.push(tok.to_string());
                    }
                }
            }
            _ => i += 1,
        }
    }
    targets
}

/// Known file-mutating commands whose path operands must route through
/// an Edit claim on the no-`semantic-bash` build.
#[cfg(not(feature = "semantic-bash"))]
const COARSE_MUTATORS: &[&str] = &[
    "rm", "cp", "mv", "mkdir", "rmdir", "touch", "chmod", "chown", "ln", "dd", "truncate", "tee",
    "install", "shred",
];

/// dirge-9bqy: coarse mutation-path scan for the no-`semantic-bash`
/// build. For each split segment whose command head is a known mutator,
/// treat non-flag operands as write targets so the write rules + external-
/// dir gate apply (matching the semantic path's `extract_mutation_paths`).
/// Conservative: mode/owner operands (`chmod 755 …`) classify in-cwd and
/// are harmless; `dd` only contributes its `of=` operand.
#[cfg(not(feature = "semantic-bash"))]
fn coarse_mutation_paths(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    for segment in quote_aware_split(command) {
        let mut toks = segment.split_whitespace();
        let Some(head) = toks.next() else { continue };
        let base = head.rsplit('/').next().unwrap_or(head);
        if !COARSE_MUTATORS.contains(&base) {
            continue;
        }
        for t in toks {
            if t.starts_with('-') {
                continue; // flag
            }
            if base == "dd" {
                if let Some(rest) = t.strip_prefix("of=") {
                    if !rest.is_empty() {
                        out.push(rest.to_string());
                    }
                }
                continue; // dd uses key=value operands only
            }
            out.push(t.to_string());
        }
    }
    out
}

#[cfg(test)]
#[cfg(unix)]
mod tests;
