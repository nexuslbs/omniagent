//! Kanban API: board view, task CRUD, dependencies, threads, history, subtasks.
//!
//! Replaces ALL SQL in `omni-dashboard/repo/server/routes/kanban.ts` (13 endpoints,
//! ~40 SQL queries). Every query uses `sql_forge!()`: no raw `sqlx::query` calls.
//!
//! Routes (all under `/kanban`):
//!
//!  - GET   /kanban/tasks                          : board tasks (flat list)
//!  - GET   /kanban/tasks/{id}                     : task detail
//!  - GET   /kanban/tasks/{id}/dependencies        : task dependencies
//!  - POST  /kanban/tasks                          : create task
//!  - PATCH /kanban/tasks/{id}/status              : change status (+ position shift)
//!  - PATCH /kanban/tasks/{id}/position            : change position (+ cross-column)
//!  - PATCH /kanban/tasks/{id}                     : update task fields
//!  - DELETE /kanban/tasks/{id}                    : delete task
//!  - GET   /kanban/tasks/{id}/threads             : threads for a task
//!  - POST  /kanban/tasks/{id}/dependencies        : add dependency
//!  - DELETE /kanban/tasks/{id}/dependencies/{depId}: remove dependency
//!  - GET   /kanban/tasks/{id}/history             : history log
//!  - GET   /kanban/tasks/{id}/subtasks            : subtasks

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use tracing::error;

use super::{err_json, ok_json, AppState};
use crate::boards::{boards_enabled, task_board};
use crate::db::threads::{
    create_kanban_step_thread, dispatch_task_for_status, kanban_step_actionable,
};
use crate::workflows::{Workflow, WorkflowConfigError, WorkflowsFile};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const VALID_STATUSES: &[&str] = &[
    "backlog", "todo", "running", "testing", "review", "blocked", "done",
];

/// Sentinel used for optional integer fields (channel, priority) to
/// signal "keep existing value" inside a static UPDATE statement.
const IGNORE_INT: i64 = -999_999;

/// Sentinel used for optional text fields where empty string is a valid value
/// (body) so we can distinguish "not provided" from "explicitly empty".
/// NOTE: cannot contain NUL bytes (Postgres rejects 0x00 in text params with
/// error 22021 "invalid byte sequence for encoding UTF8"). Use a unique
/// printable sentinel that users are extremely unlikely to type.
const IGNORE_STR: &str = "\u{10FFFF}__NO_UPDATE__\u{10FFFF}";

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
        .route(
            "/kanban/tasks/{id}",
            patch(update_task_handler).put(update_task_handler),
        )
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
        // 12b. History (by query param task_id, for frontend kanban-history page)
        .route("/kanban/history", get(list_all_history_handler))
        .route("/review", post(review_handler))
        // 13. Subtasks
        .route("/kanban/tasks/{id}/subtasks", get(list_subtasks_handler))
        // 14. Workflows CRUD
        .route("/workflows", get(list_workflows_handler))
        .route("/workflows/{key}", put(upsert_workflow_handler))
        .route("/workflows/{key}", post(upsert_workflow_handler))
        .route("/workflows/{key}", delete(delete_workflow_handler))
        // 15. Reset workflow executions
        .route(
            "/kanban/tasks/{id}/workflow/executions/reset",
            post(reset_workflow_executions_handler),
        )
        // 16. Dispatch: promote the highest-priority eligible 'todo' task to 'ready'
        .route("/kanban/dispatch", post(dispatch_handler))
        // 17. Redispatch: re-create the role thread for a task already in a
        //     workflow column (running/testing/review) without changing status.
        .route("/kanban/tasks/{id}/redispatch", post(redispatch_handler))
        // 18. Boards CRUD (config/boards.yml)
        .route("/boards", get(list_boards_handler))
        .route(
            "/boards/{key}",
            put(upsert_board_handler).post(upsert_board_handler),
        )
        .route("/boards/{key}", delete(delete_board_handler))
}

