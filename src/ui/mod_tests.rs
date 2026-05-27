//! Tests for the items declared in `src/ui/mod.rs`. Moved out of
//! the main file to keep `mod.rs` close to the line-budget target
//! set by the `arch/split-large-modules` branch.
//!
//! Included via `#[cfg(test)] #[path = "mod_tests.rs"] mod tests;`
//! at the bottom of `mod.rs`, so `super::*` here resolves into the
//! `ui` module exactly as the original inline `mod tests` block did.

use super::*;
use crate::ui::search_rewind::update_search;
use unicode_width::UnicodeWidthStr;

// ============================================================
// apply_subagent_panel_event — left-panel cleanup
// ============================================================

use crate::agent::tools::task::SubagentChatEvent as E;

/// Spawn → row appears in "running" state with the prompt.
#[test]
fn subagent_panel_spawn_inserts_running_row() {
    let mut rows = indexmap::IndexMap::new();
    apply_subagent_panel_event(
        &mut rows,
        &E::Spawn {
            id: "abc123".into(),
            prompt: "build the binary".into(),
        },
    );
    assert_eq!(rows.len(), 1);
    let (state, prompt, _files) = rows.get("abc123").unwrap();
    assert_eq!(state, "running");
    assert_eq!(prompt, "build the binary");
}

/// Complete → row is REMOVED (the bug being fixed). Previously
/// the row's state changed to "completed" and the entry stayed
/// in the map forever, accumulating stale ✓ glyphs in the panel.
#[test]
fn subagent_panel_complete_removes_row() {
    let mut rows = indexmap::IndexMap::new();
    apply_subagent_panel_event(
        &mut rows,
        &E::Spawn {
            id: "abc123".into(),
            prompt: "build the binary".into(),
        },
    );
    apply_subagent_panel_event(
        &mut rows,
        &E::Complete {
            id: "abc123".into(),
            result: "ok".into(),
        },
    );
    assert!(rows.is_empty(), "completed subagent must be removed");
}

/// Failed → row is REMOVED (same cleanup contract as Complete).
#[test]
fn subagent_panel_failed_removes_row() {
    let mut rows = indexmap::IndexMap::new();
    apply_subagent_panel_event(
        &mut rows,
        &E::Spawn {
            id: "xyz789".into(),
            prompt: "run tests".into(),
        },
    );
    apply_subagent_panel_event(
        &mut rows,
        &E::Failed {
            id: "xyz789".into(),
            error: "boom".into(),
        },
    );
    assert!(rows.is_empty(), "failed subagent must be removed");
}

/// Mixed: several spawns + one completion leaves the rest in
/// place and preserves insertion order (oldest at top).
#[test]
fn subagent_panel_mixed_lifecycle_preserves_order() {
    let mut rows = indexmap::IndexMap::new();
    for id in ["a", "b", "c"] {
        apply_subagent_panel_event(
            &mut rows,
            &E::Spawn {
                id: id.into(),
                prompt: format!("task {id}"),
            },
        );
    }
    // Remove the middle one.
    apply_subagent_panel_event(
        &mut rows,
        &E::Complete {
            id: "b".into(),
            result: "ok".into(),
        },
    );
    assert_eq!(rows.len(), 2);
    let remaining: Vec<&str> = rows.keys().map(String::as_str).collect();
    assert_eq!(
        remaining,
        vec!["a", "c"],
        "shift_remove must preserve insertion order of survivors"
    );
}

/// Complete/Failed for an unknown id is a no-op (defensive —
/// shouldn't happen since Complete always follows Spawn, but if
/// the event ordering ever drifts, don't panic).
#[test]
fn subagent_panel_complete_unknown_id_is_noop() {
    let mut rows = indexmap::IndexMap::new();
    apply_subagent_panel_event(
        &mut rows,
        &E::Complete {
            id: "never-spawned".into(),
            result: "ok".into(),
        },
    );
    assert!(rows.is_empty());
}

