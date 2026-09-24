use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::err_str;
use crate::error::AppResult;

/// Update a kanban task's status and record the transition in history: atomically.
///
/// Fetches the current status as `initial_board`, updates it, and inserts a
/// kanban_history row with action = "moved" so the board transition is always tracked.
///
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
        r#"
        UPDATE kanban_tasks SET
            status = :status,
            updated_at = NOW()
        WHERE id = :id
        "#,
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

    // Stop the task's old-status threads: any pending/processing thread
    // whose workflow step does not serve the new status is marked skipped
    // (single choke point) so it can never keep running against a task
    // that moved away from it. Step-scoped: a thread already serving the
    // new status (e.g. a just-dispatched step thread) is untouched, and a
    // no-op update (old == new) never skips the current-status thread.
    if old_status != new_status {
        if let Err(e) =
            crate::db::threads::skip_stale_threads_for_status(pool, task_id, new_status, None).await
        {
            tracing::warn!(
                "[kanban] failed to skip stale threads after moving task {} to {}: {:?}",
                task_id,
                new_status,
                e
            );
        }
    }

    Ok(())
}

/// Pickup-time workflow status sync (supervisor): set the kanban task to the
/// status the picked-up workflow step serves.
///
/// MANUAL STATUS CHANGE WINS (operator report 2026-09-14, task
/// kanban_moving_a_task_to_backlog_must): the sync is suppressed for a task
/// the operator parked (`backlog`/`todo`) or that is terminal (`blocked`/
/// `done`). A thread that was already claimed when the operator moved the task
/// must never resurrect the task's status - otherwise moving a review task to
/// backlog is undone by the pickup of the very thread the move skipped (the
/// reported bug: task flipped back to `review` seconds after the move).
///
/// Returns `true` when the sync was applied, `false` when it was suppressed.
pub async fn sync_task_status_on_pickup(
    pool: &PgPool,
    task_id: &str,
    step_status: &str,
) -> AppResult<bool> {
    let current: Option<String> = sql_forge!(
        scalar String,
        "SELECT status FROM kanban_tasks WHERE id = :id",
        ( :id = task_id )
    )
    .fetch_optional(pool)
    .await?
    .map(|v| v.to_string());

    // Unknown task (None) or a parked/terminal status: nothing to sync.
    match current.as_deref() {
        Some(status) if !crate::agent::fail_thread::is_parked_status(status) => {
            update_kanban_task_status(pool, task_id, step_status).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
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

/// The toolset id defined by a kanban task (`kanban_tasks.toolset`); `None`
/// when the task defines none or does not exist. Used as the TASK level of the
/// first-match toolset resolution (`workflow_role > workflow > task > channel
/// > profile`) when a step thread is spawned.
pub async fn task_toolset(pool: &PgPool, task_id: &str) -> AppResult<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT toolset FROM kanban_tasks WHERE id = $1")
            .bind(task_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|r| r.0))
}

/// The board (`kanban_tasks.board`) a kanban task belongs to; `None` when the
/// task does not exist or defines no board.
///
/// Used as the BOARD tier of the toolset chain when an unresolved
/// `threads.toolset` id must be attributed to the level that defined it
/// (`toolset 'foo' defined by board 'omnidev' ...`).
pub async fn task_board(pool: &PgPool, task_id: &str) -> AppResult<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT board FROM kanban_tasks WHERE id = $1")
            .bind(task_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|r| r.0))
}

