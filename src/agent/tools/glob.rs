use ignore::WalkBuilder;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use std::path::Path;

use crate::agent::tools::MAX_FIND_RESULTS;
use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm, check_perm_path};

pub struct GlobTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub cache: Option<ToolCache>,
}

impl GlobTool {
    /// Construct without a cache. Retained for parity with other
    /// tools (Read/Grep/FindFiles/ListDir) and exercised by unit
    /// tests; production paths use `with_cache`.
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self {
            permission,
            ask_tx,
            cache: None,
        }
    }

    /// Builder that matches the dual-constructor pattern used by
    /// Read/Grep/FindFiles/ListDir. Same `ToolCache` is shared
    /// across tools so a `bash`/`write` that mutates the filesystem
    /// invalidates glob results too via `cache.clear()`.
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

#[derive(Deserialize)]
pub struct GlobArgs {
    pub pattern: String,
    pub path: Option<String>,
    /// Include dotfiles in the walk. Default `false`. See the
    /// equivalent doc on `FindFilesArgs::include_hidden`.
    #[serde(default)]
    pub include_hidden: bool,
}

fn glob_to_regex(pattern: &str) -> Result<regex::Regex, String> {
    let mut regex_str = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            // ** — match any depth
            if i + 2 < chars.len() && chars[i + 2] == '/' {
                regex_str.push_str("(?:.*/)?");
                i += 3;
                continue;
            } else {
                regex_str.push_str(".*");
                i += 2;
                continue;
            }
        } else if chars[i] == '*' {
            regex_str.push_str("[^/]*");
        } else if chars[i] == '?' {
            regex_str.push_str("[^/]");
        } else {
            let c = chars[i];
            if ".+()[]{}^$|\\".contains(c) {
                regex_str.push('\\');
            }
            regex_str.push(c);
        }
        i += 1;
    }
    regex_str.push('$');
    regex::Regex::new(&regex_str).map_err(|e| format!("invalid glob pattern: {}", e))
}

impl Tool for GlobTool {
    const NAME: &'static str = "glob";

