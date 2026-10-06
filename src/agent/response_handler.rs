use crate::agent::config::AgentContext;
use crate::agent::context_compactor::strip_tool_chain;
use crate::agent::helpers;
use crate::agent::response_hygiene;
use crate::agent::terminal_summary::{
    deterministic_activity_summary, deterministic_interrupted_summary,
};
use crate::db::types as queries;
use crate::db::types::{CompleteThreadStats, Message, MessageNew, Thread};
use crate::error::AppResult;
use crate::llm::{ChatMessage, CompletionRequest, LLMClient, Usage};
use sql_forge::sql_forge;
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
    usage_entries: &mut Vec<serde_json::Value>,
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
            // The fail-thread tool already persisted its Error-type last
            // message AND inserted the thread-end Usage message just before it
            // (it inserts the Usage message first, then its Error message one
            // sequence later). This call is therefore normally a no-op; it only
            // fires when a thread was marked FAILED without going through that
            // tool, in which case the Usage message is appended after the last
            // message (best effort, never a silent absence). Existing rows are
            // never mutated.
            let last_seq = crate::db::threads::get_max_thread_sequence(&cfg.pool, thread.id)
                .await
                .unwrap_or(0);
            // Same per-thread counter as every other row: the Usage message is
            // an ordinary thread message, never a bespoke iteration 0.
            let iteration = crate::db::messages::current_thread_iteration(&cfg.pool, thread.id)
                .await
                .unwrap_or(0);
            if let Err(e) = insert_thread_usage_message_at(
                &cfg.pool,
                thread.id,
                last_seq + 1,
                usage_entries,
                cumulative_usage.as_ref(),
                iteration,
            )
            .await
            {
                warn!(
                    "[usage] Failed to insert thread-end usage message for thread {}: {:?}",
                    thread.id, e
                );
            }
            // This branch returns BEFORE the loop-exit aggregate write, and the
            // fail-thread tool finalized the row mid-loop (terminal), so the
            // threads-table usage aggregates must be written here directly
            // (tester verdict thread 3861: Failed rows kept full_* = 0 while
            // their usage array summed to non-zero; reproduced live on omnidev
            // thread 478, ended 21:57:41Z with full_input_tokens = 0).
            let usage_stats = thread_usage_stats(
                usage_entries,
                cumulative_usage.as_ref(),
                start_time.elapsed().as_millis() as i32,
            );
            if let Err(e) = crate::db::threads::update_thread_usage_aggregates(
                &cfg.pool,
                thread.id,
                &usage_stats,
            )
            .await
            {
                warn!(
                    "[usage] Failed to write usage aggregates for failed thread {}: {:?}",
                    thread.id, e
                );
            }
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
        // Tool results and the assistant tool_calls they answer are removed
        // together: the chat protocol requires complete tool_call/tool-result
        // chains, so the pair is always dropped as a unit (provider-neutral,
        // see `strip_tool_chain`).
        let mut summary_msgs: Vec<ChatMessage> = strip_tool_chain(messages);
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
             accomplishments and remaining work. Inform the user they can request to continue. \
             Tools are DISABLED for this call: reply in plain prose only, never emit tool calls or \
             any tool-call markup, and do not plan further actions - no further tool call can run in \
             this interrupted thread.",
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
        let (mut summary_text, summary_token_usage, summary_stop_reason) = match per_thread_llm
            .completion(summary_request)
            .await
        {
            Ok(resp) => {
                if let Some(ref u) = resp.usage {
                    usage_entries.push(crate::agent::usage_entries::omniagent_usage_entry(
                        u,
                        &per_thread_llm.config.provider.0,
                        &per_thread_llm.config.model,
                        &thread.profile,
                    ));
                }
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
                (text, tokens, resp.finish_reason.clone())
            }
            Err(e) => {
                warn!(
                    "[summary] Failed to generate summary for thread {}: {:?}",
                    thread.id, e
                );
                (format!("Summary generation failed: {}", e), None, None)
            }
        };

        // Terminal-content hygiene (interrupted threads must end with a PROPER
        // summary; thread 1596). A provider in text-tool mode sometimes answers
        // the summary prompt (tools:None) with raw PROVIDER-SPECIFIC tool-call
        // markup (for example an XML envelope) or with continuation prose ("I'll update
        // the subtasks..."). Persist neither: the global setting
        // `malformed_response_tool` names the MCP tool that owns that detection
        // (empty default = the core's built-in provider-neutral heuristic), and
        // when nothing coherent remains or the text is only continuation intent
        // we fall back to the deterministic digest-based summary so the
        // terminal message is always a genuine, well-formed summary with no
        // pending tool-call intent.
        let hygiene = response_hygiene::assess_with_stop(
            &cfg.ctx,
            &summary_text,
            true,
            summary_stop_reason.as_deref(),
        )
        .await;
        if hygiene.fallback {
            summary_text = deterministic_interrupted_summary(
                &cause_msg.content,
                build_tool_evidence_digest(messages).as_deref(),
                current_iter,
                iter_limit,
            );
            info!(
                "[summary] thread {}: summary response was empty/malformed/continuation-only (via_tool={}, malformed={}); using deterministic interrupted summary",
                thread.id, hygiene.via_tool, hygiene.malformed
            );
        } else {
            summary_text = hygiene.cleaned;
        }
        // Usage message first: the final (summary) message must be the LAST row
        // of the thread (operator correction 2026-10-02, telegram thread 3883).
        let next_seq = usage_then_final_seq(
            &cfg.pool,
            thread.id,
            next_seq,
            usage_entries,
            cumulative_usage.as_ref(),
            current_iter,
        )
        .await;
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
            token_usage: summary_token_usage.unwrap_or_else(|| serde_json::json!({})),
        };

        let summary_saved = queries::create_message(&cfg.pool, &summary_msg).await?;
        info!("[summary] Saved summary message for thread {}", thread.id);
        helpers::enqueue_delivery(
            &cfg.ctx,
            &summary_saved,
            channel,
            thread,
            cause_msg.external_id.clone(),
            true,
        )
        .await;
        summary_saved
    } else if is_empty_response {
        if let Some(digest) = build_tool_evidence_digest(messages) {
            // The agent returned no final message but did perform tool activity:
            // summarize what was accomplished from the tool evidence instead of
            // reporting a bare "empty response" error.
            let mut summary_msgs = strip_tool_chain(messages);
            summary_msgs.push(ChatMessage::system(&format!(
                "The agent returned an empty final message, but the following tool activity \
                 was recorded (tool results, newest first):\n{}",
                digest
            )));
            let iter_summary = "The agent produced no final message, but tool activity was \
                 recorded. Write a reasonably brief summary (a few sentences to a short \
                 paragraph) - the reader needs the key accomplishments and remaining work. \
                 Tools are disabled for this call: reply in plain prose only, never emit tool \
                 calls or any tool-call markup.";
            summary_msgs.push(ChatMessage::system(iter_summary));
            let summary_request = CompletionRequest {
                messages: summary_msgs,
                max_tokens: cfg.config_snapshot().max_tokens,
                temperature: 0.3,
                stream: false,
                tools: None,
            };
            let (mut summary_text, _summary_token_usage) =
                match per_thread_llm.completion(summary_request).await {
                    Ok(resp) => {
                        if let Some(ref u) = resp.usage {
                            usage_entries.push(crate::agent::usage_entries::omniagent_usage_entry(
                                u,
                                &per_thread_llm.config.provider.0,
                                &per_thread_llm.config.model,
                                &thread.profile,
                            ));
                        }
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
            // Terminal-content hygiene: same protection as the interrupted path
            // (thread 1596): never persist provider-specific tool-call markup or
            // continuation prose as the activity summary. The configured
            // `malformed_response_tool` (when set) owns that detection.
            let hygiene = response_hygiene::assess(&cfg.ctx, &summary_text, true).await;
            if hygiene.fallback {
                summary_text =
                    deterministic_activity_summary(&cause_msg.content, Some(digest.as_str()));
                info!(
                    "[summary] thread {}: empty-final summary response was empty/malformed/continuation-only (via_tool={}, malformed={}); using deterministic activity summary",
                    thread.id, hygiene.via_tool, hygiene.malformed
                );
            } else {
                summary_text = hygiene.cleaned;
            }
            // Usage message first: the final (summary) message must be the
            // LAST row of the thread.
            let next_seq = usage_then_final_seq(
                &cfg.pool,
                thread.id,
                next_seq,
                usage_entries,
                cumulative_usage.as_ref(),
                current_iter,
            )
            .await;
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
                true,
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
            // Usage message first: the error message must be the LAST row of
            // the thread.
            let next_seq = usage_then_final_seq(
                &cfg.pool,
                thread.id,
                next_seq,
                usage_entries,
                cumulative_usage.as_ref(),
                current_iter,
            )
            .await;
            let agent_msg = MessageNew {
                thread_id: thread.id,
                role: "agent".to_string(),
                content: agent_content,
                thread_sequence: next_seq,
                external_id: None,
                metadata: serde_json::json!({
                    "context": evidence_metadata["context"],
                    "grounding": evidence_metadata["grounding"],
                    "prompt_accounting": evidence_metadata["prompt_accounting"],
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
                true,
            )
            .await;
            saved
        }
    } else {
        // Normal completion: the agent's final message IS the summary. Hygiene
        // still applies: a model occasionally emits PROVIDER-SPECIFIC malformed
        // tool-call markup as its "final answer" content (threads 1550/1588
        // persisted exactly that as their last message). The configured
        // `malformed_response_tool` (when set) owns that detection; otherwise
        // the core's built-in heuristic is used. Either way the markup is never
        // persisted or parsed as a terminal summary, falling back to the
        // deterministic digest summary when the whole "final" text was markup.
        let hygiene = response_hygiene::assess(&cfg.ctx, &final_content, false).await;
        let final_text = if hygiene.fallback {
            info!(
                "[summary] thread {}: final content was malformed tool-call markup only (via_tool={}, malformed={}); using deterministic interrupted summary",
                thread.id, hygiene.via_tool, hygiene.malformed
            );
            deterministic_interrupted_summary(
                &cause_msg.content,
                build_tool_evidence_digest(messages).as_deref(),
                current_iter,
                iter_limit,
            )
        } else {
            hygiene.cleaned
        };
        // Usage message first: the final message must be the LAST row of the
        // thread (operator correction 2026-10-02, telegram thread 3883).
        let next_seq = usage_then_final_seq(
            &cfg.pool,
            thread.id,
            next_seq,
            usage_entries,
            cumulative_usage.as_ref(),
            current_iter,
        )
        .await;
        let agent_msg = MessageNew {
            thread_id: thread.id,
            role: "agent".to_string(),
            content: final_text,
            thread_sequence: next_seq,
            external_id: None,
            metadata: serde_json::json!({
                "context": evidence_metadata["context"],
                "grounding": evidence_metadata["grounding"],
                "prompt_accounting": evidence_metadata["prompt_accounting"],
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
            true,
        )
        .await;
        saved
    };
    // Define final status before potential early return
    let final_status = post_loop_final_status(*force_failed, limit_reached);

    // Post-loop subtask enforcement: if any subtasks remain pending/processing
    // (or legacy in_progress) after the tool-calling loop ends (regardless of
    // why it ended), fail the thread.
    // Subtasks must only be marked completed/cancelled by the LLM via manage_subtasks tool.
    // Exception: if the iteration limit was reached, unfinished subtasks are expected
    //: keep the interrupted status rather than downgrading to failed.
    if enable_subtasks && !*force_failed && !limit_reached && final_status == "completed" {
        if let Ok(post_subtasks) = crate::subtask::list_subtasks(&cfg.pool, thread.id).await {
            let unfinished: Vec<_> = post_subtasks
                .iter()
                .filter(|st| st.is_unfinished())
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

    // Thread-end Usage message: inserted BEFORE the final message by
    // `usage_then_final_seq` at each branch above, carrying the concatenated
    // usage array (all `_meta.usage` items from tool call results in call
    // order + the omniagent's own LLM-call entries). The message content is
    // the array itself; the aggregate fields are written to the threads table
    // below (operator UPDATE 2026-09-30 threads 3702/3705/3707/3709).

    // Threads-table aggregate columns (operator UPDATE 2026-09-30): sums over
    // the usage array items, min-clamped against the omniagent's own bare
    // totals (aggregate_fields does the clamp), populated at thread end exactly
    // like input_tokens / cached_tokens / output_tokens are today.
    let usage_stats =
        thread_usage_stats(usage_entries, cumulative_usage.as_ref(), agent_elapsed_ms);

    helpers::finalize_thread(
        &cfg.ctx,
        &cfg.pool,
        thread.id,
        Some(cause_msg),
        Some(channel),
        final_status,
        usage_stats,
    )
    .await?;

    // If this thread is linked to a kanban task, update its status
    crate::agent::kanban_updater::update_kanban_status(cfg, thread, final_status).await;

    // 11. Cancel remaining background tasks after completion
    crate::agent::summary_trigger::trigger_summary_and_cleanup(cfg, thread).await;

    Ok(saved)
}

/// Threads-table usage stats for a terminating thread: the omniagent's own
/// cumulative bare totals plus the `full_*` / `cost` aggregates over the
/// collected usage array items (min-clamped, see
/// [`crate::agent::usage_entries::aggregate_fields`]).
///
/// Shared by the loop-exit finalization and the early return of a thread the
/// fail-thread tool already finalized (`complete_thread` is a no-op there, so
/// the aggregates must be written explicitly).
fn thread_usage_stats(
    usage_entries: &[serde_json::Value],
    cumulative_usage: Option<&Usage>,
    duration_ms: i32,
) -> CompleteThreadStats {
    let agg = crate::agent::usage_entries::aggregate_fields(usage_entries, cumulative_usage);
    CompleteThreadStats {
        // `threads.input_tokens` is the omniagent's CACHE-MISS input only
        // (operator UPDATE 2026-10-02, telegram thread 3915: "input_tokens is
        // only cache miss"); the provider's `prompt_tokens` is the TOTAL input
        // with the cache hit included, so the hit is subtracted here.
        input_tokens: cumulative_usage
            .map(|u| u.prompt_tokens.saturating_sub(u.cached_tokens.unwrap_or(0)) as i32)
            .unwrap_or(0),
        cached_tokens: cumulative_usage
            .map(|u| u.cached_tokens.unwrap_or(0) as i32)
            .unwrap_or(0),
        output_tokens: cumulative_usage
            .map(|u| u.completion_tokens as i32)
            .unwrap_or(0),
        duration_ms,
        full_input_tokens: agg.full_input_tokens as i32,
        full_cached_tokens: agg.full_cached_tokens as i32,
        full_output_tokens: agg.full_output_tokens as i32,
        full_reasoning_tokens: agg.full_reasoning_tokens as i32,
        // `cost` = omniagent-only, `full_cost` = omniagent + external (3916/3917).
        cost: agg.omniagent_cost,
        full_cost: agg.cost,
    }
}

/// Insert the thread-end "Usage"-type message at an EXPLICIT
/// `thread_sequence`, WITHOUT mutating any already-persisted message row.
///
/// The message content is the usage ARRAY itself (all `_meta.usage` items
/// from tool call results in call order + the omniagent's own LLM-call
/// entries) - no wrapper object, no `full_*` keys (operator UPDATE 2026-09-30
/// threads 3705/3707/3709). The aggregate fields ride in the message's
/// `token_usage` metadata so `complete_thread`'s fallback aggregation can sum
/// them on fail/interrupt paths; the live path also writes them to the
/// threads table columns via `CompleteThreadStats` - see
/// [`crate::agent::usage_entries`].
///
/// PLACEMENT (operator correction 2026-10-02, telegram thread 3883): the call
/// site inserts the Usage message at the sequence the thread's FINAL message
/// will use, so the final message lands one sequence later and the Usage
/// message is the thread's 2nd-last message - LAST IN CREATION ORDER TOO (the
/// Usage row's `id`/`created_at` are LOWER than the final row's). The previous
/// implementation inserted it after the final message and shifted that row's
/// `thread_sequence` backwards (append-only exception), which made the Usage
/// row the newest row of the thread; that seq-shift UPDATE is gone.
///
/// Returns `true` when the message was inserted. Skipped (`false`) when the
/// thread has no messages at all or when it already carries a
/// `msg_type = 'usage'` message - idempotent across the terminal paths that
/// can both reach a thread (the builtin fail-thread tool inserts it before its
/// Error message; `handle_response` inserts it before the final message).
///
/// `iteration` is the thread's CURRENT iteration (the same per-thread counter
/// the agentic rows carry, see `crate::db::messages::current_thread_iteration`),
/// NOT a hardcoded 0: the Usage message is an ordinary thread message and must
/// report the turn it belongs to (a thread that ran 7 LLM calls gets 7).
pub(crate) async fn insert_thread_usage_message_at(
    pool: &sqlx::PgPool,
    thread_id: i64,
    seq: i32,
    entries: &[serde_json::Value],
    cumulative_usage: Option<&Usage>,
    iteration: i32,
) -> AppResult<bool> {
    let max_seq = crate::db::threads::get_max_thread_sequence(pool, thread_id).await?;
    if max_seq == 0 || seq <= 0 {
        return Ok(false);
    }
    // NOTE: `$1` (sqlx placeholder), not `:thread_id` - a raw sqlx query is not
    // rewritten by `sql_forge!`, so a named placeholder reaches Postgres
    // literally and fails with a 42601 syntax error at the ':'.
    let already: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM messages WHERE thread_id = $1 AND msg_type = 'usage' LIMIT 1",
    )
    .bind(thread_id)
    .fetch_optional(pool)
    .await?;
    if already.is_some() {
        return Ok(false);
    }
    // The message content is the usage ARRAY itself (operator UPDATE 3709);
    // the aggregates ride in the message's token_usage metadata, which
    // complete_thread's fallback aggregation reads when the live stats are
    // zero (fail/interrupt paths).
    let content = crate::agent::usage_entries::usage_message_content(entries);
    let agg = crate::agent::usage_entries::aggregate_fields(entries, cumulative_usage);
    let msg = MessageNew {
        thread_id,
        role: "agent".to_string(),
        content: serde_json::to_string(&content).unwrap_or_else(|_| "[]".to_string()),
        thread_sequence: seq,
        external_id: None,
        metadata: serde_json::json!({ "is_usage": true }),
        embedding: None,
        summary_text: None,
        is_summary: false,
        original_thread_id: None,
        msg_type: "usage".to_string(),
        msg_subtype: None,
        // Same per-thread counter as every other message row (never 0-by-default).
        iteration_number: iteration,
        duration_ms: 0,
        token_usage: serde_json::json!({
            "full_input_tokens": agg.full_input_tokens,
            "full_cached_tokens": agg.full_cached_tokens,
            "full_output_tokens": agg.full_output_tokens,
            "full_reasoning_tokens": agg.full_reasoning_tokens,
            "cost": agg.omniagent_cost,
            "full_cost": agg.cost,
        }),
    };
    let metadata_val: serde_json::Value =
        serde_json::from_str(&msg.metadata.to_string()).unwrap_or_default();
    // Plain INSERT into the append-only messages table: nothing is updated, so
    // no trigger exception is required and no existing row is touched. It is
    // inlined (no new_message hook): the Usage message is internal
    // bookkeeping, never delivered to the platform.
    sql_forge!(
        r#"INSERT INTO messages (
            thread_id, role, content, thread_sequence, external_id,
            metadata, embedding, summary_text, is_summary,
            msg_type, msg_subtype, original_thread_id, iteration_number,
            duration_ms, token_usage, channel_id
        )
        VALUES (:thread_id, :role, :content, :thread_sequence, NULLIF(:external_id, '')::text,
            :metadata, NULLIF(:embedding, '')::text, NULLIF(:summary_text, '')::text, :is_summary,
            :msg_type, NULLIF(:msg_subtype, '')::text, NULLIF(:original_thread_id, -1::bigint)::bigint, :iteration_number,
            :duration_ms, COALESCE(NULLIF(:token_usage, '')::jsonb, '{}'::jsonb),
            (SELECT channel_id FROM threads WHERE id = :thread_id))"#,
        ( :thread_id = msg.thread_id, :role = &msg.role, :content = &msg.content, :thread_sequence = msg.thread_sequence, :external_id = msg.external_id.as_deref().unwrap_or(""), :metadata = &metadata_val, :embedding = msg.embedding.as_deref().unwrap_or(""), :summary_text = msg.summary_text.as_deref().unwrap_or(""), :is_summary = msg.is_summary, :msg_type = &msg.msg_type, :msg_subtype = msg.msg_subtype.as_deref().unwrap_or(""), :original_thread_id = msg.original_thread_id.unwrap_or(-1i64), :iteration_number = msg.iteration_number, :duration_ms = msg.duration_ms, :token_usage = &msg.token_usage.to_string() )
    )
    .execute(pool)
    .await?;
    Ok(true)
}

/// Insert the thread-end Usage message at `next_seq` and answer the sequence
/// the thread's FINAL message must use: `next_seq + 1` when the Usage message
/// was inserted (so the final message lands after it), `next_seq` otherwise.
///
/// A failed insertion is logged and never blocks finalization.
pub(crate) async fn usage_then_final_seq(
    pool: &sqlx::PgPool,
    thread_id: i64,
    next_seq: i32,
    entries: &[serde_json::Value],
    cumulative_usage: Option<&Usage>,
    iteration: i32,
) -> i32 {
    match insert_thread_usage_message_at(
        pool,
        thread_id,
        next_seq,
        entries,
        cumulative_usage,
        iteration,
    )
    .await
    {
        Ok(true) => next_seq + 1,
        Ok(false) => next_seq,
        Err(e) => {
            warn!(
                "[usage] Failed to insert thread-end usage message for thread {}: {:?}",
                thread_id, e
            );
            next_seq
        }
    }
}

/// Convenience wrapper for the early/supervisor finalize paths that have no
/// usage collector in scope: takes the per-thread usage snapshot published by
/// the loop, inserts the thread-end Usage message at `next_seq` and answers the
/// sequence the final (error) message must use, so the Usage message is the
/// thread's 2nd-last message and the error message is LAST.
pub(crate) async fn usage_message_seq_for_thread(
    pool: &sqlx::PgPool,
    thread_id: i64,
    next_seq: i32,
) -> i32 {
    let entries = crate::agent::usage_entries::thread_usage_snapshot(thread_id);
    // The thread's CURRENT iteration, read from its own messages (the same
    // counter `threads.iterations` is derived from) - so callers that have no
    // `current_iter` in scope still write the real turn, never a bespoke 0.
    let iteration = crate::db::messages::current_thread_iteration(pool, thread_id)
        .await
        .unwrap_or(0);
    usage_then_final_seq(pool, thread_id, next_seq, &entries, None, iteration).await
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
/// tool_call/tool-result chain requirement is never reintroduced.
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
    fn strip_tool_chain_removes_tools_and_assistant_calls() {
        let msgs = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("let me check"),
            ChatMessage::tool_result("call_1", "filesystem_read", "content"),
        ];
        let stripped = strip_tool_chain(&msgs);
        assert_eq!(stripped.len(), 2);
        assert!(stripped.iter().all(|m| m.role != "tool"));
        assert!(stripped.iter().all(|m| m.tool_calls.is_none()));
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
