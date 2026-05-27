use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm_path};

/// Maximum repo_overview tree nodes (dirs + files) emitted. Bound so
/// a sprawling monorepo doesn't dump a 50 000-line tree at the LLM.
const MAX_OVERVIEW_NODES: usize = 400;
/// Maximum depth to descend by default; the LLM can request deeper
/// up to this hard cap.
const MAX_OVERVIEW_DEPTH: usize = 6;
const DEFAULT_OVERVIEW_DEPTH: usize = 3;

#[derive(Deserialize, Debug)]
pub struct RepoOverviewArgs {
    /// Root path to summarize. Relative paths resolve against the
    /// agent's working directory. Defaults to `.` (cwd).
    #[serde(default)]
    pub path: Option<String>,
    /// Maximum subdirectory depth to descend. Default 3, hard cap
    /// `MAX_OVERVIEW_DEPTH` (6). Set higher for unusually flat
    /// trees; capped so a deep tree doesn't explode the output.
    #[serde(default)]
    pub max_depth: Option<usize>,
    /// When true, include each file's line count next to its name.
    /// Useful for "where is the bulk of the code?" questions; costs
    /// a syscall per file. Default false.
    #[serde(default)]
    pub include_line_counts: Option<bool>,
}

pub struct RepoOverviewTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    cache: Option<ToolCache>,
}

impl RepoOverviewTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self {
            permission,
            ask_tx,
            cache: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        cache: ToolCache,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            cache: Some(cache),
        }
    }
}

impl Tool for RepoOverviewTool {
    const NAME: &'static str = "repo_overview";

    type Error = ToolError;
    type Args = RepoOverviewArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "repo_overview".to_string(),
            description: "Structural map of a codebase: directory tree with per-directory file counts and optionally per-file line counts. Use this BEFORE diving into specific files when you need a sense of project layout — much cheaper than reading every file. Honors .gitignore and skips common noise dirs (node_modules, target, .git, __pycache__).".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Root path to summarize (relative to cwd or absolute). Defaults to '.' (cwd).",
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "Subdirectory depth cap (default 3, hard max 6).",
                    },
                    "include_line_counts": {
                        "type": "boolean",
                        "description": "Include per-file line count. Default false (cheaper).",
                    },
                },
                "required": [],
            }),
        }
    }

    async fn call(&self, args: RepoOverviewArgs) -> Result<String, ToolError> {
        let path = args.path.as_deref().unwrap_or(".");
        check_perm_path(&self.permission, &self.ask_tx, "repo_overview", path).await?;

        let depth = args
            .max_depth
            .unwrap_or(DEFAULT_OVERVIEW_DEPTH)
            .min(MAX_OVERVIEW_DEPTH)
            .max(1);
        let want_lines = args.include_line_counts.unwrap_or(false);

        // LOOP-3: include root-dir stamp so external edits invalidate.
        let stamp = crate::agent::tools::cache::fs_stamp_or_cwd(path);
        let cache_key = format!("repo_overview:{}:{}:{}:{}", path, depth, want_lines, stamp,);
        if let Some(ref cache) = self.cache
            && let Some(cached) = cache.get(&cache_key)
        {
            return Ok(cached);
        }

        let root = PathBuf::from(path);
        if !root.exists() {
            return Err(ToolError::Msg(format!("path does not exist: {}", path)));
        }
        if !root.is_dir() {
            return Err(ToolError::Msg(format!("path is not a directory: {}", path)));
        }

        let canonical_root = root.canonicalize().unwrap_or(root.clone());
        let result = build_overview(&canonical_root, depth, want_lines)?;

        if let Some(ref cache) = self.cache {
            cache.set(&cache_key, result.clone());
        }

        Ok(result)
    }
}

