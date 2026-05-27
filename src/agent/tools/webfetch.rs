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
    // Also handles alternative IPv4 notations (decimal, octal, hex)
    // that std::net::IpAddr doesn't parse — e.g. `http://2852039166/`
    // (decimal 127.0.0.1) or `http://0x7f.0.0.1/`.
    let is_blocked_ip = if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        is_private_or_loopback(ip)
    } else {
        // Try alternative IPv4 notations.
        match parse_alt_ipv4(host) {
            Some(octets) => is_private_ipv4(octets),
            None => false,
        }
    };
    if is_blocked_ip {
        return Err(format!(
            "webfetch refused {url:?}: host {host} resolves to a private/loopback/link-local address. \
             Set DIRGE_WEBFETCH_ALLOW_PRIVATE=1 to allow this."
        ));
    }
    Ok(())
}

/// Check whether a parsed `IpAddr` is a private/loopback/link-local address.
fn is_private_or_loopback(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.octets()[0] >= 240 // class E + multicast
                || v4.is_broadcast()
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                // IPv4-mapped IPv6: ::ffff:x.x.x.x
                // segments[0..5] are all zero, segments[5] is 0xffff,
                // the last 32 bits encode an IPv4 address.
                // TOOL-1: explicit check so ::ffff:127.0.0.1 is blocked.
                || is_ipv4_mapped_ipv6(v6)
        }
    }
}

/// Check whether a V6 address is an IPv4-mapped address
/// (e.g. ::ffff:127.0.0.1) and if so, whether the embedded
/// IPv4 address is private/loopback.
/// TOOL-1: SSRF bypass — std::net::Ipv6Addr doesn't expose
/// `to_ipv4_mapped()` in stable Rust, so we check the segment
/// pattern manually.
fn is_ipv4_mapped_ipv6(v6: std::net::Ipv6Addr) -> bool {
    let segs = v6.segments();
    // IPv4-mapped: first 80 bits are zero, next 16 bits are 0xffff.
    if segs[0] == 0
        && segs[1] == 0
        && segs[2] == 0
        && segs[3] == 0
        && segs[4] == 0
        && segs[5] == 0xffff
    {
        // The last 32 bits are the IPv4 address.
        let v4_bytes = v6.octets();
        let octets = [v4_bytes[12], v4_bytes[13], v4_bytes[14], v4_bytes[15]];
        return is_private_ipv4(octets);
    }
    false
}

/// Check whether 4 octets represent a private/loopback address.
fn is_private_ipv4(octets: [u8; 4]) -> bool {
    match octets {
        // Loopback: 127.0.0.0/8
        [127, _, _, _] => true,
        // Private: 10.0.0.0/8
        [10, _, _, _] => true,
        // Private: 172.16.0.0/12
        [172, b, _, _] => (16..=31).contains(&b),
        // Private: 192.168.0.0/16
        [192, 168, _, _] => true,
        // Link-local: 169.254.0.0/16
        [169, 254, _, _] => true,
        // Unspecified
        [0, 0, 0, 0] => true,
        // Class E + multicast (240+)
        [a, _, _, _] => a >= 240,
        _ => false,
    }
}

/// Parse alternative IPv4 notations that `std::net::IpAddr` rejects:
/// - Decimal: `http://2852039166/` → 127.0.0.1
/// - Hex (no dots): `http://0xa9fea9fe/` → 169.254.169.254
/// - Octal: `http://0177.0.0.1/` → 127.0.0.1
/// - Hex (dotted): `http://0x7f.0.0.1/` → 127.0.0.1
/// - Mixed: `http://0x7f.0.0x1/` → 127.0.0.1
fn parse_alt_ipv4(s: &str) -> Option<[u8; 4]> {
    // TOOL-1: hex-without-dots — e.g. "0xa9fea9fe"
    // std::net::IpAddr rejects hex literals without dots, but
    // many HTTP libraries resolve them. Parse as a single u32.
    // Only trigger when the ENTIRE string is a hex number (no dots,
    // no colons — those fall through to dotted-quad or IPv6 parsing).
    let lower = s.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix("0x") {
        if !hex.contains('.') && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            if let Ok(n) = u32::from_str_radix(hex, 16) {
                return Some([(n >> 24) as u8, (n >> 16) as u8, (n >> 8) as u8, n as u8]);
            }
        }
        // Don't return None here — the "0x" prefix on a dotted-quad
        // (e.g. "0x7f.0.0.1") falls through to per-octet parsing below.
    }
    // Try pure-decimal (no dots): e.g. "2852039166" → 127.0.0.1
    if !s.contains('.') && s.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(n) = s.parse::<u64>() {
            if n <= u32::MAX as u64 {
                return Some([(n >> 24) as u8, (n >> 16) as u8, (n >> 8) as u8, n as u8]);
            }
        }
        return None;
    }
    // Try dotted-quad with per-octet parsing (handles octal, hex, mixed).
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        // Not a dotted-quad — let std::net handle it.
        return None;
    }
    // Check if this is a normal all-decimal dotted-quad (no leading zeros).
    // std::net::IpAddr handles those fine.
    let all_simple_decimal = parts.iter().all(|p| {
        !p.is_empty()
            && p.chars().all(|c| c.is_ascii_digit())
            && (p.len() == 1 || !p.starts_with('0'))
    });
    if all_simple_decimal {
        return None;
    }
    // Per-octet parsing for non-standard notations.
    let mut octets = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        octets[i] = parse_alt_octet(part)?;
    }
    Some(octets)
}

