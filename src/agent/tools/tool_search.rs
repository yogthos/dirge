//! `tool_search` — dynamic tool-discovery meta-tool.
//!
//! Phase 3 / part 1 of `docs/AGENTIC_LOOP_PLAN.md`. When the
//! per-session `dynamic_tool_search` knob is on, the agent loop
//! ships ONLY this tool + a small always-on set in its tool defs
//! per turn. The model calls `tool_search(query)` to discover the
//! right tool; on the next turn, the harness injects that tool's
//! full schema into the request via the shared "loaded" set.
//!
//! ## Token-savings story
//!
//! Long sessions with MCP-heavy toolsets can spend 30%+ of every
//! request payload on tool-definition repetition. Filtering down
//! to a small fixed set + a query-driven expansion keeps the
//! request small without hiding tools from the model.
//!
//! ## Implementation
//!
//! Plain string similarity (substring containment + bigram overlap)
//! over tool name AND description. No NLP / embedding dep —
//! deliberately cheap. Returns top-K (default 8) results as JSON.
//!
//! Side effect: each returned name is inserted into the shared
//! `Arc<Mutex<HashSet<String>>>` filter set. The factory
//! (`rig_stream_factory::invoke_one_stream`) reads the SAME set
//! per request and surfaces those tools' definitions to the
//! model on the NEXT turn.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::Value;

use crate::agent::agent_loop::LoopTool;
use crate::agent::agent_loop::result::LoopToolResult;
use crate::agent::agent_loop::tool::{AbortSignal, LoopToolUpdate};

/// One tool's metadata as the search ranks it.
#[derive(Debug, Clone)]
pub struct ToolMeta {
    pub name: String,
    pub description: String,
    /// Short parameter summary, e.g. "{path: string, offset?: number}".
    /// Computed from the tool's JSON Schema at registry build time.
    pub parameter_summary: String,
}

/// Built-in tool name (the meta-tool itself).
pub const TOOL_SEARCH_NAME: &str = "tool_search";

/// Built-in default top-K. Keep small — the goal is "save tokens",
/// not "ship 30 results".
pub const DEFAULT_TOP_K: usize = 8;

/// The always-on set: tools the agent must be able to call without
/// going through `tool_search` first. UI / control tools the model
/// needs unconditionally + `tool_search` itself.
///
/// Keep this list short — every name here is a tool whose
/// definition ships every turn regardless of the filter.
pub const ALWAYS_ON_TOOLS: &[&str] = &[TOOL_SEARCH_NAME, "task_status", "write_todo_list"];

/// `tool_search` meta-tool. Registers as a LoopTool so it can
/// mutate the per-session loaded set during execution.
pub struct ToolSearchTool {
    /// Live registry of every tool that COULD be loaded, searched by
    /// `tool_search`. Seeded at session start in
    /// `agent::builder::build_loop_tools`. Behind a `Mutex` (was an
    /// immutable snapshot) so the background MCP loader can append
    /// late-connected tools via `AnyAgent::extend_loop_tools` and keep
    /// them search-gated rather than always-visible (dirge-tpx6). The
    /// lock is taken only on a `tool_search` call, so contention is nil.
    registry: Arc<Mutex<Vec<ToolMeta>>>,
    /// Per-session "loaded" set. The factory filter reads this
    /// (Arc-shared with `LoopConfig.tool_def_filter`). Tool calls
    /// here APPEND to it; once a tool is loaded, it stays loaded
    /// for the rest of the session.
    loaded: Arc<Mutex<HashSet<String>>>,
    /// Default top-K returned when the model omits `top_k`.
    top_k: usize,
}

impl std::fmt::Debug for ToolSearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolSearchTool")
            .field(
                "registry_size",
                &self.registry.lock().map(|r| r.len()).unwrap_or(0),
            )
            .field("top_k", &self.top_k)
            .finish()
    }
}

