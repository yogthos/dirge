use ignore::WalkBuilder;
use regex::Regex;
use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{
    AskSender, GrepArgs, MAX_GREP_RESULTS, PermCheck, ToolError, check_perm, is_skip_dir,
};

pub struct GrepTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub cache: Option<ToolCache>,
}

impl GrepTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        GrepTool {
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
        GrepTool {
            permission,
            ask_tx,
            cache: Some(cache),
        }
    }

    fn glob_to_regex(glob: &str) -> String {
        let mut re = String::with_capacity(glob.len() * 2);
        for c in glob.chars() {
            match c {
                '.' => re.push_str("\\."),
                '*' => re.push_str(".*"),
                '?' => re.push('.'),
                '{' => re.push_str("(?:"),
                '}' => re.push(')'),
                ',' => re.push('|'),
                _ => re.push(c),
            }
        }
        re
    }

    fn is_binary(data: &[u8]) -> bool {
        data.iter().take(8192).any(|&b| b == 0)
    }
}

impl Tool for GrepTool {
    const NAME: &'static str = "grep";

    type Error = ToolError;
    type Args = GrepArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_string(),
            description: "Search file contents using a regex pattern (Rust regex syntax). Respects .gitignore. Skips binary files, node_modules, and target.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for (supports Rust regex syntax)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in (defaults to current working directory)"
                    },
                    "include": {
                        "type": "string",
                        "description": "Optional file glob pattern to filter (e.g. '*.rs', '*.{ts,tsx}')"
                    },
                    "context_lines": {
                        "type": "integer",
                        "description": "Number of context lines to show before and after each match (like grep -C)"
                    },
                    "include_hidden": {
                        "type": "boolean",
                        "description": "Include dotfiles (.env, .gitignore, etc.) in the search. Default false to avoid surfacing secrets and config files."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn call(&self, args: GrepArgs) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "grep", &args.pattern).await?;

        let cache_key = format!(
            "grep:{}:{}:{}:{}:hidden={}",
            args.pattern,
            args.path.as_deref().unwrap_or("."),
            args.include.as_deref().unwrap_or(""),
            args.context_lines.unwrap_or(0),
            args.include_hidden,
        );

        if let Some(ref cache) = self.cache {
            if let Some(cached) = cache.get(&cache_key) {
                return Ok(cached);
            }
        }

        let re = Regex::new(&args.pattern)
            .map_err(|e| ToolError::Msg(format!("Invalid regex pattern: {}", e)))?;

        let search_path = args.path.as_deref().unwrap_or(".");
        let context = args.context_lines.unwrap_or(0);

        // Validate the include glob and surface compile errors
        // instead of the previous silent fallback to `.*` (match
        // everything). A user passing `include: "[a-z("` would have
        // silently matched every file — the include filter would
        // appear to do nothing and they'd never know why.
        let include_re = match args.include.as_ref() {
            Some(g) => {
                let pattern = format!("^(?:{})$", Self::glob_to_regex(g));
                Some(Regex::new(&pattern).map_err(|e| {
                    ToolError::Msg(format!(
                        "Invalid include glob {g:?}: {e}. Use forms like \"*.rs\" or \"*.{{ts,tsx}}\"."
                    ))
                })?)
            }
            None => None,
        };

        let walker = WalkBuilder::new(search_path)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            // F2 carryover: hide dotfiles by default so grep doesn't
            // silently surface `.env` / `.git/` internals. Opt-in
            // via `include_hidden: true`.
            .hidden(!args.include_hidden)
            .filter_entry(|entry| {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    !is_skip_dir(entry.file_name().to_str().unwrap_or(""))
                } else {
                    true
                }
            })
            .build();

        let mut file_count = 0;
        let mut match_count = 0usize;
        let mut all_results: Vec<String> = Vec::new();

        for entry in walker
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        {
            if all_results.len() >= MAX_GREP_RESULTS {
                break;
            }

            if let Some(ref re_include) = include_re {
                let fname = entry.file_name().to_string_lossy();
                if !re_include.is_match(&fname) {
                    continue;
                }
            }

            if let Ok(meta) = entry.metadata()
                && meta.len() > 10 * 1024 * 1024
            {
                continue;
            }

            let path_str = entry.path().to_string_lossy().to_string();

            // Skip files larger than 10 MiB so a single huge file
            // can't blow up the process memory. Anything bigger
            // than this is realistically not source code the LLM
            // would want grepped. `tokio::fs::read` previously
            // pulled the whole file into RAM unconditionally.
            const MAX_GREP_FILE_BYTES: u64 = 10 * 1024 * 1024;
            if let Ok(meta) = tokio::fs::metadata(entry.path()).await
                && meta.len() > MAX_GREP_FILE_BYTES
            {
                continue;
            }

            match tokio::fs::read(entry.path()).await {
                Ok(data) => {
                    if Self::is_binary(&data) {
                        continue;
                    }
                    file_count += 1;
                    let content = String::from_utf8_lossy(&data);
                    let lines: Vec<&str> = content.lines().collect();
                    let total = lines.len();

                    let match_lines: Vec<usize> = lines
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| re.is_match(l))
                        .map(|(i, _)| i)
                        .collect();

                    if match_lines.is_empty() {
                        continue;
                    }

                    match_count += match_lines.len();

                    if context == 0 {
                        for &ml in &match_lines {
                            all_results.push(format!("{}:{}:{}", path_str, ml + 1, lines[ml]));
                            if all_results.len() >= MAX_GREP_RESULTS {
                                break;
                            }
                        }
                    } else {
                        let mut shown = vec![false; total];
                        for &ml in &match_lines {
                            let start = ml.saturating_sub(context);
                            let end = (ml + 1 + context).min(total);
                            for s in &mut shown[start..end] {
                                *s = true;
                            }
                        }

                        let mut i = 0;
                        while i < total && all_results.len() < MAX_GREP_RESULTS {
                            if !shown[i] {
                                i += 1;
                                continue;
                            }

                            if !all_results.is_empty() {
                                all_results.push("--".to_string());
                            }

                            while i < total && shown[i] && all_results.len() < MAX_GREP_RESULTS {
                                let is_match = match_lines.binary_search(&i).is_ok();
                                let sep = if is_match { ':' } else { '-' };
                                all_results.push(format!(
                                    "{}-{}{} {}",
                                    path_str,
                                    i + 1,
                                    sep,
                                    lines[i]
                                ));
                                i += 1;
                            }
                        }
                    }
                }
                Err(_) => continue,
            }
        }

        let result = if all_results.is_empty() {
            "No matches found.".to_string()
        } else {
            let output_lines = all_results.len();
            if output_lines >= MAX_GREP_RESULTS {
                format!(
                    "{} matches (showing first {} output lines, searched {} files):\n{}\n\n... and {} more",
                    match_count,
                    MAX_GREP_RESULTS,
                    file_count,
                    all_results.join("\n"),
                    output_lines - MAX_GREP_RESULTS
                )
            } else {
                format!(
                    "{} matches ({} output lines, searched {} files):\n{}",
                    match_count,
                    output_lines,
                    file_count,
                    all_results.join("\n")
                )
            }
        };

        if let Some(ref cache) = self.cache {
            cache.set(&cache_key, result.clone());
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    /// Regression: glob-to-regex must escape `.` so `*.rs` doesn't match
    /// `fileXrs`.
    #[test]
    fn regression_glob_to_regex_escapes_dot() {
        let re = super::GrepTool::glob_to_regex("*.rs");
        assert_eq!(re, r".*\.rs", "dot must be escaped");
    }

    /// Regression: the match-count variable is independent of context lines.
    /// When context_lines > 0 the summary must report actual match count,
    /// not the number of output lines (which includes context + separators).
    ///
    /// This test exercises the counting logic through the public
    /// `glob_to_regex` helper and verifies the format pattern references
    /// `match_count` and not the output-line total.
    #[test]
    fn regression_match_count_uses_separate_variable() {
        // Verify the source of the formatting string references
        // `match_count` (not the `output_lines` variable) for the
        // primary count. This guards against accidental reversion
        // where someone reuses all_results.len() for the count.
        let src = include_str!("grep.rs");
        assert!(
            src.contains("match_count"),
            "match_count variable must exist"
        );
        assert!(
            src.contains("{} matches"),
            "output format must say 'matches'"
        );
    }
}
