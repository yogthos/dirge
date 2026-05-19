use crate::ui::input::InputEditor;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn meta(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::ALT)
}

fn shift_enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
}

fn meta_enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
}

fn type_str(editor: &mut InputEditor, s: &str) {
    for c in s.chars() {
        editor.handle_key(press(KeyCode::Char(c)));
    }
}

fn len_bytes(s: &str) -> usize {
    s.len()
}

// ── existing tests ──────────────────────────────────────────

#[test]
fn typing_multibyte_chars_does_not_panic() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "på ");
    assert_eq!(editor.buffer.as_str(), "på ");
    assert_eq!(editor.cursor, editor.buffer.len());
}

#[test]
fn typing_mixed_ascii_and_multibyte() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hei på deg så fin dag æøå");
    assert_eq!(editor.buffer.as_str(), "hei på deg så fin dag æøå");
    assert_eq!(editor.cursor, editor.buffer.len());
}

#[test]
fn backspace_after_multibyte_does_not_panic() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "å");
    editor.handle_key(press(KeyCode::Backspace));
    assert_eq!(editor.buffer.as_str(), "");
    assert_eq!(editor.cursor, 0);
}

#[test]
fn left_arrow_steps_one_char_not_one_byte() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "aåb");
    assert_eq!(editor.cursor, 4);
    editor.handle_key(press(KeyCode::Left));
    assert_eq!(editor.cursor, 3);
    editor.handle_key(press(KeyCode::Left));
    assert_eq!(editor.cursor, 1);
}

#[test]
fn right_arrow_steps_one_char_not_one_byte() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "aåb");
    editor.cursor = 0;
    editor.handle_key(press(KeyCode::Right));
    assert_eq!(editor.cursor, 1);
    editor.handle_key(press(KeyCode::Right));
    assert_eq!(editor.cursor, 3);
}

#[test]
fn enter_returns_buffer_and_resets() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hei på");
    let out = editor.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(out.as_str(), "hei på");
    assert_eq!(editor.cursor, 0);
    assert_eq!(editor.buffer.as_str(), "");
}

// ── Ctrl+A / Ctrl+E ─────────────────────────────────────────

#[test]
fn ctrl_a_moves_to_start_of_line() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    assert_eq!(editor.cursor, len_bytes("hello world"));
    editor.handle_key(ctrl(KeyCode::Char('a')));
    assert_eq!(editor.cursor, 0);
    assert_eq!(editor.buffer.as_str(), "hello world");
}

#[test]
fn ctrl_e_moves_to_end_of_line() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.cursor = 0;
    editor.handle_key(ctrl(KeyCode::Char('e')));
    assert_eq!(editor.cursor, len_bytes("hello world"));
    assert_eq!(editor.buffer.as_str(), "hello world");
}

// ── Ctrl+B / Ctrl+F ─────────────────────────────────────────

#[test]
fn ctrl_b_moves_left_one_char() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "abc");
    assert_eq!(editor.cursor, 3);
    editor.handle_key(ctrl(KeyCode::Char('b')));
    assert_eq!(editor.cursor, 2);
    editor.handle_key(ctrl(KeyCode::Char('b')));
    assert_eq!(editor.cursor, 1);
}

#[test]
fn ctrl_b_at_start_does_nothing() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "abc");
    editor.cursor = 0;
    editor.handle_key(ctrl(KeyCode::Char('b')));
    assert_eq!(editor.cursor, 0);
}

#[test]
fn ctrl_f_moves_right_one_char() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "abc");
    editor.cursor = 0;
    editor.handle_key(ctrl(KeyCode::Char('f')));
    assert_eq!(editor.cursor, 1);
    editor.handle_key(ctrl(KeyCode::Char('f')));
    assert_eq!(editor.cursor, 2);
}

#[test]
fn ctrl_f_at_end_does_nothing() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "abc");
    editor.handle_key(ctrl(KeyCode::Char('f')));
    assert_eq!(editor.cursor, 3);
}

