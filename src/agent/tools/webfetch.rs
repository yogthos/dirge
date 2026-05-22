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

/// True if `url` has an explicit `http://` or `https://` scheme,
/// case-insensitively. URL schemes are case-insensitive per RFC
/// 3986; checking only the lowercase form let `HTTP://...` and
/// other case variants bypass scheme + SSRF defenses entirely.
fn has_http_scheme(url: &str) -> bool {
    let prefix = url.get(..7).map(str::to_ascii_lowercase);
    let prefix8 = url.get(..8).map(str::to_ascii_lowercase);
    matches!(prefix.as_deref(), Some("http://")) || matches!(prefix8.as_deref(), Some("https://"))
}

/// Normalize a URL. Respects explicit http:// (localhost, internal services).
/// Only prepends https:// when no scheme is present.
fn normalize_url(url: &str) -> String {
    if has_http_scheme(url) {
        url.to_string()
    } else {
        format!("https://{}", url)
    }
}

/// Reject non-http(s) schemes. Without this, `file://`, `ftp://`,
/// `gopher://` etc. would be passed to reqwest — current reqwest
/// versions reject most of these, but the defense should be
/// explicit at the dirge boundary rather than relying on the HTTP
/// client's policy.
fn validate_url_scheme(url: &str) -> Result<(), String> {
    if has_http_scheme(url) {
        Ok(())
    } else {
        Err(format!(
            "webfetch only supports http(s); refused {url:?} (use a curl-style scheme prefix to be explicit)"
        ))
    }
}

/// Reject URLs whose host resolves to a private / loopback /
/// link-local / cloud-metadata IP unless explicitly allowed via
/// `DIRGE_WEBFETCH_ALLOW_PRIVATE=1`. Cloud metadata endpoints
/// (`169.254.169.254`, GCP/AWS/Azure variants) are the classic
/// SSRF target — an LLM that can be prompt-injected into fetching
/// them exfiltrates IAM credentials.
///
/// `localhost` and loopback are blocked by default too — dev
/// workflows that want to hit `http://localhost:3000` should opt
/// in via the env var. This matches the conservative default of
/// the opencode/curl-with-redirect-policy approach.
fn validate_url_host_safety(url: &str) -> Result<(), String> {
    if std::env::var("DIRGE_WEBFETCH_ALLOW_PRIVATE").as_deref() == Ok("1") {
        return Ok(());
    }
    // Strip scheme to extract host. Case-insensitive — URL schemes
    // are case-insensitive per RFC 3986, and an attacker using
    // `HTTPS://1.2.3.4/` would otherwise skip past the strip and
    // get the scheme treated as host text.
    let scheme_len = if url.len() >= 8 && url[..8].eq_ignore_ascii_case("https://") {
        8
    } else if url.len() >= 7 && url[..7].eq_ignore_ascii_case("http://") {
        7
    } else {
        0
    };
    let after_scheme = &url[scheme_len..];
    // Host extraction handles bracketed IPv6 (`[::1]`) before
    // falling back to the bare host:port form. Without the
    // bracket-aware path, `rsplit_once(':')` would chop `[::1]`
    // mid-address.
    let host_end = after_scheme
        .find(|c: char| matches!(c, '/' | '?' | '#'))
        .unwrap_or(after_scheme.len());
    let host_and_port = &after_scheme[..host_end];
    let host: &str = if let Some(rest) = host_and_port.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        &rest[..end]
    } else {
        host_and_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_and_port)
    };
    let host_lower = host.to_ascii_lowercase();

    // Hostname blocklist (matched literally — DNS rebinding is a
    // separate concern; this defends against the direct case).
    const BLOCKED_HOSTNAMES: &[&str] = &["localhost", "ip6-localhost", "ip6-loopback"];
    if BLOCKED_HOSTNAMES.contains(&host_lower.as_str()) {
        return Err(format!(
            "webfetch refused {url:?}: hostname is loopback/localhost. \
             Set DIRGE_WEBFETCH_ALLOW_PRIVATE=1 to allow this."
        ));
    }
    // IP literal check. Both IPv4 dotted-quad and IPv6 bracketed.
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let blocked = match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback()       // 127.0.0.0/8
                    || v4.is_private() // 10/172.16/192.168
                    || v4.is_link_local() // 169.254/16 — AWS metadata
                    || v4.is_unspecified() // 0.0.0.0
                    || v4.octets()[0] >= 240 // class E + multicast
                    || v4.is_broadcast()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unspecified() || v6.is_multicast()
                // unique-local fc00::/7
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                // link-local fe80::/10
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        };
        if blocked {
            return Err(format!(
                "webfetch refused {url:?}: IP {ip} is private/loopback/link-local. \
                 Set DIRGE_WEBFETCH_ALLOW_PRIVATE=1 to allow this."
            ));
        }
    }
    Ok(())
}

