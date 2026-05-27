use super::*;

/// wrap_editor: empty buffer → one empty row, cursor at (0, 0).
#[test]
fn wrap_editor_empty() {
    let (rows, r, c) = wrap_editor("", 0, 80);
    assert_eq!(rows, vec![String::new()]);
    assert_eq!((r, c), (0, 0));
}

/// wrap_editor: short single-line text doesn't wrap.
#[test]
fn wrap_editor_no_wrap_short() {
    let (rows, r, c) = wrap_editor("hello", 5, 80);
    assert_eq!(rows, vec!["hello".to_string()]);
    assert_eq!((r, c), (0, 5));
}

/// wrap_editor: hard newlines split into logical rows.
#[test]
fn wrap_editor_newlines_split() {
    let (rows, r, c) = wrap_editor("a\nb\ncc", 5, 80);
    assert_eq!(
        rows,
        vec!["a".to_string(), "b".to_string(), "cc".to_string()]
    );
    // Cursor at byte 5 = "cc" position 1.
    assert_eq!((r, c), (2, 1));
}

/// wrap_editor: long line soft-wraps to wrap_w cells. Cursor
/// lands on the wrapped row.
#[test]
fn wrap_editor_soft_wrap() {
    let s = "abcdefghij"; // 10 chars
    let (rows, r, c) = wrap_editor(s, 10, 4);
    // Wrap to 4 cells: ["abcd", "efgh", "ij"] (cursor at end).
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], "abcd");
    assert_eq!(rows[1], "efgh");
    assert_eq!(rows[2], "ij");
    assert_eq!((r, c), (2, 2));
}

/// dirge-ov2 Phase A: chat switching saves the prior chat's
/// buffer and selection, then loads the target chat's snapshot.
/// Round-trip preserves content.
#[test]
fn chat_snapshot_save_load_roundtrip() {
    let mut r = Renderer::new().expect("renderer");
    // Default chat is "main" at index 0.
    assert_eq!(r.active_chat(), 0);
    assert_eq!(r.chat_count(), 1);
    assert_eq!(r.chat_names(), vec!["main".to_string()]);

    // Seed main chat with some content.
    r.buffer.push(LineEntry {
        text: CompactString::new("main-line-1"),
        color: Color::White,
    });
    r.scroll_offset = 5;

    // Spawn a subagent chat and switch to it.
    let sub_idx = r.add_chat("subagent-1");
    assert_eq!(sub_idx, 1);
    assert_eq!(r.chat_count(), 2);
    r.switch_chat(sub_idx);
    assert_eq!(r.active_chat(), 1);

    // Subagent chat starts empty.
    assert!(r.buffer.is_empty());
    assert_eq!(r.scroll_offset, 0);

    // Add content to the subagent chat.
    r.buffer.push(LineEntry {
        text: CompactString::new("sub-line-1"),
        color: Color::Cyan,
    });
    r.scroll_offset = 2;

    // Switch back to main — its content must be restored.
    r.switch_chat(0);
    assert_eq!(r.buffer.len(), 1);
    assert_eq!(r.buffer[0].text.as_str(), "main-line-1");
    assert_eq!(r.scroll_offset, 5);

    // Switch back to subagent — its content also restored.
    r.switch_chat(1);
    assert_eq!(r.buffer.len(), 1);
    assert_eq!(r.buffer[0].text.as_str(), "sub-line-1");
    assert_eq!(r.scroll_offset, 2);

    // Switch to same chat is a no-op.
    r.switch_chat(1);
    assert_eq!(r.buffer.len(), 1);

    // Out-of-range index is a no-op (defensive — caller bug).
    r.switch_chat(99);
    assert_eq!(r.active_chat(), 1);
}