// ── Option+Left / Option+Right (word skip) ──────────────────

#[test]
fn option_left_skips_to_prev_word_start() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world foo");
    let len = len_bytes("hello world foo");
    assert_eq!(editor.cursor, len);
    editor.handle_key(meta(KeyCode::Left));
    assert_eq!(editor.cursor, len_bytes("hello world ")); // start of "foo"
    editor.handle_key(meta(KeyCode::Left));
    assert_eq!(editor.cursor, len_bytes("hello ")); // start of "world"
    editor.handle_key(meta(KeyCode::Left));
    assert_eq!(editor.cursor, 0); // start of "hello"
}

#[test]
fn option_left_at_start_does_nothing() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "abc");
    editor.cursor = 0;
    editor.handle_key(meta(KeyCode::Left));
    assert_eq!(editor.cursor, 0);
}

#[test]
fn option_left_from_middle_of_word_goes_to_its_start() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.cursor = len_bytes("hello wo"); // middle of "world"
    editor.handle_key(meta(KeyCode::Left));
    assert_eq!(editor.cursor, len_bytes("hello ")); // start of "world"
}

#[test]
fn option_right_skips_to_next_word_start() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world foo");
    editor.cursor = 0;
    editor.handle_key(meta(KeyCode::Right));
    assert_eq!(editor.cursor, len_bytes("hello ")); // start of "world"
    editor.handle_key(meta(KeyCode::Right));
    assert_eq!(editor.cursor, len_bytes("hello world ")); // start of "foo"
    editor.handle_key(meta(KeyCode::Right));
    assert_eq!(editor.cursor, len_bytes("hello world foo")); // end of line
}

#[test]
fn option_right_at_end_does_nothing() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "abc");
    editor.handle_key(meta(KeyCode::Right));
    assert_eq!(editor.cursor, 3);
}

#[test]
fn option_right_skips_punctuation() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "foo.bar_baz");
    editor.cursor = 0;
    editor.handle_key(meta(KeyCode::Right));
    // skip "foo" (word) then skip "." (punct) → land at "bar_baz" start
    assert_eq!(editor.cursor, len_bytes("foo."));
    assert_eq!(&editor.buffer[editor.cursor..], "bar_baz");
}

#[test]
fn option_right_multibyte_words() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hå bør");
    editor.cursor = 0;
    editor.handle_key(meta(KeyCode::Right));
    assert_eq!(editor.cursor, len_bytes("hå ")); // after "hå ", start of "bør"
    editor.handle_key(meta(KeyCode::Right));
    assert_eq!(editor.cursor, len_bytes("hå bør")); // end
}

// ── Meta+B / Meta+F (Emacs-style word skip) ─────────────────

#[test]
fn meta_b_skips_to_prev_word() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    assert_eq!(editor.cursor, len_bytes("hello world"));
    editor.handle_key(meta(KeyCode::Char('b')));
    assert_eq!(editor.cursor, len_bytes("hello ")); // start of "world"
    editor.handle_key(meta(KeyCode::Char('b')));
    assert_eq!(editor.cursor, 0); // start of "hello"
}

#[test]
fn meta_f_skips_to_next_word() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.cursor = 0;
    editor.handle_key(meta(KeyCode::Char('f')));
    assert_eq!(editor.cursor, len_bytes("hello ")); // start of "world"
    editor.handle_key(meta(KeyCode::Char('f')));
    assert_eq!(editor.cursor, len_bytes("hello world")); // end
}

// ── Ctrl+K (kill to line end) ────────────────────────────────

#[test]
fn ctrl_k_kills_to_end_of_line() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.cursor = len_bytes("hello ");
    editor.handle_key(ctrl(KeyCode::Char('k')));
    assert_eq!(editor.buffer.as_str(), "hello ");
    assert_eq!(editor.cursor, len_bytes("hello "));
}

