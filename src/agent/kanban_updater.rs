use crate::agent::config::AgentContext;
use crate::agent::fail_thread::{engine_transition, RerunKind};
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
        // Phase 3: failed / interrupted / skipped terminals go through the
        // atomic engine transition (re-run with retry guard, or blocked).
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
