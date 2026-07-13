//! Kanban API — board view, task CRUD, dependencies, threads, history, subtasks.
//!
//! Replaces ALL SQL in `omni-dashboard/repo/server/routes/kanban.ts` (13 endpoints,
//! ~40 SQL queries). Every query uses `sql_forge!()` — no raw `sqlx::query` calls.
//!
//! Routes (all under `/kanban`):
//!
//!  - GET   /kanban/tasks                           — board tasks (flat list)
//!  - GET   /kanban/tasks/{id}                      — task detail
//!  - GET   /kanban/tasks/{id}/dependencies         — task dependencies
//!  - POST  /kanban/tasks                           — create task
//!  - PATCH /kanban/tasks/{id}/status               — change status (+ position shift)
//!  - PATCH /kanban/tasks/{id}/position             — change position (+ cross-column)
//!  - PATCH /kanban/tasks/{id}                      — update task fields
//!  - DELETE /kanban/tasks/{id}                     — delete task
//!  - GET   /kanban/tasks/{id}/threads              — threads for a task
//!  - POST  /kanban/tasks/{id}/dependencies         — add dependency
//!  - DELETE /kanban/tasks/{id}/dependencies/{depId}— remove dependency
//!  - GET   /kanban/tasks/{id}/history              — history log
//!  - GET   /kanban/tasks/{id}/subtasks             — subtasks

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use tracing::error;

use super::{err_json, ok_json, AppState};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const VALID_STATUSES: &[&str] = &[
    "backlog", "todo", "ready", "running", "review", "blocked", "done",
];

/// Sentinel used for optional integer fields (channel_id, priority) to
/// signal "keep existing value" inside a static UPDATE statement.
const IGNORE_INT: i64 = -999_999;

/// Sentinel used for optional text fields where empty string is a valid value
/// (body) so we can distinguish "not provided" from "explicitly empty".
const IGNORE_STR: &str = "\x00__NO_UPDATE__\x00";

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn kanban_router() -> Router<Arc<AppState>> {
    Router::new()
        // 1. Board / list
        .route("/kanban/tasks", get(list_tasks_handler))
        // 2. Task detail
        .route("/kanban/tasks/{id}", get(get_task_handler))
        // 3. Dependencies list
        .route(
            "/kanban/tasks/{id}/dependencies",
            get(list_dependencies_handler),
        )
        // 4. Create task
        .route("/kanban/tasks", post(create_task_handler))
        // 5. Change status
        .route("/kanban/tasks/{id}/status", patch(change_status_handler))
        // 6. Change position
        .route(
            "/kanban/tasks/{id}/position",
            patch(change_position_handler),
        )
        // 7. Update task fields
        .route("/kanban/tasks/{id}", patch(update_task_handler))
        // 8. Delete task
        .route("/kanban/tasks/{id}", delete(delete_task_handler))
        // 9. Threads
        .route("/kanban/tasks/{id}/threads", get(list_threads_handler))
        // 10. Add dependency
        .route(
            "/kanban/tasks/{id}/dependencies",
            post(add_dependency_handler),
        )
        // 11. Remove dependency
        .route(
            "/kanban/tasks/{id}/dependencies/{depId}",
            delete(remove_dependency_handler),
        )
        // 12. History
        .route("/kanban/tasks/{id}/history", get(list_history_handler))
        // 13. Subtasks
        .route("/kanban/tasks/{id}/subtasks", get(list_subtasks_handler))
}