#[test]
fn ctrl_k_at_end_does_nothing() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello");
    editor.handle_key(ctrl(KeyCode::Char('k')));
    assert_eq!(editor.buffer.as_str(), "hello");
    assert_eq!(editor.cursor, 5);
}

#[test]
fn consecutive_ctrl_k_accumulates_in_kill_ring() {
    let mut editor = InputEditor::new();

    // first kill
    type_str(&mut editor, "one two three");
    editor.cursor = len_bytes("one ");
    editor.handle_key(ctrl(KeyCode::Char('k'))); // kills "two three"
    assert_eq!(editor.buffer.as_str(), "one ");

    // consecutive kill (no non-kill action in between)
    editor.handle_key(ctrl(KeyCode::Char('k'))); // kills "" (nothing after cursor)
    // ring[0] should be "two three" (empty string appended = no change)

    // yank should return "two three"
    editor.handle_key(ctrl(KeyCode::Char('y')));
    assert_eq!(editor.buffer.as_str(), "one two three");
}

#[test]
fn non_kill_action_resets_kill_accumulation() {
    let mut editor = InputEditor::new();

    // first kill
    type_str(&mut editor, "one two three");
    editor.cursor = len_bytes("one ");
    editor.handle_key(ctrl(KeyCode::Char('k'))); // kill "two three"

    // type something (breaks kill accumulation)
    type_str(&mut editor, "X");
    assert_eq!(editor.buffer.as_str(), "one X");

    // Ctrl+K at end does nothing (cursor at end, nothing to kill)
    // The kill ring still has "two three" from earlier.
    // Kill accumulation was reset, but the ring entry persists.
    editor.handle_key(ctrl(KeyCode::Char('k')));

    // yank returns the previous kill (ring entries persist across typing)
    editor.cursor = 0;
    editor.handle_key(ctrl(KeyCode::Char('y')));
    assert_eq!(editor.buffer.as_str(), "two threeone X");
}

// ── Ctrl+U (kill to line start) ─────────────────────────────

#[test]
fn ctrl_u_kills_to_start_of_line() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.cursor = len_bytes("hello ");
    editor.handle_key(ctrl(KeyCode::Char('u')));
    assert_eq!(editor.buffer.as_str(), "world");
    assert_eq!(editor.cursor, 0);
}

#[test]
fn ctrl_u_at_start_does_nothing() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello");
    editor.cursor = 0;
    editor.handle_key(ctrl(KeyCode::Char('u')));
    assert_eq!(editor.buffer.as_str(), "hello");
    assert_eq!(editor.cursor, 0);
}

#[test]
fn ctrl_u_kills_entire_line_when_at_end() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello");
    editor.handle_key(ctrl(KeyCode::Char('u')));
    assert_eq!(editor.buffer.as_str(), "");
    assert_eq!(editor.cursor, 0);
}

// ── Ctrl+W (kill word before) ────────────────────────────────

#[test]
fn ctrl_w_kills_previous_word() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.handle_key(ctrl(KeyCode::Char('w')));
    assert_eq!(editor.buffer.as_str(), "hello ");
    assert_eq!(editor.cursor, len_bytes("hello "));
}

#[test]
fn ctrl_w_in_middle_of_word_kills_partial_word() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    // cursor at middle of "world": after 'r' (byte 9)
    editor.cursor = len_bytes("hello wor");
    editor.handle_key(ctrl(KeyCode::Char('w')));
    // prev_word_boundary goes to start of "world" → kills "wor"
    assert_eq!(editor.buffer.as_str(), "hello ld");
    assert_eq!(editor.cursor, len_bytes("hello "));
}

#[test]
fn ctrl_w_at_start_does_nothing() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello");
    editor.cursor = 0;
    editor.handle_key(ctrl(KeyCode::Char('w')));
    assert_eq!(editor.buffer.as_str(), "hello");
    assert_eq!(editor.cursor, 0);
}

#[test]
fn ctrl_w_kills_word_with_punctuation() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "foo.bar baz");
    editor.handle_key(ctrl(KeyCode::Char('w')));
    assert_eq!(editor.buffer.as_str(), "foo.bar ");
    assert_eq!(editor.cursor, len_bytes("foo.bar "));
}

