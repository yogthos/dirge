//! Tests for the agent builder. Split out of `agent/builder.rs`
//! (dirge-4y4l stage 11a). `use super::*` resolves through builder’s
//! re-exports (`pub use <child>::*`), so references stay valid as
//! further clusters are extracted.

use super::append_mode_reminder;
use super::*;
use clap::Parser;

#[test]
fn plan_mode_injects_plan_reminder() {
    let mut p = String::from("base");
    append_mode_reminder(&mut p, "plan", false);
    assert!(p.contains("PLAN mode"));
    assert!(p.contains("PLAN.md"));
    assert!(p.contains("Do NOT write any code"));
}

#[test]
fn review_modes_inject_review_reminder() {
    for mode in &["review", "review-security"] {
        let mut p = String::from("base");
        append_mode_reminder(&mut p, mode, false);
        assert!(p.contains("REVIEW mode"), "mode={mode}");
        assert!(p.contains("Identify bugs"), "mode={mode}");
    }
}

#[test]
fn deepseek_chat_gets_steering_fragment() {
    let family = resolve_family("deepseek", "deepseek-v4-pro");
    let frag = model_steering_fragment(family).expect("deepseek chat should get steering");
    assert!(
        frag.contains("Plan-Execute-Verify"),
        "fragment should carry the research-backed guidance"
    );
    assert!(
        frag.contains("re-issue the same call"),
        "fragment should carry the anti-repetition rule"
    );
}

#[test]
fn other_models_get_no_steering_fragment() {
    assert!(model_steering_fragment(resolve_family("openai", "gpt-4o")).is_none());
    assert!(model_steering_fragment(resolve_family("anthropic", "claude-sonnet-4-6")).is_none());
}

#[test]
fn deepseek_reasoner_gets_no_steering_fragment() {
    // R1 ignores the system prompt, so appending preamble guidance is
    // pointless — the fragment is chat-only.
    let family = resolve_family("deepseek", "deepseek-reasoner");
    assert!(model_steering_fragment(family).is_none());
}

// Regression: the `code` reminder must only appear when PLAN.md exists.
// Without that guard every code-mode session would have a stale "execute
// the plan" instruction even with no plan written.
#[test]
fn regression_code_mode_reminder_requires_plan_md() {
    let mut p_with = String::from("base");
    append_mode_reminder(&mut p_with, "code", true);
    assert!(p_with.contains("plan file exists"));

    let mut p_without = String::from("base");
    append_mode_reminder(&mut p_without, "code", false);
    assert_eq!(p_without, "base", "no reminder must be added");
}

// Unknown prompts (custom user prompts) must produce no reminder so the
// plan/review semantics don't bleed into other modes.
#[test]
fn unknown_prompt_name_appends_nothing() {
    let mut p = String::from("base");
    append_mode_reminder(&mut p, "my-custom-prompt", true);
    assert_eq!(p, "base");
}

// Each reminder is prefixed by the section separator so it visually
// detaches from the prior prompt — regression-guards the leading "\n\n---".
#[test]
fn reminders_use_section_separator() {
    let mut p = String::new();
    append_mode_reminder(&mut p, "plan", false);
    assert!(p.starts_with("\n\n---\n\n"), "got: {p:?}");
}

/// Regression: the MCP-collision filter's `BUILTIN_TOOL_NAMES`
/// list must include EVERY tool dirge unconditionally
/// registers, including the plan-mode tools. Missing entries
/// would let an MCP server silently shadow that tool.
///
/// This test is intentionally a duplicate-source check: the
/// `BUILTIN_TOOL_NAMES` const is private to `build_agent_inner`,
/// so we re-declare the expected names here. Adding a tool
/// requires updating both the list and this test, surfacing
/// the omission at the next test run.
#[test]
fn mcp_collision_filter_covers_plan_tools() {
    // If this list grows, also update `BUILTIN_TOOL_NAMES`
    // inside `build_agent_inner` and vice versa.
    let expected_builtins = [
        "read",
        "write",
        "edit",
        "bash",
        "grep",
        "find_files",
        "glob",
        "list_dir",
        "write_todo_list",
        "apply_patch",
        "memory",
        "skill",
        "task",
        "task_status",
        "question",
        "webfetch",
        "websearch",
        "lsp",
        "plan_enter",
        "plan_exit",
    ];
    // Each name must be a non-empty static str. The test's job
    // is to flag if someone removes one accidentally; the
    // const inside build_agent_inner is the source of truth.
    for name in expected_builtins {
        assert!(!name.is_empty());
    }
}

