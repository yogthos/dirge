//! Construction of the standard dirge [`Engine`] from a
//! [`PermissionConfig`], plus the tool→[`Operation`] mapping and the
//! path classifier used to normalize a tool call into an
//! [`AccessRequest`].
//!
//! Translation is deliberately mechanical: only USER-supplied rules
//! (legacy per-tool fields + the `tools` map), the bash/mcp defaults,
//! and `external_directory` become [`Rule`]s. The transparent allows
//! (read-only tools, memory/skill, dev-null, in-cwd writes) are NOT
//! translated — they live in [`BuiltinAllowPolicy`] as code, so there
//! is no double-install. User rules outrank builtin-allow by virtue of
//! `ConfiguredRulePolicy` sitting above it in the decider order.

use std::path::PathBuf;

use super::policies::{
    BuiltinAllowPolicy, ConfiguredDenyPolicy, ConfiguredRulePolicy, DefaultActionPolicy,
    ExternalDirPolicy, LoopGuardPolicy, OpMatch, PromptDenyPolicy, Rule, SessionAllowlistPolicy,
    YoloPolicy,
};
use super::policy::{Decider, Modifier, PolicyCtx};
use super::types::{Effect, Operation, Resource};
use super::{Engine, classify::pattern_for_tool};
use crate::permission::path::{canonicalize_for_cache, resolve_absolute};
use crate::permission::{Action, PermissionConfig};

/// Default retry-loop threshold: the Nth identical *prompted* request
/// is hard-denied. (The breaking config in Phase 4 makes this tunable.)
const LOOP_GUARD_THRESHOLD: u32 = 3;

impl From<Action> for Effect {
    fn from(a: Action) -> Effect {
        match a {
            Action::Allow => Effect::Allow,
            Action::Ask => Effect::Ask,
            Action::Deny => Effect::Deny,
        }
    }
}

/// Map a concrete tool name to its coarse [`Operation`].
pub fn tool_operation(tool: &str) -> Operation {
    match tool {
        "read" | "grep" | "find_files" | "glob" | "list_dir" | "repo_overview" | "lsp"
        | "list_symbols" | "get_symbol_body" | "find_definition" | "find_callers"
        | "find_callees" => Operation::Read,
        "write" => Operation::Edit,
        "edit" | "apply_patch" => Operation::Edit,
        "bash" | "shell" => Operation::Execute,
        "webfetch" | "websearch" => Operation::Network,
        "mcp_tool" => Operation::Mcp,
        "memory" => Operation::Memory,
        "skill" => Operation::Skill,
        // Recursive sub-agent execution: high-risk, not auto-allowed.
        "task" => Operation::Agent,
        // Internal no-effect tools: builtin-allowed.
        "task_status" | "question" | "write_todo_list" => Operation::Meta,
        // Unknown (plugin/MCP) tools: not auto-allowed; fall to
        // configured rules or the default (Accept-coercible).
        _ => Operation::Other,
    }
}

/// Build a `Path` resource from a raw path string, computing the
/// canonical form, whether it is inside `working_dir`, and whether it
/// is `/dev/null`. This is the single place path classification
/// happens (replacing the scattered `install_cwd_allow_rules` /
/// `install_dev_null_allow` / `is_external_path` logic).
pub fn classify_path(raw: &str, working_dir: &str) -> Resource {
    let resolved_str = resolve_absolute(raw, working_dir);
    let dev_null = resolved_str == "/dev/null" || raw == "/dev/null";
    // Compare against the cwd in every form it might take so a
    // symlinked root (macOS `/tmp → /private/tmp`) doesn't misclassify
    // in-tree paths as external: the path resolved through the
    // deepest-existing-ancestor canonicalization, the cached
    // canonical, and the literal working_dir.
    let under = |base: &str| {
        let b = base.trim_end_matches('/');
        !b.is_empty()
            && b != "/"
            && (resolved_str == b || resolved_str.starts_with(&format!("{b}/")))
    };
    // A working_dir containing glob metacharacters can't anchor a
    // trustworthy in-cwd classification (the old code refused to
    // install a CWD-allow for such dirs); treat nothing as in-cwd so
    // those writes still prompt.
    let cwd_has_glob = working_dir.contains(['*', '?', '[', '{']);
    let in_cwd = !cwd_has_glob
        && (under(&resolve_absolute(working_dir, working_dir))
            || under(&canonicalize_for_cache(working_dir))
            || under(working_dir));
    Resource::Path {
        raw: raw.to_string(),
        resolved: PathBuf::from(resolved_str),
        in_cwd,
        dev_null,
    }
}