// ---------------------------------------------------------------------------
// Query string types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListTasksQuery {
    show_archived: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ThreadsQuery {
    offset: Option<i64>,
    limit: Option<i64>,
    order: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    action: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

// ---------------------------------------------------------------------------
// Request body types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateTaskRequest {
    title: String,
    body: Option<String>,
    channel_id: Option<i64>,
    profile: Option<String>,
    priority: Option<i32>,
    status: Option<String>,
    template: Option<String>,
    plan: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ChangeStatusRequest {
    status: String,
    position: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct ChangePositionRequest {
    status: Option<String>,
    position: i32,
}

#[derive(Debug, Deserialize)]
struct UpdateTaskRequest {
    title: Option<String>,
    body: Option<String>,
    channel_id: Option<i64>,
    profile: Option<String>,
    priority: Option<i32>,
    status: Option<String>,
    archived: Option<bool>,
    template: Option<String>,
    plan: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct AddDependencyRequest {
    depends_on_id: String,
}

// ---------------------------------------------------------------------------
// Row types (sqlx::FromRow for sql_forge!)
// ---------------------------------------------------------------------------

#[derive(FromRow)]
struct KanbanTaskRow {
    id: String,
    title: String,
    body: Option<String>,
    status: String,
    priority: Option<i32>,
    position: Option<i32>,
    assignee: Option<String>,
    channel_id: Option<i64>,
    profile: Option<String>,
    archived: Option<bool>,
    template: Option<String>,
    plan: Option<bool>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(FromRow)]
struct DeleteReturningIdRow {
    id: String,
}

#[derive(FromRow)]
struct DeleteIdRow {
    id: String,
    title: Option<String>,
    body: Option<String>,
    status: Option<String>,
    priority: Option<i32>,
    position: Option<i32>,
    assignee: Option<String>,
    channel_id: Option<i64>,
    profile: Option<String>,
    archived: Option<bool>,
    template: Option<String>,
    plan: Option<bool>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(FromRow)]
struct PosRow {
    next_pos: Option<i32>,
}

#[derive(FromRow)]
struct CountRow {
    total: Option<i64>,
}

#[derive(FromRow)]
struct DependencyRow {
    id: String,
    title: String,
    status: String,
    priority: Option<i32>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(FromRow)]
struct KanbanThreadRow {
    id: i64,
    thread_id: i64,
    role: Option<String>,
    content: Option<String>,
    msg_type: Option<String>,
    msg_subtype: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    processing_time_ms: Option<i32>,
    token_usage: Option<serde_json::Value>,
    iteration_number: Option<i32>,
    thread_sequence: Option<i32>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    metadata: Option<serde_json::Value>,
    thread_status: Option<String>,
    channel_name: Option<String>,
}

#[derive(FromRow)]
struct HistoryRow {
    id: i64,
    kanban_task_id: String,
    action: String,
    initial_board: Option<String>,
    final_board: Option<String>,
    previous_values: Option<serde_json::Value>,
    created_at: Option<String>,
}

#[derive(FromRow)]
struct SubtaskRow {
    id: i64,
    description: String,
    status: Option<String>,
    priority: Option<i32>,
    thread_id: i64,
    thread_title: Option<String>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(FromRow)]
struct DepCheckRow {
    task_id: String,
}

// ---------------------------------------------------------------------------
// Response entry types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct KanbanTaskEntry {
    id: String,
    title: String,
    body: Option<String>,
    status: String,
    priority: i32,
    position: i32,
    assignee: Option<String>,
    channel_id: Option<i64>,
    profile: Option<String>,
    archived: bool,
    template: Option<String>,
    plan: Option<bool>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Serialize)]
struct DependencyEntry {
    id: String,
    title: String,
    status: String,
    priority: i32,
    created_at: Option<String>,
}

#[derive(Serialize)]
struct KanbanThreadEntry {
    id: i64,
    thread_id: i64,
    role: Option<String>,
    content: Option<String>,
    msg_type: Option<String>,
    msg_subtype: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    processing_time_ms: Option<i64>,
    token_usage: Option<serde_json::Value>,
    iteration_number: Option<i32>,
    thread_sequence: Option<i32>,
    created_at: Option<String>,
    metadata: Option<serde_json::Value>,
    thread_status: Option<String>,
    channel_name: Option<String>,
}

#[derive(Serialize)]
struct ThreadsResponse {
    rows: Vec<KanbanThreadEntry>,
    total: i64,
}

#[derive(Serialize)]
struct HistoryEntry {
    id: i64,
    kanban_task_id: String,
    action: String,
    initial_board: Option<String>,
    final_board: Option<String>,
    previous_values: Option<serde_json::Value>,
    created_at: Option<String>,
}

#[derive(Serialize)]
struct SubtaskEntry {
    id: i64,
    description: String,
    status: Option<String>,
    priority: i32,
    thread_id: i64,
    thread_title: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Serialize)]
struct CreateTaskResponse {
    success: bool,
    id: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn task_row_to_entry(r: KanbanTaskRow) -> KanbanTaskEntry {
    KanbanTaskEntry {
        id: r.id,
        title: r.title,
        body: r.body,
        status: r.status,
        priority: r.priority.unwrap_or(0),
        position: r.position.unwrap_or(0),
        assignee: r.assignee,
        channel_id: r.channel_id,
        profile: r.profile,
        archived: r.archived.unwrap_or(false),
        template: r.template,
        plan: r.plan,
        created_at: r
            .created_at
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        updated_at: r
            .updated_at
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    }
}

fn dep_row_to_entry(r: DependencyRow) -> DependencyEntry {
    DependencyEntry {
        id: r.id,
        title: r.title,
        status: r.status,
        priority: r.priority.unwrap_or(0),
        created_at: r
            .created_at
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    }
}

fn history_row_to_entry(r: HistoryRow) -> HistoryEntry {
    HistoryEntry {
        id: r.id,
        kanban_task_id: r.kanban_task_id,
        action: r.action,
        initial_board: r.initial_board,
        final_board: r.final_board,
        previous_values: r.previous_values,
        created_at: r.created_at,
    }
}

fn subtask_row_to_entry(r: SubtaskRow) -> SubtaskEntry {
    SubtaskEntry {
        id: r.id,
        description: r.description,
        status: r.status,
        priority: r.priority.unwrap_or(0),
        thread_id: r.thread_id,
        thread_title: Some(r.thread_title.unwrap_or_default()),
        created_at: r
            .created_at
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        updated_at: r
            .updated_at
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    }
}

/// Validate that a status string is one of the known kanban columns.
fn validate_status(status: &str) -> bool {
    VALID_STATUSES.contains(&status)
}

/// Get the next available position for a given status column.
async fn next_position(pool: &sqlx::PgPool, status: &str) -> Result<i32, sqlx::Error> {
    let row: PosRow = sql_forge!(
        PosRow,
        r#"
        SELECT COALESCE(MAX(position), -1) + 1 AS next_pos
        FROM kanban_tasks
        WHERE status = :status
        "#,
        ( :status = status )
    )
    .fetch_one(pool)
    .await?;
    Ok(row.next_pos.unwrap_or(0))
}

// ---------------------------------------------------------------------------
// 1. GET /kanban/tasks — List all board tasks
// ---------------------------------------------------------------------------

async fn list_tasks_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListTasksQuery>,
) -> impl IntoResponse {
    let show_archived_bool = params
        .show_archived
        .as_deref()
        .unwrap_or("")
        .parse::<bool>()
        .unwrap_or(false);

    let rows = match sql_forge!(
        KanbanTaskRow,
        r#"
        SELECT
            id, title, body, status, priority, position, assignee,
            channel_id, profile, archived, template, plan,
            created_at, updated_at
        FROM kanban_tasks
        WHERE (:show_archived_bool OR archived = false)
           OR (NOT :show_archived_bool AND archived = false)
        ORDER BY position ASC, created_at DESC
        "#,
        ( :show_archived_bool = show_archived_bool )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[kanban/tasks] list query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch kanban tasks",
            );
        }
    };

    let entries: Vec<KanbanTaskEntry> = rows.into_iter().map(task_row_to_entry).collect();
    ok_json(entries)
}

