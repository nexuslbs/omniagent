use crate::agent::config::AgentContext;
use crate::db::types::Thread;
use crate::mcp::McpToolCall;

/// Trigger a cross-thread summary check via the memory plugin and cancel
/// any remaining background tasks for this thread.
pub async fn trigger_summary_and_cleanup(cfg: &AgentContext, thread: &Thread) {
    // Trigger cross-thread summary check
    let mcp_call = McpToolCall {
        name: "prompt_summarize".to_string(),
        arguments: serde_json::json!({
            "thread_id": thread.id,
        }),
        id: String::new(),
    };
    match cfg.plugin_manager.snapshot_registry().await.execute(&mcp_call, cfg.ctx.clone()).await
    {
        Ok(_) => {}
        Err(e) => {
            tracing::debug!("[executor] Post-thread summary failed (non-critical): {:?}", e);
        }
    }

    // Cancel any remaining background tasks for this thread
    let registry = crate::agent::task_registry::TASK_REGISTRY
        .get()
        .cloned();
    if let Some(reg) = registry {
        let count = reg.cancel_all_for_thread(thread.id).await;
        if count > 0 {
            tracing::info!("Cancelled {} remaining background task(s) for thread {}", count, thread.id);
        }
    }
}
