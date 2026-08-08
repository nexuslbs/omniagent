use crate::agent::config::AgentContext;
use crate::agent::fail_thread::{engine_transition, RerunKind};
use crate::db::types as queries;
use crate::db::types::Thread;
use crate::workflows::WorkflowsFile;
use sql_forge::sql_forge;

/// If the thread is linked to a kanban task, update its status based on
/// the thread's final outcome.
///
/// Phase 3: failed / interrupted / skipped terminals go through the atomic
/// engine transition (re-run with retry guard, or blocked).
///
/// Phase 4 (reviewer/tester decisions — spec §3 rows 7-17):
/// - reviewer success (`review` step, clean completion) → `done` (R12)
/// - reviewer half-finished (completed with a failed final tool result) → `blocked`
/// - tester success (`testing` step, clean completion) → `review`; a reviewer
///   role in the workflow gets a scheduled review thread (row 7), otherwise
///   the task waits for manual review (no thread)
/// - tester failure (`testing` step, any error — D5) → executor step: task
///   `running` + scheduled executor thread (consumes the executor retry
///   budget; the guard blocks the task at the limit — rows 8/9)
pub async fn update_kanban_status(cfg: &AgentContext, thread: &Thread, final_status: &str) {
    let Some(ref task_id) = thread.task_id else {
        return;
    };

    // Phase 3: non-success terminals → atomic engine transition.
    if matches!(final_status, "failed" | "interrupted" | "skipped") {
        let kind = match final_status {
            "interrupted" => RerunKind::Interrupted,
            "skipped" => RerunKind::Skipped,
            _ => RerunKind::Failed,
        };
        match engine_transition(&cfg.pool, &cfg.ctx.data_dir, thread, kind).await {
            Ok(Some(new_id)) => {
                tracing::info!(
                    "[workflow] thread #{} terminal '{}': created re-run thread #{}",
                    thread.id,
                    final_status,
                    new_id
                );
            }
            Ok(None) => {
                tracing::info!(
                    "[workflow] thread #{} terminal '{}': blocked or no transition",
                    thread.id,
                    final_status
                );
            }
            Err(e) => {
                tracing::warn!(
                    "[workflow] thread #{} terminal '{}': engine transition failed ({e}); falling back to blocked",
                    thread.id,
                    final_status
                );
                if let Err(e2) =
                    queries::update_kanban_task_status(&cfg.pool, task_id, "blocked").await
                {
                    tracing::warn!(
                        "[workflow] failed to block kanban task {}: {:?}",
                        task_id,
                        e2
                    );
                }
            }
        }
        return;
    }

    if final_status != "completed" {
        return;
    }

    let errored = last_tool_result_errored(&cfg.pool, thread.id).await;
    let step = thread.workflow_step.as_deref().unwrap_or("");
    let wf_id = thread_workflow_id(&cfg.pool, thread.id).await;
    let has_reviewer = workflow_has_role(&cfg.ctx.data_dir, &wf_id, "reviewer");

    let has_tester = workflow_has_role(&cfg.ctx.data_dir, &wf_id, "tester");
    match route_completed_thread(step, errored, has_reviewer, has_tester) {
        // R12: reviewer approves via normal completion + summary → done.
        CompletedRoute::Done => {
            let comment = format!("Reviewer approved (thread #{}). Task done.", thread.id);
            match transition_with_comment(&cfg.pool, task_id, "done", None, &comment).await {
                Ok(()) => tracing::info!(
                    "[workflow] reviewer approved (thread #{}) → task {} done",
                    thread.id,
                    task_id
                ),
                Err(e) => tracing::warn!("[workflow] failed to mark task {} done: {}", task_id, e),
            }
        }
        // Inconclusive review — the final tool result errored, so there is no
        // clean approve signal (R12); block for manual intervention.
        CompletedRoute::BlockedInconclusiveReview => {
            let comment = format!(
                "Task blocked: review thread #{} completed with a failed tool result (inconclusive). Manual review required.",
                thread.id
            );
            match transition_with_comment(&cfg.pool, task_id, "blocked", None, &comment).await {
                Ok(()) => tracing::info!(
                    "[workflow] inconclusive review (thread #{}) → task {} blocked",
                    thread.id,
                    task_id
                ),
                Err(e) => tracing::warn!("[workflow] failed to block task {}: {}", task_id, e),
            }
        }
        // Row 7: tester pass → review step, with a scheduled review thread.
        CompletedRoute::ReviewWithThread => match create_review_thread(&cfg.pool, thread).await {
            Ok(Some(new_id)) => {
                let comment = format!(
                    "Tester passed (thread #{}). Task in review — review thread #{new_id}.",
                    thread.id
                );
                match transition_with_comment(
                    &cfg.pool,
                    task_id,
                    "review",
                    Some("scheduled"),
                    &comment,
                )
                .await
                {
                    Ok(()) => tracing::info!(
                        "[workflow] tester passed (thread #{}) → task {} in review (review thread #{})",
                        thread.id,
                        task_id,
                        new_id
                    ),
                    Err(e) => tracing::warn!(
                        "[workflow] failed to move task {} to review: {}",
                        task_id,
                        e
                    ),
                }
            }
            _ => {
                let comment = format!(
                    "Tester passed (thread #{}). Task in review — review thread creation failed, manual review required.",
                    thread.id
                );
                match transition_with_comment(&cfg.pool, task_id, "review", None, &comment).await {
                    Ok(()) => tracing::warn!(
                        "[workflow] tester passed (thread #{}) but review thread creation failed; task {} in review (manual)",
                        thread.id,
                        task_id
                    ),
                    Err(e) => tracing::warn!(
                        "[workflow] failed to move task {} to review: {}",
                        task_id,
                        e
                    ),
                }
            }
        },
        // R7-D4: executor success with a tester role → testing step thread, task → 'testing'.
        CompletedRoute::TestingWithThread => match create_testing_thread(&cfg.pool, thread).await {
            Ok(Some(new_id)) => {
                let comment = format!(
                    "Executor done (thread #{}). Task in testing — tester step thread #{new_id}.",
                    thread.id
                );
                match transition_with_comment(
                    &cfg.pool,
                    task_id,
                    "testing",
                    Some("scheduled"),
                    &comment,
                )
                .await
                {
                    Ok(()) => tracing::info!(
                        "[workflow] executor done (thread #{}) -> task {} in testing (tester thread #{})",
                        thread.id,
                        task_id,
                        new_id
                    ),
                    Err(e) => tracing::warn!(
                        "[workflow] failed to move task {} to testing: {}",
                        task_id,
                        e
                    ),
                }
            }
            _ => {
                let comment = format!(
                    "Executor done (thread #{}). Task in review — tester thread creation failed, manual review required.",
                    thread.id
                );
                match transition_with_comment(&cfg.pool, task_id, "review", None, &comment).await {
                    Ok(()) => tracing::warn!(
                        "[workflow] executor done (thread #{}) but tester thread creation failed; task {} in review (manual)",
                        thread.id,
                        task_id
                    ),
                    Err(e) => tracing::warn!(
                        "[workflow] failed to move task {} to review: {}",
                        task_id,
                        e
                    ),
                }
            }
        },
        // Row 7: tester pass, no reviewer role → manual review (no thread).
        CompletedRoute::ReviewManual => {
            let comment = format!(
                "Tester passed (thread #{}). Task in review (manual review — no reviewer role).",
                thread.id
            );
            match transition_with_comment(&cfg.pool, task_id, "review", None, &comment).await {
                Ok(()) => tracing::info!(
                    "[workflow] tester passed (thread #{}) → task {} in review (manual)",
                    thread.id,
                    task_id
                ),
                Err(e) => tracing::warn!(
                    "[workflow] failed to move task {} to review: {}",
                    task_id,
                    e
                ),
            }
        }
        // D5: any test error → executor step (running + scheduled executor thread).
        CompletedRoute::TesterErrorToExecutor => {
            match engine_transition(&cfg.pool, &cfg.ctx.data_dir, thread, RerunKind::Failed).await {
                Ok(Some(new_id)) => tracing::info!(
                    "[workflow] tester error (thread #{}) → executor re-run thread #{}",
                    thread.id,
                    new_id
                ),
                Ok(None) => {
                    tracing::info!("[workflow] tester error (thread #{}) → blocked", thread.id)
                }
                Err(e) => {
                    tracing::warn!(
                        "[workflow] tester error transition failed ({e}); falling back to blocked"
                    );
                    if let Err(e2) =
                        queries::update_kanban_task_status(&cfg.pool, task_id, "blocked").await
                    {
                        tracing::warn!(
                            "[workflow] failed to block kanban task {}: {:?}",
                            task_id,
                            e2
                        );
                    }
                }
            }
        }
        // Half-finished executor run — cannot promote to review.
        CompletedRoute::BlockedHalfFinished => {
            if let Err(e) = queries::update_kanban_task_status(&cfg.pool, task_id, "blocked").await
            {
                tracing::warn!(
                    "[executor] Failed to update kanban task {} status: {:?}",
                    task_id,
                    e
                );
            }
        }
    }
}

