use crate::agent::config::AgentContext;
use crate::agent::helpers;
use crate::db::types as queries;
use crate::db::types::{CompleteThreadStats, Message, MessageNew, Thread};
use crate::error::AppResult;
use crate::llm::{ChatMessage, CompletionRequest, LLMClient, Usage};
use tracing::{info, warn};

pub(crate) async fn handle_response(
    cfg: &AgentContext,
    thread: &Thread,
    cause_msg: &Message,
    channel: &crate::db::types::Channel,
    next_seq: i32,
    start_time: std::time::Instant,
    messages: &[ChatMessage],
    cumulative_usage: &mut Option<Usage>,
    force_failed: &mut bool,
    limit_reached: bool,
    current_iter: i32,
    iter_limit: i32,
    per_thread_llm: &LLMClient,
    final_content: String,
    token_usage_json: Option<serde_json::Value>,
    evidence_metadata: serde_json::Value,
    enable_subtasks: bool,
) -> AppResult<Message> {
    let agent_elapsed_ms = start_time.elapsed().as_millis() as i32;
    let is_empty_response = final_content.trim().is_empty();

    let saved = if limit_reached {
        // ── Summary generation (when interrupted / iteration limit reached) ──
        // Generate an LLM summary that reports what was accomplished and what remains.
        // This replaces the hardcoded message so the summary is the only output.
        let mut summary_msgs: Vec<ChatMessage> = messages
            .iter()
            .filter(|m| m.role != "tool")
            .map(|m| {
                let mut cloned = m.clone();
                // Remove tool_calls from assistant messages since we removed
                // the corresponding tool results: DeepSeek requires tool_call
                // chains to be complete.
                if cloned.role == "assistant" && cloned.tool_calls.is_some() {
                    cloned.tool_calls = None;
                }
                cloned
            })
            .collect();
        let iter_summary = format!(
            "The iteration limit ({}/{}) was reached so the task may be incomplete. \
             Summarize what was accomplished (including what the agent did and found) and what remains to be done. \
             Inform the user they can request to continue.",
            current_iter, iter_limit,
        );
        summary_msgs.push(ChatMessage::system(&iter_summary));

        let summary_request = CompletionRequest {
            messages: summary_msgs,
            max_tokens: cfg.config_snapshot().thread_summary_tokens,
            temperature: 0.3,
            stream: false,
            tools: None,
        };

        let _summary_start = std::time::Instant::now();
        let (summary_text, _summary_token_usage) = match per_thread_llm
            .completion(summary_request)
            .await
        {
            Ok(resp) => {
                let usage = resp.usage.clone();
                helpers::merge_usage(cumulative_usage, resp.usage);
                let tokens = usage.as_ref().map(|u| {
                    serde_json::json!({
                        "prompt_tokens": u.prompt_tokens,
                        "completion_tokens": u.completion_tokens,
                        "cached_tokens": u.cached_tokens,
                        "reasoning_tokens": u.reasoning_tokens,
                    })
                });
                info!(
                    "[summary] Generated summary for thread {} ({} chars, reasoning={}, limit_reached={})",
                    thread.id,
                    resp.content.len(),
                    resp.reasoning.as_ref().map(|r| r.len()).unwrap_or(0),
                    limit_reached,
                );
                let text = if resp.content.trim().is_empty() {
                    resp.reasoning.clone().unwrap_or_default()
                } else {
                    resp.content
                };
                (text, tokens)
            }
            Err(e) => {
                warn!(
                    "[summary] Failed to generate summary for thread {}: {:?}",
                    thread.id, e
                );
                (format!("Summary generation failed: {}", e), None)
            }
        };

        let summary_msg = MessageNew {
            thread_id: thread.id,
            role: "agent".to_string(),
            content: summary_text,
            thread_sequence: next_seq,
            external_id: None,
            metadata: serde_json::json!({}),
            embedding: None,
            summary_text: None,
            is_summary: true,
            msg_type: "summary".to_string(),
            msg_subtype: Some("interrupted".to_string()),
            iteration_number: current_iter,
            duration_ms: 0,
            token_usage: serde_json::json!({}),
        };

        let summary_saved = queries::create_message(&cfg.pool, &summary_msg).await?;
        info!("[summary] Saved summary message for thread {}", thread.id);
        helpers::enqueue_delivery(
            &cfg.ctx,
            &summary_saved,
            channel,
            thread,
            cause_msg.external_id.clone(),
        )
        .await;
        summary_saved
    } else if is_empty_response {
        let agent_content = format!(
            "The LLM returned an empty response. The task failed.\n\
             Possible causes: token explosion (context too large), provider error, or LLM output limits.\n\
             Prompt tokens used in this turn: {}",
            token_usage_json.as_ref()
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(|v| v.as_i64())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );
        let agent_msg = MessageNew {
            thread_id: thread.id,
            role: "agent".to_string(),
            content: agent_content,
            thread_sequence: next_seq,
            external_id: None,
            metadata: serde_json::json!({
                "context": evidence_metadata["context"],
                "grounding": evidence_metadata["grounding"],
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "error".to_string(),
            msg_subtype: Some("empty_response".to_string()),
            iteration_number: current_iter,
            duration_ms: 0,
            token_usage: serde_json::json!({}),
        };
        let saved = queries::create_message(&cfg.pool, &agent_msg).await?;
        helpers::enqueue_delivery(
            &cfg.ctx,
            &saved,
            channel,
            thread,
            cause_msg.external_id.clone(),
        )
        .await;
        saved
    } else {
        // Normal completion: the agent's final message IS the summary
        let agent_msg = MessageNew {
            thread_id: thread.id,
            role: "agent".to_string(),
            content: final_content.clone(),
            thread_sequence: next_seq,
            external_id: None,
            metadata: serde_json::json!({
                "context": evidence_metadata["context"],
                "grounding": evidence_metadata["grounding"],
            }),
            embedding: None,
            summary_text: None,
            is_summary: true,
            msg_type: "summary".to_string(),
            msg_subtype: None,
            iteration_number: current_iter,
            duration_ms: 0,
            token_usage: serde_json::json!({}),
        };
        let saved = queries::create_message(&cfg.pool, &agent_msg).await?;
        helpers::enqueue_delivery(
            &cfg.ctx,
            &saved,
            channel,
            thread,
            cause_msg.external_id.clone(),
        )
        .await;
        saved
    };
    // Define final status before potential early return
    let final_status = if *force_failed {
        "failed"
    } else if limit_reached {
        "interrupted"
    } else {
        "completed"
    };

    // Post-loop subtask enforcement: if any subtasks remain pending/in_progress
    // after the tool-calling loop ends (regardless of why it ended), fail the thread.
    // Subtasks must only be marked completed/cancelled by the LLM via manage_subtasks tool.
    // Exception: if the iteration limit was reached, unfinished subtasks are expected
    //: keep the interrupted status rather than downgrading to failed.
    if enable_subtasks && !*force_failed && !limit_reached && final_status == "completed" {
        if let Ok(post_subtasks) = crate::subtask::list_subtasks(&cfg.pool, thread.id).await {
            let unfinished: Vec<_> = post_subtasks
                .iter()
                .filter(|st| st.status == "pending" || st.status == "in_progress")
                .collect();
            if !unfinished.is_empty() {
                warn!(
                    "[subtask] Post-loop enforcement: {} subtask(s) still unfinished for thread {}: forcing failure",
                    unfinished.len(),
                    thread.id,
                );
                *force_failed = true;
            }
        }
    }

    // Recompute final status after post-loop enforcement
    let final_status = if *force_failed {
        "failed"
    } else if limit_reached {
        "interrupted"
    } else {
        "completed"
    };

    queries::complete_thread(
        &cfg.pool,
        thread.id,
        final_status,
        CompleteThreadStats {
            input_tokens: cumulative_usage
                .as_ref()
                .map(|u| u.prompt_tokens as i32)
                .unwrap_or(0),
            cached_tokens: cumulative_usage
                .as_ref()
                .map(|u| u.cached_tokens.unwrap_or(0) as i32)
                .unwrap_or(0),
            output_tokens: cumulative_usage
                .as_ref()
                .map(|u| u.completion_tokens as i32)
                .unwrap_or(0),
            duration_ms: agent_elapsed_ms,
        },
    )
    .await?;

    // ── Send completion reaction to platform ──
    crate::agent::reaction_handler::send_completion_reaction(cfg, channel, cause_msg, final_status).await;

    // If this thread is linked to a kanban task, update its status
    crate::agent::kanban_updater::update_kanban_status(cfg, thread, final_status).await;

    // 11. Trigger cross-thread summary check via memory plugin + cancel bg tasks
    crate::agent::summary_trigger::trigger_summary_and_cleanup(cfg, thread).await;

    Ok(saved)
}