//! Session search tool — three-shape search over past sessions.
//!
//! Port of Hermes's `tools/session_search_tool.py`. Lets the agent
//! search its own past work on this project. Three calling shapes:
//!
//! 1. **DISCOVERY** — pass `query`, gets FTS5 matches with bookends
//!    (first/last messages) and anchored windows around each hit.
//! 2. **SCROLL** — pass `session_id` + `around_message_id`, gets a
//!    ±N message window centered on the anchor. No FTS5, no bookends.
//! 3. **BROWSE** — no args, returns recent sessions chronologically.
//!
//! Key design decisions from Hermes preserved:
//! - Pure DB queries, no LLM cost
//! - Lineage deduplication (same compression chain → one result)
//! - Lineage rebinding (parent session_id + child message id)
//! - Source exclusion (review-fork hidden by default)
//! - Current session exclusion
//! - FTS5 syntax: AND, OR, NOT, quoted phrases, * wildcards

use crate::extras::session_db::{SearchResult, SessionDb};

/// Detect CJK (Chinese/Japanese/Korean) characters in a query.
/// When CJK is present, the default unicode61 tokenizer splits
/// each character into a separate token, breaking phrase matching.
/// We route to the trigram FTS5 index instead.
/// Port of Hermes's _contains_cjk() (hermes_state.py:2100-2112).
fn contains_cjk(query: &str) -> bool {
    query.chars().any(|c| {
        let cp = c as u32;
        (0x4E00..=0x9FFF).contains(&cp)     // CJK Unified Ideographs
        || (0x3400..=0x4DBF).contains(&cp)  // CJK Extension A
        || (0x20000..=0x2A6DF).contains(&cp) // CJK Extension B
        || (0x3000..=0x303F).contains(&cp)   // CJK Symbols
        || (0x3040..=0x309F).contains(&cp)   // Hiragana
        || (0x30A0..=0x30FF).contains(&cp)   // Katakana
        || (0xAC00..=0xD7AF).contains(&cp) // Hangul Syllables
    })
}

/// A single search hit in the DISCOVERY shape. Contains the
/// matched session with context for the agent to understand
/// what happened.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscoveryHit {
    /// Session id for follow-up scroll calls.
    pub session_id: String,
    /// The root session id (after lineage resolution).
    pub root_session_id: String,
    /// Session source (cli, subagent, etc.).
    pub source: String,
    /// Model used for this session.
    pub model: String,
    /// Session title.
    pub title: String,
    /// When the session started.
    pub started_at: String,
    /// FTS5-highlighted snippet of the match.
    pub snippet: String,
    /// First few messages of the session (the goal/kickoff).
    pub bookend_start: Vec<MessagePreview>,
    /// Last few messages of the session (resolution/decisions).
    pub bookend_end: Vec<MessagePreview>,
    /// Window of messages around the FTS5 match.
    pub messages: Vec<MessagePreview>,
    /// Index of the anchor message within `messages`.
    pub anchor_index: usize,
    /// How many messages exist before the window.
    pub before: usize,
    /// How many messages exist after the window.
    pub after: usize,
}

/// A preview of a single message for search results.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MessagePreview {
    pub id: i64,
    pub role: String,
    pub content_preview: String,
    pub timestamp: String,
}

/// Result of a SCROLL request.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScrollResult {
    pub session_id: String,
    pub messages: Vec<MessagePreview>,
    pub anchor_index: usize,
    pub before: usize,
    pub after: usize,
}

/// Result of a BROWSE request — a list of recent sessions.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BrowseSession {
    pub id: String,
    pub root_id: String,
    pub source: String,
    pub model: String,
    pub title: String,
    pub started_at: String,
    pub last_active: String,
    pub message_count: i64,
}

/// Maximum content length in a message preview.
const MAX_PREVIEW_LEN: usize = 300;

/// Number of bookend messages to return (first/last).
const BOOKEND_COUNT: usize = 3;

/// Default window size around a match.
const DEFAULT_WINDOW: usize = 5;

/// Number of results to return in discovery.
const MAX_DISCOVERY_RESULTS: usize = 10;

