## Build: cargo test (1356 pass), cargo check for verify. Zero warnings.

## Audit fixes complete (26 findings, 5 phases)
P1: PROV-1 (custom provider HTTPS+name collision), TOOL-1 (webfetch SSRF: decimal/octal/hex/mixed IPv4, hex-without-dots 0xa9fea9fe, IPv4-mapped), SESS-3 (0600 files + 0700 dir), UI-1/2 (ANSI strip), PERM-19 (high-risk tools), PERM-5 (heredoc), PERM-6 (complex-bash paths)
P2: LOOP-7/18 (toolResult camelCase), LOOP-9 (bridge text reset)
P3: PROV-2 (RetryNotice), LOOP-4 (AbortSignal dual), EXT-6 (--prompt)
P4: LOOP-1 (set_by_path empty-path guard), PERM-1/2 (doom-loop HashMap+window32)
P5: SESS-1 (cycle detect), EXT-3/4/5/7 (LSP fixes), UI-6 (paste cap), PERM-7+EXT-11+UI-3 (already fixed)

## Enum variant additions
AgentEvent: 8 files. StreamEvent: +4 files (retry, stream, rig_stream, rig_stream_factory). LoopEvent: +2 files (message kind, bridge). CompactString::new() not new_inline().

## Key state
end_session() no longer #[cfg(test)] — for compression splits only, not per-turn
guard.rs: LazyLock regex, 10 invisible chars. UsageStore mut for record_view.
24 learning_loop integration tests. Session DB FTS5 + trigram.
§
## AbortSignal dual flags

LOOP-4: `AbortSignal` in `tool.rs` has two `Arc<AtomicBool>`: `cancelled` (hard abort, tools check) and `interjected` (graceful stop at turn boundary, loop checks in `run.rs`). `into_agent_runner()` in `integration.rs` calls `signal.interject()`, not `signal.cancel()`, for UI interject signals.
§
## Doom-loop: HashMap per-key counter + FIFO ring, window 32, check-before-track (threshold 2)

checker.rs: `repeat_counts: HashMap<String, u32>` keyed `"{tool}\x00{input}"`. track_doom_loop bumps counter + pushes FIFO; eviction pops front + decrements. is_doom_loop checks count >= 2 BEFORE tracking (counter reflects previous only, not current). Window 32 (was 16) defeats 14-call decoy-gap. `repeat_counts.clear()` on set_working_dir.