// ---------------------------------------------------------------------------
// Query string types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListTasksQuery {
    show_archived: Option<String>,
    /// Optional board name filter: when set, only tasks of that board are returned.
    board: Option<String>,
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
    task_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Request body types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateTaskRequest {
    title: String,
    body: Option<String>,
    assignee: Option<String>,
    channel: Option<String>,
    profile: Option<String>,
    priority: Option<i32>,
    status: Option<String>,
    template: Option<String>,
    plan: Option<bool>,
    workflow: Option<String>,
    board: Option<String>,
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
    assignee: Option<String>,
    channel: Option<String>,
    profile: Option<String>,
    priority: Option<i32>,
    status: Option<String>,
    archived: Option<bool>,
    template: Option<String>,
    plan: Option<bool>,
    workflow: Option<String>,
    board: Option<String>,
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
    channel_id: Option<String>,
    profile: Option<String>,
    archived: Option<bool>,
    template: Option<String>,
    plan: Option<bool>,
    workflow_id: Option<String>,
    board: Option<String>,
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
    channel_id: Option<String>,
    profile: Option<String>,
    archived: Option<bool>,
    template: Option<String>,
    plan: Option<bool>,
    workflow_id: Option<String>,
    board: Option<String>,
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
    comment: Option<String>,
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
    channel: Option<String>,
    profile: Option<String>,
    archived: bool,
    template: Option<String>,
    plan: Option<bool>,
    workflow: Option<String>,
    board: Option<String>,
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
    channel: Option<String>,
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
    comment: Option<String>,
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

fn task_row_to_entry(data_dir: &str, r: KanbanTaskRow) -> KanbanTaskEntry {
    // Resolve the task's fallback fields (task → board → channel → global)
    // AT LOAD TIME: the API hands out resolved channel/workflow/profile/plan/
    // template — never the shallow row (board-based tasks carry NULLs even
    // though the board defines their effective values). On an invalid board
    // (boards.yml present + unknown board) dispatch fails loudly elsewhere;
    // here we log and fall back to the raw row so the API stays displayable.
    let resolved = match crate::resolution::resolve_task_defaults(
        data_dir,
        &crate::resolution::TaskFallbackFields {
            board: r.board.as_deref(),
            workflow_id: r.workflow_id.as_deref(),
            channel_id: r.channel_id.as_deref(),
            profile: r.profile.as_deref(),
            plan: r.plan,
            template: r.template.as_deref(),
        },
    ) {
        Ok(res) => Some(res),
        Err(e) => {
            tracing::warn!(
                "[kanban/tasks] resolve_task_defaults failed for {} (shallow fallback): {}",
                r.id,
                e
            );
            None
        }
    };
    KanbanTaskEntry {
        id: r.id,
        title: r.title,
        body: r.body,
        status: r.status,
        priority: r.priority.unwrap_or(0),
        position: r.position.unwrap_or(0),
        assignee: r.assignee,
        channel: resolved
            .as_ref()
            .map(|res| res.channel_id.clone())
            .filter(|c| !c.is_empty())
            .or_else(|| r.channel_id.clone()),
        profile: resolved
            .as_ref()
            .map(|res| res.profile.clone())
            .filter(|p| !p.is_empty())
            .or_else(|| r.profile.clone()),
        archived: r.archived.unwrap_or(false),
        template: resolved
            .as_ref()
            .and_then(|res| res.template.clone())
            .or_else(|| r.template.clone()),
        plan: resolved.as_ref().and_then(|res| res.plan).or(r.plan),
        workflow: resolved
            .as_ref()
            .and_then(|res| res.workflow_id.clone())
            .or_else(|| r.workflow_id.clone()),
        board: r.board,
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
        comment: r.comment,
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

/// Validate the `board` value on task CREATE.
///
/// When boards are enabled (boards.yml present) the board is REQUIRED and
/// must name an existing board — a board-less task would otherwise be
/// silently skipped by the auto-dispatcher forever. When boards are disabled
/// the field is inert and any value is accepted.
fn validate_create_board(
    data_dir: impl AsRef<std::path::Path>,
    board: Option<&str>,
) -> Result<(), String> {
    if !boards_enabled(data_dir.as_ref()) {
        return Ok(());
    }
    match board {
        Some(b) if !b.trim().is_empty() => task_board(data_dir, Some(b.trim())).map(|_| ()),
        _ => Err("board is required when boards are enabled (boards.yml present)".to_string()),
    }
}

/// Validate the `board` field on task UPDATE.
///
/// When boards are enabled (boards.yml present) the resulting task must
/// always carry a valid board: a missing field keeps the existing (already
/// valid) board; an explicit clear (empty string) or an unknown board name is
/// rejected. When boards are disabled the field is inert (clearing allowed).
fn validate_update_board(
    data_dir: impl AsRef<std::path::Path>,
    board: Option<&str>,
) -> Result<(), String> {
    if !boards_enabled(data_dir.as_ref()) {
        return Ok(());
    }
    match board {
        None => Ok(()),
        Some(b) if b.trim().is_empty() => {
            Err("board cannot be cleared when boards are enabled (boards.yml present)".to_string())
        }
        Some(b) => task_board(data_dir, Some(b.trim())).map(|_| ()),
    }
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
// 1. GET /kanban/tasks: List all board tasks
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
            channel_id, profile, archived, template, plan, workflow_id, board,
            created_at, updated_at
        FROM kanban_tasks
        WHERE ((:show_archived_bool OR archived = false)
           OR (NOT :show_archived_bool AND archived = false))
          AND (:board = '' OR board = :board)
        ORDER BY position ASC, created_at DESC
        "#,
        ( :show_archived_bool = show_archived_bool, :board = params.board.as_deref().unwrap_or("") )
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

    let entries: Vec<KanbanTaskEntry> = rows
        .into_iter()
        .map(|r| task_row_to_entry(&state.data_dir, r))
        .collect();
    ok_json(entries)
}

// ---------------------------------------------------------------------------
// 2. GET /kanban/tasks/{id}: Task detail
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
            channel_id, profile, archived, template, plan, workflow_id, board,
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

    ok_json(task_row_to_entry(&state.data_dir, row))
}

// ---------------------------------------------------------------------------
// 3. GET /kanban/tasks/{id}/dependencies: Task dependencies
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
// 4. POST /kanban/tasks: Create a new task
// ---------------------------------------------------------------------------

async fn create_task_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateTaskRequest>,
) -> impl IntoResponse {
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "Title is required");
    }

    // Board validation: when boards are enabled (boards.yml present), the
    // board is required and must name an existing board — otherwise the
    // auto-dispatcher silently skips the task forever (boards.rs).
    if let Err(msg) = validate_create_board(&state.data_dir, body.board.as_deref()) {
        return err_json(StatusCode::BAD_REQUEST, &msg);
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

    // Resolve the effective `plan` AT CREATE (spec: thread creation resolves
    // plan/profile/provider/model from the board). `kanban_tasks.plan` is
    // `BOOLEAN NOT NULL DEFAULT false` — storing `body.plan.unwrap_or(false)`
    // for a board task WITHOUT an explicit plan would materialize `false` and
    // SHADOW the board's `plan: true` at dispatch/review time (the raw
    // Some(false) beats the board in resolve_task_defaults, so
    // create_kanban_step_thread / manual_review_decision would create
    // no-plan threads). An explicit task plan still wins over the board.
    let task_plan = match body.plan {
        Some(p) => p,
        None => crate::boards::task_board(&state.data_dir, body.board.as_deref())
            .ok()
            .flatten()
            .and_then(|b| b.plan)
            .unwrap_or(false),
    };

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
            (id, title, body, assignee, status, priority, channel_id, profile, position, template, plan, workflow_id, board)
        VALUES
            (:id, :title, :body, NULLIF(:assignee, '')::text, :status, :priority, NULLIF(:channel_id, '')::text, NULLIF(:profile, '')::text,
             :position, NULLIF(:template, '')::text, :plan::boolean, NULLIF(:workflow_id, '')::text, NULLIF(:board, '')::text)
        "#,
        ( :id = id.as_str(),
          :title = &title,
          :body = body.body.as_deref().unwrap_or(""),
          :assignee = body.assignee.as_deref().unwrap_or(""),
          :status = &task_status,
          :priority = task_priority,
          :channel_id = body.channel.as_deref().unwrap_or(""),
          :profile = body.profile.as_deref().unwrap_or(""),
          :position = next_pos,
          :template = body.template.as_deref().unwrap_or(""),
          :plan = task_plan,
            :workflow_id = body.workflow.as_deref().unwrap_or(""),
            :board = body.board.as_deref().unwrap_or(""),
    )
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
        // Non-fatal: task was already created
    }

    ok_json(CreateTaskResponse { success: true, id })
}

