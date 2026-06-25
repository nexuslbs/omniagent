//! Built-in "actions" tool — triggers system actions like kanban dispatcher,
//! relevance indexer, and hindsight populator.

use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use crate::mcp::{AppContext, McpTool, McpToolResult};

/// Returns the list of built-in action tools.
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
        name: "actions_kanban_dispatcher".to_string(),
        description: "Trigger the kanban dispatcher — picks up pending kanban tasks and creates agent threads for them.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
        server_name: None,
        handler: Arc::new(|_args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let pool = ctx.pool.clone();
            let data_dir = ctx.data_dir.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::scheduler::run_kanban_dispatcher(&pool, &data_dir).await {
                    tracing::error!("[actions] kanban_dispatcher failed: {:?}", e);
                }
            });
            Ok(McpToolResult {
                call_id: "".to_string(),
                content: "Kanban dispatcher triggered".to_string(),
                is_error: false,
            })
        }),
    }
}

fn relevance_indexer_tool() -> McpTool {
    McpTool {
        name: "actions_relevance_indexer".to_string(),
        description: "Trigger the relevance indexer — scans wiki files and updates the relevant-index.md based on recency and reference count.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
        server_name: None,
        handler: Arc::new(|_args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let pool = ctx.pool.clone();
            let data_dir = ctx.data_dir.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::relevance::run_relevance_indexer(&pool, &data_dir).await {
                    tracing::error!("[actions] relevance_indexer failed: {:?}", e);
                }
            });
            Ok(McpToolResult {
                call_id: "".to_string(),
                content: "Relevance indexer triggered".to_string(),
                is_error: false,
            })
        }),
    }
}

fn hindsight_populator_tool() -> McpTool {
    McpTool {
        name: "actions_hindsight_populator".to_string(),
        description: "Trigger the hindsight populator — queries recent messages from the database and retains them into the omniagent-hindsight persistent memory store for future recall.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
        server_name: None,
        handler: Arc::new(|_args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let pool = ctx.pool.clone();
            let data_dir = ctx.data_dir.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::hindsight_populator::run_hindsight_populator(&pool, &data_dir).await {
                    tracing::error!("[actions] hindsight_populator failed: {:?}", e);
                }
            });
            Ok(McpToolResult {
                call_id: "".to_string(),
                content: "Hindsight populator triggered".to_string(),
                is_error: false,
            })
        }),
    }
}

fn setup_knowledge_pipeline_tool() -> McpTool {
    McpTool {
        name: "actions_setup_knowledge_pipeline".to_string(),
        description: "Create the Knowledge Pipeline cron job (idempotent). Sets up a periodic maintenance pipeline that summarizes channels, updates wiki/skills from thread messages, runs relevance indexing, and populates hindsight memory. Accepts optional schedule and prompt overrides (default: every 6 hours).".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "schedule": {
                    "type": "string",
                    "description": "Optional cron schedule expression (default: '0 */6 * * *' = every 6 hours)"
                },
                "prompt": {
                    "type": "string",
                    "description": "Optional prompt override (default: 'Execute the Knowledge Pipeline according to the task template above.')"
                }
            },
            "required": []
        }),
        server_name: None,
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let pool = ctx.pool.clone();
            let data_dir = ctx.data_dir.clone();
            let schedule = args.get("schedule").and_then(|v| v.as_str()).map(|s| s.to_string());
            let prompt = args.get("prompt").and_then(|v| v.as_str()).map(|s| s.to_string());

            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    match crate::scheduler::setup_knowledge_pipeline(&pool, &data_dir, schedule, prompt).await {
                        Ok(()) => {
                            tracing::info!("[actions] Knowledge pipeline cron created/verified");
                            Ok(McpToolResult {
                                call_id: "".to_string(),
                                content: "Knowledge Pipeline cron job created or already exists. It will run every 6 hours (default) to summarize channels, update wiki/skills, run relevance indexing, and populate hindsight.".to_string(),
                                is_error: false,
                            })
                        }
                        Err(e) => {
                            tracing::error!("[actions] Knowledge pipeline setup failed: {:?}", e);
                            Ok(McpToolResult {
                                call_id: "".to_string(),
                                content: format!("Failed to create Knowledge Pipeline: {}", e),
                                is_error: true,
                            })
                        }
                    }
                })
            })
        }),
    }
}
