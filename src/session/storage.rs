use std::path::PathBuf;

use crate::session::Session;

fn session_dir() -> PathBuf {
    dirs_path().join("sessions")
}

fn home_fallback() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn dirs_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("DIRGE_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let base = dirs::data_dir().unwrap_or_else(home_fallback);
    base.join("dirge")
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
    let dir = session_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", session.id));
    let json = serde_json::to_string_pretty(session)?;
    // Atomic write: write to a sibling temp file, fsync, then rename
    // over the target. A crash mid-write leaves the temp behind but
    // never a truncated `.json`. The rename is atomic on every OS we
    // target. Use the same parent dir so rename stays on one filesystem.
    //
    // The tmp filename includes a per-call nonce (pid + nanos +
    // monotonic counter) so two concurrent saves of the same session
    // id don't collide on the tmp file. The counter is the
    // load-bearing piece — two threads firing in the same nanosecond
    // still get distinct counter values, eliminating same-process
    // collisions. The rename race remains harmless (last writer wins
    // on the target; each tmp is fully written before rename).
    static SAVE_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        SAVE_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    );
    let tmp = dir.join(format!(".{}.{}.json.tmp", session.id, nonce));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        // Best-effort fsync; non-fatal if the platform doesn't support it.
        let _ = f.sync_all();
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

pub fn load_session(id: &str) -> anyhow::Result<Session> {
    validate_session_id(id)?;
    let dir = session_dir();
    let path = dir.join(format!("{}.json", id));
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
    let _ = session;
    // No-op for v0 → v1. Future migrations gate on
    // `if session.schema_version < N` checks.
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

    #[test]
    fn validate_session_id_accepts_uuids() {
        assert!(validate_session_id("a1b2c3d4-e5f6-7890-abcd-ef1234567890").is_ok());
        assert!(validate_session_id("plain-id").is_ok());
        assert!(validate_session_id("2024.session").is_ok());
        assert!(validate_session_id("session_42").is_ok());
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
}

pub fn find_sessions_by_prefix(prefix: &str) -> anyhow::Result<Vec<Session>> {
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
    let mut sessions: Vec<Session> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(session) = serde_json::from_str::<Session>(&json)
        {
            sessions.push(session);
        }
    }
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions.truncate(limit);
    Ok(sessions)
}

pub fn agents_path() -> PathBuf {
    config_path().join("agent").join("AGENTS.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_session_id_accepts_uuids() {
        assert!(validate_session_id("a1b2c3d4-e5f6-7890-abcd-ef1234567890").is_ok());
        assert!(validate_session_id("plain-id").is_ok());
        assert!(validate_session_id("2024.session").is_ok());
        assert!(validate_session_id("session_42").is_ok());
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
}