// ---------------------------------------------------------------------------
// 5. PATCH /kanban/tasks/{id}/status: Change task status (move column)
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
               workflow_id, board, created_at, updated_at
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
        // No-op: already there
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
            // Moving down: shift intermediate tasks up
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
            // Moving up: shift intermediate tasks down
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

    // 5. History: only if status actually changed
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

    // 6. Dispatch: a status change into a workflow column starts the mapped
    //    role thread (running -> executor, testing -> tester, review ->
    //    reviewer) via the shared status-dispatch path. Best-effort: the
    //    column move already succeeded; a dispatch failure is logged and
    //    surfaced in the response, never rolled back.
    let mut dispatched_thread: Option<i64> = None;
    if old_status != body.status {
        match dispatch_task_for_status(&state.pool, &state.data_dir, &id, &body.status).await {
            Ok(Some(tid)) => dispatched_thread = Some(tid),
            Ok(None) => {}
            Err(e) => {
                error!(
                    "[kanban/tasks/{}/status] dispatch after status change failed: {:?}",
                    id, e
                );
            }
        }
    }

    ok_json(serde_json::json!({
        "success": true,
        "dispatched": dispatched_thread.is_some(),
        "thread_id": dispatched_thread,
    }))
}

// ---------------------------------------------------------------------------
// 6. PATCH /kanban/tasks/{id}/position: Change task position
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
               workflow_id, board, created_at, updated_at
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
            // Moving down: shift intermediate tasks up
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
            // Moving up: shift intermediate tasks down
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

    // 4. History: only if status changed
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
// 7. PATCH /kanban/tasks/{id}: Update task fields
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
               workflow_id, board, created_at, updated_at
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

    // Workflow definitions are immutable while an execution is active.
    if body.workflow.is_some()
        && matches!(
            before.status.as_deref(),
            Some("running" | "testing" | "review")
        )
        && body.workflow.as_deref() != before.workflow_id.as_deref()
    {
        return err_json(
            StatusCode::BAD_REQUEST,
            "workflow cannot be changed while the task is active",
        );
    }

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

    // Board validation: when boards are enabled, the resulting task must
    // always carry a valid board — clearing (empty string) or setting an
    // unknown board is rejected; a missing field keeps the existing board.
    if let Err(msg) = validate_update_board(&state.data_dir, body.board.as_deref()) {
        return err_json(StatusCode::BAD_REQUEST, &msg);
    }

    // 4. Ensure at least one field was provided
    let has_fields = body.title.is_some()
        || body.body.is_some()
        || body.channel.is_some()
        || body.profile.is_some()
        || body.priority.is_some()
        || body.status.is_some()
        || body.archived.is_some()
        || body.template.is_some()
        || body.plan.is_some()
        || body.workflow.is_some()
        || body.board.is_some()
        || body.assignee.is_some();

    if !has_fields {
        return err_json(StatusCode::BAD_REQUEST, "No fields to update");
    }

    // 5. Execute the update: use static SQL with sentinel/COALESCE pattern
    //    so that fields not provided keep their existing values.
    if let Err(e) = sql_forge!(
        r#"
        UPDATE kanban_tasks SET
            title = CASE WHEN :title = '' THEN title ELSE NULLIF(:title, '')::text END,
            body = CASE WHEN :body = :ign_str THEN body ELSE :body END,
            assignee = CASE WHEN :assignee = '' THEN assignee ELSE NULLIF(:assignee, '')::text END,
            channel_id = CASE WHEN :channel_id = :ign_channel THEN channel_id ELSE NULLIF(:channel_id, '')::text END,
            profile = CASE WHEN :profile = '' THEN profile ELSE NULLIF(:profile, '')::text END,
            priority = CASE WHEN :priority = -999999::bigint THEN priority::bigint ELSE :priority END,
            status = CASE WHEN :status = '' THEN status ELSE :status END,
            archived = :archived,
            template = CASE WHEN :template = '' THEN template ELSE NULLIF(:template, '')::text END,
            plan = :plan,
            workflow_id = CASE WHEN :workflow_id = '' THEN workflow_id ELSE NULLIF(:workflow_id, '')::text END,
            board = CASE WHEN :board = '' THEN board ELSE NULLIF(:board, '')::text END,
            updated_at = NOW()
        WHERE id = :id
        "#,
        ( :id = id.as_str(),
          :title = body.title.as_deref().unwrap_or(""),
          :body = body.body.as_deref().unwrap_or(IGNORE_STR),
          :assignee = body.assignee.as_deref().unwrap_or(""),
          :ign_str = IGNORE_STR,
          :ign_channel = IGNORE_STR,
          :channel_id = body.channel.as_deref().unwrap_or(IGNORE_STR),
          :profile = body.profile.as_deref().unwrap_or(""),
          :priority = body.priority.map(|v| v as i64).unwrap_or(IGNORE_INT),
          :status = body.status.as_deref().unwrap_or(""),
          :archived = body.archived.unwrap_or(before.archived.unwrap_or(false)),
          :template = body.template.as_deref().unwrap_or(""),
          :plan = body.plan.or(before.plan).unwrap_or(false),
          :workflow_id = body.workflow.as_deref().unwrap_or(""),
          :board = body.board.as_deref().unwrap_or(""),
    )
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
        // Field edit: log with full previous values
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
            "board": before.board,
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
// 8. DELETE /kanban/tasks/{id}: Delete task
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
               workflow_id, board, created_at, updated_at
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
        "board": before.board,
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
        // Non-fatal: the ON DELETE CASCADE will handle it
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
// 9. GET /kanban/tasks/{id}/threads: Threads for a task
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

    // Fetch paginated rows: use CASE in ORDER BY for dynamic direction
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
            t.channel_id AS channel_name
        FROM threads t
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
            channel: r.channel_name,
        })
        .collect();

    ok_json(ThreadsResponse {
        rows: entries,
        total,
    })
}

