use crate::agent::config::AgentContext;
use crate::agent::helpers;
use crate::db::types::Message;

/// Send a completion reaction (emoji) to the user's platform for the
/// cause message. Uses the pre-configured emoji per final status.
pub async fn send_completion_reaction(
    cfg: &AgentContext,
    channel: &crate::db::types::Channel,
    cause_msg: &Message,
    final_status: &str,
) {
    let reaction_ext_id = if cause_msg.external_id.is_some() {
        cause_msg.external_id.clone()
    } else {
        crate::db::threads::get_cause_message(&cfg.pool, cause_msg.thread_id)
            .await
            .ok()
            .flatten()
            .and_then(|m| m.external_id)
    };
    if let Some(ref ext_id) = reaction_ext_id {
        if let Some(ref platform) = channel.platform {
            if let Some(ref resource) = channel.resource_identifier {
                let emoji = match final_status {
                    "completed" => ":white_check_mark:",
                    "failed" => ":x:",
                    "interrupted" => ":broken_heart:",
                    _ => ":o:",
                };
                helpers::enqueue_reaction(&cfg.ctx, platform, resource, ext_id, emoji).await;
            }
        }
    }
}
