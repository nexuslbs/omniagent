use crate::agent::config::AgentContext;
use crate::db::types::Thread;
use crate::db::types as queries;

/// If the thread is linked to a kanban task, update its status based on
/// the thread's final outcome.
pub async fn update_kanban_status(cfg: &AgentContext, thread: &Thread, final_status: &str) {
    if let Some(ref task_id) = thread.task_id {
        let cfg_snap = cfg.config_snapshot();
        let kanban_status = if final_status == "completed" {
            &cfg_snap.kanban_completed_status
        } else {
            &cfg_snap.kanban_failed_status
        };
        if let Err(e) = queries::update_kanban_task_status(&cfg.pool, task_id, kanban_status).await {
            tracing::warn!("[executor] Failed to update kanban task {} status: {:?}", task_id, e);
        }
    }
}