// ---------------------------------------------------------------------------
// 10. POST /kanban/tasks/{id}/dependencies: Add a dependency
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
// 11. DELETE /kanban/tasks/{id}/dependencies/{depId}: Remove dependency
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
// 12. GET /kanban/tasks/{id}/history: History log
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
               previous_values, comment, created_at::text AS created_at
        FROM kanban_history
        WHERE kanban_task_id = :p1
          AND (:action_filter = '' OR action = :action_filter)
        ORDER BY id DESC
        LIMIT :limit OFFSET :offset
        "#,
        (
            :p1 = id.as_str(),
            :action_filter = action_filter,
            :limit = limit,
            :offset = offset,
        )
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

/// GET /kanban/history: History log filtered by query params (task_id optional).
/// The frontend kanban-history page calls this with `?task_id=...&action=...&limit=200&offset=0`.
async fn list_all_history_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HistoryQuery>,
) -> impl IntoResponse {
    let action_filter = params.action.as_deref().unwrap_or("");
    let task_filter = params.task_id.as_deref().unwrap_or("");
    let limit = params.limit.unwrap_or(200).clamp(1, 500);
    let offset = params.offset.unwrap_or(0).max(0);

    let rows = match sql_forge!(
        HistoryRow,
        r#"
        SELECT id, kanban_task_id, action, initial_board, final_board,
               previous_values, comment, created_at::text AS created_at
        FROM kanban_history
        WHERE (:task_filter = '' OR kanban_task_id = :task_filter)
          AND (:action_filter = '' OR action = :action_filter)
        ORDER BY id DESC
        LIMIT :limit OFFSET :offset
        "#,
        (
            :task_filter = task_filter,
            :action_filter = action_filter,
            :limit = limit,
            :offset = offset,
        )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[kanban/history] query failed: {:?}", e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch history");
        }
    };

    let entries: Vec<HistoryEntry> = rows.into_iter().map(history_row_to_entry).collect();
    ok_json(entries)
}

// ---------------------------------------------------------------------------
// 13. GET /kanban/tasks/{id}/subtasks: Subtasks
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

// ── Tests ──────────────────────────────────────────────────────────────────

// ---------------------------------------------------------------------------
// POST /review: manual/API-only review decision (spec §8 R12)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ReviewRequest {
    task_id: String,
    decision: String,
    comment: Option<String>,
}

/// Manual/API-only review decision. The reviewer AGENT does not call this
/// endpoint (R12): it signals approve via normal thread completion and issues
/// via fail-thread with `workflow_step` = running/testing/blocked (N6).
/// Decisions: approve | rework | retest | block (+ optional comment).
/// Server-side target validation (R5) + retry guards (D1/R2) live in
/// `crate::agent::manual_review_decision`.
async fn review_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ReviewRequest>,
) -> impl IntoResponse {
    match crate::agent::manual_review_decision(
        &state.pool,
        &state.data_dir,
        &body.task_id,
        &body.decision,
        body.comment.as_deref(),
    )
    .await
    {
        Ok(outcome) => ok_json(serde_json::json!({
            "success": true,
            "task_id": outcome.task_id,
            "status": outcome.status,
            "thread_id": outcome.thread_id,
            "comment": outcome.comment,
        })),
        Err(e) => err_json(StatusCode::BAD_REQUEST, &e),
    }
}

// ---------------------------------------------------------------------------
// Workflows CRUD (Phase 5)
// ---------------------------------------------------------------------------

/// Absolute path to the deployment's `workflows.yml` (under the data dir).
fn workflows_file_path(state: &AppState) -> std::path::PathBuf {
    crate::config_path::config_path(&state.data_dir, "workflows.yml")
}

/// Load the workflows file; a missing file counts as an empty document.
fn load_workflows_file(state: &AppState) -> Result<WorkflowsFile, WorkflowConfigError> {
    let path = workflows_file_path(state);
    match WorkflowsFile::load(&path) {
        Ok(file) => Ok(file),
        Err(WorkflowConfigError::NotFound { .. }) => Ok(WorkflowsFile::default()),
        Err(err) => Err(err),
    }
}

/// Serialize a workflows file (with effective per-role resolution) for the API.
fn workflows_response(file: &WorkflowsFile) -> serde_json::Value {
    let workflows: Vec<serde_json::Value> = file
        .resolve_all()
        .into_iter()
        .map(|(key, workflow, resolved)| {
            let resolved_map: serde_json::Map<String, serde_json::Value> = resolved
                .into_iter()
                .map(|(role_key, role)| {
                    (
                        role_key,
                        serde_json::json!({
                            "template": role.template,
                            "profile": role.profile,
                            "provider": role.provider,
                            "model": role.model,
                            "plan_mode": role.plan_mode,
                            "retries": role.retries,
                        }),
                    )
                })
                .collect();
            serde_json::json!({
                "key": key,
                "workflow": workflow,
                "resolved": resolved_map,
            })
        })
        .collect();
    serde_json::json!({ "workflows": workflows })
}

async fn list_workflows_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match load_workflows_file(&state) {
        Ok(file) => ok_json(workflows_response(&file)),
        Err(err) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to load workflows.yml: {err}"),
        ),
    }
}

