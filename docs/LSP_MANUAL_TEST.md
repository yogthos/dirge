# LSP integration — manual end-to-end test

The LSP integration is covered by ~120 unit tests against mock spawners and
duplex pipes. This document captures the manual smoke test against a real
`rust-analyzer` process — the one thing CI can't reproduce.

## Prerequisites

- `rust-analyzer` on `$PATH`. Verify with:
  ```bash
  which rust-analyzer && rust-analyzer --version
  ```
  If absent, `rustup component add rust-analyzer` from the default toolchain.
- A Rust project on disk. The dirge repo itself works.

## Scenario 1 — diagnostic surfacing on edit

**Goal**: after a deliberately broken edit, the `edit` tool's output
contains a `<diagnostics file="...">` block with the compile error.

1. Start dirge in the dirge repo: `cargo run --release`.
2. Wait for the initial prompt.
3. Type:
   ```
   Edit src/agent/builder.rs and change `pub async fn build_agent_inner` to
   `pub async fnn build_agent_inner` (typo: fnn instead of fn). After the
   edit, show me the exact tool output you got back.
   ```
4. **Expected behavior**:
   - The agent calls the `edit` tool, the file is mutated.
   - Within ~10 seconds (the `DIAGNOSTIC_WAIT` constant), the agent receives
     the tool result containing a section like:
     ```
     LSP errors detected in this file, please fix:
     <diagnostics file="/Users/.../src/agent/builder.rs">
     ERROR [N:M] expected one of `(`, `[`, `;`, or `<`, found `build_agent_inner`
     ...
     </diagnostics>
     ```
   - The agent then proposes a corrective edit (`fn` not `fnn`).
5. Ctrl+C to abort, then revert: `git checkout src/agent/builder.rs`.

**Failure modes to watch for**:
- No diagnostic block → check `rust-analyzer` is on PATH and the project has
  built at least once.
- Diagnostic block but with WARN entries → bug: only ERRORs should surface.
- Block appears but takes >15 seconds → bug: bounded wait isn't firing.

## Scenario 2 — `lsp` tool: hover at a known position

**Goal**: `lsp` tool dispatches `hover` and returns server-side info.

1. From inside dirge:
   ```
   Use the lsp tool to hover at src/main.rs line 1 character 1.
   ```
2. **Expected behavior**:
   - The agent calls `lsp` with
     `{"operation": "hover", "file_path": "src/main.rs", "line": 1, "character": 1}`.
   - Result is pretty-printed JSON from rust-analyzer: a `contents` field
     with the hovered-token info. Even at "mod agent;" position 1, the
     server may return null hover info; the tool reports
     `(no results from hover)`.
   - **Validation**: cursor at a real identifier (say, "build_agent" inside
     `src/main.rs`) should return non-empty hover content.

## Scenario 3 — workspace symbol search

**Goal**: `lsp.workspaceSymbol` returns matches across the workspace.

1. From inside dirge:
   ```
   Use the lsp tool with workspaceSymbol operation to find symbols
   matching "build_agent_inner". Pass src/main.rs as file_path (just to
   pick the workspace).
   ```
2. **Expected behavior**:
   - Pretty-printed JSON array with at least one entry pointing to
     `src/agent/builder.rs`.

## Scenario 4 — concurrent file edits don't spawn duplicate servers

**Goal**: the inflight-spawn dedupe works end-to-end.

1. Open two dirge sessions in different terminals, same repo.
2. In both, ask the agent to read `src/main.rs`. The first read triggers a
   spawn; the second should reuse the cached client (verifiable via
   `ps aux | grep rust-analyzer` — only one process per workspace within
   a single dirge session; across sessions there's one process per).
3. **Expected behavior**: across the lifetime of one dirge session,
   `ps aux | grep rust-analyzer | wc -l` returns 1 (the workspace root
   stays the same, so one server services all .rs touches).

## Scenario 5 — `--no-lsp` actually disables

**Goal**: feature gate / CLI flag work.

1. Run `cargo run --release -- --no-lsp` and ask the agent to edit a Rust
   file. The tool output must NOT contain any `<diagnostics>` block.
2. Run `cargo run --release --no-default-features --features 'loop git-worktree mcp'`
   and verify the binary builds and runs. The `lsp` tool and diagnostic
   block both should be absent.

## Scenario 6 — broken spawn doesn't retry

**Goal**: failed spawn marks (root, server_id) as broken so subsequent
file touches don't re-spawn.

1. Disable rust-analyzer temporarily:
   ```bash
   mv "$(which rust-analyzer)" "$(which rust-analyzer).bak"
   ```
2. Start dirge, ask to read a `.rs` file. First touch triggers a spawn;
   it fails (binary missing).
3. Ask to read another `.rs` file. Watch for log lines (run with
   `RUST_LOG=warn`): the second read should NOT log "spawn failed" — the
   broken-set blocks the retry.
4. Restore: `mv "$(which rust-analyzer).bak" "$(which rust-analyzer)"`.

## What this test plan deliberately doesn't cover

These are exercised by unit tests against the mock spawner and don't need
manual verification:
- JSON-RPC framing edge cases (multi-message buffers, partial reads).
- Request correlation by id under concurrent in-flight requests.
- Diagnostic dedupe + MAX_PER_FILE caps.
- Push-vs-pull diagnostic merge.
- URI ↔ path round-tripping with special characters.
- Config schema parsing.

See the corresponding tests under `src/lsp/**/tests::` for the contracts
those features guarantee.