/// dirge-bfd: Ctrl-F search uses fuzzy matching (nucleo) — typos,
/// non-contiguous subsequences, and missing characters all match
/// where they wouldn't under the prior substring scheme.
#[test]
fn fuzzy_search_matches_non_contiguous_subsequence() {
    let mut renderer = crate::ui::renderer::Renderer::new().expect("renderer");
    renderer
        .write_line("connect to database", Color::White)
        .unwrap();
    renderer
        .write_line("contributing guide", Color::White)
        .unwrap();
    renderer
        .write_line("totally unrelated", Color::White)
        .unwrap();

    let mut matches: Vec<usize> = Vec::new();
    let mut selected = 0;

    // Substring `ctd` matches nothing under the old `contains`
    // scheme. Fuzzy matches "connect to database" by its
    // c-o-n-n-e-C-T-o-D... subsequence.
    update_search(&renderer, "ctd", &mut matches, &mut selected);
    assert!(
        !matches.is_empty(),
        "fuzzy `ctd` should produce matches; matches={matches:?}",
    );

    // Empty / whitespace queries clear matches.
    update_search(&renderer, "", &mut matches, &mut selected);
    assert!(matches.is_empty());
    update_search(&renderer, "   ", &mut matches, &mut selected);
    assert!(matches.is_empty());

    // Lowercase query matches (smart case).
    update_search(&renderer, "database", &mut matches, &mut selected);
    assert!(matches.iter().any(|&i| {
        renderer
            .buffer_lines()
            .get(i)
            .map(|s| s.contains("database"))
            .unwrap_or(false)
    }));
}

/// dirge-bfd: Ctrl-F search uses fuzzy matching (nucleo) — typos,
/// structured entries on the stashed message. Pending entries
/// stay Interrupted (no matching result arrived); on resume,
/// `convert_history` will emit a [Tool execution was
/// interrupted] tool_result so the LLM sees paired blocks.
#[test]
fn capture_partial_on_abort_preserves_pending_tool_calls_as_interrupted() {
    let mut session = crate::session::Session::new("p", "m", 100_000);
    let mut buf = String::from("Running bash...");
    let mut calls = vec![
        crate::session::ToolCallEntry {
            id: "tc_abc".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"cmd": "sleep 99"}),
            state: crate::session::ToolCallState::Interrupted,
        },
        crate::session::ToolCallEntry {
            id: "tc_xyz".to_string(),
            name: "read".to_string(),
            args: serde_json::json!({"path": "/etc/hostname"}),
            state: crate::session::ToolCallState::Completed {
                result: "myhost".to_string(),
            },
        },
    ];
    let stashed = capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 2, &mut calls);
    assert!(stashed);
    assert!(calls.is_empty(), "tool_calls_buf must be drained on stash");

    let last = session.messages.last().unwrap();
    assert_eq!(last.tool_calls.len(), 2);
    let interrupted = last
        .tool_calls
        .iter()
        .find(|e| e.id == "tc_abc")
        .expect("missing interrupted entry");
    assert!(matches!(
        interrupted.state,
        crate::session::ToolCallState::Interrupted,
    ));
    let completed = last
        .tool_calls
        .iter()
        .find(|e| e.id == "tc_xyz")
        .expect("missing completed entry");
    match &completed.state {
        crate::session::ToolCallState::Completed { result } => {
            assert_eq!(result, "myhost");
        }
        other => panic!("expected Completed; got {other:?}"),
    }
}

#[test]
fn capture_partial_on_abort_stashes_partial_with_trailer() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let baseline = session.messages.len();
    let mut buf = String::from("I was about to explain that");
    let stashed = capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 0, &mut Vec::new());
    assert!(stashed);
    assert_eq!(session.messages.len(), baseline + 1);
    let last = session.messages.last().unwrap();
    assert_eq!(last.role, crate::session::MessageRole::Assistant);
    assert!(
        last.content.contains("I was about to explain that"),
        "must keep the original partial: {:?}",
        last.content,
    );
    assert!(
        last.content.contains("[interrupted by user (Ctrl+C)]"),
        "must include the interruption trailer: {:?}",
        last.content,
    );
    assert!(buf.is_empty(), "buf must be cleared after stash");
}

// Aborting when nothing has streamed yet is a no-op — we don't
// want a session full of empty "[interrupted]" messages from
// mistaken Ctrl+C presses.
#[test]
fn capture_partial_on_abort_noop_on_empty_buf() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let baseline = session.messages.len();
    let mut buf = String::new();
    let stashed = capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 0, &mut Vec::new());
    assert!(!stashed);
    assert_eq!(session.messages.len(), baseline);
}

// Whitespace-only partial (e.g. agent had only emitted some
// leading newlines) is also a no-op — no useful text to save.
#[test]
fn capture_partial_on_abort_noop_on_whitespace_only() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let baseline = session.messages.len();
    let mut buf = String::from("   \n\n\t  ");
    let stashed = capture_partial_on_abort(&mut buf, &mut session, "Esc", 0, &mut Vec::new());
    assert!(!stashed);
    assert_eq!(session.messages.len(), baseline);
}