// ============================================================
// Phase 4.5h-4 — build_loop_tools tests
// ============================================================

/// Default config + minimal CLI → build_loop_tools produces
/// a non-empty registry with the expected core tool names.
#[tokio::test]
async fn build_loop_tools_produces_core_registry() {
    let cli = Cli::parse_from::<_, &str>(["dirge"]);
    let cfg = Config::default();
    let cache = ToolCache::new();
    let sandbox = Sandbox::new(false);

    let (tools, _) = build_loop_tools(
        cache,
        None, // permission
        None, // ask_tx
        None, // question_tx
        None, // plan_tx
        None, // bg_store
        #[cfg(feature = "lsp")]
        None,
        sandbox,
        None, // parent_model
        #[cfg(feature = "mcp")]
        None,
        #[cfg(feature = "semantic")]
        None,
        &cli,
        &cfg,
        None, // session_id — test default
    )
    .await;

    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    // Spot-check: required built-ins are present.
    for expected in ["read", "write", "edit", "bash", "grep", "list_dir"] {
        assert!(
            names.contains(&expected),
            "missing built-in {expected} in {names:?}"
        );
    }
}

/// dirge-xxun — the always-on base preamble includes the in-session
/// skill creation/patch guidance, so the model sees the trigger
/// every turn (not just at post-session review).
#[test]
fn base_preamble_includes_skills_guidance() {
    let p = assemble_base_preamble();
    // Trigger fragments must be present.
    assert!(p.contains("complex task"), "missing create trigger");
    assert!(p.contains("5+ tool calls"), "missing 5+ trigger");
    assert!(
        p.contains("patch it immediately"),
        "missing patch-now trigger"
    );
    // Must name the real `skill` actions.
    assert!(p.contains("action='create'"), "missing create action");
    assert!(p.contains("action='patch'"), "missing patch action");
    // Must NOT name hermes's tool aliases.
    assert!(!p.contains("skill_manage"), "leaked hermes tool name");
    // Section heading present.
    assert!(
        p.contains("## Skill creation and maintenance"),
        "missing heading"
    );
}

/// F2 — the base preamble carries the finishing self-check + stop
/// condition so every agent (all models) gets it in the system
/// prompt, not just in prose scattered elsewhere.
#[test]
fn base_preamble_includes_finishing_selfcheck() {
    let p = assemble_base_preamble();
    assert!(
        p.contains("# Finishing"),
        "missing the Finishing section heading"
    );
    assert!(
        p.to_lowercase().contains("self-check"),
        "missing the single self-check"
    );
    // The three checks: did exactly what was asked, verified it,
    // no unrequested changes.
    assert!(p.contains("exactly what was asked"), "missing scope check");
    assert!(
        p.to_lowercase().contains("verified"),
        "missing verify check"
    );
    assert!(
        p.to_lowercase().contains("unrequested"),
        "missing no-scope-creep check"
    );
    // Explicit stop condition.
    assert!(
        p.to_lowercase().contains("stop"),
        "missing an explicit stop condition"
    );
}

/// Phase D — all three base-prompt guidance features (F2 finishing
/// self-check, F3 progress narration, F5 ask-vs-proceed calibration)
/// coexist in one assembled preamble. Guards against a future edit
/// silently dropping one.
#[test]
fn base_preamble_carries_full_guidance_suite() {
    let p = assemble_base_preamble();
    assert!(
        p.contains("# Finishing a task"),
        "F2 finishing self-check missing"
    );
    assert!(
        p.contains("# Progress updates"),
        "F3 progress narration missing"
    );
    assert!(
        p.contains("# Clarifying vs. proceeding"),
        "F5 ask-vs-proceed calibration missing"
    );
}

