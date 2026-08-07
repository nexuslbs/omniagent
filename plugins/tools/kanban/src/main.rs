//! mcp-server-kanban: standalone MCP server for kanban task management.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: create_kanban_task, list_kanban_tasks, update_kanban_task,
//!        delete_kanban_task, add_kanban_dependency, remove_kanban_dependency

use anyhow::Result;
use mcp_server_util::*;
use omniagent::agent::{manual_review_decision, validate_review_decision};
use omniagent::db;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use unicode_normalization::UnicodeNormalization;

use chrono::{DateTime, Utc};
use sqlx::FromRow;

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
struct KanbanTaskRow {
    id: String,
    title: String,
    body: Option<String>,
    status: String,
    priority: Option<i32>,
    assignee: Option<String>,
    #[allow(dead_code)]
    template: Option<String>,
    created_at: Option<DateTime<Utc>>,
    #[allow(dead_code)]
    updated_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Kanban history helper
// ---------------------------------------------------------------------------

/// Insert a history record via a direct sqlx query.
async fn insert_history(
    pool: &PgPool,
    task_id: &str,
    action: &str,
    initial_board: Option<&str>,
    final_board: Option<&str>,
    previous_values: Option<serde_json::Value>,
) -> Result<()> {
    db::kanban::insert_kanban_history(
        pool,
        task_id,
        action,
        initial_board,
        final_board,
        previous_values,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to insert kanban history: {e}"))
}

/// Build a JSON with the current task fields for previous_values.
fn task_to_json(task: &db::kanban::KanbanTaskDb) -> serde_json::Value {
    serde_json::json!({
        "title": task.title,
        "body": task.body,
        "status": task.status,
        "priority": task.priority,
        "assignee": task.assignee,
        "profile": task.profile,
        "template": task.template,
        "archived": task.archived,
        "channel_id": task.channel_id,
        "plan": task.plan,
    })
}

// ---------------------------------------------------------------------------
// Tool: create_kanban_task
// ---------------------------------------------------------------------------

async fn handle_create(
    pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let title = args["title"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'title'"))?;

    if title.is_empty() {
        return Err(anyhow::anyhow!("Task title must not be empty"));
    }

    let body = args["body"].as_str().unwrap_or("");
    let status = args["status"].as_str().unwrap_or("backlog");
    let priority = args["priority"].as_i64().unwrap_or(0) as i32;
    let assignee = args["assignee"].as_str().unwrap_or("");
    // channel_id: explicit argument wins; else the CURRENT channel from the
    // agent's runtime context (_meta.channel_id); else the default cron channel.
    let channel_id_arg = args["channel_id"]
        .as_i64()
        .or_else(|| meta.and_then(|m| m.channel_id));
    // profile: explicit argument wins; else the agent's ACTIVE profile from
    // _meta.profile_name (the task runs under that profile when dispatched).
    let profile = args["profile"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| meta.and_then(|m| m.profile_name.clone()));
    let template = args["template"].as_str().unwrap_or("");
    let workflow_id = args["workflow_id"].as_str().map(|s| s.to_string());

    // Validate status
    let valid_statuses = [
        "backlog", "todo", "running", "testing", "review", "blocked", "done",
    ];
    if !valid_statuses.contains(&status) {
        return Err(anyhow::anyhow!(
            "Invalid status '{}'. Must be one of: backlog, todo, running, testing, review, blocked, done",
            status
        ));
    }

    // Generate a human-readable id from the title: lowercase, strip diacritics,
    // replace non-alphanumeric with "-", prefix "task_", suffix "_<unix_timestamp>"
    // Use NFD decomposition so accented chars like "é" become "e" + combining mark,
    // then we keep only ASCII lowercase letters and digits.
    let slug_base: String = title
        .chars()
        .flat_map(|c| c.to_lowercase())
        .collect::<String>()
        .nfd()
        .collect::<String>()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect::<String>();
    let slug = if slug_base.is_empty() {
        "task".to_string()
    } else {
        slug_base
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let id = format!("task_{}_{}", slug, ts);

    // Resolve channel_id: if not provided, find the default cron channel
    let resolved_channel_id = if let Some(cid) = channel_id_arg {
        cid
    } else {
        match db::types::get_channel_by_platform_name(pool, "cron", "cron").await {
            Ok(Some(ch)) => ch.id,
            _ => anyhow::bail!(
                "No default cron channel found. Create a channel with platform='cron' and name='cron' first."
            ),
        }
    };

    sql_forge!(
        r#"
        INSERT INTO kanban_tasks (id, title, body, status, priority, assignee, channel_id, profile, template, workflow_id)
        VALUES (:id, :title, :body, :status, :priority, :assignee, :channel_id, NULLIF(:profile, '')::text, :template, NULLIF(:workflow_id, '')::text)
        "#,
        ( :id = &id, :title = title, :body = body, :status = status, :priority = priority, :assignee = assignee, :channel_id = resolved_channel_id, :profile = profile.as_deref().unwrap_or(""), :template = template, :workflow_id = workflow_id.as_deref().unwrap_or("") )
    )
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to create kanban task: {}", e))?;

    // ── Kanban history: record creation ──
    insert_history(pool, &id, "created", None, Some(status), None).await?;

    Ok((
        format!(
            "Kanban task '{}' created with id '{}' and status '{}'",
            title, id, status
        ),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: list_kanban_tasks
// ---------------------------------------------------------------------------

async fn handle_list(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let _status_filter = args["status"].as_str().map(|s| s.to_string());

    let result: Vec<KanbanTaskRow> = sql_forge!(
        KanbanTaskRow,
        r#"
        SELECT id, title, body, status, priority, assignee, template, created_at, updated_at
        FROM kanban_tasks
        ORDER BY status, priority DESC, created_at DESC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to list kanban tasks: {e}"))?;

    if result.is_empty() {
        return Ok(("_No kanban tasks found._".to_string(), false));
    }

    // Group by status
    let mut grouped: BTreeMap<String, Vec<&KanbanTaskRow>> = BTreeMap::new();
    for r in &result {
        grouped.entry(r.status.clone()).or_default().push(r);
    }

    let mut lines = vec!["**Kanban Tasks:**".to_string()];
    for (status, tasks) in &grouped {
        lines.push(format!("\n**{}** ({} tasks):", status, tasks.len()));
        for (i, task) in tasks.iter().enumerate() {
            let priority_str = match task.priority.unwrap_or(0) {
                5 => "🔴 Critical".to_string(),
                3 => "🟠 High".to_string(),
                1 => "🟡 Med".to_string(),
                _ => "⚪ Low".to_string(),
            };
            let assignee_str = task.assignee.as_deref().unwrap_or("unassigned");
            let body_preview = task
                .body
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect::<String>();
            let created = task
                .created_at
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "?".to_string());
            lines.push(format!(
                "  {}. **{}** (`{}`)\n     - Priority: {} | Assignee: {} | Created: {}\n     - {}",
                i + 1,
                task.title,
                task.id,
                priority_str,
                assignee_str,
                created,
                body_preview
            ));
        }
    }

    Ok((lines.join("\n"), false))
}

// ---------------------------------------------------------------------------
// Tool: update_kanban_task
// ---------------------------------------------------------------------------

async fn handle_update(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let id = args["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'id'"))?;

    let id_clone = id.to_string();

    // Fetch the task before update to record previous_values
    let before = db::kanban::get_kanban_task(pool, &id_clone)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch task before update: {e}"))?;

    let before_row = match before {
        Some(t) => t,
        None => anyhow::bail!("Kanban task '{id_clone}' not found"),
    };

    let old_status = before_row.status.clone();

    // Build previous_values with ALL current fields before the edit
    let all_previous = task_to_json(&before_row);

    // ── Apply field updates ──
    let mut has_field_changes = false;

    if let Some(title) = args["title"].as_str() {
        if title.is_empty() {
            anyhow::bail!("Task title must not be empty");
        }
        has_field_changes = true;
        sql_forge!(
            "UPDATE kanban_tasks SET title = :val, updated_at = NOW() WHERE id = :id",
            ( :val = title, :id = &id_clone )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update title: {e}"))?;
    }
    if args.get("body").is_some() {
        let body = args["body"].as_str().unwrap_or("");
        has_field_changes = true;
        sql_forge!(
            "UPDATE kanban_tasks SET body = :val, updated_at = NOW() WHERE id = :id",
            ( :val = body, :id = &id_clone )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update body: {e}"))?;
    }
    if let Some(status) = args["status"].as_str() {
        let valid_statuses = [
            "backlog", "todo", "running", "testing", "review", "blocked", "done",
        ];
        if !valid_statuses.contains(&status) {
            anyhow::bail!("Invalid status '{status}'");
        }
        has_field_changes = true;
        sql_forge!(
            "UPDATE kanban_tasks SET status = :val, updated_at = NOW() WHERE id = :id",
            ( :val = status, :id = &id_clone )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update status: {e}"))?;
    }
    if args.get("priority").is_some() {
        let priority = args["priority"].as_i64().unwrap_or(0) as i32;
        has_field_changes = true;
        sql_forge!(
            "UPDATE kanban_tasks SET priority = :val, updated_at = NOW() WHERE id = :id",
            ( :val = priority, :id = &id_clone )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update priority: {e}"))?;
    }
    if args.get("assignee").is_some() {
        let assignee = args["assignee"].as_str().unwrap_or("");
        has_field_changes = true;
        sql_forge!(
            "UPDATE kanban_tasks SET assignee = :val, updated_at = NOW() WHERE id = :id",
            ( :val = assignee, :id = &id_clone )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update assignee: {e}"))?;
    }
    if args.get("channel_id").is_some() {
        let cid = args["channel_id"].as_i64().unwrap_or(0);
        has_field_changes = true;
        sql_forge!(
            "UPDATE kanban_tasks SET channel_id = :val, updated_at = NOW() WHERE id = :id",
            ( :val = cid, :id = &id_clone )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update channel_id: {e}"))?;
    }
    if args.get("profile").is_some() {
        let profile = args["profile"].as_str().unwrap_or("");
        has_field_changes = true;
        sql_forge!(
            "UPDATE kanban_tasks SET profile = NULLIF(:val, '')::text, updated_at = NOW() WHERE id = :id",
            ( :val = profile, :id = &id_clone )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update profile: {e}"))?;
    }
    // Handle archive/unarchive via the "archived" field
    if let Some(archived) = args["archived"].as_bool() {
        has_field_changes = true;
        sql_forge!(
            "UPDATE kanban_tasks SET archived = :val, updated_at = NOW() WHERE id = :id",
            ( :val = archived, :id = &id_clone )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update archived: {e}"))?;
    }

    if args.get("workflow_id").is_some() {
        let workflow_id = args["workflow_id"].as_str().unwrap_or("");
        has_field_changes = true;
        sql_forge!(
            "UPDATE kanban_tasks SET workflow_id = NULLIF(:val, '')::text, updated_at = NOW() WHERE id = :id",
            ( :val = workflow_id, :id = &id_clone )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update workflow_id: {e}"))?;
    }

    // ── Kanban history ──
    // previous_values is ONLY stored for "edited" and "deleted" actions (not move/archive/unarchive)
    let new_status = args["status"].as_str();
    let archived = args["archived"].as_bool();

    if let Some(arch) = archived {
        if arch {
            insert_history(pool, &id_clone, "archived", None, None, None).await?;
        } else {
            insert_history(pool, &id_clone, "unarchived", None, None, None).await?;
        }
    } else if let Some(new_st) = new_status {
        if new_st != old_status {
            insert_history(
                pool,
                &id_clone,
                "moved",
                Some(&old_status),
                Some(new_st),
                None,
            )
            .await?;
        } else if has_field_changes {
            // Same status, other fields changed: log as "edited" with full previous values
            insert_history(pool, &id_clone, "edited", None, None, Some(all_previous)).await?;
        }
    } else if has_field_changes {
        // No status change, other fields changed: log as "edited" with full previous values
        insert_history(pool, &id_clone, "edited", None, None, Some(all_previous)).await?;
    }

    Ok((format!("Kanban task '{}' updated successfully", id), false))
}

// ---------------------------------------------------------------------------
// Tool: delete_kanban_task
// ---------------------------------------------------------------------------

async fn handle_delete(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let id = args["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'id'"))?;

    let id_clone = id.to_string();

    // Fetch the task before deleting to capture previous_values
    let before = db::kanban::get_kanban_task(pool, &id_clone)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch task before delete: {e}"))?;

    let previous_json = before.as_ref().map(task_to_json);

    // ── Kanban history: record deletion with full previous values ──
    insert_history(pool, &id_clone, "deleted", None, None, previous_json).await?;

    // Remove all dependency rows referencing this task (both as task_id and depends_on_id)
    sql_forge!(
        "DELETE FROM kanban_task_dependencies WHERE task_id = :id OR depends_on_id = :id",
        ( :id = &id_clone )
    )
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to clean up dependencies for task '{}': {e}", id))?;

    let deleted = sql_forge!(
        "DELETE FROM kanban_tasks WHERE id = :id",
        ( :id = &id_clone )
    )
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to delete kanban task: {}", e))?;

    if deleted.rows_affected() == 0 {
        return Err(anyhow::anyhow!("Kanban task '{}' not found", id));
    }

    Ok((format!("Kanban task '{}' deleted successfully", id), false))
}

// ---------------------------------------------------------------------------
// Tool: add_kanban_dependency
// ---------------------------------------------------------------------------

async fn handle_add_dependency(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let task_id = args["task_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'task_id'"))?;
    let depends_on_id = args["depends_on_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'depends_on_id'"))?;

    if task_id == depends_on_id {
        return Err(anyhow::anyhow!("A task cannot depend on itself"));
    }

    let task_id_clone = task_id.to_string();
    let depends_on_id_clone = depends_on_id.to_string();

    let exists: i64 = sql_forge!(
        scalar i64,
        "SELECT COUNT(*) FROM kanban_tasks WHERE id IN (:tid, :did)",
        ( :tid = &task_id_clone, :did = &depends_on_id_clone )
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to verify tasks: {e}"))?;

    if exists != 2 {
        anyhow::bail!("One or both kanban tasks not found");
    }

    // Circular dependency check: if the reverse relationship already exists, bail
    let reverse_count: i64 = sql_forge!(
        scalar i64,
        "SELECT COUNT(*) FROM kanban_task_dependencies WHERE task_id = :did AND depends_on_id = :tid",
        ( :did = &depends_on_id_clone, :tid = &task_id_clone )
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to check for circular dependency: {e}"))?;

    if reverse_count > 0 {
        anyhow::bail!(
            "Circular dependency detected: '{}' already depends on '{}'. Cannot add reverse dependency.",
            depends_on_id,
            task_id
        );
    }

    // Check for duplicate dependency
    let duplicate_count: i64 = sql_forge!(
        scalar i64,
        "SELECT COUNT(*) FROM kanban_task_dependencies WHERE task_id = :tid AND depends_on_id = :did",
        ( :tid = &task_id_clone, :did = &depends_on_id_clone )
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to check for duplicate dependency: {e}"))?;

    if duplicate_count > 0 {
        anyhow::bail!(
            "Duplicate dependency: task '{}' already depends on '{}'",
            task_id,
            depends_on_id
        );
    }

    sql_forge!(
        r#"
        INSERT INTO kanban_task_dependencies (task_id, depends_on_id)
        VALUES (:task_id, :depends_on_id)
        "#,
        ( :task_id = &task_id_clone, :depends_on_id = &depends_on_id_clone )
    )
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to add dependency: {e}"))?;

    Ok((
        format!(
            "Dependency added: '{}' now depends on '{}'",
            task_id, depends_on_id
        ),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: remove_kanban_dependency
// ---------------------------------------------------------------------------

async fn handle_remove_dependency(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let task_id = args["task_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'task_id'"))?;
    let depends_on_id = args["depends_on_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'depends_on_id'"))?;

    let task_id_clone = task_id.to_string();
    let depends_on_id_clone = depends_on_id.to_string();

    let result = sql_forge!(
        "DELETE FROM kanban_task_dependencies WHERE task_id = :task_id AND depends_on_id = :depends_on_id",
        ( :task_id = &task_id_clone, :depends_on_id = &depends_on_id_clone )
    )
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to remove dependency: {e}"))?;

    if result.rows_affected() == 0 {
        anyhow::bail!(
            "Dependency not found between '{}' and '{}'",
            task_id,
            depends_on_id
        );
    }

    Ok((
        format!(
            "Dependency removed between '{}' and '{}'",
            task_id, depends_on_id
        ),
        false,
    ))
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
// Tool: kanban_review_task (MANUAL/API only — spec §8 R12)
// ---------------------------------------------------------------------------

/// Manual/API-only review decision with the same validation as POST /review:
/// decision whitelist (approve | rework | retest | block) + R5 target
/// validation + retry guards — all enforced by
/// `omniagent::agent::manual_review_decision`.
async fn handle_review(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let task_id = args["task_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'task_id'"))?;
    let decision = args["decision"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'decision'"))?;
    let comment = args["comment"].as_str();

    validate_review_decision(decision).map_err(|e| anyhow::anyhow!("{e}"))?;

    let data_dir = std::env::var("OMNI_DIR").unwrap_or_else(|_| "/opt/omni".to_string());

    match manual_review_decision(pool, &data_dir, task_id, decision, comment).await {
        Ok(outcome) => Ok((
            serde_json::json!({
                "success": true,
                "task_id": outcome.task_id,
                "status": outcome.status,
                "thread_id": outcome.thread_id,
                "comment": outcome.comment,
            })
            .to_string(),
            false,
        )),
        Err(e) => Ok((
            serde_json::json!({ "success": false, "error": e }).to_string(),
            true,
        )),
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Shared pool — populated by configure callback before any tool call
    let pool: Arc<RwLock<Option<PgPool>>> = Arc::new(RwLock::new(None));

    let p_create = pool.clone();
    let create_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_create.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_create(&pool, &args, meta.as_ref()).await
        })
    });

    let p_list = pool.clone();
    let list_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_list.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_list(&pool, &args).await
        })
    });

    let p_update = pool.clone();
    let update_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_update.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_update(&pool, &args).await
        })
    });

    let p_delete = pool.clone();
    let delete_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_delete.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_delete(&pool, &args).await
        })
    });

    let p_add_dep = pool.clone();
    let add_dep_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_add_dep.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_add_dependency(&pool, &args).await
        })
    });

    let p_rm_dep = pool.clone();
    let rm_dep_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_rm_dep.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_remove_dependency(&pool, &args).await
        })
    });

    let p_review = pool.clone();
    let review_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_review.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_review(&pool, &args).await
        })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "create_kanban_task".to_string(),
                description:
                    "Create a new kanban task. Adds a task to the kanban board with optional body, status, priority, and assignee."
                        .to_string(),
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
                            "description": "Optional status (default: 'backlog'). One of: backlog, todo, running, testing, review, blocked, done",
                            "enum": ["backlog", "todo", "running", "testing", "review", "blocked", "done"]
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
                            "description": "Optional channel ID for thread/cause creation (default: current channel)"
                        },
                        "profile": {
                            "type": "string",
                            "description": "Optional profile name for the task (default: current profile)"
                        },
                        "template": {
                            "type": "string",
                            "description": "Optional template file name (without .md) to use for execution context"
                        },
                        "workflow_id": {
                            "type": "string",
                            "description": "Optional workflow key (e.g. exec-test-review) this task belongs to"
                        }
                    },
                    "required": ["title"]
                }),
            },
            handler: create_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "list_kanban_tasks".to_string(),
                description: "List kanban tasks grouped by status.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "description": "Optional status filter. One of: backlog, todo, running, testing, review, blocked, done"
                        }
                    }
                }),
            },
            handler: list_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "update_kanban_task".to_string(),
                description: "Update an existing kanban task. Only provided fields are updated. Status changes are recorded in history.".to_string(),
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
                            "description": "New status. One of: backlog, todo, running, testing, review, blocked, done",
                            "enum": ["backlog", "todo", "running", "testing", "review", "blocked", "done"]
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
                            "description": "New channel ID"
                        },
                        "profile": {
                            "type": "string",
                            "description": "New profile name"
                        },
                        "archived": {
                            "type": "boolean",
                            "description": "Set to true to archive, false to unarchive"
                        },
                        "workflow_id": {
                            "type": "string",
                            "description": "Set the task workflow key (e.g. exec-test-review)"
                        }
                    },
                    "required": ["id"]
                }),
            },
            handler: update_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "delete_kanban_task".to_string(),
                description: "Delete a kanban task. The deletion is recorded in history.".to_string(),
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
            },
            handler: delete_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "add_kanban_dependency".to_string(),
                description: "Add a dependency between two kanban tasks.".to_string(),
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
            },
            handler: add_dep_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "remove_kanban_dependency".to_string(),
                description: "Remove a dependency between two kanban tasks.".to_string(),
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
            },
            handler: rm_dep_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "kanban_review_task".to_string(),
                description:
                    "MANUAL/API-only review decision for a kanban task. Decision: approve (task done), rework (back to running with a new executor thread), retest (back to testing with a new tester thread), block (task blocked). Invalid decisions and invalid targets (e.g. retest without a tester role in the workflow) are rejected. The reviewer AGENT never calls this tool — it signals approve via normal completion and issues via fail-thread."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Task ID to review"
                        },
                        "decision": {
                            "type": "string",
                            "description": "Review decision. One of: approve, rework, retest, block",
                            "enum": ["approve", "rework", "retest", "block"]
                        },
                        "comment": {
                            "type": "string",
                            "description": "Optional comment recorded in the task history"
                        }
                    },
                    "required": ["task_id", "decision"]
                }),
            },
            handler: review_handler,
        },
    ];

    // Start the MCP server
    let server_info = ServerInfo {
        name: "mcp-server-kanban".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server_with_config(server_info, tools, {
        let p = pool.clone();
        Some(move |params: serde_json::Value| {
            let config = PluginConfig::from_json(&params);
            tokio::task::block_in_place(|| {
                let rt = tokio::runtime::Handle::current();
                let new_pool = rt
                    .block_on(db::connect(&config.database_url))
                    .expect("Failed to connect to database");
                *p.blocking_write() = Some(new_pool);
            });
            tracing::info!("Kanban plugin configured with database_url");
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_decision_whitelist_matches_server() {
        // Same whitelist as POST /review (spec §8 R12).
        for d in ["approve", "rework", "retest", "block"] {
            assert!(validate_review_decision(d).is_ok(), "'{d}' should be valid");
        }
        for d in ["", "approved", "reject", "REWORK", "done"] {
            assert!(
                validate_review_decision(d).is_err(),
                "'{d}' should be invalid"
            );
        }
    }
}