// ── Option/Meta+Backspace (delete word before) ──────────────

#[test]
fn meta_backspace_deletes_previous_word() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.handle_key(meta(KeyCode::Backspace));
    assert_eq!(editor.buffer.as_str(), "hello ");
    assert_eq!(editor.cursor, len_bytes("hello "));
}

// ── Option/Meta+D (delete word after) ───────────────────────

#[test]
fn meta_d_deletes_next_word() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world foo");
    editor.cursor = 0;
    editor.handle_key(meta(KeyCode::Char('d')));
    assert_eq!(editor.buffer.as_str(), "world foo");
    assert_eq!(editor.cursor, 0);
}

#[test]
fn meta_d_in_middle_of_word_deletes_to_word_end() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.cursor = 1; // after 'h'
    editor.handle_key(meta(KeyCode::Char('d')));
    assert_eq!(editor.buffer.as_str(), "hworld");
    assert_eq!(editor.cursor, 1);
}

#[test]
fn meta_d_at_end_does_nothing() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello");
    editor.handle_key(meta(KeyCode::Char('d')));
    assert_eq!(editor.buffer.as_str(), "hello");
    assert_eq!(editor.cursor, 5);
}

// ── Ctrl+Y (yank) ───────────────────────────────────────────

#[test]
fn ctrl_y_yanks_most_recent_kill() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.cursor = len_bytes("hello ");
    editor.handle_key(ctrl(KeyCode::Char('k'))); // kill "world"
    assert_eq!(editor.buffer.as_str(), "hello ");

    editor.cursor = 0;
    editor.handle_key(ctrl(KeyCode::Char('y')));
    assert_eq!(editor.buffer.as_str(), "worldhello ");
    assert_eq!(editor.cursor, len_bytes("world"));
}

#[test]
fn ctrl_y_yanks_ctrl_w_kill() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.handle_key(ctrl(KeyCode::Char('w'))); // kill "world"
    assert_eq!(editor.buffer.as_str(), "hello ");

    editor.handle_key(ctrl(KeyCode::Char('y'))); // yank back
    assert_eq!(editor.buffer.as_str(), "hello world");
}

#[test]
fn ctrl_y_yanks_ctrl_u_kill() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.cursor = len_bytes("hello ");
    editor.handle_key(ctrl(KeyCode::Char('u'))); // kill "hello "
    assert_eq!(editor.buffer.as_str(), "world");

    editor.cursor = len_bytes("world");
    editor.handle_key(ctrl(KeyCode::Char('y'))); // yank "hello "
    assert_eq!(editor.buffer.as_str(), "worldhello ");
}

#[test]
fn ctrl_y_does_nothing_with_empty_kill_ring() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello");
    editor.handle_key(ctrl(KeyCode::Char('y')));
    assert_eq!(editor.buffer.as_str(), "hello");
    assert_eq!(editor.cursor, 5);
}

// ── Meta+Y (yank-pop / cycle kill ring) ─────────────────────

#[test]
fn meta_y_cycles_kill_ring_after_yank() {
    let mut editor = InputEditor::new();

    // first kill: "world"
    type_str(&mut editor, "hello world");
    editor.cursor = len_bytes("hello ");
    editor.handle_key(ctrl(KeyCode::Char('k')));
    assert_eq!(editor.buffer.as_str(), "hello ");

    // type to break kill accumulation
    type_str(&mut editor, "foo bar");
    // now buffer is "hello foo bar"
    editor.cursor = len_bytes("hello foo ");
    editor.handle_key(ctrl(KeyCode::Char('k'))); // kill "bar"
    assert_eq!(editor.buffer.as_str(), "hello foo ");

    // yank — most recent kill
    editor.handle_key(ctrl(KeyCode::Char('y')));
    assert_eq!(editor.buffer.as_str(), "hello foo bar");

    // meta+y → cycle to "world" (previous entry)
    editor.handle_key(meta(KeyCode::Char('y')));
    assert_eq!(editor.buffer.as_str(), "hello foo world");
}

