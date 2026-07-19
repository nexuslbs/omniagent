use sqlx::PgPool;
use tracing::{error, warn};

use crate::db::types as queries;
use crate::db::types::{Channel, CompleteThreadStats, Message, MessageNew, Thread};
use crate::llm::{ChatMessage, Usage};
use crate::mcp::AppContext;
use crate::platform::enqueue_notification;
use crate::platform::queue::OutboundEnvelope;

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

/// Check if a message type supports structured templates.
/// Structured types (kanban, cron, Cause) have task metadata that
/// may include a template name for structured execution.
pub fn is_structured_msg_type(msg_type: &str) -> bool {
    matches!(msg_type, "kanban" | "cron" | "Cause")
}

/// Check if a database error is a foreign key violation (PostgreSQL code 23503).
/// These indicate the thread was deleted or the FK constraint was broken
/// the thread should be marked as failed rather than retried.
fn is_fk_violation(e: &crate::error::Error) -> bool {
    if let crate::error::Error::Sqlx(sqlx::Error::Database(ref dberr)) = e {
        return dberr.code().as_deref() == Some("23503");
    }
    false
}

/// Persist a message and detect FK violations that should abort thread processing.
/// Returns the created message on success, or an error variant.
pub enum CreateMessageResult {
    Success(Message),
    FkViolation,
    OtherError(crate::error::Error),
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
                "FK violation inserting message for thread {}: marking thread as failed",
                thread_id
            );
            // Mark the thread as failed
            if let Err(e) = queries::complete_thread(
                pool,
                thread_id,
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
                tracing::warn!("[helpers] Failed to mark thread {} failed after FK violation: {:?}", thread_id, e);
            }
            CreateMessageResult::FkViolation
        }
        Err(e) => CreateMessageResult::OtherError(e),
    }
}

