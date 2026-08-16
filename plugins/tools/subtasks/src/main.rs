//! mcp-server-subtasks: standalone MCP server for thread subtask management.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: add_subtask, list_subtasks, update_subtask, delete_subtask, get_subtask_counts,
//!        manage_subtasks (unified tool with `action` param).

#![allow(dead_code)]

use anyhow::Result;
use mcp_server_util::*;
use omniagent::subtask;
use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Tool: add_subtask
// ---------------------------------------------------------------------------

async fn handle_add(pool: &PgPool, args: &Value, meta: Option<&McpMeta>) -> Result<(String, bool)> {
    let thread_id = args["thread_id"]
        .as_i64()
        .or_else(|| meta.and_then(|m| m.thread_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Missing 'thread_id' (no current thread in context). Pass thread_id explicitly."
            )
        })?;
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

    let subtasks_json: Vec<Value> = all
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "thread_id": s.thread_id,
                "description": s.description,
                "status": s.status,
                "priority": s.priority.unwrap_or(0),
                "created_at": s.created_at,
                "updated_at": s.updated_at,
            })
        })
        .collect();

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

async fn handle_list(
    pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let thread_id = args["thread_id"]
        .as_i64()
        .or_else(|| meta.and_then(|m| m.thread_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Missing 'thread_id' (no current thread in context). Pass thread_id explicitly."
            )
        })?;

    let all = subtask::list_subtasks(pool, thread_id).await?;
    let counts = subtask::get_subtask_counts(pool, thread_id).await?;
    let current = subtask::get_current_subtask(pool, thread_id).await?;

    let subtasks_json: Vec<Value> = all
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "thread_id": s.thread_id,
                "description": s.description,
                "status": s.status,
                "priority": s.priority.unwrap_or(0),
                "created_at": s.created_at,
                "updated_at": s.updated_at,
            })
        })
        .collect();

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

async fn handle_update(
    pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let subtask_id = args["subtask_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'subtask_id'"))?;

    // Need thread_id for counts after update
    let thread_id = args["thread_id"]
        .as_i64()
        .or_else(|| meta.and_then(|m| m.thread_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Missing 'thread_id' (no current thread in context). Pass thread_id explicitly."
            )
        })?;

    let mut updated_any = false;

    if let Some(status) = args["status"].as_str() {
        let valid_statuses = ["pending", "completed", "cancelled", "error"];
        if !valid_statuses.contains(&status) {
            anyhow::bail!(
                "Invalid status '{}'. Must be one of: pending, completed, cancelled, error",
                status
            );
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

    let subtasks_json: Vec<Value> = all
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "thread_id": s.thread_id,
                "description": s.description,
                "status": s.status,
                "priority": s.priority.unwrap_or(0),
                "created_at": s.created_at,
                "updated_at": s.updated_at,
            })
        })
        .collect();

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

async fn handle_delete(
    pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let subtask_id = args["subtask_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'subtask_id'"))?;

    let thread_id = args["thread_id"]
        .as_i64()
        .or_else(|| meta.and_then(|m| m.thread_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Missing 'thread_id' (no current thread in context). Pass thread_id explicitly."
            )
        })?;

    let rows = subtask::delete_subtask(pool, subtask_id).await?;
    if rows == 0 {
        anyhow::bail!("Subtask {} not found", subtask_id);
    }

    let counts = subtask::get_subtask_counts(pool, thread_id).await?;
    let current = subtask::get_current_subtask(pool, thread_id).await?;
    let all = subtask::list_subtasks(pool, thread_id).await?;

    let subtasks_json: Vec<Value> = all
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "thread_id": s.thread_id,
                "description": s.description,
                "status": s.status,
                "priority": s.priority.unwrap_or(0),
                "created_at": s.created_at,
                "updated_at": s.updated_at,
            })
        })
        .collect();

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

async fn handle_get_counts(
    pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let thread_id = args["thread_id"]
        .as_i64()
        .or_else(|| meta.and_then(|m| m.thread_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Missing 'thread_id' (no current thread in context). Pass thread_id explicitly."
            )
        })?;

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
// Tool: manage_subtasks (unified) — action: add | list | update | delete | get_counts
// ---------------------------------------------------------------------------

/// The unified `manage_subtasks` action, validated from the incoming args.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManageAction {
    Add,
    List,
    Update,
    Delete,
    GetCounts,
}