async fn upsert_workflow_handler(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<Workflow>,
) -> impl IntoResponse {
    let key = key.trim().to_string();
    // Validate the payload on its own first (executor role required;
    // tester/reviewer templates required when the role is present).
    let mut candidate = WorkflowsFile::default();
    candidate.workflows.insert(key.clone(), body.clone());
    if let Err(err) = candidate.validate() {
        return err_json(StatusCode::BAD_REQUEST, &format!("invalid workflow: {err}"));
    }
    let mut file = match load_workflows_file(&state) {
        Ok(file) => file,
        Err(err) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to load workflows.yml: {err}"),
            );
        }
    };
    if let Err(err) = file.upsert(&key, body) {
        return err_json(StatusCode::BAD_REQUEST, &format!("invalid workflow: {err}"));
    }
    if let Err(err) = file.save(&workflows_file_path(&state)) {
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to write workflows.yml: {err}"),
        );
    }
    ok_json(workflows_response(&file))
}

async fn delete_workflow_handler(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    let mut file = match load_workflows_file(&state) {
        Ok(file) => file,
        Err(err) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to load workflows.yml: {err}"),
            );
        }
    };
    if file.remove(&key).is_none() {
        return err_json(
            StatusCode::NOT_FOUND,
            &format!("workflow '{key}' not found"),
        );
    }
    if let Err(err) = file.save(&workflows_file_path(&state)) {
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to write workflows.yml: {err}"),
        );
    }
    ok_json(workflows_response(&file))
}

// ---------------------------------------------------------------------------
// Boards CRUD (kanban boards — config/boards.yml)
// ---------------------------------------------------------------------------

/// Absolute path to the deployment's `boards.yml` (under the data dir).
fn boards_file_path(state: &AppState) -> std::path::PathBuf {
    crate::config_path::config_path(&state.data_dir, "boards.yml")
}

/// Load the boards file; a missing file counts as an empty document.
fn load_boards_file(
    state: &AppState,
) -> Result<crate::boards::BoardsFile, crate::boards::BoardsConfigError> {
    let path = boards_file_path(state);
    match crate::boards::BoardsFile::load(&path) {
        Ok(file) => Ok(file),
        Err(crate::boards::BoardsConfigError::NotFound { .. }) => {
            Ok(crate::boards::BoardsFile::default())
        }
        Err(err) => Err(err),
    }
}

/// Serialize a boards file for the API.
fn boards_response(file: &crate::boards::BoardsFile) -> serde_json::Value {
    let boards: Vec<serde_json::Value> = file
        .boards
        .iter()
        .map(|(key, board)| serde_json::json!({ "key": key, "board": board }))
        .collect();
    serde_json::json!({ "boards": boards })
}

async fn list_boards_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match load_boards_file(&state) {
        Ok(file) => ok_json(boards_response(&file)),
        Err(err) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to load boards.yml: {err}"),
        ),
    }
}

async fn upsert_board_handler(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<crate::boards::BoardConfig>,
) -> impl IntoResponse {
    let key = key.trim().to_string();
    if key.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "board name cannot be empty");
    }
    let mut file = match load_boards_file(&state) {
        Ok(file) => file,
        Err(err) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to load boards.yml: {err}"),
            );
        }
    };
    file.upsert(&key, body);
    if let Err(err) = file.save(&boards_file_path(&state)) {
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to write boards.yml: {err}"),
        );
    }
    ok_json(boards_response(&file))
}

/// Delete a board AND every task that belongs to it. The per-task cleanup
/// mirrors `delete_task_handler` (history record, dependency rows, thread
/// detach) so board deletion is consistent with the existing task-delete
/// behavior; only then is the board removed from boards.yml.
async fn delete_board_handler(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    let mut file = match load_boards_file(&state) {
        Ok(file) => file,
        Err(err) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to load boards.yml: {err}"),
            );
        }
    };
    if file.remove(&key).is_none() {
        return err_json(StatusCode::NOT_FOUND, &format!("board '{key}' not found"));
    }

    // 1. History: record every deleted task.
    let _ = sql_forge!(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values)
        SELECT id, 'deleted', NULL, NULL, jsonb_build_object('board', :board::text)
        FROM kanban_tasks WHERE board = :board
        "#,
        ( :board = &key )
    )
    .execute(&state.pool)
    .await
    .map_err(|e| {
        tracing::warn!("[kanban/boards/{}] history insert failed: {:?}", key, e)
    });

    // 2. Clear dependencies (both directions) for the board's tasks.
    let _ = sql_forge!(
        r#"
        DELETE FROM kanban_task_dependencies
        WHERE task_id IN (SELECT id FROM kanban_tasks WHERE board = :board)
           OR depends_on_id IN (SELECT id FROM kanban_tasks WHERE board = :board)
        "#,
        ( :board = &key )
    )
    .execute(&state.pool)
    .await
    .map_err(|e| tracing::warn!("[kanban/boards/{}] dependency delete failed: {:?}", key, e));

    // 3. Detach threads of the board's tasks.
    let _ = sql_forge!(
        r#"
        UPDATE threads SET task_id = NULL
        WHERE task_id IN (SELECT id FROM kanban_tasks WHERE board = :board)
        "#,
        ( :board = &key )
    )
    .execute(&state.pool)
    .await
    .map_err(|e| tracing::warn!("[kanban/boards/{}] thread detach failed: {:?}", key, e));

    // 4. Delete the tasks themselves.
    if let Err(e) = sql_forge!(
        r#"DELETE FROM kanban_tasks WHERE board = :board"#,
        ( :board = &key )
    )
    .execute(&state.pool)
    .await
    {
        error!("[kanban/boards/{}] task delete failed: {:?}", key, e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to delete board tasks",
        );
    }

    // 5. Persist the boards file (board removed).
    if let Err(err) = file.save(&boards_file_path(&state)) {
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to write boards.yml: {err}"),
        );
    }
    ok_json(boards_response(&file))
}

// ---------------------------------------------------------------------------
// Reset workflow executions (Phase 5)
// ---------------------------------------------------------------------------