/// next_chat wraps around from last → first.
#[test]
fn next_chat_cycles_forward_with_wrap() {
    let mut r = Renderer::new().expect("renderer");
    r.add_chat("one");
    r.add_chat("two");
    assert_eq!(r.chat_count(), 3); // main + one + two
    assert_eq!(r.active_chat(), 0);
    r.next_chat();
    assert_eq!(r.active_chat(), 1);
    r.next_chat();
    assert_eq!(r.active_chat(), 2);
    r.next_chat(); // wrap
    assert_eq!(r.active_chat(), 0);
}

/// prev_chat wraps around from first → last.
#[test]
fn prev_chat_cycles_backward_with_wrap() {
    let mut r = Renderer::new().expect("renderer");
    r.add_chat("one");
    r.add_chat("two");
    assert_eq!(r.chat_count(), 3);
    // prev from 0 wraps to 2
    r.prev_chat();
    assert_eq!(r.active_chat(), 2);
    r.prev_chat();
    assert_eq!(r.active_chat(), 1);
    r.prev_chat();
    assert_eq!(r.active_chat(), 0);
}

/// next/prev are no-ops with only one chat.
#[test]
fn next_prev_noop_with_single_chat() {
    let mut r = Renderer::new().expect("renderer");
    assert_eq!(r.chat_count(), 1);
    r.next_chat();
    assert_eq!(r.active_chat(), 0);
    r.prev_chat();
    assert_eq!(r.active_chat(), 0);
}

/// remove_chat removes a chat and adjusts active_chat.
#[test]
fn remove_chat_adjusts_active() {
    let mut r = Renderer::new().expect("renderer");
    r.add_chat("one");
    r.add_chat("two");
    r.add_chat("three");
    // chats: [main, one, two, three], active=0
    r.switch_chat(2); // active = "two"
    assert_eq!(r.active_chat(), 2);
    // Remove chat 1 ("one") — active stays 2 but now points
    // to what WAS chat 2 (now shifted to index 1).
    r.remove_chat(1);
    assert_eq!(r.chat_count(), 3);
    assert_eq!(r.active_chat(), 1); // shifted down
    // Remove active chat — moves to next (or last if at end).
    r.switch_chat(2); // active = last chat ("three")
    r.remove_chat(2);
    assert_eq!(r.active_chat(), 0); // wraps to 0

    // Cannot remove the last remaining chat.
    let mut r2 = Renderer::new().expect("renderer");
    r2.remove_chat(0);
    assert_eq!(r2.chat_count(), 1);
    assert_eq!(r2.active_chat(), 0);
}

/// Create a renderer with a synthetic buffer of `n` short lines so we
/// can drive scroll/append behavior without touching a real terminal.
/// If `n` is less than `visible + min_scroll_margin`, pads to that size
/// so scroll_line_up actually has room to scroll regardless of terminal
/// height. Pass `min_scroll_margin: 15` for typical tests that need 10
/// scroll-up presses.
fn fresh_with_lines_scrollable(n: usize, min_scroll_margin: usize) -> Renderer {
    let mut r = Renderer::new().expect("renderer");
    let visible = r.visible_lines();
    let need = (visible + min_scroll_margin).max(n);
    for i in 0..need {
        r.buffer.push(LineEntry {
            text: CompactString::new(&format!("line {i}")),
            color: Color::White,
        });
    }
    r.lines = r.buffer.len() as u16;
    r
}

/// Create a renderer with a synthetic buffer of `n` short lines so we
/// can drive scroll/append behavior without touching a real terminal.
fn fresh_with_lines(n: usize) -> Renderer {
    fresh_with_lines_scrollable(n, /* min_scroll_margin */ 15)
}

/// Absolute index of the first visible line in the current viewport,
/// matching the formula used by `render_viewport`.
fn view_start(r: &Renderer) -> usize {
    let visible = r.visible_lines();
    let total = r.buffer.len();
    let start = if r.scroll_offset == 0 {
        total.saturating_sub(visible)
    } else {
        total.saturating_sub(r.scroll_offset + visible)
    };
    start.min(total.saturating_sub(visible))
}

