//! Shared 3-message action-thread flow (kanban role actions, cron/schedule
//! tasks with mode=action, hook tasks with mode=action).
//!
//! Every action-task run produces the same thread contract:
//! - seq-0: cause message (msg_type = cron|kanban|hook, content = the action
//!   name) - created by the caller via `create_thread_with_cause`;
//! - seq-1: msg_type='action', msg_subtype = the action_id, content = the
//!   FULL action output/logs, duration_ms = the real wall-clock action time;
//! - seq-2: msg_type='summary', short summary carrying the real duration,
//!   e.g. `Action 'cron-daily-backup' completed successfully in 5123ms.`
//!
//! The three messages are delivered to the platform exactly like agentic
//! thread messages: seq-0 is posted first (creating the root post, whose real
//! platform id is saved back), seq-1 and seq-2 reply in the thread, the
//! terminal status reaction is added to the cause post, and platforms with
//! first/last-only collapse suppress seq-1 exactly like they suppress
//! intermediate agentic messages (the summary, is_final=true, is always
//! delivered).

use std::time::{Duration, Instant};

use serde_json::json;
use sqlx::PgPool;
use tracing::warn;

use crate::agent::helpers;
use crate::db::types as queries;
use crate::db::types::{Channel, Message, Thread};
use crate::error::AppResult;
use crate::mcp::AppContext;

/// Outcome spec of one action-task run: the inputs shared by every message
/// of the 3-message thread.
pub struct ActionRunSpec<'a> {
    /// Human-readable action/schedule name (used in the summary text).
    pub name: &'a str,
    /// actions.yml action id (seq-1 msg_subtype).
    pub action_id: &'a str,
    /// Full action output/logs (seq-1 content).
    pub output: &'a str,
    /// Real wall-clock action time in milliseconds.
    pub duration_ms: i64,
    pub is_error: bool,
}

/// seq-1 content: the FULL action output/logs, prefixed with a line naming
/// the action and its outcome (a success states it explicitly; an error
/// carries the ACTUAL error text, never a generic "run failed").
pub fn action_message_content(spec: &ActionRunSpec<'_>) -> String {
    let detail = spec.output.trim();
    if spec.is_error {
        if detail.is_empty() {
            format!(
                "Action '{}' FAILED (no error text was returned).",
                spec.name
            )
        } else {
            format!("Action '{}' FAILED: {}", spec.name, detail)
        }
    } else if detail.is_empty() {
        format!("Action '{}' completed successfully.", spec.name)
    } else {
        format!(
            "Action '{}' completed successfully.\n\n{}",
            spec.name, detail
        )
    }
}

/// seq-2 content: the SHORT summary with the real action time, e.g.
/// `Action 'cron-daily-backup' completed successfully in 5123ms.`
pub fn action_summary_content(spec: &ActionRunSpec<'_>) -> String {
    if spec.is_error {
        format!("Action '{}' failed in {}ms.", spec.name, spec.duration_ms)
    } else {
        format!(
            "Action '{}' completed successfully in {}ms.",
            spec.name, spec.duration_ms
        )
    }
}

/// Persist the seq-1 (action, full logs) and seq-2 (summary) messages of an
/// action thread. Returns the two saved messages (for platform delivery).
///
/// `seq1_external_id` is the caller-provided unique id of the action message
/// (the summary message carries no external id, mirroring agentic summaries).
/// `metadata` is merged into the action message's metadata (the summary keeps
/// the minimal `is_error`/`action_id` keys).
pub async fn persist_action_messages(
    pool: &PgPool,
    thread: &Thread,
    spec: &ActionRunSpec<'_>,
    seq1_external_id: String,
    metadata: serde_json::Value,
) -> AppResult<(Message, Message)> {
    let action_msg = queries::MessageNew {
        thread_id: thread.id,
        role: "agent".to_string(),
        content: action_message_content(spec),
        thread_sequence: 1,
        external_id: Some(seq1_external_id),
        metadata,
        embedding: None,
        summary_text: None,
        is_summary: false,
        original_thread_id: None,
        msg_type: "action".to_string(),
        msg_subtype: Some(spec.action_id.to_string()),
        // Documented 0: action threads make NO LLM call at all, so
        // `threads.iterations` stays 0 and 0 is their current iteration.
        iteration_number: 0,
        duration_ms: spec.duration_ms as i32,
        token_usage: json!({}),
    };
    let action_saved = queries::create_message(pool, &action_msg).await?;

    let summary_msg = queries::MessageNew {
        thread_id: thread.id,
        role: "agent".to_string(),
        content: action_summary_content(spec),
        thread_sequence: 2,
        external_id: None,
        metadata: json!({
            "is_error": spec.is_error,
            "action_id": spec.action_id,
        }),
        embedding: None,
        summary_text: None,
        is_summary: true,
        original_thread_id: None,
        msg_type: "summary".to_string(),
        msg_subtype: Some(spec.action_id.to_string()),
        // Documented 0: see the action message above - non-agentic action thread.
        iteration_number: 0,
        duration_ms: spec.duration_ms as i32,
        token_usage: json!({}),
    };
    let summary_saved = queries::create_message(pool, &summary_msg).await?;

    Ok((action_saved, summary_saved))
}

