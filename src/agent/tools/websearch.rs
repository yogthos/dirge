use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};

/// Exa search result item.
#[derive(Debug, Deserialize)]
struct ExaResult {
    title: Option<String>,
    url: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExaResponse {
    results: Vec<ExaResult>,
}

#[derive(Debug, Serialize)]
struct ExaRequest<'a> {
    query: &'a str,
    #[serde(rename = "type")]
    search_type: &'a str,
    contents: ExaContents,
    #[serde(rename = "numResults")]
    num_results: usize,
}

#[derive(Debug, Serialize)]
struct ExaContents {
    text: bool,
}

pub struct WebSearchTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    api_key: String,
}

impl WebSearchTool {
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        api_key: String,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            api_key,
        }
    }
}

#[derive(Deserialize)]
pub struct WebSearchArgs {
    pub query: String,
    #[serde(default = "default_num_results")]
    pub num_results: usize,
}

fn default_num_results() -> usize {
    10
}

fn format_search_results(results: &[ExaResult]) -> String {
    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n---\n\n");
        }
        if let Some(title) = &r.title {
            out.push_str(&format!("**{}**\n", title));
        }
        if let Some(url) = &r.url {
            out.push_str(&format!("{}\n", url));
        }
        if let Some(text) = &r.text {
            let truncated: String = text.chars().take(500).collect();
            out.push_str(&format!("\n{}\n", truncated));
        }
    }
    if out.is_empty() {
        out = "No results found.".to_string();
    }
    out
}

impl Tool for WebSearchTool {
    const NAME: &'static str = "websearch";

    type Error = ToolError;
    type Args = WebSearchArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "websearch".to_string(),
            description: "Search the web using Exa. Returns structured results with titles, URLs, and text snippets. Use for looking up current documentation, API references, or up-to-date information beyond your training cutoff."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "num_results": {
                        "type": "number",
                        "description": "Maximum number of results (default: 10)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, args: WebSearchArgs) -> Result<String, ToolError> {
        check_perm(
            &self.permission,
            &self.ask_tx,
            "websearch",
            &args.query,
        )
        .await?;

        let client = reqwest::Client::new();
        let body = ExaRequest {
            query: &args.query,
            search_type: "auto",
            contents: ExaContents { text: true },
            num_results: args.num_results.min(20),
        };

        let resp = client
            .post("https://api.exa.ai/search")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Msg(format!("websearch request failed: {}", e)))?;

        let status = resp.status();
        let body_text = resp
            .text()
            .await
            .map_err(|e| ToolError::Msg(format!("websearch read failed: {}", e)))?;

        if !status.is_success() {
            return Err(ToolError::Msg(format!(
                "websearch returned {}: {}",
                status.as_u16(),
                &body_text.chars().take(300).collect::<String>()
            )));
        }

        let parsed: ExaResponse =
            serde_json::from_str(&body_text).map_err(|e| {
                ToolError::Msg(format!("websearch parse error: {}", e))
            })?;

        Ok(format_search_results(&parsed.results))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_search_results_single() {
        let results = vec![ExaResult {
            title: Some("Test Title".to_string()),
            url: Some("https://example.com".to_string()),
            text: Some("Some text content".to_string()),
        }];
        let formatted = format_search_results(&results);
        assert!(formatted.contains("**Test Title**"));
        assert!(formatted.contains("https://example.com"));
        assert!(formatted.contains("Some text content"));
    }

    #[test]
    fn test_format_search_results_empty() {
        let formatted = format_search_results(&[]);
        assert_eq!(formatted, "No results found.");
    }

    #[test]
    fn test_format_search_results_multiple() {
        let results = vec![
            ExaResult {
                title: Some("First".to_string()),
                url: Some("https://first.example".to_string()),
                text: Some("First text".to_string()),
            },
            ExaResult {
                title: Some("Second".to_string()),
                url: Some("https://second.example".to_string()),
                text: Some("Second text".to_string()),
            },
        ];
        let formatted = format_search_results(&results);
        assert!(formatted.contains("**First**"));
        assert!(formatted.contains("**Second**"));
        assert!(formatted.contains("---"));
    }

    #[tokio::test]
    async fn test_definition_has_correct_name() {
        let tool = WebSearchTool::new(None, None, "test-key".to_string());
        let def = tool.definition(String::new()).await;
        assert_eq!(def.name, "websearch");
    }
}