/// Parse + validate the `action` argument and the per-action required fields.
/// Pure function (no DB access) — unit-tested.
fn parse_manage_action(args: &Value) -> Result<ManageAction> {
    let action = args["action"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'action'"))?;
    match action {
        "add" => {
            let desc = args["description"].as_str().ok_or_else(|| {
                anyhow::anyhow!("Missing required argument: 'description' for action='add'")
            })?;
            if desc.trim().is_empty() {
                anyhow::bail!("Subtask description must not be empty");
            }
            Ok(ManageAction::Add)
        }
        "list" => Ok(ManageAction::List),
        "update" => {
            args["subtask_id"].as_i64().ok_or_else(|| {
                anyhow::anyhow!("Missing required argument: 'subtask_id' for action='update'")
            })?;
            let has_status = args["status"].as_str().is_some();
            let has_desc = args["description"].as_str().is_some();
            if !has_status && !has_desc {
                anyhow::bail!("No fields provided to update. Specify 'status' or 'description'.");
            }
            if let Some(status) = args["status"].as_str() {
                let valid_statuses = ["pending", "completed", "cancelled", "error"];
                if !valid_statuses.contains(&status) {
                    anyhow::bail!(
                        "Invalid status '{}'. Must be one of: pending, completed, cancelled, error",
                        status
                    );
                }
            }
            Ok(ManageAction::Update)
        }
        "delete" => {
            args["subtask_id"].as_i64().ok_or_else(|| {
                anyhow::anyhow!("Missing required argument: 'subtask_id' for action='delete'")
            })?;
            Ok(ManageAction::Delete)
        }
        "get_counts" => Ok(ManageAction::GetCounts),
        other => anyhow::bail!(
            "Invalid action '{}'. Must be one of: add, list, update, delete, get_counts",
            other
        ),
    }
}

fn subtask_json(s: &subtask::SubtaskRow) -> Value {
    serde_json::json!({
        "id": s.id,
        "thread_id": s.thread_id,
        "description": s.description,
        "status": s.status,
        "priority": s.priority.unwrap_or(0),
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

fn current_subtask_json(s: Option<&subtask::SubtaskRow>) -> Value {
    match s {
        Some(s) => serde_json::json!({
            "id": s.id,
            "description": s.description,
            "status": s.status,
            "priority": s.priority.unwrap_or(0),
        }),
        None => Value::Null,
    }
}

fn counts_json(c: &subtask::SubtaskCounts) -> Value {
    serde_json::json!({
        "completed_count": c.completed_count,
        "pending_count": c.pending_count,
        "cancelled_count": c.cancelled_count,
        "error_count": c.error_count,
        "total_count": c.total_count,
    })
}

async fn handle_manage(
    pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let action = parse_manage_action(args)?;

    let thread_id = args["thread_id"]
        .as_i64()
        .or_else(|| meta.and_then(|m| m.thread_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Missing 'thread_id' (no current thread in context). Pass thread_id explicitly."
            )
        })?;

    match action {
        ManageAction::Add => {
            let description = args["description"].as_str().unwrap_or("");
            let priority = args["priority"].as_i64().unwrap_or(0) as i32;
            let added = subtask::add_subtask(pool, thread_id, description, priority).await?;
            let counts = subtask::get_subtask_counts(pool, thread_id).await?;
            let current = subtask::get_current_subtask(pool, thread_id).await?;
            let output = serde_json::json!({
                "action": "add",
                "id": added.id,
                "subtask": subtask_json(&added),
                "counts": counts_json(&counts),
                "current_subtask": current_subtask_json(current.as_ref()),
                "message": format!("Subtask added: {}", description),
            });
            Ok((serde_json::to_string_pretty(&output)?, false))
        }
        ManageAction::List => {
            let all = subtask::list_subtasks(pool, thread_id).await?;
            let counts = subtask::get_subtask_counts(pool, thread_id).await?;
            let current = subtask::get_current_subtask(pool, thread_id).await?;
            let subtasks_json: Vec<Value> = all.iter().map(subtask_json).collect();
            let output = serde_json::json!({
                "action": "list",
                "counts": counts_json(&counts),
                "current_subtask": current_subtask_json(current.as_ref()),
                "subtasks": subtasks_json,
            });
            Ok((serde_json::to_string_pretty(&output)?, false))
        }
        ManageAction::Update => {
            let subtask_id = args["subtask_id"].as_i64().unwrap_or(0);
            let mut updated_any = false;
            if let Some(status) = args["status"].as_str() {
                let rows = subtask::update_subtask_status(pool, subtask_id, status).await?;
                if rows == 0 {
                    anyhow::bail!("Subtask {} not found", subtask_id);
                }
                updated_any = true;
            }
            if let Some(description) = args["description"].as_str() {
                if !description.is_empty() {
                    let rows =
                        subtask::update_subtask_description(pool, subtask_id, description).await?;
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
            let output = serde_json::json!({
                "action": "update",
                "subtask_id": subtask_id,
                "counts": counts_json(&counts),
                "current_subtask": current_subtask_json(current.as_ref()),
                "message": format!("Subtask {} updated successfully", subtask_id),
            });
            Ok((serde_json::to_string_pretty(&output)?, false))
        }
        ManageAction::Delete => {
            let subtask_id = args["subtask_id"].as_i64().unwrap_or(0);
            let rows = subtask::delete_subtask(pool, subtask_id).await?;
            if rows == 0 {
                anyhow::bail!("Subtask {} not found", subtask_id);
            }
            let counts = subtask::get_subtask_counts(pool, thread_id).await?;
            let current = subtask::get_current_subtask(pool, thread_id).await?;
            let output = serde_json::json!({
                "action": "delete",
                "subtask_id": subtask_id,
                "counts": counts_json(&counts),
                "current_subtask": current_subtask_json(current.as_ref()),
                "message": format!("Subtask {} deleted", subtask_id),
            });
            Ok((serde_json::to_string_pretty(&output)?, false))
        }
        ManageAction::GetCounts => {
            let counts = subtask::get_subtask_counts(pool, thread_id).await?;
            let current = subtask::get_current_subtask(pool, thread_id).await?;
            let output = serde_json::json!({
                "action": "get_counts",
                "counts": counts_json(&counts),
                "current_subtask": current_subtask_json(current.as_ref()),
            });
            Ok((serde_json::to_string_pretty(&output)?, false))
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin config hook
// ---------------------------------------------------------------------------

/// Callback invoked when the host sends configuration via configure message.
/// Plugin config — received via configure message.
#[derive(Debug, Clone)]
struct PluginConfig {
    pub database_url: String,
}

impl PluginConfig {
    fn from_json(v: &serde_json::Value) -> Self {
        Self {
            database_url: v
                .get("database_url")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    eprintln!("FATAL: database_url not in configure message");
                    std::process::exit(1);
                }),
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Shared pool — populated by configure callback before any tool call
    let pool = Arc::new(RwLock::new(None::<PgPool>));

    // Wrap each handler to capture a clone of the pool
    let p_add = pool.clone();
    let add_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_add.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_add(&pool, &args, meta.as_ref()).await
        })
    });
    let p_list = pool.clone();
    let list_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_list.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_list(&pool, &args, meta.as_ref()).await
        })
    });
    let p_upd = pool.clone();
    let update_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_upd.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_update(&pool, &args, meta.as_ref()).await
        })
    });
    let p_del = pool.clone();
    let delete_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_del.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_delete(&pool, &args, meta.as_ref()).await
        })
    });
    let p_cnt = pool.clone();
    let counts_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_cnt.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_get_counts(&pool, &args, meta.as_ref()).await
        })
    });
    let p_mng = pool.clone();
    let manage_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_mng.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_manage(&pool, &args, meta.as_ref()).await
        })
    });

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
                        "thread_id": { "type": "integer", "description": "The thread ID to add the subtask to (default: current thread)" },
                        "description": { "type": "string", "description": "Subtask description (required)" },
                        "priority": { "type": "integer", "description": "Subtask priority (default: 0). Higher = more important." },
                    },
                    "required": ["description"],
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
                        "thread_id": { "type": "integer", "description": "The thread ID to list subtasks for (default: current thread)" },
                    },
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
                        "thread_id": { "type": "integer", "description": "The thread ID the subtask belongs to (default: current thread)" },
                        "status": {
                            "type": "string",
                            "description": "New status: pending, completed, cancelled, error",
                            "enum": ["pending", "completed", "cancelled", "error"]
                        },
                        "description": { "type": "string", "description": "New description for the subtask" },
                    },
                    "required": ["subtask_id"],
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
                        "thread_id": { "type": "integer", "description": "The thread ID the subtask belongs to (default: current thread)" },
                    },
                    "required": ["subtask_id"],
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
                        "thread_id": { "type": "integer", "description": "The thread ID to get counts for (default: current thread)" },
                    },
                }),
            },
            handler: counts_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "manage_subtasks".to_string(),
                description:
                    "Manage subtasks for a thread with a single tool. `action` selects the operation: \
                 'add' (requires description, optional priority), 'list' (full list + counts), \
                 'update' (requires subtask_id + status and/or description), 'delete' (requires subtask_id), \
                 'get_counts' (counts + current subtask). Returns compact output: counts + affected \
                 row, NOT the full list on every call (token efficiency)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "description": "Operation to perform: add, list, update, delete, get_counts",
                            "enum": ["add", "list", "update", "delete", "get_counts"]
                        },
                        "thread_id": { "type": "integer", "description": "The thread ID (default: current thread)" },
                        "description": { "type": "string", "description": "Subtask description (required for add; optional for update)" },
                        "priority": { "type": "integer", "description": "Subtask priority for add (default: 0). Higher = more important." },
                        "subtask_id": { "type": "integer", "description": "The subtask ID (required for update/delete)" },
                        "status": {
                            "type": "string",
                            "description": "New status for update: pending, completed, cancelled, error",
                            "enum": ["pending", "completed", "cancelled", "error"]
                        },
                    },
                    "required": ["action"],
                }),
            },
            handler: manage_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-subtasks".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server_with_config(server_info, tools, {
        let p = pool.clone();
        Some(move |params: serde_json::Value| {
            let config = PluginConfig::from_json(&params);
            tokio::task::block_in_place(|| {
                let rt = tokio::runtime::Handle::current();
                let new_pool = rt
                    .block_on(omniagent::db::connect(&config.database_url))
                    .expect("Failed to connect to database");
                *p.blocking_write() = Some(new_pool);
            });
            tracing::info!("Subtasks plugin configured with database_url");
        })
    })
    .await
}

