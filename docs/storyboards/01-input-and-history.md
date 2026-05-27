# Storyboard 01 — Input, history, and draft preservation

## Scenario

The user is mid-conversation. They've typed `let me think about this for a moment, I want to clarify ` into the input box but want to recall a previous prompt to copy a file path from it. They press **Up** twice, copy what they need, then press **Down** twice to get back to their draft.

## What the user sees

```
[ session log above ]

(o o)  let me think about this for a moment, I want to clarify ▮
       12 tk
```

User presses **Up**:

```
(o o)  fix the bug in src/agent/agent_loop/run.rs▮
       6 tk
```

User presses **Up** again:

```
(o o)  read the README▮
       3 tk
```

User presses **Down**:

```
(o o)  fix the bug in src/agent/agent_loop/run.rs▮
       6 tk
```

User presses **Down**:

```
(o o)  let me think about this for a moment, I want to clarify ▮
       12 tk
```

Cursor returns to the **same position** in the draft (right after the trailing space).

## Code trace

### Step 1 — User types text

- Each keystroke routes through `InputEditor::handle_event` in
  `src/ui/input.rs`. `KeyCode::Char(c)` lands in the catch-all editing
  branch and `self.buffer.push(c)` accumulates.
- Cursor advances; `display()` projects the buffer (including paste
  placeholders) to the rendered string.

### Step 2 — User presses Up the first time

- Routed to `InputEditor::history_up` (`src/ui/input.rs:1160-1180`).
- `self.history_pos` is `None` (not currently navigating).
- **PROV-5 / SESS-13 follow-up — the draft is stashed:**
  ```rust
  if self.history_pos.is_none() {
      self.history_draft = Some((self.buffer.clone(), self.cursor));
  }
  ```
- `self.history_pos = Some(hist_len - 1)`; `self.buffer = self.history[pos].clone()`; `self.cursor = self.buffer.len()`.
- Rendered: the most-recent history entry, cursor at end.

### Step 3 — User presses Up again

- `history_pos` is `Some(hist_len - 1)`. Branch `Some(p) if p > 0 => p - 1`
  decrements to the second-most-recent.
- The draft stash is untouched (still holds the original buffer +
  cursor). The check `if self.history_pos.is_none()` only runs on the
  transition.

### Step 4 — User presses Down

- `InputEditor::history_down` (`src/ui/input.rs:1183-1207`).
- `history_pos == Some(hist_len - 2)`; `pos + 1 < self.history.len()` is
  true, so it advances to `hist_len - 1` (back to most-recent).

### Step 5 — User presses Down past the newest entry

- `history_pos == Some(hist_len - 1)`; `pos + 1 < self.history.len()` is
  **false**. The `Some(_)` arm runs:
  ```rust
  self.history_pos = None;
  if let Some((draft, cursor)) = self.history_draft.take() {
      self.buffer = draft;
      self.cursor = cursor.min(self.buffer.len());
  } else {
      self.buffer.clear();
      self.cursor = 0;
  }
  ```
- The draft and the cursor position are restored.

## Coverage

Four unit tests in `src/ui/input.rs`:

- `history_up_stashes_in_progress_draft` — full round-trip with
  cursor restoration.
- `history_up_with_empty_draft_still_returns_empty_on_restore` —
  Down-past-end with no draft falls through to empty (existing
  behavior preserved).
- `history_down_with_no_navigation_is_noop` — Down with `history_pos
  == None` doesn't disturb the buffer.
- `set_text_clears_history_draft` — `/fork` restoring a prompt clears
  any stash (the restored text IS the new draft).

## Edge cases verified

- **Cursor past end of buffer after restore**: clamped by
  `cursor.min(self.buffer.len())`. If somehow `cursor > len`, restore
  lands at end-of-buffer.
- **Multi-line draft**: the buffer is a flat string with `\n`s, cursor
  is a byte offset. Stashing/restoring is a straight clone — line
  structure is preserved.
- **Submit clears stash**: the Enter branch sets `self.history_draft = None`
  along with `history_pos = None`. A subsequent Up from the freshly-
  cleared editor stashes the empty buffer (which restores cleanly to
  empty on Down past end).
