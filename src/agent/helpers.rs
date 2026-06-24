use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::agent::config::AgentConfig;
use crate::db::types as queries;
use crate::db::types::{Channel, CompleteThreadStats, Message, MessageNew, Thread};
use crate::llm::{ChatMessage, CompletionRequest, LLMClient, Usage};
use crate::mcp::{AppContext};
use crate::platform::queue::OutboundEnvelope;
use crate::platform::enqueue_notification;

/// Merge cumulative usage with a new usage value.
pub fn merge_usage(cumulative: &mut Option<Usage>, new_usage: Option<Usage>) {
    if let Some(new) = new_usage {
        if let Some(ref mut cum) = cumulative {
            cum.prompt_tokens += new.prompt_tokens;
            cum.completion_tokens += new.completion_tokens;
            cum.cached_tokens =
                Some(cum.cached_tokens.unwrap_or(0) + new.cached_tokens.unwrap_or(0));
            cum.reasoning_tokens = cum.reasoning_tokens.or(new.reasoning_tokens);
        } else {
            *cumulative = Some(new);
        }
    }
}

/// Check if a database error is a foreign key violation (PostgreSQL code 23503).
/// These indicate the thread was deleted or the FK constraint was broken —
/// the thread should be marked as failed rather than retried.
fn is_fk_violation(e: &anyhow::Error) -> bool {
    if let Some(sqlx::Error::Database(ref dberr)) = e.downcast_ref::<sqlx::Error>() {
            return dberr.code().as_deref() == Some("23503");
    }
    false
}

/// Persist a message and detect FK violations that should abort thread processing.
/// Returns the created message on success, or an error variant.
pub enum CreateMessageResult {
    Success(Message),
    FkViolation,
    OtherError(anyhow::Error),
}

pub async fn persist_or_abort(
    pool: &PgPool,
    msg: &MessageNew,
    thread_id: i64,
) -> CreateMessageResult {
    match queries::create_message(pool, msg).await {
        Ok(saved) => CreateMessageResult::Success(saved),
        Err(e) if is_fk_violation(&e) => {
            error!(
                "FK violation inserting message for thread {} — marking thread as failed",
                thread_id
            );
            // Mark the thread as failed
            let _ = queries::complete_thread(pool, thread_id, "failed", CompleteThreadStats { input_tokens: 0, cached_tokens: 0, output_tokens: 0, duration_ms: 0 }).await;
            CreateMessageResult::FkViolation
        }
        Err(e) => CreateMessageResult::OtherError(e),
    }
}

/// Estimate the total character count of all messages in the conversation.
/// This is a rough proxy for prompt tokens (~4 chars per token).
pub fn estimate_chars(messages: &[ChatMessage]) -> usize {
    messages.iter().map(|m| {
        let mut len = m.content.len();
        if let Some(ref calls) = m.tool_calls {
            for tc in calls {
                len += tc.function.name.len() + tc.function.arguments.len() + 50; // overhead
            }
        }
        len
    }).sum()
}

/// Count the actual token count of messages by serializing to JSON and
/// running through tiktoken BPE. Much more accurate than estimate_chars
/// for models that use cl100k_base / o200k_base tokenization.
///
/// `encoding` is a tiktoken model name like "gpt-4", "cl100k_base", or
/// "o200k_base". Falls back to estimate_chars on any error.
///
/// When `tools` is provided, those tool definitions are included in the
/// serialized JSON so the token count reflects the full API request
/// (messages + tool schemas/descriptions), not just the message list.
pub fn count_tokens(messages: &[ChatMessage], encoding: &str, tools: Option<&[serde_json::Value]>) -> usize {
    // Serialize messages to the JSON format the API receives.
    // When tools are present, wrap in a full request mock to capture
    // the tool definition tokens (which can add 200-300K tokens).
    let json = match tools {
        Some(t) if !t.is_empty() => {
            let request = serde_json::json!({
                "messages": messages,
                "tools": t,
            });
            serde_json::to_string(&request)
        }
        _ => {
            // No tools: serialize just the messages array (lighter, same as before)
            serde_json::to_string(&messages)
        }
    };

    let json = match json {
        Ok(j) => j,
        Err(e) => {
            warn!("[tokens] Failed to serialize messages for token counting: {}", e);
            return estimate_chars(messages);
        }
    };

    // Return early for empty messages
    if json.is_empty() {
        return 0;
    }

    // Load the BPE encoding
    let bpe = match tiktoken_rs::get_bpe_from_model(encoding) {
        Ok(bpe) => bpe,
        Err(e) => {
            warn!("[tokens] Failed to load BPE encoding '{}': {} — falling back to char estimate", encoding, e);
            return estimate_chars(messages);
        }
    };

    // Count tokens (includes special tokens like <|im_start|>, <|im_end|>)
    let tokens = bpe.encode_with_special_tokens(&json);
    let count = tokens.len();
    
    // info!("[tokens] Counted {} tokens for {} messages using '{}' encoding", count, messages.len(), encoding);
    count
}

