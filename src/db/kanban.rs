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
/// GOAL STATE (omnidev task 4): when the target status is `blocked`, the
/// transition ALSO writes goal state - goal_phase='blocked' and, when the
/// task has no typed blocked code yet, a fallback code `unspecified` (an
/// existing typed code is preserved; the human message falls back to the
/// existing prose or ''). The goal CAS revision (goal_revision) is bumped on
/// every blocked transition. Transitions to any other status leave goal state
/// untouched.
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

    // 2. Update the status (and goal state when transitioning to blocked)
    sql_forge!(
        r#"
        UPDATE kanban_tasks SET
            status = :status,
            goal_phase = CASE WHEN :status = 'blocked' THEN 'blocked' ELSE goal_phase END,
            goal_blocked_code = CASE WHEN :status = 'blocked'
                THEN COALESCE(NULLIF(goal_blocked_code, ''), 'unspecified')
                ELSE goal_blocked_code END,
            goal_blocked_message = CASE WHEN :status = 'blocked'
                THEN COALESCE(goal_blocked_message, '')
                ELSE goal_blocked_message END,
            goal_revision = CASE WHEN :status = 'blocked' THEN goal_revision + 1 ELSE goal_revision END,
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

// ── Kanban Goal State (omnidev task 4) ──────────────────────────────────────
//
// Durable per-task goal state: phase (active/paused/blocked/complete), a
// stable machine-routable blocked code (kebab-case) + human message, an
// optional max-rounds cap, and a CAS revision counter. All mutations go
// through update_kanban_task_goal (CAS-guarded) or the blocked status
// transition in update_kanban_task_status. Goals are strictly per-task state
// - no thread/channel execution semantics are touched.

/// Valid goal phases (mirrors the DB CHECK constraint
/// chk_kanban_tasks_goal_phase and VALID_GOAL_PHASES in src/server/kanban.rs).
pub const VALID_GOAL_PHASES: &[&str] = &["active", "paused", "blocked", "complete"];

/// A goal-state mutation request: every field optional; omitted fields keep
/// the task's existing value. `expected_revision` is the CAS guard - when set,
/// the update only applies if the task's current goal_revision matches.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GoalPatch {
    pub phase: Option<String>,
    pub blocked_code: Option<String>,
    pub blocked_message: Option<String>,
    pub max_rounds: Option<i32>,
    pub expected_revision: Option<i32>,
}

/// Outcome of a CAS-guarded goal update.
#[derive(Debug, Clone)]
pub enum GoalUpdateResult {
    /// The update applied; `revision` is the new goal_revision.
    Updated { revision: i32 },
    /// CAS mismatch: the task's current goal_revision differs from the
    /// expected value, so nothing was written.
    Conflict { current_revision: i32 },
}

/// Sentinel for "keep existing value" for optional text fields (same pattern
/// as IGNORE_STR in src/server/kanban.rs). Distinct from an explicit empty
/// string, which clears the field (NULL).
const GOAL_IGNORE_STR: &str = "\u{10FFFF}__NO_UPDATE__\u{10FFFF}";

/// Current goal-state row of a kanban task (for the CAS read + history event).
#[derive(Clone, sqlx::FromRow)]
struct GoalRow {
    goal_phase: Option<String>,
    goal_blocked_code: Option<String>,
    goal_blocked_message: Option<String>,
    goal_max_rounds: Option<i32>,
    goal_revision: Option<i32>,
}