// Regression: previously, when the user scrolled up while output was
// streaming, scroll_offset stayed fixed but the buffer grew — so the
// viewport drifted forward into newer content. The fix bumps
// scroll_offset by one per appended line so the view stays anchored to
// the same absolute lines.
#[test]
fn regression_scrolled_up_view_stays_anchored_through_appends() {
    let mut r = fresh_with_lines(50);
    // Scroll up 10 lines. View start changes; record it.
    for _ in 0..10 {
        r.scroll_line_up();
    }
    let pinned_start = view_start(&r);

    // Stream in 8 new lines while the user is scrolled up.
    for i in 0..8 {
        r.push_buffer_line(LineEntry {
            text: CompactString::new(&format!("new {i}")),
            color: Color::White,
        });
    }

    // The first visible line index hasn't moved.
    assert_eq!(view_start(&r), pinned_start);
}

// Regression: replace_from (used by the streaming-token markdown path)
// also has to honor the scroll anchor. If the agent's current response
// grows (or shrinks) while the user is scrolled up viewing earlier
// content, the earlier content must stay in view.
#[test]
fn regression_replace_from_keeps_view_anchored_when_scrolled_up() {
    // Build a buffer with enough lines that scrolling into the
    // middle actually works regardless of terminal height.
    let mut r = fresh_with_lines_scrollable(50, /* margin */ 15);
    for _ in 0..10 {
        r.scroll_line_up();
    }
    let pinned_start = view_start(&r);

    // Replace the tail of the buffer (last 10 lines) with twice
    // as many — simulates a streaming markdown re-render that
    // grew the current response. The user is scrolled above the
    // replaced region, so the view must stay anchored.
    let total = r.buffer.len();
    let repl_start = total.saturating_sub(10);
    let new_lines: Vec<LineEntry> = (0..20)
        .map(|i| LineEntry {
            text: CompactString::new(&format!("repl {i}")),
            color: Color::White,
        })
        .collect();
    r.replace_from(repl_start, new_lines);

    assert_eq!(
        view_start(&r),
        pinned_start,
        "view drifted after replace-with-more"
    );

    // Now replace with FEWER lines (response got shorter via
    // re-render). The view should not drift upward past where
    // the user originally was.
    let total = r.buffer.len();
    let repl_start = total.saturating_sub(8);
    let shorter: Vec<LineEntry> = (0..3)
        .map(|i| LineEntry {
            text: CompactString::new(&format!("sh {i}")),
            color: Color::White,
        })
        .collect();
    r.replace_from(repl_start, shorter);
    let after = view_start(&r);
    assert!(
        after <= pinned_start,
        "view drifted upward: after={after} pinned_start={pinned_start}",
    );
}

// When the user is AT the bottom (scroll_offset == 0), new content must
// be visible — the view follows the bottom. The anchor behavior must not
// accidentally pin the bottom-anchored view.
#[test]
fn at_bottom_view_follows_new_content() {
    let mut r = fresh_with_lines(50);
    assert_eq!(r.scroll_offset, 0);

    for i in 0..5 {
        r.push_buffer_line(LineEntry {
            text: CompactString::new(&format!("new {i}")),
            color: Color::White,
        });
    }
    assert_eq!(r.scroll_offset, 0, "bottom-anchored view must stay at 0");

    let visible = r.visible_lines();
    let total = r.buffer.len();
    assert_eq!(view_start(&r), total.saturating_sub(visible));
}

// Selection indices are absolute and must NOT shift when content
// streams in. Prior to the anchor fix the selection rectangle visually
// drifted because scroll_offset stayed put while the viewport advanced;
// now the indices are still preserved and the viewport stays anchored,
// so the selection rectangle stays where the user dragged it.
#[test]
fn selection_indices_stay_absolute_under_streaming_appends() {
    let mut r = fresh_with_lines(50);
    for _ in 0..10 {
        r.scroll_line_up();
    }
    r.selection_active = true;
    r.selection_start = Some((15, 0));
    r.selection_end = Some((20, 5));

    for i in 0..7 {
        r.push_buffer_line(LineEntry {
            text: CompactString::new(&format!("new {i}")),
            color: Color::White,
        });
    }

    // Selection indices are absolute and remain untouched.
    assert_eq!(r.selection_start, Some((15, 0)));
    assert_eq!(r.selection_end, Some((20, 5)));
}

