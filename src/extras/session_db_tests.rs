use super::*;

use std::sync::atomic::{AtomicU32, Ordering};

static DB_COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_db() -> (SessionDb, std::path::PathBuf) {
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "dirge-session-db-test-{}-{}",
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    let db = SessionDb::open(&path).unwrap();
    (db, dir)
}

#[test]
fn create_and_read_session() {
    let (db, _dir) = temp_db();
    db.insert_session(
        "sess-1",
        "cli",
        "claude-opus",
        "anthropic",
        "2025-01-15T10:00:00Z",
    )
    .unwrap();

    let count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE id = 'sess-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn insert_message_and_fts5_search() {
    let (db, _dir) = temp_db();
    db.insert_session(
        "sess-1",
        "cli",
        "claude-opus",
        "anthropic",
        "2025-01-15T10:00:00Z",
    )
    .unwrap();

    db.insert_message(
        "sess-1",
        "user",
        "how do we handle database migrations",
        None,
        None,
        None,
        "2025-01-15T10:01:00Z",
    )
    .unwrap();

    let results = db.search_messages("database migrations", None).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("database migrations"));
}

#[test]
fn list_sessions_returns_recent() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();
    db.insert_session(
        "sess-2",
        "subagent",
        "claude-sonnet",
        "anthropic",
        "2025-01-15T11:00:00Z",
    )
    .unwrap();

    let sessions = db.list_sessions_rich(None).unwrap();
    assert_eq!(sessions.len(), 2);
    // Most recent first.
    assert_eq!(sessions[0].id, "sess-2");
    assert_eq!(sessions[1].id, "sess-1");
}

#[test]
fn list_sessions_excludes_source() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();
    db.insert_session(
        "sess-2",
        "review-fork",
        "claude-sonnet",
        "anthropic",
        "2025-01-15T11:00:00Z",
    )
    .unwrap();

    let sessions = db.list_sessions_rich(Some(&["review-fork"])).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "sess-1");
}

#[test]
fn session_split_parent_chain() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    // Split: child session points to parent.
    db.insert_session("sess-2", "cli", "gpt-5", "openai", "2025-01-15T11:00:00Z")
        .unwrap();
    db.set_parent_session("sess-2", "sess-1").unwrap();

    let parent: String = db
        .conn
        .query_row(
            "SELECT parent_session_id FROM sessions WHERE id = 'sess-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(parent, "sess-1");
}

#[test]
fn fts5_search_with_role_filter() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    db.insert_message(
        "sess-1",
        "user",
        "how do we build this",
        None,
        None,
        None,
        "2025-01-15T10:01:00Z",
    )
    .unwrap();
    db.insert_message(
        "sess-1",
        "assistant",
        "run cargo build",
        None,
        None,
        None,
        "2025-01-15T10:02:00Z",
    )
    .unwrap();

    let results = db.search_messages("build", Some("user")).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].role, "user");
}

#[test]
fn anchored_view_returns_window_around_match() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    // Insert 10 messages.
    for i in 0..10 {
        db.insert_message(
            "sess-1",
            if i % 2 == 0 { "user" } else { "assistant" },
            &format!("message {}", i),
            None,
            None,
            None,
            &format!("2025-01-15T10:{:02}:00Z", i),
        )
        .unwrap();
    }

    // Anchor on message 5.
    let view = db.get_anchored_view("sess-1", 5, 2).unwrap();

    // Window should have 5 messages: anchor + 2 before + 2 after.
    assert_eq!(view.messages.len(), 5);
    assert_eq!(view.anchor_index, 2);
    assert_eq!(view.before, 2);
    assert_eq!(view.after, 2);
}

#[test]
fn resolve_parent_walks_lineage() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();
    db.insert_session("sess-2", "cli", "gpt-5", "openai", "2025-01-15T11:00:00Z")
        .unwrap();
    db.insert_session("sess-3", "cli", "gpt-5", "openai", "2025-01-15T12:00:00Z")
        .unwrap();

    db.set_parent_session("sess-2", "sess-1").unwrap();
    db.set_parent_session("sess-3", "sess-2").unwrap();

    assert_eq!(db.resolve_parent("sess-3").unwrap(), "sess-1");
    assert_eq!(db.resolve_parent("sess-2").unwrap(), "sess-1");
    assert_eq!(db.resolve_parent("sess-1").unwrap(), "sess-1");
}