/// Strip the `executions` key from a `workflow_state` document.
/// Returns `None` when the state is absent or not a JSON object.
fn reset_executions_json(state: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let value = state?;
    let mut obj = value.as_object()?.clone();
    obj.remove("executions");
    Some(serde_json::Value::Object(obj))
}

/// Decide whether a reset is meaningful and compute the cleared state.
/// A task without a `workflow_id` is a no-op (idempotent reset).
fn resolve_workflow_reset(
    workflow_id: &Option<String>,
    state: &Option<serde_json::Value>,
) -> (bool, Option<serde_json::Value>) {
    match workflow_id {
        None => (false, None),
        Some(_) => (true, reset_executions_json(state.as_ref())),
    }
}

#[derive(FromRow)]
struct WorkflowResetRow {
    id: String,
    workflow_id: Option<String>,
    board: Option<String>,
    workflow_state: Option<serde_json::Value>,
}

#[derive(FromRow)]
struct WorkflowResetUpdateRow {
    id: String,
}

async fn reset_workflow_executions_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let row = match sql_forge!(
        WorkflowResetRow,
        r#"SELECT id, workflow_id, board, workflow_state
           FROM kanban_tasks
           WHERE id = :id"#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(err) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to load task: {err}"),
            );
        }
    };
    let row = match row {
        Some(row) => row,
        None => return err_json(StatusCode::NOT_FOUND, "task not found"),
    };
    // Resolve the task's effective workflow ONCE at load (task → board): a
    // board task has raw NULL workflow_id but inherits the board's workflow,
    // so the reset decision must use the RESOLVED workflow_id.
    let resolved = match crate::resolution::resolve_task_defaults(
        &state.data_dir,
        &crate::resolution::TaskFallbackFields {
            board: row.board.as_deref(),
            workflow_id: row.workflow_id.as_deref(),
            channel_id: None,
            profile: None,
            plan: None,
            template: None,
        },
    ) {
        Ok(r) => r,
        Err(board_err) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("cannot resolve task defaults: {board_err}"),
            );
        }
    };
    let (should_reset, cleared) =
        resolve_workflow_reset(&resolved.workflow_id, &row.workflow_state);
    if !should_reset {
        return ok_json(serde_json::json!({
            "reset": false,
            "message": "task has no workflow assigned; nothing to reset",
        }));
    }
    let cleared = cleared.unwrap_or_else(|| serde_json::json!({}));
    if let Err(err) = sql_forge!(
        WorkflowResetUpdateRow,
        r#"UPDATE kanban_tasks
           SET workflow_state = :state
           WHERE id = :id
           RETURNING id"#,
        ( :state = &cleared, :id = &id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to reset workflow executions: {err}"),
        );
    }
    ok_json(serde_json::json!({ "reset": true }))
}

// ---------------------------------------------------------------------------
// Dispatch (POST /kanban/dispatch)
// ---------------------------------------------------------------------------

