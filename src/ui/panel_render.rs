//! Build the info-panel snapshot (cwd, MCP, LSP, todos, modified
//! files) and a small cache of the modified-files list keyed by the
//! tracker's monotonic version.
//!
//! Extracted from `ui/mod.rs`. Reading global statics (TODO_LIST,
//! MODIFIED_FILES) under their own mutexes is fine from the UI loop
//! tick — they're all short-lived locks.

#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::session::Session;
use crate::ui::renderer::PanelData;
use crate::ui::sysload::SharedSysLoad;

/// Cache of the panel's rendered MODIFIED list, keyed by
/// `(modified::version, cwd)`. Skips the lock + 256-PathBuf clone +
/// path-strip on every redraw when nothing has changed. Single-
/// threaded read (the UI loop) so a Mutex around the tuple is the
/// simplest correct shape; contention is nil.
static PANEL_MODIFIED_CACHE: std::sync::Mutex<Option<(u64, std::path::PathBuf, Vec<String>)>> =
    std::sync::Mutex::new(None);

pub(crate) fn panel_modified_cached(cwd: &std::path::Path) -> Vec<String> {
    let v = crate::agent::tools::modified::version();
    {
        let guard = PANEL_MODIFIED_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((cached_v, cached_cwd, cached_data)) = guard.as_ref()
            && *cached_v == v
            && cached_cwd.as_path() == cwd
        {
            return cached_data.clone();
        }
    }
    // Cache miss — rebuild. Lock the modified tracker, project to
    // display strings, store back.
    let cwd_buf = cwd.to_path_buf();
    let rendered: Vec<String> = crate::agent::tools::modified::recent(256)
        .into_iter()
        .map(|p| {
            p.strip_prefix(&cwd_buf)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(String::from)
                        .unwrap_or_else(|| p.display().to_string())
                })
        })
        .collect();
    let mut guard = PANEL_MODIFIED_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *guard = Some((v, cwd_buf, rendered.clone()));
    rendered
}

/// Snapshot the various pieces of state the info panel surfaces (cwd, MCP,
/// LSP, todos, modified files) into a `PanelData` ready to hand to the
/// renderer. Reads global statics (TODO_LIST, MODIFIED_FILES) under their
/// own mutexes; safe to call from the UI loop tick.
pub(crate) fn build_panel_data(
    session: &Session,
    sysload: Option<&SharedSysLoad>,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
    #[cfg(feature = "lsp")] lsp_manager: Option<&std::sync::Arc<crate::lsp::manager::LspManager>>,
) -> PanelData {
    use std::path::Path;

    #[cfg(feature = "mcp")]
    let mcp: Vec<(String, bool)> = mcp_manager
        .map(|m| {
            m.connections_snapshot()
                .into_iter()
                .map(|(name, _conn)| (name, true))
                .collect()
        })
        .unwrap_or_default();
    #[cfg(not(feature = "mcp"))]
    let mcp: Vec<(String, bool)> = Vec::new();

    #[cfg(feature = "lsp")]
    let lsp: Vec<(String, String, bool)> = lsp_manager
        .map(|m| {
            let cwd_path = Path::new(session.working_dir.as_str());
            let shorten = |p: &Path| -> String {
                p.strip_prefix(cwd_path)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(String::from)
                            .unwrap_or_else(|| p.display().to_string())
                    })
            };
            let mut all = Vec::new();
            for (id, root) in m.active_servers() {
                all.push((id, shorten(&root), true));
            }
            for (id, root) in m.broken_servers() {
                all.push((id, shorten(&root), false));
            }
            all
        })
        .unwrap_or_default();
    #[cfg(not(feature = "lsp"))]
    let lsp: Vec<(String, String, bool)> = Vec::new();

    let todos: Vec<(String, String)> = {
        let list = crate::agent::tools::todo::TODO_LIST
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        list.iter()
            .take(8)
            .map(|t| {
                let status = match t.status.as_str() {
                    "in_progress" => "[~]",
                    "completed" => "[x]",
                    _ => "[ ]",
                };
                (status.to_string(), t.content.to_string())
            })
            .collect()
    };

    let cwd_path = Path::new(session.working_dir.as_str()).to_path_buf();
    // Pull the full tracked set (capped at MAX_MODIFIED=256 inside the
    // tracker). The renderer's `build_panel_lines` decides how many
    // actually fit in the panel based on remaining terminal rows and
    // appends a `+N older` footer when truncated — matches opencode's
    // grow-to-fit pattern.
    //
    // Review #6: cache the rendered Vec<String> against the
    // tracker's monotonic version counter. The panel redraws on
    // every keystroke / streamed token; without the cache we'd
    // lock + clone 256 PathBufs + path-strip per redraw. The cache
    // also includes the cwd so a `/cd` invalidates it correctly.
    let modified = panel_modified_cached(&cwd_path);

    PanelData {
        mcp,
        lsp,
        todos,
        modified,
        sysload: sysload.map(|s| s.snapshot()),
    }
}
