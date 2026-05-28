use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::permission::allowlist;
use crate::permission::engine;
use crate::permission::path;
use crate::permission::pattern::Pattern;
use crate::permission::{Action, PermissionConfig, SecurityMode, ToolPerm};

pub type PermCheck = Arc<Mutex<PermissionChecker>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    Allowed,
    Ask,
    Denied(String),
}

pub struct PermissionChecker {
    rules: HashMap<String, Vec<(Pattern, Action)>>,
    default_action: Action,
    ext_dir_rules: Vec<(Pattern, Action)>,
    doom_loop_action: Action,
    working_dir: String,
    /// Cached canonical form of `working_dir`, computed once at
    /// construction (and refreshed by `set_working_dir`). Used by
    /// `is_external_path` to compare canonical paths without
    /// hitting the filesystem on every permission check — the
    /// canonicalize syscall is otherwise called once per
    /// read/write/edit/grep call, accumulating to hundreds of
    /// stat()s per session.
    working_dir_canonical: String,
    /// The currently-installed CWD-scoped allow-glob (e.g.
    /// `/Users/foo/proj/**`) used by `install_cwd_allow_rules` and
    /// `set_working_dir`. Recorded so that on cd we can find and
    /// remove the stale entries from `rules` before installing
    /// fresh ones, without touching user-configured rules pushed
    /// onto the same Vec. `None` when no CWD-allow was installable
    /// (degenerate working_dir, e.g. empty or `/`).
    cwd_allow_pattern: Option<String>,
    session_allowlist: Vec<(String, Pattern)>,
    recent_calls: VecDeque<(String, String)>,
    /// PERM-1: per-key repeat counter. Tracks how many times each
    /// (tool, input) pair has been seen. Uses a HashMap keyed by
    /// "{tool}\x00{input}" so the lookup is O(1) instead of scanning
    /// the FIFO window. Counts persist until evicted by the FIFO
    /// ring (window 32) — a 14-call decoy-gap attack can't flush a
    /// specific key because the ring is 2× the old window.
    repeat_counts: HashMap<String, u32>,
    mode: SecurityMode,
    /// Tools denied by the currently-active prompt's frontmatter
    /// `deny_tools` list. Enforced at the top of every `check` /
    /// `check_path` call — even before Yolo mode's blanket allow.
    /// This is the permission-layer enforcement of plan/review/etc.
    /// modes; previously plan mode relied on prose ("don't write
    /// code") + inline `is_plan_file` gates in edit/write/apply_patch,
    /// which an adversarial / confused LLM could route around via
    /// `bash` or by bypassing the gate name-check.
    ///
    /// Updated by `set_prompt_deny_tools` whenever the active prompt
    /// changes (slash `/prompt <name>`, session load, startup). Empty
    /// when no prompt is active or the active prompt has no
    /// frontmatter.
    prompt_deny_tools: Vec<String>,
}

/// Tools that execute external code with broad effects. Accept mode
/// does NOT coerce `Ask → Allow` for these — the "I trust the agent
/// inside cwd" rationale that justifies the coercion for other
/// non-path tools doesn't generalize to shell + MCP servers.
fn is_high_risk_non_path_tool(tool: &str) -> bool {
    engine::is_high_risk_non_path_tool(tool)
}

/// Tool names where the input is a filesystem path. For these, `*` keeps
/// classic glob semantics (one segment, doesn't cross `/`). Everything else
/// is treated as shell/text where `*` means "any chars including /".
pub(crate) fn is_path_tool_name(tool: &str) -> bool {
    engine::is_path_tool_name(tool)
}

/// Build a Pattern with the right `*` semantics for the given tool.
pub(crate) fn pattern_for_tool(tool: &str, pat: &str) -> Pattern {
    engine::pattern_for_tool(tool, pat)
}

