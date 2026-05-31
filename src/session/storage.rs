use std::path::PathBuf;

use crate::session::Session;

fn session_dir() -> PathBuf {
    dirs_path().join("sessions")
}

#[cfg(not(test))]
fn home_fallback() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub(crate) fn dirs_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("DIRGE_DATA_DIR") {
        return PathBuf::from(dir);
    }
    // dirge-sn1k: tests must NEVER write to the user's real data dir.
    // Several tests exercise the runtime `new_session` handler, which
    // calls `save_session` — and they build fixtures with model "m" and
    // "stale"/"outgoing" messages. Without isolation those persist into
    // ~/.../dirge/sessions and pollute the user's recent-sessions list on
    // every `cargo test`. Route all test writes to a per-process temp dir
    // (DIRGE_DATA_DIR still wins, so tests can pick their own location).
    #[cfg(test)]
    {
        return std::env::temp_dir().join(format!("dirge-test-data-{}", std::process::id()));
    }
    #[cfg(not(test))]
    {
        let base = dirs::data_dir().unwrap_or_else(home_fallback);
        base.join("dirge")
    }
}

pub(crate) fn config_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("DIRGE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("dirge")
}

/// Validate that a session id is safe to interpolate into a path.
/// Session ids are normally UUIDs (hex + hyphens), but they round-trip
/// through JSON on disk so a tampered-with file could carry an id like
/// `../../etc/passwd`. Reject anything that isn't strictly
/// `[A-Za-z0-9._-]+` so a malicious id can't escape the session dir.
fn validate_session_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() {
        anyhow::bail!("session id is empty");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        anyhow::bail!("session id contains disallowed characters: {:?}", id);
    }
    // Belt-and-braces: `..` or leading `.` would still resolve relatively
    // via `Path::join` even after the char check (`.` is allowed for
    // legitimate ids like `2024.session`).
    if id == "." || id == ".." || id.contains("/") || id.contains("\\") {
        anyhow::bail!("session id resolves outside the session dir: {:?}", id);
    }
    Ok(())
}

pub fn save_session(session: &Session) -> anyhow::Result<()> {
    validate_session_id(&session.id)?;
    // SESS-15: refuse to save a session that was loaded from a
    // file with `schema_version > SCHEMA_VERSION`. Newer-version
    // fields silently zeroed via `#[serde(default)]` at load;
    // writing the truncated form would permanently lose data the
    // newer dirge cared about. Better to surface an explicit
    // error so the user upgrades dirge instead of silently
    // corrupting their session.
    if let Some(file_version) = session.loaded_from_newer_version {
        anyhow::bail!(
            "refusing to save session {}: it was loaded from a newer schema (file version {}, this dirge supports {}). Upgrade dirge, or copy the session to a new id to write a fresh file.",
            session.id,
            file_version,
            crate::session::SCHEMA_VERSION,
        );
    }
    let dir = session_dir();
    std::fs::create_dir_all(&dir)?;
    // SESS-3: restrict session directory to owner-only (0700).
    // Session files contain user prompts, file contents, and
    // command outputs — other users on multi-user hosts should
    // not be able to list session IDs.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = dir.join(format!("{}.json", session.id));
    let json = serde_json::to_string_pretty(session)?;

    // Batch2-3 (audit fix): concurrent-writer detection. If another
    // dirge instance saved to this file since we loaded it (i.e. the
    // on-disk mtime is newer than `session.loaded_mtime`), writing
    // verbatim would clobber the other instance's work. Divert to a
    // `<id>.conflict-<unix_ts>.json` sibling so neither side loses
    // data, and surface a clear error so the UI's "save failed"
    // warning explains the situation.
    if let Some(loaded_mtime) = session.loaded_mtime
        && let Ok(meta) = std::fs::metadata(&path)
        && let Ok(disk_mtime) = meta.modified()
        && disk_mtime > loaded_mtime
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let conflict_path = dir.join(format!("{}.conflict-{}.json", session.id, ts));
        crate::fs_atomic::atomic_write_sync(&conflict_path, json.as_bytes())?;
        anyhow::bail!(
            "session {} was modified by another dirge instance; your changes saved to {} so neither copy is lost. Reload the session to see the other instance's state.",
            session.id,
            conflict_path.display()
        );
    }

    // Atomic write — write to a sibling `.tmp.<nonce>` file,
    // fsync, then rename over the target. A crash mid-write leaves
    // the temp behind but never a truncated `.json`. POSIX
    // rename(2) is atomic on the same filesystem; the helper picks
    // a temp in the same parent dir to preserve that invariant.
    //
    // Extracted into `crate::fs_atomic` so this path + the
    // file-mutating tools (`write`/`edit`/`apply_patch`) share one
    // implementation. Previously the tools called
    // `tokio::fs::write` directly which truncates in place — a
    // corruption vector on crash.
    crate::fs_atomic::atomic_write_sync(&path, json.as_bytes())?;
    Ok(())
}