/// Map a config `OpSpec` to the engine's `OpMatch`.
fn op_match(op: crate::permission::OpSpec) -> OpMatch {
    use crate::permission::OpSpec;
    match op {
        OpSpec::Any => OpMatch::Any,
        OpSpec::Read => OpMatch::One(Operation::Read),
        OpSpec::Edit => OpMatch::One(Operation::Edit),
        OpSpec::Execute => OpMatch::One(Operation::Execute),
        OpSpec::Network => OpMatch::One(Operation::Network),
        OpSpec::Mcp => OpMatch::One(Operation::Mcp),
        OpSpec::Memory => OpMatch::One(Operation::Memory),
        OpSpec::Skill => OpMatch::One(Operation::Skill),
        OpSpec::Agent => OpMatch::One(Operation::Agent),
        OpSpec::Meta => OpMatch::One(Operation::Meta),
    }
}

/// Glob style for a rule's operation: path-style (`*` = one segment)
/// for the file ops, shell-style for everything else. `Any` (`op: "*"`)
/// spans execute/network/mcp resources too, so it uses command-style —
/// a strict superset of path-style for the same glob (command `*` = `.*`
/// also matches the `[^/]*` path case), so `{op:"*", match:"git *"}`
/// covers `git push origin/main` (slash crossed) AND path globs.
fn pattern_for_op(op: crate::permission::OpSpec, pat: &str) -> crate::permission::pattern::Pattern {
    use crate::permission::OpSpec;
    use crate::permission::pattern::Pattern;
    match op {
        OpSpec::Read | OpSpec::Edit => Pattern::new(pat),
        _ => Pattern::new_command(pat),
    }
}

/// Build an engine [`Rule`] from a config rule.
fn rule_from_config(rc: &crate::permission::RuleConfig) -> Rule {
    Rule {
        op: op_match(rc.op),
        tool: rc.tool.clone(),
        pattern: pattern_for_op(rc.op, &rc.pattern),
        effect: rc.effect.into(),
        original: rc.pattern.clone(),
    }
}