#[test]
fn meta_y_does_nothing_without_prior_yank() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello world");
    editor.cursor = len_bytes("hello ");
    editor.handle_key(ctrl(KeyCode::Char('k')));

    editor.handle_key(meta(KeyCode::Char('y')));
    // no prior yank, should do nothing
    assert_eq!(editor.buffer.as_str(), "hello ");
}

// ── Ctrl+N / Ctrl+P (history) ────────────────────────────────

#[test]
fn ctrl_p_navigates_history_up() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "first");
    editor.handle_key(press(KeyCode::Enter));
    type_str(&mut editor, "second");
    editor.handle_key(press(KeyCode::Enter));

    assert!(editor.buffer.is_empty());
    editor.handle_key(ctrl(KeyCode::Char('p')));
    assert_eq!(editor.buffer.as_str(), "second");
    editor.handle_key(ctrl(KeyCode::Char('p')));
    assert_eq!(editor.buffer.as_str(), "first");
}

#[test]
fn ctrl_n_navigates_history_down() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "first");
    editor.handle_key(press(KeyCode::Enter));
    type_str(&mut editor, "second");
    editor.handle_key(press(KeyCode::Enter));

    editor.handle_key(ctrl(KeyCode::Char('p')));
    editor.handle_key(ctrl(KeyCode::Char('p')));
    assert_eq!(editor.buffer.as_str(), "first");

    editor.handle_key(ctrl(KeyCode::Char('n')));
    assert_eq!(editor.buffer.as_str(), "second");
}

#[test]
fn ctrl_n_at_bottom_clears_buffer() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "first");
    editor.handle_key(press(KeyCode::Enter));

    editor.handle_key(ctrl(KeyCode::Char('n')));
    assert!(editor.buffer.is_empty());
    assert_eq!(editor.cursor, 0);
}

// ── Kill ring max size ───────────────────────────────────────

#[test]
fn kill_ring_capped_at_10_entries() {
    let mut editor = InputEditor::new();
    // Create 12 distinct kill entries by breaking accumulation between kills.
    // Submit each word to history (Enter resets kill accumulation), then
    // navigate back and kill it. This gives 12 separate ring entries.
    for i in 0..12 {
        type_str(&mut editor, &format!("word{}", i));
        editor.handle_key(press(KeyCode::Enter)); // pushes to history, clears buffer
        // Navigate back to the word, position cursor, and kill
        editor.handle_key(ctrl(KeyCode::Char('p'))); // load "wordN" from history
        editor.cursor = 0;
        editor.handle_key(ctrl(KeyCode::Char('k'))); // kill entire buffer → ring entry
        assert!(editor.buffer.is_empty());
    }
    // Yank the most recent kill (word11, since limit is 10, word0 and word1 were evicted)
    editor.handle_key(ctrl(KeyCode::Char('y')));
    assert_eq!(editor.buffer.as_str(), "word11");
}

// ── Option+arrow during picker ──────────────────────────────

#[test]
fn option_left_with_picker_active_handled_by_picker() {
    let mut editor = InputEditor::new();
    editor.cursor = 0;
    editor.handle_key(press(KeyCode::Char('@')));
    assert!(editor.picker.as_ref().is_some_and(|p| p.active));

    let result = editor.handle_key(meta(KeyCode::Left));
    assert!(result.is_none());
}

// ── Multi-line input ────────────────────────────────────────

#[test]
fn shift_enter_inserts_newline() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello");
    editor.handle_key(shift_enter());
    type_str(&mut editor, "world");
    assert_eq!(editor.buffer.as_str(), "hello\nworld");
}

#[test]
fn meta_enter_inserts_newline() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "hello");
    editor.handle_key(meta_enter());
    type_str(&mut editor, "world");
    assert_eq!(editor.buffer.as_str(), "hello\nworld");
}