/// Prune old tool results from the conversation history, with iteration-aware
/// progressive tightening.
///
/// Keeps the most recent turn's results intact and strips old tool result
/// bodies, replacing them with a short summary, while preserving all
/// user, assistant, and system messages unchanged.
///
/// The truncation becomes more aggressive as iterations increase:
///   0–5:    no pruning (keep full)
///   6–10:   truncate bodies >1,000 chars to 200-char preview
///   11–15:  truncate bodies >300 chars to 100-char preview
///   16+:    replace entire body with metadata-only label
pub fn prune_old_tool_results(messages: &mut [ChatMessage], current_iter: u32) {
    // Find the index of the last assistant message with tool_calls — this
    // marks the most recent turn boundary. Tool results after it are kept.
    let last_tool_turn_idx = messages
        .iter()
        .rposition(|m| m.role == "assistant" && m.tool_calls.is_some());

    let keep_from = last_tool_turn_idx.unwrap_or(0);

    // Determine truncation level based on iteration
    let (max_body_chars, compact_mode) = match current_iter {
        0..=5 => (usize::MAX, false),       // no pruning
        6..=10 => (1000, false),             // moderate truncation
        11..=15 => (300, false),             // aggressive truncation
        _ => (0, true),                      // zero content — just the label
    };

    for msg in messages.iter_mut().take(keep_from) {
        if msg.role == "tool" {
            if compact_mode {
                let tool_name = msg.name.as_deref().unwrap_or("unknown");
                msg.content = format!(
                    "[Tool result for `{}` — {} total chars, omitted]",
                    tool_name, msg.content.len()
                );
            } else if msg.content.len() > max_body_chars {
                let preview: String = msg.content.chars().take(200).collect();
                msg.content = format!(
                    "[Pruned tool result — was {} chars] {}",
                    msg.content.len(), preview
                );
            }
        }
    }
}