// ---------------------------------------------------------------------------
// 2. GET /kanban/tasks/{id} — Task detail
// ---------------------------------------------------------------------------

async fn get_task_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let row = match sql_forge!(
        KanbanTaskRow,
        r#"
        SELECT
            id, title, body, status, priority, position, assignee,
            channel_id, profile, archived, template, plan,
            created_at, updated_at
        FROM kanban_tasks
        WHERE id = :id
        "#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(r)) => r,
        Ok(None) => {
            return err_json(StatusCode::NOT_FOUND, "Task not found");
        }
        Err(e) => {
            error!("[kanban/tasks/{}] get query failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch task");
        }
    };

    ok_json(task_row_to_entry(row))
}

// ---------------------------------------------------------------------------
// 3. GET /kanban/tasks/{id}/dependencies — Task dependencies
// ---------------------------------------------------------------------------

async fn list_dependencies_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let rows = match sql_forge!(
        DependencyRow,
        r#"
        SELECT
            d.depends_on_id AS id,
            t.title,
            t.status,
            t.priority,
            d.created_at
        FROM kanban_task_dependencies d
        JOIN kanban_tasks t ON t.id = d.depends_on_id
        WHERE d.task_id = :task_id
        ORDER BY t.priority ASC, t.created_at DESC
        "#,
        ( :task_id = &id )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[kanban/tasks/{}/dependencies] query failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch dependencies",
            );
        }
    };

    let entries: Vec<DependencyEntry> = rows.into_iter().map(dep_row_to_entry).collect();
    ok_json(entries)
}

// ---------------------------------------------------------------------------
// 4. POST /kanban/tasks — Create a new task
// ---------------------------------------------------------------------------

async fn create_task_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateTaskRequest>,
) -> impl IntoResponse {
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "Title is required");
    }

    let id = format!(
        "task_{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    let task_status = body
        .status
        .as_deref()
        .filter(|s| validate_status(s))
        .unwrap_or("backlog")
        .to_string();

    let task_priority = body.priority.unwrap_or(0);

    // Get max position for this status
    let next_pos = match next_position(&state.pool, &task_status).await {
        Ok(pos) => pos,
        Err(e) => {
            error!("[kanban/tasks] next_position query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to compute task position",
            );
        }
    };

    // Insert the task
    if let Err(e) = sql_forge!(
        r#"
        INSERT INTO kanban_tasks
            (id, title, body, status, priority, channel_id, profile, position, template, plan)
        VALUES
            (:id, :title, :body, :status, :priority, NULLIF(:channel_id, 0::bigint), NULLIF(:profile, '')::text,
             :position, NULLIF(:template, '')::text, :plan::boolean)
        "#,
        ( :id = id.as_str(),
          :title = &title,
          :body = body.body.as_deref().unwrap_or(""),
          :status = &task_status,
          :priority = task_priority,
          :channel_id = body.channel_id.unwrap_or(0),
          :profile = body.profile.as_deref().unwrap_or(""),
          :position = next_pos,
          :template = body.template.as_deref().unwrap_or(""),
          :plan = body.plan.unwrap_or(false), )
    )
    .execute(&state.pool)
    .await
    {
        error!("[kanban/tasks] insert failed: {:?}", e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create task",
        );
    }

    // Insert creation history (best-effort)
    if let Err(e) = sql_forge!(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values)
        VALUES (:task_id, 'created', NULL, :final_board::text, NULL)
        "#,
        ( :task_id = &id, :final_board = &task_status )
    )
    .execute(&state.pool)
    .await
    {
        error!("[kanban/tasks] history insert for create failed: {:?}", e);
        // Non-fatal — task was already created
    }

    ok_json(CreateTaskResponse { success: true, id })
}

