//! Web tools -- fetch URLs and search the web.

use crate::tool::{AgentTool, ToolResult};
use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

/// Default response limit for `web_fetch`.
const DEFAULT_MAX_BYTES: usize = 100_000;
/// Largest response a `web_fetch` call may retain.
const HARD_MAX_BYTES: usize = 1_000_000;

/// Return whether an address is safe for direct public-network access.
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

/// Reject non-routable and special-purpose IPv4 ranges.
fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();

    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

/// Reject non-routable, transition, and special-purpose IPv6 ranges.
fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }

    let segments = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || segments[0] == 0
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x0064 && segments[1] == 0xff9b)
        || (segments[0] == 0x0100 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0)
        || segments[0] == 0x2002
        || (segments[0] == 0x2001 && segments[1] == 0x0000)
        || (segments[0] == 0x2001 && segments[1] == 0x0002)
        || (segments[0] == 0x2001 && (segments[1] & 0xfff0) == 0x0010)
        || (segments[0] == 0x2001 && (segments[1] & 0xfff0) == 0x0020)
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
        || segments[0] == 0x5f00)
}

/// Require every DNS answer to be public and select one address to pin.
fn select_public_address(
    host: &str,
    port: u16,
    addresses: impl IntoIterator<Item = IpAddr>,
) -> Result<SocketAddr> {
    let addresses: Vec<IpAddr> = addresses.into_iter().collect();
    if addresses.is_empty() {
        bail!("host {host} resolved to no addresses");
    }
    if addresses.iter().any(|ip| !is_public_ip(*ip)) {
        bail!("host {host} resolves to a non-public address");
    }

    Ok(SocketAddr::new(addresses[0], port))
}

/// Parse and validate a fetch target, returning an optional pinned DNS result.
async fn prepare_target(raw_url: &str) -> Result<(Url, Option<(String, SocketAddr)>)> {
    let url = Url::parse(raw_url).context("invalid URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("web_fetch only supports http and https URLs");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("web_fetch URLs cannot contain credentials");
    }

    let host = url
        .host_str()
        .context("web_fetch URL has no host")?
        .to_string();
    let port = url
        .port_or_known_default()
        .context("web_fetch URL has no usable port")?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_public_ip(ip) {
            bail!("web_fetch target must use a public address");
        }
        return Ok((url, None));
    }

    let resolved = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("failed to resolve host {host}"))?;
    let pinned = select_public_address(&host, port, resolved.map(|addr| addr.ip()))?;
    Ok((url, Some((host, pinned))))
}

/// Parse a bounded byte limit from tool parameters.
fn max_response_bytes(params: &Value) -> Result<usize> {
    let Some(value) = params.get("max_bytes") else {
        return Ok(DEFAULT_MAX_BYTES);
    };
    let value = value
        .as_u64()
        .context("max_bytes must be a positive integer")?;
    let value = usize::try_from(value).context("max_bytes is too large")?;
    if !(1..=HARD_MAX_BYTES).contains(&value) {
        bail!("max_bytes must be between 1 and {HARD_MAX_BYTES}");
    }
    Ok(value)
}

/// Read at most `max_bytes` plus one sentinel byte from a response.
async fn read_bounded_body(mut response: reqwest::Response, max_bytes: usize) -> Result<String> {
    let mut body = Vec::with_capacity(max_bytes.min(64 * 1024));
    while let Some(chunk) = response.chunk().await? {
        let remaining = max_bytes.saturating_add(1).saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if body.len() > max_bytes {
            break;
        }
    }

    let truncated = body.len() > max_bytes;
    body.truncate(max_bytes);
    Ok(render_body(&body, max_bytes, truncated))
}

/// Convert bounded response bytes to panic-free lossy UTF-8 output.
fn render_body(body: &[u8], max_bytes: usize, truncated: bool) -> String {
    let mut rendered = String::from_utf8_lossy(&body).into_owned();
    if truncated {
        rendered.push_str(&format!("...\n[truncated at {max_bytes} bytes]"));
    }
    rendered
}

// ─── WebFetch ───────────────────────────────────────────────────────────────

/// Fetches bounded content from validated public HTTP(S) targets.
pub struct WebFetchTool;

/// Implements the public-network fetch contract for agent tool calls.
#[async_trait::async_trait]
impl AgentTool for WebFetchTool {
    /// Return the stable tool name.
    fn name(&self) -> &str {
        "web_fetch"
    }

    /// Describe the public-network fetch behavior.
    fn description(&self) -> &str {
        "Fetch the content of a URL. Returns the page text. \
         Use for reading documentation, APIs, or any web content."
    }

    /// Return the validated fetch parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch." },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": HARD_MAX_BYTES,
                    "description": "Max response bytes. Default 100000."
                }
            },
            "required": ["url"]
        })
    }

    /// Fetch one validated URL with DNS pinning and a bounded response body.
    async fn execute(&self, params: Value, _cwd: &Path) -> Result<ToolResult> {
        let raw_url = match params.get("url").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: url".into(),
                    is_error: true,
                });
            }
        };
        let max_bytes = max_response_bytes(&params)?;
        let (url, pinned) = prepare_target(raw_url).await?;

        let mut client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        if let Some((host, address)) = &pinned {
            client = client.resolve(host, *address);
        }
        let client = client.build()?;

        let resp = client.get(url).send().await?;
        if resp.status().is_redirection() {
            return Ok(ToolResult {
                content: format!("HTTP {}: redirects are not followed", resp.status()),
                is_error: true,
            });
        }
        if !resp.status().is_success() {
            return Ok(ToolResult {
                content: format!("HTTP {}", resp.status()),
                is_error: true,
            });
        }

        let body = read_bounded_body(resp, max_bytes).await?;
        Ok(ToolResult {
            content: body,
            is_error: false,
        })
    }
}