impl PermissionChecker {
    pub fn new(
        config: &PermissionConfig,
        mode: SecurityMode,
        working_dir: Option<std::path::PathBuf>,
    ) -> Self {
        // M4 (dirge-ojn): default flipped Allow → Ask. Unconfigured
        // tools now prompt the user instead of silently executing.
        // Read-only tools that should NOT prompt get explicit Allow
        // rules installed below (see `install_default_allow_rules`).
        //
        // Why: dirge previously defaulted every unmatched tool to
        // Allow — e.g. `write` had no rules installed, so write to
        // any cwd path executed silently. Combined with the bash
        // redirect-target bug closed in M3 (fbcc09b), the practical
        // posture was "anything runs unless an explicit rule says no",
        // the opposite of what users expect from a coding agent.
        //
        // Mirrors maki's posture (`maki-agent/src/permissions.rs:199`:
        // bash, write, edit, MCP all default to Ask; an explicit
        // BUILTIN_ALLOW_RULES list opens specific safe tools) and
        // opencode's (`evaluate.ts:14`: `return match ?? { action:
        // "ask" }` — Ask is the universal fallback).
        let default_action = config.default.unwrap_or(Action::Ask);
        let doom_loop_action = config.doom_loop.unwrap_or(Action::Ask);

        // Resolve `working_dir` UP-FRONT so the CWD-scoped builtin
        // allow rules installed below can embed it in their
        // patterns. The actual struct field is populated from this
        // same value at the bottom of `new`.
        let working_dir = working_dir
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
            .to_string_lossy()
            .to_string();

        let mut rules: HashMap<String, Vec<(Pattern, Action)>> = HashMap::new();

        // M4 (dirge-ojn): install the builtin-allow list FIRST so user
        // rules added later (last-match-wins per check_path's
        // `matched.last()`) can override specific patterns while the
        // tool's overall posture stays Allow-by-default for safety.
        //
        // Example: user writes `read: { "/etc/**": "deny" }`. With the
        // builtin already installed as `read: { "**": allow }`, the
        // user's specific deny appends to the same Vec. On lookup the
        // last matching pattern wins:
        //   - `/etc/passwd` → both rules match → user's deny wins ✓
        //   - `/tmp/safe.txt` → only `**` matches → builtin allow ✓
        //
        // Tools NOT in this list (write/edit/apply_patch/bash/webfetch/
        // websearch/task/skill/memory) fall to the global default Ask
        // unless the user installs explicit rules.
        //
        // Adapts maki's `BUILTIN_ALLOW_RULES`
        // (`maki-agent/src/permissions.rs:16-24`) for dirge's tool set.
        // Maki includes write/edit/multiedit in its allow list — a
        // different posture choice that doesn't suit dirge given the
        // audit history (C1/C8/etc.).
        for tool in [
            "read",
            "glob",
            "grep",
            "find_files",
            "list_dir",
            "list_symbols",
            "find_definition",
            "find_callers",
            "find_callees",
            "get_symbol_body",
            "repo_overview",
            "lsp",
            "write_todo_list", // Internal-only TODO tracking; no side effects
            "task_status",     // Read-only status query for background tasks
            "question",        // Interactive by definition; gating it just adds friction
            // dirge-sm9w: memory writes are scoped to `~/.dirge/memories/`
            // (no arbitrary filesystem access) and the tool can only
            // add/edit/delete its own entries. The per-action prompt
            // is friction without security value in Standard/Accept
            // modes. Restrictive mode still demotes this back to Ask
            // in the mode switch below — its contract is "every
            // action confirms".
            "memory",
        ] {
            rules
                .entry(tool.to_string())
                .or_default()
                .push((pattern_for_tool(tool, "**"), Action::Allow));
        }

        // CWD-scoped builtin-allow for mutating filesystem tools.
        // Helper handles canonicalization + safety guards; see
        // `install_cwd_allow_rules` for the contract.
        let cwd_allow_pattern = install_cwd_allow_rules(&mut rules, &working_dir);

        // /dev/null is a harmless bit-bucket — writes silently
        // discard data, reads return immediate EOF. It must be
        // allowed for ALL tools without prompting, regardless of
        // security mode. Without this, every `> /dev/null` bash
        // redirect and every `write /dev/null` call triggers an
        // unnecessary permission dialog.
        install_dev_null_allow(&mut rules);

        // Helper: append a `ToolPerm` (Simple or Granular) onto a
        // tool's rule vec. Used by both the legacy per-tool fields and
        // the M2 `tools` map. The legacy fields are syntactic sugar
        // for `tools.{name}` — same code path.
        fn append_tool_perm(
            rules: &mut HashMap<String, Vec<(Pattern, Action)>>,
            tool_name: &str,
            tp: &ToolPerm,
        ) {
            let entries = rules.entry(tool_name.to_string()).or_default();
            match tp {
                ToolPerm::Simple(action) => {
                    entries.push((pattern_for_tool(tool_name, "*"), *action));
                }
                ToolPerm::Granular(map) => {
                    for (pat, action) in map {
                        entries.push((pattern_for_tool(tool_name, pat), *action));
                    }
                }
            }
        }

        // Track which tools the user explicitly configured (legacy
        // OR via `tools` map) so the bash / MCP default-installers
        // below can decide whether to skip themselves.
        let mut user_configured: std::collections::HashSet<&str> = std::collections::HashSet::new();

        for (tool_name, tool_perm) in [
            ("bash", &config.bash),
            ("read", &config.read),
            ("write", &config.write),
            ("edit", &config.edit),
            ("grep", &config.grep),
            ("find_files", &config.find_files),
            ("list_dir", &config.list_dir),
            // Adversarial-review #5 added; both are read-only walkers.
            ("glob", &config.glob),
            ("repo_overview", &config.repo_overview),
            ("write_todo_list", &config.write_todo_list),
            ("apply_patch", &config.apply_patch),
            ("lsp", &config.lsp),
            ("question", &config.question),
            // Newly-configurable tools (previously the perm checker
            // had no rules for them, so they always fell through to
            // the `*` default and couldn't be individually gated).
            ("webfetch", &config.webfetch),
            ("websearch", &config.websearch),
            ("task", &config.task),
            ("task_status", &config.task_status),
            ("memory", &config.memory),
            ("skill", &config.skill),
            ("list_symbols", &config.list_symbols),
            ("get_symbol_body", &config.get_symbol_body),
            ("find_definition", &config.find_definition),
            ("find_callers", &config.find_callers),
            ("find_callees", &config.find_callees),
            ("mcp_tool", &config.mcp_tool),
        ] {
            if let Some(tp) = tool_perm {
                append_tool_perm(&mut rules, tool_name, tp);
                user_configured.insert(tool_name);
            }
        }

        // M2 (dirge-cep): merge the unified `tools` map. New configs
        // declare rules for ANY tool name (including plugin / MCP /
        // future tools) without extending `PermissionConfig`. Same
        // append semantics as the legacy fields: tools-map rules are
        // pushed after legacy rules so last-match-wins.
        if let Some(tools_map) = &config.tools {
            for (tool_name, tp) in tools_map {
                append_tool_perm(&mut rules, tool_name, tp);
                // Static lifetime needed for HashSet entry —
                // restrict to the known tool name set; unknown tool
                // names (plugin/MCP) don't gate the bash/MCP
                // defaults below anyway.
                if matches!(tool_name.as_str(), "bash" | "mcp_tool") {
                    user_configured.insert(match tool_name.as_str() {
                        "bash" => "bash",
                        "mcp_tool" => "mcp_tool",
                        _ => unreachable!(),
                    });
                }
            }
        }

        // Bash defaults: only install if the user didn't supply ANY
        // bash rules (legacy or `tools` map). Bash's defaults are
        // specific allow + deny patterns that don't compose well
        // with arbitrary user rules — a `cargo *: deny` from the
        // user shouldn't have to co-exist with the default
        // `cargo build: allow`.
        if !user_configured.contains("bash") {
            let mut defaults = Vec::new();
            for (pat, action) in crate::permission::default_bash_rules() {
                defaults.push((pattern_for_tool("bash", pat), action));
            }
            // Replace any builtin-allow entry (bash isn't in the
            // builtin-allow list anyway, but be explicit).
            rules.insert("bash".to_string(), defaults);
        }

        // MCP tools execute external code (the MCP server's
        // implementation, plus whatever effects the server has on
        // the filesystem / network / API services). The previous
        // default was the inherited `default_action` (Allow) since
        // `mcp_tool` had no rule installed; that let an entire
        // sequence of MCP calls execute silently, with only the
        // doom-loop detector eventually prompting on the 3rd
        // identical call. User reported running through several
        // MCP queries without ever being asked. Install a default
        // `Ask` rule when no explicit config exists. Users who
        // trust a specific MCP server can pin it with config:
        //
        //   "permission": {
        //     "mcp_tool": {
        //       "mcp_tool:lattice:*": "allow"
        //     }
        //   }
        //
        // …or accept once and pick "allow always" for the same
        // effect via the session allowlist.
        if !user_configured.contains("mcp_tool") {
            rules.insert(
                "mcp_tool".to_string(),
                vec![(pattern_for_tool("mcp_tool", "*"), Action::Ask)],
            );
        }

        // External-directory rules are always path patterns by definition.
        let ext_dir_rules = config
            .external_directory
            .as_ref()
            .map(|map| {
                map.iter()
                    .map(|(pat, action)| (Pattern::new(pat), *action))
                    .collect()
            })
            .unwrap_or_default();

        // `working_dir` was already resolved earlier in this fn (used
        // by the CWD-scoped builtin allow installer above).
        let working_dir_canonical = canonicalize_for_cache(&working_dir);

        PermissionChecker {
            rules,
            default_action,
            ext_dir_rules,
            doom_loop_action,
            working_dir,
            working_dir_canonical,
            cwd_allow_pattern,
            session_allowlist: Vec::new(),
            recent_calls: VecDeque::with_capacity(32),
            repeat_counts: HashMap::new(),
            mode,
            prompt_deny_tools: Vec::new(),
        }
    }