/// Estimate the total character count of all messages in the conversation.
/// This is a rough proxy for prompt tokens (~4 chars per token).
pub fn estimate_chars(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| {
            let mut len = m.content.len();
            if let Some(ref calls) = m.tool_calls {
                for tc in calls {
                    len += tc.function.name.len() + tc.function.arguments.len() + 50;
                    // overhead
                }
            }
            len
        })
        .sum()
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
pub fn count_tokens(
    messages: &[ChatMessage],
    encoding: &str,
    tools: Option<&[serde_json::Value]>,
) -> usize {
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
            warn!(
                "[tokens] Failed to serialize messages for token counting: {}",
                e
            );
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
            warn!(
                "[tokens] Failed to load BPE encoding '{}': {}: falling back to char estimate",
                encoding, e
            );
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
    // Find the index of the last assistant message with tool_calls: this
    // marks the most recent turn boundary. Tool results after it are kept.
    let last_tool_turn_idx = messages
        .iter()
        .rposition(|m| m.role == "assistant" && m.tool_calls.is_some());

    let keep_from = last_tool_turn_idx.unwrap_or(0);

    // Determine truncation level based on iteration
    let (max_body_chars, compact_mode) = match current_iter {
        0..=5 => (usize::MAX, false), // no pruning
        6..=10 => (1000, false),      // moderate truncation
        11..=15 => (300, false),      // aggressive truncation
        _ => (0, true),               // zero content: just the label
    };

    for msg in messages.iter_mut().take(keep_from) {
        if msg.role == "tool" {
            if compact_mode {
                let tool_name = msg.name.as_deref().unwrap_or("unknown");
                msg.content = format!(
                    "[Tool result for `{}`: {} total chars, omitted]",
                    tool_name,
                    msg.content.len()
                );
            } else if msg.content.len() > max_body_chars {
                let preview: String = msg.content.chars().take(200).collect();
                msg.content = format!(
                    "[Pruned tool result: was {} chars] {}",
                    msg.content.len(),
                    preview
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
/// message with `tool_calls`: keeping tool messages after stripping
/// `tool_calls` from the assistant would cause a 400 error.
///
/// Tool messages are removed (not just compacted) because any `role: "tool"`
/// message without a preceding `tool_calls` violates the API contract.
/// The tool names are preserved in the assistant message content so the
/// model still knows what was called.
pub fn compact_old_assistant_messages(messages: &mut Vec<ChatMessage>, keep_recent: usize) {
    loop {
        // Find all tool-calling assistant message positions
        let tool_indices: Vec<usize> = messages
            .iter()
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
                let summary: Vec<String> = calls
                    .iter()
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
        // Continue loop: indices have shifted, re-scan
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

    let entries: Vec<String> = messages
        .iter()
        .enumerate()
        .map(|(i, msg)| {
            let idx = offset + i;
            let role_short = match msg.role.as_str() {
                "assistant" => {
                    if msg.tool_calls.is_some() {
                        "tool_call"
                    } else {
                        "assistant"
                    }
                }
                "tool" => "tool-result",
                "system" => "system",
                "cause" => "cause",
                other => other,
            };
            let meta = if msg.tool_calls.is_some() {
                let names: Vec<&str> = msg
                    .tool_calls
                    .as_ref()
                    .map(|calls| calls.iter().map(|tc| tc.function.name.as_str()).collect())
                    .unwrap_or_default();
                format!(": {}", names.join(", "))
            } else if !msg.content.is_empty() && msg.content.len() < 200 {
                format!(": {}", msg.content)
            } else {
                String::new()
            };
            format!(
                "#{} {} {} ({}{})",
                idx,
                role_short,
                msg.content.len(),
                if meta.is_empty() {
                    ""
                } else {
                    &meta[..meta.len().min(200)]
                },
                ""
            )
        })
        .collect();

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
///    comprise >90% of it, the task cannot meaningfully proceed: return an error.
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
    let system_msgs: Vec<ChatMessage> = messages
        .iter()
        .filter(|m| m.role == "system")
        .cloned()
        .collect();

    let conv_msgs: Vec<&ChatMessage> = messages.iter().filter(|m| m.role != "system").collect();

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
    let conv_start = condensed
        .iter()
        .position(|m| m.role != "system")
        .unwrap_or(condensed.len());

    if conv_start < condensed.len() {
        // Estimate how many chars the old messages (after system) take
        let old_part: usize = condensed[conv_start..]
            .iter()
            .map(|m| m.content.len())
            .sum();
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

/// Enqueue a message for delivery to its platform.
/// Uses the channel's platform and resource_identifier to determine
/// the delivery target. All messages (user and system) follow the same
/// logic: if the channel has no external platform, no delivery happens.
/// seq-0 messages create new posts in the platform channel;
/// seq-1+ messages reply in the platform thread using cause_external_id.
///
/// If cause_external_id is None but the message is seq-1+, fall back to
/// querying the cause message's external_id from the database: this
/// handles system-created threads (cron/kanban) where the seq-0 message
/// was delivered asynchronously and its platform post_id wasn't available
/// at enqueue time.
pub async fn enqueue_delivery(
    ctx: &AppContext,
    saved: &Message,
    channel: &Channel,
    thread: &Thread,
    cause_external_id: Option<String>,
) {
    // If the channel has no platform, there's nowhere to deliver
    let platform = match &channel.platform {
        Some(p) => p.clone(),
        None => return,
    };
    let resource_identifier = match &channel.resource_identifier {
        Some(r) => r.clone(),
        None => return,
    };

    // Look up the platform sender
    let sender = match ctx.platform_senders.get(&platform) {
        Some(s) => s.clone(),
        None => return,
    };

    // Never deliver tool results directly
    if saved.msg_type == "tool-result" {
        return;
    }

    // For non-seq-0 messages lacking a cause_external_id, look up the
    // cause message's external_id from the database. This is needed for
    // system-created threads (cron/kanban) whose seq-0 was delivered
    // asynchronously and had its external_id updated after delivery.
    let resolved_cause_external_id = if cause_external_id.is_none() && saved.thread_sequence > 0 {
        match crate::db::threads::get_cause_message(&ctx.pool, saved.thread_id).await {
            Ok(Some(cause_msg)) => cause_msg.external_id,
            _ => None,
        }
    } else {
        cause_external_id
    };

    // For system-originated threads (kanban, cron, etc.), add a metadata
    // prefix to the seq-0 message so the platform channel lists it with
    // context: "[{type} - {subtype} - Thread: #{id}] {content}".
    let envelope_content = if thread.cause != "user" && saved.thread_sequence == 0 {
        let subtype = saved
            .msg_subtype
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("-");
        format!(
            "[{} - {} - Thread: #{}]\n\n{}",
            saved.msg_type, subtype, saved.thread_id, saved.content
        )
    } else if saved.msg_type == "summary" && platform == "cli" {
        // Quote the seq-0 message for CLI delivery (not needed for Telegram: it uses reply threading)
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

    // ── Secret leak detection: scan outgoing content before delivery ──
    let outgoing_content = {
        let secrets = crate::safety::scan_for_secrets(&envelope_content);
        if !secrets.is_empty() {
            tracing::warn!(
                "⚠️ SECRET LEAK DETECTED in message {} ({}): {:?}",
                saved.id,
                saved.msg_type,
                secrets.iter().map(|s| s.pattern).collect::<Vec<_>>()
            );
            crate::safety::redact_secrets(&envelope_content)
        } else {
            envelope_content
        }
    };

    let envelope = OutboundEnvelope {
        message_id: saved.id,
        resource_identifier,
        content: outgoing_content,
        msg_type: saved.msg_type.clone(),
        msg_subtype: saved.msg_subtype.clone(),
        thread_id: saved.thread_id,
        thread_sequence: saved.thread_sequence,
        cause_external_id: resolved_cause_external_id,
        cause_root_id: {
            // Look up the cause message's metadata for root_id (e.g. Mattermost
            // thread root): used when the user's message was inside an existing
            // thread, so bot replies reference the thread root rather than the
            // intermediate reply (Mattermost doesn't allow nested threads).
            queries::get_cause_message(&ctx.pool, saved.thread_id)
                .await
                .ok()
                .flatten()
                .and_then(|m| {
                    m.metadata
                        .get("root_id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                })
        },
        is_summary: saved.is_summary,
        is_user_thread: thread.cause == "user",
    };

    if let Err(e) = sender.try_send(envelope) {
        tracing::warn!(
            "Failed to enqueue delivery for message {}: {:?}",
            saved.id,
            e
        );
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

/// Enqueue a typing indicator to a platform channel/thread.
/// Broadcasts "bot is typing..." while the agent is processing.
pub async fn enqueue_typing(
    ctx: &AppContext,
    platform: &str,
    resource_identifier: &str,
    parent_id: Option<String>,
) {
    let sender = match ctx.platform_senders.get(platform) {
        Some(s) => s.clone(),
        None => return,
    };

    let envelope = OutboundEnvelope {
        message_id: 0,
        resource_identifier: resource_identifier.to_string(),
        content: String::new(),
        msg_type: "typing".to_string(),
        msg_subtype: None,
        thread_id: 0,
        thread_sequence: 0,
        cause_external_id: parent_id,
        cause_root_id: None,
        is_summary: false,
        is_user_thread: false,
    };

    if let Err(e) = sender.try_send(envelope) {
        tracing::warn!("Failed to enqueue typing: {:?}", e);
    }
}
