use crate::mcp::{truncate_content, AppContext, McpTool, McpToolResult, DEFAULT_MAX_TOOL_OUTPUT_CHARS};
use crate::db::types as queries;
use crate::models::MessageNew;
use anyhow::Result;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use chrono::{DateTime, Utc};

#[derive(Debug, FromRow)]
struct KanbanTaskRow {
    id: String,
    title: String,
    body: Option<String>,
    status: String,
    priority: Option<i32>,
    assignee: Option<String>,
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
                        match queries::get_channel_by_platform_name(&pool, "cron", "cron-default").await {
                            Ok(Some(ch)) => ch.id,
                            _ => anyhow::bail!("No default cron channel found. Create a channel with platform='cron' and name='cron-default' first."),
                        }
                    };

                    sql_forge!(
                        r#"
                        INSERT INTO kanban_tasks (id, title, body, status, priority, assignee, channel_id, profile)
                        VALUES (:id, :title, :body, :status, :priority, :assignee, :channel_id, NULLIF(:profile, '')::text)
                        "#,
                        ( :id = &id, :title = title, :body = body, :status = status, :priority = priority, :assignee = assignee, :channel_id = resolved_channel_id, :profile = profile.as_deref().unwrap_or("") )
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
                            SELECT id, title, body, status, priority, assignee, created_at, updated_at
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
                            SELECT id, title, body, status, priority, assignee, created_at, updated_at
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
            let was_status_ready = args["status"].as_str().map(|s| s == "ready").unwrap_or(false);

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

                    // If status is being updated to 'ready', create a pending seq-0 message
                    if was_status_ready {
                        // Fetch the task to get its details
                        #[derive(Debug, FromRow)]
                        struct TaskRow {
                            id: String,
                            title: String,
                            body: Option<String>,
                            status: String,
                            channel_id: Option<i64>,
                            profile: Option<String>,
                        }
                        let task: Option<TaskRow> = sql_forge!(
                            TaskRow,
                            r#"
                            SELECT id, title, body, status, channel_id, profile
                            FROM kanban_tasks
                            WHERE id = :id
                            "#,
                            ( :id = &id_clone )
                        )
                        .fetch_optional(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to fetch task: {e}"))?;

                        if let Some(t) = task {
                            if t.status == "ready" {
                                // Resolve channel_id: task's channel_id, or default cron channel
                                let task_channel_id = if let Some(cid) = t.channel_id {
                                    cid
                                } else {
                                    match queries::get_channel_by_platform_name(&pool, "cron", "cron-default").await {
                                        Ok(Some(ch)) => ch.id,
                                        _ => anyhow::bail!("No default cron channel found for ready task. Create a channel with platform='cron' and name='cron-default' first."),
                                    }
                                };

                                // Resolve profile: task's profile, or channel's current_profile
                                #[derive(Debug, FromRow)]
                                struct ChannelCfgRow {
                                    current_profile: String,
                                    current_provider: Option<String>,
                                    current_model: Option<String>,
                                }
                                let channel_cfg: ChannelCfgRow = sql_forge!(
                                    ChannelCfgRow,
                                    r#"
                                    SELECT current_profile, current_provider, current_model FROM channels WHERE id = :channel_id
                                    "#,
                                    ( :channel_id = task_channel_id )
                                )
                                .fetch_one(&pool)
                                .await
                                .unwrap_or_else(|_| ChannelCfgRow {
                                    current_profile: "default".to_string(),
                                    current_provider: None,
                                    current_model: None,
                                });

                                let profile_name = t.profile.clone().unwrap_or(channel_cfg.current_profile);

                                // Resolve provider+model for stamping on the seq-0 message
                                // Order: channel.current_provider → profile provider → env → default
                                let profile_registry = crate::profile::ProfileRegistry::new(&ctx.data_dir);
                                let kanban_prof = profile_registry.get(&profile_name).cloned().unwrap_or_else(|| {
                                    crate::profile::Profile::default("default")
                                });
                                let provider = channel_cfg.current_provider.clone()
                                    .or_else(|| kanban_prof.provider.clone())
                                    .or_else(|| Some(std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "opencode-go".to_string())));
                                let model = channel_cfg.current_model.clone()
                                    .or_else(|| kanban_prof.model.clone())
                                    .or_else(|| Some(std::env::var("LLM_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string())));

                                let content = if let Some(ref body) = t.body {
                                    if !body.is_empty() {
                                        body.clone()
                                    } else {
                                        format!("Execute kanban task: {}", t.title)
                                    }
                                } else {
                                    format!("Execute kanban task: {}", t.title)
                                };

                                // Create a thread with cause='kanban'
                                let thread = queries::create_thread(
                                    &pool,
                                    "kanban",
                                    task_channel_id,
                                    &profile_name,
                                    provider.as_deref(),
                                    model.as_deref(),
                                )
                                .await
                                .map_err(|e| anyhow::anyhow!("Failed to create thread for ready task '{}': {e}", t.id))?;

                                // Add a cause message with the task content
                                let cause_msg = MessageNew {
                                    thread_id: thread.id,
                                    role: "cause".to_string(),
                                    content,
                                    thread_sequence: 0,
                                    external_id: Some(format!("kanban:{}", t.id)),
                                    metadata: serde_json::json!({
                                        "kanban_task_id": t.id,
                                        "kanban_task_title": t.title,
                                    }),
                                    embedding: None,
                                    summary_text: None,
                                    is_summary: false,
                                    msg_type: "kanban".to_string(),
                                    msg_subtype: Some(t.id.clone()),
                                    processing_time_ms: None,
                                    token_usage: None,
                                };

                                match queries::create_cause_and_set_pending(&pool, &cause_msg).await {
                                    Ok(created) => {
                                        tracing::info!(
                                            "Created cause message {} and set thread {} pending for kanban task '{}' channel {}",
                                            created.id, thread.id, t.id, task_channel_id
                                        );
                                    }
                                    Err(e) => {
                                        anyhow::bail!("Failed to create cause message for ready task '{}': {e}", t.id);
                                    }
                                }

                                // Set thread status to 'pending' — handled inside create_cause_and_set_pending

                                tracing::info!(
                                    "Thread {} set to pending for kanban task '{}'",
                                    thread.id, t.id
                                );
                            }
                        }
                    }

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
