use crate::agent::config::AgentContext;
use crate::agent::helpers;
use crate::db::types as queries;
use crate::db::types::{CompleteThreadStats, Message, MessageNew, Thread};
use crate::error::AppResult;
use crate::llm::{ChatMessage, CompletionRequest, LLMClient, Usage};
use tracing::{info, warn};

#[allow(clippy::too_many_arguments)]
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
    // ── Fail-thread tool outcome (Phase 2) ──────────────────────────────────
    // If the builtin fail-thread tool already ended this thread as FAILED
    // (Error-type message created, thread completed, kanban transition
    // applied), its outcome is authoritative: return without re-finalizing so
    // the Error-type message stays the thread's last message and the kanban
    // transition is not overwritten.
    if let Some(current_status) = queries::get_thread_status(&cfg.pool, thread.id).await? {
        if current_status == "failed" {
            info!(
                "thread {} already ended as FAILED by the fail-thread tool - skipping finalization",
                thread.id
            );
            let saved = queries::get_last_message(&cfg.pool, thread.id)
                .await?
                .unwrap_or_else(|| cause_msg.clone());
            return Ok(saved);
        }
    }

    // -- Deterministic tool-result pruning before summary generation (task 2) --
    // Shrink over-budget tool results to a bounded head/middle/tail preview so
    // the summary paths (digest + LLM call) never pay for huge dumps. Spill
    // locators (task 1) are preserved; pure slicing, zero LLM cost. The
    // original `messages` slice is not mutated - the summary paths below use
    // the pruned copy.
    let (pruned_messages, prune_report) = crate::agent::tool_result_pruner::prune_messages_owned(
        messages,
        &crate::agent::tool_result_pruner::PruneParams::from_config(&cfg.config_snapshot()),
    );
    if !prune_report.is_empty() {
        info!(
            "[prune] Pre-summary prune for thread {}: {} result(s) pruned, {} chars -> {} (saved {})",
            thread.id,
            prune_report.entries.len(),
            prune_report.chars_before,
            prune_report.chars_after,
            prune_report.chars_saved(),
        );
    }
    let messages: &[ChatMessage] = &pruned_messages;

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
        // Include a compact digest of tool activity so the summarizer can see
        // what the agent actually did (file writes, git commits, test results).
        if let Some(digest) = build_tool_evidence_digest(messages) {
            summary_msgs.push(ChatMessage::system(&format!(
                "Tool activity evidence from this thread (tool results, newest first):\n{}",
                digest
            )));
        }
        let iter_summary = format!(
            "The iteration limit ({}/{}) was reached so the task may be incomplete. \
             Write a reasonably brief summary (a few sentences to a short paragraph) - the reader needs the key \
             accomplishments and remaining work. Inform the user they can request to continue.",
            current_iter, iter_limit,
        );
        summary_msgs.push(ChatMessage::system(&iter_summary));

        let summary_request = CompletionRequest {
            messages: summary_msgs,
            max_tokens: cfg.config_snapshot().max_tokens,
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
            original_thread_id: None,
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
        if let Some(digest) = build_tool_evidence_digest(messages) {
            // The agent returned no final message but did perform tool activity:
            // summarize what was accomplished from the tool evidence instead of
            // reporting a bare "empty response" error.
            let mut summary_msgs = strip_tool_messages(messages);
            summary_msgs.push(ChatMessage::system(&format!(
                "The agent returned an empty final message, but the following tool activity \
                 was recorded (tool results, newest first):\n{}",
                digest
            )));
            let iter_summary = "The agent produced no final message, but tool activity was \
                 recorded. Write a reasonably brief summary (a few sentences to a short \
                 paragraph) - the reader needs the key accomplishments and remaining work.";
            summary_msgs.push(ChatMessage::system(iter_summary));
            let summary_request = CompletionRequest {
                messages: summary_msgs,
                max_tokens: cfg.config_snapshot().max_tokens,
                temperature: 0.3,
                stream: false,
                tools: None,
            };
            let (summary_text, _summary_token_usage) =
                match per_thread_llm.completion(summary_request).await {
                    Ok(resp) => {
                        let tokens = resp
                            .usage
                            .as_ref()
                            .map(|u| u.prompt_tokens + u.completion_tokens);
                        info!(
                            "[summary] Empty-final summary generated for thread {} ({} tokens)",
                            thread.id,
                            tokens.unwrap_or(0),
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
                            "[summary] Failed to generate empty-final summary for thread {}: {:?}",
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
                original_thread_id: None,
                msg_type: "summary".to_string(),
                msg_subtype: Some("activity_summary".to_string()),
                iteration_number: current_iter,
                duration_ms: 0,
                token_usage: serde_json::json!({}),
            };
            let summary_saved = queries::create_message(&cfg.pool, &summary_msg).await?;
            info!(
                "[summary] Saved empty-final summary message for thread {}",
                thread.id
            );
            helpers::enqueue_delivery(
                &cfg.ctx,
                &summary_saved,
                channel,
                thread,
                cause_msg.external_id.clone(),
            )
            .await;
            summary_saved
        } else {
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
                original_thread_id: None,
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
        }
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
            original_thread_id: None,
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
    let final_status = post_loop_final_status(*force_failed, limit_reached);

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
    let final_status = post_loop_final_status(*force_failed, limit_reached);

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
    // Find the cause message's external_id for the reaction target
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
                // Map status to platform emoji before enqueueing -
                // the platform plugin expects an actual emoji name, not a status string.
                let react_emoji = match final_status {
                    "completed" => ":white_check_mark:",
                    "failed" => ":x:",
                    "interrupted" => ":broken_heart:",
                    "skipped" => ":o:",
                    other => other,
                };
                helpers::enqueue_reaction(&cfg.ctx, platform, resource, ext_id, react_emoji).await;
            }
        }
    }

    // If this thread is linked to a kanban task, update its status
    crate::agent::kanban_updater::update_kanban_status(cfg, thread, final_status).await;

    // 11. Cancel remaining background tasks after completion
    crate::agent::summary_trigger::trigger_summary_and_cleanup(cfg, thread).await;

    Ok(saved)
}

