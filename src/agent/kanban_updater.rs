use crate::agent::config::AgentContext;
use crate::db::types as queries;
use crate::db::types::Thread;
use sql_forge::sql_forge;

/// If the thread is linked to a kanban task, update its status based on
/// the thread's final outcome.
///
/// `completed` → `review`, BUT only if the thread's final tool result was not
/// an error. If the last tool call failed (e.g. a git push was rejected), the
/// deliverable is NOT done — the task maps to `blocked` instead of `review`
/// so a half-finished task can't silently land in Review.
///
/// Any other terminal status (`failed`, `interrupted`, `skipped`) → `blocked`.
pub async fn update_kanban_status(cfg: &AgentContext, thread: &Thread, final_status: &str) {
    if let Some(ref task_id) = thread.task_id {
        let kanban_status = if final_status == "completed" {
            // A thread can end "completed" even when its final tool call
            // errored (the agent reports the failure in its summary but the
            // loop still terminates normally). Only promote to review when the
            // last tool result was clean.
            if last_tool_result_errored(&cfg.pool, thread.id).await {
                "blocked"
            } else {
                "review"
            }
        } else {
            "blocked"
        };
        if let Err(e) = queries::update_kanban_task_status(&cfg.pool, task_id, kanban_status).await
        {
            tracing::warn!(
                "[executor] Failed to update kanban task {} status: {:?}",
                task_id,
                e
            );
        }
    }
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
