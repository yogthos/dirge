## FTS5 formula migration: 'rebuild' doesn't work
External-content FTS5: `INSERT INTO fts(fts) VALUES('rebuild')` re-indexes using old trigger formula. To change indexed content (e.g. add tool_name to index), DELETE FROM fts then INSERT INTO fts SELECT id, new_formula FROM messages.
§
## #![allow(dead_code)] hides real dead code
Module-level suppression in agent_loop/mod.rs and lsp/mod.rs concealed ~50 genuinely unused items. Removing it revealed the true extent. Prefer targeted per-item annotations — even many are better than module-wide silence.
§
## env::set_var + parallel tests = flaky
`std::env::set_var` is global/unsafe/unsynchronized. Tests mutating same key race. Fix: static Mutex + RAII EnvGuard that clears on Drop (applied in dirge_paths.rs).
§
## Rust pitfalls
- `matches!` can't have guards before `|` → use `match`
- `CompactString::new_inline` doesn't exist → `CompactString::new()`
- `log::error!` not in deps → `tracing::error!(target: "dirge::...", "...")`
- AgentEvent variant → 8 files; StreamEvent → also rig_stream.rs + rig_stream_factory.rs
- Double `session.add_message(User)` → dup messages. Handler renders only.
- Retry test: events order `["retry", "start", "done"]` — Retry yields BEFORE new inner stream
- parse_alt_ipv4 hex: only pure hex (no dots); dotted "0x7f.0.0.1" falls through to per-octet