pub fn load_session(id: &str) -> anyhow::Result<Session> {
    validate_session_id(id)?;
    let dir = session_dir();
    let path = dir.join(format!("{}.json", id));
    // Batch2-3 (audit fix): record file mtime BEFORE reading so the
    // conflict check in save_session compares against the version
    // we actually loaded, not whatever has happened to the file
    // since. There's still a tiny window between metadata() and
    // read_to_string() — but the rename-based atomic_write makes
    // it impossible to see a torn read; if a concurrent writer
    // landed in that window we'll just detect THEIR version's
    // mtime, and our next save_session will conflict-divert.
    let loaded_mtime = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());
    let json = std::fs::read_to_string(&path)?;

    // F8: schema-version handling. Pre-F8 session files have no
    // `schema_version` field; serde defaults it to 0. New
    // sessions are at `SCHEMA_VERSION`. Anything in between gets
    // migrated. A file with schema_version > SCHEMA_VERSION
    // (forward-incompatible) loads with a warning — most fields
    // still deserialize via `#[serde(default)]`, just the new
    // ones get default values.
    let mut session: Session = serde_json::from_str(&json).map_err(|e| {
        // Add file-path context to corrupted-file errors so the
        // user knows which session is broken and can recover by
        // restoring from a backup or deleting.
        anyhow::anyhow!("failed to parse {}: {e}", path.display())
    })?;
    session.loaded_mtime = loaded_mtime;

    if session.schema_version < crate::session::SCHEMA_VERSION {
        migrate_session(&mut session);
        session.schema_version = crate::session::SCHEMA_VERSION;
    } else if session.schema_version > crate::session::SCHEMA_VERSION {
        tracing::warn!(
            target: "dirge::session",
            path = %path.display(),
            file_version = session.schema_version,
            our_version = crate::session::SCHEMA_VERSION,
            "session file is from a newer dirge version; unknown fields will default. Upgrade dirge to read it fully."
        );
        // SESS-15: remember that the file was newer so save_session
        // can refuse to overwrite. Otherwise a downgrade-then-save
        // permanently loses the newer-version fields that
        // #[serde(default)] silently zeroed at load.
        session.loaded_from_newer_version = Some(session.schema_version.into());
    }
    Ok(session)
}

/// Bring a session loaded from an older schema version up to the
/// current `SCHEMA_VERSION`. Idempotent. Each migration step
/// handles one version bump; chain them as we add versions.
///
/// Current state: SCHEMA_VERSION = 1, which is "schema-versioned"
/// vs. pre-F8 (treated as 0). No data shape changes between
/// version 0 and 1 — the field additions for branch_summaries,
/// tool_calls, current_prompt_name etc. all used
/// `#[serde(default)]` so they already migrate transparently.
/// This function exists so future schema bumps have a hook.
fn migrate_session(session: &mut Session) {
    // v0 → v1: no-op (back-compat handled entirely via `#[serde(default)]`).
    // v1 → v2: recompute `estimated_tokens` for every message + the
    // session's `total_estimated_tokens` because pre-9a044ce sessions
    // counted only assistant TEXT — tool args and tool results were
    // ignored. Without this migration, a resumed long-running session
    // shows a context usage 5–10× under reality and could silently
    // exceed the model's actual context window before any compress
    // fires.
    if session.schema_version < 2 {
        session.recompute_all_estimates();
    }
}

