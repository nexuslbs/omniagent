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

    /// Requirement D (`task_omnidev_subtasks_stop_double_creating_plan`): the
    /// listed / rendered order must be CREATION order, never priority order,
    /// and it must be stable across repeated calls.
    ///
    /// Regression for thread 3443: the engine plan batch (priority = total-i)
    /// and a mid-run addition carrying a HIGHER priority were interleaved by
    /// `priority DESC`, so the priority-10 row rendered before the plan rows
    /// and the list stopped matching the run's sequence.
    ///
    /// DB-backed: needs a live DATABASE_URL (dev stack), same convention as the
    /// other `requires a live DATABASE_URL` tests.
    #[tokio::test]
    #[ignore = "requires a live DATABASE_URL"]
    async fn list_subtasks_is_creation_order_and_stable() {
        use sqlx::PgPool;

        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let pool = PgPool::connect(&url)
            .await
            .expect("connect to DATABASE_URL");

        // thread_subtasks.thread_id has an FK to threads(id): reuse an existing
        // (oldest = surely idle) thread row instead of inventing one.
        let thread_id: i64 = sqlx::query_scalar("SELECT id FROM threads ORDER BY id ASC LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("need at least one thread row");

        let tag = "[ordering-test]";
        sql_forge!(
            "DELETE FROM thread_subtasks WHERE thread_id = :thread_id AND description LIKE :tag",
            ( :thread_id = thread_id, :tag = format!("{tag}%") )
        )
        .execute(&pool)
        .await
        .expect("cleanup before");

        // One engine plan batch, exactly like extract_plan_steps (priority = total - i).
        let mut created: Vec<i64> = Vec::new();
        for (step, priority) in [(1, 3), (2, 2), (3, 1)] {
            let row = add_subtask(
                &pool,
                thread_id,
                &format!("{tag} plan step {step}"),
                priority,
            )
            .await
            .expect("insert plan row");
            created.push(row.id);
        }
        // A mid-run discovery with a HIGHER priority than every plan row - the
        // row that used to jump to the top of the list.
        let discovered = add_subtask(&pool, thread_id, &format!("{tag} discovered mid-run"), 10)
            .await
            .expect("insert discovered row");
        created.push(discovered.id);
        created.sort_unstable();

        let listed = list_subtasks(&pool, thread_id).await.expect("list");
        let mine: Vec<i64> = listed
            .iter()
            .filter(|s| s.description.starts_with(tag))
            .map(|s| s.id)
            .collect();
        assert_eq!(
            mine, created,
            "list_subtasks must render CREATION order (ascending id), not priority order"
        );
        assert_eq!(
            *mine.last().unwrap(),
            discovered.id,
            "the mid-run addition must land AFTER the plan rows it follows"
        );
        assert_eq!(
            listed
                .iter()
                .filter(|s| s.description.starts_with(tag))
                .count(),
            created.len(),
            "no row may be dropped or duplicated by the ordering"
        );

        // Same rows, same order on a repeated call (deterministic / stable).
        let again = list_subtasks(&pool, thread_id).await.expect("list again");
        let mine_again: Vec<i64> = again
            .iter()
            .filter(|s| s.description.starts_with(tag))
            .map(|s| s.id)
            .collect();
        assert_eq!(mine, mine_again, "list_subtasks order must be stable");

        for id in &created {
            delete_subtask(&pool, *id).await.expect("cleanup row");
        }
    }
}