// Boundary: a tiny buffer where appending pushes scroll_offset past
// max_offset. The clamp inside push_buffer_line keeps it in range.
#[test]
fn push_clamps_scroll_offset_to_max_when_buffer_grows() {
    let mut r = fresh_with_lines(2);
    let visible = r.visible_lines();
    // Force a non-zero offset (clamp may already prevent it on tiny
    // buffers; assert behavior either way).
    r.scroll_offset = 100;
    for _ in 0..3 {
        r.push_buffer_line(LineEntry {
            text: CompactString::new("more"),
            color: Color::White,
        });
    }
    let max_offset = r.buffer.len().saturating_sub(visible);
    assert!(
        r.scroll_offset <= max_offset,
        "scroll_offset {} must be ≤ max {}",
        r.scroll_offset,
        max_offset
    );
}

// Streaming via commit_partial (the path used by `write` for streamed
// tokens) also goes through push_buffer_line. Verify the partial commit
// bumps the offset when scrolled up.
#[test]
fn commit_partial_routes_through_anchor_aware_push() {
    let mut r = fresh_with_lines(50);
    for _ in 0..10 {
        r.scroll_line_up();
    }
    let pinned_start = view_start(&r);

    r.partial = CompactString::new("a streamed token chunk");
    r.partial_color = Color::White;
    r.commit_partial();

    assert_eq!(view_start(&r), pinned_start);
}

// --- granular selection ----------------------------------------------

fn fresh_with_text(lines: &[&str]) -> Renderer {
    let mut r = Renderer::new().unwrap();
    for s in lines {
        r.buffer.push(LineEntry {
            text: CompactString::new(s),
            color: Color::White,
        });
    }
    r
}

/// Same-row selection extracts the substring between start.1 and
/// end.1 (char-indexed, exclusive end).
#[test]
fn selected_text_single_row_substring() {
    let mut r = fresh_with_text(&["hello world"]);
    r.selection_active = true;
    r.selection_start = Some((0, 6));
    r.selection_end = Some((0, 11));
    assert_eq!(r.selected_text(), Some("world".to_string()));
}

/// Reverse drag (end before start) still yields the same substring —
/// `selected_text` normalizes to row-major order.
#[test]
fn selected_text_reverse_drag_normalizes() {
    let mut r = fresh_with_text(&["hello world"]);
    r.selection_active = true;
    r.selection_start = Some((0, 11));
    r.selection_end = Some((0, 6));
    assert_eq!(r.selected_text(), Some("world".to_string()));
}

/// Multi-row selection takes the tail of the start row, the full
/// middle rows, and the head of the end row.
#[test]
fn selected_text_multi_row_spans_lines() {
    let mut r = fresh_with_text(&["first line", "middle", "last line"]);
    r.selection_active = true;
    r.selection_start = Some((0, 6)); // "line"
    r.selection_end = Some((2, 4)); // "last"
    assert_eq!(r.selected_text(), Some("line\nmiddle\nlast".to_string()));
}

/// Same-row empty selection (start == end) returns None — nothing
/// selected yet, just a click.
#[test]
fn selected_text_empty_selection_returns_none() {
    let mut r = fresh_with_text(&["hello"]);
    r.selection_active = true;
    r.selection_start = Some((0, 3));
    r.selection_end = Some((0, 3));
    assert!(r.selected_text().is_none());
}

