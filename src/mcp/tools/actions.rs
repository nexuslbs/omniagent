//! Built-in action tools — each system action is its own MCP tool
//! instead of a single "actions" tool with a command parameter.
//!
//! Tools: actions:kanban_dispatcher, actions:relevance_indexer,
//!        actions:hindsight_populator, actions:setup_knowledge_pipeline

use serde_json::Value;
use std::sync::Arc;

use crate::mcp::{AppContext, McpTool, McpToolResult};

/// Returns 4 separate MCP tools, one per system action.
/// Each has server_name="actions" so qualified names become "actions:<name>".
pub fn tools() -> Vec<McpTool> {
    vec![
        kanban_dispatcher_tool(),
        relevance_indexer_tool(),
        hindsight_populator_tool(),
        setup_knowledge_pipeline_tool(),
    ]
}

fn kanban_dispatcher_tool() -> McpTool {
    McpTool {
        name: "kanban_dispatcher".to_string(),
        description: "Process pending kanban tasks: move 'todo' tasks to 'ready' by creating threads and messages, respecting dependencies and ordering by priority and position.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
        }),
        server_name: Some("actions".to_string()),
        handler: Arc::new(|_: Value, ctx: AppContext| {
            Box::pin(async move {
                let pool = ctx.pool.clone();
                let data_dir = ctx.data_dir.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::scheduler::run_kanban_dispatcher(&pool, &data_dir).await {
                        tracing::error!("[actions] kanban_dispatcher failed: {:?}", e);
                    }
                });
                Ok(McpToolResult {
                    call_id: String::new(),
                    content: "Kanban dispatcher triggered".to_string(),
                    is_error: false,
                })
            })
        }),
    }
}

fn relevance_indexer_tool() -> McpTool {
    McpTool {
        name: "relevance_indexer".to_string(),
        description: "Update the wiki relevance index. Scans wiki files and updates Qdrant vector index entries.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
        }),
        server_name: Some("actions".to_string()),
        handler: Arc::new(|_: Value, ctx: AppContext| {
            Box::pin(async move {
                let pool = ctx.pool.clone();
                let data_dir = ctx.data_dir.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::relevance::run_relevance_indexer(&pool, &data_dir).await {
                        tracing::error!("[actions] relevance_indexer failed: {:?}", e);
                    }
                });
                Ok(McpToolResult {
                    call_id: String::new(),
                    content: "Relevance indexer triggered".to_string(),
                    is_error: false,
                })
            })
        }),
    }
}

fn hindsight_populator_tool() -> McpTool {
    McpTool {
        name: "hindsight_populator".to_string(),
        description: "Retain recent messages into Hindsight memory. Queries new messages since the last watermark and sends them to the hindsight memory server for long-term persistent recall.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
        }),
        server_name: Some("actions".to_string()),
        handler: Arc::new(|_: Value, ctx: AppContext| {
            Box::pin(async move {
                let pool = ctx.pool.clone();
                let data_dir = ctx.data_dir.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::hindsight_populator::run_hindsight_populator(&pool, &data_dir).await {
                        tracing::error!("[actions] hindsight_populator failed: {:?}", e);
                    }
                });
                Ok(McpToolResult {
                    call_id: String::new(),
                    content: "Hindsight populator triggered".to_string(),
                    is_error: false,
                })
            })
        }),
    }
}

fn setup_knowledge_pipeline_tool() -> McpTool {
    McpTool {
        name: "setup_knowledge_pipeline".to_string(),
        description: "Create or verify the periodic knowledge pipeline cron job. Loads the knowledge-pipeline template and runs the periodic maintenance pipeline (summarize channels, update wiki/skills, relevance indexing, hindsight populate).".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "schedule": {
                    "type": "string",
                    "description": "Optional cron schedule in 5-field Linux format. Default: '0 */6 * * *'."
                },
                "prompt": {
                    "type": "string",
                    "description": "Optional prompt override."
                }
            },
            "required": [],
        }),
        server_name: Some("actions".to_string()),
        handler: Arc::new(|args: Value, ctx: AppContext| {
            Box::pin(async move {
                let pool = ctx.pool.clone();
                let data_dir = ctx.data_dir.clone();
                let schedule = args.get("schedule").and_then(|v| v.as_str()).map(|s| s.to_string());
                let prompt = args.get("prompt").and_then(|v| v.as_str()).map(|s| s.to_string());

                match crate::scheduler::setup_knowledge_pipeline(&pool, &data_dir, schedule, prompt).await {
                    Ok(()) => {
                        tracing::info!("[actions] Knowledge pipeline cron created/verified");
                        Ok(McpToolResult {
                            call_id: String::new(),
                            content: "Knowledge Pipeline cron job created or already exists. It will run every 6 hours (default) to summarize channels, update wiki/skills, run relevance indexing, and populate hindsight.".to_string(),
                            is_error: false,
                        })
                    }
                    Err(e) => {
                        tracing::error!("[actions] Knowledge pipeline setup failed: {:?}", e);
                        Ok(McpToolResult {
                            call_id: String::new(),
                            content: format!("Failed to create Knowledge Pipeline: {}", e),
                            is_error: true,
                        })
                    }
                }
            })
        }),
    }
}