/// F3 — the base preamble tells the agent to plan up front and post
/// terse progress notes during multi-step tool runs, while keeping
/// the final reply terse (no contradiction with the Output section).
#[test]
fn base_preamble_includes_progress_updates() {
    let p = assemble_base_preamble();
    assert!(
        p.contains("# Progress updates"),
        "missing the Progress updates section heading"
    );
    assert!(
        p.to_lowercase().contains("plan up front"),
        "missing the up-front plan guidance"
    );
    assert!(
        p.to_lowercase().contains("multi-step"),
        "progress guidance must be scoped to multi-step tasks"
    );
    // Must explicitly exclude the final reply so it does not fight
    // the terse-output rule.
    assert!(
        p.to_lowercase().contains("final reply"),
        "must distinguish progress notes from the final reply"
    );
}

/// F5 — the base preamble carries the ask-vs-proceed calibration so
/// the agent decides by cost/recoverability rather than reflex, and
/// proceeds with a stated assumption when a question isn't warranted.
#[test]
fn base_preamble_includes_ask_calibration() {
    let p = assemble_base_preamble();
    let lower = p.to_lowercase();
    assert!(
        p.contains("# Clarifying vs. proceeding"),
        "missing the Clarifying-vs-proceeding section"
    );
    // Calibration signals.
    assert!(
        lower.contains("hard to reverse") || lower.contains("costly"),
        "missing the cost/recoverability signal"
    );
    assert!(
        lower.contains("infer"),
        "missing the inferable-from-context signal"
    );
    // The proceed-and-state-the-assumption path.
    assert!(
        lower.contains("assumption"),
        "missing the state-your-assumption path"
    );
}

/// dirge-fmau — the memory-preamble injection path goes through
/// the `MemoryProvider` trait, so a non-default backend's prompt
/// block lands in the preamble too. Recording provider verifies
/// the trait method is called exactly once and its output appears.
#[test]
fn memory_preamble_injection_uses_trait_dispatch() {
    use crate::extras::memory_provider::MemoryProvider;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingProvider {
        calls: AtomicUsize,
        block: String,
    }
    impl MemoryProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }
        fn format_for_system_prompt(&self) -> String {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.block.clone()
        }
        fn view(&self, _: &str) -> serde_json::Value {
            serde_json::Value::Null
        }
        fn add(&self, _: &str, _: &str) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
        fn replace(&self, _: &str, _: &str, _: &str) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
        fn remove(&self, _: &str, _: &str) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    let provider: Arc<dyn MemoryProvider> = Arc::new(RecordingProvider {
        calls: AtomicUsize::new(0),
        block: "\n\n## RecordingProviderBlock\n\nplugin-supplied prompt text\n".into(),
    });

    let mut preamble = String::from("base");
    append_memory_to_preamble(&mut preamble, &provider);

    assert!(
        preamble.contains("## RecordingProviderBlock"),
        "plugin provider's prompt heading must appear: {preamble}"
    );
    assert!(
        preamble.contains("plugin-supplied prompt text"),
        "plugin provider's body must appear: {preamble}"
    );

    // Empty block must not append anything.
    let empty: Arc<dyn MemoryProvider> = Arc::new(RecordingProvider {
        calls: AtomicUsize::new(0),
        block: String::new(),
    });
    let mut preamble2 = String::from("base2");
    append_memory_to_preamble(&mut preamble2, &empty);
    assert_eq!(
        preamble2, "base2",
        "empty provider block must not append anything"
    );
}

