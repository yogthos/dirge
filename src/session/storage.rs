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
    let json = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&json)?)
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
