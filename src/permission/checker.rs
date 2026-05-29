use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::permission::allowlist;
use crate::permission::engine;
use crate::permission::path;
use crate::permission::pattern::Pattern;
use crate::permission::{PermissionConfig, SecurityMode};

pub type PermCheck = Arc<Mutex<PermissionChecker>>;

/// Synchronous decision result. A `CheckResult`-returning query API
/// over the engine, used by the test oracle (`check`/`check_path`
/// exercise the engine across ~1700 assertions). No production caller
/// today — the runtime path returns an engine `Decision` via
/// `authorize_scope`/`enforce` — so it's `dead_code`-allowed.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    Allowed,
    Ask,
    Denied(String),
}

/// Render a decision's audit trail for the `/why` command: the final
/// effect + deciding policy, then every applicable policy's vote in
/// evaluation order (and the skipped ones, so it's clear what did and
/// didn't apply).
fn format_decision(tool: &str, input: &str, decision: &engine::types::Decision) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "why: {tool} {input:?}");
    let _ = writeln!(out, "  → {:?}  ({})", decision.effect, decision.reason());
    for e in &decision.trace {
        if e.applied {
            let eff = e
                .effect
                .map(|x| format!("{x:?}"))
                .unwrap_or_else(|| "—".to_string());
            let _ = writeln!(out, "  · {:<16} {eff:<6} {}", e.policy, e.why);
        } else {
            let _ = writeln!(out, "  · {:<16} (n/a)  {}", e.policy, e.why);
        }
    }
    out
}

/// Map an engine `Decision` onto the `CheckResult` returned by the
/// `check`/`check_path` test-oracle facade.
#[allow(dead_code)]
fn effect_to_result(decision: engine::types::Decision) -> CheckResult {
    use engine::types::Effect;
    match decision.effect {
        Effect::Allow => CheckResult::Allowed,
        Effect::Ask => CheckResult::Ask,
        Effect::Deny => CheckResult::Denied(decision.reason()),
    }
}

/// Thin facade over the unified authorization [`Engine`]. Tools call
/// `authorize_scope` / `authorize_request` (via the `enforce`
/// chokepoint) and `check` / `check_path` (a `CheckResult` wrapper used
/// by `/allow`, `/why`, and the tests). All decision logic lives in the
/// engine; the checker just normalizes inputs, holds the working
/// directory + mode, and keeps a display copy of the session allowlist.
pub struct PermissionChecker {
    working_dir: String,
    /// Cached canonical form of `working_dir`. Used by
    /// `is_external_path` (a live API consumed by the MCP tool) to
    /// compare canonical paths without a syscall per check.
    working_dir_canonical: String,
    /// Display/persistence copy of the session "allow always" grants
    /// (the engine holds the authoritative op-scoped copy used for
    /// decisions). Powers `/allow list|remove|clear` and session save.
    session_allowlist: Vec<(String, Pattern)>,
    mode: SecurityMode,
    /// Tools denied by the active prompt's frontmatter `deny_tools`.
    /// Mirrored into the engine's `PolicyCtx`; this copy backs
    /// `any_prompt_denied` (the MCP concrete-name probe).
    prompt_deny_tools: Vec<String>,
    /// The unified authorization engine — the source of truth for every
    /// runtime decision.
    engine: engine::Engine,
}

/// Tool names where the input is a filesystem path. Used by the
/// session-allowlist helpers to decide raw-vs-resolved matching.
pub(crate) fn is_path_tool_name(tool: &str) -> bool {
    engine::is_path_tool_name(tool)
}

impl PermissionChecker {
    pub fn new(
        config: &PermissionConfig,
        mode: SecurityMode,
        working_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let working_dir = working_dir
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
            .to_string_lossy()
            .to_string();
        let working_dir_canonical = canonicalize_for_cache(&working_dir);
        // All rule installation (builtin-allow, cwd-allow, dev-null,
        // bash/mcp defaults, user rules, external_directory) lives in
        // the engine now — see `Engine::from_config`.
        let engine = engine::Engine::from_config(config);

        PermissionChecker {
            working_dir,
            working_dir_canonical,
            session_allowlist: Vec::new(),
            mode,
            prompt_deny_tools: Vec::new(),
            engine,
        }
    }

