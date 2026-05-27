# Storyboard 03 — Background task completes between turns

## Scenario

The user asked the agent to run a long test suite in the background (via
`task` tool with `background=true`). The agent kicked it off and is
continuing to chat. A few turns later, the background task finishes
while the user is typing their next prompt. They submit; the LLM
sees the completion notice in-band as a `<system-reminder>` block,
but the visible chat log shows ONLY what the user typed — not the
reminder wrapper.

## What the user sees

While the user types and the bg task is mid-flight, an inline
notification briefly appears (rendered by `BackgroundStore::with_ui_sink`'s
listener) but is not part of the persisted chat:

```
[$_$] background task task-3 started: cargo test --release
```

After the test finishes:

```
[done] task-3 completed (2m34s) — 1718 pass / 0 fail
```

User submits their next prompt: `let's add a test for the new clojure adapter`

```
<you>  let's add a test for the new clojure adapter

<dirge> Good — that's a follow-up on the multi-method dispatch fix.
        I'll start by …
```

What the user does NOT see:

- A `<system-reminder>` block under `<you>`
- The task ID, exit code, or stdout dump prepended to their text
- Two copies of their message

What the **LLM** sees (verifiable via session-DB inspection):

```
<system-reminder>
The following background tasks finished since your last turn:

Task task-3 (completed):
1718 pass / 0 fail
</system-reminder>

let's add a test for the new clojure adapter
```

## Code trace

### Step 1 — Background task completes

- The agent originally dispatched `task` with `background=true`. The
  background runner stores its result in `BackgroundStore`
  (`src/agent/tools/background.rs`) when it finishes.
- `BackgroundStore::with_ui_sink` was wired with a tokio mpsc sender
  in `build_channels`. The `LifecycleEvent` variants (`Started` /
  `Finished`) are declared at `src/agent/tools/background.rs:32-39`;
  the actual `LifecycleEvent::Finished(TaskNotification { id, state:
  Completed(text), … })` send happens at line 182 when the
  background runner's task completes.
- The UI's event loop drains the channel and writes the `[done]
  task-3 completed …` line via the same renderer.

### Step 2 — User submits their next prompt

- `text` is the user's plain string: `let's add a test for the new clojure adapter`
- `prepend_pending_notifications(&prompt, bg_store.as_ref())`
  (`src/agent/tools/background.rs:300-334`) drains the
  notification store and constructs:
  ```
  <system-reminder>
  The following background tasks finished since your last turn:

  Task task-3 (completed):
  1718 pass / 0 fail
  </system-reminder>

  let's add a test for the new clojure adapter
  ```
- This combined string becomes `initial_prompt` of the agent run.
- Critically, `session.add_message(MessageRole::User, &text)` saves
  the **clean** `text` to the on-disk session — not the wrapped
  version. So the persisted history isn't polluted with stale
  notifications.

### Step 3 — Agent loop emits `MessageStart{User}`

- `integration.rs::run_agent_loop_with_summarizer` builds
  `prompts = vec![LoopMessage::User(UserMessage { content: cfg.initial_prompt })]`
  where `initial_prompt` is the FULL wrapped string.
- `run_agent_loop` emits `LoopEvent::MessageStart { message: LoopMessage::User(...) }`.

### Step 4 — Bridge converts to `AgentEvent::UserMessage`

- `EventBridge` (`src/agent/agent_loop/bridge.rs:280-296`):
  ```rust
  LoopMessage::User(u) => {
      vec![AgentEvent::UserMessage {
          content: CompactString::from(u.content),
      }]
  }
  ```
- The content carries the full wrapped string. This is intentional
  — downstream consumers (e.g. the agent runner's request builder)
  need the wrapper to forward to the LLM.

### Step 5 — UI consumer strips the wrapper before rendering

- `src/ui/mod.rs` `AgentEvent::UserMessage` arm:
  ```rust
  AgentEvent::UserMessage { content } => {
      let visible = strip_leading_system_reminder(&content);
      write_user_lines(&mut renderer, visible)?;
      renderer.write_line("", Color::White)?;
  }
  ```
- `strip_leading_system_reminder` (`src/ui/mod.rs`):
  - Returns the input unchanged if no leading `<system-reminder>` block
  - Otherwise strips through the matching `</system-reminder>` and any
    trailing whitespace/newlines
  - Conservative: missing close tag → returns input unchanged
- `write_user_lines` renders only the user's plain text under `<you>`.

## What was broken before the fix

Prior to the fix:

```
<you>  <system-reminder>
       The following background tasks finished since your last turn:

       Task task-3 (completed):
       1718 pass / 0 fail
       </system-reminder>

       let's add a test for the new clojure adapter
```

That looked like "session state got printed along with my input."

## Coverage

`strip_system_reminder_tests` module in `src/ui/mod.rs`:

- `passes_plain_text_through` — no reminder, no mutation
- `strips_block_and_trailing_blank_lines` — full round-trip
- `does_not_strip_mid_message_reminder` — only LEADING blocks
  qualify (a user could legitimately quote a reminder mid-message)
- `handles_leading_whitespace_before_reminder` — leading
  newlines/spaces before the block are tolerated
- `missing_close_tag_leaves_input_alone` — adversarial / corrupted
  input is passed through, not eaten

## Interjection-queue paths

Two additional UI paths drain queued interjections (when the user
typed while the agent was running) and spawn a new run:

- Idle drain (`src/ui/mod.rs:3144`) — at the bottom of the main
  loop, when not running and the queue is non-empty
- Runner-event drain (`src/ui/mod.rs:3285`) — after the runner
  emits its `InterjectionStop` event

Both paths previously called `write_user_lines` directly, which would
have duplicated the loop's `UserMessage` render. They now rely on
the loop bridge as the single render point, with the wrapper-strip
applied uniformly.

## Edge cases verified

- **No pending notifications**: `prepend_pending_notifications`
  returns `prompt.to_string()` unchanged. `strip_leading_system_reminder`
  is a no-op. User sees plain text under `<you>`.
- **Multiple completed tasks**: all rendered inside the single
  `<system-reminder>` block, all stripped together.
- **User's message starts with `<system-reminder>` literally**: yes,
  this strips it. Trade-off accepted: someone literally pasting that
  string would have it eaten. Mitigated by the "leading whitespace
  tolerance only" rule — a quoted reminder in the middle of a
  message survives. Worst case the LLM still receives the original
  input via `session.add_message` and the bridge.
