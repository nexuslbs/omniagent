use crate::agent::helpers;
use crate::db::types as queries;
use crate::db::types::{CompleteThreadStats, Message, MessageNew, Thread};
use crate::error::AppResult;
use crate::agent::config::AgentContext;

/// Create an error message, mark the thread as failed, deliver the error
/// back to the user's platform, and return the saved message.
///
/// Used by all validation-failure paths in process_thread.
pub(crate) async fn fail_thread(
    cfg: &AgentContext,
    thread: &Thread,
    cause_msg: &Message,
    next_seq: &mut i32,
    content: String,
    subtype: &str,
) -> AppResult<Message> {
    let seq = *next_seq;
    *next_seq += 1;

    let err_msg = MessageNew {
        thread_id: thread.id,
        role: "system".to_string(),
        content,
        thread_sequence: seq,
        external_id: Some(format!(
            "validation-error:{}:{}",
            thread.id,
            chrono::Utc::now().timestamp()
        )),
        metadata: serde_json::json!({
            "error_type": "configuration",
        }),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "error".to_string(),
        msg_subtype: Some(subtype.to_string()),
        iteration_number: 0,
        duration_ms: 0,
        token_usage: serde_json::json!({}),
    };

    let saved = queries::create_message(&cfg.pool, &err_msg).await?;

    if let Err(e) = queries::complete_thread(
        &cfg.pool,
        thread.id,
        "failed",
        CompleteThreadStats {
            input_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            duration_ms: 0,
        },
    )
    .await
    {
        tracing::warn!(
            "[executor] Failed to mark thread {} failed ({}): {:?}",
            thread.id,
            subtype,
            e
        );
    }

    // Deliver the error message back to the user's platform
    if let Ok(Some(channel)) = queries::get_channel_by_id(&cfg.pool, thread.channel_id).await {
        helpers::enqueue_delivery(
            &cfg.ctx,
            &saved,
            &channel,
            thread,
            cause_msg.external_id.clone(),
        )
        .await;

        // Send failure reaction (:x:) on the cause message
        if let Some(ref platform) = channel.platform {
            if let Some(ref resource) = channel.resource_identifier {
                if let Some(ref ext_id) = cause_msg.external_id {
                    helpers::enqueue_reaction(&cfg.ctx, platform, resource, ext_id, ":x:").await;
                }
            }
        }
    }

    Ok(saved)
}