/// Apply a goal-state mutation to a kanban task, atomically, with CAS.
///
/// Single transaction: locks the task row, verifies the optional
/// `expected_revision` CAS guard (returns `Conflict` when it does not match -
/// the HTTP layer maps that to 409), applies the provided fields (omitted
/// fields keep their existing values), bumps `goal_revision`, and records the
/// mutation as a durable `goal` history event with the before/after values.
pub async fn update_kanban_task_goal(
    pool: &PgPool,
    task_id: &str,
    patch: &GoalPatch,
) -> AppResult<GoalUpdateResult> {
    use sqlx::Transaction;

    let mut tx: Transaction<'_, sqlx::Postgres> = pool.begin().await?;

    // 1. Lock the task row and read the current goal state.
    let row: Option<GoalRow> = sql_forge!(
        GoalRow,
        r#"
        SELECT goal_phase, goal_blocked_code, goal_blocked_message,
               goal_max_rounds, goal_revision
        FROM kanban_tasks WHERE id = :id FOR UPDATE
        "#,
        ( :id = task_id )
    )
    .fetch_optional(&mut *tx)
    .await?;

    let row = match row {
        Some(r) => r,
        None => {
            tx.rollback().await?;
            return Err(err_str!("Kanban task '{}' not found", task_id));
        }
    };
    let current_revision = row.goal_revision.unwrap_or(0);

    // 2. CAS guard: when the caller pinned a revision, it must match.
    if let Some(expected) = patch.expected_revision {
        if expected != current_revision {
            tx.rollback().await?;
            return Ok(GoalUpdateResult::Conflict { current_revision });
        }
    }

    // 3. Apply the mutation (omitted fields keep their existing values) and
    //    bump the revision.
    // max_rounds is bound as &str (sentinel pattern): sql_forge! duplicates
    // the binding expression per SQL placeholder, so an owned String would be
    // moved on first use (E0382).
    let max_rounds_str = patch
        .max_rounds
        .map(|v| v.to_string())
        .unwrap_or_else(|| GOAL_IGNORE_STR.to_string());
    sql_forge!(
        r#"
        UPDATE kanban_tasks SET
            goal_phase = CASE WHEN :phase = '' THEN goal_phase ELSE :phase END,
            goal_blocked_code = CASE WHEN :code = '' THEN goal_blocked_code
                ELSE NULLIF(:code, '')::text END,
            goal_blocked_message = CASE WHEN :msg = :ign THEN goal_blocked_message
                ELSE NULLIF(:msg, '')::text END,
            goal_max_rounds = CASE WHEN :max_rounds = :ign THEN goal_max_rounds
                ELSE NULLIF(:max_rounds, '')::int END,
            goal_revision = goal_revision + 1,
            updated_at = NOW()
        WHERE id = :id
        "#,
        ( :id = task_id,
          :phase = patch.phase.as_deref().unwrap_or(""),
          :code = patch.blocked_code.as_deref().unwrap_or(""),
          :msg = patch.blocked_message.as_deref().unwrap_or(GOAL_IGNORE_STR),
          :ign = GOAL_IGNORE_STR,
          :max_rounds = max_rounds_str.as_str(),
    )
    )
    .execute(&mut *tx)
    .await?;

    // 4. Durable goal/change event: history row with the before/after values.
    let prev = serde_json::json!({
        "goal_phase": row.goal_phase,
        "goal_blocked_code": row.goal_blocked_code,
        "goal_blocked_message": row.goal_blocked_message,
        "goal_max_rounds": row.goal_max_rounds,
        "goal_revision": row.goal_revision,
    });
    sql_forge!(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values)
        VALUES (:task_id, 'goal', NULL, NULL, :previous_values::jsonb)
        "#,
        ( :task_id = task_id, :previous_values = &prev )
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(GoalUpdateResult::Updated {
        revision: current_revision + 1,
    })
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
        SELECT id, title, body, status, priority, assignee, profile, template, archived, position, channel_id, plan,
               goal_phase, goal_blocked_code, goal_blocked_message, goal_max_rounds, goal_revision,
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
    pub archived: Option<bool>,
    pub position: Option<i32>,
    pub channel_id: Option<String>,
    pub plan: bool,
    pub goal_phase: Option<String>,
    pub goal_blocked_code: Option<String>,
    pub goal_blocked_message: Option<String>,
    pub goal_max_rounds: Option<i32>,
    pub goal_revision: Option<i32>,
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