impl ToolSearchTool {
    pub fn new(registry: Arc<Mutex<Vec<ToolMeta>>>, loaded: Arc<Mutex<HashSet<String>>>) -> Self {
        Self {
            registry,
            loaded,
            top_k: DEFAULT_TOP_K,
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn with_top_k(mut self, top_k: usize) -> Self {
        self.top_k = top_k;
        self
    }

    /// Rank registry entries against `query` and return the top-K
    /// matches (owned clones — the registry lock isn't held past the
    /// call). Tests use this directly to assert rank ordering.
    #[allow(dead_code)]
    pub fn rank(&self, query: &str, top_k: usize) -> Vec<ToolMeta> {
        let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        rank_tools(&reg, query, top_k)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Expose the shared loaded-set Arc. `spawn_runner` needs the
    /// SAME Arc passed to the stream factory's filter so a tool
    /// the model just discovered shows up on the next turn's
    /// request. The Arc is cloned out (refcount bump) — cheap.
    #[allow(dead_code)]
    pub fn loaded_set(&self) -> Arc<Mutex<HashSet<String>>> {
        self.loaded.clone()
    }
}

#[derive(Deserialize)]
pub struct ToolSearchArgs {
    pub query: String,
    /// Optional override on the top-K. Defaults to
    /// `DEFAULT_TOP_K`. Clamped to [1, 20] to keep the response
    /// payload sane.
    #[serde(default)]
    pub top_k: Option<usize>,
}

impl LoopTool for ToolSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH_NAME
    }

    fn description(&self) -> &str {
        "Discover tools available in this session that aren't loaded yet. Pass a natural-language query describing what you need to do (e.g. \"read a file\", \"search the web\", \"run a shell command\") and tool_search returns the top matching tools by name + description. Once tool_search names a tool, its full schema becomes available on your NEXT turn — call it then with the right arguments. Use this when you need a capability and aren't sure which tool to call; the regular always-on tools (write_todo_list, task_status) are listed up-front and don't need discovery."
    }

    fn label(&self) -> &str {
        TOOL_SEARCH_NAME
    }

    fn parameters(&self) -> &'static Value {
        // Static schema — never changes per-instance.
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural-language description of the capability you need — tool name fragments, verbs, domain keywords, etc."
                    },
                    "top_k": {
                        "type": "integer",
                        "description": "How many tools to return (default 8, max 20)."
                    }
                },
                "required": ["query"]
            })
        })
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        args: Value,
        _signal: AbortSignal,
        _on_update: LoopToolUpdate,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<LoopToolResult, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let parsed: ToolSearchArgs = serde_json::from_value(args)
                .map_err(|e| format!("tool_search: invalid args: {e}"))?;
            if parsed.query.trim().is_empty() {
                return Err("tool_search: query must not be empty".to_string());
            }
            let k = parsed.top_k.unwrap_or(self.top_k).clamp(1, 20);
            // Rank under the registry lock and clone the top-K out (≤20)
            // so the lock is released before we take the loaded-set lock
            // or build the response — no nested locks, no lock held across
            // the await boundary.
            let ranked: Vec<ToolMeta> = {
                let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
                rank_tools(&reg, &parsed.query, k)
                    .into_iter()
                    .cloned()
                    .collect()
            };

            // Mutate the loaded set BEFORE building the response so
            // the model can rely on next-turn availability.
            {
                let mut guard = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
                for meta in &ranked {
                    guard.insert(meta.name.clone());
                }
            }

            // Build the JSON response. One entry per match —
            // {name, description, parameter_summary}. Wrapped
            // alongside a short note so the model knows the
            // tools are now available next turn.
            let entries: Vec<Value> = ranked
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "name": m.name,
                        "description": m.description,
                        "parameter_summary": m.parameter_summary,
                    })
                })
                .collect();
            let body = serde_json::json!({
                "query": parsed.query,
                "results": entries,
                "note": "These tools are now loaded for the remainder of this session. Their full schemas will be present on your next turn — call them with the right arguments then.",
            });
            let text = serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string());

            Ok(LoopToolResult {
                content: vec![serde_json::json!({
                    "type": "text",
                    "text": text,
                })],
                details: body,
                terminate: None,
            })
        })
    }
}

/// Build a `ToolMeta` for a `LoopTool`. The parameter summary is a
/// short stringified shape (first-level keys + types from the JSON
/// schema). Kept small on purpose — `tool_search` is supposed to
/// SAVE tokens; a full schema dump would defeat the point.
pub fn meta_from_loop_tool(tool: &dyn LoopTool) -> ToolMeta {
    let params = tool.parameters();
    let summary = summarize_parameters(params);
    ToolMeta {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        parameter_summary: summary,
    }
}