    /// Install the current prompt's deny-list. Called when the
    /// active prompt changes (startup, session load, `/prompt
    /// <name>`); pass an empty vec to clear.
    pub fn set_prompt_deny_tools(&mut self, denied: Vec<String>) {
        self.prompt_deny_tools = denied;
    }

    /// Returns true when `tool` is in the active prompt's
    /// `deny_tools` frontmatter list. Internal helper so both
    /// `check` and `check_path` share the same gate. Case-insensitive
    /// match (#7 fix): `deny_tools: [Edit]` correctly denies `edit`.
    fn is_prompt_denied(&self, tool: &str) -> bool {
        self.prompt_deny_tools
            .iter()
            .any(|t| t.eq_ignore_ascii_case(tool))
    }

    /// Public deny-list probe, used by code paths that route through
    /// `check_perm` with a UMBRELLA tool name (e.g. MCP tools always
    /// pass `"mcp_tool"`) and need to additionally check the
    /// CONCRETE name the LLM would think of (e.g. an MCP-exported
    /// `edit` should be blocked if the active prompt denies `edit`).
    /// Returns true if ANY of the supplied names hits the deny-list.
    pub fn any_prompt_denied(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.is_prompt_denied(n))
    }

    /// dirge-mzs4: like [`Self::check`] for the `bash` tool, but
    /// upgrades a final `Ask` outcome to `Allowed` when the caller
    /// has established that the segment's ONLY filesystem-touching
    /// effect is a `/dev/null` redirect. Writing to `/dev/null`
    /// discards data with no observable side effect, so there's no
    /// reason to prompt for that subset of commands.
    ///
    /// Deny rules still fire (the default `rm -rf /**` deny will
    /// reject `rm -rf / > /dev/null`), as does the doom-loop tracker;
    /// the only behavioural difference is the post-step that converts
    /// `Ask → Allowed`. Mode coercions, prompt-level deny lists, and
    /// the session allowlist all run through unchanged.
    pub fn check_bash_dev_null_softallow(&mut self, input: &str) -> CheckResult {
        match self.check("bash", input) {
            CheckResult::Ask => CheckResult::Allowed,
            other => other,
        }
    }

    pub fn check(&mut self, tool: &str, input: &str) -> CheckResult {
        // Prompt-level deny list runs BEFORE every other gate,
        // including Yolo mode's blanket allow. This is the
        // permission-layer enforcement of plan/review/etc. modes:
        // the prompt's frontmatter declares which tools that mode
        // CANNOT use (e.g. plan mode denies edit/write/apply_patch/
        // bash), and the LLM gets a hard refusal instead of relying
        // on the prompt prose to dissuade it from calling. Yolo is
        // still "no rule-set, all calls allowed" but a prompt's
        // deny-list is a stronger contract — the user opted into
        // this mode, so we honor it even under Yolo.
        if self.is_prompt_denied(tool) {
            return CheckResult::Denied(format!(
                "Tool {tool:?} is denied by the active prompt's `deny_tools` frontmatter. Switch with `/prompt <other>` to use it."
            ));
        }
        // PERM-7: MCP tools route through `check_perm("mcp_tool", input)`
        // with input shaped `mcp_tool:<server>:<name>`. The umbrella
        // tool name `"mcp_tool"` won't match a deny-list entry like
        // `edit` even if the MCP server exports an `edit` tool. Probe
        // the concrete tool name (the part after the second `:`) so
        // a prompt's `deny_tools: [edit]` denies an MCP-exported
        // `edit` too. Centralized here so any caller (not just the
        // McpTool wrapper) gets the defense; the wrapper's explicit
        // `any_prompt_denied` probe becomes redundant but harmless.
        if tool == "mcp_tool"
            && let Some(rest) = input.strip_prefix("mcp_tool:")
            && let Some((_server, concrete)) = rest.split_once(':')
            && self.is_prompt_denied(concrete)
        {
            return CheckResult::Denied(format!(
                "MCP tool {concrete:?} is denied by the active prompt's `deny_tools` frontmatter. Switch with `/prompt <other>` to use it."
            ));
        }
        if self.mode == SecurityMode::Yolo {
            return CheckResult::Allowed;
        }

        if self.is_session_allowed(tool, input) {
            return CheckResult::Allowed;
        }

        // Track both the action AND the matching pattern so denial
        // messages can name which rule blocked the call (was just
        // "Blocked by permission rules", giving the user no way to
        // identify and edit the offending rule).
        let mut matched: Vec<(Action, String)> = Vec::new();
        if let Some(rules) = self.rules.get(tool) {
            for (pattern, action) in rules {
                if pattern.matches(input) {
                    matched.push((*action, pattern.original.clone()));
                }
            }
        }

        let base = matched
            .last()
            .map(|(a, _)| *a)
            .unwrap_or(self.default_action);
        let last_pat = matched.last().map(|(_, p)| p.clone());
        let action = match self.mode {
            SecurityMode::Restrictive => {
                // dirge-sm9w: memory has a builtin `Allow` rule for
                // Standard/Accept. Restrictive's contract is "every
                // action confirms", so demote any non-Deny outcome
                // back to Ask. An explicit user `deny` still denies.
                if tool == "memory" && base != Action::Deny {
                    Action::Ask
                } else if matched.is_empty() && self.default_action == Action::Allow {
                    Action::Ask
                } else {
                    base
                }
            }
            SecurityMode::Accept => match base {
                Action::Ask => {
                    if self.is_path_tool(tool) && self.is_external_path(input) {
                        self.match_ext_dir(input).unwrap_or(Action::Ask)
                    } else if is_high_risk_non_path_tool(tool) {
                        // Accept mode coerces Ask → Allow for non-path
                        // tools on the assumption that "trust the
                        // agent inside cwd" generalizes. That breaks
                        // for tools that execute external code with
                        // arbitrary effects: MCP servers run third-
                        // party code; `bash` runs shell. Keep the Ask
                        // for these specifically. Review #1.
                        Action::Ask
                    } else {
                        Action::Allow
                    }
                }
                other => other,
            },
            SecurityMode::Standard => base,
            SecurityMode::Yolo => unreachable!(),
        };

        if action != Action::Deny {
            // PERM-2: check doom-loop BEFORE tracking the current
            // call. The counter reflects previous identical calls
            // only — the current call doesn't count itself.
            if self.is_doom_loop(tool, input) {
                match self.doom_loop_action {
                    Action::Deny => {
                        let preview: String = input.chars().take(60).collect();
                        return CheckResult::Denied(format!(
                            "Doom loop: repeated identical {} call ({}{})",
                            tool,
                            preview,
                            if input.chars().count() > 60 {
                                "…"
                            } else {
                                ""
                            },
                        ));
                    }
                    Action::Ask => return CheckResult::Ask,
                    Action::Allow => {}
                }
            }
            self.track_doom_loop(tool, input);
        }

        match action {
            Action::Allow => CheckResult::Allowed,
            Action::Ask => CheckResult::Ask,
            Action::Deny => CheckResult::Denied(match last_pat {
                Some(pat) => format!("Blocked by rule: {tool} {pat:?} → deny"),
                None => format!("Blocked: {tool} denied by default action"),
            }),
        }
    }

    pub fn check_path(&mut self, tool: &str, path: &str) -> CheckResult {
        // Reject paths that are clearly LLM hallucinations
        // (e.g. "1", "a", "xy") before they trigger permission
        // dialogs for non-existent files.  Absolute paths and
        // relative paths with directory components or file
        // extensions pass through to the normal check.
        if let Err(reason) = path::validate_path(path) {
            return CheckResult::Denied(reason);
        }

        // Prompt deny-list runs first, same reasoning as `check`.
        if self.is_prompt_denied(tool) {
            return CheckResult::Denied(format!(
                "Tool {tool:?} is denied by the active prompt's `deny_tools` frontmatter. Switch with `/prompt <other>` to use it."
            ));
        }
        if self.mode == SecurityMode::Yolo {
            return CheckResult::Allowed;
        }

        // Resolve BEFORE the allowlist check so we can test both the
        // raw path and the absolute form. Without this, a user who
        // granted AllowAlways for a relative path (e.g. src/main.rs)
        // gets re-prompted when the LLM sends an absolute path for
        // the same file.
        let abs_path = resolve_absolute(path, &self.working_dir);

        if self.is_session_allowed(tool, path) || self.is_session_allowed(tool, &abs_path) {
            return CheckResult::Allowed;
        }

        let mut matched: Vec<(Action, String)> = Vec::new();
        if let Some(rules) = self.rules.get(tool) {
            for (pattern, action) in rules {
                if pattern.matches(&abs_path) || pattern.matches(path) {
                    matched.push((*action, pattern.original.clone()));
                }
            }
        }

        let base = matched
            .last()
            .map(|(a, _)| *a)
            .unwrap_or(self.default_action);
        let last_pat = matched.last().map(|(_, p)| p.clone());

        // Audit H9: `external_directory` rules used to fire only in
        // `SecurityMode::Accept`. A user who configured
        // `external_directory = { "/external/safe/**" = "allow" }`
        // saw the rule silently ignored under Standard/Restrictive.
        // Pre-compute the overlay so each mode can opt into it
        // uniformly.
        let is_external = self.is_external_path(&abs_path);
        let ext_dir_action = if is_external {
            self.match_ext_dir(&abs_path)
        } else {
            None
        };

        let action = match self.mode {
            SecurityMode::Restrictive => {
                if let Some(a) = ext_dir_action {
                    a
                } else if matched.is_empty() && self.default_action == Action::Allow {
                    Action::Ask
                } else {
                    base
                }
            }
            SecurityMode::Accept => match base {
                Action::Ask => {
                    if is_external {
                        ext_dir_action.unwrap_or(Action::Ask)
                    } else {
                        Action::Allow
                    }
                }
                other => other,
            },
            SecurityMode::Standard => {
                // Explicit ext_dir rule overrides the base for external
                // paths. For non-external paths (or external paths
                // without a matching ext_dir rule) keep the prior
                // base-action behavior — the catch-all below will
                // demote unmatched external Allows to Ask.
                if let Some(a) = ext_dir_action {
                    a
                } else {
                    base
                }
            }
            SecurityMode::Yolo => unreachable!(),
        };

        let action = if matched.is_empty()
            && action == Action::Allow
            && is_external
            && ext_dir_action.is_none()
        {
            Action::Ask
        } else {
            action
        };

        if action != Action::Deny {
            self.track_doom_loop(tool, path);
            if self.is_doom_loop(tool, path) {
                match self.doom_loop_action {
                    Action::Deny => {
                        let preview: String = path.chars().take(80).collect();
                        return CheckResult::Denied(format!(
                            "Doom loop: repeated identical {} call ({}{})",
                            tool,
                            preview,
                            if path.chars().count() > 80 { "…" } else { "" },
                        ));
                    }
                    Action::Ask => return CheckResult::Ask,
                    Action::Allow => {}
                }
            }
        }

        match action {
            Action::Allow => CheckResult::Allowed,
            Action::Ask => CheckResult::Ask,
            Action::Deny => CheckResult::Denied(match last_pat {
                Some(pat) => format!("Blocked by rule: {tool} {pat:?} → deny"),
                None => format!("Blocked: {tool} denied by default action"),
            }),
        }
    }

    fn is_session_allowed(&self, tool: &str, input: &str) -> bool {
        allowlist::is_allowed(&self.session_allowlist, tool, input)
    }

    pub fn add_session_allowlist(&mut self, tool: String, pattern_str: &str) {
        allowlist::add(&mut self.session_allowlist, &tool, pattern_str);
        // F2 write↔edit↔apply_patch aliasing: when the user "always
        // allows" any of these three, also register the pattern under
        // the other two so the alias check in enforce() doesn't
        // re-prompt. Without this, a user who "always allows" write
        // gets asked again on the next write because the edit-alias
        // check returns Ask with no allowlist match.
        match tool.as_str() {
            "write" | "apply_patch" => {
                allowlist::add(&mut self.session_allowlist, "edit", pattern_str);
            }
            "edit" => {
                allowlist::add(&mut self.session_allowlist, "write", pattern_str);
                allowlist::add(&mut self.session_allowlist, "apply_patch", pattern_str);
            }
            _ => {}
        }
    }

    pub fn load_session_allowlist(&mut self, entries: &[(String, String)]) {
        // Route through add_session_allowlist (not allowlist::add
        // directly) so the write↔edit alias mirroring fires for
        // persisted sessions too.
        for (tool, pat) in entries {
            self.add_session_allowlist(tool.clone(), pat);
        }
    }

    pub fn allowlist_entries(&self) -> Vec<(String, String)> {
        allowlist::entries(&self.session_allowlist)
    }

    /// Remove the allowlist entry at the given index (0-based,
    /// matching the display order in `/allow list`). Returns the
    /// removed `(tool, pattern)` on success, or `None` if the
    /// index is out of range. Used by `/allow remove <n>`.
    pub fn remove_session_allowlist_at(&mut self, idx: usize) -> Option<(String, String)> {
        allowlist::remove_at(&mut self.session_allowlist, idx)
    }

    /// Remove ALL allowlist entries. Used by `/allow clear`.
    pub fn clear_session_allowlist(&mut self) {
        allowlist::clear(&mut self.session_allowlist);
    }

    pub fn set_mode(&mut self, mode: SecurityMode) {
        self.mode = mode;
    }

    /// Resolve a possibly-relative, possibly-symlinked path to its
    /// canonical form using the checker's own working_dir.
    /// Exposes `resolve_absolute` to callers that need the same
    /// canonical path the check ran against (audit H12 — pass this
    /// to `File::open` instead of the raw `args.path` to close the
    /// symlink-swap TOCTOU between check and open).
    pub fn resolve_path_for_tool(&self, path: &str) -> String {
        resolve_absolute(path, &self.working_dir)
    }

    /// Count of explicit `Deny` rules across all tools + the
    /// external-directory ruleset. Used by the host to warn the user
    /// when Yolo mode is active alongside non-empty deny rules —
    /// Yolo unconditionally returns `Allowed` before any rule
    /// lookup, so those deny rules are silently inert (audit H11).
    pub fn deny_rule_count(&self) -> usize {
        let in_tool_rules: usize = self
            .rules
            .values()
            .map(|v| v.iter().filter(|(_, a)| *a == Action::Deny).count())
            .sum();
        let in_ext_dir = self
            .ext_dir_rules
            .iter()
            .filter(|(_, a)| *a == Action::Deny)
            .count();
        in_tool_rules + in_ext_dir
    }

    pub fn mode(&self) -> SecurityMode {
        self.mode
    }

    pub fn set_working_dir(&mut self, dir: &str) {
        self.working_dir = dir.to_string();
        self.working_dir_canonical = canonicalize_for_cache(dir);
        // Refresh the CWD-scoped builtin-allow rules so the new
        // project gets its own auto-allow and the OLD pattern
        // doesn't keep matching after cd. Surgically removes only
        // the previously-installed pattern (identified by
        // `pattern.original`) so user-configured rules pushed onto
        // the same Vec stay intact.
        if let Some(old_pat) = self.cwd_allow_pattern.take() {
            for tool in ["write", "edit", "apply_patch"] {
                if let Some(entries) = self.rules.get_mut(tool) {
                    entries.retain(|(p, _)| p.original != old_pat);
                }
            }
        }
        self.cwd_allow_pattern = install_cwd_allow_rules(&mut self.rules, dir);
        // B3-5 (audit fix): clear session-scoped state that was
        // implicitly tied to the OLD cwd. Two concerns:
        //   1. `recent_calls` is the doom-loop counter — stale
        //      entries from before the cd would falsely trip the
        //      3-identical-calls limiter on the first calls in
        //      the new project.
        //   2. `session_allowlist` holds patterns the user
        //      approved for the prior project (e.g. `cd *`,
        //      `cargo *`). Carrying them silently to a new
        //      project means the user has implicitly granted
        //      those permissions there too — a privilege carry-
        //      over the audit flagged. Pi rebuilds the session
        //      on cwd change.
        self.recent_calls.clear();
        self.repeat_counts.clear();
        self.session_allowlist.clear();
    }

    fn is_path_tool(&self, tool: &str) -> bool {
        // Must match `is_path_tool_name` — these are the tools that
        // take a filesystem path as their permission input and need
        // `external_directory` rule consultation. `apply_patch` and
        // `lsp` are included because both route filesystem-path
        // strings through `check_perm_path`.
        is_path_tool_name(tool)
    }

    pub fn is_external_path(&self, path_str: &str) -> bool {
        // F18: previously `!is_absolute → return false`, which
        // treated `../../etc/passwd` as "internal" (not external).
        // In Accept mode that bypassed external_directory rules:
        // a relative `../../secret` would auto-allow because it
        // wasn't classified external. Now we resolve relative
        // paths against the working_dir (same logic as
        // `resolve_absolute`) before the starts_with check.
        let resolved = resolve_absolute(path_str, &self.working_dir);
        let p = Path::new(&resolved);
        if !p.is_absolute() {
            // resolve_absolute fell back to lexical join and the
            // result is still relative — usually means working_dir
            // itself is bogus. Treat as not-external; rules will
            // fall through to the default action.
            return false;
        }
        let cwd = Path::new(&self.working_dir);
        // PERM-3: re-canonicalize at check time so a symlink
        // rewrite (or `working_dir_canonical` going stale for
        // any other reason) doesn't misclassify in-tree paths
        // as external (or vice versa). The cached
        // `working_dir_canonical` is kept as a fallback for
        // when the on-disk cwd has been removed/replaced.
        let fresh_canonical = canonicalize_for_cache(&self.working_dir);
        // Comparing against the fresh canonical, the cached
        // canonical, AND the literal form handles symlinked
        // roots like macOS's `/tmp → /private/tmp`: `resolved`
        // is canonical (`/private/tmp/...`) but `cwd` may still
        // be the literal `/tmp` form. Without all three checks
        // every in-tree access in such a setup would classify
        // as external.
        let canonical_cwd_cached = Path::new(&self.working_dir_canonical);
        let canonical_cwd_fresh = Path::new(&fresh_canonical);
        !p.starts_with(canonical_cwd_fresh)
            && !p.starts_with(canonical_cwd_cached)
            && !p.starts_with(cwd)
    }

    fn match_ext_dir(&self, path_str: &str) -> Option<Action> {
        for (pattern, action) in &self.ext_dir_rules {
            if pattern.matches(path_str) {
                return Some(*action);
            }
        }
        None
    }

    fn track_doom_loop(&mut self, tool: &str, input: &str) {
        let key = format!("{}\x00{}", tool, input);
        let count = self.repeat_counts.entry(key).or_insert(0);
        *count = count.saturating_add(1);
        // Maintain a FIFO ring for TTL-based eviction.
        // PERM-1: window 32 (was 16) so a 14-call decoy gap
        // can't flush a specific key before it repeats.
        self.recent_calls
            .push_back((tool.to_string(), input.to_string()));
        if self.recent_calls.len() > 32
            && let Some((t, i)) = self.recent_calls.pop_front()
        {
            let old_key = format!("{}\x00{}", t, i);
            if let Some(c) = self.repeat_counts.get_mut(&old_key) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    self.repeat_counts.remove(&old_key);
                }
            }
        }
    }

    fn is_doom_loop(&self, tool: &str, input: &str) -> bool {
        let key = format!("{}\x00{}", tool, input);
        // PERM-2: threshold is 2 (blocks on the 3rd identical call).
        // `track_doom_loop` fires AFTER this check, so the counter
        // reflects previous calls only — not the current one.
        self.repeat_counts.get(&key).copied().unwrap_or(0) >= 2
    }
}

