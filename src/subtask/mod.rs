//! Thread subtasks — types and DB query functions using sql_forge!.
//!
//! Each subtask belongs to a thread and tracks a single actionable item
//! with status: pending, completed, cancelled.
use anyhow::Result;
use sql_forge::sql_forge;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A subtask row as returned from the database.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SubtaskRow {
    pub id: i64,
    pub thread_id: i64,
    pub description: String,
    pub status: String,
    pub priority: Option<i32>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Summary counts for a thread's subtasks.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SubtaskCounts {
    pub completed_count: i64,
    pub pending_count: i64,
    pub cancelled_count: i64,
    pub error_count: i64,
    pub total_count: i64,
}

/// DB row for subtask count query.
#[derive(Debug, Clone, sqlx::FromRow)]
struct SubtaskCountRow {
    completed_count: Option<i64>,
    pending_count: Option<i64>,
    cancelled_count: Option<i64>,
    error_count: Option<i64>,
    total_count: Option<i64>,
}

// ---------------------------------------------------------------------------
// DB query functions
// ---------------------------------------------------------------------------

/// Add a new subtask to a thread.
pub async fn add_subtask(
    pool: &PgPool,
    thread_id: i64,
    description: &str,
    priority: i32,
) -> anyhow::Result<SubtaskRow> {
    let row: SubtaskRow = sql_forge!(
        SubtaskRow,
        r#"
        INSERT INTO thread_subtasks (thread_id, description, status, priority)
        VALUES (:thread_id, :description, 'pending', :priority)
        RETURNING
            id, thread_id, description, status, priority,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        "#,
        ( :thread_id = thread_id, :description = description, :priority = priority )
    )
    .fetch_one(pool)
    .await?;

    tracing::info!("Added subtask {} to thread {}: {}", row.id, thread_id, description);
    Ok(row)
}

/// List all subtasks for a thread, ordered by priority then creation time.
pub async fn list_subtasks(pool: &PgPool, thread_id: i64) -> anyhow::Result<Vec<SubtaskRow>> {
    let rows: Vec<SubtaskRow> = sql_forge!(
        SubtaskRow,
        r#"
        SELECT
            id, thread_id, description, status, priority,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM thread_subtasks
        WHERE thread_id = :thread_id
        ORDER BY priority DESC, created_at ASC
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Update a subtask's status. Returns the number of rows affected (0 if not found).
pub async fn update_subtask_status(
    pool: &PgPool,
    subtask_id: i64,
    status: &str,
) -> anyhow::Result<u64> {
    let result = sql_forge!(
        r#"
        UPDATE thread_subtasks
        SET status = :status, updated_at = NOW()
        WHERE id = :id
        "#,
        ( :status = status, :id = subtask_id )
    )
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        tracing::info!("Updated subtask {} status to '{}'", subtask_id, status);
    }
    Ok(result.rows_affected())
}

/// Update a subtask's description. Returns the number of rows affected.
pub async fn update_subtask_description(
    pool: &PgPool,
    subtask_id: i64,
    description: &str,
) -> anyhow::Result<u64> {
    let result = sql_forge!(
        r#"
        UPDATE thread_subtasks
        SET description = :description, updated_at = NOW()
        WHERE id = :id
        "#,
        ( :description = description, :id = subtask_id )
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Delete a subtask by ID. Returns the number of rows affected.
pub async fn delete_subtask(pool: &PgPool, subtask_id: i64) -> anyhow::Result<u64> {
    let result = sql_forge!(
        "DELETE FROM thread_subtasks WHERE id = :id",
        ( :id = subtask_id )
    )
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        tracing::info!("Deleted subtask {}", subtask_id);
    }
    Ok(result.rows_affected())
}

/// Get the current (non-cancelled) subtask for a thread — the first pending one
/// ordered by priority DESC, created_at ASC.
pub async fn get_current_subtask(
    pool: &PgPool,
    thread_id: i64,
) -> anyhow::Result<Option<SubtaskRow>> {
    let rows: Vec<SubtaskRow> = sql_forge!(
        SubtaskRow,
        r#"
        SELECT
            id, thread_id, description, status, priority,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM thread_subtasks
        WHERE thread_id = :thread_id AND status = 'pending'
        ORDER BY priority DESC, created_at ASC
        LIMIT 1
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().next())
}

/// Get subtask counts for a thread.
pub async fn get_subtask_counts(pool: &PgPool, thread_id: i64) -> anyhow::Result<SubtaskCounts> {
    let row: SubtaskCountRow = sql_forge!(
        SubtaskCountRow,
        r#"
        SELECT
            COALESCE(SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END), 0)::bigint AS completed_count,
            COALESCE(SUM(CASE WHEN status = 'pending'   THEN 1 ELSE 0 END), 0)::bigint AS pending_count,
            COALESCE(SUM(CASE WHEN status = 'cancelled' THEN 1 ELSE 0 END), 0)::bigint AS cancelled_count,
            COALESCE(SUM(CASE WHEN status = 'error'    THEN 1 ELSE 0 END), 0)::bigint AS error_count,
            COUNT(*)::bigint AS total_count
        FROM thread_subtasks
        WHERE thread_id = :thread_id
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_one(pool)
    .await?;

    Ok(SubtaskCounts {
        completed_count: row.completed_count.unwrap_or(0),
        pending_count: row.pending_count.unwrap_or(0),
        cancelled_count: row.cancelled_count.unwrap_or(0),
        error_count: row.error_count.unwrap_or(0),
        total_count: row.total_count.unwrap_or(0),
    })
}