// ---------------------------------------------------------------------------
// 5. PATCH /kanban/tasks/{id}/status — Change task status (move column)
// ---------------------------------------------------------------------------

async fn change_status_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ChangeStatusRequest>,
) -> impl IntoResponse {
    if !validate_status(&body.status) {
        return err_json(
            StatusCode::BAD_REQUEST,
            &format!("Status must be one of: {}", VALID_STATUSES.join(", ")),
        );
    }

    // 1. Check task exists and get current status + position
    let task = match sql_forge!(
        DeleteIdRow,
        r#"
        SELECT id, title, body, status, priority, position, assignee,
               channel_id, profile, archived, template, plan,
               created_at, updated_at
        FROM kanban_tasks WHERE id = :id
        "#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(r)) => r,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "Task not found"),
        Err(e) => {
            error!("[kanban/tasks/{}/status] check query failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to check task");
        }
    };

    let old_status = task.status.as_deref().unwrap_or("backlog");
    let old_position = task.position.unwrap_or(0);

    // 2. Determine target position
    let target_position = match body.position {
        Some(pos) => pos,
        None => match next_position(&state.pool, &body.status).await {
            Ok(pos) => pos,
            Err(e) => {
                error!("[kanban/tasks/{}/status] next_position failed: {:?}", id, e);
                return err_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to compute position",
                );
            }
        },
    };

    if old_status == body.status && old_position == target_position {
        // No-op — already there
        return ok_json(serde_json::json!({ "success": true }));
    }

    // 3. Shift positions
    if old_status != body.status {
        // Cross-column move
        // Fill gap in old column
        if let Err(e) = sql_forge!(
            r#"UPDATE kanban_tasks SET position = position - 1 WHERE status = :status AND position > :old_pos"#,
            ( :status = old_status, :old_pos = old_position )
        )
        .execute(&state.pool)
        .await
        {
            error!("[kanban/tasks/{}/status] gap-fill failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to shift positions");
        }
        // Make room in new column
        if let Err(e) = sql_forge!(
            r#"UPDATE kanban_tasks SET position = position + 1 WHERE status = :status AND position >= :target AND id != :task_id"#,
            ( :status = &body.status, :target = target_position, :task_id = &id )
        )
        .execute(&state.pool)
        .await
        {
            error!("[kanban/tasks/{}/status] make-room failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to shift positions");
        }
    } else {
        // Reorder within the same column
        if target_position > old_position {
            // Moving down — shift intermediate tasks up
            if let Err(e) = sql_forge!(
                r#"UPDATE kanban_tasks SET position = position - 1 WHERE status = :status AND position > :old_pos AND position <= :target AND id != :task_id"#,
                ( :status = &body.status, :old_pos = old_position, :target = target_position, :task_id = &id )
            )
            .execute(&state.pool)
            .await
            {
                error!("[kanban/tasks/{}/status] reorder-down failed: {:?}", id, e);
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to reorder");
            }
        } else if target_position < old_position {
            // Moving up — shift intermediate tasks down
            if let Err(e) = sql_forge!(
                r#"UPDATE kanban_tasks SET position = position + 1 WHERE status = :status AND position >= :target AND position < :old_pos AND id != :task_id"#,
                ( :status = &body.status, :target = target_position, :old_pos = old_position, :task_id = &id )
            )
            .execute(&state.pool)
            .await
            {
                error!("[kanban/tasks/{}/status] reorder-up failed: {:?}", id, e);
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to reorder");
            }
        }
    }

    // 4. Set new status + position
    if let Err(e) = sql_forge!(
        r#"UPDATE kanban_tasks SET status = :status, position = :position, updated_at = NOW() WHERE id = :id"#,
        ( :status = &body.status, :position = target_position, :id = &id )
    )
    .execute(&state.pool)
    .await
    {
        error!("[kanban/tasks/{}/status] final update failed: {:?}", id, e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update task status",
        );
    }

    // 5. History — only if status actually changed
    if old_status != body.status {
        if let Err(e) = sql_forge!(
            r#"
            INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values)
            VALUES (:task_id, 'moved', :initial_board::text, :final_board::text, NULL)
            "#,
            ( :task_id = &id, :initial_board = old_status, :final_board = &body.status )
        )
        .execute(&state.pool)
        .await
        {
            error!("[kanban/tasks/{}/status] history insert failed: {:?}", id, e);
            // Non-fatal
        }
    }

    ok_json(serde_json::json!({ "success": true }))
}

// ---------------------------------------------------------------------------
// 6. PATCH /kanban/tasks/{id}/position — Change task position
// ---------------------------------------------------------------------------