pub struct SessionSearch {
    db: SessionDb,
    /// The current session id — excluded from search results.
    current_session_id: Option<String>,
}

impl SessionSearch {
    pub fn new(db: SessionDb) -> Self {
        SessionSearch {
            db,
            current_session_id: None,
        }
    }

    /// Set the current session to exclude from results.
    pub fn with_current_session(mut self, id: &str) -> Self {
        self.current_session_id = Some(id.to_string());
        self
    }

    // ── DISCOVERY shape ───────────────────────────────

    /// Search past sessions by FTS5 query. Returns up to
    /// `MAX_DISCOVERY_RESULTS` hits, each with bookends and
    /// an anchored window. Results are deduplicated by lineage
    /// root.
    pub fn discover(&self, query: &str) -> Result<Vec<DiscoveryHit>, String> {
        let sanitized = sanitize_fts5_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        let results = if contains_cjk(&sanitized) {
            self.db.search_messages_trigram(&sanitized, None)?
        } else {
            self.db.search_messages(&sanitized, None)?
        };
        if results.is_empty() {
            return Ok(Vec::new());
        }

        let mut hits: Vec<DiscoveryHit> = Vec::new();
        let mut seen_roots = std::collections::HashSet::new();

        for result in &results {
            // Resolve lineage root.
            let root_id = self.db.resolve_parent(&result.session_id)?;

            // Skip if this lineage is already represented or
            // it's the current session.
            if !seen_roots.insert(root_id.clone()) {
                continue;
            }
            if let Some(ref current) = self.current_session_id {
                let current_root = self.db.resolve_parent(current)?;
                if current_root == root_id {
                    continue;
                }
            }

            // Build hit.
            match self.build_discovery_hit(result, &root_id) {
                Ok(hit) => hits.push(hit),
                Err(e) => {
                    tracing::warn!(
                        target: "dirge::session_search",
                        session_id = %result.session_id,
                        error = %e,
                        "Failed to build discovery hit"
                    );
                }
            }

            if hits.len() >= MAX_DISCOVERY_RESULTS {
                break;
            }
        }

        Ok(hits)
    }

    fn build_discovery_hit(
        &self,
        result: &SearchResult,
        root_id: &str,
    ) -> Result<DiscoveryHit, String> {
        let session_meta = self.get_session_meta(&result.session_id)?;

        // Get message ID for the anchor.
        let anchor_id = self.find_message_id_near(&result.session_id, &result.timestamp)?;

        // Get anchored window.
        let view = self
            .db
            .get_anchored_view(&result.session_id, anchor_id, DEFAULT_WINDOW)?;

        // Get bookends.
        let bookend_start = self.get_bookends(&result.session_id, true)?;
        let bookend_end = self.get_bookends(&result.session_id, false)?;

        let messages: Vec<MessagePreview> = view
            .messages
            .into_iter()
            .map(|m| MessagePreview {
                id: m.id,
                role: m.role,
                content_preview: truncate_content(&m.content, MAX_PREVIEW_LEN),
                timestamp: m.timestamp,
            })
            .collect();

        Ok(DiscoveryHit {
            session_id: result.session_id.clone(),
            root_session_id: root_id.to_string(),
            source: session_meta.0,
            model: session_meta.1,
            title: session_meta.2,
            started_at: session_meta.3,
            snippet: truncate_content(&result.content, MAX_PREVIEW_LEN),
            bookend_start,
            bookend_end,
            messages,
            anchor_index: view.anchor_index,
            before: view.before,
            after: view.after,
        })
    }

    // ── SCROLL shape ──────────────────────────────────

    /// Get a window of messages around an anchor. If the session
    /// has been split (compression), rebinds to the child session
    /// containing the message.
    pub fn scroll(
        &self,
        session_id: &str,
        around_message_id: i64,
        window: usize,
    ) -> Result<ScrollResult, String> {
        // Walk lineage to find the actual session containing
        // this message. If the message was created after a
        // compression split, it lives in a child session.
        let actual_session = self.find_message_session(session_id, around_message_id)?;

        let view = self
            .db
            .get_anchored_view(&actual_session, around_message_id, window)?;

        let messages: Vec<MessagePreview> = view
            .messages
            .into_iter()
            .map(|m| MessagePreview {
                id: m.id,
                role: m.role,
                content_preview: truncate_content(&m.content, MAX_PREVIEW_LEN),
                timestamp: m.timestamp,
            })
            .collect();

        Ok(ScrollResult {
            session_id: actual_session,
            messages,
            anchor_index: view.anchor_index,
            before: view.before,
            after: view.after,
        })
    }