/// Phase 4 decision for a completed thread (pure — unit-tested).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletedRoute {
    /// Reviewer approved via clean normal completion (R12) → task done.
    Done,
    /// Review thread finished but the final tool result errored → blocked.
    BlockedInconclusiveReview,
    /// Tester passed + reviewer role → review step with a scheduled review thread (row 7).
    ReviewWithThread,
    /// Tester passed, no reviewer role → manual review, no thread (row 7).
    ReviewManual,
    /// Tester error (D5) → executor step (running + scheduled executor thread).
    TesterErrorToExecutor,
    /// Executor half-finished → blocked.
    BlockedHalfFinished,
    /// Executor success with a tester role -> spawn the testing step thread; task -> 'testing' (R7-D4).
    TestingWithThread,
}

/// Map a completed thread's (workflow step, errored flag, reviewer role) to
/// the kanban decision (spec §3 rows 7-17, R12, D5).
pub(crate) fn route_completed_thread(
    step: &str,
    errored: bool,
    has_reviewer: bool,
    has_tester: bool,
) -> CompletedRoute {
    match (step, errored) {
        ("review", false) => CompletedRoute::Done,
        ("review", true) => CompletedRoute::BlockedInconclusiveReview,
        ("testing", false) if has_reviewer => CompletedRoute::ReviewWithThread,
        ("testing", false) => CompletedRoute::ReviewManual,
        ("testing", true) => CompletedRoute::TesterErrorToExecutor,
        ("running", false) if has_tester => CompletedRoute::TestingWithThread,
        (_, true) => CompletedRoute::BlockedHalfFinished,
        _ => CompletedRoute::ReviewManual,
    }
}