async fn change_position_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ChangePositionRequest>,
) -> impl IntoResponse {
    // 1. Check task exists and get current status + position
    let task = match sql_forge!(
        DeleteIdRow,
        r#"
        SELECT id, title, body, status, priority, position, assignee,
               channel_id, profile, archived, template, plan,
               created_at, updated_at
        FROM kanban_tasks WHERE id = :id
        "#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(r)) => r,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "Task not found"),
        Err(e) => {
            error!("[kanban/tasks/{}/position] check query failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to check task");
        }
    };

    let old_status = task.status.as_deref().unwrap_or("backlog");
    let old_position = task.position.unwrap_or(0);

    let new_status = body.status.as_deref().unwrap_or(old_status);

    if let Some(ref s) = body.status {
        if !validate_status(s) {
            return err_json(
                StatusCode::BAD_REQUEST,
                &format!("Status must be one of: {}", VALID_STATUSES.join(", ")),
            );
        }
    }

    if old_status == new_status && old_position == body.position {
        // No-op
        return ok_json(serde_json::json!({ "success": true }));
    }

    // 2. Shift positions
    if old_status != new_status {
        // Cross-column move
        // Fill gap in old column
        if let Err(e) = sql_forge!(
            r#"UPDATE kanban_tasks SET position = position - 1 WHERE status = :status AND position > :old_pos"#,
            ( :status = old_status, :old_pos = old_position )
        )
        .execute(&state.pool)
        .await
        {
            error!("[kanban/tasks/{}/position] gap-fill failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to shift positions");
        }
        // Make room in new column
        if let Err(e) = sql_forge!(
            r#"UPDATE kanban_tasks SET position = position + 1 WHERE status = :status AND position >= :target AND id != :task_id"#,
            ( :status = new_status, :target = body.position, :task_id = &id )
        )
        .execute(&state.pool)
        .await
        {
            error!("[kanban/tasks/{}/position] make-room failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to shift positions");
        }
    } else {
        // Reorder within same column
        if body.position > old_position {
            // Moving down — shift intermediate tasks up
            if let Err(e) = sql_forge!(
                r#"UPDATE kanban_tasks SET position = position - 1 WHERE status = :status AND position > :old_pos AND position <= :target AND id != :task_id"#,
                ( :status = new_status, :old_pos = old_position, :target = body.position, :task_id = &id )
            )
            .execute(&state.pool)
            .await
            {
                error!("[kanban/tasks/{}/position] reorder-down failed: {:?}", id, e);
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to reorder");
            }
        } else if body.position < old_position {
            // Moving up — shift intermediate tasks down
            if let Err(e) = sql_forge!(
                r#"UPDATE kanban_tasks SET position = position + 1 WHERE status = :status AND position >= :target AND position < :old_pos AND id != :task_id"#,
                ( :status = new_status, :target = body.position, :old_pos = old_position, :task_id = &id )
            )
            .execute(&state.pool)
            .await
            {
                error!("[kanban/tasks/{}/position] reorder-up failed: {:?}", id, e);
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to reorder");
            }
        }
    }

    // 3. Set new status + position
    if let Err(e) = sql_forge!(
        r#"UPDATE kanban_tasks SET status = :status, position = :position, updated_at = NOW() WHERE id = :id"#,
        ( :status = new_status, :position = body.position, :id = &id )
    )
    .execute(&state.pool)
    .await
    {
        error!("[kanban/tasks/{}/position] final update failed: {:?}", id, e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update task position",
        );
    }

    // 4. History — only if status changed
    if old_status != new_status {
        if let Err(e) = sql_forge!(
            r#"
            INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values)
            VALUES (:task_id, 'moved', :initial_board::text, :final_board::text, NULL)
            "#,
            ( :task_id = &id, :initial_board = old_status, :final_board = new_status )
        )
        .execute(&state.pool)
        .await
        {
            error!("[kanban/tasks/{}/position] history insert failed: {:?}", id, e);
            // Non-fatal
        }
    }

    ok_json(serde_json::json!({ "success": true }))
}

// ---------------------------------------------------------------------------
// 7. PATCH /kanban/tasks/{id} — Update task fields
// ---------------------------------------------------------------------------