/// One-shot canonicalize for the working-directory cache. Best
/// effort: if canonicalize fails (cwd doesn't exist on disk, e.g.
/// in tests that pass a fixture path), fall back to the literal
/// string so the `starts_with` comparisons in `is_external_path`
/// still work for the literal form.
fn canonicalize_for_cache(working_dir: &str) -> String {
    path::canonicalize_for_cache(working_dir)
}

/// Install the CWD-scoped builtin-allow rule on `rules` for the
/// mutating filesystem tools (write/edit/apply_patch). Returns the
/// pattern string installed (`Some`) so `set_working_dir` can find
/// and remove it on cd; `None` when the working_dir is too
/// degenerate to install safely.
///
/// Refuses to install when:
///   - `working_dir` is empty (config-only init w/o cwd resolution).
///   - The canonical form is `/` or shorter than 2 chars — the
///     resulting pattern (`/**`) would silently allow writes anywhere
///     on the filesystem, defeating the "permissive only inside the
///     project" intent.
///   - `working_dir` contains glob metacharacters (`*`, `?`, `[`,
///     `{`). Such characters would be re-interpreted by the glob
///     compiler rather than matched literally; a user starting dirge
///     from `/tmp/[odd]` would get a character-class pattern matching
///     unintended paths.
///
/// Uses `canonicalize_for_cache` so the pattern matches the canonical
/// form `resolve_absolute` produces. Without this, macOS users whose
/// `/var` / `/tmp` resolve to `/private/var` / `/private/tmp` would
/// see the rule silently fail to match for any abs_path the checker
/// computed.
fn install_cwd_allow_rules(
    rules: &mut HashMap<String, Vec<(Pattern, Action)>>,
    working_dir: &str,
) -> Option<String> {
    path::install_cwd_allow_rules(rules, working_dir)
}

/// Install a builtin-allow for `/dev/null` on every tool so the
/// harmless bit-bucket never triggers a permission prompt. Writes
/// to `/dev/null` discard data; reads return immediate EOF — no
/// side effects, no security risk, no reason to ask.
fn install_dev_null_allow(rules: &mut HashMap<String, Vec<(Pattern, Action)>>) {
    path::install_dev_null_allow(rules)
}

pub(crate) fn resolve_absolute(path: &str, working_dir: &str) -> String {
    path::resolve_absolute(path, working_dir)
}

#[cfg(test)]
#[path = "checker_tests.rs"]
mod tests;