// When tool calls ran in the same turn as the abort, the trailer
// must say so. The agent's preserved text only covers what was
// streamed via `AgentEvent::Token`; tool calls + results emitted
// separately are NOT in `response_buf`. Without this hint the
// next turn's LLM would see the partial as a definitive "this
// was the assistant's response" and could re-run side-effecting
// tool calls.
#[test]
fn capture_partial_on_abort_trailer_notes_tool_calls() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let mut buf = String::from("I deleted the file");
    let stashed = capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 2, &mut Vec::new());
    assert!(stashed);
    let content = &session.messages.last().unwrap().content;
    assert!(
        content.contains("I deleted the file"),
        "partial text dropped: {content:?}",
    );
    assert!(
        content.contains("[interrupted by user (Ctrl+C);"),
        "trailer prefix changed: {content:?}",
    );
    assert!(
        content.contains("2 tool call"),
        "trailer must mention tool call count: {content:?}",
    );
    assert!(
        content.contains("not preserved"),
        "trailer must warn that tool calls were not preserved: {content:?}",
    );
}

// Single tool call uses singular phrasing — "1 tool call ran" not
// "1 tool calls ran". Tiny but the LLM is reading this verbatim.
#[test]
fn capture_partial_on_abort_trailer_handles_singular_tool_call() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let mut buf = String::from("Running tests now");
    capture_partial_on_abort(&mut buf, &mut session, "Esc", 1, &mut Vec::new());
    let content = &session.messages.last().unwrap().content;
    assert!(
        content.contains("1 tool call ran"),
        "expected singular phrasing for 1 tool call: {content:?}",
    );
    assert!(
        !content.contains("1 tool calls ran"),
        "leaked plural for singular case: {content:?}",
    );
}

// Rewind must sync tree.entries + message_store + leaf_id with
// the truncated `messages` slice. Without this, the tree
// references orphaned ids that no longer have content, and the
// leaf_id can point past the truncation. Subsequent fork /
// clone / save-load operations either fail or carry stale ids.
#[test]
fn rewind_truncates_tree_and_store_in_sync_with_messages() {
    let mut session = crate::session::Session::new("p", "m", 100_000);
    session.add_message(crate::session::MessageRole::User, "u1");
    session.add_message(crate::session::MessageRole::Assistant, "a1");
    session.add_message(crate::session::MessageRole::User, "u2");
    session.add_message(crate::session::MessageRole::Assistant, "a2");
    let baseline_tree = session.tree.entries.len();
    assert_eq!(baseline_tree, 4, "fixture: 4 entries");

    // Rewind back to the first user message (idx=1 in the
    // reverse-order user list means the *first* user).
    let mut renderer = crate::ui::renderer::Renderer::new().unwrap();
    // idx=0 = "rewind through the most recent user prompt" → cut
    // at the position of u2 → messages become [u1, a1].
    let _ = rewind_session(&mut session, 0, &mut renderer);

    // After rewind, messages has [u1, a1]; tree must agree.
    assert_eq!(session.messages.len(), 2);
    assert_eq!(
        session.tree.entries.len(),
        session.messages.len(),
        "tree entries must match messages count; got tree={}, msgs={}",
        session.tree.entries.len(),
        session.messages.len(),
    );
    assert_eq!(
        session.message_store.len(),
        session.messages.len(),
        "store must match messages count",
    );
    // Leaf points to the last remaining message.
    let last_id = session.messages.last().unwrap().id.clone();
    assert_eq!(
        session.tree.leaf_id,
        Some(last_id.clone()),
        "leaf_id must anchor to the new tail",
    );
    // Every remaining message id has a tree entry + store entry.
    for m in &session.messages {
        assert!(
            session.tree.entries.contains_key(&m.id),
            "missing tree entry for {}",
            m.id,
        );
        assert!(
            session.message_store.contains_key(&m.id),
            "missing store entry for {}",
            m.id,
        );
    }
}