/// Deliver the 3 messages of an action thread to its platform, reusing the
/// agentic delivery path (`helpers::enqueue_delivery`):
/// 1. seq-0 cause is posted first (creates the root post; the platform's
///    save-back writes the real post id),
/// 2. seq-1 (action, full logs) and seq-2 (summary, final) reply in the
///    thread,
/// 3. the terminal status reaction (completed/failed) is added to the cause
///    post, exactly like agentic threads.
///
/// Platforms with first/last-only collapse suppress seq-1 like any
/// intermediate agentic message; the summary (is_final=true) is always
/// delivered. No-op when the channel has no platform, no resource, or no
/// registered sender.
#[allow(clippy::too_many_arguments)]
pub async fn deliver_action_thread(
    ctx: &AppContext,
    pool: &PgPool,
    thread: &Thread,
    cause_msg: &Message,
    channel: Option<&Channel>,
    action_msg: &Message,
    summary_msg: &Message,
    is_error: bool,
) {
    let Some(channel) = channel else {
        return;
    };
    let Some(platform) = channel.platform.clone() else {
        return;
    };
    let Some(resource) = channel.resource_identifier.clone() else {
        return;
    };
    if !ctx.platform_senders.read().await.contains_key(&platform) {
        return;
    }

    // 1. seq-0: post the cause (root post) when it has no real platform id
    // yet (absent or synthetic cron:/hook:/kanban-action:).
    if cause_msg
        .external_id
        .as_deref()
        .is_none_or(helpers::is_synthetic_external_id)
    {
        helpers::enqueue_delivery(ctx, cause_msg, channel, thread, None, false).await;
    }

    // 2. Wait (bounded) for the platform's save-back to write the real post
    // id of the cause: seq-1/seq-2 and the reaction must reply under the
    // posted seq-0, and the save-back is asynchronous.
    let real_cause_id = await_real_cause_external_id(pool, thread.id).await;

    // 3. seq-1 (action, full logs) + seq-2 (summary, final).
    helpers::enqueue_delivery(
        ctx,
        action_msg,
        channel,
        thread,
        real_cause_id.clone(),
        false,
    )
    .await;
    helpers::enqueue_delivery(
        ctx,
        summary_msg,
        channel,
        thread,
        real_cause_id.clone(),
        true,
    )
    .await;

    // 4. Terminal status reaction on the cause post (completed/failed),
    // mirroring agentic thread finalization.
    if let Some(id) = real_cause_id {
        let status = if is_error { "failed" } else { "completed" };
        helpers::enqueue_reaction(ctx, &platform, &resource, &id, status).await;
    } else {
        warn!(
            "[action-flow] No real platform post id for thread {} cause: reaction and reply deliveries may be skipped",
            thread.id
        );
    }
}

/// Wait (bounded) until the thread's cause message carries a REAL platform
/// external id - written back by the platform plugin after it posted the
/// seq-0 message. Returns `None` when the platform never posted within the
/// budget: callers then deliver without a reply target and the platform
/// skips, exactly like any undeliverable system message.
async fn await_real_cause_external_id(pool: &PgPool, thread_id: i64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_millis(3000);
    loop {
        let id = crate::db::threads::get_cause_message(pool, thread_id)
            .await
            .ok()
            .flatten()
            .and_then(|m| {
                m.external_id
                    .filter(|id| !helpers::is_synthetic_external_id(id))
            });
        if id.is_some() {
            return id;
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(
        name: &'a str,
        action_id: &'a str,
        output: &'a str,
        duration_ms: i64,
        is_error: bool,
    ) -> ActionRunSpec<'a> {
        ActionRunSpec {
            name,
            action_id,
            output,
            duration_ms,
            is_error,
        }
    }

    #[test]
    fn action_message_content_success_with_logs() {
        let s = spec(
            "cron-daily-backup",
            "backup",
            "[backup] done\n[backup] ok",
            5123,
            false,
        );
        assert_eq!(
            action_message_content(&s),
            "Action 'cron-daily-backup' completed successfully.\n\n[backup] done\n[backup] ok"
        );
    }

    #[test]
    fn action_message_content_success_empty_output() {
        let s = spec("cron-daily-backup", "backup", "  ", 5123, false);
        assert_eq!(
            action_message_content(&s),
            "Action 'cron-daily-backup' completed successfully."
        );
    }

    #[test]
    fn action_message_content_error_carries_error_text() {
        let s = spec("cron-daily-backup", "backup", "boom: disk full", 5123, true);
        assert_eq!(
            action_message_content(&s),
            "Action 'cron-daily-backup' FAILED: boom: disk full"
        );
    }

    #[test]
    fn action_message_content_error_empty_text() {
        let s = spec("cron-daily-backup", "backup", "", 5123, true);
        assert_eq!(
            action_message_content(&s),
            "Action 'cron-daily-backup' FAILED (no error text was returned)."
        );
    }

    #[test]
    fn action_summary_content_success_has_real_duration() {
        let s = spec("cron-daily-backup", "backup", "logs", 5123, false);
        assert_eq!(
            action_summary_content(&s),
            "Action 'cron-daily-backup' completed successfully in 5123ms."
        );
    }

    #[test]
    fn action_summary_content_error_has_real_duration() {
        let s = spec("cron-daily-backup", "backup", "logs", 5123, true);
        assert_eq!(
            action_summary_content(&s),
            "Action 'cron-daily-backup' failed in 5123ms."
        );
    }
}
