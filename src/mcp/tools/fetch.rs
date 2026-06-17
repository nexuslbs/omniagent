use crate::mcp::{truncate_content, AppContext, McpTool, McpToolResult, DEFAULT_MAX_TOOL_OUTPUT_CHARS};
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

pub fn fetch_tool() -> McpTool {
    McpTool {
        name: "fetch".to_string(),
        description: "FETCH/HTTP GET a URL from the internet. Use this to download web pages, API responses, or any HTTP-accessible content. Does NOT work with file:// URLs or local files — use filesystem_read for local files.".to_string(),
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
        handler: Arc::new(|args: Value, _ctx: AppContext| -> Result<McpToolResult> {
            let url = args["url"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing 'url' argument"))?;

            let url = url.to_string();

            // Use block_in_place + Handle::current() to run async reqwest from sync handler.
            // The inner async block returns a typed Result to help inference.
            let mcp_result: McpToolResult = tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                let inner: std::result::Result<McpToolResult, anyhow::Error> = handle.block_on(async {
                    let client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(30))
                        .build()?;

                    let response = client
                        .get(&url)
                        .header("User-Agent", "OmniAgent/1.0")
                        .send()
                        .await?;

                    let status = response.status();
                    let body = response.text().await?;

                    Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!(
                            "HTTP {} {}\n\n{}",
                            status.as_u16(),
                            status.canonical_reason().unwrap_or(""),
                            truncate_content(&body, DEFAULT_MAX_TOOL_OUTPUT_CHARS)
                        ),
                        is_error: !status.is_success(),
                    })
                });
                inner
            })?;

            Ok(mcp_result)
        }),
    }
}