/// Multi-byte UTF-8: char indices ignore byte width. `é` and `🦀`
/// each count as 1 char, not their byte widths.
#[test]
fn selected_text_handles_unicode() {
    let mut r = fresh_with_text(&["café 🦀 rust"]);
    r.selection_active = true;
    r.selection_start = Some((0, 0));
    r.selection_end = Some((0, 6)); // "café 🦀"
    assert_eq!(r.selected_text(), Some("café 🦀".to_string()));
}

/// Markdown rendering bakes SGR escapes into LineEntry::text;
/// the selection path must strip them before handing the
/// string to the clipboard. Columns reflect user-perceived
/// character offsets in the visible glyphs, not the
/// escape-laden source.
#[test]
fn selected_text_strips_ansi_escapes() {
    // Visible text is "hello red world" (15 chars). The buffer
    // line carries `\x1b[31m` around "red".
    let mut r = fresh_with_text(&[]);
    r.buffer.clear();
    r.buffer.push(LineEntry {
        text: CompactString::from("hello \x1b[31mred\x1b[0m world"),
        color: Color::Reset,
    });
    r.selection_active = true;
    // Select the full visible content (cols 0..15).
    r.selection_start = Some((0, 0));
    r.selection_end = Some((0, 15));
    assert_eq!(r.selected_text(), Some("hello red world".to_string()));

    // Substring selection lands on clean chars too —
    // "red world" is cols 6..15 of the stripped text.
    r.selection_end = Some((0, 15));
    r.selection_start = Some((0, 6));
    assert_eq!(r.selected_text(), Some("red world".to_string()));
}

/// `buffer_pos_at` clamps char_col to the line's length so dragging
/// past the right edge anchors at end-of-line rather than
/// silently extending past visible content.
#[test]
fn buffer_pos_at_clamps_past_eol() {
    let r = fresh_with_text(&["short"]);
    // Row 0 is the chat top frame in the ui-redesign; row 1 is
    // the first chat content row. `buffer_line_at_row` returns
    // Some(0) for row 1 (start = 0 after saturating, idx = 0).
    let pos = r.buffer_pos_at(1, 999);
    assert_eq!(pos, Some((0, 5)));
}

// --- B3-8: display-width-aware column mapping --------------

#[test]
fn display_col_to_char_index_ascii_round_trip() {
    // ASCII: 1 char = 1 display cell. char_index == display_col.
    assert_eq!(display_col_to_char_index("hello", 0), 0);
    assert_eq!(display_col_to_char_index("hello", 3), 3);
    assert_eq!(display_col_to_char_index("hello", 5), 5);
    // Past EOL clamps to char count.
    assert_eq!(display_col_to_char_index("hello", 99), 5);
}

#[test]
fn display_col_to_char_index_cjk_compresses() {
    // "日本" — 2 chars, 4 display cells.
    let s = "日本";
    assert_eq!(display_col_to_char_index(s, 0), 0);
    // Display col 1: middle of 日 — anchor to its start (char 0).
    assert_eq!(display_col_to_char_index(s, 1), 0);
    assert_eq!(display_col_to_char_index(s, 2), 1); // start of 本
    assert_eq!(display_col_to_char_index(s, 3), 1); // middle of 本
    assert_eq!(display_col_to_char_index(s, 4), 2); // EOL
    assert_eq!(display_col_to_char_index(s, 99), 2);
}

#[test]
fn display_col_to_char_index_emoji() {
    // "a🦀b" — 3 chars, widths 1 + 2 + 1 = 4 cells.
    let s = "a🦀b";
    assert_eq!(display_col_to_char_index(s, 0), 0); // start
    assert_eq!(display_col_to_char_index(s, 1), 1); // start of 🦀
    assert_eq!(display_col_to_char_index(s, 2), 1); // middle of 🦀
    assert_eq!(display_col_to_char_index(s, 3), 2); // start of b
    assert_eq!(display_col_to_char_index(s, 4), 3); // EOL
}