async fn update_task_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateTaskRequest>,
) -> impl IntoResponse {
    // 1. Check task exists and fetch current values
    let before = match sql_forge!(
        DeleteIdRow,
        r#"
        SELECT id, title, body, status, priority, position, assignee,
               channel_id, profile, archived, template, plan,
               created_at, updated_at
        FROM kanban_tasks WHERE id = :id
        "#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(r)) => r,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "Task not found"),
        Err(e) => {
            error!("[kanban/tasks/{}] check query failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to check task");
        }
    };

    // 2. Validate title if provided
    if let Some(ref title) = body.title {
        if title.trim().is_empty() {
            return err_json(StatusCode::BAD_REQUEST, "Title cannot be empty");
        }
    }

    // 3. Validate status if provided
    if let Some(ref status) = body.status {
        if !validate_status(status) {
            return err_json(
                StatusCode::BAD_REQUEST,
                &format!("Status must be one of: {}", VALID_STATUSES.join(", ")),
            );
        }
    }

    // 4. Ensure at least one field was provided
    let has_fields = body.title.is_some()
        || body.body.is_some()
        || body.channel_id.is_some()
        || body.profile.is_some()
        || body.priority.is_some()
        || body.status.is_some()
        || body.archived.is_some()
        || body.template.is_some()
        || body.plan.is_some();

    if !has_fields {
        return err_json(StatusCode::BAD_REQUEST, "No fields to update");
    }

    // 5. Execute the update — use static SQL with sentinel/COALESCE pattern
    //    so that fields not provided keep their existing values.
    if let Err(e) = sql_forge!(
        r#"
        UPDATE kanban_tasks SET
            title = CASE WHEN :title = '' THEN title ELSE NULLIF(:title, '')::text END,
            body = CASE WHEN :body = :ign_str THEN body ELSE :body END,
            channel_id = CASE WHEN :channel_id = -999999::bigint THEN channel_id ELSE :channel_id END,
            profile = CASE WHEN :profile = '' THEN profile ELSE NULLIF(:profile, '')::text END,
            priority = CASE WHEN :priority = -999999::bigint THEN priority::bigint ELSE :priority END,
            status = CASE WHEN :status = '' THEN status ELSE :status END,
            archived = :archived,
            template = CASE WHEN :template = '' THEN template ELSE NULLIF(:template, '')::text END,
            plan = :plan,
            updated_at = NOW()
        WHERE id = :id
        "#,
        ( :id = id.as_str(),
          :title = body.title.as_deref().unwrap_or(""),
          :body = body.body.as_deref().unwrap_or(IGNORE_STR),
          :ign_str = IGNORE_STR,
          :channel_id = body.channel_id.unwrap_or(IGNORE_INT),
          :profile = body.profile.as_deref().unwrap_or(""),
          :priority = body.priority.map(|v| v as i64).unwrap_or(IGNORE_INT),
          :status = body.status.as_deref().unwrap_or(""),
          :archived = body.archived.unwrap_or(before.archived.unwrap_or(false)),
          :template = body.template.as_deref().unwrap_or(""),
          :plan = body.plan.or(before.plan).unwrap_or(false), )
    )
    .execute(&state.pool)
    .await
    {
        error!("[kanban/tasks/{}] update failed: {:?}", id, e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update task",
        );
    }

    // 6. Insert kanban history
    let has_status_change = body
        .status
        .as_ref()
        .map(|s| s.as_str() != before.status.as_deref().unwrap_or(""))
        .unwrap_or(false);
    let has_archive_change = body
        .archived
        .map(|a| Some(a) != before.archived)
        .unwrap_or(false);

    if has_archive_change {
        // Archived / unarchived
        let action = if body.archived == Some(true) {
            "archived"
        } else {
            "unarchived"
        };
        if let Err(e) = sql_forge!(
            r#"
            INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values)
            VALUES (:task_id, :action, NULL, NULL, NULL)
            "#,
            ( :task_id = &id, :action = action )
        )
        .execute(&state.pool)
        .await
        {
            error!("[kanban/tasks/{}] archive history insert failed: {:?}", id, e);
        }
    } else if has_status_change {
        // Status move
        let old_s = before.status.as_deref().unwrap_or("");
        let new_s = body.status.as_deref().unwrap_or("");
        if let Err(e) = sql_forge!(
            r#"
            INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values)
            VALUES (:task_id, 'moved', :initial_board::text, :final_board::text, NULL)
            "#,
            ( :task_id = &id, :initial_board = old_s, :final_board = new_s )
        )
        .execute(&state.pool)
        .await
        {
            error!("[kanban/tasks/{}] status history insert failed: {:?}", id, e);
        }
    } else {
        // Field edit — log with full previous values
        let prev = serde_json::json!({
            "title": before.title,
            "body": before.body,
            "status": before.status,
            "priority": before.priority,
            "channel_id": before.channel_id,
            "profile": before.profile,
            "template": before.template,
            "plan": before.plan,
            "archived": before.archived,
            "assignee": before.assignee,
        });
        if let Err(e) = sql_forge!(
            r#"
            INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values)
            VALUES (:task_id, 'edited', NULL, NULL, :previous_values::jsonb)
            "#,
            ( :task_id = &id, :previous_values = &prev )
        )
        .execute(&state.pool)
        .await
        {
            error!("[kanban/tasks/{}] edit history insert failed: {:?}", id, e);
        }
    }

    ok_json(serde_json::json!({ "success": true }))
}

// ---------------------------------------------------------------------------
// 8. DELETE /kanban/tasks/{id} — Delete task
// ---------------------------------------------------------------------------

