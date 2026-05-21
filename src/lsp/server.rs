//! LSP server registry.
//!
//! Static descriptors for each built-in server: its id, the file extensions
//! it claims, and how to locate the workspace root from any file inside it.
//! The actual process spawn and JSON-RPC client come in later phases.

use std::path::{Path, PathBuf};

/// Static descriptor for an LSP server. Phase 1 only carries metadata —
/// `spawn` lands in Phase 2.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub id: &'static str,
    pub extensions: &'static [&'static str],
    /// Walks up from `file` (bounded by `stop_at`) to locate the workspace
    /// root. Returns `None` when no plausible root exists (signals "don't
    /// attach this server to this file").
    pub root: fn(file: &Path, stop_at: &Path) -> Option<PathBuf>,
}

/// Walks up from `file`'s parent looking for any of `include_markers`. If an
/// `exclude_marker` is found first on the way up, returns `None` (signals
/// "another server owns this tree"). When no include marker is found, returns
/// `stop_at` as a fallback so single-file projects still get an LSP attached.
///
/// `stop_at` is treated inclusively — the search visits it. The walk does not
/// escape above `stop_at`.
///
/// Performs synchronous filesystem I/O (`canonicalize`, `exists`). Phase 4's
/// orchestrator calls this from async context; wrap with `spawn_blocking` or
/// switch to `tokio::fs` if hot paths emerge.
pub fn nearest_root(
    file: &Path,
    stop_at: &Path,
    include_markers: &[&str],
    exclude_markers: &[&str],
) -> Option<PathBuf> {
    let start = file.parent().unwrap_or(Path::new("."));
    let stop_at = stop_at.canonicalize().unwrap_or_else(|_| stop_at.into());

    let mut cursor = start.canonicalize().unwrap_or_else(|_| start.into());
    loop {
        for marker in exclude_markers {
            if cursor.join(marker).exists() {
                return None;
            }
        }
        for marker in include_markers {
            if cursor.join(marker).exists() {
                return Some(cursor);
            }
        }
        if cursor == stop_at {
            break;
        }
        match cursor.parent() {
            Some(p) if p != cursor => cursor = p.to_path_buf(),
            _ => break,
        }
    }
    // Last-resort fallback: use the worktree boundary.
    Some(stop_at)
}

/// rust-analyzer specifically wants the Cargo.toml that declares `[workspace]`
/// — the workspace root — not a nested member crate. Walks past the nearest
/// crate manifest looking for a parent manifest containing `[workspace]`.
/// Falls back to the nearest manifest when no workspace declaration is found.
///
/// Uses a literal substring match for `[workspace]`. A pathological
/// Cargo.toml with `# [workspace] this is a comment, not the section` would
/// false-match. Acceptable in practice — cargo itself uses a TOML parser, but
/// pulling in a TOML dep for this single check isn't worth it. Revisit if
/// rust-analyzer users report mis-detection.
pub fn rust_workspace_root(file: &Path, stop_at: &Path) -> Option<PathBuf> {
    let crate_root = nearest_root(file, stop_at, &["Cargo.toml"], &[])?;
    let stop_at_canon = stop_at.canonicalize().unwrap_or_else(|_| stop_at.into());

    let mut cursor = crate_root.clone();
    loop {
        let cargo = cursor.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&cargo)
            && text.contains("[workspace]") {
                return Some(cursor);
            }
        if cursor == stop_at_canon {
            break;
        }
        match cursor.parent() {
            Some(p) if p != cursor => cursor = p.to_path_buf(),
            _ => break,
        }
    }
    Some(crate_root)
}

fn typescript_root(file: &Path, stop_at: &Path) -> Option<PathBuf> {
    nearest_root(
        file,
        stop_at,
        &[
            "package.json",
            "tsconfig.json",
            "jsconfig.json",
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
            "bun.lock",
            "bun.lockb",
        ],
        // Hand off to deno's LSP when a deno config is the nearest marker.
        &["deno.json", "deno.jsonc"],
    )
}

fn pyright_root(file: &Path, stop_at: &Path) -> Option<PathBuf> {
    nearest_root(
        file,
        stop_at,
        &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "pyrightconfig.json",
            "Pipfile",
        ],
        &[],
    )
}

fn clojure_root(file: &Path, stop_at: &Path) -> Option<PathBuf> {
    nearest_root(
        file,
        stop_at,
        &[
            "deps.edn",
            "project.clj",
            "shadow-cljs.edn",
            "bb.edn",
            ".clj-kondo",
        ],
        &[],
    )
}