/// Final thread status after the executor loop (pure, unit-tested).
/// - `force_failed` (fail-thread tool, truncation fail-fast, empty-response
///   exhaustion, subtask enforcement) → "failed": the task goes blocked (or
///   review when `review_on_fail` is set) - it never advances forward.
/// - iteration-limit interruption → "interrupted" (resumable).
/// - otherwise → "completed".
pub(crate) fn post_loop_final_status(force_failed: bool, limit_reached: bool) -> &'static str {
    if force_failed {
        "failed"
    } else if limit_reached {
        "interrupted"
    } else {
        "completed"
    }
}

/// Maximum number of tool messages included in the tool-evidence digest.
const MAX_DIGEST_TOOL_MSGS: usize = 30;
/// Maximum number of characters kept from each tool message's output.
const MAX_DIGEST_TOOL_CHARS: usize = 300;

/// Build a compact, bounded digest of tool activity from the thread's tool
/// messages (newest first). Returns `None` when the thread has no tool
/// messages. Each entry is `[tool] <name> <truncated output>` - enough for
/// the summarizer to see file writes, git commits, and test results without
/// blowing the summary token budget. Plain text in a `system` message, so the
/// DeepSeek tool_call/tool-result chain requirement is never reintroduced.
fn build_tool_evidence_digest(messages: &[ChatMessage]) -> Option<String> {
    let tool_msgs: Vec<&ChatMessage> = messages
        .iter()
        .filter(|m| m.role == "tool")
        .rev()
        .take(MAX_DIGEST_TOOL_MSGS)
        .collect();
    if tool_msgs.is_empty() {
        return None;
    }
    let mut entries = Vec::with_capacity(tool_msgs.len());
    for m in tool_msgs {
        let name = m.name.clone().unwrap_or_else(|| "tool".to_string());
        let preview: String = m.content.chars().take(MAX_DIGEST_TOOL_CHARS).collect();
        let truncated = preview.chars().count() < m.content.chars().count();
        let output = if truncated {
            format!("{}…", preview)
        } else {
            preview
        };
        entries.push(format!("[tool] {} {}", name, output));
    }
    Some(entries.join("\n"))
}