/// Produce a one-line "shape" string from a JSON Schema object.
/// Output is `{key: type, key?: type, ...}` — `?` marks
/// non-required keys. Best-effort — exotic schemas fall back to
/// `"<schema>"`.
pub fn summarize_parameters(schema: &Value) -> String {
    let obj = match schema.as_object() {
        Some(o) => o,
        None => return "<schema>".to_string(),
    };
    let props = match obj.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return "<schema>".to_string(),
    };
    let required: HashSet<String> = obj
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut parts: Vec<String> = Vec::new();
    for (key, ty_schema) in props.iter() {
        let ty = ty_schema
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("any");
        let suffix = if required.contains(key) { "" } else { "?" };
        parts.push(format!("{key}{suffix}: {ty}"));
    }
    format!("{{{}}}", parts.join(", "))
}

/// Score a single tool against the query. Higher = better match.
/// Components (all 0..=1, summed):
///   - exact name match (huge bonus)
///   - name substring containment
///   - name bigram overlap
///   - description substring containment
///   - description bigram overlap
fn score(query: &str, meta: &ToolMeta) -> f32 {
    let q = query.to_ascii_lowercase();
    let q = q.trim();
    let name = meta.name.to_ascii_lowercase();
    let desc = meta.description.to_ascii_lowercase();

    let mut s: f32 = 0.0;

    if name == q {
        s += 10.0;
    }
    if name.contains(q) || q.contains(&name) {
        // partial name hit — large but not exact-match large
        s += 4.0;
    }
    // Per-token name hits — "read file" should hit `read`.
    for tok in q.split_whitespace() {
        if tok.len() >= 2 && name.contains(tok) {
            s += 2.0;
        }
    }
    s += 3.0 * bigram_overlap(q, &name);

    if desc.contains(q) {
        s += 1.5;
    }
    for tok in q.split_whitespace() {
        if tok.len() >= 3 && desc.contains(tok) {
            s += 0.5;
        }
    }
    s += 1.0 * bigram_overlap(q, &desc);

    s
}

/// Bigram-overlap ratio (Sørensen–Dice on character bigrams).
/// Range: 0..=1. Empty strings → 0.
fn bigram_overlap(a: &str, b: &str) -> f32 {
    let a_grams = bigrams(a);
    let b_grams = bigrams(b);
    if a_grams.is_empty() || b_grams.is_empty() {
        return 0.0;
    }
    // Intersection count — multi-set intersection via sorted match.
    let mut hits: usize = 0;
    let mut b_used = vec![false; b_grams.len()];
    for ag in &a_grams {
        for (i, bg) in b_grams.iter().enumerate() {
            if !b_used[i] && ag == bg {
                b_used[i] = true;
                hits += 1;
                break;
            }
        }
    }
    (2.0 * hits as f32) / (a_grams.len() + b_grams.len()) as f32
}

fn bigrams(s: &str) -> Vec<[char; 2]> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        return Vec::new();
    }
    chars.windows(2).map(|w| [w[0], w[1]]).collect()
}