    /// Engine-backed decision for the `enforce` chokepoint. Normalizes
    /// a single (tool, input) into an [`engine::types::AccessRequest`],
    /// authorizes it, commits (loop-guard accounting), and returns the
    /// [`engine::types::Decision`]. `is_path` selects path-resource
    /// classification (resolved + in_cwd + dev_null) vs a raw resource.
    pub fn authorize_scope(
        &mut self,
        tool: &str,
        input: &str,
        is_path: bool,
    ) -> engine::types::Decision {
        let req = self.build_request(tool, input, is_path);
        let decision = self.engine.authorize(&req);
        self.engine.commit(&req, &decision);
        decision
    }

    /// Authorize a pre-built (possibly multi-claim) request and commit
    /// it. Used by tools — bash especially — that decompose one
    /// invocation into several claims (command segments + redirect /
    /// mutation targets) and want ONE atomic decision + at most one
    /// prompt instead of N independent `enforce` calls.
    pub fn authorize_request(
        &mut self,
        req: &engine::types::AccessRequest,
    ) -> engine::types::Decision {
        let decision = self.engine.authorize(req);
        self.engine.commit(req, &decision);
        decision
    }

    /// The checker's working directory — tools building their own
    /// multi-claim requests need it to classify path resources. (Only
    /// the `semantic-bash` bash path uses this today.)
    #[cfg_attr(not(feature = "semantic-bash"), allow(dead_code))]
    pub fn working_dir(&self) -> &str {
        &self.working_dir
    }

    /// Dry-run a decision and render its full audit trail (which
    /// policy decided and why, plus every applicable policy's vote).
    /// Pure: no commit, no loop-guard accounting. Backs the `/why`
    /// command so the user can see exactly what governs an action.
    pub fn explain(&self, tool: &str, input: &str, is_path: bool) -> String {
        let req = self.build_request(tool, input, is_path);
        let decision = self.engine.authorize(&req);
        format_decision(tool, input, &decision)
    }

    /// Normalize a (tool, input) pair into a one-resource request. The
    /// raw-resource variant picks the resource type from the tool:
    /// shell → Command, mcp_tool → Mcp, webfetch/websearch → Url,
    /// everything else → Bareword (memory/skill action, grep pattern…).
    fn build_request(
        &self,
        tool: &str,
        input: &str,
        is_path: bool,
    ) -> engine::types::AccessRequest {
        use engine::types::Resource;
        let resource = if is_path {
            engine::classify_path(input, &self.working_dir)
        } else {
            match tool {
                "bash" | "shell" => Resource::Command {
                    raw: input.to_string(),
                    head: input.split_whitespace().next().unwrap_or("").to_string(),
                },
                "mcp_tool" => {
                    // input shape: "mcp_tool:<server>:<name>"
                    let mut parts = input.splitn(3, ':');
                    let _umbrella = parts.next();
                    let server = parts.next().unwrap_or("").to_string();
                    let name = parts.next().unwrap_or("").to_string();
                    Resource::Mcp {
                        server,
                        name,
                        raw: input.to_string(),
                    }
                }
                "webfetch" | "websearch" => Resource::Url(input.to_string()),
                _ => Resource::Bareword(input.to_string()),
            }
        };
        engine::types::AccessRequest::single(
            tool,
            engine::tool_operation(tool),
            resource,
            self.mode,
            input,
        )
    }