async fn delete_task_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // 1. Fetch task for history
    let before = match sql_forge!(
        DeleteIdRow,
        r#"
        SELECT id, title, body, status, priority, position, assignee,
               channel_id, profile, archived, template, plan,
               created_at, updated_at
        FROM kanban_tasks WHERE id = :id
        "#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(r)) => r,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "Task not found"),
        Err(e) => {
            error!("[kanban/tasks/{}] check query failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to check task");
        }
    };

    // 2. Insert history with full previous values
    let prev = serde_json::json!({
        "title": before.title,
        "body": before.body,
        "status": before.status,
        "priority": before.priority,
        "channel_id": before.channel_id,
        "profile": before.profile,
        "template": before.template,
        "plan": before.plan,
        "archived": before.archived,
        "assignee": before.assignee,
    });

    if let Err(e) = sql_forge!(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values)
        VALUES (:task_id, 'deleted', NULL, NULL, :previous_values::jsonb)
        "#,
        ( :task_id = &id, :previous_values = &prev )
    )
    .execute(&state.pool)
    .await
    {
        error!("[kanban/tasks/{}] history insert failed: {:?}", id, e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to record history",
        );
    }

    // 3. Clear dependencies (both directions)
    if let Err(e) = sql_forge!(
        r#"DELETE FROM kanban_task_dependencies WHERE task_id = :id OR depends_on_id = :id"#,
        ( :id = &id )
    )
    .execute(&state.pool)
    .await
    {
        error!("[kanban/tasks/{}] dependency delete failed: {:?}", id, e);
        // Non-fatal — the ON DELETE CASCADE will handle it
    }

    // 4. Detach threads
    if let Err(e) = sql_forge!(
        r#"UPDATE threads SET task_id = NULL WHERE task_id = :id"#,
        ( :id = &id )
    )
    .execute(&state.pool)
    .await
    {
        error!("[kanban/tasks/{}] thread detach failed: {:?}", id, e);
        // Non-fatal
    }

    // 5. Delete the task
    let deleted = match sql_forge!(
        DeleteReturningIdRow,
        r#"DELETE FROM kanban_tasks WHERE id = :id RETURNING id"#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            error!("[kanban/tasks/{}] delete failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to delete task");
        }
    };

    if !deleted {
        return err_json(StatusCode::NOT_FOUND, "Task not found");
    }

    ok_json(serde_json::json!({ "success": true }))
}

// ---------------------------------------------------------------------------
// 9. GET /kanban/tasks/{id}/threads — Threads for a task
// ---------------------------------------------------------------------------

async fn list_threads_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<ThreadsQuery>,
) -> impl IntoResponse {
    let offset = params.offset.unwrap_or(0).max(0);
    let limit = params.limit.unwrap_or(10).clamp(1, 100);
    let order = match params.order.as_deref() {
        Some("asc") => "ASC",
        _ => "DESC",
    };

    // Total count
    let total = match sql_forge!(
        CountRow,
        r#"SELECT COUNT(*) AS total FROM threads WHERE task_id = :task_id"#,
        ( :task_id = &id )
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => row.total.unwrap_or(0),
        Err(e) => {
            error!("[kanban/tasks/{}/threads] count query failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to count threads");
        }
    };

    // Fetch paginated rows — use CASE in ORDER BY for dynamic direction
    let order_asc = order == "ASC";
    let rows = match sql_forge!(
        KanbanThreadRow,
        r#"
        SELECT
            m.id,
            t.id AS thread_id,
            m.role,
            m.content,
            m.msg_type AS msg_type,
            m.msg_subtype AS msg_subtype,
            t.provider,
            t.model,
            t.duration_ms AS processing_time_ms,
            jsonb_build_object(
                'input_tokens', t.input_tokens,
                'output_tokens', t.output_tokens,
                'cached_tokens', t.cached_tokens
            ) AS token_usage,
            m.iteration_number,
            m.thread_sequence,
            m.created_at,
            m.metadata,
            t.status AS thread_status,
            c.name AS channel_name
        FROM threads t
        LEFT JOIN channels c ON c.id = t.channel_id
        LEFT JOIN LATERAL (
            SELECT m_sub.*
            FROM messages m_sub
            WHERE m_sub.thread_id = t.id
            ORDER BY m_sub.id DESC
            LIMIT 1
        ) m ON true
        WHERE t.task_id = :task_id
        ORDER BY
            CASE WHEN :order_asc THEN m.created_at END ASC NULLS LAST,
            CASE WHEN NOT :order_asc THEN m.created_at END DESC NULLS LAST
        LIMIT :limit_val OFFSET :offset_val
        "#,
        ( :task_id = &id,
          :order_asc = order_asc,
          :limit_val = limit,
          :offset_val = offset )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[kanban/tasks/{}/threads] data query failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch threads");
        }
    };

    let entries: Vec<KanbanThreadEntry> = rows
        .into_iter()
        .map(|r| KanbanThreadEntry {
            id: r.id,
            thread_id: r.thread_id,
            role: r.role,
            content: r.content,
            msg_type: r.msg_type,
            msg_subtype: r.msg_subtype,
            provider: r.provider,
            model: r.model,
            processing_time_ms: r.processing_time_ms.map(|v| v as i64),
            token_usage: r.token_usage,
            iteration_number: r.iteration_number,
            thread_sequence: r.thread_sequence,
            created_at: r
                .created_at
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            metadata: r.metadata,
            thread_status: r.thread_status,
            channel_name: r.channel_name,
        })
        .collect();

    ok_json(ThreadsResponse {
        rows: entries,
        total,
    })
}

// ---------------------------------------------------------------------------
// 10. POST /kanban/tasks/{id}/dependencies — Add a dependency
// ---------------------------------------------------------------------------