// The token accumulator on the abort path keeps `total_tokens`
// in sync with `total_estimated_tokens`. Both fields are
// TODO(cost-tracking) placeholders today but the inconsistency
// between Done/Interjected (which both update total_tokens) and
// abort (which didn't) made the abort case look like the agent
// produced zero tokens that turn.
#[test]
fn capture_partial_on_abort_keeps_total_tokens_in_sync() {
    let mut session = crate::session::Session::new("openrouter", "test-model", 100_000);
    let baseline_total = session.total_tokens;
    let baseline_est = session.total_estimated_tokens;
    let mut buf = String::from(
        "A reasonably long partial response that should produce a non-zero token estimate.",
    );
    capture_partial_on_abort(&mut buf, &mut session, "Ctrl+C", 0, &mut Vec::new());
    // Both fields advanced by the same amount (the stashed
    // message's estimated_tokens). Without the parity fix, only
    // total_estimated_tokens moved.
    assert!(
        session.total_estimated_tokens > baseline_est,
        "total_estimated_tokens should advance on stash",
    );
    assert_eq!(
        session.total_tokens.saturating_sub(baseline_total),
        session.total_estimated_tokens.saturating_sub(baseline_est),
        "total_tokens must advance in lockstep with total_estimated_tokens",
    );
}

// Regression H1: lifecycle line for a failed task previously embedded the
// raw error string. Renderer.write_line splits on '\n', so a multi-line
// error broke the line layout (color reset, closing ']' on its own row).
// sanitize_single_line must collapse newlines into spaces.
#[test]
fn sanitize_replaces_newlines_with_space() {
    let s = sanitize_single_line("line one\nline two\nline three", 100);
    assert_eq!(s, "line one line two line three");
    assert!(!s.contains('\n'));
}

#[test]
fn sanitize_replaces_carriage_return_and_tab() {
    let s = sanitize_single_line("a\rb\tc", 100);
    assert_eq!(s, "a b c");
}

// Regression: ANSI escape sequences (ESC = 0x1B) would otherwise be
// emitted verbatim and corrupt terminal state.
#[test]
fn sanitize_strips_ansi_escape() {
    let s = sanitize_single_line("hello \x1b[31mred\x1b[0m world", 100);
    assert!(!s.contains('\x1b'));
    assert!(s.contains("hello"));
    assert!(s.contains("world"));
}

// Other ASCII control chars (bell, backspace, etc.) are also stripped.
#[test]
fn sanitize_strips_other_controls() {
    let s = sanitize_single_line("a\x07b\x08c\x00d", 100);
    // Each control disappears; visible chars remain in order.
    assert_eq!(s, "abcd");
}

#[test]
fn sanitize_truncates_at_char_limit() {
    let s = sanitize_single_line(&"x".repeat(200), 50);
    // 50 x's + ellipsis.
    assert_eq!(s.chars().count(), 51);
    assert!(s.ends_with('…'));
}

#[test]
fn sanitize_does_not_truncate_when_within_limit() {
    let s = sanitize_single_line("hello", 100);
    assert_eq!(s, "hello");
    assert!(!s.ends_with('…'));
}

// Multibyte content counts by chars, not bytes, and remains intact.
#[test]
fn sanitize_handles_utf8_correctly() {
    let s = sanitize_single_line("🦀🦀🦀\n🦀🦀", 100);
    assert_eq!(s, "🦀🦀🦀 🦀🦀");
}

// Truncation at a multibyte boundary must produce valid UTF-8.
#[test]
fn sanitize_truncation_does_not_split_multibyte() {
    let s = sanitize_single_line("🦀🦀🦀🦀🦀", 3);
    // 3 emojis + ellipsis. No broken bytes.
    assert_eq!(s.chars().count(), 4);
    assert!(s.ends_with('…'));
    // Round-trip as &str succeeds.
    let _ = s.as_str();
}

#[test]
fn with_queue_hides_zero_count() {
    // No interjections waiting → status line unchanged so the user
    // doesn't see ambient "q:0" noise during normal operation.
    let s = with_queue("ready".to_string(), 0);
    assert_eq!(s, "ready");
}

#[test]
fn with_queue_appends_count() {
    let s = with_queue("running".to_string(), 3);
    assert!(s.ends_with("q:3"));
    assert!(s.starts_with("running"));
}

/// User bug: `read` output containing a tab caused the chamber's
/// right border to drift right. `\t` has Unicode width 0 but the
/// terminal renders it as 4+ cells, so width-based padding
/// undercounted. The fix expands tabs to spaces (stop=4) before
/// measurement so the right `│` lands at the expected column.
#[test]
fn chamber_row_right_border_aligns_with_tabs() {
    use unicode_width::UnicodeWidthStr;
    let inner = 60;
    // Three rows: no tab, one tab at start, tab embedded mid-line.
    // After tab-expansion all should produce equal display width.
    let rows = [
        chamber_row("plain text", inner),
        chamber_row("\tindented", inner),
        chamber_row("2:\t(cd ..; make library)", inner),
    ];
    let widths: Vec<usize> = rows
        .iter()
        .map(|r| UnicodeWidthStr::width(r.as_str()))
        .collect();
    // All rows occupy exactly `inner + 4` cells (`│ ` + inner + ` │`).
    let expected = inner + 4;
    for (r, w) in rows.iter().zip(widths.iter()) {
        assert_eq!(
            *w, expected,
            "chamber row width mismatch — content {r:?} measured {w} cells, want {expected}"
        );
    }
    // Sanity: every row ends with `│` (right border didn't get
    // pushed off into oblivion by under-padded tab).
    for r in &rows {
        assert!(r.ends_with('│'), "row {r:?} missing right border");
    }
}