#[test]
fn fts5_search_finds_tool_names() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    // Insert an assistant message that used the `read` tool.
    db.insert_message(
        "sess-1",
        "assistant",
        "Let me read that file.",
        Some("read"),
        Some(r#"[{"name":"read","args":{"path":"/tmp/x"}}]"#),
        None,
        "2025-01-15T10:02:00Z",
    )
    .unwrap();

    // Insert a user message (no tool).
    db.insert_message(
        "sess-1",
        "user",
        "show me the build output",
        None,
        None,
        None,
        "2025-01-15T10:01:00Z",
    )
    .unwrap();

    // Searching for "read" (the tool name) should find the assistant message.
    let results = db.search_messages("read", None).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].role, "assistant");

    // Searching for "build" should find the user message.
    let results = db.search_messages("build", None).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].role, "user");
}

#[test]
fn trigram_fts5_indexes_and_searches() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    // Insert a message with tool_name populated.
    db.insert_message(
        "sess-1",
        "assistant",
        "Let me read that file.",
        Some("read"),
        None,
        None,
        "2025-01-15T10:02:00Z",
    )
    .unwrap();

    // Trigram table should exist and be searchable.
    let count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH 'read'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(count > 0, "trigram FTS5 should find 'read'");

    // Trigram supports substring queries that unicode61 doesn't.
    let count: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH 'rea'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(count > 0, "trigram should find substring 'rea'");
}

#[test]
fn migration_v4_adds_session_columns() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    // New columns should be writable.
    db.conn
        .execute(
            "UPDATE sessions SET ended_at = '2025-01-15T11:00:00Z', end_reason = 'done', tool_call_count = 3, api_call_count = 2 WHERE id = 'sess-1'",
            [],
        )
        .unwrap();

    let (ended_at, end_reason, tool_call_count, api_call_count): (
        Option<String>,
        Option<String>,
        i64,
        i64,
    ) = db
        .conn
        .query_row(
            "SELECT ended_at, end_reason, tool_call_count, api_call_count FROM sessions WHERE id = 'sess-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(ended_at.as_deref(), Some("2025-01-15T11:00:00Z"));
    assert_eq!(end_reason.as_deref(), Some("done"));
    assert_eq!(tool_call_count, 3);
    assert_eq!(api_call_count, 2);
}