#[cfg(test)]
mod manage_action_tests {
    use super::*;

    #[test]
    fn parse_add_valid() {
        let args = serde_json::json!({
            "action": "add",
            "description": "Read the task body",
            "priority": 2,
        });
        assert_eq!(parse_manage_action(&args).unwrap(), ManageAction::Add);
    }

    #[test]
    fn parse_add_missing_description() {
        let args = serde_json::json!({"action": "add"});
        let err = parse_manage_action(&args).unwrap_err();
        assert!(err.to_string().contains("description"), "{err}");
    }

    #[test]
    fn parse_add_empty_description() {
        let args = serde_json::json!({"action": "add", "description": "   "});
        let err = parse_manage_action(&args).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn parse_list_valid() {
        let args = serde_json::json!({"action": "list"});
        assert_eq!(parse_manage_action(&args).unwrap(), ManageAction::List);
    }

    #[test]
    fn parse_update_valid_status() {
        let args = serde_json::json!({
            "action": "update",
            "subtask_id": 7,
            "status": "completed",
        });
        assert_eq!(parse_manage_action(&args).unwrap(), ManageAction::Update);
    }

    #[test]
    fn parse_update_missing_subtask_id() {
        let args = serde_json::json!({"action": "update", "status": "completed"});
        let err = parse_manage_action(&args).unwrap_err();
        assert!(err.to_string().contains("subtask_id"), "{err}");
    }

    #[test]
    fn parse_update_no_fields() {
        let args = serde_json::json!({"action": "update", "subtask_id": 7});
        let err = parse_manage_action(&args).unwrap_err();
        assert!(err.to_string().contains("No fields"), "{err}");
    }

    #[test]
    fn parse_update_invalid_status() {
        let args = serde_json::json!({
            "action": "update",
            "subtask_id": 7,
            "status": "banana",
        });
        let err = parse_manage_action(&args).unwrap_err();
        assert!(err.to_string().contains("Invalid status"), "{err}");
    }

    #[test]
    fn parse_delete_valid() {
        let args = serde_json::json!({"action": "delete", "subtask_id": 3});
        assert_eq!(parse_manage_action(&args).unwrap(), ManageAction::Delete);
    }

    #[test]
    fn parse_delete_missing_subtask_id() {
        let args = serde_json::json!({"action": "delete"});
        let err = parse_manage_action(&args).unwrap_err();
        assert!(err.to_string().contains("subtask_id"), "{err}");
    }

    #[test]
    fn parse_get_counts_valid() {
        let args = serde_json::json!({"action": "get_counts"});
        assert_eq!(parse_manage_action(&args).unwrap(), ManageAction::GetCounts);
    }

    #[test]
    fn parse_missing_action() {
        let args = serde_json::json!({});
        let err = parse_manage_action(&args).unwrap_err();
        assert!(err.to_string().contains("'action'"), "{err}");
    }

    #[test]
    fn parse_invalid_action() {
        let args = serde_json::json!({"action": "explode"});
        let err = parse_manage_action(&args).unwrap_err();
        assert!(err.to_string().contains("Invalid action"), "{err}");
    }
}