/// Compact old assistant messages that contain tool_calls JSON.
///
/// Replaces the full function arguments with a condensed reference
/// like `tool_a(), tool_b()` and **removes** the following tool-role
/// messages entirely. This is necessary because OpenAI-compatible APIs
/// require every `tool` message to be immediately preceded by an assistant
/// message with `tool_calls` — keeping tool messages after stripping
/// `tool_calls` from the assistant would cause a 400 error.
///
/// Tool messages are removed (not just compacted) because any `role: "tool"`
/// message without a preceding `tool_calls` violates the API contract.
/// The tool names are preserved in the assistant message content so the
/// model still knows what was called.
pub fn compact_old_assistant_messages(messages: &mut Vec<ChatMessage>, keep_recent: usize) {
    loop {
        // Find all tool-calling assistant message positions
        let tool_indices: Vec<usize> = messages.iter()
            .enumerate()
            .filter(|(_, m)| m.role == "assistant" && m.tool_calls.is_some())
            .map(|(i, _)| i)
            .collect();

        if tool_indices.len() <= keep_recent {
            return;
        }

        let compact_up_to = tool_indices.len() - keep_recent;
        // Process from the end so removal doesn't shift remaining indices
        for &idx in tool_indices.iter().take(compact_up_to).rev() {
            if let Some(ref calls) = messages[idx].tool_calls {
                let summary: Vec<String> = calls.iter()
                    .map(|tc| format!("{}()", tc.function.name))
                    .collect();

                // Find the range of tool messages that follow this assistant
                let mut tool_end = idx + 1;
                while tool_end < messages.len() && messages[tool_end].role == "tool" {
                    tool_end += 1;
                }

                let tool_count = tool_end - idx - 1;
                let tool_info = if tool_count > 0 {
                    let tool_names: Vec<&str> = messages[idx..tool_end]
                        .iter()
                        .skip(1)
                        .filter_map(|m| m.name.as_deref())
                        .collect();
                    if !tool_names.is_empty() {
                        format!(". Results from: {}", tool_names.join(", "))
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };

                // Remove the tool messages (they can't stay as role="tool"
                // without a preceding assistant with tool_calls)
                if tool_count > 0 {
                    messages.drain(idx + 1..tool_end);
                }

                // Now compact the assistant message (index unchanged since
                // we drained from idx+1, and we're processing rev)
                messages[idx].tool_calls = None;
                messages[idx].content = format!(
                    "[#{} Tool calls compacted: {}{}]",
                    idx,
                    summary.join(", "),
                    tool_info,
                );
            }
        }
        // Continue loop — indices have shifted, re-scan
    }
}

/// Build a compact metadata block for a range of messages.
/// Each entry includes message role, type indicator, and size.
/// This is used during emergency condensation to preserve message IDs
/// and metadata without keeping the full content.
pub fn build_message_metadata_block(messages: &[ChatMessage], offset: usize) -> String {
    if messages.is_empty() {
        return String::new();
    }

    let entries: Vec<String> = messages.iter().enumerate().map(|(i, msg)| {
        let idx = offset + i;
        let role_short = match msg.role.as_str() {
            "assistant" => {
                if msg.tool_calls.is_some() { "tool_call" } else { "assistant" }
            }
            "tool" => "tool_result",
            "system" => "system",
            "user" => "user",
            other => other,
        };
        let meta = if msg.tool_calls.is_some() {
            let names: Vec<&str> = msg.tool_calls.as_ref()
                .map(|calls| calls.iter().map(|tc| tc.function.name.as_str()).collect())
                .unwrap_or_default();
            format!(" — {}", names.join(", "))
        } else if !msg.content.is_empty() && msg.content.len() < 200 {
            format!(" — {}", msg.content)
        } else {
            String::new()
        };
        format!("#{} {} {} ({}{})", idx, role_short, msg.content.len(),
            if meta.is_empty() { "" } else { &meta[..meta.len().min(200)] }, "")
    }).collect();

    format!("==== Old Messages Compacted ====\nMessages {}–{} have been condensed. Query with query_database if full content is needed.\n{}\n",
        offset,
        offset + messages.len() - 1,
        entries.join("\n")
    )
}

/// Condense messages when the prompt budget is exceeded.
///
/// Strategy:
/// 1. Separate system messages (always keep) from conversation messages.
/// 2. Safety check: if system messages alone exceed PROMPT_CHAR_BUDGET_SOFT or
///    comprise >90% of it, the task cannot meaningfully proceed — return an error.
/// 3. Keep the last N full assistant→tool cycles verbatim.
/// 4. Replace everything before that with a compact metadata block.
/// 5. Trim old messages until old_message_char_budget is satisfied.
///
/// Returns the condensed message list, or an error if the always-keep portion
/// is too large to leave room for context.
pub fn condense_messages(
    messages: Vec<ChatMessage>,
    old_msg_budget: usize,
    keep_turns: usize,
    soft_budget: usize,
) -> Result<Vec<ChatMessage>, String> {
    // 1. Separate system messages from conversation
    let system_msgs: Vec<ChatMessage> = messages.iter()
        .filter(|m| m.role == "system")
        .cloned()
        .collect();

    let conv_msgs: Vec<&ChatMessage> = messages.iter()
        .filter(|m| m.role != "system")
        .collect();

    // 2. Safety check: always-keep portion too large?
    let system_chars: usize = system_msgs.iter().map(|m| m.content.len()).sum();
    let soft_budget_ninety = (soft_budget as f64 * 0.9) as usize;

    if system_chars > soft_budget_ninety {
        return Err(format!(
            "Always-keep messages (system prompt + MEMORY + subtasks) use {} chars, \
             which exceeds 90% of the PROMPT_CHAR_BUDGET_SOFT ({}). \
             The task cannot proceed with meaningful context. \
             Reduce system prompt/MEMORY.md size or increase PROMPT_CHAR_BUDGET_SOFT (currently {}).",
            system_chars, soft_budget_ninety, soft_budget
        ));
    }
    if system_chars > soft_budget {
        return Err(format!(
            "Always-keep messages (system prompt + MEMORY + subtasks) use {} chars, \
             which exceeds PROMPT_CHAR_BUDGET_SOFT ({}). \
             Reduce them or increase the budget.",
            system_chars, soft_budget
        ));
    }

    if conv_msgs.is_empty() {
        return Ok(messages); // nothing to condense
    }

    // 3. Find where the last N turns start
    let conv_len = conv_msgs.len();
    let mut keep_from = 0usize;
    let mut turns_found = 0usize;

    for i in (0..conv_len).rev() {
        if conv_msgs[i].role == "assistant" && conv_msgs[i].tool_calls.is_some() {
            turns_found += 1;
            if turns_found >= keep_turns {
                keep_from = i;
                break;
            }
        }
    }

    // 4. Build metadata block for messages before keep_from
    let early_conv: Vec<ChatMessage> = conv_msgs[..keep_from]
        .iter()
        .map(|m| (*m).clone())
        .collect();

    let metadata_text = if !early_conv.is_empty() {
        build_message_metadata_block(&early_conv, 0)
    } else {
        String::new()
    };

    // 5. Assemble the condensed list
    let mut condensed: Vec<ChatMessage> = system_msgs;

    if !metadata_text.is_empty() {
        condensed.push(ChatMessage::system(&metadata_text));
    }

    // Add the kept messages
    for m in conv_msgs.iter().skip(keep_from) {
        condensed.push((*m).clone());
    }

    // 6. If old messages still exceed the old_msg_budget, progressively trim
    //    the metadata block and what's kept of the old messages
    let conv_start = condensed.iter().position(|m| m.role != "system")
        .unwrap_or(condensed.len());

    if conv_start < condensed.len() {
        // Estimate how many chars the old messages (after system) take
        let old_part: usize = condensed[conv_start..].iter().map(|m| m.content.len()).sum();
        if old_part > old_msg_budget {
            // Trim oldest messages before the last `keep_turns` turns
            // (re-scan in the condensed list)
            let mut trim_end = condensed.len();
            let mut found = 0usize;
            for i in (conv_start..condensed.len()).rev() {
                if condensed[i].role == "assistant" && condensed[i].tool_calls.is_some() {
                    found += 1;
                    if found >= keep_turns {
                        trim_end = i;
                        break;
                    }
                }
            }

            // Build a tighter metadata block for everything up to trim_end
            if trim_end > conv_start {
                let to_compact: Vec<ChatMessage> = condensed.drain(conv_start..trim_end).collect();
                let meta = build_message_metadata_block(&to_compact, 0);
                condensed.insert(conv_start, ChatMessage::system(&meta));
            }
        }
    }

    Ok(condensed)
}

/// Check if enough completed threads have accumulated since the
/// last summary for this channel, and if so, generate a new cross-thread summary.
///
/// Algorithm:
/// 1. Get the `next_thread_id` from the latest summary (0 if none).
/// 2. Count completed threads with id > next_thread_id.
/// 3. If count >= 2*N (where N = SUMMARY_WINDOW), generate a summary.
/// 4. The first thread id = first thread, the last = last thread.
/// 5. For each of the 2*N threads, fetch ALL its messages.
/// 6. Build a summarization prompt with previous summary context.
/// 7. Save with `next_thread_id` = the N-th thread's id (window slides by N).
pub async fn check_and_generate_summary(
    pool: &PgPool,
    llm: &LLMClient,
    config: &AgentConfig,
    channel_id: i64,
) {
    let window = config.summary_window as i64;
    if window == 0 {
        return; // summaries disabled
    }
    let trigger_count = window * 2; // need 2*N threads to trigger

    // 1. Get latest summary's next_thread_id
    let since_id = match queries::get_latest_summary(pool, channel_id).await {
        Ok(Some(summary)) => summary.next_thread_id,
        _ => 0i64,
    };

    // 2. Fetch completed threads since the last summary
    let completed_threads = match queries::get_completed_seq0_threads_since(
        pool, channel_id, since_id, trigger_count,
    )
    .await
    {
        Ok(threads) => threads,
        Err(e) => {
            warn!(
                "[thread-summary] Failed to fetch completed threads for channel {}: {:?}",
                channel_id, e
            );
            return;
        }
    };

    if (completed_threads.len() as i64) < trigger_count {
        // Not enough threads yet
        return;
    }

    // We have 2*N threads. The first thread's id is completed_threads[0].id.
    // The N-th thread's id (the sliding window point):
    let pivot_thread_id = completed_threads[(window - 1) as usize].id;
    let first_thread_id = completed_threads[0].id;
    let last_thread_id = completed_threads[(trigger_count - 1) as usize].id;

    info!(
        "[thread-summary] Generating summary for channel {}: {} threads (id {} to {}), pivot={}",
        channel_id, trigger_count, first_thread_id, last_thread_id, pivot_thread_id
    );

    // 3. For each of the 2*N threads, fetch ALL messages
    let mut all_thread_content = String::new();
    for thread_db in &completed_threads {
        let tid = thread_db.id;
        match queries::get_thread_messages(pool, tid).await {
            Ok(thread_msgs) => {
                all_thread_content.push_str(&format!(
                    "\n=== Thread #{} (cause: {} at {}) ===\n",
                    tid,
                    thread_db.cause,
                    thread_db.created_at.as_deref().unwrap_or("?"),
                ));
                for m in &thread_msgs {
                    let role_display = match m.role.as_str() {
                        "user" => "User",
                        "agent" => "Assistant",
                        "system" => "System",
                        _ => &m.role,
                    };
                    // Skip tool results to keep context manageable
                    if m.msg_type == "tool_result" || m.msg_type == "tool" {
                        continue;
                    }
                    all_thread_content.push_str(&format!(
                        "[{}]: {}\n",
                        role_display,
                        m.content.chars().take(1000).collect::<String>()
                    ));
                }
            }
            Err(e) => {
                warn!(
                    "[thread-summary] Failed to fetch messages for thread {}: {:?}",
                    tid, e
                );
            }
        }
    }

    // 4. Fetch the last summary for context (to avoid repeating info)
    let previous_summary_text = match queries::get_latest_summary(pool, channel_id).await {
        Ok(Some(s)) => s.content,
        _ => String::new(),
    };

    // 5. Build summarization prompt — structured output
    //    The LLM produces a structured summary that can be parsed, searched, and
    //    cross-referenced with hindsight and Qdrant.
    let system_summarizer_prompt =
        "You are a conversation summarizer for an autonomous agent system. \
         Produce a structured summary in the exact format below. \
         Be specific — include file paths, config keys, exact numbers, and command names. \
         Do NOT repeat information covered in the previous summary (if provided). \
         Every claim must be grounded in the provided conversation content.\n\n\
         ## Format:\n\
         ### Topics\n\
         - topic: <topic_name> | detail: <one sentence with specifics>\n\n\
         ### Key Decisions\n\
         - decision: <what was decided> | context: <why> | files: <affected files, if any>\n\n\
         ### Action Items\n\
         - status: <done|pending|failed> | task: <what> | details: <specifics>\n\n\
         ### Entities Referenced\n\
         - <entity_name> (<type>): <relation to conversation>\n\n\
         ### Thread Count\n\
         - total: <number> | first: <id> | last: <id>\n\n\
         Keep each entry on a single line. Use | as field separator.";

    let summary_prompt = if previous_summary_text.is_empty() {
        format!(
            "Summarize the following conversations from a single channel.\n\n{}",
            all_thread_content
        )
    } else {
        format!(
            "PREVIOUS SUMMARY (do NOT repeat):\n{}\n\n---\n\n\
             Now summarize the following new conversations, \
             connecting to the previous summary if relevant.\n\n{}",
            previous_summary_text, all_thread_content
        )
    };

    // 6. Call LLM for summary
    let summary_request = CompletionRequest {
        messages: vec![
            ChatMessage::system(&system_summarizer_prompt),
            ChatMessage::user(&summary_prompt),
        ],
        max_tokens: config.summary_tokens,
        temperature: 0.2, // lower temperature for factual consistency
        stream: false,
        tools: None,
    };

    let summary_content = match llm.completion(summary_request).await {
        Ok(resp) => {
            info!(
                "[thread-summary] Generated summary for channel {} ({} chars, {} tokens)",
                channel_id,
                resp.content.len(),
                resp.usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0),
            );
            resp.content
        }
        Err(e) => {
            warn!(
                "[thread-summary] Failed to generate summary for channel {}: {:?}",
                channel_id, e
            );
            return;
        }
    };

    // 7. Save the summary with next_thread_id = the N-th thread's id
    //    (window slides by N, so the next trigger will start from this pivot)
    match queries::create_summary(pool, channel_id, pivot_thread_id, &summary_content).await {
        Ok(summary) => {
            info!(
                "[thread-summary] Saved summary {} for channel {} (next_thread_id={}, covers {} threads)",
                summary.id, channel_id, pivot_thread_id, trigger_count
            );
        }
        Err(e) => {
            warn!(
                "[thread-summary] Failed to save summary for channel {}: {:?}",
                channel_id, e
            );
        }
    }
}

