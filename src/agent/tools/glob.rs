use ignore::WalkBuilder;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use std::path::Path;

use crate::agent::tools::MAX_FIND_RESULTS;
use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};

pub struct GlobTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

impl GlobTool {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self { permission, ask_tx }
    }
}

#[derive(Deserialize)]
pub struct GlobArgs {
    pub pattern: String,
    pub path: Option<String>,
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

        let re = glob_to_regex(&args.pattern).map_err(|e| ToolError::Msg(e))?;

        let root = args
            .path
            .as_deref()
            .map(Path::new)
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| Path::new("."));

        let mut matches: Vec<(String, std::path::PathBuf)> = Vec::new();

        let walker = WalkBuilder::new(root)
            .hidden(false)
            .git_global(false)
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
        if results.is_empty() {
            Ok(String::new())
        } else {
            Ok(results.join("\n"))
        }
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
}