/// L-R3: buffer_pos_at clamps to VISIBLE char count (post ANSI
/// strip) not raw char count. Without this, a click far right
/// on a styled line would clamp past the visible-text length
/// and selected_text's slice would either return an empty
/// string or land in the middle of the escape bytes.
#[test]
fn buffer_pos_at_clamps_to_visible_chars_not_raw_bytes() {
    let mut r = fresh_with_text(&[]);
    r.buffer.clear();
    // Visible: "hello red world" — 15 chars. Raw: 25 chars
    // (including 10 chars of `\x1b[31m` + `\x1b[0m` escape).
    r.buffer.push(LineEntry {
        text: CompactString::from("hello \x1b[31mred\x1b[0m world"),
        color: Color::Reset,
    });
    // Click well past the visible end. content_indent() is 0
    // in the default test renderer, so col == char_col. Row 1
    // is the first chat content row (row 0 is the chat frame).
    let pos = r.buffer_pos_at(1, 999).expect("must resolve");
    assert_eq!(pos.1, 15, "clamp should hit visible length 15, not raw 25");
}

// --- wrap_input -------------------------------------------------------

fn lines(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

#[test]
fn wrap_empty_buffer_has_one_row() {
    let (rows, cr, cc) = wrap_input(&lines(&[""]), 0, 0, 10);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].logical_line, 0);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 0));
    assert_eq!((cr, cc), (0, 0));
}

#[test]
fn wrap_short_line_no_split() {
    let (rows, cr, cc) = wrap_input(&lines(&["hi"]), 0, 2, 10);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 2));
    assert_eq!((cr, cc), (0, 2));
}

#[test]
fn wrap_splits_long_line_into_multiple_visual_rows() {
    // "abcdefghi" with wrap_width=3 -> 3 rows of 3 chars each.
    let (rows, cr, cc) = wrap_input(&lines(&["abcdefghi"]), 0, 5, 3);
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 3));
    assert_eq!((rows[1].char_start, rows[1].char_end), (3, 6));
    assert_eq!((rows[2].char_start, rows[2].char_end), (6, 9));
    // cursor at col 5 -> row 1, col 2
    assert_eq!((cr, cc), (1, 2));
}

#[test]
fn wrap_cursor_at_exact_boundary_stays_on_filled_row() {
    // "abc" with wrap_width=3 — cursor at col 3 (end of line). Should
    // sit at the right edge of the only row, not on a phantom row 1.
    let (rows, cr, cc) = wrap_input(&lines(&["abc"]), 0, 3, 3);
    assert_eq!(rows.len(), 1);
    assert_eq!((cr, cc), (0, 3));
}

#[test]
fn wrap_cursor_after_full_row_with_continuation() {
    // "abcdef" with wrap_width=3 — cursor at col 6 (end). Two rows, cursor
    // at end of row 1 (col 3), not at start of phantom row 2.
    let (rows, cr, cc) = wrap_input(&lines(&["abcdef"]), 0, 6, 3);
    assert_eq!(rows.len(), 2);
    assert_eq!((cr, cc), (1, 3));
}

#[test]
fn wrap_cursor_at_start_of_continuation_row() {
    // "abcdef" with wrap_width=3 — cursor at col 3 (just past first row).
    // Not the exact-boundary "at end of line" case: chars continue.
    let (rows, cr, cc) = wrap_input(&lines(&["abcdef"]), 0, 3, 3);
    assert_eq!(rows.len(), 2);
    assert_eq!((cr, cc), (1, 0));
}

#[test]
fn wrap_multiple_logical_lines() {
    // Two logical lines, second one has the cursor.
    let (rows, cr, cc) = wrap_input(&lines(&["abc", "defgh"]), 1, 4, 3);
    // Line 0: 1 row (3 chars); Line 1: 2 rows (3 + 2)
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].logical_line, 0);
    assert_eq!(rows[1].logical_line, 1);
    assert_eq!(rows[2].logical_line, 1);
    // Cursor at line 1, col 4 -> within line 1's row 1 (visual row 2 overall), col 1
    assert_eq!((cr, cc), (2, 1));
}

