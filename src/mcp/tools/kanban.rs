use crate::mcp::{truncate_content, AppContext, McpTool, McpToolResult, DEFAULT_MAX_TOOL_OUTPUT_CHARS};
use anyhow::Result;
use serde_json::Value;
use sql_forge::sql_forge;
use std::sync::Arc;

pub fn create_kanban_task_tool() -> McpTool {
    McpTool {
        name: "create_kanban_task".to_string(),
        description: "Create a new kanban task. Adds a task to the kanban board with optional body, status, priority, and assignee.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Task title"
                },
                "body": {
                    "type": "string",
                    "description": "Optional task description/body"
                },
                "status": {
                    "type": "string",
                    "description": "Optional status (default: 'backlog'). One of: backlog, todo, ready, running, review, done, blocked",
                    "enum": ["backlog", "todo", "ready", "running", "review", "done", "blocked"]
                },
                "priority": {
                    "type": "integer",
                    "description": "Optional priority (default: 0). 0=Low, 1=Med, 3=High, 5=Critical"
                },
                "assignee": {
                    "type": "string",
                    "description": "Optional assignee name"
                }
            },
            "required": ["title"]
        }),
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let title = args["title"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'title'"))?;

            if title.is_empty() {
                anyhow::bail!("Task title must not be empty");
            }

            let body = args["body"].as_str().unwrap_or("");
            let status = args["status"].as_str().unwrap_or("backlog");
            let priority = args["priority"].as_i64().unwrap_or(0) as i32;
            let assignee = args["assignee"].as_str().unwrap_or("");

            // Validate status
            let valid_statuses = ["backlog", "todo", "ready", "running", "review", "done", "blocked"];
            if !valid_statuses.contains(&status) {
                anyhow::bail!("Invalid status '{}'. Must be one of: backlog, todo, ready, running, review, done, blocked", status);
            }

            let pool = ctx.pool.clone();

            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    // Generate a unique ID using a database sequence idiom
                    let id = format!("task_{:x}", {
                        use std::time::{SystemTime, UNIX_EPOCH};
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    });

                    sql_forge!(
                        r#"
                        INSERT INTO kanban_tasks (id, title, body, status, priority, assignee)
                        VALUES (:id, :title, :body, :status, :priority, :assignee)
                        "#,
                        ( :id = &id, :title = title, :body = body, :status = status, :priority = priority, :assignee = assignee )
                    )
                    .execute(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to create kanban task: {}", e))?;

                    Ok::<_, anyhow::Error>(McpToolResult {
                        call_id: String::new(),
                        content: format!("Kanban task '{}' created with id '{}' and status '{}'", title, id, status),
                        is_error: false,
                    })
                })
            })
        }),
    }
}

pub fn list_kanban_tasks_tool() -> McpTool {
    McpTool {
        name: "list_kanban_tasks".to_string(),
        description: "List all kanban tasks, optionally filtered by status. Returns tasks grouped by status column.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "description": "Optional status filter. One of: backlog, todo, ready, running, review, done, blocked"
                }
            }
        }),
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let status_filter = args["status"].as_str().map(|s| s.to_string());
            let pool = ctx.pool.clone();

            let result = tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    if let Some(ref status) = status_filter {
                        sqlx::query_as::<_, (String, String, Option<String>, String, i32, Option<String>, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>(
                            "SELECT id, title, body, status, priority, assignee, created_at, updated_at FROM kanban_tasks WHERE status = $1 ORDER BY priority DESC, created_at DESC",
                        )
                        .bind(status)
                        .fetch_all(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to list kanban tasks: {}", e))
                    } else {
                        sqlx::query_as::<_, (String, String, Option<String>, String, i32, Option<String>, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>(
                            "SELECT id, title, body, status, priority, assignee, created_at, updated_at FROM kanban_tasks ORDER BY status, priority DESC, created_at DESC",
                        )
                        .fetch_all(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to list kanban tasks: {}", e))
                    }
                })
            })?;

            use std::collections::BTreeMap;
            let mut grouped: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();

            for (id, title, body, status, priority, assignee, created_at, updated_at) in &result {
                let entry = serde_json::json!({
                    "id": id,
                    "title": title,
                    "body": body,
                    "status": status,
                    "priority": priority,
                    "assignee": assignee,
                    "created_at": created_at.to_rfc3339(),
                    "updated_at": updated_at.to_rfc3339(),
                });
                grouped.entry(status.clone()).or_default().push(entry);
            }

            let output = serde_json::to_string_pretty(&grouped)?;
            Ok(McpToolResult {
                call_id: String::new(),
                content: truncate_content(&output, DEFAULT_MAX_TOOL_OUTPUT_CHARS),
                is_error: false,
            })
        }),
    }
}

