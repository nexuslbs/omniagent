use crate::mcp::{truncate_content, AppContext, McpTool, McpToolResult, DEFAULT_MAX_TOOL_OUTPUT_CHARS};
use crate::db::types as queries;
use anyhow::Result;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use chrono::{DateTime, Utc};

#[derive(Debug, FromRow)]
struct KanbanTaskRow {
    id: String,
    #[expect(dead_code)]
    display_id: Option<i64>,
    title: String,
    body: Option<String>,
    status: String,
    priority: Option<i32>,
    assignee: Option<String>,
    #[expect(dead_code)]
    template: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

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
                },
                "channel_id": {
                    "type": "integer",
                    "description": "Optional channel ID to execute this task in. If omitted, uses the default cron channel."
                },
                "profile": {
                    "type": "string",
                    "description": "Optional profile name to use when executing this task. If omitted, uses the channel's current_profile."
                },
                "template": {
                    "type": "string",
                    "description": "Optional template name to use for structured guidance when executing this task. Templates are .md files in profiles/<name>/templates/"
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
            let channel_id_arg = args["channel_id"].as_i64();
            let profile = args["profile"].as_str().map(|s| s.to_string());
            let template = args["template"].as_str().unwrap_or("");

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

                    // Resolve channel_id: if not provided, find the default cron channel
                    let resolved_channel_id = if let Some(cid) = channel_id_arg {
                        cid
                    } else {
                        match queries::get_channel_by_platform_name(&pool, "cron", "cron").await {
                            Ok(Some(ch)) => ch.id,
                            _ => anyhow::bail!("No default cron channel found. Create a channel with platform='cron' and name='cron' first."),
                        }
                    };

                    sql_forge!(
                        r#"
                        INSERT INTO kanban_tasks (id, title, body, status, priority, assignee, channel_id, profile, template)
                        VALUES (:id, :title, :body, :status, :priority, :assignee, :channel_id, NULLIF(:profile, '')::text, :template)
                        "#,
                        ( :id = &id, :title = title, :body = body, :status = status, :priority = priority, :assignee = assignee, :channel_id = resolved_channel_id, :profile = profile.as_deref().unwrap_or(""), :template = template )
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

            let result: Vec<KanbanTaskRow> = tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    if let Some(ref status) = status_filter {
                        sql_forge!(
                            KanbanTaskRow,
                            r#"
                            SELECT id, display_id, title, body, status, priority, assignee, template, created_at, updated_at
                            FROM kanban_tasks
                            WHERE status = :status
                            ORDER BY priority DESC, created_at DESC
                            "#,
                            ( :status = status )
                        )
                        .fetch_all(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to list kanban tasks: {}", e))
                    } else {
                        sql_forge!(
                            KanbanTaskRow,
                            r#"
                            SELECT id, display_id, title, body, status, priority, assignee, template, created_at, updated_at
                            FROM kanban_tasks
                            WHERE 1 = :_one
                            ORDER BY status, priority DESC, created_at DESC
                            "#,
                            ( :_one = 1i32 )
                        )
                        .fetch_all(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to list kanban tasks: {}", e))
                    }
                })
            })?;

            use std::collections::BTreeMap;
            let mut grouped: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();

            for r in &result {
                let entry = serde_json::json!({
                    "id": r.id,
                    "title": r.title,
                    "body": r.body,
                    "status": r.status,
                    "priority": r.priority.unwrap_or(0),
                    "assignee": r.assignee,
                    "created_at": r.created_at.map(|t| t.to_rfc3339()),
                    "updated_at": r.updated_at.map(|t| t.to_rfc3339()),
                });
                grouped.entry(r.status.clone()).or_default().push(entry);
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
                },
                "channel_id": {
                    "type": "integer",
                    "description": "Optional channel ID to execute this task in. If omitted, keeps existing value."
                },
                "profile": {
                    "type": "string",
                    "description": "Optional profile name to use when executing this task. If omitted, keeps existing value."
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
                    .map_err(|e| anyhow::anyhow!("Failed to check task existence: {e}"))?
                    > 0;

                    if !exists {
                        anyhow::bail!("Kanban task '{id_clone}' not found");
                    }

                    // Apply individual UPDATEs per provided field (static SQL per field)
                    if let Some(title) = args["title"].as_str() {
                        if title.is_empty() {
                            anyhow::bail!("Task title must not be empty");
                        }
                        sql_forge!(
                            "UPDATE kanban_tasks SET title = :val, updated_at = NOW() WHERE id = :id",
                            ( :val = title, :id = &id_clone )
                        )
                        .execute(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to update title: {e}"))?;
                    }
                    if args.get("body").is_some() {
                        let body = args["body"].as_str().unwrap_or("");
                        sql_forge!(
                            "UPDATE kanban_tasks SET body = :val, updated_at = NOW() WHERE id = :id",
                            ( :val = body, :id = &id_clone )
                        )
                        .execute(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to update body: {e}"))?;
                    }
                    if let Some(status) = args["status"].as_str() {
                        let valid_statuses = ["backlog", "todo", "ready", "running", "review", "done", "blocked"];
                        if !valid_statuses.contains(&status) {
                            anyhow::bail!("Invalid status '{status}'");
                        }
                        sql_forge!(
                            "UPDATE kanban_tasks SET status = :val, updated_at = NOW() WHERE id = :id",
                            ( :val = status, :id = &id_clone )
                        )
                        .execute(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to update status: {e}"))?;
                    }
                    if args.get("priority").is_some() {
                        let priority = args["priority"].as_i64().unwrap_or(0) as i32;
                        sql_forge!(
                            "UPDATE kanban_tasks SET priority = :val, updated_at = NOW() WHERE id = :id",
                            ( :val = priority, :id = &id_clone )
                        )
                        .execute(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to update priority: {e}"))?;
                    }
                    if args.get("assignee").is_some() {
                        let assignee = args["assignee"].as_str().unwrap_or("");
                        sql_forge!(
                            "UPDATE kanban_tasks SET assignee = :val, updated_at = NOW() WHERE id = :id",
                            ( :val = assignee, :id = &id_clone )
                        )
                        .execute(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to update assignee: {e}"))?;
                    }
                    if args.get("channel_id").is_some() {
                        let cid = args["channel_id"].as_i64().unwrap_or(0);
                        sql_forge!(
                            "UPDATE kanban_tasks SET channel_id = :val, updated_at = NOW() WHERE id = :id",
                            ( :val = cid, :id = &id_clone )
                        )
                        .execute(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to update channel_id: {e}"))?;
                    }
                    if args.get("profile").is_some() {
                        let profile = args["profile"].as_str().unwrap_or("");
                        sql_forge!(
                            "UPDATE kanban_tasks SET profile = NULLIF(:val, '')::text, updated_at = NOW() WHERE id = :id",
                            ( :val = profile, :id = &id_clone )
                        )
                        .execute(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to update profile: {e}"))?;
                    }

                    // Note: Todo→ready flow is handled by kanban_dispatcher cron job

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

pub fn add_kanban_dependency_tool() -> McpTool {
    McpTool {
        name: "add_kanban_dependency".to_string(),
        description: "Add a dependency from one kanban task to another. The dependent task will only proceed from todo to ready when the dependency is done or archived.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task that depends on another"
                },
                "depends_on_id": {
                    "type": "string",
                    "description": "The task that must be completed first"
                }
            },
            "required": ["task_id", "depends_on_id"]
        }),
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let task_id = args["task_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'task_id'"))?;
            let depends_on_id = args["depends_on_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'depends_on_id'"))?;

            if task_id == depends_on_id {
                anyhow::bail!("A task cannot depend on itself");
            }

            let pool = ctx.pool.clone();
            let task_id_clone = task_id.to_string();
            let depends_on_id_clone = depends_on_id.to_string();

            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    let exists: i64 = sql_forge!(
                        scalar i64,
                        "SELECT COUNT(*) FROM kanban_tasks WHERE id IN (:tid, :did)",
                        ( :tid = &task_id_clone, :did = &depends_on_id_clone )
                    )
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to verify tasks: {e}"))?;

                    if exists != 2 {
                        anyhow::bail!("One or both kanban tasks not found");
                    }

                    sql_forge!(
                        r#"
                        INSERT INTO kanban_task_dependencies (task_id, depends_on_id)
                        VALUES (:task_id, :depends_on_id)
                        ON CONFLICT (task_id, depends_on_id) DO NOTHING
                        "#,
                        ( :task_id = &task_id_clone, :depends_on_id = &depends_on_id_clone )
                    )
                    .execute(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to add dependency: {e}"))?;

                    Ok::<_, anyhow::Error>(())
                })
            })?;

            Ok(McpToolResult {
                call_id: String::new(),
                content: format!("Dependency added: '{}' now depends on '{}'", task_id, depends_on_id),
                is_error: false,
            })
        }),
    }
}

