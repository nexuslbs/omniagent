//! mcp-server-subtasks — standalone MCP server for thread subtask management.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: add_subtask, list_subtasks, update_subtask, delete_subtask, get_subtask_counts

use anyhow::{Context, Result};
use mcp_server_util::*;
use omniagent::subtask;
use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Tool: add_subtask
// ---------------------------------------------------------------------------

async fn handle_add(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let thread_id = args["thread_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'thread_id'"))?;
    let description = args["description"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'description'"))?;

    if description.is_empty() {
        anyhow::bail!("Subtask description must not be empty");
    }

    let priority = args["priority"].as_i64().unwrap_or(0) as i32;

    let _subtask = subtask::add_subtask(pool, thread_id, description, priority).await?;

    let counts = subtask::get_subtask_counts(pool, thread_id).await?;
    let current = subtask::get_current_subtask(pool, thread_id).await?;
    let all = subtask::list_subtasks(pool, thread_id).await?;

    let subtasks_json: Vec<Value> = all.iter().map(|s| {
        serde_json::json!({
            "id": s.id,
            "thread_id": s.thread_id,
            "description": s.description,
            "status": s.status,
            "priority": s.priority.unwrap_or(0),
            "created_at": s.created_at,
            "updated_at": s.updated_at,
        })
    }).collect();

    let output = serde_json::json!({
        "current_subtask": current.map(|s| serde_json::json!({
            "id": s.id,
            "description": s.description,
            "status": s.status,
            "priority": s.priority.unwrap_or(0),
        })),
        "completed_count": counts.completed_count,
        "pending_count": counts.pending_count,
        "cancelled_count": counts.cancelled_count,
        "error_count": counts.error_count,
        "subtasks": subtasks_json,
        "message": format!("Subtask added: {}", description),
    });

    Ok((serde_json::to_string_pretty(&output)?, false))
}

// ---------------------------------------------------------------------------
// Tool: list_subtasks
// ---------------------------------------------------------------------------

async fn handle_list(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let thread_id = args["thread_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'thread_id'"))?;

    let all = subtask::list_subtasks(pool, thread_id).await?;
    let counts = subtask::get_subtask_counts(pool, thread_id).await?;
    let current = subtask::get_current_subtask(pool, thread_id).await?;

    let subtasks_json: Vec<Value> = all.iter().map(|s| {
        serde_json::json!({
            "id": s.id,
            "thread_id": s.thread_id,
            "description": s.description,
            "status": s.status,
            "priority": s.priority.unwrap_or(0),
            "created_at": s.created_at,
            "updated_at": s.updated_at,
        })
    }).collect();

    let output = serde_json::json!({
        "current_subtask": current.map(|s| serde_json::json!({
            "id": s.id,
            "description": s.description,
            "status": s.status,
            "priority": s.priority.unwrap_or(0),
        })),
        "completed_count": counts.completed_count,
        "pending_count": counts.pending_count,
        "cancelled_count": counts.cancelled_count,
        "error_count": counts.error_count,
        "subtasks": subtasks_json,
    });

    Ok((serde_json::to_string_pretty(&output)?, false))
}

// ---------------------------------------------------------------------------
// Tool: update_subtask
// ---------------------------------------------------------------------------

async fn handle_update(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let subtask_id = args["subtask_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'subtask_id'"))?;

    // Need thread_id for counts after update
    let thread_id = args["thread_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'thread_id'"))?;

    let mut updated_any = false;

    if let Some(status) = args["status"].as_str() {
        let valid_statuses = ["pending", "completed", "cancelled", "error"];
        if !valid_statuses.contains(&status) {
            anyhow::bail!("Invalid status '{}'. Must be one of: pending, completed, cancelled, error", status);
        }
        let rows = subtask::update_subtask_status(pool, subtask_id, status).await?;
        if rows == 0 {
            anyhow::bail!("Subtask {} not found", subtask_id);
        }
        updated_any = true;
    }

    if let Some(description) = args["description"].as_str() {
        if !description.is_empty() {
            let rows = subtask::update_subtask_description(pool, subtask_id, description).await?;
            if rows == 0 {
                anyhow::bail!("Subtask {} not found", subtask_id);
            }
            updated_any = true;
        }
    }

    if !updated_any {
        anyhow::bail!("No fields provided to update. Specify 'status' or 'description'.");
    }

    let counts = subtask::get_subtask_counts(pool, thread_id).await?;
    let current = subtask::get_current_subtask(pool, thread_id).await?;
    let all = subtask::list_subtasks(pool, thread_id).await?;

    let subtasks_json: Vec<Value> = all.iter().map(|s| {
        serde_json::json!({
            "id": s.id,
            "thread_id": s.thread_id,
            "description": s.description,
            "status": s.status,
            "priority": s.priority.unwrap_or(0),
            "created_at": s.created_at,
            "updated_at": s.updated_at,
        })
    }).collect();

    let output = serde_json::json!({
        "current_subtask": current.map(|s| serde_json::json!({
            "id": s.id,
            "description": s.description,
            "status": s.status,
            "priority": s.priority.unwrap_or(0),
        })),
        "completed_count": counts.completed_count,
        "pending_count": counts.pending_count,
        "cancelled_count": counts.cancelled_count,
        "error_count": counts.error_count,
        "subtasks": subtasks_json,
        "message": format!("Subtask {} updated successfully", subtask_id),
    });

    Ok((serde_json::to_string_pretty(&output)?, false))
}

// ---------------------------------------------------------------------------
// Tool: delete_subtask
// ---------------------------------------------------------------------------