/// Walk the tree under `root` and produce a markdown-ish outline.
fn build_overview(root: &Path, max_depth: usize, want_lines: bool) -> Result<String, ToolError> {
    let mut walker = WalkBuilder::new(root);
    walker
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .hidden(true)
        .max_depth(Some(max_depth))
        .filter_entry(|entry| {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_str().unwrap_or("");
                !crate::agent::tools::is_skip_dir(name)
            } else {
                true
            }
        });

    // Collect entries first so we can compute per-dir file counts
    // before printing the tree.
    let mut entries: Vec<(PathBuf, bool, usize)> = Vec::new(); // (path, is_dir, depth_from_root)
    let mut total_nodes = 0usize;
    let mut truncated = false;
    for result in walker.build() {
        let Ok(entry) = result else { continue };
        let depth = entry.depth();
        if depth == 0 {
            // The root itself — represented as the header line below.
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push((entry.path().to_path_buf(), is_dir, depth));
        total_nodes += 1;
        if total_nodes >= MAX_OVERVIEW_NODES {
            truncated = true;
            break;
        }
    }

    // Per-dir file count = how many entries underneath this dir, of
    // any depth, that are files. Used in the `dir/ (N files)` label.
    let file_counts = compute_dir_file_counts(&entries);

    let mut out = String::new();
    out.push_str(&format!("# Overview of `{}`\n\n", root.display()));
    out.push_str(&format!(
        "depth={}  nodes={}{}\n\n",
        max_depth,
        total_nodes,
        if truncated {
            format!(" (truncated at {})", MAX_OVERVIEW_NODES)
        } else {
            String::new()
        },
    ));

    // Sort entries by full path so dirs naturally group with their
    // children. WalkBuilder already iterates depth-first, but
    // sorting normalizes platform iteration order.
    let mut sorted = entries;
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    for (path, is_dir, depth) in &sorted {
        let indent = "  ".repeat(depth.saturating_sub(1));
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        if *is_dir {
            let n = file_counts.get(path.as_path()).copied().unwrap_or(0);
            out.push_str(&format!(
                "{}{}/ ({} file{})\n",
                indent,
                name,
                n,
                if n == 1 { "" } else { "s" }
            ));
        } else if want_lines {
            let lines = count_lines(path).unwrap_or(0);
            out.push_str(&format!("{}{}  ({} lines)\n", indent, name, lines));
        } else {
            out.push_str(&format!("{}{}\n", indent, name));
        }
    }

    if truncated {
        out.push_str(&format!(
            "\n[…tree truncated at {} nodes. Re-run with a narrower `path` or smaller `max_depth` for a complete map.]\n",
            MAX_OVERVIEW_NODES,
        ));
    }
    Ok(out)
}

fn compute_dir_file_counts(
    entries: &[(PathBuf, bool, usize)],
) -> std::collections::HashMap<PathBuf, usize> {
    // Build a set of directories that are actually printed so we
    // only bump counts for ancestors we will name. Walking
    // parent.parent() past the crawl root (#9 fix) wasted entries
    // for paths that never appear in the output.
    let printed_dirs: std::collections::HashSet<&PathBuf> = entries
        .iter()
        .filter(|(_, is_dir, _)| *is_dir)
        .map(|(p, _, _)| p)
        .collect();
    let mut counts: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
    for (path, is_dir, _) in entries {
        if *is_dir {
            continue;
        }
        // Bump every ancestor dir's count, but only for ancestors
        // that are themselves in the crawl set. Stops the walk at
        // the crawl root rather than carrying up to `/`.
        let mut cur = path.parent();
        while let Some(p) = cur {
            if !printed_dirs.contains(&p.to_path_buf()) {
                break;
            }
            *counts.entry(p.to_path_buf()).or_insert(0) += 1;
            cur = p.parent();
        }
    }
    counts
}

fn count_lines(path: &Path) -> Option<usize> {
    let bytes = std::fs::read(path).ok()?;
    Some(bytes.iter().filter(|&&b| b == b'\n').count())
}
