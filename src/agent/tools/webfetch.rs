use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};

pub struct WebFetchTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

impl WebFetchTool {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self { permission, ask_tx }
    }
}

#[derive(Deserialize)]
pub struct WebFetchArgs {
    pub urls: Vec<String>,
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
}

fn default_max_chars() -> usize {
    3000
}

fn html_to_markdown(html: &str) -> String {
    // Second arg is line-wrap width (100 cols), not buffer size
    html2text::from_read(html.as_bytes(), 100).unwrap_or_else(|_| html.to_string())
}

/// Normalize a URL. Respects explicit http:// (localhost, internal services).
/// Only prepends https:// when no scheme is present.
fn normalize_url(url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://{}", url)
    }
}

async fn fetch_url(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let url = normalize_url(url);

    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                format!("timeout fetching {}", url)
            } else {
                format!("fetch error for {}: {}", url, e)
            }
        })?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{} returned {}", url, status.as_u16()));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("read error for {}: {}", url, e))?;

    Ok(html_to_markdown(&body))
}

impl Tool for WebFetchTool {
    const NAME: &'static str = "webfetch";

    type Error = ToolError;
    type Args = WebFetchArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "webfetch".to_string(),
            description: "Fetch the content of one or more URLs and return it as markdown. Schemeless URLs get https:// prepended. Explicit http:// URLs (localhost, internal services) are respected. Use for reading documentation pages, API references, or any web content."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "urls": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "URLs to fetch (may be comma-separated)"
                    },
                    "max_chars": {
                        "type": "number",
                        "description": "Maximum characters to return per URL (default: 3000)"
                    }
                },
                "required": ["urls"]
            }),
        }
    }

    async fn call(&self, args: WebFetchArgs) -> Result<String, ToolError> {
        if args.urls.is_empty() {
            return Err(ToolError::Msg("no URLs provided".to_string()));
        }
        if args.urls.len() > 10 {
            return Err(ToolError::Msg("maximum 10 URLs per call".to_string()));
        }

        check_perm(
            &self.permission,
            &self.ask_tx,
            "webfetch",
            &format!("fetch {} urls", args.urls.len()),
        )
        .await?;

        let client = reqwest::Client::builder()
            .user_agent("dirge/1.0")
            .build()
            .map_err(|e| ToolError::Msg(format!("client build error: {}", e)))?;

        let mut output = String::new();
        let max = args.max_chars.min(10000);

        for (i, url) in args.urls.iter().enumerate() {
            if i > 0 {
                output.push_str("\n\n---\n\n");
            }
            output.push_str(&format!("## {}\n\n", url));

            match fetch_url(&client, url).await {
                Ok(content) => {
                    let truncated: String = content.chars().take(max).collect();
                    output.push_str(&truncated);
                    if content.chars().count() > max {
                        output.push_str("\n\n*(truncated)*");
                    }
                }
                Err(e) => {
                    output.push_str(&format!("Error: {}", e));
                }
            }
        }

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_url_https() {
        assert_eq!(normalize_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn test_normalize_url_http_preserved() {
        assert_eq!(
            normalize_url("http://localhost:3000"),
            "http://localhost:3000"
        );
    }

    #[test]
    fn test_normalize_url_schemeless_prepends_https() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
    }

    #[test]
    fn test_normalize_url_internal_http() {
        assert_eq!(
            normalize_url("http://169.254.169.254"),
            "http://169.254.169.254"
        );
    }

    #[test]
    fn test_html_to_markdown_basic() {
        let html = "<h1>Title</h1><p>Paragraph</p>";
        let md = html_to_markdown(html);
        assert!(md.contains("Title"));
        assert!(md.contains("Paragraph"));
    }

    #[test]
    fn test_html_to_markdown_links() {
        let html = r#"<a href="https://example.com">click here</a>"#;
        let md = html_to_markdown(html);
        assert!(md.contains("click here"));
    }

    #[tokio::test]
    async fn test_definition_has_correct_name() {
        let tool = WebFetchTool::new(None, None);
        let def = tool.definition(String::new()).await;
        assert_eq!(def.name, "webfetch");
    }
}