    /// Install the current prompt's deny-list. Called when the
    /// active prompt changes (startup, session load, `/prompt
    /// <name>`); pass an empty vec to clear.
    pub fn set_prompt_deny_tools(&mut self, denied: Vec<String>) {
        self.engine.ctx_mut().prompt_deny = denied.clone();
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

    #[allow(dead_code)] // test oracle (see CheckResult)
    /// Decision for a non-path tool input (bash command, mcp id,
    /// memory/skill action, grep pattern…). Delegates to the unified
    /// engine. A `CheckResult` convenience wrapper used by `/allow`,
    /// `/why`, and the test suite; the engine is the source of truth.
    pub fn check(&mut self, tool: &str, input: &str) -> CheckResult {
        effect_to_result(self.authorize_scope(tool, input, false))
    }

    #[allow(dead_code)] // test oracle (see CheckResult)
    /// Decision for a filesystem-path tool input. Path classification
    /// (resolved / in_cwd / dev_null) happens inside `authorize_scope`.
    pub fn check_path(&mut self, tool: &str, path: &str) -> CheckResult {
        // Reject obvious LLM hallucinations ("1", "a") before the
        // engine — preserves the old `validate_path` guard.
        if let Err(reason) = path::validate_path(path) {
            return CheckResult::Denied(reason);
        }
        effect_to_result(self.authorize_scope(tool, path, true))
    }

    #[allow(dead_code)] // test oracle
    fn is_session_allowed(&self, tool: &str, input: &str) -> bool {
        allowlist::is_allowed(&self.session_allowlist, tool, input)
    }

    /// Side-effect-free re-check of ONLY the session allowlist (the
    /// state a fresh "allow always" mutates) for a pending request.
    /// Unlike [`Self::check`] / [`Self::check_path`] it does NOT touch
    /// the doom-loop counters or apply mode coercion — it answers the
    /// narrow question "would the current session allowlist allow this
    /// right now?".
    ///
    /// Used by the UI to coalesce parallel-tool permission prompts:
    /// when the agent fires several tool calls at once, each that needs
    /// permission queues its own request. If the user picks "allow
    /// always" on the first, the queued siblings that the new pattern
    /// now covers should be auto-allowed instead of re-prompting (and
    /// re-flashing the Alert avatar). Mirrors the raw-vs-path dispatch
    /// and the resolve-both-forms logic of the real checks so a
    /// relative allow-always pattern matches an absolute probe.
    pub fn session_allows_now(&self, tool: &str, input: &str) -> bool {
        // Read the ENGINE allowlist (the runtime source of truth that
        // `enforce` consults), op-scoped.
        let op = engine::tool_operation(tool);
        let al = &self.engine.ctx().allowlist;
        if is_path_tool_name(tool) {
            let abs = resolve_absolute(input, &self.working_dir);
            al.allows(op, input) || al.allows(op, &abs)
        } else {
            al.allows(op, input)
        }
    }

    pub fn add_session_allowlist(&mut self, tool: String, pattern_str: &str) {
        // dirge-yevn fix #1: register the pattern AND a
        // canonicalized variant for path-tool entries so the check
        // hits whichever form the upstream path arrives in (raw vs
        // canonical, symlinked vs realpath). The UI's
        // `suggest_pattern` derives the pattern from the input the
        // LLM passed (often the symlinked form), but `check_path`
        // canonicalizes the probe path via `resolve_absolute`. Prior
        // to this fix, a user who "Allow always"'d a write under
        // `/tmp/proj/src/` on macOS got the pattern stored as
        // `/tmp/proj/src/**` while subsequent checks compared against
        // `/private/tmp/proj/src/foo.rs` — no match, re-prompt.
        register_with_canonical_variant(
            &mut self.session_allowlist,
            &tool,
            pattern_str,
            &self.working_dir,
        );
        // F2 write↔edit↔apply_patch aliasing: when the user "always
        // allows" any of these three, also register the pattern under
        // the OTHER TWO so the alias check in enforce() doesn't
        // re-prompt. Without this, a user who "always allows" write
        // gets asked again on the next write because the edit-alias
        // check returns Ask with no allowlist match.
        //
        // dirge-yevn fix #2: previously this only mirrored
        // write→edit and edit→{write,apply_patch}, leaving
        // apply_patch→write unmirrored. Result: an "Allow always" on
        // a write left apply_patch's own rules (in the checker's
        // `check_path("apply_patch", ...)`) with no allowlist entry,
        // so a subsequent apply_patch call re-prompted. The fix is
        // full bidirectional mirroring across the three aliases.
        let aliases: &[&str] = match tool.as_str() {
            "write" => &["edit", "apply_patch"],
            "edit" => &["write", "apply_patch"],
            "apply_patch" => &["write", "edit"],
            _ => &[],
        };
        for alias in aliases {
            register_with_canonical_variant(
                &mut self.session_allowlist,
                alias,
                pattern_str,
                &self.working_dir,
            );
        }

        // Engine (runtime source of truth). Op-scoped: write/edit/
        // apply_patch all map to Operation::Edit, so a single grant
        // covers the trio — no mirroring needed. Add a canonical
        // variant for path tools so a relative "allow always" pattern
        // matches the absolute probe the engine checks against.
        let op = engine::tool_operation(&tool);
        self.engine.allow_always(op, pattern_str);
        if is_path_tool_name(&tool)
            && let Some(canon) = canonicalize_path_pattern(pattern_str, &self.working_dir)
            && canon != pattern_str
        {
            self.engine.allow_always(op, &canon);
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
        let removed = allowlist::remove_at(&mut self.session_allowlist, idx)?;
        let (tool, pattern_str) = &removed;
        // Also revoke from the ENGINE allowlist (the runtime source of
        // truth that SessionAllowlistPolicy reads). The display list and
        // engine list aren't 1:1, so remove by matched (op, original) —
        // for both the raw pattern and the canonical-path twin that
        // `add_session_allowlist` registered for path tools.
        let op = engine::tool_operation(tool);
        let canon = if is_path_tool_name(tool) {
            canonicalize_path_pattern(pattern_str, &self.working_dir).filter(|c| c != pattern_str)
        } else {
            None
        };
        let al = &mut self.engine.ctx_mut().allowlist;
        al.remove(op, pattern_str);
        if let Some(c) = canon {
            al.remove(op, &c);
        }
        Some(removed)
    }

    /// Remove ALL allowlist entries. Used by `/allow clear`.
    pub fn clear_session_allowlist(&mut self) {
        allowlist::clear(&mut self.session_allowlist);
        self.engine.ctx_mut().allowlist.clear();
    }

    pub fn set_mode(&mut self, mode: SecurityMode) {
        self.mode = mode;
    }

    /// Count of explicit `Deny` rules (configured + external_directory).
    /// Used by the host to warn when Yolo mode renders them inert
    /// (audit H11). Delegates to the engine, the rule owner.
    pub fn deny_rule_count(&self) -> usize {
        self.engine.deny_rule_count()
    }

    pub fn mode(&self) -> SecurityMode {
        self.mode
    }

    pub fn set_working_dir(&mut self, dir: &str) {
        self.working_dir = dir.to_string();
        self.working_dir_canonical = canonicalize_for_cache(dir);
        // A cwd change drops session-scoped state tied to the old
        // project: the loop-guard counters and the "allow always"
        // grants (privilege carry-over guard — the engine recomputes
        // in_cwd per request, so no rule glob needs rebuilding).
        self.session_allowlist.clear();
        self.engine.ctx_mut().repeat.clear();
        self.engine.ctx_mut().allowlist.clear();
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
}

/// One-shot canonicalize for the working-directory cache. Best
/// effort: if canonicalize fails (cwd doesn't exist on disk, e.g.
/// in tests that pass a fixture path), fall back to the literal
/// string so the `starts_with` comparisons in `is_external_path`
/// still work for the literal form.
fn canonicalize_for_cache(working_dir: &str) -> String {
    path::canonicalize_for_cache(working_dir)
}

pub(crate) fn resolve_absolute(path: &str, working_dir: &str) -> String {
    path::resolve_absolute(path, working_dir)
}

/// Register `pattern_str` under `tool` in the session allowlist,
/// and ALSO register a canonicalized variant when the pattern is a
/// path-tool entry whose literal prefix differs from its canonical
/// form. Closes the symlink-mismatch bug: a pattern derived from
/// the symlinked working_dir (e.g. `/tmp/proj/src/**`) wouldn't
/// otherwise match a canonicalized probe path (e.g.
/// `/private/tmp/proj/src/foo.rs`).
///
/// Non-path tools (`bash`, `mcp_tool`, etc.) skip the second
/// registration since their patterns aren't filesystem paths and
/// canonicalization is meaningless.
///
/// Dedup is handled by `allowlist::add`, so a no-op when the
/// canonical form already equals the original.
fn register_with_canonical_variant(
    allowlist: &mut Vec<(String, crate::permission::pattern::Pattern)>,
    tool: &str,
    pattern_str: &str,
    working_dir: &str,
) {
    allowlist::add(allowlist, tool, pattern_str);
    if !is_path_tool_name(tool) {
        return;
    }
    if let Some(canonical_pat) = canonicalize_path_pattern(pattern_str, working_dir)
        && canonical_pat != pattern_str
    {
        allowlist::add(allowlist, tool, &canonical_pat);
    }
}

/// Best-effort canonicalize the literal-prefix portion of a path
/// glob pattern. Splits on the first glob metacharacter (`*`, `?`,
/// `[`, `{`); canonicalizes the prefix; reassembles the pattern.
/// Used by `register_with_canonical_variant` to add a realpath-form
/// twin to a symlink-form session-allowlist pattern.
///
/// Returns `None` when:
///   - the literal prefix is empty (pattern starts with a glob),
///   - `canonicalize` fails AND the prefix doesn't resolve via
///     `resolve_absolute` (relative path that doesn't exist on
///     disk and `working_dir` itself is bogus).
fn canonicalize_path_pattern(pattern_str: &str, working_dir: &str) -> Option<String> {
    let split_idx = pattern_str
        .find(['*', '?', '[', '{'])
        .unwrap_or(pattern_str.len());
    if split_idx == 0 {
        return None;
    }
    let (head, tail) = pattern_str.split_at(split_idx);
    // Trim a trailing `/` from the head so the canonicalize call
    // operates on the directory itself; we re-attach the slash
    // when reassembling. Without this, a head like
    // `/tmp/proj/src/` would round-trip as `/private/tmp/proj/src`
    // (no trailing slash) and the reassembled pattern would lose
    // a slash compared to the original.
    let (head_trimmed, had_trailing_slash) = match head.strip_suffix('/') {
        Some(stripped) => (stripped, true),
        None => (head, false),
    };
    if head_trimmed.is_empty() {
        return None;
    }
    // RELATIVE-HEAD ANCHORING (re-prompt bug): `suggest_pattern`
    // derives a path-tool pattern from the parent of the LLM's input.
    // When the LLM sends a relative path (e.g. `src/main.rs`), the
    // stored pattern is the RELATIVE glob `src/**`, which compiles to
    // `^src(?:/.*)?$`. But `check_path` always matches against the
    // canonical ABSOLUTE form via `resolve_absolute`, so the next call
    // (especially when the LLM sends an absolute path, or the same
    // file resolved through the cwd) never matches the relative
    // pattern and the user is re-prompted despite "allow always".
    //
    // The canonical twin must be anchored at the CHECKER's
    // `working_dir`, not the process cwd. A bare `std::fs::canonicalize`
    // on a relative head resolves against `std::env::current_dir()`,
    // which can differ from the checker's working_dir (the agent may
    // have `cd`'d via `set_working_dir`). For relative heads, anchor at
    // `working_dir` first; this keeps the boundary tight — the twin can
    // only point inside `working_dir` (or wherever the symlink-followed
    // canonical path lands), never escaping to an arbitrary absolute
    // location chosen by the LLM.
    if !std::path::Path::new(head_trimmed).is_absolute() {
        let resolved = resolve_absolute(head_trimmed, working_dir);
        if resolved != head_trimmed {
            let mut out = resolved;
            if had_trailing_slash {
                out.push('/');
            }
            out.push_str(tail);
            return Some(out);
        }
    }
    let canonical_head = std::fs::canonicalize(head_trimmed)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| {
            // Fallback 1: try resolving as a possibly-relative path
            // anchored at working_dir. Only useful when the head
            // exists on disk; resolve_absolute is best-effort.
            let resolved = resolve_absolute(head_trimmed, working_dir);
            if resolved != head_trimmed {
                return Some(resolved);
            }
            // Fallback 2: the literal head doesn't exist (yet) —
            // walk up to the closest existing ancestor, canonicalize
            // THAT, and project the missing suffix back on. Handles
            // "Allow always" on a not-yet-existent path that gets
            // created later (e.g. user opts into a directory that
            // doesn't exist; the next operation creates it; the
            // canonicalised probe would otherwise diverge from the
            // stored symlink-form pattern). See the symlink discussion
            // in `register_with_canonical_variant`.
            project_canonical_from_existing_ancestor(head_trimmed)
        })?;
    let mut out = canonical_head;
    if had_trailing_slash {
        out.push('/');
    }
    out.push_str(tail);
    Some(out)
}

/// Walk up the ancestors of `path` until we find one that exists on
/// disk, canonicalize that, and re-attach the missing-from-disk
/// suffix. Returns `None` when no ancestor canonicalizes (e.g.
/// pathological inputs or filesystem permission errors). Used by
/// `canonicalize_path_pattern` to handle "Allow always" on a
/// not-yet-existent path that's later created.
fn project_canonical_from_existing_ancestor(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    let mut tail_components: Vec<&std::ffi::OsStr> = Vec::new();
    let mut anchor = p;
    loop {
        match anchor.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                // Cache the component we're stripping so we can
                // re-attach it after canonicalizing the parent.
                if let Some(name) = anchor.file_name() {
                    tail_components.push(name);
                }
                anchor = parent;
                if let Ok(canonical) = std::fs::canonicalize(anchor) {
                    let mut out = canonical;
                    for name in tail_components.iter().rev() {
                        out.push(name);
                    }
                    return Some(out.to_string_lossy().into_owned());
                }
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
#[path = "checker_tests.rs"]
mod tests;