#[test]
fn wrap_empty_then_filled_line_cursor_on_empty() {
    // ["", "abc"] with cursor on line 0 at col 0.
    let (rows, cr, cc) = wrap_input(&lines(&["", "abc"]), 0, 0, 3);
    // Line 0: 1 (empty) row; Line 1: 1 row of "abc"
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 0));
    assert_eq!((rows[1].char_start, rows[1].char_end), (0, 3));
    assert_eq!((cr, cc), (0, 0));
}

#[test]
fn wrap_width_one_degenerate() {
    // wrap_width=1 in extremely narrow terminal — every char becomes its
    // own row. Should not panic and cursor should still map.
    let (rows, cr, cc) = wrap_input(&lines(&["abc"]), 0, 2, 1);
    assert_eq!(rows.len(), 3);
    assert_eq!((cr, cc), (2, 0));
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn terminal_title_idle_and_done_show_simple_title() {
    use crate::ui::avatar::AvatarState;
    let t = super::format_terminal_title(AvatarState::Idle, None);
    assert_eq!(t, "● dirge");
    let t = super::format_terminal_title(AvatarState::Done, Some("bash"));
    assert_eq!(t, "● dirge");
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn terminal_title_shows_tool_name_for_working_states() {
    use crate::ui::avatar::AvatarState;
    let t = super::format_terminal_title(AvatarState::Reading, Some("grep"));
    assert!(t.contains("grep"), "title should contain tool name: {t:?}");
    assert!(
        t.contains("◌"),
        "working states should use yellow dot marker: {t:?}"
    );
    let t = super::format_terminal_title(AvatarState::Writing, Some("edit"));
    assert!(t.contains("edit"), "title should contain tool name: {t:?}");
    let t = super::format_terminal_title(AvatarState::Bash, Some("bash"));
    assert!(t.contains("bash"), "title should contain tool name: {t:?}");
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn terminal_title_error_and_alert_show_warning_marker() {
    use crate::ui::avatar::AvatarState;
    let t = super::format_terminal_title(AvatarState::Error, None);
    assert!(t.contains("ERROR"));
    assert!(
        t.contains("✗"),
        "error states should use red dot marker: {t:?}"
    );
    let t = super::format_terminal_title(AvatarState::Alert, None);
    assert!(t.contains("needs input"));
    assert!(
        t.contains("✗"),
        "alert states should use red dot marker: {t:?}"
    );
}

/// PR #144 follow-up: tool names containing BEL/ESC/newline must
/// be scrubbed before being concatenated into the OSC payload —
/// otherwise a hostile plugin or MCP server could inject further
/// escape sequences via `set_last_tool_name`.
#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn terminal_title_strips_control_bytes_from_tool_name() {
    use crate::ui::avatar::AvatarState;
    let t = super::format_terminal_title(AvatarState::Reading, Some("evil\x07\x1b[31m"));
    assert!(!t.contains('\x07'));
    assert!(!t.contains('\x1b'));
    // The clean residue should still surface so the user sees
    // *something* if the name was mostly text.
    assert!(t.contains("evil"));
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn terminal_title_all_control_bytes_falls_back_to_working() {
    use crate::ui::avatar::AvatarState;
    let t = super::format_terminal_title(AvatarState::Bash, Some("\x07\x1b\n"));
    assert_eq!(t, "◌ dirge: working");
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn osc_set_title_uses_st_terminator() {
    let bytes = super::osc_set_title("hello");
    // OSC introducer `\x1b]0;` + payload + ST terminator `\x1b\\`
    assert_eq!(bytes, b"\x1b]0;hello\x1b\\");
    assert!(
        !bytes.contains(&0x07),
        "BEL should not be used: {:?}",
        bytes
    );
}

#[cfg(feature = "experimental-ui-terminal-tab")]
#[test]
fn osc_reset_title_releases_to_shell() {
    let bytes = super::osc_reset_title();
    assert_eq!(bytes, b"\x1b]0;\x1b\\");
}