async fn add_dependency_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<AddDependencyRequest>,
) -> impl IntoResponse {
    let task_id = &id;
    let depends_on_id = &body.depends_on_id;

    // Validate: cannot depend on itself
    if task_id == depends_on_id {
        return err_json(StatusCode::BAD_REQUEST, "A task cannot depend on itself");
    }

    // 1. Check that the dependency target exists
    let dep_exists = match sql_forge!(
        DepCheckRow,
        r#"SELECT id AS task_id FROM kanban_tasks WHERE id = :id"#,
        ( :id = depends_on_id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            error!(
                "[kanban/tasks/{}/dependencies] check target failed: {:?}",
                task_id, e
            );
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to check dependency target",
            );
        }
    };

    if !dep_exists {
        return err_json(
            StatusCode::NOT_FOUND,
            &format!("Dependency task '{}' not found", depends_on_id),
        );
    }

    // 2. Check for circular dependency
    let circular = match sql_forge!(
        DepCheckRow,
        r#"
        SELECT task_id FROM kanban_task_dependencies
        WHERE task_id = :depends_on_id AND depends_on_id = :task_id
        "#,
        ( :depends_on_id = depends_on_id, :task_id = task_id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            error!(
                "[kanban/tasks/{}/dependencies] circular check failed: {:?}",
                task_id, e
            );
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to check circular dependencies",
            );
        }
    };

    if circular {
        return err_json(StatusCode::BAD_REQUEST, "Circular dependency detected");
    }

    // 3. Check for duplicate
    let duplicate = match sql_forge!(
        DepCheckRow,
        r#"
        SELECT task_id FROM kanban_task_dependencies
        WHERE task_id = :task_id AND depends_on_id = :depends_on_id
        "#,
        ( :task_id = task_id, :depends_on_id = depends_on_id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            error!(
                "[kanban/tasks/{}/dependencies] duplicate check failed: {:?}",
                task_id, e
            );
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to check duplicate dependencies",
            );
        }
    };

    if duplicate {
        return err_json(
            StatusCode::BAD_REQUEST,
            &format!(
                "Duplicate dependency: task '{}' already depends on '{}'",
                task_id, depends_on_id
            ),
        );
    }

    // 4. Insert the dependency
    if let Err(e) = sql_forge!(
        r#"INSERT INTO kanban_task_dependencies (task_id, depends_on_id) VALUES (:task_id, :depends_on_id)"#,
        ( :task_id = task_id, :depends_on_id = depends_on_id )
    )
    .execute(&state.pool)
    .await
    {
        error!(
            "[kanban/tasks/{}/dependencies] insert failed: {:?}",
            task_id, e
        );
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to add dependency",
        );
    }

    ok_json(serde_json::json!({ "success": true }))
}

// ---------------------------------------------------------------------------
// 11. DELETE /kanban/tasks/{id}/dependencies/{depId} — Remove dependency
// ---------------------------------------------------------------------------

async fn remove_dependency_handler(
    State(state): State<Arc<AppState>>,
    Path((id, dep_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Err(e) = sql_forge!(
        r#"DELETE FROM kanban_task_dependencies WHERE task_id = :task_id AND depends_on_id = :dep_id"#,
        ( :task_id = &id, :dep_id = &dep_id )
    )
    .execute(&state.pool)
    .await
    {
        error!(
            "[kanban/tasks/{}/dependencies/{}] delete failed: {:?}",
            id, dep_id, e
        );
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to remove dependency",
        );
    }

    ok_json(serde_json::json!({ "success": true }))
}

// ---------------------------------------------------------------------------
// 12. GET /kanban/tasks/{id}/history — History log
// ---------------------------------------------------------------------------

async fn list_history_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<HistoryQuery>,
) -> impl IntoResponse {
    let action_filter = params.action.as_deref().unwrap_or("");
    let limit = params.limit.unwrap_or(200).clamp(1, 500);
    let offset = params.offset.unwrap_or(0).max(0);

    let rows = match sql_forge!(
        HistoryRow,
        r#"
        SELECT id, kanban_task_id, action, initial_board, final_board,
               previous_values, created_at::text AS created_at
        FROM kanban_history
        WHERE kanban_task_id = :task_id
          AND (:action = '' OR action = :action)
        ORDER BY id DESC
        LIMIT :limit_val OFFSET :offset_val
        "#,
        ( :task_id = &id,
          :action = action_filter,
          :limit_val = limit,
          :offset_val = offset )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[kanban/tasks/{}/history] query failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch history");
        }
    };

    let entries: Vec<HistoryEntry> = rows.into_iter().map(history_row_to_entry).collect();
    ok_json(entries)
}

// ---------------------------------------------------------------------------
// 13. GET /kanban/tasks/{id}/subtasks — Subtasks
// ---------------------------------------------------------------------------

async fn list_subtasks_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let rows = match sql_forge!(
        SubtaskRow,
        r#"
        SELECT
            ts.id,
            ts.description,
            ts.status,
            ts.priority,
            ts.thread_id,
            COALESCE(NULLIF(t.cause, ''), t.id::text) AS thread_title,
            ts.created_at,
            ts.updated_at
        FROM thread_subtasks ts
        JOIN threads t ON t.id = ts.thread_id
        WHERE t.task_id = :task_id
        ORDER BY t.id, ts.priority DESC, ts.id ASC
        "#,
        ( :task_id = &id )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[kanban/tasks/{}/subtasks] query failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch subtasks",
            );
        }
    };

    let entries: Vec<SubtaskEntry> = rows.into_iter().map(subtask_row_to_entry).collect();
    ok_json(entries)
}