    type Error = ToolError;
    type Args = GlobArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "glob".to_string(),
            description: "Find files matching a glob pattern (e.g., '**/*.rs', 'src/**/*.tsx'). Respects .gitignore via ignore crate. Returns matching relative file paths sorted by modification time (newest first). Returns empty string when no files match. Use this for natural path pattern matching instead of regex-based find_files."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match file paths (e.g. '**/*.rs', 'src/agent/**/*.rs')"
                    },
                    "path": {
                        "type": "string",
                        "description": "Root directory to search in (default: current working directory)"
                    },
                    "include_hidden": {
                        "type": "boolean",
                        "description": "Include dotfiles (.env, .gitignore, etc.). Default false to avoid surfacing secrets and config files."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn call(&self, args: GlobArgs) -> Result<String, ToolError> {
        check_perm(
            &self.permission,
            &self.ask_tx,
            "glob",
            &format!("pattern:{}", args.pattern),
        )
        .await?;
        // Path-side check: external_directory rules + Accept-mode
        // working-dir gating live in check_perm_path. Without this
        // a glob over `/etc` or `~/.ssh` skipped the rules entirely.
        let perm_path = args.path.as_deref().unwrap_or(".");
        check_perm_path(&self.permission, &self.ask_tx, "glob", perm_path).await?;

        let cache_key = format!(
            "glob:{}:{}:hidden={}",
            args.pattern,
            args.path.as_deref().unwrap_or("."),
            args.include_hidden,
        );
        if let Some(ref cache) = self.cache
            && let Some(cached) = cache.get(&cache_key)
        {
            return Ok(cached);
        }

        let re = glob_to_regex(&args.pattern).map_err(|e| ToolError::Msg(e))?;

        let root = args
            .path
            .as_deref()
            .map(Path::new)
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| Path::new("."));

        let mut matches: Vec<(String, std::path::PathBuf)> = Vec::new();

        let walker = WalkBuilder::new(root)
            // Hide dotfiles by default. See `FindFilesArgs::include_hidden`.
            .hidden(!args.include_hidden)
            // Honor the user's global `~/.gitignore` to match the
            // behavior of grep / find_files / list_dir. Previously
            // glob set this to `false`, silently surfacing files
            // the user had globally excluded.
            .git_global(true)
            .git_ignore(true)
            .git_exclude(true)
            .build();

        for entry in walker {
            let entry = entry.map_err(|e| ToolError::Msg(e.to_string()))?;
            if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                continue;
            }

            let abs_path = entry.path().to_path_buf();
            let relative = abs_path
                .strip_prefix(root)
                .unwrap_or(&abs_path)
                .to_string_lossy()
                .into_owned();

            if re.is_match(&relative) {
                matches.push((relative, abs_path));
            }

            if matches.len() >= MAX_FIND_RESULTS {
                break;
            }
        }

        // Sort by modification time (newest first), fall back to alphabetical
        matches.sort_by(|(_, abs_a), (_, abs_b)| {
            let ma = std::fs::metadata(abs_a)
                .ok()
                .and_then(|m| m.modified().ok());
            let mb = std::fs::metadata(abs_b)
                .ok()
                .and_then(|m| m.modified().ok());
            match (ma, mb) {
                (Some(a), Some(b)) => b.cmp(&a),
                _ => abs_a.cmp(abs_b),
            }
        });

        let results: Vec<String> = matches.into_iter().map(|(rel, _)| rel).collect();
        let out = if results.is_empty() {
            String::new()
        } else {
            results.join("\n")
        };
        if let Some(ref cache) = self.cache {
            cache.set(&cache_key, out.clone());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_to_regex_basic() {
        let re = glob_to_regex("*.rs").unwrap();
        assert!(re.is_match("main.rs"));
        assert!(re.is_match("lib.rs"));
        assert!(!re.is_match("main.py"));
        assert!(!re.is_match("src/main.rs"));
    }

    #[test]
    fn test_glob_to_regex_recursive() {
        let re = glob_to_regex("**/*.rs").unwrap();
        assert!(re.is_match("main.rs"));
        assert!(re.is_match("src/main.rs"));
        assert!(re.is_match("src/agent/tools/foo.rs"));
        assert!(!re.is_match("main.py"));
    }

    #[test]
    fn test_glob_to_regex_nested_dir() {
        let re = glob_to_regex("src/**/*.rs").unwrap();
        assert!(!re.is_match("main.rs"));
        assert!(re.is_match("src/main.rs"));
        assert!(re.is_match("src/agent/tools/foo.rs"));
        assert!(!re.is_match("lib/main.rs"));
    }

    #[test]
    fn test_glob_to_regex_question_mark() {
        let re = glob_to_regex("file.??").unwrap();
        assert!(re.is_match("file.rs"));
        assert!(re.is_match("file.py"));
        assert!(!re.is_match("file.cpp"));
        assert!(!re.is_match("file.r"));
    }

    #[tokio::test]
    async fn test_definition_has_correct_name() {
        let tool = GlobTool::new(None, None);
        let def = tool.definition(String::new()).await;
        assert_eq!(def.name, "glob");
    }

    // ---- Integration tests against a real temp directory ----

    struct TempTree {
        root: std::path::PathBuf,
    }

    impl TempTree {
        fn new(suffix: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "dirge-glob-test-{}-{}",
                std::process::id(),
                suffix
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn write(&self, rel: &str, content: &str) -> std::path::PathBuf {
            let p = self.root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
            p
        }

        fn root_str(&self) -> String {
            self.root.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn glob_walks_files_under_path() {
        let tree = TempTree::new("walks");
        tree.write("a.rs", "");
        tree.write("b.rs", "");
        tree.write("c.py", "");
        tree.write("sub/d.rs", "");

        let tool = GlobTool::new(None, None);
        let out = tool
            .call(GlobArgs {
                pattern: "**/*.rs".into(),
                path: Some(tree.root_str()),
                include_hidden: false,
            })
            .await
            .unwrap();

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "got: {out}");
        for f in ["a.rs", "b.rs", "sub/d.rs"] {
            assert!(lines.contains(&f), "expected {f} in: {out}");
        }
        // .py file should be excluded.
        assert!(!out.contains("c.py"));
    }

    // Regression: previously returned the literal string "no files matched",
    // which the agent had to special-case. Now returns "" so the response is
    // cleanly empty.
    #[tokio::test]
    async fn regression_empty_result_returns_empty_string() {
        let tree = TempTree::new("empty");
        tree.write("a.txt", "");

        let tool = GlobTool::new(None, None);
        let out = tool
            .call(GlobArgs {
                pattern: "**/*.nonexistent".into(),
                path: Some(tree.root_str()),
                include_hidden: false,
            })
            .await
            .unwrap();
        assert_eq!(out, "");
    }

    // Regression: mtime sort previously called metadata() on the relative path,
    // looked up against CWD instead of the explicit root. When `path` was not
    // CWD, all metadata calls failed silently and sort degraded to alphabetical.
    // Now we keep absolute paths for metadata lookups.
    #[tokio::test]
    async fn regression_mtime_sort_with_explicit_path() {
        let tree = TempTree::new("mtime");
        tree.write("oldest.rs", "");
        // Walk-clock-time is sufficient on every platform we run.
        std::thread::sleep(std::time::Duration::from_millis(20));
        tree.write("middle.rs", "");
        std::thread::sleep(std::time::Duration::from_millis(20));
        tree.write("newest.rs", "");

        let tool = GlobTool::new(None, None);
        let out = tool
            .call(GlobArgs {
                pattern: "*.rs".into(),
                path: Some(tree.root_str()),
                include_hidden: false,
            })
            .await
            .unwrap();

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines, vec!["newest.rs", "middle.rs", "oldest.rs"]);
    }

    // Regression: `ignore::WalkBuilder` is configured with `git_ignore(true)`
    // so .gitignore'd files are skipped without needing an explicit deny list.
    #[tokio::test]
    async fn regression_respects_gitignore() {
        let tree = TempTree::new("gitignore");
        // .gitignore is only honored inside a git repo, so we make one.
        std::fs::create_dir_all(tree.root.join(".git")).unwrap();
        tree.write(".gitignore", "ignored.rs\n");
        tree.write("kept.rs", "");
        tree.write("ignored.rs", "");

        let tool = GlobTool::new(None, None);
        let out = tool
            .call(GlobArgs {
                pattern: "*.rs".into(),
                path: Some(tree.root_str()),
                include_hidden: false,
            })
            .await
            .unwrap();

        assert!(out.contains("kept.rs"));
        assert!(!out.contains("ignored.rs"), "got: {out}");
    }

    // The glob→regex conversion escapes regex metachars so they can't be
    // interpreted as regex. Without this, an agent passing `file.rs` would
    // unexpectedly match `fileXrs` because `.` is regex metasyntax.
    #[test]
    fn glob_escapes_regex_metacharacters() {
        let re = glob_to_regex("file.rs").unwrap();
        assert!(re.is_match("file.rs"));
        assert!(!re.is_match("fileXrs"));
        assert!(!re.is_match("filers"));
    }

    // `*` is intentionally bounded to a single path segment — `*` does NOT
    // descend into subdirs (only `**` does). Regression-guard against
    // accidentally swapping `[^/]*` for `.*`.
    #[test]
    fn star_does_not_cross_directory_boundary() {
        let re = glob_to_regex("*.rs").unwrap();
        assert!(re.is_match("main.rs"));
        assert!(!re.is_match("src/main.rs"));
    }

    /// F2: dotfiles must be skipped by default. `.env`, `.gitignore`,
    /// `.DS_Store` etc. previously appeared in glob results,
    /// risking secret leakage into the LLM context.
    #[tokio::test]
    async fn glob_skips_dotfiles_by_default() {
        let tree = TempTree::new("hidden-default");
        tree.write("main.rs", "");
        tree.write(".env", "SECRET=hunter2");
        tree.write(".gitignore", "target/");

        let tool = GlobTool::new(None, None);
        let out = tool
            .call(GlobArgs {
                pattern: "*".into(),
                path: Some(tree.root_str()),
                include_hidden: false,
            })
            .await
            .unwrap();

        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.contains(&"main.rs"), "main.rs missing: {out}");
        assert!(
            !lines.iter().any(|l| l.starts_with('.')),
            "dotfile leaked into default glob: {out}",
        );
    }

    /// F2: setting `include_hidden: true` opts back in to seeing
    /// dotfiles. The LLM uses this when it explicitly needs to
    /// inspect `.gitignore`, `.env.example`, etc.
    #[tokio::test]
    async fn glob_includes_dotfiles_when_asked() {
        let tree = TempTree::new("hidden-opt-in");
        tree.write("main.rs", "");
        tree.write(".gitignore", "target/");

        let tool = GlobTool::new(None, None);
        let out = tool
            .call(GlobArgs {
                pattern: "*".into(),
                path: Some(tree.root_str()),
                include_hidden: true,
            })
            .await
            .unwrap();
        assert!(out.contains("main.rs"), "main.rs missing: {out}");
        assert!(
            out.contains(".gitignore"),
            "dotfile missing when opt-in: {out}"
        );
    }
}