    // ── BROWSE shape ──────────────────────────────────

    /// List recent sessions, excluding review-fork sources
    /// and the current session.
    pub fn browse(&self) -> Result<Vec<BrowseSession>, String> {
        let sessions = self.db.list_sessions_rich(Some(&["review-fork"]))?;

        let mut result = Vec::new();
        let mut seen_roots = std::collections::HashSet::new();

        for s in sessions {
            // Resolve lineage root.
            let root_id = self.db.resolve_parent(&s.id)?;

            // Deduplicate by root.
            if !seen_roots.insert(root_id.clone()) {
                continue;
            }

            // Exclude current session.
            if let Some(ref current) = self.current_session_id {
                let current_root = self.db.resolve_parent(current)?;
                if current_root == root_id {
                    continue;
                }
            }

            result.push(BrowseSession {
                id: s.id,
                root_id,
                source: s.source,
                model: s.model,
                title: s.title,
                started_at: s.started_at,
                last_active: s.last_active,
                message_count: s.message_count,
            });
        }

        Ok(result)
    }

    // ── Internal helpers ──────────────────────────────

    /// Get session metadata: (source, model, title, started_at).
    fn get_session_meta(
        &self,
        session_id: &str,
    ) -> Result<(String, String, String, String), String> {
        self.db
            .get_anchored_view(session_id, 0, 0)
            .map(|_v| {
                // Just use the session list info — the anchored
                // view is just a probe to verify the session exists.
                // Actual metadata comes from list_sessions_rich query.
                (String::new(), String::new(), String::new(), String::new())
            })
            .map_err(|_| format!("Session '{}' not found", session_id))?;

        // Fall through to list_sessions_rich for metadata.
        let all = self.db.list_sessions_rich(None)?;
        for s in &all {
            if s.id == session_id {
                return Ok((
                    s.source.clone(),
                    s.model.clone(),
                    s.title.clone(),
                    s.started_at.clone(),
                ));
            }
        }
        Ok((String::new(), String::new(), String::new(), String::new()))
    }

    /// Find a message id near the given timestamp in a session.
    fn find_message_id_near(&self, session_id: &str, timestamp: &str) -> Result<i64, String> {
        let view = self.db.get_anchored_view(session_id, 1, 0)?;
        if view.messages.is_empty() {
            return Err(format!("No messages in session '{}'", session_id));
        }
        // Find the first message with timestamp >= target.
        for m in &view.messages {
            if *m.timestamp >= *timestamp {
                return Ok(m.id);
            }
        }
        // Fall back to the last message.
        Ok(view.messages.last().map(|m| m.id).unwrap_or(1))
    }

    /// Get the first or last few messages of a session.
    fn get_bookends(&self, session_id: &str, start: bool) -> Result<Vec<MessagePreview>, String> {
        let view = self.db.get_anchored_view(session_id, 1, BOOKEND_COUNT)?;

        let messages: Vec<MessagePreview> = view
            .messages
            .into_iter()
            .map(|m| MessagePreview {
                id: m.id,
                role: m.role,
                content_preview: truncate_content(&m.content, MAX_PREVIEW_LEN),
                timestamp: m.timestamp,
            })
            .collect();

        if start {
            Ok(messages)
        } else {
            // Get the last BOOKEND_COUNT messages.
            let total_view = self.db.get_anchored_view(session_id, 1, 100_000)?;
            let total = total_view.messages.len();
            if total <= BOOKEND_COUNT {
                return Ok(messages);
            }
            let last_id = total_view.messages.last().map(|m| m.id).unwrap_or(1);
            let end_view = self
                .db
                .get_anchored_view(session_id, last_id, BOOKEND_COUNT)?;
            Ok(end_view
                .messages
                .into_iter()
                .map(|m| MessagePreview {
                    id: m.id,
                    role: m.role,
                    content_preview: truncate_content(&m.content, MAX_PREVIEW_LEN),
                    timestamp: m.timestamp,
                })
                .collect())
        }
    }