/// Clone the conversation without tool-result messages, stripping assistant
/// `tool_calls` so the tool_call/tool-result chain requirement is not
/// violated when raw tool messages are not passed back to the model.
fn strip_tool_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .filter(|m| m.role != "tool")
        .map(|m| {
            let mut cloned = m.clone();
            if cloned.role == "assistant" && cloned.tool_calls.is_some() {
                cloned.tool_calls = None;
            }
            cloned
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_msg(name: &str, content: &str) -> ChatMessage {
        ChatMessage::tool_result("call_1", name, content)
    }

    #[test]
    fn digest_includes_tool_names_and_truncated_output() {
        let long = "x".repeat(1000);
        let msgs = vec![
            ChatMessage::user("hi"),
            tool_msg("filesystem_write", &format!("wrote file /tmp/a {}", long)),
            tool_msg("git_commit-and-push", "pushed commit abc123"),
        ];
        let digest = build_tool_evidence_digest(&msgs).expect("digest should exist");
        assert!(digest.contains("[tool] filesystem_write"));
        assert!(digest.contains("[tool] git_commit-and-push"));
        assert!(digest.contains("wrote file /tmp/a"));
        // The long output is truncated, so the digest is far smaller than raw.
        assert!(digest.contains('…'));
        assert!(digest.len() < 500);
    }

    #[test]
    fn digest_is_bounded() {
        let msgs: Vec<ChatMessage> = (0..50)
            .map(|i| tool_msg(&format!("tool_{}", i), &"y".repeat(500)))
            .collect();
        let digest = build_tool_evidence_digest(&msgs).expect("digest should exist");
        // Only the last MAX_DIGEST_TOOL_MSGS are included, newest first.
        assert_eq!(digest.lines().count(), MAX_DIGEST_TOOL_MSGS);
        assert!(digest.contains("[tool] tool_49"));
        assert!(!digest.contains("[tool] tool_0"));
        // Each entry's output portion is bounded to MAX_DIGEST_TOOL_CHARS + 1 (ellipsis).
        for entry in digest.lines() {
            let body = entry.strip_prefix("[tool] ").unwrap_or(entry);
            let name_end = body.find(' ').unwrap_or(0);
            let output = &body[name_end + 1..];
            assert!(output.chars().count() <= MAX_DIGEST_TOOL_CHARS + 1);
        }
    }

    #[test]
    fn digest_none_without_tool_messages() {
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
        ];
        assert!(build_tool_evidence_digest(&msgs).is_none());
    }

    #[test]
    fn empty_final_routing_depends_on_tool_activity() {
        // (c) empty final + tool activity => digest Some => summary path.
        let with_activity = vec![tool_msg("filesystem_write", "wrote x")];
        assert!(build_tool_evidence_digest(&with_activity).is_some());
        // (d) empty final + no tool activity => digest None => error path.
        let no_activity: Vec<ChatMessage> = vec![ChatMessage::user("hi")];
        assert!(build_tool_evidence_digest(&no_activity).is_none());
    }

    #[test]
    fn strip_tool_messages_removes_tools_and_assistant_calls() {
        let msgs = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("let me check"),
            ChatMessage::tool_result("call_1", "filesystem_read", "content"),
        ];
        let stripped = strip_tool_messages(&msgs);
        assert_eq!(stripped.len(), 2);
        assert!(stripped.iter().all(|m| m.role != "tool"));
    }

    #[test]
    fn post_loop_final_status_force_failed_wins() {
        // The FailFast/empty-response/subtask-enforcement contract: any
        // force_failed => "failed" regardless of limit_reached.
        assert_eq!(post_loop_final_status(true, false), "failed");
        assert_eq!(post_loop_final_status(true, true), "failed");
        assert_eq!(post_loop_final_status(false, true), "interrupted");
        assert_eq!(post_loop_final_status(false, false), "completed");
    }
}