/// Exercises SSRF classification, byte limits, and safe response rendering.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Verifies representative public addresses remain fetchable.
    #[test]
    fn accepts_public_addresses() {
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2001:4860:4860::8888".parse().unwrap()));
    }

    /// Verifies local, private, special-use, and mapped addresses are rejected.
    #[test]
    fn rejects_non_public_addresses() {
        for raw in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.0.1",
            "198.18.0.1",
            "224.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "100::1",
            "2001:10::1",
            "2001:20::1",
            "2001:db8::1",
            "3fff::1",
            "5f00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public_ip(raw.parse().unwrap()), "accepted {raw}");
        }
    }

    /// Verifies mixed DNS answers fail closed instead of selecting only a public answer.
    #[test]
    fn rejects_mixed_dns_answers() {
        let result = select_public_address(
            "example.test",
            443,
            [
                "93.184.216.34".parse().unwrap(),
                "127.0.0.1".parse().unwrap(),
            ],
        );

        assert!(result.is_err());
    }

    /// Verifies response limits reject invalid values and retain the documented default.
    #[test]
    fn validates_response_limits() {
        assert_eq!(max_response_bytes(&json!({})).unwrap(), DEFAULT_MAX_BYTES);
        assert!(max_response_bytes(&json!({"max_bytes": 0})).is_err());
        assert!(max_response_bytes(&json!({"max_bytes": 1.5})).is_err());
        assert!(max_response_bytes(&json!({"max_bytes": HARD_MAX_BYTES + 1})).is_err());
    }

    /// Verifies truncating within a multibyte sequence cannot panic.
    #[test]
    fn renders_split_utf8_without_panicking() {
        let rendered = render_body(&[0xe2, 0x82], 2, true);

        assert!(rendered.contains('\u{fffd}'));
        assert!(rendered.ends_with("[truncated at 2 bytes]"));
    }

    /// Verifies literal private targets are rejected before any connection attempt.
    #[tokio::test]
    async fn web_fetch_rejects_private_literal_targets() {
        let result = WebFetchTool
            .execute(json!({"url": "http://127.0.0.1/admin"}), Path::new("/tmp"))
            .await;

        assert!(result.is_err());
    }

    /// Verifies non-HTTP schemes and embedded credentials fail closed.
    #[tokio::test]
    async fn web_fetch_rejects_unsafe_url_forms() {
        let file_result = WebFetchTool
            .execute(json!({"url": "file:///etc/passwd"}), Path::new("/tmp"))
            .await;
        let credential_result = WebFetchTool
            .execute(
                json!({"url": "https://user:secret@example.com/"}),
                Path::new("/tmp"),
            )
            .await;

        assert!(file_result.is_err());
        assert!(credential_result.is_err());
    }
}

// ─── WebSearch ──────────────────────────────────────────────────────────────

/// Searches the public web through DuckDuckGo's HTML endpoint.
pub struct WebSearchTool;

/// Implements the fixed-endpoint web search contract for agent tool calls.
#[async_trait::async_trait]
impl AgentTool for WebSearchTool {
    /// Return the stable tool name.
    fn name(&self) -> &str {
        "web_search"
    }

    /// Describe the web search result format.
    fn description(&self) -> &str {
        "Search the web for information. Returns search results with titles, URLs, and snippets."
    }

    /// Return the search parameter schema.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query." },
                "num_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "description": "Number of results. Default 5."
                }
            },
            "required": ["query"]
        })
    }

    /// Search the fixed DuckDuckGo endpoint and parse result summaries.
    async fn execute(&self, params: Value, _cwd: &Path) -> Result<ToolResult> {
        let query = match params.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => {
                return Ok(ToolResult {
                    content: "Missing required parameter: query".into(),
                    is_error: true,
                });
            }
        };

        // Use DuckDuckGo HTML (no API key needed)
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("Mozilla/5.0 (compatible; Synapse/1.0)")
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;

        let resp = client
            .get("https://html.duckduckgo.com/html/")
            .query(&[("q", query)])
            .send()
            .await?;

        if !resp.status().is_success() {
            return Ok(ToolResult {
                content: format!("Search failed: HTTP {}", resp.status()),
                is_error: true,
            });
        }

        let body = read_bounded_body(resp, HARD_MAX_BYTES).await?;
        let num_results = params
            .get("num_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 20) as usize;

        // Simple HTML parsing for DuckDuckGo results
        let mut results = Vec::new();
        for chunk in body.split("class=\"result__a\"").skip(1).take(num_results) {
            let title = chunk
                .split('>')
                .nth(1)
                .and_then(|s| s.split('<').next())
                .unwrap_or("")
                .trim();
            let url = chunk
                .split("href=\"")
                .nth(0)
                .and_then(|_| chunk.split("href=\"").nth(1))
                .and_then(|s| s.split('"').next())
                .unwrap_or("");
            let snippet = chunk
                .split("class=\"result__snippet\"")
                .nth(1)
                .and_then(|s| s.split('>').nth(1))
                .and_then(|s| s.split('<').next())
                .unwrap_or("")
                .trim();

            if !title.is_empty() {
                results.push(format!("• {}\n  {}\n  {}", title, url, snippet));
            }
        }

        if results.is_empty() {
            Ok(ToolResult {
                content: "No results found.".into(),
                is_error: false,
            })
        } else {
            Ok(ToolResult {
                content: results.join("\n\n"),
                is_error: false,
            })
        }
    }
}
