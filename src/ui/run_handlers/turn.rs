//! `AgentEvent::TurnStart` / `TurnEnd` arms extracted from
//! `run_interactive`. Both are entirely plugin-gated — they drive the
//! per-turn token batcher and the `on-turn-start` / `on-turn-end` plugin
//! hooks — so the whole module is `cfg(feature = "plugin")`. Behavior is
//! identical to the inline code; pure refactor (dirge-4y4l).

use std::sync::{Arc, Mutex};

use crate::plugin::PluginManager;
use crate::ui::streaming::TokenBatcher;

/// Clears tool-hook slots (`harness-block` / `-mutate-input` /
/// `-replace-result`) so a turn hook can't bleed block/mutate/replace into
/// the next tool call — turn hooks bypass `dispatch_tool_hook`'s own clear.
fn clear_tool_hook_slots(mgr: &mut PluginManager) {
    let _ = mgr.eval(
        "(do (set harness-block nil) \
             (set harness-mutate-input nil) \
             (set harness-replace-result nil))",
    );
}

/// New turn: reset the per-turn batcher + accumulator (else `current_turn_text`
/// accumulates across turns and the tracked index drifts from the runner's),
/// record the runner's turn index, fire `on-turn-start`, clear hook slots.
pub(crate) fn handle_turn_start(
    plugin_manager: Option<&Arc<Mutex<PluginManager>>>,
    token_batcher: &mut TokenBatcher,
    current_turn_text: &mut String,
    current_turn_index: &mut u32,
    index: u32,
) {
    token_batcher.reset();
    current_turn_text.clear();
    *current_turn_index = index;
    if let Some(pm) = plugin_manager {
        let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
        let _ = mgr.dispatch("on-turn-start", &format!("@{{:index {}}}", index));
        clear_tool_hook_slots(&mut mgr);
    }
}

/// Turn end: flush any sub-threshold batched tokens as a final
/// `on-message-update`, fire `on-turn-end` with the full turn text, clear
/// hook slots.
pub(crate) fn handle_turn_end(
    plugin_manager: Option<&Arc<Mutex<PluginManager>>>,
    token_batcher: &mut TokenBatcher,
    current_turn_text: &str,
    index: u32,
) {
    if let Some(pm) = plugin_manager {
        // Flush tokens that didn't reach the batcher threshold so the final
        // partial update lands. `current_turn_text` already covers them
        // (pushed in lockstep with the batcher).
        if token_batcher.flush_remaining().is_some() {
            let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
            let _ = mgr.dispatch(
                "on-message-update",
                &format!(
                    "@{{:index {} :partial \"{}\"}}",
                    index,
                    crate::plugin::escape_janet_string(current_turn_text),
                ),
            );
        }
        let mut mgr = pm.lock().unwrap_or_else(|e| e.into_inner());
        let _ = mgr.dispatch(
            "on-turn-end",
            &format!(
                "@{{:index {} :message \"{}\"}}",
                index,
                crate::plugin::escape_janet_string(current_turn_text),
            ),
        );
        clear_tool_hook_slots(&mut mgr);
    }
}
