//! mcp-server-fetch: standalone MCP server for HTTP requests.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: fetch
//!
//! Config:
//!   - `allow_unsafe_methods` (env, default "false"): when false, only
//!     SAFE/read-only methods are allowed (GET, HEAD, OPTIONS); when true,
//!     all reqwest-supported methods (POST, PUT, PATCH, DELETE, ...) are
//!     allowed. The env var name matches the plugin.json config_schema key so
//!     config overrides (plugins.yml `config:` map, or the per-plugin config
//!     endpoint POST /api/plugins/tools/bundled/fetch/config) reach the
//!     subprocess; the uppercase `ALLOW_UNSAFE_METHODS` spelling is accepted
//!     as a fallback (e.g. when launched from a plain docker-compose env).

use anyhow::Result;
use mcp_server_util::*;
use serde_json::Value;

/// Methods that are always allowed (safe / read-only).
const SAFE_METHODS: [&str; 3] = ["GET", "HEAD", "OPTIONS"];

/// Parse the `allow_unsafe_methods` config from the environment (default false).
fn allow_unsafe_methods_from_env() -> bool {
    for key in ["allow_unsafe_methods", "ALLOW_UNSAFE_METHODS"] {
        if let Ok(raw) = std::env::var(key) {
            let t = raw.trim().to_ascii_lowercase();
            return t == "true" || t == "1" || t == "yes";
        }
    }
    false
}

/// Decide whether `method` may be sent under the given config.
///
/// Safe methods (GET/HEAD/OPTIONS) are always allowed; any other method is
/// allowed only when `allow_unsafe` is true (reqwest validates the final
/// method string, so unknown methods still fail cleanly).
fn method_allowed(method: &str, allow_unsafe: bool) -> bool {
    let upper = method.trim().to_ascii_uppercase();
    if SAFE_METHODS.contains(&upper.as_str()) {
        return true;
    }
    allow_unsafe
}

/// Fetch a URL over HTTP(S).
///
/// Fully async (reqwest async client) with connect + total timeouts — a hung
/// upstream can NEVER block an async worker thread or wedge the plugin
/// runtime (Aug 2026 all-plugins-async push).
async fn handle_fetch(args: Value) -> Result<(String, bool)> {
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'url'"))?;

    let method = args["method"]
        .as_str()
        .unwrap_or("GET")
        .trim()
        .to_ascii_uppercase();

    // Gate non-safe methods BEFORE any request is sent.
    let allow_unsafe = allow_unsafe_methods_from_env();
    if !method_allowed(&method, allow_unsafe) {
        return Ok((
            format!(
                "Error: HTTP method '{}' is not allowed. The fetch plugin only allows \
                 safe/read-only methods (GET, HEAD, OPTIONS) unless the config \
                 `allow_unsafe_methods` is set to true (then POST, PUT, PATCH, \
                 DELETE, etc. are allowed).",
                method
            ),
            true,
        ));
    }

    let method_parsed = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| anyhow::anyhow!("Invalid HTTP method: '{}'", method))?;

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let response = client
        .request(method_parsed, url)
        .header("User-Agent", "OmniAgent/1.0")
        .send()
        .await?;

    let status = response.status();
    let body = response.text().await?;

    // Truncate to ~50K chars
    let max_chars: usize = 50_000;
    let truncated = if body.len() > max_chars {
        format!(
            "{}\n\n[... truncated from {} to ~{} chars]",
            &body[..max_chars],
            body.len(),
            max_chars
        )
    } else {
        body
    };

    let text = format!(
        "HTTP {} {}\n\n{}",
        status.as_u16(),
        status.canonical_reason().unwrap_or(""),
        truncated
    );
    Ok((text, !status.is_success()))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let fetch_handler: ToolHandler = Box::new(|args: Value, _meta: Option<McpMeta>| {
        Box::pin(async move { handle_fetch(args).await })
    });

    let tools = vec![McpToolEntry {
        def: McpToolDef {
            name: "fetch".to_string(),
            description:
                "FETCH/HTTP a URL from the internet (default method GET). Use this to download web pages, API responses, or any HTTP-accessible content. Optional 'method' argument: GET/HEAD/OPTIONS are always allowed; POST/PUT/PATCH/DELETE only when the plugin config allow_unsafe_methods=true. Does NOT work with file:// URLs or local files: use filesystem_read for local files."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
                    },
                    "method": {
                        "type": "string",
                        "enum": ["GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH", "DELETE"],
                        "description": "HTTP method (default GET). Non-safe methods (POST/PUT/PATCH/DELETE) require config allow_unsafe_methods=true."
                    }
                },
                "required": ["url"]
            }),
        },
        handler: fetch_handler,
    }];

    let server_info = ServerInfo {
        name: "mcp-server-fetch".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server(server_info, tools).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_methods_always_allowed() {
        for m in ["GET", "HEAD", "OPTIONS", "get", "Head", "options"] {
            assert!(method_allowed(m, false), "{} should be allowed", m);
            assert!(method_allowed(m, true), "{} should be allowed", m);
        }
    }

    #[test]
    fn unsafe_methods_rejected_by_default() {
        for m in ["POST", "PUT", "PATCH", "DELETE"] {
            assert!(
                !method_allowed(m, false),
                "{} must be rejected when allow_unsafe_methods=false",
                m
            );
        }
    }

    #[test]
    fn unsafe_methods_allowed_when_configured() {
        for m in ["POST", "PUT", "PATCH", "DELETE", "post", "Put"] {
            assert!(method_allowed(m, true), "{} should be allowed", m);
        }
    }

    #[test]
    fn empty_or_unknown_methods() {
        assert!(!method_allowed("", false));
        assert!(!method_allowed("FOOBAR", false));
        // With allow_unsafe=true reqwest still rejects unknown methods at
        // parse time, but the gate itself lets them through.
        assert!(method_allowed("FOOBAR", true));
    }

    #[test]
    fn allow_unsafe_defaults_false_when_env_unset() {
        // Test process env does not define the var → default false.
        assert!(!allow_unsafe_methods_from_env());
    }
}