/// Move the task to `to` and record a workflow history entry with `comment`
/// (D3: transitions persist a comment).
pub(crate) async fn transition_with_comment(
    pool: &sqlx::PgPool,
    task_id: &str,
    to: &str,
    thread_status: Option<&str>,
    comment: &str,
) -> Result<(), String> {
    let from: Option<String> =
        sqlx::query_scalar::<_, String>("SELECT status FROM kanban_tasks WHERE id = $1")
            .bind(task_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("fetch task status: {e}"))?;
    let from = from.unwrap_or_else(|| to.to_string());

    sqlx::query("UPDATE kanban_tasks SET status = $1, thread_status = $2 WHERE id = $3")
        .bind(to)
        .bind(thread_status)
        .bind(task_id)
        .execute(pool)
        .await
        .map_err(|e| format!("transition task: {e}"))?;

    sql_forge!(
        "INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
         VALUES (:id, 'workflow', :from, :to, :comment)",
        (:id = task_id, :from = from, :to = to, :comment = comment)
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(())
}

/// Create a scheduled `review` thread for a passed tester thread (row 7).
/// Mirrors the thread creation in `engine_transition` (cause message seq-0).
async fn create_review_thread(pool: &sqlx::PgPool, thread: &Thread) -> Result<Option<i64>, String> {
    #[derive(sqlx::FromRow)]
    struct IdRow {
        id: i64,
    }
    let cause = thread.cause.clone();
    let wf_id = thread_workflow_id(pool, thread.id)
        .await
        .unwrap_or_default();
    let new_id = sqlx::query_as::<_, IdRow>(
        "INSERT INTO threads (status, cause, channel_id, profile, provider, model,
         task_id, parent_id, workflow_id, workflow_step, task_type)
         VALUES ('pending', $1, $2, $3, $4, $5,
         $6, $7, $8, 'review', 'kanban') RETURNING id",
    )
    .bind(cause.as_str())
    .bind(thread.channel_id)
    .bind(thread.profile.as_str())
    .bind(thread.provider.clone().unwrap_or_default())
    .bind(thread.model.clone().unwrap_or_default())
    .bind(thread.task_id.clone().unwrap_or_default())
    .bind(thread.id)
    .bind(wf_id)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("create review thread: {e}"))?;

    sqlx::query(
        "INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
         VALUES ($1, 'cause', $2, 0, 'cause')",
    )
    .bind(new_id.id)
    .bind(cause)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(Some(new_id.id))
}