pub fn delete_session(id: &str) -> anyhow::Result<()> {
    validate_session_id(id)?;
    let dir = session_dir();
    let path = dir.join(format!("{}.json", id));
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dirge-sn1k: under test, the data dir must route to a per-process
    /// temp location (never the user's real ~/.../dirge), so persistence
    /// tests can't leak fixtures into the recent-sessions list. (An
    /// explicit DIRGE_DATA_DIR override still wins.)
    #[test]
    fn test_data_dir_is_isolated_to_temp() {
        if std::env::var_os("DIRGE_DATA_DIR").is_some() {
            return; // explicit override — isolation is the caller's choice
        }
        let p = dirs_path();
        assert!(
            p.starts_with(std::env::temp_dir()),
            "test data dir must be under temp, got {p:?}"
        );
        assert!(p.to_string_lossy().contains("dirge-test-data"), "got {p:?}");
        // And the real data dir must NOT be the session location in tests.
        assert!(
            !session_dir()
                .to_string_lossy()
                .contains("Application Support/dirge")
                && !session_dir()
                    .to_string_lossy()
                    .ends_with(".local/share/dirge/sessions"),
            "session_dir leaked to the real data dir: {:?}",
            session_dir()
        );
    }

    #[test]
    fn validate_session_id_accepts_uuids() {
        assert!(validate_session_id("a1b2c3d4-e5f6-7890-abcd-ef1234567890").is_ok());
        assert!(validate_session_id("plain-id").is_ok());
        assert!(validate_session_id("2024.session").is_ok());
        assert!(validate_session_id("session_42").is_ok());
    }

    /// Review #2: v1 → v2 migration recomputes
    /// `estimated_tokens` because pre-9a044ce sessions counted
    /// only assistant TEXT. A v1 session JSON with under-counted
    /// values must come up with the new (correct) higher count.
    #[test]
    fn v1_to_v2_recomputes_under_counted_estimates() {
        use crate::session::{MessageRole, Session, SessionMessage, ToolCallEntry, ToolCallState};
        // Build a v1-shape session manually with a tool call whose
        // result is 8000 chars but estimated_tokens reflects only
        // the assistant text (5 chars / 4 = 1).
        let mut s = Session::new("p", "m", 128_000);
        // Forcibly create a message that mimics the pre-9a044ce
        // accounting (skip add_message_with_tool_calls' new logic).
        let tc = ToolCallEntry {
            id: "t1".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"command": "..."}),
            state: ToolCallState::Completed {
                result: "x".repeat(8000),
            },
        };
        let msg = SessionMessage {
            role: MessageRole::Assistant,
            content: compact_str::CompactString::new("hello"),
            estimated_tokens: 1, // ← under-counted on purpose
            id: compact_str::CompactString::new("m1"),
            timestamp: 1,
            tool_calls: vec![tc],
        };
        s.messages.push(msg.clone());
        s.message_store
            .insert(compact_str::CompactString::new("m1"), msg);
        s.total_estimated_tokens = 1;
        s.schema_version = 1;
        // Apply migration.
        migrate_session(&mut s);
        // After migration: total reflects text + args + result + name + 16.
        assert!(
            s.total_estimated_tokens >= 1900,
            "migration must recompute estimates; got {}",
            s.total_estimated_tokens,
        );
        // Per-message field also corrected.
        assert!(s.messages[0].estimated_tokens >= 1900);
    }

    /// F8: pre-F8 session files (no `schema_version` field) load
    /// with `schema_version` defaulted to 0, then get migrated up
    /// to `SCHEMA_VERSION`. The migration is idempotent and
    /// transparent for current schema (no data shape changes
    /// between v0 and v1).
    #[test]
    fn load_session_migrates_pre_f8_files() {
        // Write a minimal pre-F8 session JSON without the
        // schema_version field to a temp session id, then load.
        let id = format!("dirge-test-load-{}", std::process::id());
        let dir = session_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.json", id));
        std::fs::write(
            &path,
            r#"{
                "id": "dirge-test-load-pre-f8",
                "name": "",
                "messages": [],
                "compactions": [],
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "total_tokens": 0,
                "total_cost": 0.0,
                "total_estimated_tokens": 0,
                "context_window": 100000,
                "model": "test-model",
                "provider": "test",
                "working_dir": "/tmp"
            }"#,
        )
        .unwrap();

        let result = load_session(&id);
        let _ = std::fs::remove_file(&path);

        let session = result.expect("pre-F8 file must load");
        assert_eq!(
            session.schema_version,
            crate::session::SCHEMA_VERSION,
            "migration must bump schema_version",
        );
        assert_eq!(session.model, "test-model");
    }

    /// F8: a truncated JSON file surfaces a CLEAR error mentioning
    /// the file path. Previously the user got
    /// `expected ',' or '}' at line N column M` with no file
    /// context, making it hard to identify which session was
    /// broken when many existed.
    #[test]
    fn load_session_corrupted_file_includes_path_in_error() {
        let id = format!("dirge-test-corrupt-{}", std::process::id());
        let dir = session_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.json", id));
        // Truncated JSON.
        std::fs::write(&path, r#"{"id": "x", "name":"#).unwrap();

        let err = load_session(&id).expect_err("truncated file must error");
        let _ = std::fs::remove_file(&path);

        let msg = format!("{:?}", err);
        assert!(
            msg.contains(&id) || msg.contains("failed to parse"),
            "error must reference the file: {msg}",
        );
    }

    #[test]
    fn validate_session_id_rejects_traversal() {
        assert!(validate_session_id("../../../etc/passwd").is_err());
        assert!(validate_session_id("..\\windows").is_err());
        assert!(validate_session_id("..").is_err());
        assert!(validate_session_id(".").is_err());
        assert!(validate_session_id("a/b").is_err());
        assert!(validate_session_id("a\\b").is_err());
        assert!(validate_session_id("").is_err());
        // Null bytes, newlines, spaces — anything non-id-shaped.
        assert!(validate_session_id("foo bar").is_err());
        assert!(validate_session_id("foo\nbar").is_err());
    }

    /// Batch2-3: when another writer's mtime is newer than ours
    /// at save time, the save diverts to a `.conflict-<ts>.json`
    /// sibling and returns an error so the UI surfaces a warning.
    /// The original on-disk file is preserved (so the other
    /// instance doesn't lose its work).
    #[test]
    fn save_session_diverts_to_conflict_on_concurrent_write() {
        use crate::session::Session;

        // Use a deterministic test id so cleanup is easy + tests
        // can run in parallel without colliding (each test thread
        // picks a unique id).
        let id = format!(
            "test-conflict-{}",
            std::process::id() as u64 * 1000
                + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos() as u64
        );
        let mut sess = Session::new("openrouter", "test-model", 128_000);
        sess.id = compact_str::CompactString::from(id.clone());

        // First write — establishes the on-disk file with mtime T0.
        save_session(&sess).expect("first save");

        // Simulate "loaded earlier": set loaded_mtime to T0 - 1s so
        // the on-disk mtime is necessarily newer. (We could also
        // sleep + re-save to advance the on-disk mtime; the sub-
        // second approach keeps the test fast.)
        sess.loaded_mtime = Some(std::time::SystemTime::now() - std::time::Duration::from_secs(60));

        // Second save with stale loaded_mtime — should detect the
        // newer on-disk file and divert.
        let result = save_session(&sess);
        assert!(result.is_err(), "expected conflict error");
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("modified by another"), "got: {err_msg}");
        assert!(err_msg.contains(".conflict-"), "got: {err_msg}");

        // Cleanup: remove both the original + conflict files.
        let dir = session_dir();
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&format!("{id}.conflict-")))
                .unwrap_or(false)
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    /// Fresh save (loaded_mtime = None) doesn't trigger the
    /// conflict check — first-write case must succeed.
    #[test]
    fn save_session_fresh_no_conflict_check() {
        use crate::session::Session;
        let id = format!(
            "test-fresh-{}",
            std::process::id() as u64 * 1000
                + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos() as u64
        );
        let mut sess = Session::new("openrouter", "test-model", 128_000);
        sess.id = compact_str::CompactString::from(id.clone());
        assert!(sess.loaded_mtime.is_none());
        save_session(&sess).expect("fresh save must succeed");
        let dir = session_dir();
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
    }

    // --- Integration: full persistence round-trips -------------------

    fn unique_test_id(prefix: &str) -> String {
        format!(
            "test-{prefix}-{}",
            std::process::id() as u64 * 1000
                + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos() as u64
        )
    }

    fn cleanup_session(id: &str) {
        let dir = session_dir();
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        // Also clean any conflict files.
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(id))
                .unwrap_or(false)
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    /// Full cycle: new session → add messages → save → load → verify.
    #[test]
    fn roundtrip_session_with_messages_survives_save_and_load() {
        use crate::session::{MessageRole, Session};
        let id = unique_test_id("roundtrip-msgs");
        let mut s = Session::new("anthropic", "claude-opus", 200_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.add_message(MessageRole::User, "what is the answer?");
        s.add_message(MessageRole::Assistant, "the answer is 42");
        let orig_msg_count = s.messages.len();
        let orig_tokens = s.total_estimated_tokens;

        save_session(&s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        assert_eq!(loaded.messages.len(), orig_msg_count);
        assert_eq!(loaded.messages[0].role, MessageRole::User);
        assert_eq!(loaded.messages[0].content.as_str(), "what is the answer?");
        assert_eq!(loaded.messages[1].role, MessageRole::Assistant);
        assert_eq!(loaded.messages[1].content.as_str(), "the answer is 42");
        assert_eq!(loaded.total_estimated_tokens, orig_tokens);
        assert_eq!(loaded.model, "claude-opus");
        assert_eq!(loaded.provider, "anthropic");
        assert_eq!(loaded.context_window, 200_000);
        assert!(loaded.loaded_mtime.is_some(), "load must record mtime");
    }

    /// Messages with structured tool calls must survive the round-trip.
    #[test]
    fn roundtrip_tool_calls_survive_save_and_load() {
        use crate::session::{MessageRole, Session, ToolCallEntry, ToolCallState};
        let id = unique_test_id("roundtrip-tools");
        let mut s = Session::new("openai", "gpt-4", 128_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.add_message(MessageRole::User, "read the file");
        s.add_message_with_tool_calls(
            MessageRole::Assistant,
            "let me check",
            vec![ToolCallEntry {
                id: "call_abc".to_string(),
                name: "read".to_string(),
                args: serde_json::json!({"path": "/tmp/data.txt"}),
                state: ToolCallState::Completed {
                    result: "hello world\n".to_string(),
                },
            }],
        );

        save_session(&s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        let last = loaded.messages.last().unwrap();
        assert_eq!(last.tool_calls.len(), 1);
        assert_eq!(last.tool_calls[0].id, "call_abc");
        assert_eq!(last.tool_calls[0].name, "read");
        match &last.tool_calls[0].state {
            ToolCallState::Completed { result } => {
                assert_eq!(result, "hello world\n");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// Permission allowlist entries survive save/load.
    #[test]
    fn roundtrip_permission_allowlist_survives() {
        use crate::session::{PermissionAllowEntry, Session};
        let id = unique_test_id("roundtrip-perm");
        let mut s = Session::new("openrouter", "test", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.permission_allowlist.push(PermissionAllowEntry {
            tool: "bash".to_string(),
            pattern: "cargo *".to_string(),
        });
        s.permission_allowlist.push(PermissionAllowEntry {
            tool: "read".to_string(),
            pattern: "/tmp/**".to_string(),
        });

        save_session(&s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        assert_eq!(loaded.permission_allowlist.len(), 2);
        assert_eq!(loaded.permission_allowlist[0].tool, "bash");
        assert_eq!(loaded.permission_allowlist[0].pattern, "cargo *");
        assert_eq!(loaded.permission_allowlist[1].tool, "read");
        assert_eq!(loaded.permission_allowlist[1].pattern, "/tmp/**");
    }

    /// Compaction records persist through save/load.
    #[test]
    fn roundtrip_compaction_persists() {
        use crate::session::{MessageRole, Session};
        let id = unique_test_id("roundtrip-compact");
        let mut s = Session::new("p", "m", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.add_message(MessageRole::User, "long conversation part 1");
        s.add_message(MessageRole::Assistant, "reply 1");
        s.add_message(MessageRole::User, "long conversation part 2");
        s.add_message(MessageRole::Assistant, "reply 2");
        s.compress("summary of first 4 messages".to_string(), 4, 50);

        save_session(&s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        assert_eq!(loaded.compactions.len(), 1);
        assert_eq!(
            loaded.compactions[0].summary.as_str(),
            "summary of first 4 messages"
        );
        assert_eq!(loaded.compactions[0].summarized_count, 4);
        // After compress: summary at index 0, then any remaining messages.
        assert_eq!(loaded.messages[0].role, MessageRole::System);
        assert!(
            loaded.messages[0]
                .content
                .contains("summary of first 4 messages")
        );
    }

    /// Tree + message_store round-trip (fork/clone/switch surfaces
    /// the same branches after reload).
    #[test]
    fn roundtrip_tree_and_message_store_survives() {
        use crate::session::{MessageRole, Session};
        let id = unique_test_id("roundtrip-tree");
        let mut s = Session::new("p", "m", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.add_message(MessageRole::User, "root question");
        s.add_message(MessageRole::Assistant, "first answer");
        let fork_target = s.messages[1].id.clone();
        s.fork_at(&fork_target).expect("fork");
        s.add_message(MessageRole::Assistant, "alternate answer");

        save_session(&s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        // Tree must contain all 3 messages' nodes (root + fork-original
        // + alternate). Messages shows only the active branch (2).
        assert_eq!(
            loaded.tree.entries.len(),
            3,
            "tree must hold all 3 nodes: got {}",
            loaded.tree.entries.len(),
        );
        assert_eq!(
            loaded.messages.len(),
            2,
            "active branch has 2 messages: got {}",
            loaded.messages.len(),
        );
        assert_eq!(
            loaded.message_store.len(),
            3,
            "store must hold all 3 messages (root + fork original + alternate): got {}",
            loaded.message_store.len(),
        );
        // Leaf points to current end.
        assert_eq!(loaded.tree.leaf_id.as_ref(), Some(&loaded.messages[1].id));
        // The original fork target is still in the store (not in messages).
        assert!(loaded.message_store.contains_key(&fork_target));
    }

    /// Plugin entries survive save/load round-trip.
    #[test]
    fn roundtrip_plugin_entries_survives() {
        use crate::session::Session;
        let id = unique_test_id("roundtrip-plugin");
        let mut s = Session::new("p", "m", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.append_plugin_entry("bookmark", "save point 1", true);
        s.append_plugin_entry("stats", r#"{"tokens": 500}"#, false);

        save_session(&s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        assert_eq!(loaded.extra_entries.len(), 2);
        assert_eq!(loaded.extra_entries[0].custom_type, "bookmark");
        assert_eq!(loaded.extra_entries[0].data, "save point 1");
        assert!(loaded.extra_entries[0].display);
        assert_eq!(loaded.extra_entries[1].custom_type, "stats");
        assert!(!loaded.extra_entries[1].display);
        // seq values are monotonic and unique.
        assert!(loaded.extra_entries[0].seq < loaded.extra_entries[1].seq);
        assert!(loaded.next_entry_seq >= 2);
    }

    /// Session metadata (schema_version, id, name, created_at) survives
    /// a double save — the second save updates updated_at but preserves
    /// the rest.
    #[test]
    fn roundtrip_resave_preserves_metadata() {
        use crate::session::Session;
        let id = unique_test_id("roundtrip-resave");
        let mut s = Session::new("openrouter", "test", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        let orig_created = s.created_at.clone();
        save_session(&s).expect("save");

        // Load, modify, re-save.
        let mut loaded = load_session(&id).expect("load");
        loaded.add_message(crate::session::MessageRole::User, "added after reload");
        save_session(&loaded).expect("resave");

        let reloaded = load_session(&id).expect("reload");
        cleanup_session(&id);

        assert_eq!(reloaded.id, loaded.id);
        assert_eq!(reloaded.created_at, orig_created);
        assert_eq!(reloaded.messages.len(), 1);
        assert_eq!(reloaded.messages[0].content.as_str(), "added after reload");
        // updated_at must advance (or at least be present).
        assert!(!reloaded.updated_at.is_empty());
    }

    /// Deleting a session then loading it returns an error with
    /// the file path in the message.
    #[test]
    fn delete_session_removes_file() {
        use crate::session::Session;
        let id = unique_test_id("roundtrip-delete");
        let mut s = Session::new("p", "m", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        save_session(&s).expect("save");

        delete_session(&id).expect("delete");
        let err = load_session(&id).expect_err("load after delete must fail");
        cleanup_session(&id); // best-effort — file already gone

        let msg = format!("{:?}", err);
        assert!(
            msg.contains("o such file") || msg.contains("No such file"),
            "error must mention missing file: {msg}"
        );
    }

    /// Branch summaries survive save/load (Phase 4 — pruned subtree
    /// records).
    #[test]
    fn roundtrip_branch_summaries_survive() {
        use crate::session::{BranchSummary, Session};
        let id = unique_test_id("roundtrip-branch");
        let mut s = Session::new("p", "m", 100_000);
        s.id = compact_str::CompactString::from(id.clone());
        s.branch_summaries.push(BranchSummary {
            root_id: compact_str::CompactString::from("root-1"),
            parent_id: compact_str::CompactString::from("parent-1"),
            message_count: 12,
            preview: "alternative approach...".to_string(),
            created_at: "2026-05-01T00:00:00Z".to_string(),
        });

        save_session(&s).expect("save");
        let loaded = load_session(&id).expect("load");
        cleanup_session(&id);

        assert_eq!(loaded.branch_summaries.len(), 1);
        assert_eq!(loaded.branch_summaries[0].root_id, "root-1");
        assert_eq!(loaded.branch_summaries[0].parent_id, "parent-1");
        assert_eq!(loaded.branch_summaries[0].message_count, 12);
        assert_eq!(
            loaded.branch_summaries[0].preview,
            "alternative approach..."
        );
    }
}

pub fn find_sessions_by_prefix(prefix: &str) -> anyhow::Result<Vec<Session>> {
    // SESS-5: `stem.starts_with("")` matches every session file, so
    // `/sessions ` or `/sessions delete ` (trailing space) would
    // enumerate or operate on ALL sessions instead of a prefix
    // match. Reject empty prefix so callers must supply at least
    // one character.
    if prefix.is_empty() {
        anyhow::bail!("session prefix must not be empty");
    }
    let dir = session_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut sessions: Vec<Session> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && stem.starts_with(prefix)
            && let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(session) = serde_json::from_str::<Session>(&json)
        {
            sessions.push(session);
        }
    }
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

pub fn find_recent_sessions(limit: usize) -> anyhow::Result<Vec<Session>> {
    let dir = session_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    // Audit L10: previously read + parsed every `*.json` then sorted
    // by `updated_at` then truncated. For a user with 5 000 stored
    // sessions this is 5 000 file reads + parses on every `/sessions`
    // invocation. Sort by filesystem mtime first (cheap; uses the
    // metadata already read by `read_dir`), then parse only the top
    // `limit`. mtime corresponds closely to `updated_at` since both
    // are bumped on every `save_session` write.
    let mut entries: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        entries.push((path, mtime));
    }
    // Newest first.
    entries.sort_by_key(|e| std::cmp::Reverse(e.1));
    entries.truncate(limit);

    let mut sessions: Vec<Session> = Vec::with_capacity(entries.len());
    for (path, _) in entries {
        if let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(session) = serde_json::from_str::<Session>(&json)
        {
            sessions.push(session);
        }
    }
    // Refine ordering by the in-file updated_at — mtime is a good
    // proxy but `updated_at` is canonical. Cheap on the already-
    // truncated list.
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

pub fn agents_path() -> PathBuf {
    config_path().join("agent").join("AGENTS.md")
}