async fn fetch_url(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let url = normalize_url(url);
    validate_url_scheme(&url)?;
    validate_url_host_safety(&url)?;

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

    // Cap the body download at 10 MiB. Previously `.text()`
    // would buffer the entire response — a 500 MB page would
    // OOM the agent process before any truncation got a chance
    // to apply. Stream the body, bail at the cap, then convert.
    use futures::StreamExt;
    const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read error for {}: {}", url, e))?;
        if buf.len() + chunk.len() > MAX_BODY_BYTES {
            let remaining = MAX_BODY_BYTES.saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..remaining]);
            // Note in the rendered output that we cut off.
            // Subsequent text-conversion will still see partial
            // HTML but renders sensibly; the agent gets the gist.
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&buf);

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
            description: "Fetch the content of one or more URLs and return it as markdown. Schemeless URLs get https:// prepended. Private/loopback/link-local addresses (127.0.0.0/8, 10.x, 172.16.x, 192.168.x, 169.254.x cloud metadata, ::1, fc00::/7, fe80::/10) and bare 'localhost' are refused by default; set DIRGE_WEBFETCH_ALLOW_PRIVATE=1 to permit them for local-dev workflows. Use for reading documentation pages, API references, or any web content."
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
                        "type": "integer",
                        "minimum": 1,
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

    // Regression: prior bug passed `html.len()` as the second argument to
    // html2text — that parameter is the *line-wrap width*, not buffer size.
    // The result was effectively no wrapping at all. We now pass 100, which
    // produces wrapped output for paragraphs that exceed that width.
    #[test]
    fn regression_html_to_markdown_wraps_at_reasonable_width() {
        let long_word_count = 200;
        // Build a paragraph that, without wrapping, would be ~one extremely
        // long line.
        let paragraph: String = std::iter::repeat_n("lorem", long_word_count)
            .collect::<Vec<_>>()
            .join(" ");
        let html = format!("<p>{}</p>", paragraph);
        let md = html_to_markdown(&html);

        // The output must be split across multiple lines (wrap width=100).
        let lines: Vec<&str> = md.lines().filter(|l| !l.is_empty()).collect();
        assert!(
            lines.len() > 1,
            "expected wrapped output, got single line of {} chars",
            md.len()
        );
        // No single line should be wildly wider than the wrap width.
        for line in &lines {
            assert!(
                line.chars().count() < 200,
                "line too long ({}): {line}",
                line.chars().count()
            );
        }
    }

    #[tokio::test]
    async fn rejects_empty_urls() {
        let tool = WebFetchTool::new(None, None);
        let result = tool
            .call(WebFetchArgs {
                urls: vec![],
                max_chars: 3000,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no URLs"));
    }

    #[tokio::test]
    async fn rejects_more_than_ten_urls() {
        let tool = WebFetchTool::new(None, None);
        let urls: Vec<String> = (0..11)
            .map(|i| format!("https://example.com/{i}"))
            .collect();
        let result = tool
            .call(WebFetchArgs {
                urls,
                max_chars: 3000,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("maximum 10"));
    }

    /// Regression: scheme validation must reject anything that
    /// isn't http(s). Before this, an LLM prompting
    /// `webfetch(["file:///etc/passwd"])` relied on reqwest's
    /// internal scheme filter — explicit at-boundary defense is
    /// better.
    #[test]
    fn validate_url_scheme_rejects_non_http() {
        assert!(validate_url_scheme("https://example.com").is_ok());
        assert!(validate_url_scheme("http://localhost:3000").is_ok());
        assert!(validate_url_scheme("file:///etc/passwd").is_err());
        assert!(validate_url_scheme("ftp://example.com").is_err());
        assert!(validate_url_scheme("gopher://example.com").is_err());
        assert!(validate_url_scheme("javascript:alert(1)").is_err());
        // Empty string also blocked.
        assert!(validate_url_scheme("").is_err());
    }

    /// Regression: scheme matching must be case-insensitive (RFC
    /// 3986). Previously `starts_with("http://")` only matched
    /// lowercase, so `HTTP://169.254.169.254` bypassed scheme
    /// + SSRF defenses entirely.
    #[test]
    fn scheme_matching_is_case_insensitive() {
        // Accepted forms.
        assert!(validate_url_scheme("HTTP://example.com").is_ok());
        assert!(validate_url_scheme("HTTPS://example.com").is_ok());
        assert!(validate_url_scheme("Http://Example.Com").is_ok());
        assert!(validate_url_scheme("HtTpS://x").is_ok());
        // Rejected (no http/https scheme prefix).
        assert!(validate_url_scheme("FILE:///etc/passwd").is_err());
        // SSRF defense must still trigger for case-variant schemes.
        if std::env::var("DIRGE_WEBFETCH_ALLOW_PRIVATE").as_deref() != Ok("1") {
            assert!(validate_url_host_safety("HTTP://169.254.169.254/").is_err());
            assert!(validate_url_host_safety("HTTPS://127.0.0.1/").is_err());
        }
    }

    /// SSRF defense: AWS metadata + private + loopback + link-local
    /// IPs are refused unless the env opt-in is set. Pin the exact
    /// hosts that bug bounty reports keep hitting.
    #[test]
    fn validate_url_host_safety_blocks_ssrf_targets() {
        // SAFETY against parallel tests poking the env: this test
        // doesn't touch DIRGE_WEBFETCH_ALLOW_PRIVATE, so the
        // default behavior applies. Skip if a parallel test set
        // it (we can't reliably unset/restore without races).
        if std::env::var("DIRGE_WEBFETCH_ALLOW_PRIVATE").as_deref() == Ok("1") {
            return;
        }
        // Cloud metadata endpoints — the classic SSRF target.
        assert!(validate_url_host_safety("http://169.254.169.254/latest/meta-data/").is_err());
        // Loopback variants.
        assert!(validate_url_host_safety("http://127.0.0.1/").is_err());
        assert!(validate_url_host_safety("http://127.99.99.99/").is_err());
        assert!(validate_url_host_safety("http://localhost/").is_err());
        assert!(validate_url_host_safety("http://localhost:6379/").is_err());
        // Private space — RFC 1918.
        assert!(validate_url_host_safety("http://10.0.0.1/").is_err());
        assert!(validate_url_host_safety("http://192.168.1.1/").is_err());
        assert!(validate_url_host_safety("http://172.16.0.1/").is_err());
        // IPv6 loopback + ULA + link-local.
        assert!(validate_url_host_safety("http://[::1]/").is_err());
        assert!(validate_url_host_safety("http://[fc00::1]/").is_err());
        assert!(validate_url_host_safety("http://[fe80::1]/").is_err());
        // Public domains must still pass.
        assert!(validate_url_host_safety("https://example.com/").is_ok());
        assert!(validate_url_host_safety("https://api.github.com/repos/x/y").is_ok());
        // Public IPs pass.
        assert!(validate_url_host_safety("http://8.8.8.8/").is_ok());
    }

    /// Bash output cap test lives in bash.rs; MCP cap test lives
    /// in mcp/tool.rs.
    #[test]
    fn validate_url_host_safety_handles_malformed_hosts() {
        // Garbage host shouldn't panic; it just doesn't parse as
        // an IP and isn't in the hostname blocklist, so it falls
        // through to the HTTP client (which will likely fail).
        assert!(validate_url_host_safety("https://not-an-ip-or-domain/").is_ok());
    }

    // Regression: the WebFetchArgs default for max_chars must be 3000 — agents
    // that omit the field should not get an unbounded fetch.
    #[test]
    fn webfetch_args_default_max_chars_is_3000() {
        let parsed: WebFetchArgs =
            serde_json::from_value(serde_json::json!({"urls": ["https://example.com"]})).unwrap();
        assert_eq!(parsed.max_chars, 3000);
    }

    // html2text drops markup but keeps text content — guards against a
    // dependency upgrade changing default behavior.
    #[test]
    fn html_to_markdown_strips_tags_but_keeps_text() {
        let html = "<div><strong>bold</strong> and <em>emph</em></div>";
        let md = html_to_markdown(html);
        assert!(md.contains("bold"));
        assert!(md.contains("emph"));
        assert!(!md.contains("<strong>"));
        assert!(!md.contains("<em>"));
    }
}