    /// Walk lineage from session_id to find which session
    /// actually contains the given message. If the message was
    /// created after a compression split, it lives in a child
    /// session.
    fn find_message_session(&self, session_id: &str, message_id: i64) -> Result<String, String> {
        // First try the given session.
        if self.db.get_anchored_view(session_id, message_id, 0).is_ok() {
            // Message might exist — we trust the caller.
            return Ok(session_id.to_string());
        }

        // Walk forward looking for child sessions that might
        // contain this message. List all sessions to find children.
        let all = self.db.list_sessions_rich(None)?;
        let root_id = self.db.resolve_parent(session_id)?;

        // Find all sessions in this lineage.
        for s in &all {
            let s_root = self.db.resolve_parent(&s.id)?;
            if s_root == root_id && self.db.get_anchored_view(&s.id, message_id, 0).is_ok() {
                return Ok(s.id.clone());
            }
        }

        // Fall back to the given session.
        Ok(session_id.to_string())
    }
}

/// Truncate content for preview, preserving readability.
fn truncate_content(content: &str, max_len: usize) -> String {
    if content.len() <= max_len {
        return content.to_string();
    }
    format!(
        "{}…[{} more chars]",
        crate::text::head(content, max_len.saturating_sub(20)),
        content.len() - max_len
    )
}