/// Create a scheduled `testing` step thread for a completed executor thread (R7-D4,
/// spec §5). Mirrors `create_review_thread` with workflow_step='testing' and
/// task_type='kanban' so the workflow test harness can find it.
async fn create_testing_thread(
    pool: &sqlx::PgPool,
    thread: &Thread,
) -> Result<Option<i64>, String> {
    #[derive(sqlx::FromRow)]
    struct IdRow {
        id: i64,
    }
    let task_id = match thread.task_id.clone() {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(None),
    };
    let cause = format!("Workflow step: testing. Task: {task_id}");
    let wf_id = thread_workflow_id(pool, thread.id)
        .await
        .unwrap_or_default();
    let new_id = sqlx::query_as::<_, IdRow>(
        "INSERT INTO threads (status, cause, channel_id, profile, provider, model,
         task_id, parent_id, workflow_id, workflow_step, task_type)
         VALUES ('pending', $1, $2, $3, $4, $5,
         $6, $7, $8, 'testing', 'kanban') RETURNING id",
    )
    .bind(cause.as_str())
    .bind(thread.channel_id)
    .bind(thread.profile.as_str())
    .bind(thread.provider.clone().unwrap_or_default())
    .bind(thread.model.clone().unwrap_or_default())
    .bind(task_id)
    .bind(thread.id)
    .bind(wf_id)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("create testing thread: {e}"))?;

    sqlx::query(
        "INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
         VALUES ($1, 'cause', $2, 0, 'cause')",
    )
    .bind(new_id.id)
    .bind(cause)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(Some(new_id.id))
}

/// True when the workflow referenced by the thread declares `role`.
/// Fetch the workflow_id of a thread (the `Thread` struct does not carry it).
async fn thread_workflow_id(pool: &sqlx::PgPool, thread_id: i64) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT workflow_id FROM threads WHERE id = $1")
        .bind(thread_id)
        .fetch_one(pool)
        .await
        .ok()
        .flatten()
}

fn workflow_has_role(data_dir: &str, workflow_id: &Option<String>, role: &str) -> bool {
    let Some(wf_id) = workflow_id else {
        return false;
    };
    let Ok(wfs) = WorkflowsFile::load(&std::path::PathBuf::from(format!(
        "{data_dir}/workflows.yml"
    ))) else {
        return false;
    };
    wfs.workflows
        .get(wf_id.as_str())
        .map(|wf| wf.roles.contains_key(role))
        .unwrap_or(false)
}

/// Returns true if the thread's LAST tool-result message had
/// `metadata.is_error = true` (the final operation of the thread failed).
async fn last_tool_result_errored(pool: &sqlx::PgPool, thread_id: i64) -> bool {
    let is_err: Option<String> = sql_forge!(
        scalar String,
        r#"
        SELECT COALESCE(metadata->>'is_error', 'false')
        FROM messages
        WHERE thread_id = :thread_id AND msg_type = 'tool-result'
        ORDER BY id DESC
        LIMIT 1
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    is_err.as_deref() == Some("true")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_completed_thread_reviewer_approve_goes_done() {
        // R12: reviewer approves via normal completion (clean) → done,
        // regardless of whether a reviewer role is declared (manual state).
        assert_eq!(
            route_completed_thread("review", false, true, false),
            CompletedRoute::Done
        );
        assert_eq!(
            route_completed_thread("review", false, false, false),
            CompletedRoute::Done
        );
    }

    #[test]
    fn route_completed_thread_reviewer_half_finished_blocks() {
        // A review that completed with a failed final tool result is
        // inconclusive — it cannot count as an approve (R12).
        assert_eq!(
            route_completed_thread("review", true, true, false),
            CompletedRoute::BlockedInconclusiveReview
        );
    }

    #[test]
    fn route_completed_thread_tester_pass_goes_review() {
        // Row 7: tester pass → review step (scheduled review thread when a
        // reviewer role exists, otherwise manual review with no thread).
        assert_eq!(
            route_completed_thread("testing", false, true, false),
            CompletedRoute::ReviewWithThread
        );
        assert_eq!(
            route_completed_thread("testing", false, false, false),
            CompletedRoute::ReviewManual
        );
    }

    #[test]
    fn route_completed_thread_tester_error_goes_executor() {
        // D5: any test error → executor step (running + scheduled thread).
        assert_eq!(
            route_completed_thread("testing", true, false, false),
            CompletedRoute::TesterErrorToExecutor
        );
    }

    #[test]
    fn route_completed_thread_executor_paths() {
        // Executor success without a tester role → review (manual); half-finished
        // executor runs are blocked.
        assert_eq!(
            route_completed_thread("running", false, false, false),
            CompletedRoute::ReviewManual
        );
        assert_eq!(
            route_completed_thread("", false, false, false),
            CompletedRoute::ReviewManual
        );
        assert_eq!(
            route_completed_thread("running", true, false, false),
            CompletedRoute::BlockedHalfFinished
        );
    }
    #[test]
    fn testing_step_route_r7d4() {
        // Executor success with a tester role → testing step thread; without → manual review.
        assert_eq!(
            route_completed_thread("running", false, false, true),
            CompletedRoute::TestingWithThread
        );
        assert_eq!(
            route_completed_thread("running", false, true, true),
            CompletedRoute::TestingWithThread
        );
        assert_eq!(
            route_completed_thread("running", false, false, false),
            CompletedRoute::ReviewManual
        );
    }
}
