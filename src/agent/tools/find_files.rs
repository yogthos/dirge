use ignore::WalkBuilder;
use regex::Regex;
use rig::completion::ToolDefinition;
use rig::tool::Tool;

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{
    AskSender, FindFilesArgs, MAX_FIND_RESULTS, PermCheck, ToolError, check_perm, check_perm_path,
    is_skip_dir,
};

pub struct FindFilesTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub cache: Option<ToolCache>,
}

impl FindFilesTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        FindFilesTool {
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
        FindFilesTool {
            permission,
            ask_tx,
            cache: Some(cache),
        }
    }
}

impl Tool for FindFilesTool {
    const NAME: &'static str = "find_files";

    type Error = ToolError;
    type Args = FindFilesArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "find_files".to_string(),
            description: "Recursively find FILES whose FILENAME matches a regex pattern. Use this to locate a file by name (e.g. `^Cargo\\.toml$`, `.*_test\\.py$`). NOT for finding symbol definitions — use `find_definition` for that. NOT for content search — use `grep`. Respects .gitignore; skips node_modules and target.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to match file names against"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in (defaults to current working directory)"
                    },
                    "include_hidden": {
                        "type": "boolean",
                        "description": "Include dotfiles (.env, .gitignore, etc.) in results. Default false to avoid surfacing secrets and config files."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn call(&self, args: FindFilesArgs) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "find_files", &args.pattern).await?;
        // Path-side check: external_directory rules + Accept-mode
        // gating live in check_perm_path. Without this find_files
        // over `/etc` skipped the rules entirely.
        let perm_path = args.path.as_deref().unwrap_or(".");
        check_perm_path(&self.permission, &self.ask_tx, "find_files", perm_path).await?;

        // LOOP-3: dir stamp catches file add/remove/rename.
        let stamp =
            crate::agent::tools::cache::fs_stamp_or_cwd(args.path.as_deref().unwrap_or("."));
        let cache_key = format!(
            "find_files:{}:{}:hidden={}:{}",
            args.pattern,
            args.path.as_deref().unwrap_or("."),
            args.include_hidden,
            stamp,
        );

        if let Some(ref cache) = self.cache {
            if let Some(cached) = cache.get(&cache_key) {
                return Ok(cached);
            }
        }

        let re = Regex::new(&args.pattern)
            .map_err(|e| ToolError::Msg(format!("Invalid regex: {}", e)))?;

        let search_path = args.path.as_deref().unwrap_or(".");

        // `WalkBuilder::hidden(true)` means SKIP hidden entries.
        // Default behavior (`include_hidden = false`) hides
        // dotfiles so .env / .git / .DS_Store don't leak into the
        // LLM's view of the filesystem unintentionally. The LLM
        // can pass `include_hidden: true` when it explicitly needs
        // them. Matches pi + opencode defaults.
        let walker = WalkBuilder::new(search_path)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .hidden(!args.include_hidden)
            .filter_entry(|entry| {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    !is_skip_dir(entry.file_name().to_str().unwrap_or(""))
                } else {
                    true
                }
            })
            .build();

        let mut results: Vec<String> = Vec::new();
        // Keep counting matches past MAX_FIND_RESULTS so the footer
        // can report a meaningful "...and N more" instead of the
        // tautological "...and 0 more" that the old `total = results.len()`
        // formula produced. Capped at 10× the result limit so a
        // pattern matching the entire monorepo doesn't pin the
        // walker indefinitely; if we hit the count ceiling we mark
        // the footer with a `+`.
        let mut total_matched: usize = 0;
        const COUNT_CEILING_MULTIPLIER: usize = 10;
        let count_ceiling = MAX_FIND_RESULTS.saturating_mul(COUNT_CEILING_MULTIPLIER);

        for entry in walker
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        {
            let fname = entry.file_name().to_string_lossy();
            if re.is_match(&fname) {
                total_matched += 1;
                if results.len() < MAX_FIND_RESULTS {
                    results.push(entry.path().to_string_lossy().to_string());
                }
                if total_matched >= count_ceiling {
                    break;
                }
            }
        }

        let result = if results.is_empty() {
            "No files found matching the pattern.".to_string()
        } else {
            results.sort();
            if total_matched > MAX_FIND_RESULTS {
                let suffix = if total_matched >= count_ceiling {
                    format!("{}+", total_matched)
                } else {
                    total_matched.to_string()
                };
                let more = total_matched.saturating_sub(MAX_FIND_RESULTS);
                let more_suffix = if total_matched >= count_ceiling {
                    format!("{}+", more)
                } else {
                    more.to_string()
                };
                format!(
                    "{} files found (showing first {}):\n{}\n\n... and {} more",
                    suffix,
                    MAX_FIND_RESULTS,
                    results.join("\n"),
                    more_suffix,
                )
            } else {
                format!("{} files found:\n{}", total_matched, results.join("\n"))
            }
        };

        if let Some(ref cache) = self.cache {
            cache.set(&cache_key, result.clone());
        }

        Ok(result)
    }
}
