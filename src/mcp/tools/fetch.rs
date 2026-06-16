use crate::mcp::{AppContext, McpTool, McpToolResult};
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

pub fn fetch_tool() -> McpTool {
    McpTool {
        name: "fetch".to_string(),
        description: "Make an HTTP GET request to a URL. Returns the response body as text. Use for research, API calls, and web scraping.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                },
                "headers": {
                    "type": "object",
                    "description": "Optional HTTP headers to include",
                    "additionalProperties": {"type": "string"}
                }
            },
            "required": ["url"]
        }),
        handler: Arc::new(|args: Value, _ctx: AppContext| -> Result<McpToolResult> {
            let url = args["url"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing 'url' argument"))?;

            // Validate URL scheme
            let parsed = url::Url::parse(url)
                .map_err(|e| anyhow::anyhow!("Invalid URL '{}': {}", url, e))?;
            match parsed.scheme() {
                "http" | "https" => {}
                scheme => anyhow::bail!("Unsupported URL scheme '{}'. Only http/https allowed.", scheme),
            }

            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent("OmniAgent/1.0")
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))?;

            let mut req = client.get(url);
            if let Some(headers) = args["headers"].as_object() {
                for (key, val) in headers {
                    if let Some(val_str) = val.as_str() {
                        req = req.header(key, val_str);
                    }
                }
            }

            let resp = req
                .send()
                .map_err(|e| anyhow::anyhow!("HTTP request failed: {}", e))?;

            let status = resp.status();
            let body = resp
                .text()
                .map_err(|e| anyhow::anyhow!("Failed to read response body: {}", e))?;

            let content = format!(
                "Status: {}\n\n{}",
                status.as_u16(),
                if body.len() > 50000 {
                    format!("{}... [truncated to 50000 chars]", &body[..50000])
                } else {
                    body
                }
            );

            Ok(McpToolResult {
                call_id: String::new(),
                content,
                is_error: !status.is_success(),
            })
        }),
    }
}