#[test]
fn plain_enter_submits_multiline_text() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "line1");
    editor.handle_key(shift_enter());
    type_str(&mut editor, "line2");
    let submitted = editor.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(submitted.as_str(), "line1\nline2");
    assert!(editor.buffer.is_empty());
}

#[test]
fn multiline_history_saves_and_restores() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "first");
    editor.handle_key(shift_enter());
    type_str(&mut editor, "second");
    editor.handle_key(press(KeyCode::Enter)); // submit

    // Navigate up in history
    editor.handle_key(press(KeyCode::Up));
    assert_eq!(editor.buffer.as_str(), "first\nsecond");
}

#[test]
fn multiline_cursor_up_moves_to_previous_logical_line() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "line1");
    editor.handle_key(shift_enter());
    type_str(&mut editor, "line2");
    // cursor is at end: after "line2"
    let end_pos = editor.cursor;
    editor.handle_key(press(KeyCode::Up));
    // cursor should now be on line1
    assert!(editor.cursor < end_pos);
    // verify it's on line1
    let line1_end = "line1".len();
    assert!(editor.cursor <= line1_end);
}

#[test]
fn multiline_cursor_down_moves_to_next_logical_line() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "line1");
    editor.handle_key(shift_enter());
    type_str(&mut editor, "line2");
    // Move to start of line1
    editor.cursor = 0;
    editor.handle_key(press(KeyCode::Down));
    // cursor should now be on line2
    let line1_len = "line1\n".len();
    assert!(editor.cursor >= line1_len);
}

#[test]
fn multiline_up_at_top_goes_to_history() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "prev");
    editor.handle_key(press(KeyCode::Enter)); // submit to history

    type_str(&mut editor, "line1");
    editor.handle_key(shift_enter());
    type_str(&mut editor, "line2");
    editor.cursor = 0; // at top of multi-line buffer

    // Up at top should go to history
    editor.handle_key(press(KeyCode::Up));
    assert_eq!(editor.buffer.as_str(), "prev");
}

#[test]
fn multiline_down_at_bottom_goes_to_history() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "prev");
    editor.handle_key(press(KeyCode::Enter));

    type_str(&mut editor, "next");
    editor.handle_key(press(KeyCode::Enter));

    // Load "next" from history
    editor.handle_key(press(KeyCode::Up));
    assert_eq!(editor.buffer.as_str(), "next");

    // Down from bottom of single line should clear
    editor.handle_key(press(KeyCode::Down));
    assert!(editor.buffer.is_empty());
}

#[test]
fn enter_with_picker_active_does_not_submit() {
    let mut editor = InputEditor::new();
    editor.cursor = 0;
    editor.handle_key(press(KeyCode::Char('@')));
    assert!(editor.picker.as_ref().is_some_and(|p| p.active));

    let result = editor.handle_key(press(KeyCode::Enter));
    assert!(result.is_none());
    assert!(editor.picker.as_ref().is_some_and(|p| p.active));
}

#[test]
fn meta_enter_with_picker_active_inserts_newline() {
    let mut editor = InputEditor::new();
    editor.cursor = 0;
    editor.handle_key(press(KeyCode::Char('@')));
    // Even with picker active, Meta+Enter should insert newline
    // (the handle_picker_key returns false for Meta+Enter, so it falls through)
    let result = editor.handle_key(meta_enter());
    assert!(result.is_none());
}

// ── Paste collapse ──────────────────────────────────────────

#[test]
fn short_paste_inserts_raw() {
    // Single-line paste — should go in as plain text, no placeholder.
    let mut editor = InputEditor::new();
    editor.handle_paste("hello world");
    assert_eq!(editor.buffer.as_str(), "hello world");
    assert_eq!(editor.cursor, "hello world".len());
}

#[test]
fn three_line_paste_still_raw() {
    // Threshold is 4 lines — three lines should not collapse.
    let mut editor = InputEditor::new();
    editor.handle_paste("a\nb\nc");
    assert_eq!(editor.buffer.as_str(), "a\nb\nc");
}

