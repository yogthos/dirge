# Storyboard 04 — `/compress <focus>` Hermes-style focus compaction

## Scenario

The user has been pair-programming for two hours. The session covers
three unrelated subtasks: an MCP debugging foray, refactoring the
permission layer, and adding LSP features. They want to wrap up the
permission refactor and keep working — but the conversation is
pressing the context window. The other two threads are tangentially
useful but not essential.

They run:

```
/compress permission layer refactor
```

The compactor produces a summary that gives ~65% of its budget to
permission-layer details (file paths, function names, decisions,
test names) and one-line each to the MCP and LSP threads.

## What the user sees

```
> /compress permission layer refactor
compressing...

[compact summary]
## Goal
The user is refactoring the permission layer to add
…
```

After completion:

```
░ compacted 1 times (saved ~12483 tokens)
```

Subsequent turns see the summary as a system-prefixed message and
the recent tail of original turns; older turns are gone.

## Code trace

### Step 1 — Slash command parses the focus argument

- `src/ui/slash.rs:778`:
  ```rust
  "/compress" | "/compact" => {
      let instructions = if parts.len() > 1 {
          Some(parts[1..].join(" "))
      } else {
          None
      };
      return Err(anyhow::anyhow!("DEFER_COMPRESS:{}", instr_str));
  }
  ```
- `parts` is the whitespace-split command. For
  `/compress permission layer refactor`, `parts[1..]` is
  `["permission", "layer", "refactor"]` joined into
  `"permission layer refactor"`.
- The `DEFER_COMPRESS:<focus>` sentinel is caught upstream and
  routed to `handle_compress` with the focus as `instructions`.

### Step 2 — `handle_compress` builds the prompt

- `handle_compress` (`src/ui/slash.rs:117`) is the OLD per-session
  flow. It selects `messages_to_summarize` (everything below the
  tail that fits in `keep_recent_tokens`) and calls
  `client.compress_messages(model, msgs, prev_summary, Some("permission layer refactor"))`.

### Step 3 — `compress_messages` applies the focus framing

- `src/provider/mod.rs:451-477`:
  ```rust
  let instructions_block = match instructions {
      Some(text) if !text.trim().is_empty() => format!(
          "FOCUS TOPIC: \"{}\"\n\
           The user has requested that this compaction PRIORITISE \
           preserving all information related to the focus topic …\
           The focus topic sections should receive roughly 60-70% \
           of the summary token budget. …\n\
           NEVER preserve API keys, tokens, passwords, or \
           credentials — use [REDACTED].",
          text.trim(),
          text.trim(),
      ),
      _ => "(none)".to_string(),
  };
  ```
- Verbatim port of `hermes-agent/agent/context_compressor.py:1050-1054`.
- Substituted into `COMPACTION_PROMPT`'s `{instructions}` placeholder.

### Step 4 — Summarizer runs

- `summarize::summarize_with_model` invokes the configured
  auxiliary model (typically the same as the agent model).
- PROV-9: if the assembled prompt exceeds 128 KiB it's
  head_tail_truncated (40% head, 60% tail, newline-aligned)
  before dispatch.
- Standard retry policy applies — network / rate-limit errors
  get the same backoff as agent turns.

### Step 5 — Apply the summary

- `handle_compress` pushes a system message with the summary into
  `session.compactions` and rewrites `session.messages` to keep
  only the head + summary + tail.
- The next `convert_history` call (when a new prompt is submitted)
  reads the compaction via `session.compacted_context()` and emits
  `Message::system("[Previous conversation summary]\n…")` plus the
  preserved tail.

## How this differs from the agent-loop auto-compaction path

There are TWO compaction paths in dirge:

| Path | Trigger | Function | Prompt source |
|---|---|---|---|
| Slash `/compress` | Explicit user command | `handle_compress` → `client.compress_messages` | `COMPACTION_PROMPT` template + focus framing in `{instructions}` |
| Loop auto-compact | Loop usage decision at >75% context | `run_compaction_pass_with_focus` → `compression::build_summary_prompt` | Direct Rust-built Hermes prompt with `focus_topic` parameter |

Both paths now honor focus_topic in the same Hermes-style framing.
The slash command's text-after-the-command is treated as the focus.

## Coverage

- Hermes parity: prompt template wording verbatim ported from
  `context_compressor.py:1050-1054`.
- `agent::compression::tests::full_compaction_wire_with_mock_summarizer`
  exercises the agent-loop path with a mock summarizer.
- The slash-command path is tested via the existing
  `handle_compress` integration tests in `src/ui/slash.rs`.

## Edge cases verified

- **Empty focus** (`/compress` with no args): `instructions = None`
  → `{instructions}` placeholder gets `"(none)"`. The Hermes
  framing block is NOT emitted; the summary defaults to the
  general structure.
- **Whitespace-only focus** (`/compress     `): the `text.trim()`
  empties; same as empty focus path.
- **Focus containing quotes**: `text.trim()` is interpolated raw
  into the prompt string. The model sees the literal quotes; not
  exploitable (the prompt is going to an LLM, not a shell).
- **Focus + previous compaction**: the previous summary is fed in
  via `previous_summary` parameter, and `build_summary_prompt`'s
  "update an existing summary" branch fires. The focus framing
  is appended to BOTH the new-summary and update-summary
  branches.