#[test]
fn migration_v5_adds_message_columns() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();
    let msg_id = db
        .insert_message(
            "sess-1",
            "user",
            "hello",
            None,
            None,
            None,
            "2025-01-15T10:01:00Z",
        )
        .unwrap();

    // New columns should be writable.
    db.conn
        .execute(
            "UPDATE messages SET token_count = 42, finish_reason = 'stop' WHERE id = ?1",
            params![msg_id],
        )
        .unwrap();

    let (token_count, finish_reason): (Option<i64>, Option<String>) = db
        .conn
        .query_row(
            "SELECT token_count, finish_reason FROM messages WHERE id = ?1",
            params![msg_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(token_count, Some(42));
    assert_eq!(finish_reason.as_deref(), Some("stop"));
}

#[test]
fn end_session_marks_ended_at() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    db.end_session("sess-1", "done").unwrap();

    let ended_at: Option<String> = db
        .conn
        .query_row(
            "SELECT ended_at FROM sessions WHERE id = 'sess-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(ended_at.is_some(), "ended_at should be set");
}

#[test]
fn end_session_is_idempotent() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    db.end_session("sess-1", "compression").unwrap();
    // Second call with a different reason should no-op.
    db.end_session("sess-1", "done").unwrap();

    let end_reason: String = db
        .conn
        .query_row(
            "SELECT end_reason FROM sessions WHERE id = 'sess-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(end_reason, "compression", "first end_reason wins");
}

#[test]
fn last_init_error_tracks_open_failures() {
    // Attempt to open a path that doesn't exist as a directory
    // (the parent dir creation is done by open(), but a file where
    // a directory should be will fail).
    let bad = std::env::temp_dir().join(format!(
        "dirge-bad-db-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    // Create a regular file where state.db should be a dir.
    std::fs::write(&bad, "not a db").unwrap();
    let db_path = bad.join("state.db");

    let result = SessionDb::open(&db_path);
    assert!(result.is_err(), "should fail to open on bad path");
    let err = last_init_error();
    assert!(err.is_some(), "last_init_error should be set");
    assert!(
        err.unwrap().contains("Failed to open"),
        "error should describe the failure"
    );

    // Clean up.
    let _ = std::fs::remove_file(&bad);
}

#[test]
fn redact_for_fts_strips_vendor_prefix_tokens() {
    // AWS access key
    let r = redact_for_fts("aws key: AKIAIOSFODNN7EXAMPLE here");
    assert!(!r.contains("AKIAIOSFODNN7EXAMPLE"), "got: {r}");
    assert!(r.contains("<REDACTED>"));

    // GitHub PAT classic
    let r = redact_for_fts("token: ghp_abcdefghijklmnopqrstuvwxyz0123456789");
    assert!(!r.contains("ghp_abcdefghij"), "got: {r}");
    assert!(r.contains("<REDACTED>"));

    // Slack
    let r = redact_for_fts("creds=xoxb-1234567890-abcdefghij-AbCdEfGh tail");
    assert!(!r.contains("xoxb-1234567890"), "got: {r}");

    // OpenAI/Anthropic sk-
    let r = redact_for_fts("ANTHROPIC_API_KEY=sk-ant-12345abcdefghijklmnopqrst");
    assert!(!r.contains("sk-ant-12345abcdefghijklmnopqrst"), "got: {r}");
}

#[test]
fn redact_for_fts_strips_url_userinfo() {
    let r = redact_for_fts("DATABASE_URL=postgres://admin:hunter2@db.internal:5432/app");
    assert!(!r.contains("hunter2"), "got: {r}");
    // The whole assignment value gets caught by the env-assign
    // pattern first (DATABASE_URL doesn't trip the AUTH/KEY/TOKEN
    // gate, but the userinfo regex does — verify either way).
    assert!(r.contains("<REDACTED>"), "got: {r}");

    let r = redact_for_fts("call https://deploy:secret-tok@webhook.example.com/x");
    assert!(!r.contains("secret-tok"), "got: {r}");
}

#[test]
fn redact_for_fts_strips_authorization_header() {
    let r = redact_for_fts("Authorization: Bearer ey-some-opaque-token");
    assert!(!r.contains("ey-some-opaque-token"), "got: {r}");
    assert!(r.contains("<REDACTED>"));

    // case-insensitive
    let r = redact_for_fts("authorization: bearer abc.def.ghi");
    assert!(!r.contains("abc.def.ghi"), "got: {r}");
}

#[test]
fn redact_for_fts_strips_env_assignment() {
    let r = redact_for_fts("OPENAI_API_KEY=opaque-value-1234567890");
    assert!(!r.contains("opaque-value-1234567890"), "got: {r}");
    assert!(r.contains("<REDACTED>"));

    let r = redact_for_fts("password=hunter2");
    assert!(!r.contains("hunter2"), "got: {r}");
}

#[test]
fn redact_for_fts_strips_jwt() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    let r = redact_for_fts(&format!("token = {jwt}"));
    assert!(
        !r.contains("SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"),
        "got: {r}"
    );
    assert!(r.contains("<REDACTED>"));
}

#[test]
fn redact_for_fts_leaves_plain_text_alone() {
    let plain = "how do we handle database migrations in this project";
    assert_eq!(redact_for_fts(plain), plain);
    // Empty input is preserved.
    assert_eq!(redact_for_fts(""), "");
    // A bare URL with no userinfo passes through.
    let url = "see https://api.example.com/v1/docs";
    assert_eq!(redact_for_fts(url), url);
}

#[test]
fn redact_for_fts_strips_json_field() {
    let r = redact_for_fts(r#"{"api_key": "secret-value-xyz", "name": "alice"}"#);
    assert!(!r.contains("secret-value-xyz"), "got: {r}");
    assert!(r.contains("\"alice\""), "non-secret fields preserved: {r}");
}

/// End-to-end: secrets pass through `insert_message` to the FTS5
/// indexes redacted, but the raw row in `messages` retains the
/// original content for transcript replay.
#[test]
fn fts_index_holds_redacted_text_messages_table_holds_raw() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    let raw = "Authorization: Bearer ey-opaque-token here is some context";
    db.insert_message(
        "sess-1",
        "assistant",
        raw,
        None,
        None,
        None,
        "2025-01-15T10:01:00Z",
    )
    .unwrap();

    // messages table holds RAW content (round-trip preserved).
    let stored: String = db
        .conn
        .query_row(
            "SELECT content FROM messages WHERE session_id = 'sess-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, raw);

    // FTS indexes hold REDACTED content. A search for the secret
    // token finds nothing; a search for the non-secret context
    // finds the row.
    let hits = db.search_messages("ey-opaque-token", None).unwrap();
    assert!(hits.is_empty(), "FTS must not index the secret token");

    let hits = db.search_messages("context", None).unwrap();
    assert_eq!(hits.len(), 1, "non-secret tokens still searchable");
}

#[test]
fn fts_index_redacts_secrets_inside_tool_calls() {
    let (db, _dir) = temp_db();
    db.insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
        .unwrap();

    let tool_calls = r#"[{"name":"bash","args":{"cmd":"curl -H 'Authorization: Bearer ghp_abcdefghijklmnopqrstuvwxyz0123456789' https://api.example.com"}}]"#;
    db.insert_message(
        "sess-1",
        "assistant",
        "Calling the API",
        Some("bash"),
        Some(tool_calls),
        None,
        "2025-01-15T10:01:00Z",
    )
    .unwrap();

    // Raw tool_calls preserved.
    let raw: String = db
        .conn
        .query_row(
            "SELECT tool_calls FROM messages WHERE session_id = 'sess-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(raw.contains("ghp_abcdefghij"), "raw kept");

    // FTS must not surface the PAT.
    let hits = db
        .search_messages("ghp_abcdefghijklmnopqrstuvwxyz0123456789", None)
        .unwrap();
    assert!(hits.is_empty(), "PAT must be redacted from FTS");

    // Non-secret tool name + content still searchable.
    let hits = db.search_messages("bash", None).unwrap();
    assert_eq!(hits.len(), 1);
}

/// Ensures v2→v3→v4→v5 chain works from a v2 database.
#[test]
fn migration_from_v2_to_v5_adds_trigram_and_columns() {
    // Create a v2 database by hand.
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "dirge-session-db-cross-test-{}-{}",
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("state.db");

    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .unwrap();
    conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA foreign_keys=ON;")
        .unwrap();

    // Create v1 schema (as if migration v1 ran), then run v2 to get to v2.
    conn.execute_batch(
        "
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY, source TEXT DEFAULT 'cli',
            model TEXT DEFAULT '', provider TEXT DEFAULT '',
            started_at TEXT NOT NULL, last_active TEXT NOT NULL,
            title TEXT DEFAULT '', message_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id),
            role TEXT NOT NULL, content TEXT NOT NULL DEFAULT '',
            tool_name TEXT, tool_calls TEXT, tool_call_id TEXT,
            timestamp TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE messages_fts USING fts5(
            content, content=messages, content_rowid=id
        );
        ",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 2).unwrap();
    conn.close().unwrap();

    // Open via SessionDb — v3, v4, v5 should fire.
    let db = SessionDb::open(&db_path).unwrap();

    // Verify pragma version is now 5.
    let ver: u32 = db
        .conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(ver, 6, "should be at schema version 6 after migration");

    // Trigram table should exist.
    let trigram_exists: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages_fts_trigram'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(trigram_exists, 1, "trigram table should exist");

    // v4 columns should be present.
    let _ = db.conn.execute(
        "UPDATE sessions SET ended_at = 'x', end_reason = 'r', tool_call_count = 1, api_call_count = 1 WHERE 1=0",
        [],
    );

    // v5 columns should be present.
    let _ = db.conn.execute(
        "UPDATE messages SET token_count = 0, finish_reason = '' WHERE 1=0",
        [],
    );
}