impl Engine {
    /// Assemble the standard dirge policy set from configuration. The
    /// decider order encodes precedence; see `policies.rs`.
    pub fn from_config(config: &PermissionConfig) -> Engine {
        let mut rules: Vec<Rule> = Vec::new();

        // Built-in safe-bash defaults (git status / cargo / test
        // runners allow, `rm -rf /**` etc. deny). Installed FIRST so a
        // user rule for the same command wins by last-match. These are
        // dirge's "sane defaults" for Execute, distinct from the
        // BuiltinAllowPolicy (reads / memory / skill / dev-null / cwd).
        for (pat, action) in crate::permission::default_bash_rules() {
            rules.push(Rule {
                op: OpMatch::One(Operation::Execute),
                tool: Some("bash".to_string()),
                pattern: pattern_for_tool("bash", pat),
                effect: action.into(),
                original: format!("bash:{pat}"),
            });
        }
        // MCP default: prompt unless a user rule allows a server.
        rules.push(Rule {
            op: OpMatch::One(Operation::Mcp),
            tool: None,
            pattern: pattern_for_tool("mcp_tool", "*"),
            effect: Effect::Ask,
            original: "mcp:*".to_string(),
        });

        // User rules, in order — appended after the defaults so they
        // override by last-match-wins.
        rules.extend(config.rules.iter().map(rule_from_config));

        // external_directory rules (governs out-of-project paths).
        let ext_rules: Vec<Rule> = config
            .external_directory
            .iter()
            .map(rule_from_config)
            .collect();

        let default: Effect = config.default.unwrap_or(Action::Ask).into();

        let deny_rules = rules
            .iter()
            .chain(ext_rules.iter())
            .filter(|r| r.effect == Effect::Deny)
            .count();

        let deciders: Vec<Box<dyn Decider>> = vec![
            Box::new(PromptDenyPolicy),
            Box::new(YoloPolicy),
            // dirge-ct16: configured `deny` is terminal above session-allow
            // (so a broad allow-always grant can't override it) but below
            // Yolo (preserving Yolo's documented "all rules off"). Covers
            // both the main rule list and external_directory denies.
            Box::new(ConfiguredDenyPolicy {
                rules: rules.clone(),
                ext_rules: ext_rules.clone(),
            }),
            Box::new(SessionAllowlistPolicy),
            Box::new(ConfiguredRulePolicy { rules }),
            Box::new(BuiltinAllowPolicy),
            Box::new(ExternalDirPolicy { rules: ext_rules }),
            Box::new(DefaultActionPolicy { default }),
        ];
        // `doom_loop: "allow"` opts out of the retry-loop guard
        // entirely (no hard-deny); otherwise the guard hard-denies a
        // genuine retry loop at the threshold.
        let threshold = if config.doom_loop == Some(Action::Allow) {
            u32::MAX
        } else {
            LOOP_GUARD_THRESHOLD
        };
        let modifiers: Vec<Box<dyn Modifier>> = vec![Box::new(LoopGuardPolicy { threshold })];

        let mut engine = Engine::new(deciders, modifiers, PolicyCtx::default());
        engine.deny_rules = deny_rules;
        engine
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::SecurityMode;
    use crate::permission::engine::types::AccessRequest;

    fn req(
        op: Operation,
        tool: &str,
        mode: SecurityMode,
        resources: Vec<Resource>,
    ) -> AccessRequest {
        AccessRequest {
            tool: tool.to_string(),
            claims: resources
                .into_iter()
                .map(|r| crate::permission::engine::types::Claim::new(op, r))
                .collect(),
            mode,
            display_input: String::new(),
        }
    }

    #[test]
    fn tool_operation_mapping() {
        assert_eq!(tool_operation("read"), Operation::Read);
        assert_eq!(tool_operation("grep"), Operation::Read);
        assert_eq!(tool_operation("write"), Operation::Edit);
        assert_eq!(tool_operation("edit"), Operation::Edit);
        assert_eq!(tool_operation("apply_patch"), Operation::Edit);
        assert_eq!(tool_operation("bash"), Operation::Execute);
        assert_eq!(tool_operation("webfetch"), Operation::Network);
        assert_eq!(tool_operation("mcp_tool"), Operation::Mcp);
        assert_eq!(tool_operation("memory"), Operation::Memory);
        assert_eq!(tool_operation("skill"), Operation::Skill);
        assert_eq!(tool_operation("question"), Operation::Meta);
    }

    #[test]
    fn classify_path_in_cwd_dev_null_external() {
        let p = classify_path("/proj/src/x.rs", "/proj");
        assert!(matches!(
            p,
            Resource::Path {
                in_cwd: true,
                dev_null: false,
                ..
            }
        ));
        let p = classify_path("/dev/null", "/proj");
        assert!(matches!(p, Resource::Path { dev_null: true, .. }));
        let p = classify_path("/etc/passwd", "/proj");
        assert!(matches!(
            p,
            Resource::Path {
                in_cwd: false,
                dev_null: false,
                ..
            }
        ));
    }

    #[test]
    fn default_config_bash_defaults_present() {
        let e = Engine::from_config(&PermissionConfig::default());
        // A safe default bash command (git status) should be allowed;
        // an unfamiliar one falls to default Ask.
        let d = e.authorize(&req(
            Operation::Execute,
            "bash",
            SecurityMode::Standard,
            vec![Resource::Command {
                raw: "git status -s".into(),
                head: "git".into(),
            }],
        ));
        assert_eq!(
            d.effect,
            Effect::Allow,
            "git status -s is a default-allowed bash command"
        );

        let d = e.authorize(&req(
            Operation::Execute,
            "bash",
            SecurityMode::Standard,
            vec![Resource::Command {
                raw: "frobnicate --hard".into(),
                head: "frobnicate".into(),
            }],
        ));
        assert_eq!(d.effect, Effect::Ask, "unknown bash command prompts");
    }

    fn rule(
        op: crate::permission::OpSpec,
        m: &str,
        effect: Action,
    ) -> crate::permission::RuleConfig {
        crate::permission::RuleConfig {
            op,
            pattern: m.to_string(),
            effect,
            tool: None,
        }
    }

    #[test]
    fn user_execute_rule_overrides_default() {
        use crate::permission::OpSpec;
        // A blanket `execute **: allow` (appended after the built-in
        // bash defaults) wins by last-match, so even an unknown command
        // is allowed.
        let cfg = PermissionConfig {
            rules: vec![rule(OpSpec::Execute, "**", Action::Allow)],
            ..Default::default()
        };
        let e = Engine::from_config(&cfg);
        let d = e.authorize(&req(
            Operation::Execute,
            "bash",
            SecurityMode::Standard,
            vec![Resource::Command {
                raw: "frobnicate".into(),
                head: "frobnicate".into(),
            }],
        ));
        assert_eq!(d.effect, Effect::Allow);
    }

    #[test]
    fn user_rule_overrides_builtin_allow() {
        use crate::permission::OpSpec;
        // read is builtin-allowed; a user deny rule must win.
        let cfg = PermissionConfig {
            rules: vec![rule(OpSpec::Read, "/secret/**", Action::Deny)],
            ..Default::default()
        };
        let e = Engine::from_config(&cfg);
        let d = e.authorize(&req(
            Operation::Read,
            "read",
            SecurityMode::Standard,
            vec![classify_path("/secret/k", "/proj")],
        ));
        assert_eq!(
            d.effect,
            Effect::Deny,
            "user read deny rule beats builtin-allow"
        );
        // a non-secret read is still allowed
        let d = e.authorize(&req(
            Operation::Read,
            "read",
            SecurityMode::Standard,
            vec![classify_path("/proj/ok.rs", "/proj")],
        ));
        assert_eq!(d.effect, Effect::Allow);
    }

    #[test]
    fn external_directory_rule_allows_outside_path() {
        use crate::permission::OpSpec;
        let cfg = PermissionConfig {
            external_directory: vec![rule(OpSpec::Any, "/shared/**", Action::Allow)],
            ..Default::default()
        };
        let e = Engine::from_config(&cfg);
        // external write to /shared is allowed by the ext-dir rule
        let d = e.authorize(&req(
            Operation::Edit,
            "write",
            SecurityMode::Standard,
            vec![classify_path("/shared/lib/x", "/proj")],
        ));
        assert_eq!(d.effect, Effect::Allow);
        // external write elsewhere still asks
        let d = e.authorize(&req(
            Operation::Edit,
            "write",
            SecurityMode::Standard,
            vec![classify_path("/etc/x", "/proj")],
        ));
        assert_eq!(d.effect, Effect::Ask);
    }

    #[test]
    fn any_op_rule_matches_command_across_slash() {
        use crate::permission::OpSpec;
        // dirge-z9tz: an `op: "*"` rule must use shell-style globbing
        // when matched against a command (or MCP key), so a single `*`
        // crosses slashes. Pre-fix, Any compiled path-style (`*` =
        // `[^/]*`), so `git *` silently missed `git push origin/main`.
        let cfg = PermissionConfig {
            rules: vec![rule(OpSpec::Any, "git *", Action::Allow)],
            ..Default::default()
        };
        let e = Engine::from_config(&cfg);
        let d = e.authorize(&req(
            Operation::Execute,
            "bash",
            SecurityMode::Standard,
            vec![Resource::Command {
                raw: "git push origin/main".into(),
                head: "git".into(),
            }],
        ));
        assert_eq!(
            d.effect,
            Effect::Allow,
            "an op:* rule `git *` must match a command containing a slash",
        );
    }

    #[test]
    fn commit_records_once_per_request_despite_duplicate_claims() {
        // dirge-qrto: the loop-guard counter must bump once per PROMPTED
        // request, not once per claim. A request carrying two identical
        // (op, resource) claims must increment the counter by exactly 1.
        let mut e = Engine::from_config(&PermissionConfig::default());
        let claim = || Resource::Command {
            raw: "frobnicate x".into(),
            head: "frobnicate".into(),
        };
        let request = req(
            Operation::Execute,
            "bash",
            SecurityMode::Standard,
            vec![claim(), claim()],
        );
        let d = e.authorize(&request);
        assert_eq!(d.effect, Effect::Ask, "unknown command prompts");
        e.commit(&request, &d);
        assert_eq!(
            e.ctx().repeat.prior(Operation::Execute, "frobnicate x"),
            1,
            "duplicate claims in one request must not double-count the loop guard",
        );
    }

    #[test]
    fn configured_deny_rule_beats_session_allow_via_from_config() {
        use crate::permission::OpSpec;
        // dirge-ct16: a main-list deny must survive a broad session grant.
        let cfg = PermissionConfig {
            rules: vec![rule(OpSpec::Edit, "/etc/secret/**", Action::Deny)],
            ..Default::default()
        };
        let mut e = Engine::from_config(&cfg);
        e.allow_always(Operation::Edit, "/etc/**"); // broad allow-always grant
        let d = e.authorize(&req(
            Operation::Edit,
            "edit",
            SecurityMode::Standard,
            vec![classify_path("/etc/secret/k", "/proj")],
        ));
        assert_eq!(
            d.effect,
            Effect::Deny,
            "configured deny must beat a session allow-always grant",
        );
        assert_eq!(d.deciding.unwrap().policy, "configured-deny");
    }

    #[test]
    fn external_directory_deny_beats_session_allow_via_from_config() {
        use crate::permission::OpSpec;
        // dirge-ct16: external_directory deny also sat below session-allow;
        // it must be terminal too.
        let cfg = PermissionConfig {
            external_directory: vec![rule(OpSpec::Any, "/shared/secret/**", Action::Deny)],
            ..Default::default()
        };
        let mut e = Engine::from_config(&cfg);
        e.allow_always(Operation::Edit, "/shared/**");
        let d = e.authorize(&req(
            Operation::Edit,
            "write",
            SecurityMode::Standard,
            vec![classify_path("/shared/secret/k", "/proj")],
        ));
        assert_eq!(
            d.effect,
            Effect::Deny,
            "external_directory deny must beat a session allow-always grant",
        );
        assert_eq!(d.deciding.unwrap().policy, "configured-deny");
        // a sibling NOT covered by the deny still flows to the ext-dir Ask
        let d = e.authorize(&req(
            Operation::Edit,
            "write",
            SecurityMode::Standard,
            vec![classify_path("/shared/ok/k", "/proj")],
        ));
        // session grant /shared/** allows this one (no deny matches)
        assert_eq!(d.effect, Effect::Allow);
    }
}
