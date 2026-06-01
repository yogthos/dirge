//! Preamble / system-prompt assembly helpers for the agent builder.
//! Split out of `agent/builder.rs` (dirge-4y4l stage 11a): the small,
//! independently-testable text-building helpers that `build_agent_inner`
//! layers into the system prompt.

use crate::agent::model_family::ModelFamily;
use crate::agent::prompt::{
    DEEPSEEK_GUIDANCE, MEMORY_GUIDANCE, SESSION_SEARCH_GUIDANCE, SKILLS_GUIDANCE, SYSTEM_PROMPT,
    TODO_TOOLS_PROMPT,
};

/// Append a memory provider's prompt block to the assembled preamble.
/// Goes through `MemoryProvider::format_for_system_prompt`
/// (trait-dispatched) so a non-default backend's block lands in the
/// preamble too — pre-fix `builder.rs` called the concrete
/// `MemoryToolStore::format_for_system_prompt` directly, which broke
/// any future plugin provider's prompt contribution. See dirge-fmau.
pub(crate) fn append_memory_to_preamble(
    preamble: &mut String,
    provider: &std::sync::Arc<dyn crate::extras::memory_provider::MemoryProvider>,
) {
    tracing::debug!(
        target: "dirge::memory",
        provider = provider.name(),
        "Injecting memory provider prompt block"
    );
    let block = provider.format_for_system_prompt();
    if !block.is_empty() {
        preamble.push_str(&block);
    }
}

/// Assemble the always-on base preamble — `SYSTEM_PROMPT`,
/// `TODO_TOOLS_PROMPT`, and the in-session `SKILLS_GUIDANCE`
/// (dirge-xxun, mirroring hermes `SKILLS_GUIDANCE`). Other contextual
/// blocks (AGENTS.md, prompts, project skills, memory) are layered on
/// top by `build_agent_inner`. Extracted so the assembly is testable
/// without exercising the full DI signature.
pub(crate) fn assemble_base_preamble() -> String {
    let mut p = SYSTEM_PROMPT.to_string();
    p.push('\n');
    p.push_str(TODO_TOOLS_PROMPT);
    // dirge-xxun: skills self-improvement nudge (hermes SKILLS_GUIDANCE).
    p.push_str(SKILLS_GUIDANCE);
    // dirge-a6bv: memory + past-session recall guidance (hermes
    // MEMORY_GUIDANCE + SESSION_SEARCH_GUIDANCE). Both tools are always
    // present in dirge's registry, so we inject unconditionally rather
    // than tool-gating like hermes does on `valid_tool_names`.
    p.push_str(MEMORY_GUIDANCE);
    p.push_str(SESSION_SEARCH_GUIDANCE);
    p
}

/// Model-specific steering fragment to append to the preamble, if any.
///
/// Returns the DeepSeek guidance for DeepSeek **chat** models and `None`
/// for everything else (other vendors, and the DeepSeek reasoner, which
/// ignores the system prompt). Appended last by `build_agent_inner` so it
/// sits closest to the conversation / action boundary — research shows
/// rules stated far from the decision point lose influence in long
/// tool-calling loops ("prompt-distance drift").
pub(crate) fn model_steering_fragment(family: ModelFamily) -> Option<&'static str> {
    if family.is_deepseek_chat() {
        Some(DEEPSEEK_GUIDANCE)
    } else {
        None
    }
}

/// Append a mode-specific reminder to `preamble` based on the active prompt
/// name. `plan_exists` reports whether `PLAN.md` is present in CWD — only
/// consulted for the `code` mode reminder. Unknown prompt names produce no
/// reminder so custom prompts don't accidentally pick up plan/review semantics.
pub(crate) fn append_mode_reminder(preamble: &mut String, prompt_name: &str, plan_exists: bool) {
    match prompt_name {
        "plan" => {
            preamble.push_str("\n\n---\n\nYou are now in PLAN mode. Create a detailed implementation plan. Save it to PLAN.md in the current directory. Analyze the task, break it into concrete steps, consider edge cases and trade-offs. Do NOT write any code or run any commands until the user reviews and approves the plan.");
        }
        "review" | "review-security" => {
            preamble.push_str("\n\n---\n\nYou are now in REVIEW mode. Review the code or plan carefully. Identify bugs, security issues, performance problems, and design flaws. Be thorough and specific. Provide actionable feedback.");
        }
        "code" if plan_exists => {
            preamble.push_str(
                "\n\n---\n\nA plan file exists at PLAN.md. Execute the plan step by step. Write and test code following the plan. Report progress after each step. The plan is your guide — follow it closely.",
            );
        }
        _ => {}
    }
}