pub fn remove_kanban_dependency_tool() -> McpTool {
    McpTool {
        name: "remove_kanban_dependency".to_string(),
        description: "Remove a dependency between two kanban tasks.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The dependent task"
                },
                "depends_on_id": {
                    "type": "string",
                    "description": "The task it no longer depends on"
                }
            },
            "required": ["task_id", "depends_on_id"]
        }),
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let task_id = args["task_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'task_id'"))?;
            let depends_on_id = args["depends_on_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'depends_on_id'"))?;

            let pool = ctx.pool.clone();
            let task_id_clone = task_id.to_string();
            let depends_on_id_clone = depends_on_id.to_string();

            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    let result = sql_forge!(
                        "DELETE FROM kanban_task_dependencies WHERE task_id = :task_id AND depends_on_id = :depends_on_id",
                        ( :task_id = &task_id_clone, :depends_on_id = &depends_on_id_clone )
                    )
                    .execute(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to remove dependency: {e}"))?;

                    if result.rows_affected() == 0 {
                        anyhow::bail!("Dependency not found between '{}' and '{}'", task_id, depends_on_id);
                    }

                    Ok::<_, anyhow::Error>(())
                })
            })?;

            Ok(McpToolResult {
                call_id: String::new(),
                content: format!("Dependency removed between '{}' and '{}'", task_id, depends_on_id),
                is_error: false,
            })
        }),
    }
}