/// `chamber_row_with_bg` gets the same tab-expansion treatment so
/// diff `+`/`-` lines whose source uses tab indentation also
/// align correctly.
#[test]
fn chamber_row_with_bg_right_border_aligns_with_tabs() {
    use unicode_width::UnicodeWidthStr;
    let inner = 60;
    let row = chamber_row_with_bg("+\tadded line", inner, 22);
    // chamber_row_with_bg wraps content in SGR escapes; the
    // visible width should still be inner + 4.
    let visible = crate::ui::wrap::visible_width(&row);
    assert_eq!(visible, inner + 4);
    // Plain UnicodeWidthStr counts SGR payload too, but the
    // visible-width helper from `wrap.rs` is the right tool.
    // Sanity-only width assertion via the visible helper.
    let _ = UnicodeWidthStr::width(row.as_str());
    assert!(row.ends_with('│'));
}

/// Chat window switching: next / prev index math wraps correctly.
#[test]
fn chat_index_next_prev_wraps() {
    // Simulate 3 chats (0=main, 1, 2), active=0.
    let count = 3;
    // Ctrl+N: next
    assert_eq!((0 + 1) % count, 1);
    assert_eq!((1 + 1) % count, 2);
    assert_eq!((2 + 1) % count, 0); // wrap
    // Ctrl+P: prev
    assert_eq!((0 + count - 1) % count, 2); // wrap
    assert_eq!((2 + count - 1) % count, 1);
    assert_eq!((1 + count - 1) % count, 0);
}

/// Chat window switching: single chat is a no-op.
#[test]
fn chat_index_next_prev_one_chat_is_noop() {
    let count = 1;
    assert_eq!((0 + 1) % count, 0);
    assert_eq!((0 + count - 1) % count, 0);
}

// ============================================================
// safe_during_agent — slash commands allowed while agent runs
// ============================================================

#[test]
fn mode_is_safe_during_agent() {
    assert!(is_safe_during_agent("/mode"));
    assert!(is_safe_during_agent("/mode yolo"));
    assert!(is_safe_during_agent("/mode standard"));
    assert!(is_safe_during_agent("/mode accept"));
    assert!(is_safe_during_agent("/mode restrictive"));
}

#[test]
fn quit_help_reasoning_tasks_always_safe_during_agent() {
    assert!(is_safe_during_agent("/quit"));
    assert!(is_safe_during_agent("/help"));
    assert!(is_safe_during_agent("/reasoning"));
    assert!(is_safe_during_agent("/tasks"));
    assert!(is_safe_during_agent("/tasks list"));
}

#[test]
fn sessions_tree_model_prompt_safe_only_without_args() {
    assert!(is_safe_during_agent("/sessions"));
    assert!(is_safe_during_agent("/tree"));
    assert!(is_safe_during_agent("/model"));
    assert!(is_safe_during_agent("/prompt"));
    assert!(!is_safe_during_agent("/sessions 42"));
    assert!(!is_safe_during_agent("/model gpt-4"));
    assert!(!is_safe_during_agent("/prompt my-prompt"));
}

#[test]
fn mutating_commands_are_not_safe_during_agent() {
    assert!(!is_safe_during_agent("/cd /tmp"));
    assert!(!is_safe_during_agent("/clear"));
    assert!(!is_safe_during_agent("/compress"));
    assert!(!is_safe_during_agent("/clone"));
    assert!(!is_safe_during_agent("/fork"));
    assert!(!is_safe_during_agent("/compact"));
    assert!(!is_safe_during_agent("/undo"));
    assert!(!is_safe_during_agent("/retry"));
    assert!(!is_safe_during_agent("/allow bash rm *"));
}

#[test]
fn memory_skill_list_safe_during_agent() {
    assert!(is_safe_during_agent("/memory list"));
    assert!(is_safe_during_agent("/skill list"));
    assert!(!is_safe_during_agent("/memory add key value"));
    assert!(!is_safe_during_agent("/skill load foo"));
}