/// Fetch a single kanban task row by id.
pub async fn get_kanban_task(pool: &PgPool, task_id: &str) -> AppResult<Option<KanbanTaskDb>> {
    let rows = sql_forge!(
        KanbanTaskDb,
        r#"
        SELECT id, title, body, status, priority, assignee, profile, template, toolset, archived, position, channel_id, plan,
               created_at, updated_at
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
    pub toolset: Option<String>,
    pub archived: Option<bool>,
    pub position: Option<i32>,
    pub channel_id: Option<String>,
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

/// Update a kanban task's `thread_status` (NULL | 'scheduled' | 'running').
/// This is the thread-status lifecycle side of the workflow engine: a re-run
/// thread is picked up by the omniagent loop only while the task's
/// thread_status is 'scheduled'; it flips to 'running' on pickup.
pub async fn update_kanban_task_thread_status(
    pool: &PgPool,
    task_id: &str,
    thread_status: &str,
) -> AppResult<()> {
    sql_forge!(
        "UPDATE kanban_tasks SET thread_status = :thread_status WHERE id = :task_id",
        ( :thread_status = thread_status, :task_id = task_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete kanban_history rows older than `before`. kanban_history has NO FK
/// to kanban_tasks (`kanban_task_id` is plain text), so it can be pruned
/// independently of the tasks themselves.
pub async fn delete_old_kanban_history(
    pool: &PgPool,
    before: chrono::DateTime<chrono::Utc>,
) -> AppResult<u64> {
    let result = sql_forge!(
        "DELETE FROM kanban_history WHERE created_at < :cutoff",
        ( :cutoff = before )
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------
// Workflow executions reset (executor/tester/reviewer = 0)
// ---------------------------------------------------------------------------

/// `workflow_state` key recording WHEN the task's workflow execution counters
/// were last reset (UTC, `YYYY-MM-DDTHH:MM:SS.ffffffZ`).
///
/// It is the baseline the task-detail counters use: every attempt older than
/// it belongs to a previous life of the task and is no longer counted, so a
/// reset is visible as 0 in the API/dashboard instead of only in the engine.
pub const EXECUTIONS_RESET_AT_KEY: &str = "executions_reset_at";

/// Pure helper: the `workflow_state` document AFTER a full executions reset.
///
/// Every per-step counter the workflow engine keeps in
/// `workflow_state.executions` is written back as an explicit ZERO
/// (`running` -> executor, `testing` -> tester, `review` -> reviewer): a reset
/// must be observable as 0 in the DB/API, not as a removed key. The reset
/// baseline (`executions_reset_at`) is recorded alongside, and every other key
/// of the document is preserved.
///
/// A `workflow_state` that is absent (or not a JSON object) yields a document
/// holding just the zeroed executions + the baseline.
pub fn cleared_workflow_state(
    state: Option<&serde_json::Value>,
    reset_at: &str,
) -> serde_json::Value {
    let mut obj = state
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    obj.insert(
        "executions".to_string(),
        serde_json::json!({ "running": 0, "testing": 0, "review": 0 }),
    );
    obj.insert(
        EXECUTIONS_RESET_AT_KEY.to_string(),
        serde_json::Value::String(reset_at.to_string()),
    );
    serde_json::Value::Object(obj)
}

/// Reset ALL workflow execution counters of one kanban task.
///
/// The engine's retry budget lives in `workflow_state.executions`
/// (`fail_thread::execution_count`: limit = `retries + 1` TOTAL executions),
/// so zeroing it hands the task a FULL retry budget again. Used by:
///
/// - `POST /kanban/tasks/{id}/workflow/executions/reset` (the dashboard
///   "Reset workflow execution" button), and
/// - the dispatcher, when a task is dispatched FROM `Todo`: a re-queued task
///   starts a fresh life instead of inheriting the exhausted counters of its
///   previous one (`kanban_dispatch::dispatch_todo_tasks`).
///
/// Returns `Ok(false)` when the task does not exist (nothing written); the
/// call is idempotent, so a repeated reset is harmless.
pub async fn reset_workflow_executions(pool: &PgPool, task_id: &str) -> AppResult<bool> {
    #[derive(sqlx::FromRow)]
    struct ResetRow {
        workflow_state: Option<serde_json::Value>,
        /// DB clock, in the same format as `threads.created_at`, so the
        /// baseline and the attempt timestamps share one clock. `Option`
        /// because the driver reports a computed column as nullable.
        now_utc: Option<String>,
    }

    let row = sql_forge!(
        ResetRow,
        // NOTE: the TO_CHAR format is built with CHR(58) instead of literal
        // colons: a ':' in the SQL text is read as a bind-parameter marker by
        // the `sql_forge!` macro (`:MI`, `:SS`). Same trick as the created_at
        // projections elsewhere in this crate.
        r#"
        SELECT
            workflow_state,
            TO_CHAR(NOW() AT TIME ZONE 'UTC',
                    'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"')
                AS now_utc
        FROM kanban_tasks
        WHERE id = :id
        "#,
        ( :id = task_id )
    )
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(false);
    };

    // The baseline must be a REAL timestamp: without it the counters would not
    // restart (the filter would treat it as "no baseline"). Cannot happen with
    // `NOW()`, so fail loudly instead of writing a useless reset.
    let reset_at = match row.now_utc.as_deref() {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => {
            return Err(err_str!(
                "cannot read the database clock for the executions reset of task '{}'",
                task_id
            ))
        }
    };

    let cleared = cleared_workflow_state(row.workflow_state.as_ref(), &reset_at);

    sql_forge!(
        r#"
        UPDATE kanban_tasks
        SET workflow_state = :state,
            updated_at = NOW()
        WHERE id = :id
        "#,
        ( :state = &cleared, :id = task_id )
    )
    .execute(pool)
    .await?;

    Ok(true)
}

#[cfg(test)]
mod executions_reset_tests {
    use super::*;

    #[test]
    fn cleared_workflow_state_zeroes_every_step_counter() {
        let state = serde_json::json!({
            "executions": {"running": 3, "testing": 2, "review": 5},
            "step_index": 4,
        });
        let cleared = cleared_workflow_state(Some(&state), "2026-09-24T15:20:31.123456Z");

        // The three engine counters come back as an EXPLICIT zero: the reset
        // must be observable in the DB row, not by an absent key.
        assert_eq!(cleared["executions"]["running"], 0);
        assert_eq!(cleared["executions"]["testing"], 0);
        assert_eq!(cleared["executions"]["review"], 0);
        // The baseline makes the (thread-derived) dashboard counters restart.
        assert_eq!(
            cleared[EXECUTIONS_RESET_AT_KEY],
            serde_json::json!("2026-09-24T15:20:31.123456Z")
        );
        // Unrelated workflow state survives.
        assert_eq!(cleared["step_index"], 4);
    }

    #[test]
    fn cleared_workflow_state_handles_absent_state_and_is_idempotent() {
        // No workflow_state at all (or a non-object one): still a valid reset.
        let cleared = cleared_workflow_state(None, "2026-09-24T15:20:31.123456Z");
        assert_eq!(cleared["executions"]["running"], 0);
        assert_eq!(cleared["executions"]["testing"], 0);
        assert_eq!(cleared["executions"]["review"], 0);

        let scalar = serde_json::json!("not-an-object");
        let cleared_scalar = cleared_workflow_state(Some(&scalar), "2026-09-24T15:20:31.123456Z");
        assert_eq!(cleared_scalar["executions"]["review"], 0);

        // Resetting an already reset document is a no-op (same baseline).
        let twice = cleared_workflow_state(Some(&cleared), "2026-09-24T15:20:31.123456Z");
        assert_eq!(cleared, twice);
    }

    #[test]
    fn cleared_workflow_state_keeps_other_baselines_in_place() {
        // A SECOND reset moves the baseline forward and keeps zeroing.
        let first = cleared_workflow_state(None, "2026-09-24T15:00:00.000000Z");
        let second = cleared_workflow_state(Some(&first), "2026-09-24T16:00:00.000000Z");
        assert_eq!(
            second[EXECUTIONS_RESET_AT_KEY],
            serde_json::json!("2026-09-24T16:00:00.000000Z")
        );
        assert_eq!(second["executions"]["running"], 0);
    }
}