/// Rank `meta` entries against `query`, returning top-K by score.
/// Entries with score <= 0 are filtered out — empty result is
/// acceptable when nothing matches.
pub fn rank_tools<'a>(meta: &'a [ToolMeta], query: &str, top_k: usize) -> Vec<&'a ToolMeta> {
    let mut scored: Vec<(f32, &ToolMeta)> = meta.iter().map(|m| (score(query, m), m)).collect();
    // Stable sort so equal scores preserve registry order — keeps
    // determinism for tests.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .filter(|(s, _)| *s > 0.0)
        .take(top_k)
        .map(|(_, m)| m)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Arc<Mutex<Vec<ToolMeta>>> {
        Arc::new(Mutex::new(vec![
            ToolMeta {
                name: "read".into(),
                description: "Read a file from disk and return its contents.".into(),
                parameter_summary: "{path: string, offset?: integer, limit?: integer}".into(),
            },
            ToolMeta {
                name: "write".into(),
                description: "Write content to a file on disk.".into(),
                parameter_summary: "{path: string, content: string}".into(),
            },
            ToolMeta {
                name: "bash".into(),
                description: "Run a shell command.".into(),
                parameter_summary: "{command: string, timeout?: integer}".into(),
            },
            ToolMeta {
                name: "websearch".into(),
                description: "Search the web with a query and return ranked URLs.".into(),
                parameter_summary: "{query: string}".into(),
            },
        ]))
    }

    #[test]
    fn rank_exact_name_match_wins() {
        let reg = fixture();
        let loaded: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let tool = ToolSearchTool::new(reg, loaded);
        let r = tool.rank("read", 3);
        assert!(!r.is_empty());
        assert_eq!(r[0].name, "read", "exact-name match must be first");
    }

    #[test]
    fn rank_returns_sensible_top_k_on_simple_query() {
        let reg = fixture();
        let loaded: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let tool = ToolSearchTool::new(reg, loaded);
        // "search the web" should land websearch first.
        let r = tool.rank("search the web", 3);
        assert!(!r.is_empty(), "non-empty ranked output");
        assert_eq!(r[0].name, "websearch");
    }

    #[test]
    fn rank_empty_on_meaningless_query() {
        // Build a fixture using EXOTIC tool names + descriptions
        // so no bigram of the query overlaps with anything in
        // the registry. The bigram-overlap metric is fuzzy by
        // design — even random ASCII can score >0 against
        // English descriptions because bigrams like "th" / "er"
        // recur. Use a registry of pure-ASCII letters that
        // contain no bigrams from the query alphabet.
        let reg = Arc::new(Mutex::new(vec![ToolMeta {
            name: "xyz".into(),
            description: "yyy yyy yyy".into(),
            parameter_summary: "{}".into(),
        }]));
        let loaded: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let tool = ToolSearchTool::new(reg, loaded);
        // Query bigrams: "qw", "ww", "ww", "wq" — none in xyz/yyy.
        let r = tool.rank("qwwwq", 5);
        assert!(r.is_empty(), "non-matching query → empty result");
    }

    #[test]
    fn rank_respects_top_k() {
        let reg = fixture();
        let loaded: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let tool = ToolSearchTool::new(reg, loaded);
        // "file" matches read / write descriptions; cap to 1.
        let r = tool.rank("file", 1);
        assert_eq!(r.len(), 1);
    }

    #[tokio::test]
    async fn execute_inserts_into_loaded_set() {
        let reg = fixture();
        let loaded: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let tool = ToolSearchTool::new(reg, loaded.clone());

        let result = tool
            .execute(
                "id1",
                serde_json::json!({"query": "read"}),
                AbortSignal::new(),
                Arc::new(|_| {}),
            )
            .await
            .expect("ok");

        // The result text should mention "read".
        let text = result.content[0]["text"].as_str().unwrap_or("");
        assert!(text.contains("read"));

        // The loaded set MUST now contain "read".
        let guard = loaded.lock().unwrap();
        assert!(
            guard.contains("read"),
            "tool_search must populate loaded set"
        );
    }

    #[tokio::test]
    async fn execute_rejects_empty_query() {
        let reg = fixture();
        let loaded: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let tool = ToolSearchTool::new(reg, loaded);

        let result = tool
            .execute(
                "id1",
                serde_json::json!({"query": "   "}),
                AbortSignal::new(),
                Arc::new(|_| {}),
            )
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn summarize_parameters_basic() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer"},
            },
            "required": ["path"],
        });
        let s = summarize_parameters(&schema);
        assert!(s.contains("path: string"));
        assert!(s.contains("offset?: integer"));
    }

    #[test]
    fn always_on_includes_tool_search() {
        assert!(ALWAYS_ON_TOOLS.contains(&TOOL_SEARCH_NAME));
    }

    /// dirge-tpx6: the registry is shared (Arc<Mutex>) and read live, so
    /// tools appended AFTER the ToolSearchTool is built — e.g. by the
    /// background MCP loader via `extend_loop_tools` — become discoverable
    /// without rebuilding the tool. This is the mechanism the search-gated
    /// fix depends on.
    #[test]
    fn rank_sees_tools_appended_after_construction() {
        let reg: Arc<Mutex<Vec<ToolMeta>>> = Arc::new(Mutex::new(Vec::new()));
        let loaded: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let tool = ToolSearchTool::new(reg.clone(), loaded);

        // Nothing to find yet.
        assert!(tool.rank("websearch", 5).is_empty());

        // Background injection appends to the SAME Arc the tool holds.
        reg.lock().unwrap().push(ToolMeta {
            name: "websearch".into(),
            description: "Search the web with a query.".into(),
            parameter_summary: "{query: string}".into(),
        });

        // Now discoverable through the live registry.
        let r = tool.rank("websearch", 5);
        assert_eq!(r.len(), 1, "late-appended tool must be searchable");
        assert_eq!(r[0].name, "websearch");
    }
}