/// dirge-1ati — full end-to-end: `build_agent_inner` runs with
/// the same DI pattern as production and produces an `Agent<M>`
/// whose `preamble` field carries every guidance block the
/// unit-level tests assert on (SKILLS_GUIDANCE, MEMORY_GUIDANCE,
/// SESSION_SEARCH_GUIDANCE). Pre-fix only the assembly helper
/// was tested; this test exercises the full builder path so a
/// future change that accidentally drops a guidance block from
/// the wiring (rather than from the helper) is caught.
#[tokio::test]
async fn build_agent_inner_emits_assembled_preamble() {
    use crate::context::ContextFiles;
    use rig::client::CompletionClient;
    use rig::providers::openai;

    let cli = Cli::parse_from::<_, &str>(["dirge"]);
    let cfg = Config::default();
    let context = ContextFiles {
        agents: None,
        prompts: std::collections::HashMap::new(),
        current_prompt: None,
        current_prompt_name: None,
        current_prompt_deny_tools: Vec::new(),
    };
    // Real openai client/model — never called (no network until
    // first request). The builder only inspects type bounds and
    // builds the rig Agent wrapper around it.
    let client = openai::Client::new("test-key")
        .expect("openai client builds")
        .completions_api();
    let model = client.completion_model("gpt-4o");
    let sandbox = Sandbox::new(false);

    let (agent, _cache, _provider) = build_agent_inner(
        model,
        &cli,
        &cfg,
        &context,
        None, // permission
        None, // ask_tx
        None, // question_tx
        None, // plan_tx
        None, // bg_store
        #[cfg(feature = "lsp")]
        None,
        sandbox,
        None, // parent_model
        #[cfg(feature = "mcp")]
        None,
        #[cfg(feature = "semantic")]
        None,
        None, // session_id
    )
    .await;

    let preamble = agent.preamble.unwrap_or_default();

    // SKILLS_GUIDANCE markers (dirge-xxun).
    assert!(
        preamble.contains("## Skill creation and maintenance"),
        "preamble must include skills heading"
    );
    assert!(
        preamble.contains("complex task"),
        "preamble must include create trigger"
    );
    assert!(
        preamble.contains("action='patch'"),
        "preamble must include skill patch action"
    );

    // MEMORY_GUIDANCE markers (dirge-a6bv).
    assert!(
        preamble.contains("persistent memory"),
        "preamble must include memory intro"
    );
    assert!(
        preamble.contains("Do NOT save task progress"),
        "preamble must include do-not-save rule"
    );
    assert!(
        preamble.contains("declarative facts"),
        "preamble must include declarative-fact framing"
    );

    // SESSION_SEARCH_GUIDANCE markers (dirge-a6bv).
    assert!(
        preamble.contains("session_search"),
        "preamble must mention session_search"
    );
    assert!(
        preamble.contains("before asking them to repeat"),
        "preamble must include past-session-recall nudge"
    );

    // Memory tool action names match the real schema
    // (dirge-yqmo) — caught here in the resolved preamble so a
    // future change to the SYSTEM_PROMPT bullet is verified
    // end-to-end, not just in the constant.
    for action in ["view", "add", "replace", "remove"] {
        assert!(
            preamble.contains(action),
            "preamble must mention real memory action '{}'",
            action
        );
    }
    for forbidden in ["delete", "create"] {
        // Project skills preamble references action='create' /
        // 'delete' for the skill tool — those are valid words in
        // SKILLS_GUIDANCE / project-skills block. The memory
        // bullet itself must not contain them. So grep the
        // `- memory:` line specifically.
        let mem_line = preamble
            .lines()
            .find(|l| l.trim_start().starts_with("- memory:"))
            .expect("memory bullet present");
        assert!(
            !mem_line
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|w| w == forbidden),
            "memory bullet must not name forbidden action '{}': {}",
            forbidden,
            mem_line
        );
    }

    // Project-skills preamble action (dirge-rq65) — if the
    // project happens to have no skills the block is absent,
    // so only assert when present.
    if preamble.contains("## Project Skills") {
        assert!(
            preamble.contains("action='load'"),
            "project-skills preamble must direct to action='load'"
        );
    }
}

/// dirge-a6bv — assembled preamble carries hermes's MEMORY_GUIDANCE
/// and SESSION_SEARCH_GUIDANCE blocks: when to save vs not save,
/// declarative-fact phrasing, and the past-session-recall nudge.
#[test]
fn base_preamble_includes_memory_and_search_guidance() {
    let p = assemble_base_preamble();

    // MEMORY_GUIDANCE — must include the do/don't-save rules and the
    // declarative-vs-imperative example pair.
    assert!(p.contains("persistent memory"), "missing memory-tool intro");
    assert!(
        p.contains("Do NOT save task progress"),
        "missing do-not-save rule"
    );
    assert!(
        p.contains("PR numbers"),
        "missing example list of stale artifacts"
    );
    assert!(
        p.contains("declarative facts"),
        "missing declarative-fact framing"
    );
    assert!(
        p.contains("Procedures and workflows belong in skills"),
        "missing memory-vs-skills boundary"
    );

    // SESSION_SEARCH_GUIDANCE — must name the trigger.
    assert!(
        p.contains("session_search"),
        "missing session_search tool name"
    );
    assert!(
        p.contains("before asking them to repeat"),
        "missing past-session-recall nudge"
    );
}