/// POST /kanban/dispatch
///
/// Thin HTTP wrapper over the shared in-process dispatch routine
/// [`crate::kanban_dispatch::dispatch_todo_tasks`] (also driven by the core
/// background loop, `kanban_dispatcher_interval`). The decision logic —
/// board gate, dependency gate, channel-busy gate, priority ordering — lives
/// in `kanban_dispatch`; this handler only formats the outcome as an HTTP
/// response. Returns `{"dispatched": false}` when nothing is eligible.
async fn dispatch_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match crate::kanban_dispatch::dispatch_todo_tasks(&state.pool, &state.data_dir).await {
        Ok(summary) => {
            if summary.dispatched {
                ok_json(serde_json::json!({
                    "dispatched": true,
                    "task_id": summary.task_id,
                    "thread_id": summary.thread_id,
                }))
            } else {
                ok_json(serde_json::json!({
                    "dispatched": false,
                    "message": summary.message,
                }))
            }
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// POST /kanban/tasks/{id}/redispatch
///
/// Re-create the role thread for a task ALREADY in a workflow column
/// (`running`/`testing`/`review`) without changing its status. No-op
/// (`redispatch: false`) when the status has no role to run or the task
/// already has an active thread. Example: a task stuck in `running` whose
/// executor thread died without a terminal transition -> redispatch creates a
/// fresh executor thread and the agent loop picks it up.
async fn redispatch_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // 1. Load the task; 404 when missing.
    #[derive(FromRow)]
    struct RedispatchTaskRow {
        status: String,
        workflow_id: Option<String>,
        board: Option<String>,
    }
    let task = match sql_forge!(
        RedispatchTaskRow,
        r#"SELECT status, workflow_id, board FROM kanban_tasks WHERE id = :id"#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(r)) => r,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "Task not found"),
        Err(e) => {
            error!("[kanban/tasks/{}/redispatch] load failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to load task");
        }
    };

    // 1a. Resolve the task's effective defaults ONCE at load (task → board →
    //     channel → global settings) — the universal resolution pattern. The
    //     role gate below must use the RESOLVED workflow_id: a board task has
    //     raw NULL workflow_id but inherits the board's workflow.
    let resolved = match crate::resolution::resolve_task_defaults(
        &state.data_dir,
        &crate::resolution::TaskFallbackFields {
            board: task.board.as_deref(),
            workflow_id: task.workflow_id.as_deref(),
            channel_id: None,
            profile: None,
            plan: None,
            template: None,
        },
    ) {
        Ok(r) => r,
        Err(board_err) => {
            // Fail-loud invalid board: redispatch cannot determine the
            // workflow (mirrors create_kanban_step_thread's doomed-thread
            // failure) → no role to run.
            return ok_json(serde_json::json!({
                "redispatch": false,
                "reason": format!("cannot resolve task defaults: {board_err}"),
            }));
        }
    };
    let workflow_id = resolved.workflow_id;

    // 2. Role gate: only workflow columns map to a role thread. `running`
    //    always; `testing`/`review` only when the workflow defines the role.
    let role_present = match load_workflows_file(&state) {
        Ok(file) => workflow_id
            .as_deref()
            .and_then(|wf_id| file.workflows.get(wf_id))
            .and_then(|wf| {
                crate::workflows::role_for_step(&task.status).and_then(|role| wf.resolve_role(role))
            })
            .is_some(),
        Err(_) => false,
    };
    if !kanban_step_actionable(&task.status, workflow_id.as_deref(), role_present) {
        return ok_json(serde_json::json!({
            "redispatch": false,
            "reason": format!("status '{}' has no role to run", task.status),
        }));
    }

    // 3. Already-active thread -> no-op (redispatch is for tasks that are NOT
    //    actually running; never skip a live thread here).
    let active: i64 = match sql_forge!(
        scalar i64,
        r#"SELECT COUNT(*) FROM threads WHERE task_id = :task_id AND status IN ('pending', 'processing')"#,
        ( :task_id = &id )
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            error!("[kanban/tasks/{}/redispatch] active check failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to check active threads",
            );
        }
    };
    if active > 0 {
        return ok_json(serde_json::json!({
            "redispatch": false,
            "reason": "already active",
        }));
    }

    // 4. Create the role thread for the task's CURRENT status (no stale
    //    skip — the check above guarantees no active thread) and mark
    //    thread_status='scheduled'. Task status is left unchanged.
    match create_kanban_step_thread(&state.pool, &state.data_dir, &id, &task.status, false).await {
        Ok(Some(tid)) => ok_json(serde_json::json!({
            "redispatch": true,
            "thread_id": tid,
        })),
        Ok(None) => ok_json(serde_json::json!({
            "redispatch": false,
            "reason": format!("status '{}' has no role to run", task.status),
        })),
        Err(e) => {
            error!("[kanban/tasks/{}/redispatch] create failed: {:?}", id, e);
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Failed to redispatch: {e}"),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reset_executions_cleared() {
        let state = serde_json::json!({
            "executions": [{"step": "research", "ok": true}],
            "step_index": 2,
        });
        let cleared = reset_executions_json(Some(&state)).expect("object in, object out");
        assert!(
            cleared.get("executions").is_none(),
            "executions must be cleared"
        );
        assert_eq!(cleared["step_index"], 2, "other state must be preserved");
    }

    #[test]
    fn test_reset_executions_idempotent() {
        let state = serde_json::json!({"executions": [1, 2, 3]});
        let once = reset_executions_json(Some(&state)).expect("cleared once");
        let twice = reset_executions_json(Some(&once)).expect("cleared twice");
        assert_eq!(once, twice);
        assert!(twice.get("executions").is_none());
    }

    #[test]
    fn test_create_task_request_deserializes_workflow() {
        let json = r#"{"title": "T", "workflow": "wf-x"}"#;
        let req: CreateTaskRequest =
            serde_json::from_str(json).expect("deserialize CreateTaskRequest");
        assert_eq!(req.workflow.as_deref(), Some("wf-x"));
        let empty: CreateTaskRequest =
            serde_json::from_str(r#"{"title": "T"}"#).expect("deserialize w/o wf");
        assert!(empty.workflow.is_none());
    }

    #[test]
    fn test_task_row_to_entry_preserves_workflow() {
        let row = KanbanTaskRow {
            id: "task-1".to_string(),
            title: "Test Task".to_string(),
            body: Some("A body".to_string()),
            status: "todo".to_string(),
            priority: Some(3),
            position: Some(1),
            assignee: Some("alice".to_string()),
            channel_id: Some("kanban".to_string()),
            profile: Some("default".to_string()),
            archived: Some(false),
            template: None,
            plan: None,
            workflow_id: Some("wf-x".to_string()),
            board: None,
            created_at: None,
            updated_at: None,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let entry = task_row_to_entry(dir.path().to_str().unwrap(), row);
        assert_eq!(entry.workflow.as_deref(), Some("wf-x"));
    }

    #[test]
    fn test_resolve_workflow_reset_noop_without_workflow_id() {
        let state = serde_json::json!({"executions": [1]});
        let (should, cleared) = resolve_workflow_reset(&None, &Some(state));
        assert!(!should, "no workflow_id -> no reset");
        assert!(cleared.is_none());

        // A workflow_id makes the reset meaningful even with absent state.
        let (should, cleared) = resolve_workflow_reset(&Some("wf-1".to_string()), &None);
        assert!(should);
        assert!(cleared.is_none());
    }

    #[test]
    fn test_validate_status_valid() {
        assert!(validate_status("backlog"));
        assert!(validate_status("todo"));
        assert!(!validate_status("ready"));
        assert!(validate_status("testing"));
        assert!(validate_status("running"));
        assert!(validate_status("review"));
        assert!(validate_status("blocked"));
        assert!(validate_status("done"));
    }

    #[test]
    fn test_validate_status_invalid() {
        assert!(!validate_status("invalid"));
        assert!(!validate_status(""));
        assert!(!validate_status("DONE"));
    }

    #[test]
    fn test_validate_create_board_disabled_accepts_any() {
        // boards.yml absent -> feature disabled: board optional/inert.
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(validate_create_board(dir.path(), None).is_ok());
        assert!(validate_create_board(dir.path(), Some("")).is_ok());
        assert!(validate_create_board(dir.path(), Some("anything")).is_ok());
    }

    #[test]
    fn test_validate_create_board_enabled_requires_board() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crate::boards::boards_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "boards:\n  main:\n    channel: kanban\n").unwrap();
        // Missing/empty/whitespace -> "board is required".
        let err = validate_create_board(dir.path(), None).unwrap_err();
        assert!(err.contains("board is required"), "got: {err}");
        let err = validate_create_board(dir.path(), Some("")).unwrap_err();
        assert!(err.contains("board is required"), "got: {err}");
        let err = validate_create_board(dir.path(), Some("   ")).unwrap_err();
        assert!(err.contains("board is required"), "got: {err}");
        // Unknown board -> not found in boards.yml.
        let err = validate_create_board(dir.path(), Some("nope")).unwrap_err();
        assert!(err.contains("not found in boards.yml"), "got: {err}");
        // Valid board -> ok.
        assert!(validate_create_board(dir.path(), Some("main")).is_ok());
    }

    #[test]
    fn test_validate_update_board_disabled_allows_clear() {
        // boards.yml absent -> clearing allowed, any value ok.
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(validate_update_board(dir.path(), None).is_ok());
        assert!(validate_update_board(dir.path(), Some("")).is_ok());
        assert!(validate_update_board(dir.path(), Some("anything")).is_ok());
    }

    #[test]
    fn test_validate_update_board_enabled_blocks_clear_and_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = crate::boards::boards_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "boards:\n  main:\n    channel: kanban\n").unwrap();
        // Field not sent -> keep existing board.
        assert!(validate_update_board(dir.path(), None).is_ok());
        // Explicit clear -> rejected.
        let err = validate_update_board(dir.path(), Some("")).unwrap_err();
        assert!(err.contains("cannot be cleared"), "got: {err}");
        // Unknown board -> rejected.
        let err = validate_update_board(dir.path(), Some("nope")).unwrap_err();
        assert!(err.contains("not found in boards.yml"), "got: {err}");
        // Valid board -> ok.
        assert!(validate_update_board(dir.path(), Some("main")).is_ok());
    }

    #[test]
    fn test_task_row_to_entry_basic() {
        let row = KanbanTaskRow {
            id: "task-1".to_string(),
            title: "Test Task".to_string(),
            body: Some("A body".to_string()),
            status: "todo".to_string(),
            priority: Some(3),
            position: Some(1),
            assignee: Some("alice".to_string()),
            channel_id: Some("kanban".to_string()),
            profile: Some("default".to_string()),
            archived: Some(false),
            template: None,
            plan: None,
            workflow_id: None,
            board: None,
            created_at: None,
            updated_at: None,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let entry = task_row_to_entry(dir.path().to_str().unwrap(), row);
        assert_eq!(entry.id, "task-1");
        assert_eq!(entry.priority, 3);
        assert_eq!(entry.position, 1);
        assert!(!entry.archived);
    }

    #[test]
    fn test_task_row_to_entry_defaults() {
        let row = KanbanTaskRow {
            id: "task-2".to_string(),
            title: "No Options".to_string(),
            body: None,
            status: "done".to_string(),
            priority: None,
            position: None,
            assignee: None,
            channel_id: None,
            profile: None,
            archived: None,
            template: None,
            plan: None,
            workflow_id: None,
            board: None,
            created_at: None,
            updated_at: None,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let entry = task_row_to_entry(dir.path().to_str().unwrap(), row);
        assert_eq!(entry.priority, 0);
        assert_eq!(entry.position, 0);
        assert!(!entry.archived);
        assert_eq!(entry.created_at, None);
    }

    #[test]
    fn test_dep_row_to_entry() {
        use chrono::TimeZone;
        let dt = chrono::Utc
            .with_ymd_and_hms(2026, 1, 15, 10, 30, 0)
            .unwrap();
        let row = DependencyRow {
            id: "dep-1".to_string(),
            title: "Depends on X".to_string(),
            status: "todo".to_string(),
            priority: Some(2),
            created_at: Some(dt),
        };
        let entry = dep_row_to_entry(row);
        assert_eq!(entry.id, "dep-1");
        assert_eq!(entry.priority, 2);
        assert!(entry.created_at.unwrap().contains("2026-01-15T10:30:00"));
    }

    #[test]
    fn test_dep_row_to_entry_defaults() {
        let row = DependencyRow {
            id: "dep-2".to_string(),
            title: "No priority".to_string(),
            status: "blocked".to_string(),
            priority: None,
            created_at: None,
        };
        let entry = dep_row_to_entry(row);
        assert_eq!(entry.priority, 0);
        assert_eq!(entry.created_at, None);
    }

    #[test]
    fn test_history_row_to_entry() {
        let row = HistoryRow {
            id: 1,
            kanban_task_id: "task-1".to_string(),
            action: "status_change".to_string(),
            initial_board: Some("todo".to_string()),
            final_board: Some("done".to_string()),
            previous_values: None,
            created_at: Some("2026-01-15T10:30:00Z".to_string()),
            comment: Some("moved from todo to done".to_string()),
        };
        let entry = history_row_to_entry(row);
        assert_eq!(entry.id, 1);
        assert_eq!(entry.action, "status_change");
        assert_eq!(entry.initial_board, Some("todo".to_string()));
        assert_eq!(entry.created_at, Some("2026-01-15T10:30:00Z".to_string()));
        assert_eq!(entry.comment, Some("moved from todo to done".to_string()));
    }

    #[test]
    fn test_subtask_row_to_entry() {
        use chrono::TimeZone;
        let dt = chrono::Utc.with_ymd_and_hms(2026, 2, 1, 8, 0, 0).unwrap();
        let row = SubtaskRow {
            id: 10,
            description: "Do the thing".to_string(),
            status: Some("done".to_string()),
            priority: Some(1),
            thread_id: 123,
            thread_title: Some("Main thread".to_string()),
            created_at: Some(dt),
            updated_at: Some(dt),
        };
        let entry = subtask_row_to_entry(row);
        assert_eq!(entry.id, 10);
        assert_eq!(entry.description, "Do the thing");
        assert_eq!(entry.priority, 1);
        assert_eq!(entry.thread_id, 123);
    }

    #[test]
    fn test_subtask_row_to_entry_defaults() {
        let row = SubtaskRow {
            id: 11,
            description: "Another subtask".to_string(),
            status: None,
            priority: None,
            thread_id: 456,
            thread_title: None,
            created_at: None,
            updated_at: None,
        };
        let entry = subtask_row_to_entry(row);
        assert_eq!(entry.priority, 0);
        assert_eq!(entry.status, None);
        assert_eq!(entry.created_at, None);
    }
}
