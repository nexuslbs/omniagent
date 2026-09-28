//! Thread subtasks: types and DB query functions using sql_forge!.
//!
//! Each subtask belongs to a thread and tracks a single actionable item
//! with status: pending, processing, completed, cancelled (plus the
//! internal `error` state). `processing` marks the subtask the agent is
//! CURRENTLY working on (visibility/progress state, not a completion state).
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

impl SubtaskRow {
    /// True while the subtask is still outstanding: `pending` (not started yet),
    /// `processing` (the one the agent is currently working on) or the legacy
    /// `in_progress`. The enforcement gates use this so a thread can never close
    /// while a subtask is still marked `processing`.
    pub fn is_unfinished(&self) -> bool {
        matches!(
            self.status.as_str(),
            "pending" | "processing" | "in_progress"
        )
    }
}

/// Summary counts for a thread's subtasks.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SubtaskCounts {
    pub completed_count: i64,
    pub pending_count: i64,
    pub processing_count: i64,
    pub cancelled_count: i64,
    pub error_count: i64,
    pub total_count: i64,
}

/// DB row for subtask count query.
#[derive(Debug, Clone, sqlx::FromRow)]
struct SubtaskCountRow {
    completed_count: Option<i64>,
    pending_count: Option<i64>,
    processing_count: Option<i64>,
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

    tracing::info!(
        "Added subtask {} to thread {}: {}",
        row.id,
        thread_id,
        description
    );
    Ok(row)
}

/// List all subtasks for a thread in CREATION order (the order the plan defined
/// them / the agent created them, i.e. execution order).
///
/// Sort key: `created_at ASC, id ASC`. Rows created as one plan batch keep
/// their insertion order and mid-run additions land AFTER the steps they
/// follow; `id` breaks ties when several rows share a timestamp. The `priority`
/// column is deliberately NOT part of the display order any more: it used to
/// interleave engine-created plan rows (priority = total-i) with agent-created
/// rows (default priority 0), so the rendered list stopped matching the run.
/// No backfill is needed: `created_at`/`id` exist for every row, so old threads
/// render in creation order too.
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
        ORDER BY created_at ASC, id ASC
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

/// Get the current subtask for a thread: a `processing` subtask when one is
/// marked (that is the one the agent is focused on), else the oldest pending
/// one. Within a status group the order is created_at ASC, id ASC (creation
/// order, see `list_subtasks`).
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
        WHERE thread_id = :thread_id AND status IN ('processing', 'pending')
        ORDER BY (status = 'processing') DESC, created_at ASC, id ASC
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
            COALESCE(SUM(CASE WHEN status = 'pending'    THEN 1 ELSE 0 END), 0)::bigint AS pending_count,
            COALESCE(SUM(CASE WHEN status = 'processing' THEN 1 ELSE 0 END), 0)::bigint AS processing_count,
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
        processing_count: row.processing_count.unwrap_or(0),
        cancelled_count: row.cancelled_count.unwrap_or(0),
        error_count: row.error_count.unwrap_or(0),
        total_count: row.total_count.unwrap_or(0),
    })
}

/// Cancel ALL pending/processing/in-progress subtasks of a thread. Called on the FAIL
/// path (builtin_fail-thread, validation failures): a failed thread's
/// remaining subtasks can never be completed by the agent, so they must be
/// auto-cancelled instead of left dangling (observed: after a fail-thread
/// call the thread kept running and the LLM had to manually complete each
/// pending subtask). Completed/cancelled subtasks are left untouched.
/// Returns the number of subtasks cancelled.
pub async fn cancel_pending_subtasks(pool: &PgPool, thread_id: i64) -> anyhow::Result<u64> {
    let result = sql_forge!(
        r#"
        UPDATE thread_subtasks
        SET status = 'cancelled', updated_at = NOW()
        WHERE thread_id = :thread_id AND status IN ('pending', 'processing', 'in_progress')
        "#,
        ( :thread_id = thread_id )
    )
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        tracing::info!(
            "[subtask] Auto-cancelled {} pending subtask(s) of thread {} (fail path)",
            result.rows_affected(),
            thread_id
        );
    }
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: &str) -> SubtaskRow {
        SubtaskRow {
            id: 1,
            thread_id: 1,
            description: "check the build".to_string(),
            status: status.to_string(),
            priority: Some(0),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn processing_pending_and_legacy_in_progress_are_unfinished() {
        for s in ["pending", "processing", "in_progress"] {
            assert!(row(s).is_unfinished(), "{s} must count as unfinished");
        }
        for s in ["completed", "cancelled", "error"] {
            assert!(!row(s).is_unfinished(), "{s} must not count as unfinished");
        }
    }
}