#[test]
fn four_line_paste_collapses_to_placeholder() {
    let mut editor = InputEditor::new();
    editor.handle_paste("a\nb\nc\nd");
    // Buffer holds a marker block; render_line should show "[4 lines pasted]".
    assert!(editor.buffer.contains('\u{0001}'));
    let raw_line = editor.buffer.as_str();
    let (display, _) = editor.render_line(raw_line, editor.cursor);
    assert_eq!(display, "[4 lines pasted]");
    // Expanded text is the original paste.
    assert_eq!(editor.expanded().as_str(), "a\nb\nc\nd");
}

#[test]
fn submit_expands_placeholders() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "before ");
    editor.handle_paste("L1\nL2\nL3\nL4");
    type_str(&mut editor, " after");
    let submitted = editor.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(submitted.as_str(), "before L1\nL2\nL3\nL4 after");
    // Buffer and pastes both cleared after submit.
    assert!(editor.buffer.is_empty());
}

#[test]
fn left_right_skip_placeholder_as_unit() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "x");
    editor.handle_paste("a\nb\nc\nd");
    type_str(&mut editor, "y");
    // Buffer now: "x" + marker + "y"
    let end = editor.cursor;
    // One Left should jump from after 'y' to between marker close and 'y'.
    editor.handle_key(press(KeyCode::Left));
    assert!(editor.cursor < end);
    // Next Left should skip the entire marker block, landing just after 'x'.
    let after_first = editor.cursor;
    editor.handle_key(press(KeyCode::Left));
    assert_eq!(editor.cursor, 1); // just after 'x'
    assert!(editor.cursor < after_first);
}

#[test]
fn backspace_deletes_whole_placeholder() {
    let mut editor = InputEditor::new();
    type_str(&mut editor, "a");
    editor.handle_paste("L1\nL2\nL3\nL4");
    type_str(&mut editor, "b");
    let len_with_marker = editor.buffer.len();
    // Move left past 'b', so cursor sits just after the marker.
    editor.handle_key(press(KeyCode::Left));
    // Backspace removes the whole marker block in one go.
    editor.handle_key(press(KeyCode::Backspace));
    assert!(editor.buffer.len() < len_with_marker);
    assert_eq!(editor.buffer.as_str(), "ab");
}

#[test]
fn second_paste_of_same_content_expands_inline() {
    let mut editor = InputEditor::new();
    editor.handle_paste("a\nb\nc\nd");
    // After first paste: buffer holds a marker; expanded length matches input.
    assert!(editor.buffer.contains('\u{0001}'));
    // Second paste of identical content expands the existing placeholder.
    editor.handle_paste("a\nb\nc\nd");
    assert_eq!(editor.buffer.as_str(), "a\nb\nc\nd");
    assert!(!editor.buffer.contains('\u{0001}'));
}

#[test]
fn second_paste_of_different_content_creates_new_placeholder() {
    let mut editor = InputEditor::new();
    editor.handle_paste("a\nb\nc\nd");
    editor.handle_paste("X\nY\nZ\nW");
    // Two distinct placeholder markers in the buffer.
    let marker_count = editor.buffer.matches('\u{0001}').count();
    assert_eq!(marker_count, 4); // two open + two close
    // Expanded contains both bodies in order.
    assert_eq!(editor.expanded().as_str(), "a\nb\nc\ndX\nY\nZ\nW");
}

#[test]
fn paste_mark_chars_stripped_from_input() {
    // A malicious paste containing PASTE_MARK shouldn't break the parser.
    let mut editor = InputEditor::new();
    editor.handle_paste("a\nb\n\u{0001}\nc\nd");
    // PASTE_MARK chars are stripped before storage; line count after strip is 5
    // (still >= 4) so it should collapse.
    assert_eq!(editor.expanded().as_str(), "a\nb\n\nc\nd");
}