async fn handle_delete(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let subtask_id = args["subtask_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'subtask_id'"))?;

    let thread_id = args["thread_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'thread_id'"))?;

    let rows = subtask::delete_subtask(pool, subtask_id).await?;
    if rows == 0 {
        anyhow::bail!("Subtask {} not found", subtask_id);
    }

    let counts = subtask::get_subtask_counts(pool, thread_id).await?;
    let current = subtask::get_current_subtask(pool, thread_id).await?;
    let all = subtask::list_subtasks(pool, thread_id).await?;

    let subtasks_json: Vec<Value> = all.iter().map(|s| {
        serde_json::json!({
            "id": s.id,
            "thread_id": s.thread_id,
            "description": s.description,
            "status": s.status,
            "priority": s.priority.unwrap_or(0),
            "created_at": s.created_at,
            "updated_at": s.updated_at,
        })
    }).collect();

    let output = serde_json::json!({
        "current_subtask": current.map(|s| serde_json::json!({
            "id": s.id,
            "description": s.description,
            "status": s.status,
            "priority": s.priority.unwrap_or(0),
        })),
        "completed_count": counts.completed_count,
        "pending_count": counts.pending_count,
        "cancelled_count": counts.cancelled_count,
        "error_count": counts.error_count,
        "subtasks": subtasks_json,
        "message": format!("Subtask {} deleted", subtask_id),
    });

    Ok((serde_json::to_string_pretty(&output)?, false))
}

// ---------------------------------------------------------------------------
// Tool: get_subtask_counts
// ---------------------------------------------------------------------------

async fn handle_get_counts(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let thread_id = args["thread_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'thread_id'"))?;

    let counts = subtask::get_subtask_counts(pool, thread_id).await?;
    let current = subtask::get_current_subtask(pool, thread_id).await?;

    let output = serde_json::json!({
        "current_subtask": current.map(|s| serde_json::json!({
            "id": s.id,
            "description": s.description,
            "status": s.status,
            "priority": s.priority.unwrap_or(0),
        })),
        "completed_count": counts.completed_count,
        "pending_count": counts.pending_count,
        "cancelled_count": counts.cancelled_count,
        "error_count": counts.error_count,
    });

    Ok((serde_json::to_string_pretty(&output)?, false))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let database_url =
        std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let pool = omniagent::db::connect(&database_url).await
        .context("Failed to connect to database")?;
    let pool = Arc::new(pool);

    // Wrap each handler to capture a clone of the pool
    let p_add = pool.clone();
    let add_handler: ToolHandler = Box::new(move |args: Value| Box::pin(async move { handle_add(&p_add, &args).await }));
    let p_list = pool.clone();
    let list_handler: ToolHandler = Box::new(move |args: Value| Box::pin(async move { handle_list(&p_list, &args).await }));
    let p_upd = pool.clone();
    let update_handler: ToolHandler = Box::new(move |args: Value| Box::pin(async move { handle_update(&p_upd, &args).await }));
    let p_del = pool.clone();
    let delete_handler: ToolHandler = Box::new(move |args: Value| Box::pin(async move { handle_delete(&p_del, &args).await }));
    let p_cnt = pool.clone();
    let counts_handler: ToolHandler = Box::new(move |args: Value| Box::pin(async move { handle_get_counts(&p_cnt, &args).await }));

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "add_subtask".to_string(),
                description:
                    "Add a new subtask to a thread. Subtasks are actionable items that belong to a thread. \
                     Returns the current subtask, counts, and full subtask list."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "thread_id": { "type": "integer", "description": "The thread ID to add the subtask to" },
                        "description": { "type": "string", "description": "Subtask description (required)" },
                        "priority": { "type": "integer", "description": "Subtask priority (default: 0). Higher = more important." },
                    },
                    "required": ["thread_id", "description"],
                }),
            },
            handler: add_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "list_subtasks".to_string(),
                description:
                    "List all subtasks for a thread, ordered by priority then creation time. \
                     Returns current subtask, counts, and full subtask list."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "thread_id": { "type": "integer", "description": "The thread ID to list subtasks for" },
                    },
                    "required": ["thread_id"],
                }),
            },
            handler: list_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "update_subtask".to_string(),
                description:
                    "Update a subtask's status and/or description. Status can be: pending, completed, cancelled, error."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "subtask_id": { "type": "integer", "description": "The subtask ID to update" },
                        "thread_id": { "type": "integer", "description": "The thread ID the subtask belongs to" },
                        "status": {
                            "type": "string",
                            "description": "New status: pending, completed, cancelled, error",
                            "enum": ["pending", "completed", "cancelled", "error"]
                        },
                        "description": { "type": "string", "description": "New description for the subtask" },
                    },
                    "required": ["subtask_id", "thread_id"],
                }),
            },
            handler: update_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "delete_subtask".to_string(),
                description:
                    "Delete a subtask by its ID. Returns the updated subtask list and counts."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "subtask_id": { "type": "integer", "description": "The subtask ID to delete" },
                        "thread_id": { "type": "integer", "description": "The thread ID the subtask belongs to" },
                    },
                    "required": ["subtask_id", "thread_id"],
                }),
            },
            handler: delete_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "get_subtask_counts".to_string(),
                description:
                    "Get subtask counts and current subtask for a thread. Returns completed, pending, cancelled, error counts."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "thread_id": { "type": "integer", "description": "The thread ID to get counts for" },
                    },
                    "required": ["thread_id"],
                }),
            },
            handler: counts_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-subtasks".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server(server_info, tools).await
}
