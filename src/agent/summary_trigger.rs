use crate::agent::config::AgentContext;
use crate::db::types::Thread;

/// Cancel any remaining background tasks for this thread; summary generation is
/// handled by the core response handler and is not a plugin contract.
pub async fn trigger_summary_and_cleanup(_cfg: &AgentContext, thread: &Thread) {
    // Cancel any remaining background tasks for this thread
    let registry = crate::agent::task_registry::TASK_REGISTRY.get().cloned();
    if let Some(reg) = registry {
        let count = reg.cancel_all_for_thread(thread.id).await;
        if count > 0 {
            tracing::info!(
                "Cancelled {} remaining background task(s) for thread {}",
                count,
                thread.id
            );
        }
    }
}
