use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::err_str;
use crate::error::AppResult;

/// Update a kanban task's status and record the transition in history: atomically.
///
/// Fetches the current status as `initial_board`, updates it, and inserts a
/// kanban_history row with action = "moved" so the board transition is always tracked.
pub async fn update_kanban_task_status(
    pool: &PgPool,
    task_id: &str,
    new_status: &str,
) -> AppResult<()> {
    use sqlx::Transaction;

    let mut tx: Transaction<'_, sqlx::Postgres> = pool.begin().await?;

    // 1. Fetch the current status (initial_board)
    let old_status: Option<String> = sql_forge!(
        scalar String,
        "SELECT status FROM kanban_tasks WHERE id = :id FOR UPDATE",
        ( :id = task_id )
    )
    .fetch_optional(&mut *tx)
    .await?
    .map(|v| v.to_string());

    let old_status = match old_status {
        Some(s) => s,
        None => {
            tx.rollback().await?;
            return Err(err_str!("Kanban task '{}' not found", task_id));
        }
    };

    // 2. Update the status
    sql_forge!(
        "UPDATE kanban_tasks SET status = :status, updated_at = NOW() WHERE id = :id",
        ( :status = new_status, :id = task_id )
    )
    .execute(&mut *tx)
    .await?;

    // 3. Insert history record (only if the status actually changed)
    if old_status != new_status {
        sql_forge!(
            r#"
            INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board)
            VALUES (:task_id, 'moved', :initial_board::text, :final_board::text)
            "#,
            ( :task_id = task_id, :initial_board = &old_status, :final_board = new_status )
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

// ── Kanban History ──

/// Insert a kanban_history record using sql_forge! with bound parameters.
pub async fn insert_kanban_history(
    pool: &PgPool,
    task_id: &str,
    action: &str,
    initial_board: Option<&str>,
    final_board: Option<&str>,
    previous_values: Option<serde_json::Value>,
) -> AppResult<()> {
    let pv = previous_values.unwrap_or(serde_json::Value::Null);

    sql_forge!(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values)
        VALUES (:task_id, :action, NULLIF(:initial_board, '')::text, NULLIF(:final_board, '')::text, :previous_values::jsonb)
        "#,
        ( :task_id = task_id, :action = action, :initial_board = initial_board.unwrap_or(""), :final_board = final_board.unwrap_or(""), :previous_values = &pv )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch a single kanban task row by id.
pub async fn get_kanban_task(pool: &PgPool, task_id: &str) -> AppResult<Option<KanbanTaskDb>> {
    let rows = sql_forge!(
        KanbanTaskDb,
        r#"
        SELECT id, title, body, status, priority, assignee, profile, template, archived, position, channel_id, plan, created_at, updated_at
        FROM kanban_tasks
        WHERE id = :id
        "#,
        ( :id = task_id )
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().next())
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct KanbanTaskDb {
    pub id: String,
    pub title: String,
    pub body: Option<String>,
    pub status: String,
    pub priority: Option<i32>,
    pub assignee: Option<String>,
    pub profile: Option<String>,
    pub template: Option<String>,
    pub archived: Option<bool>,
    pub position: Option<i32>,
    pub channel_id: Option<i64>,
    pub plan: bool,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ── History query types ──

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct KanbanHistoryRow {
    pub id: i64,
    pub kanban_task_id: String,
    pub action: String,
    pub initial_board: Option<String>,
    pub final_board: Option<String>,
    pub previous_values: Option<serde_json::Value>,
    pub created_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KanbanHistoryParams {
    pub task_id: Option<String>,
    pub action: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// List kanban history with optional filters: fully parameterized via sql_forge!.
pub async fn list_kanban_history(
    pool: &PgPool,
    params: &KanbanHistoryParams,
) -> AppResult<Vec<KanbanHistoryRow>> {
    let limit: i64 = params.limit.unwrap_or(50).clamp(0, 500);
    let offset: i64 = params.offset.unwrap_or(0).max(0);
    let task_id_filter = params.task_id.as_deref().unwrap_or("");
    let action_filter = params.action.as_deref().unwrap_or("");

    let rows: Vec<KanbanHistoryRow> = sql_forge!(
        KanbanHistoryRow,
        r#"
        SELECT id, kanban_task_id, action, initial_board, final_board,
               previous_values,
               created_at::text AS created_at
        FROM kanban_history
        WHERE (:task_id = '' OR kanban_task_id = :task_id)
          AND (:action = '' OR action = :action)
        ORDER BY id DESC
        LIMIT :limit OFFSET :offset
        "#,
        ( :task_id = task_id_filter, :action = action_filter, :limit = limit, :offset = offset )
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