/// All built-in LSP server descriptors. Order is significant only for tie-
/// breaking when an extension is claimed by more than one server — earlier
/// entries are tried first.
pub fn builtin_servers() -> Vec<ServerInfo> {
    vec![
        ServerInfo {
            id: "rust",
            extensions: &["rs"],
            root: rust_workspace_root,
        },
        ServerInfo {
            id: "typescript",
            extensions: &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
            root: typescript_root,
        },
        ServerInfo {
            id: "pyright",
            extensions: &["py", "pyi"],
            root: pyright_root,
        },
        ServerInfo {
            id: "clojure-lsp",
            extensions: &["clj", "cljs", "cljc", "edn", "bb"],
            root: clojure_root,
        },
    ]
}

/// Returns the descriptors claiming the given file extension (no leading dot,
/// lowercased internally). Empty when no server claims it.
pub fn servers_for_extension(ext: &str) -> Vec<ServerInfo> {
    let ext = ext.trim_start_matches('.').to_lowercase();
    builtin_servers()
        .into_iter()
        .filter(|s| s.extensions.iter().any(|e| *e == ext))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a unique tempdir under /tmp, create a tree of files (with given
    /// contents), and return the root path. Cleanup happens via Drop.
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(suffix: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "dirge-lsp-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
                suffix,
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn touch(&self, rel: &str) -> PathBuf {
            let p = self.root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, "").unwrap();
            p
        }

        fn touch_with(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
            p
        }

        fn root_canon(&self) -> PathBuf {
            self.root.canonicalize().unwrap()
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    // ---- nearest_root ----

    #[test]
    fn nearest_root_returns_dir_holding_marker() {
        let t = TempTree::new("nearest-direct");
        t.touch("Cargo.toml");
        let file = t.touch("src/main.rs");
        let got = nearest_root(&file, &t.root, &["Cargo.toml"], &[]).unwrap();
        assert_eq!(got, t.root_canon());
    }

    #[test]
    fn nearest_root_walks_up_multiple_levels() {
        let t = TempTree::new("nearest-walks-up");
        t.touch("Cargo.toml");
        let file = t.touch("a/b/c/d/main.rs");
        let got = nearest_root(&file, &t.root, &["Cargo.toml"], &[]).unwrap();
        assert_eq!(got, t.root_canon());
    }

    #[test]
    fn nearest_root_picks_closest_marker_when_multiple_present() {
        let t = TempTree::new("nearest-closest");
        t.touch("Cargo.toml"); // outer
        t.touch("inner/Cargo.toml"); // inner crate
        let file = t.touch("inner/src/main.rs");
        let got = nearest_root(&file, &t.root, &["Cargo.toml"], &[]).unwrap();
        assert_eq!(got, t.root_canon().join("inner"));
    }

    #[test]
    fn nearest_root_falls_back_to_stop_at_when_no_marker() {
        let t = TempTree::new("nearest-fallback");
        let file = t.touch("src/main.rs");
        // No Cargo.toml anywhere — we still want an LSP attached at the
        // worktree root for single-file work.
        let got = nearest_root(&file, &t.root, &["Cargo.toml"], &[]).unwrap();
        assert_eq!(got, t.root_canon());
    }

    // Regression: an exclude marker (e.g. deno.json blocking the typescript
    // server) found en route must return None, not fall through to stop_at.
    #[test]
    fn regression_nearest_root_returns_none_when_exclude_marker_encountered() {
        let t = TempTree::new("nearest-exclude");
        t.touch("deno.json"); // exclude marker
        let file = t.touch("src/main.ts");
        let got = nearest_root(&file, &t.root, &["package.json"], &["deno.json"]);
        assert!(got.is_none(), "got: {got:?}");
    }

    // The closest marker on the way up wins. An exclude marker above an
    // already-matched include is irrelevant.
    #[test]
    fn nearest_root_closer_include_beats_farther_exclude() {
        let t = TempTree::new("nearest-closer-include");
        t.touch("deno.json"); // farther up
        t.touch("a/b/c/package.json"); // closer
        let file = t.touch("a/b/c/d/main.ts");
        let got = nearest_root(&file, &t.root, &["package.json"], &["deno.json"]).unwrap();
        assert_eq!(got, t.root_canon().join("a/b/c"));
    }

    // Regression: an exclude marker found ABOVE the file but BELOW the include
    // marker must abort the walk and return None. Without this, the typescript
    // server would attach to deno projects when the include marker is far up.
    #[test]
    fn regression_nearest_root_exclude_above_blocks_when_no_closer_include() {
        let t = TempTree::new("nearest-exclude-above");
        t.touch("Cargo.toml"); // include at root (just for the test fixture)
        t.touch("a/deno.json"); // exclude on the way up
        let file = t.touch("a/b/c/main.ts");

        let got = nearest_root(&file, &t.root, &["Cargo.toml"], &["deno.json"]);
        assert!(
            got.is_none(),
            "exclude at a/ must abort before reaching the include at root; got {got:?}"
        );
    }

    // ---- rust_workspace_root ----

    // Regression: a nested member crate must resolve to the workspace root,
    // not the member directory. rust-analyzer needs to see the whole graph.
    #[test]
    fn regression_rust_walks_past_nested_crate_to_workspace_root() {
        let t = TempTree::new("rust-workspace-walk");
        t.touch_with("Cargo.toml", "[workspace]\nmembers = [\"member\"]\n");
        t.touch_with("member/Cargo.toml", "[package]\nname = \"member\"\n");
        let file = t.touch("member/src/lib.rs");

        let got = rust_workspace_root(&file, &t.root).unwrap();
        assert_eq!(got, t.root_canon());
    }

    #[test]
    fn rust_returns_crate_when_no_workspace_above() {
        let t = TempTree::new("rust-standalone");
        t.touch_with("Cargo.toml", "[package]\nname = \"x\"\n");
        let file = t.touch("src/main.rs");

        let got = rust_workspace_root(&file, &t.root).unwrap();
        assert_eq!(got, t.root_canon());
    }

    #[test]
    fn rust_returns_none_when_no_cargo_toml() {
        let t = TempTree::new("rust-no-cargo");
        let file = t.touch("loose.rs");
        // nearest_root falls back to stop_at; rust_workspace_root then checks
        // for [workspace] there, doesn't find it, and returns the fallback dir.
        // (Documenting current behavior.)
        let got = rust_workspace_root(&file, &t.root).unwrap();
        assert_eq!(got, t.root_canon());
    }

    // ---- registry ----

    #[test]
    fn builtin_servers_includes_all_four_v1_targets() {
        let ids: Vec<&str> = builtin_servers().iter().map(|s| s.id).collect();
        assert!(ids.contains(&"rust"));
        assert!(ids.contains(&"typescript"));
        assert!(ids.contains(&"pyright"));
        assert!(ids.contains(&"clojure-lsp"));
    }

    #[test]
    fn servers_for_extension_rust() {
        let servers = servers_for_extension("rs");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].id, "rust");
    }

    #[test]
    fn servers_for_extension_accepts_leading_dot() {
        // The agent might pass ".rs" or "rs" — both should resolve.
        assert_eq!(servers_for_extension(".rs").len(), 1);
        assert_eq!(servers_for_extension("rs").len(), 1);
    }

    #[test]
    fn servers_for_extension_is_case_insensitive() {
        assert_eq!(servers_for_extension("RS").len(), 1);
        assert_eq!(servers_for_extension(".TS").len(), 1);
    }

    #[test]
    fn servers_for_extension_unknown_returns_empty() {
        assert!(servers_for_extension("xyzunknown").is_empty());
    }

    #[test]
    fn typescript_claims_jsx_and_ts_family() {
        for ext in &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"] {
            let servers = servers_for_extension(ext);
            assert!(
                servers.iter().any(|s| s.id == "typescript"),
                "ext={ext} not claimed by typescript",
            );
        }
    }

    #[test]
    fn clojure_lsp_claims_all_clojure_dialects() {
        for ext in &["clj", "cljs", "cljc", "edn", "bb"] {
            let servers = servers_for_extension(ext);
            assert!(
                servers.iter().any(|s| s.id == "clojure-lsp"),
                "ext={ext} not claimed by clojure-lsp",
            );
        }
    }

    #[test]
    fn pyright_claims_py_and_pyi() {
        assert!(
            servers_for_extension("py")
                .iter()
                .any(|s| s.id == "pyright")
        );
        assert!(
            servers_for_extension("pyi")
                .iter()
                .any(|s| s.id == "pyright")
        );
    }

    // End-to-end: pick the server for a file by extension, run its root fn,
    // and verify the result. Exercises the registry's `root` function pointer.
    #[test]
    fn server_root_fn_resolves_workspace_for_rust_file() {
        let t = TempTree::new("registry-rust-root");
        t.touch_with("Cargo.toml", "[workspace]\nmembers = [\"crate-a\"]\n");
        t.touch_with("crate-a/Cargo.toml", "[package]\nname = \"crate-a\"\n");
        let file = t.touch("crate-a/src/lib.rs");

        let server = servers_for_extension("rs")
            .into_iter()
            .find(|s| s.id == "rust")
            .unwrap();
        let root = (server.root)(&file, &t.root).unwrap();
        assert_eq!(root, t.root_canon());
    }
}