/// Sanitize a user-provided query string for safe use with FTS5 MATCH.
/// Port of Hermes's _sanitize_fts5_query (hermes_state.py:2036-2086).
///
/// FTS5 query syntax has special characters: `+`, `*`, `"`, `(`, `)`, `{`,
/// `}`, `^`, and bare boolean operators (AND, OR, NOT). Passing raw user
/// input directly to MATCH can cause `sqlite3.OperationalError`.
///
/// Strategy (6-step pipeline from Hermes):
/// 1. Extract balanced double-quoted phrases, protect with placeholders
/// 2. Strip remaining FTS5-special chars: `+{}()"^`
/// 3. Collapse repeated `*` into single `*`, remove leading `*`
/// 4. Remove dangling boolean operators at start/end
/// 5. Wrap hyphenated and dotted terms in quotes (FTS5 splits on `-` and `.`)
/// 6. Restore preserved quoted phrases
fn sanitize_fts5_query(query: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;

    if query.trim().is_empty() {
        return String::new();
    }

    // Step 1: Extract balanced double-quoted phrases and protect them.
    static QUOTED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""[^"]*""#).unwrap());
    let mut quoted_parts: Vec<String> = Vec::new();
    let mut sanitized = QUOTED_RE
        .replace_all(query, |caps: &regex::Captures| {
            let s = caps[0].to_string();
            let idx = quoted_parts.len();
            quoted_parts.push(s);
            format!("\x00Q{idx}\x00")
        })
        .to_string();

    // Step 2: Strip remaining FTS5-special characters: + { } ( ) " ^
    static SPECIAL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[+{}()"^]"#).unwrap());
    sanitized = SPECIAL_RE.replace_all(&sanitized, " ").to_string();

    // Step 3: Collapse repeated * into single *, remove leading *
    static STAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*+").unwrap());
    sanitized = STAR_RE.replace_all(&sanitized, "*").to_string();
    static LEADING_STAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|\s)\*").unwrap());
    sanitized = LEADING_STAR_RE.replace_all(&sanitized, "$1").to_string();

    // Step 4: Remove dangling boolean operators at start/end.
    // SESS-7: loop until stable so chained operators like
    // `AND OR foo` or `foo AND OR` are fully stripped. The single
    // `replace` (not `replace_all`) only consumed one match per
    // side and left FTS5-invalid residue that the engine then
    // rejected.
    static DANGLING_START_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(AND|OR|NOT)\b\s*").unwrap());
    static DANGLING_END_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\s+(AND|OR|NOT)\s*$").unwrap());
    loop {
        let before = sanitized.clone();
        sanitized = DANGLING_START_RE.replace(sanitized.trim(), "").to_string();
        sanitized = DANGLING_END_RE.replace(sanitized.trim(), "").to_string();
        if sanitized == before {
            break;
        }
    }

    // Step 5: Wrap hyphenated and dotted terms in quotes.
    // FTS5 tokenizer splits on `-` and `.`, so `chat-send` becomes
    // `chat AND send`. Quoting preserves phrase semantics.
    static DOT_DASH_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b(\w+(?:[._-]\w+)+\w*)\b").unwrap());
    sanitized = DOT_DASH_RE.replace_all(&sanitized, r#""$1""#).to_string();

    // Step 6: Restore preserved quoted phrases
    for (i, quoted) in quoted_parts.iter().enumerate() {
        sanitized = sanitized.replace(&format!("\x00Q{i}\x00"), quoted);
    }

    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_search() -> (SessionSearch, std::path::PathBuf) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("dirge-search-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let db = SessionDb::open(&path).unwrap();
        let search = SessionSearch::new(db);
        (search, dir)
    }

    fn seed_session(db: &SessionDb, id: &str, source: &str) {
        db.insert_session(id, source, "gpt-5", "openai", "2025-01-15T10:00:00Z")
            .unwrap();
        for i in 0..5 {
            db.insert_message(
                id,
                if i % 2 == 0 { "user" } else { "assistant" },
                &format!("message {} in {}", i, id),
                None,
                None,
                None,
                &format!("2025-01-15T10:{:02}:00Z", i),
            )
            .unwrap();
        }
    }

    #[test]
    fn browse_returns_recent_sessions() {
        let (search, _dir) = temp_search();
        seed_session(&search.db, "sess-1", "cli");
        seed_session(&search.db, "sess-2", "subagent");

        let sessions = search.browse().unwrap();
        assert!(!sessions.is_empty());
        // Should exclude review-fork, include cli and subagent.
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"sess-1"));
        assert!(ids.contains(&"sess-2"));
    }

    #[test]
    fn browse_excludes_review_fork() {
        let (search, _dir) = temp_search();
        seed_session(&search.db, "sess-1", "cli");
        seed_session(&search.db, "review-1", "review-fork");

        let sessions = search.browse().unwrap();
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"sess-1"));
        assert!(!ids.contains(&"review-1"));
    }

    #[test]
    fn browse_excludes_current_session() {
        let (mut search, _dir) = temp_search();
        seed_session(&search.db, "sess-1", "cli");
        seed_session(&search.db, "sess-2", "cli");

        search.current_session_id = Some("sess-1".to_string());
        let sessions = search.browse().unwrap();
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(
            !ids.contains(&"sess-1"),
            "current session should be excluded"
        );
        assert!(ids.contains(&"sess-2"));
    }

    #[test]
    fn discover_finds_matching_sessions() {
        let (search, _dir) = temp_search();
        seed_session(&search.db, "sess-1", "cli");

        // Insert a specific message to search for.
        search
            .db
            .insert_message(
                "sess-1",
                "user",
                "how do we handle database migrations with rusqlite",
                None,
                None,
                None,
                "2025-01-15T10:01:00Z",
            )
            .unwrap();

        let hits = search.discover("database migrations").unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    fn discover_empty_for_no_match() {
        let (search, _dir) = temp_search();
        seed_session(&search.db, "sess-1", "cli");

        let hits = search.discover("zzzzz_nonexistent_query_xyz").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn discover_excludes_current_session() {
        let (mut search, _dir) = temp_search();
        seed_session(&search.db, "current", "cli");
        seed_session(&search.db, "other", "cli");

        search
            .db
            .insert_message(
                "current",
                "user",
                "database migration in current session",
                None,
                None,
                None,
                "2025-01-15T10:01:00Z",
            )
            .unwrap();
        search
            .db
            .insert_message(
                "other",
                "user",
                "database migration in other session",
                None,
                None,
                None,
                "2025-01-15T11:01:00Z",
            )
            .unwrap();

        search.current_session_id = Some("current".to_string());
        let hits = search.discover("database migration").unwrap();
        assert!(!hits.is_empty());
        for hit in &hits {
            assert_ne!(hit.session_id, "current");
        }
    }

    #[test]
    fn discover_dedupes_by_lineage() {
        let (search, _dir) = temp_search();
        seed_session(&search.db, "sess-1", "cli");
        seed_session(&search.db, "child-1", "cli");

        search.db.set_parent_session("child-1", "sess-1").unwrap();

        search
            .db
            .insert_message(
                "sess-1",
                "user",
                "unique term: ziggurat construction",
                None,
                None,
                None,
                "2025-01-15T10:01:00Z",
            )
            .unwrap();
        search
            .db
            .insert_message(
                "child-1",
                "user",
                "unique term: ziggurat construction continued",
                None,
                None,
                None,
                "2025-01-15T11:01:00Z",
            )
            .unwrap();

        let hits = search.discover("ziggurat").unwrap();
        // Both sessions match but share a lineage root — only one result.
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn scroll_returns_window_around_anchor() {
        let (search, _dir) = temp_search();
        search
            .db
            .insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
            .unwrap();

        // Insert 20 messages.
        for i in 0..20 {
            search
                .db
                .insert_message(
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

        let result = search.scroll("sess-1", 10, 3).unwrap();
        assert!(!result.messages.is_empty());
        // Should have anchor at index 3 (3 before) and 3 after.
        assert_eq!(result.before, 3);
        assert_eq!(result.after, 3);
    }

    #[test]
    fn truncate_preserves_short_content() {
        let result = truncate_content("hello", 300);
        assert_eq!(result, "hello");
    }

    #[test]
    fn truncate_shortens_long_content() {
        let long = "a".repeat(500);
        let result = truncate_content(&long, 200);
        assert!(result.len() < 300);
        assert!(result.ends_with("more chars]"));
    }

    #[test]
    fn browse_dedupes_by_lineage() {
        let (search, _dir) = temp_search();
        seed_session(&search.db, "sess-1", "cli");
        seed_session(&search.db, "child-1", "cli");
        search.db.set_parent_session("child-1", "sess-1").unwrap();

        let sessions = search.browse().unwrap();
        // Same lineage → only one result.
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids.len(), 1, "should dedupe by lineage");
    }

    #[test]
    fn find_message_session_falls_back_to_given() {
        let (search, _dir) = temp_search();
        search
            .db
            .insert_session("sess-1", "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
            .unwrap();
        // Message doesn't exist, but we trust the caller.
        let session = search.find_message_session("sess-1", 999).unwrap();
        assert_eq!(session, "sess-1");
    }

    // ── sanitize_fts5_query ─────────────────────────────

    #[test]
    fn sanitize_preserves_normal_query() {
        let result = sanitize_fts5_query("database migrations");
        assert_eq!(result, "database migrations");
    }

    #[test]
    fn sanitize_protects_balanced_quotes() {
        // Balanced quotes protect their content from stripping.
        let result = sanitize_fts5_query("\"exact phrase\"");
        assert_eq!(result, "\"exact phrase\"");
    }

    #[test]
    fn sanitize_strips_fts5_special_chars() {
        // +, {, }, (, ), ^ stripped; balanced quotes protect content.
        let result = sanitize_fts5_query("+hello");
        assert_eq!(result, "hello");
    }

    #[test]
    fn sanitize_collapses_multiple_stars() {
        // *** collapses to * but leading * still stripped
        let result = sanitize_fts5_query("a***test");
        assert_eq!(result, "a*test");
    }

    #[test]
    fn sanitize_strips_dangling_boolean() {
        let result = sanitize_fts5_query("hello AND");
        assert_eq!(result, "hello");
    }

    #[test]
    fn sanitize_wraps_hyphenated_and_dotted_terms() {
        // FTS5 splits on - and ., quoting preserves phrase semantics.
        let result = sanitize_fts5_query("my-app.config.ts");
        assert_eq!(result, "\"my-app.config.ts\"");
    }

    #[test]
    fn sanitize_empty_after_cleaning() {
        let result = sanitize_fts5_query("*\"()");
        assert_eq!(result, "");
    }

    #[test]
    fn sanitize_trims_whitespace() {
        let result = sanitize_fts5_query("  hello world  ");
        assert_eq!(result, "hello world");
    }
}