/// Enqueue a message for delivery to its platform.
pub async fn enqueue_delivery(
    ctx: &AppContext,
    saved: &Message,
    channel: &Channel,
    thread: &Thread,
    cause_external_id: Option<String>,
) {
    let platform = match &channel.platform {
        Some(p) => p.clone(),
        None => return,
    };
    let resource_identifier = match &channel.resource_identifier {
        Some(r) => r.clone(),
        None => return,
    };

    // Look up the per-platform sender
    let sender = match ctx.platform_senders.get(&platform) {
        Some(s) => s.clone(),
        None => return,
    };

    // For non-user threads, only deliver summaries and errors
    if thread.cause != "user" && saved.msg_type != "summary" && saved.msg_type != "error" {
        return;
    }

    // Never deliver tool results directly
    if saved.msg_type == "tool_result" {
        return;
    }

    let envelope_content = if saved.msg_type == "summary" && platform == "cli" {
        // Quote the seq-0 message for CLI delivery (not needed for Telegram — it uses reply threading)
        match queries::get_cause_message(&ctx.pool, saved.thread_id).await {
            Ok(Some(cause)) => {
                let cause_trimmed: String = cause.content.chars().take(100).collect();
                let quoted = if cause.content.len() > 100 {
                    format!("> {}...\n\n{}", cause_trimmed, saved.content)
                } else {
                    format!("> {}\n\n{}", cause_trimmed, saved.content)
                };
                quoted
            }
            _ => saved.content.clone(),
        }
    } else {
        saved.content.clone()
    };

    let envelope = OutboundEnvelope {
        message_id: saved.id,
        resource_identifier,
        content: envelope_content,
        msg_type: saved.msg_type.clone(),
        msg_subtype: saved.msg_subtype.clone(),
        thread_id: saved.thread_id,
        thread_sequence: saved.thread_sequence,
        cause_external_id,
        is_summary: saved.is_summary,
        is_user_thread: thread.cause == "user",
    };

    if let Err(e) = sender.try_send(envelope) {
        tracing::warn!("Failed to enqueue delivery for message {}: {:?}", saved.id, e);
    }

    // If this is a summary, also deliver to all subscribers of this channel
    if saved.msg_type == "summary" {
        let subscribers = queries::get_subscribers_for_channel(&ctx.pool, channel.id).await;
        if let Ok(subs) = subscribers {
            for sub in subs {
                tracing::info!(
                    "Forwarding summary from channel '{}' to subscriber {}:{}",
                    channel.name,
                    sub.subscriber_platform,
                    sub.subscriber_resource,
                );
                enqueue_notification(
                    &ctx.platform_senders,
                    &sub.subscriber_platform,
                    &sub.subscriber_resource,
                    &format!("[summary from {}]\n{}", channel.name, saved.content),
                );
            }
        }
    }
}
