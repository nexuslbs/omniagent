use crate::mcp::{truncate_content, AppContext, McpTool, McpToolResult, DEFAULT_MAX_TOOL_OUTPUT_CHARS};
use crate::subtask;
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

pub fn manage_subtasks_tool() -> McpTool {
    McpTool {
        name: "manage_subtasks".to_string(),
        description: "Manage thread subtasks. Supports actions: add, list, update, delete, get_counts. \
                      Returns structured JSON with current_subtask, completed_count, pending_count, and subtasks array."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "thread_id": {
                    "type": "integer",
                    "description": "Optional. The thread ID to manage subtasks for. If omitted, the current thread is used."
                },
                "action": {
                    "type": "string",
                    "description": "Action to perform: add, list, update, delete, get_counts",
                    "enum": ["add", "list", "update", "delete", "get_counts"]
                },
                "description": {
                    "type": "string",
                    "description": "Subtask description (required for 'add', optional for 'update')"
                },
                "subtask_id": {
                    "type": "integer",
                    "description": "Subtask ID (required for 'update' and 'delete')"
                },
                "status": {
                    "type": "string",
                    "description": "New status for the subtask (for 'update': pending, completed, cancelled, error)",
                    "enum": ["pending", "completed", "cancelled", "error"]
                },
                "priority": {
                    "type": "integer",
                    "description": "Subtask priority (for 'add', default: 0). Higher = more important."
                }
            },
            "required": ["action"]
        }),
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let thread_id = if let Some(id) = args["thread_id"].as_i64() {
                id
            } else if let Some(id) = ctx.current_thread_id {
                id
            } else {
                anyhow::bail!("'thread_id' is required when not running within a thread context");
            };

            let action = args["action"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'action'"))?;

            let pool = ctx.pool.clone();

            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    match action {
                        "add" => {
                            let description = args["description"]
                                .as_str()
                                .ok_or_else(|| anyhow::anyhow!("Missing required argument for 'add': 'description'"))?;

                            if description.is_empty() {
                                anyhow::bail!("Subtask description must not be empty");
                            }

                            let priority = args["priority"].as_i64().unwrap_or(0) as i32;

                            let _subtask = subtask::add_subtask(&pool, thread_id, description, priority).await?;

                            let counts = subtask::get_subtask_counts(&pool, thread_id).await?;
                            let current = subtask::get_current_subtask(&pool, thread_id).await?;
                            let all = subtask::list_subtasks(&pool, thread_id).await?;

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

                            Ok(McpToolResult {
                                call_id: String::new(),
                                content: truncate_content(&serde_json::to_string_pretty(&output)?, DEFAULT_MAX_TOOL_OUTPUT_CHARS),
                                is_error: false,
                            })
                        }

                        "list" => {
                            let all = subtask::list_subtasks(&pool, thread_id).await?;
                            let counts = subtask::get_subtask_counts(&pool, thread_id).await?;
                            let current = subtask::get_current_subtask(&pool, thread_id).await?;

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

                            Ok(McpToolResult {
                                call_id: String::new(),
                                content: truncate_content(&serde_json::to_string_pretty(&output)?, DEFAULT_MAX_TOOL_OUTPUT_CHARS),
                                is_error: false,
                            })
                        }

                        "update" => {
                            let subtask_id = args["subtask_id"]
                                .as_i64()
                                .ok_or_else(|| anyhow::anyhow!("Missing required argument for 'update': 'subtask_id'"))?;

                            let mut updated_any = false;

                            if let Some(status) = args["status"].as_str() {
                                let valid_statuses = ["pending", "completed", "cancelled", "error"];
                                if !valid_statuses.contains(&status) {
                                    anyhow::bail!("Invalid status '{}'. Must be one of: pending, completed, cancelled, error", status);
                                }
                                let rows = subtask::update_subtask_status(&pool, subtask_id, status).await?;
                                if rows == 0 {
                                    anyhow::bail!("Subtask {} not found", subtask_id);
                                }
                                updated_any = true;
                            }

                            if let Some(description) = args["description"].as_str() {
                                if !description.is_empty() {
                                    let rows = subtask::update_subtask_description(&pool, subtask_id, description).await?;
                                    if rows == 0 {
                                        anyhow::bail!("Subtask {} not found", subtask_id);
                                    }
                                    updated_any = true;
                                }
                            }

                            if !updated_any {
                                anyhow::bail!("No fields provided to update. Specify 'status' or 'description'.");
                            }

                            let counts = subtask::get_subtask_counts(&pool, thread_id).await?;
                            let current = subtask::get_current_subtask(&pool, thread_id).await?;
                            let all = subtask::list_subtasks(&pool, thread_id).await?;

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

                            Ok(McpToolResult {
                                call_id: String::new(),
                                content: truncate_content(&serde_json::to_string_pretty(&output)?, DEFAULT_MAX_TOOL_OUTPUT_CHARS),
                                is_error: false,
                            })
                        }

                        "delete" => {
                            let subtask_id = args["subtask_id"]
                                .as_i64()
                                .ok_or_else(|| anyhow::anyhow!("Missing required argument for 'delete': 'subtask_id'"))?;

                            let rows = subtask::delete_subtask(&pool, subtask_id).await?;
                            if rows == 0 {
                                anyhow::bail!("Subtask {} not found", subtask_id);
                            }

                            let counts = subtask::get_subtask_counts(&pool, thread_id).await?;
                            let current = subtask::get_current_subtask(&pool, thread_id).await?;
                            let all = subtask::list_subtasks(&pool, thread_id).await?;

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

                            Ok(McpToolResult {
                                call_id: String::new(),
                                content: truncate_content(&serde_json::to_string_pretty(&output)?, DEFAULT_MAX_TOOL_OUTPUT_CHARS),
                                is_error: false,
                            })
                        }

                        "get_counts" => {
                            let counts = subtask::get_subtask_counts(&pool, thread_id).await?;
                            let current = subtask::get_current_subtask(&pool, thread_id).await?;

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

                            Ok(McpToolResult {
                                call_id: String::new(),
                                content: truncate_content(&serde_json::to_string_pretty(&output)?, DEFAULT_MAX_TOOL_OUTPUT_CHARS),
                                is_error: false,
                            })
                        }

                        _ => {
                            anyhow::bail!("Unknown action '{}'. Allowed: add, list, update, delete, get_counts", action);
                        }
                    }
                })
            })
        }),
    }
}