pub fn update_kanban_task_tool() -> McpTool {
    McpTool {
        name: "update_kanban_task".to_string(),
        description: "Update a kanban task's fields (title, body, status, priority, assignee). Only provided fields are updated.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Task ID to update"
                },
                "title": {
                    "type": "string",
                    "description": "New title"
                },
                "body": {
                    "type": "string",
                    "description": "New body/description"
                },
                "status": {
                    "type": "string",
                    "description": "New status. One of: backlog, todo, ready, running, review, done, blocked"
                },
                "priority": {
                    "type": "integer",
                    "description": "New priority. 0=Low, 1=Med, 3=High, 5=Critical"
                },
                "assignee": {
                    "type": "string",
                    "description": "New assignee"
                }
            },
            "required": ["id"]
        }),
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let id = args["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'id'"))?;

            let pool = ctx.pool.clone();
            let id_clone = id.to_string();

            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    // Check task exists
                    let exists: bool = sql_forge!(
                        scalar i64,
                        "SELECT COUNT(*) FROM kanban_tasks WHERE id = :id",
                        ( :id = &id_clone )
                    )
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to check task existence: {}", e))?
                    > 0;

                    if !exists {
                        anyhow::bail!("Kanban task '{}' not found", id_clone);
                    }

                    // Build dynamic UPDATE
                    let mut set_clauses: Vec<String> = Vec::new();
                    let mut params: Vec<serde_json::Value> = Vec::new();
                    let mut param_idx = 2u32;

                    if let Some(title) = args["title"].as_str() {
                        if title.is_empty() {
                            anyhow::bail!("Task title must not be empty");
                        }
                        set_clauses.push(format!("title = ${}", param_idx));
                        params.push(serde_json::json!(title));
                        param_idx += 1;
                    }
                    if args.get("body").is_some() {
                        set_clauses.push(format!("body = ${}", param_idx));
                        params.push(args["body"].clone());
                        param_idx += 1;
                    }
                    if let Some(status) = args["status"].as_str() {
                        let valid_statuses = ["backlog", "todo", "ready", "running", "review", "done", "blocked"];
                        if !valid_statuses.contains(&status) {
                            anyhow::bail!("Invalid status '{}'", status);
                        }
                        set_clauses.push(format!("status = ${}", param_idx));
                        params.push(serde_json::json!(status));
                        param_idx += 1;
                    }
                    if args.get("priority").is_some() {
                        set_clauses.push(format!("priority = ${}", param_idx));
                        params.push(args["priority"].clone());
                        param_idx += 1;
                    }
                    if args.get("assignee").is_some() {
                        set_clauses.push(format!("assignee = ${}", param_idx));
                        params.push(args["assignee"].clone());
                    }

                    if set_clauses.is_empty() {
                        anyhow::bail!("No fields to update");
                    }

                    set_clauses.push("updated_at = NOW()".to_string());

                    let sql = format!(
                        "UPDATE kanban_tasks SET {} WHERE id = $1",
                        set_clauses.join(", ")
                    );

                    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str())).bind(&id_clone);
                    for p in &params {
                        if let Some(s) = p.as_str() {
                            query = query.bind(s);
                        } else if let Some(n) = p.as_i64() {
                            query = query.bind(n as i32);
                        } else if p.is_null() {
                            let val: Option<String> = None;
                            query = query.bind(val);
                        } else {
                            query = query.bind(p.to_string());
                        }
                    }

                    query.execute(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to update kanban task: {}", e))?;

                    Ok::<_, anyhow::Error>(())
                })
            })?;

            Ok(McpToolResult {
                call_id: String::new(),
                content: format!("Kanban task '{}' updated successfully", id),
                is_error: false,
            })
        }),
    }
}

pub fn delete_kanban_task_tool() -> McpTool {
    McpTool {
        name: "delete_kanban_task".to_string(),
        description: "Delete a kanban task by its ID.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Task ID to delete"
                }
            },
            "required": ["id"]
        }),
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let id = args["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'id'"))?;

            let pool = ctx.pool.clone();
            let id_clone = id.to_string();

            let deleted = tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    sql_forge!(
                        "DELETE FROM kanban_tasks WHERE id = :id",
                        ( :id = &id_clone )
                    )
                    .execute(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to delete kanban task: {}", e))
                })
            })?;

            if deleted.rows_affected() == 0 {
                anyhow::bail!("Kanban task '{}' not found", id);
            }

            Ok(McpToolResult {
                call_id: String::new(),
                content: format!("Kanban task '{}' deleted successfully", id),
                is_error: false,
            })
        }),
    }
}