/// Parse a single octet in decimal, hex (0x…), or octal (0…).
fn parse_alt_octet(s: &str) -> Option<u8> {
    if s.is_empty() {
        return None;
    }
    if s.starts_with("0x") || s.starts_with("0X") {
        u8::from_str_radix(&s[2..], 16).ok()
    } else if s.starts_with('0') && s.len() > 1 {
        // Leading zero → octal (e.g. "0177" = 127)
        u8::from_str_radix(s, 8).ok()
    } else {
        s.parse::<u8>().ok()
    }
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

        // Audit L12: previously this displayed only the URL count to
        // the user's permission prompt; for a single-URL fetch the
        // user had no idea which host was being contacted. Include
        // each host inline when there are 3 or fewer; for more, just
        // the count (full list would crowd the alert).
        let perm_summary = if args.urls.len() <= 3 {
            let hosts: Vec<&str> = args
                .urls
                .iter()
                .map(|u| {
                    u.split("://")
                        .nth(1)
                        .unwrap_or(u)
                        .split('/')
                        .next()
                        .unwrap_or(u)
                })
                .collect();
            format!(
                "fetch {} url{} ({})",
                args.urls.len(),
                if args.urls.len() == 1 { "" } else { "s" },
                hosts.join(", "),
            )
        } else {
            format!("fetch {} urls", args.urls.len())
        };
        check_perm(&self.permission, &self.ask_tx, "webfetch", &perm_summary).await?;

        // C2 (audit fix): defend the SSRF host check across redirects.
        // The default reqwest policy follows 30x up to 10 hops; the
        // initial validate_url_host_safety only covers the first
        // URL. An attacker-controlled public page can 302 to
        // 169.254.169.254 (cloud metadata), RFC1918, loopback, etc.
        // Install a custom policy that re-runs the host check on
        // every hop and stops the redirect on failure.
        let client = reqwest::Client::builder()
            .user_agent("dirge/1.0")
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                // Bound the chain at the default-ish 10 hops so a
                // pathological loop can't run forever even if every
                // hop validates.
                if attempt.previous().len() >= 10 {
                    return attempt.error("redirect chain exceeded 10 hops");
                }
                let next = attempt.url().as_str();
                if let Err(reason) = validate_url_host_safety(next) {
                    return attempt
                        .error(format!("redirect target blocked by SSRF guard: {reason}"));
                }
                attempt.follow()
            }))
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
        let paragraph: String = std::iter::repeat("lorem")
            .take(long_word_count)
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

    // ============================================================
    // TOOL-1: SSRF defenses against alternative IPv4 notations
    // ============================================================

    #[test]
    fn decimal_ipv4_loopback_is_blocked() {
        // 2130706433 = 127.0.0.1 in decimal
        assert!(validate_url_host_safety("http://2130706433/").is_err());
    }

    #[test]
    fn decimal_ipv4_private_is_blocked() {
        // 167772160 = 10.0.0.0 in decimal
        assert!(validate_url_host_safety("http://167772160/").is_err());
    }

    #[test]
    fn hex_ipv4_loopback_is_blocked() {
        assert!(validate_url_host_safety("http://0x7f.0.0.1/").is_err());
    }

    #[test]
    fn octal_ipv4_loopback_is_blocked() {
        assert!(validate_url_host_safety("http://0177.0.0.1/").is_err());
    }

    #[test]
    fn mixed_hex_octal_ipv4_is_blocked() {
        assert!(validate_url_host_safety("http://0x7f.0.0.0x1/").is_err());
    }

    #[test]
    fn normal_public_ip_passes() {
        assert!(validate_url_host_safety("https://93.184.216.34/").is_ok());
    }

    /// TOOL-1: hex-without-dots — http://0xa9fea9fe/ = 169.254.169.254
    /// (link-local, AWS IMDS metadata endpoint). std::net::IpAddr
    /// rejects this format, but many HTTP stacks resolve it.
    #[test]
    fn hex_without_dots_link_local_is_blocked() {
        // 0xa9fea9fe = 169.254.169.254 (link-local)
        assert!(validate_url_host_safety("http://0xa9fea9fe/").is_err());
    }

    #[test]
    fn hex_without_dots_loopback_is_blocked() {
        // 0x7f000001 = 127.0.0.1
        assert!(validate_url_host_safety("http://0x7f000001/").is_err());
    }

    /// TOOL-1: IPv4-mapped IPv6 — http://[::ffff:127.0.0.1]/
    /// std::net parses this but is_private_or_loopback only checked
    /// the V6 flags (not the embedded V4 address).
    #[test]
    fn ipv4_mapped_ipv6_loopback_is_blocked() {
        assert!(validate_url_host_safety("http://[::ffff:127.0.0.1]/").is_err());
    }

    #[test]
    fn ipv4_mapped_ipv6_private_is_blocked() {
        assert!(validate_url_host_safety("http://[::ffff:10.0.0.1]/").is_err());
    }

    #[test]
    fn ipv4_mapped_ipv6_public_passes() {
        assert!(validate_url_host_safety("https://[::ffff:93.184.216.34]/").is_ok());
    }
}
