//! mcp-server-fetch: standalone MCP server for HTTP GET requests.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: fetch

use anyhow::Result;
use mcp_server_util::*;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Tool: fetch
// ---------------------------------------------------------------------------

fn handle_fetch(args: Value) -> Result<(String, bool)> {
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'url'"))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let response = client
        .get(url)
        .header("User-Agent", "OmniAgent/1.0")
        .send()?;

    let status = response.status();
    let body = response.text()?;

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

    let text = format!("HTTP {} {}\n\n{}", status.as_u16(), status.canonical_reason().unwrap_or(""), truncated);
    Ok((text, !status.is_success()))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let fetch_handler: AsyncToolHandler = Box::new(|args: Value| {
        Box::pin(async move { handle_fetch(args) })
    });

    let tools = vec![McpToolEntry {
        def: McpToolDef {
            name: "fetch".to_string(),
            description:
                "FETCH/HTTP GET a URL from the internet. Use this to download web pages, API responses, or any HTTP-accessible content. Does NOT work with file:// URLs or local files: use filesystem_read for local files."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
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