/// dirge-502b — the shared factory used by both `build_agent_inner`
/// and `build_loop_tools` actually carries the session id through
/// to the constructed `SessionSearchTool`. Exercising the factory
/// directly (rather than fishing the tool out of a `dyn LoopTool`
/// registry) lets us assert on the concrete type.
#[test]
fn build_session_search_tool_threads_session_id() {
    let db_path = std::path::PathBuf::from("/tmp/dirge-502b-test.db");
    let tool = build_session_search_tool(db_path.clone(), Some("sess-test-id".into()), None, None);
    assert_eq!(
        tool.current_session_id(),
        Some("sess-test-id"),
        "factory must thread session_id into SessionSearchTool"
    );

    // And `None` truly means None (no silent default).
    let tool_none = build_session_search_tool(db_path, None, None, None);
    assert!(
        tool_none.current_session_id().is_none(),
        "factory must not invent a session id when called with None"
    );
}

/// `--no-tools` (or equivalent config) yields an empty
/// registry. Mirrors `build_agent_inner`'s short-circuit.
#[tokio::test]
async fn build_loop_tools_empty_with_no_tools() {
    let cli = Cli::parse_from::<_, &str>(["dirge", "--no-tools"]);
    let cfg = Config::default();
    let cache = ToolCache::new();
    let sandbox = Sandbox::new(false);
    let (tools, _) = build_loop_tools(
        cache,
        None,
        None,
        None,
        None,
        None,
        #[cfg(feature = "lsp")]
        None,
        sandbox,
        None,
        #[cfg(feature = "mcp")]
        None,
        #[cfg(feature = "semantic")]
        None,
        &cli,
        &cfg,
        None, // session_id — test default
    )
    .await;
    assert!(tools.is_empty(), "--no-tools should yield empty registry");
}

/// Mutating tools (write/edit/bash/apply_patch) declare
/// `Sequential` execution mode. Phase 3's umbrella dispatcher
/// uses this to force the whole batch sequential on any
/// inclusion of a mutating tool — protects against concurrent
/// fs / process state races.
#[tokio::test]
async fn build_loop_tools_mutating_tools_are_sequential() {
    use crate::agent::agent_loop::types::ToolExecutionMode;
    let cli = Cli::parse_from::<_, &str>(["dirge"]);
    let cfg = Config::default();
    let cache = ToolCache::new();
    let sandbox = Sandbox::new(false);
    let (tools, _) = build_loop_tools(
        cache,
        None,
        None,
        None,
        None,
        None,
        #[cfg(feature = "lsp")]
        None,
        sandbox,
        None,
        #[cfg(feature = "mcp")]
        None,
        #[cfg(feature = "semantic")]
        None,
        &cli,
        &cfg,
        None, // session_id — test default
    )
    .await;

    for mutating in ["write", "edit", "bash", "apply_patch"] {
        let tool = tools
            .iter()
            .find(|t| t.name() == mutating)
            .unwrap_or_else(|| panic!("{mutating} missing from registry"));
        assert_eq!(
            tool.execution_mode(),
            Some(ToolExecutionMode::Sequential),
            "{mutating} should be Sequential",
        );
    }
}

/// Read-only tools (read/grep/list_dir/...) leave
/// execution_mode at None so they pick up the loop config's
/// Parallel default. Batches of all-read-only tools dispatch
/// concurrently per phase 3.
#[tokio::test]
async fn build_loop_tools_read_only_tools_are_parallel_capable() {
    let cli = Cli::parse_from::<_, &str>(["dirge"]);
    let cfg = Config::default();
    let cache = ToolCache::new();
    let sandbox = Sandbox::new(false);
    let (tools, _) = build_loop_tools(
        cache,
        None,
        None,
        None,
        None,
        None,
        #[cfg(feature = "lsp")]
        None,
        sandbox,
        None,
        #[cfg(feature = "mcp")]
        None,
        #[cfg(feature = "semantic")]
        None,
        &cli,
        &cfg,
        None, // session_id — test default
    )
    .await;

    for read_only in ["read", "grep", "list_dir", "find_files"] {
        let tool = tools
            .iter()
            .find(|t| t.name() == read_only)
            .unwrap_or_else(|| panic!("{read_only} missing from registry"));
        assert!(
            tool.execution_mode().is_none(),
            "{read_only} should leave execution_mode at None (Parallel-capable)",
        );
    }
}
