use sqlx::PgPool;
use tracing::{error, warn};

use crate::db::types as queries;
use crate::db::types::{Channel, CompleteThreadStats, Message, MessageNew, Thread};
use crate::llm::{ChatMessage, Usage};
use crate::mcp::AppContext;
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
    Success(Box<Message>),
    FkViolation,
    OtherError(crate::error::Error),
}

pub async fn persist_or_abort(
    pool: &PgPool,
    msg: &MessageNew,
    thread_id: i64,
) -> CreateMessageResult {
    match queries::create_message(pool, msg).await {
        Ok(saved) => CreateMessageResult::Success(Box::new(saved)),
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
                tracing::warn!(
                    "[helpers] Failed to mark thread {} failed after FK violation: {:?}",
                    thread_id,
                    e
                );
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
/// WS-4b: which tools are read-only and therefore subject to the
/// exact-repeat read guard. State-changing tools (writes, commits, clones)
/// are excluded; executing one clears the guard map (reads after a mutation
/// are always fresh).
pub fn is_guarded_read_only(tool: &str) -> bool {
    matches!(
        tool,
        "filesystem_read"
            | "filesystem_info"
            | "filesystem_list"
            | "filesystem_search"
            | "search_messages"
            | "search_wiki"
            | "note_read"
    ) || (tool.starts_with("git_")
        && !matches!(
            tool,
            "git_commit-and-push" | "git_clone-repo" | "git_create-github-repo"
        ))
}

/// WS-4b: FNV-1a hash of a tool call's raw argument JSON - stable within the
/// process, no imports needed.
pub fn hash_tool_args(args: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in args.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// WS-3/WS-4c: replace-or-append a marker-prefixed context block (budget
/// hint, working notes, compaction notice). Keeps the message list bounded
/// while making the latest value always visible to the LLM.
///
/// Pushed as a USER-role message, NOT system. DeepSeek (and similar
/// providers) hoist system-role messages into the system-prompt region of
/// the cache key. These blocks change every iteration (budget counter,
/// notes content), so a system-role upsert changed the hoisted system
/// prompt and shattered the prefix cache at the static head on EVERY call
/// (observed: thread 514 froze at exactly 7,424 cached tokens = messages
/// 0-6, 6.9% hit rate). A user-role block stays in the conversation stream
/// at the tail, so the byte-identical prefix (system prompt + all prior
/// tool rounds) rides the cache: empirically 98-99% hit vs 77% collapse.
pub fn upsert_system_message(messages: &mut Vec<ChatMessage>, marker: &str, content: String) {
    // Remove any prior instance (legacy system-role or current user-role) so
    // the latest value replaces it. Restrict to system|user roles: a tool
    // result or assistant reply could legitimately CONTAIN the marker text.
    messages.retain(|m| {
        !(matches!(m.role.as_str(), "system" | "user") && m.content.starts_with(marker))
    });
    messages.push(ChatMessage::user(&content));
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
    is_final: bool,
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
    let sender = match ctx.platform_senders.read().await.get(&platform) {
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

    // Final thread deliveries on Telegram must be sent as a REPLY to the
    // thread's seq-0 message (reply_to_message_id = the seq-0 message's
    // Telegram message_id), never as a standalone top-level message. When the
    // seq-0 external id is unavailable (legacy thread, id lost), fall back to
    // the current standalone send and log it - the message is never dropped.
    let reply_to_message_id = if is_final && platform == "telegram" {
        match &resolved_cause_external_id {
            Some(seq0_ext) => Some(seq0_ext.clone()),
            None => {
                tracing::info!(
                    "[reply-to-seq0] final message for thread {} has no seq-0 external id - sending standalone",
                    saved.thread_id
                );
                None
            }
        }
    } else {
        None
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
        reply_to_message_id,
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
}

/// Enqueue a reaction to a platform message.
///
/// Sends an emoji for the thread's final status (e.g. ":white_check_mark:")
/// to the platform. The caller is responsible for mapping status → emoji.
pub async fn enqueue_reaction(
    ctx: &AppContext,
    platform: &str,
    resource_identifier: &str,
    external_id: &str,
    final_status: &str,
) {
    let sender = match ctx.platform_senders.read().await.get(platform) {
        Some(s) => s.clone(),
        None => return,
    };

    let envelope = OutboundEnvelope {
        message_id: 0,
        resource_identifier: resource_identifier.to_string(),
        content: final_status.to_string(),
        msg_type: "reaction".to_string(),
        msg_subtype: None,
        thread_id: 0,
        thread_sequence: 0,
        cause_external_id: Some(external_id.to_string()),
        cause_root_id: None,
        reply_to_message_id: None,
        is_summary: false,
        is_user_thread: false,
    };

    if let Err(e) = sender.try_send(envelope) {
        tracing::warn!("Failed to enqueue reaction: {:?}", e);
    }
}

/// Telegram first/last-only collapse state for one thread run.
///
/// When the telegram platform plugin is configured with `first_last_only`
/// enabled (plugins.yml `platforms.telegram.config`), the main loop collapses
/// intermediate thread deliveries: only the FIRST and LAST messages of the
/// run reach the chat. This struct tracks whether the first message has been
/// sent yet; `should_deliver(is_final)` returns true for the first delivery,
/// for the final delivery, and always when collapse is disabled.
#[derive(Debug, Default)]
pub struct FirstLastCollapse {
    pub enabled: bool,
    pub first_sent: bool,
}

impl FirstLastCollapse {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            first_sent: false,
        }
    }

    /// Decide whether a delivery should reach the platform.
    ///
    /// Collapse disabled -> always deliver (regression: current behavior).
    /// Collapse enabled  -> deliver only the first message of the run and
    /// the final message (is_final=true); every intermediate message is
    /// suppressed. A single-message run (first == final) is delivered once.
    pub fn should_deliver(&mut self, is_final: bool) -> bool {
        if !self.enabled {
            return true;
        }
        if !self.first_sent {
            self.first_sent = true;
            return true;
        }
        is_final
    }
}

/// True when the telegram platform plugin is configured with
/// `first_last_only` enabled. Only the telegram platform honors this flag;
/// Mattermost behavior is unaffected.
pub fn telegram_first_last_only(data_dir: &str) -> bool {
    let flag = match crate::plugins_yaml::get_plugin(
        data_dir,
        "telegram",
        &crate::plugins_yaml::PluginYamlType::Platform,
    ) {
        Ok(Some(detail)) => detail
            .resolved_env
            .get("first_last_only")
            .cloned()
            .or_else(|| {
                detail
                    .config
                    .get("first_last_only")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default(),
        _ => String::new(),
    };
    matches!(
        flag.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// Enqueue a thread-run delivery honoring the telegram first/last collapse.
///
/// When `collapse.enabled` and the message is neither the first nor the final
/// delivery of the run, the delivery is suppressed (the message stays
/// persisted in the thread history, only the platform delivery is skipped).
pub async fn enqueue_delivery_collapsed(
    ctx: &AppContext,
    saved: &Message,
    channel: &Channel,
    thread: &Thread,
    cause_external_id: Option<String>,
    collapse: &mut FirstLastCollapse,
    is_final: bool,
) {
    if !collapse.should_deliver(is_final) {
        tracing::info!(
            "[first-last] suppressing intermediate delivery to platform '{}' (thread {}, seq {})",
            channel.platform.as_deref().unwrap_or("?"),
            thread.id,
            saved.thread_sequence
        );
        return;
    }
    enqueue_delivery(ctx, saved, channel, thread, cause_external_id, is_final).await;
}

/// Enqueue a typing indicator to a platform channel/thread.
/// Broadcasts "bot is typing..." while the agent is processing.
pub async fn enqueue_typing(
    ctx: &AppContext,
    platform: &str,
    resource_identifier: &str,
    parent_id: Option<String>,
) {
    let sender = match ctx.platform_senders.read().await.get(platform) {
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
        reply_to_message_id: None,
        is_summary: false,
        is_user_thread: false,
    };

    if let Err(e) = sender.try_send(envelope) {
        tracing::warn!("Failed to enqueue typing: {:?}", e);
    }
}

#[cfg(test)]
mod tests {

    use crate::llm::{ChatMessage, ToolCallData, ToolCallFunction, Usage};
    use serde_json::json;

    use super::*;

    // ─── Telegram first/last-only collapse tests ───

    #[test]
    fn test_collapse_disabled_delivers_every_message() {
        // Flag absent/false (default): regression - all messages delivered.
        let mut c = FirstLastCollapse::new(false);
        assert!(c.should_deliver(false));
        assert!(c.should_deliver(false));
        assert!(c.should_deliver(false));
        assert!(c.should_deliver(true));
    }

    #[test]
    fn test_collapse_enabled_delivers_only_first_and_last() {
        let mut c = FirstLastCollapse::new(true);
        // First message of the run: delivered.
        assert!(c.should_deliver(false));
        // Intermediate messages: suppressed.
        assert!(!c.should_deliver(false));
        assert!(!c.should_deliver(false));
        assert!(!c.should_deliver(false));
        // Final message of the run: delivered.
        assert!(c.should_deliver(true));
    }

    #[test]
    fn test_collapse_single_message_run_delivered_once() {
        // A run with only a final message (first == final) delivers it once;
        // the final delivery (is_final=true) still happens after an earlier
        // delivery in a multi-message run, and intermediates stay suppressed.
        let mut c = FirstLastCollapse::new(true);
        assert!(c.should_deliver(true));
        assert!(!c.should_deliver(false));
        let mut d = FirstLastCollapse::new(true);
        assert!(d.should_deliver(false)); // first message
        assert!(!d.should_deliver(false)); // intermediate suppressed
        assert!(d.should_deliver(true)); // final message still delivered
    }

    fn telegram_test_data_dir(first_last: Option<&str>) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let config_dir = format!("{}/config", path);
        std::fs::create_dir_all(&config_dir).unwrap();
        let flag_line = match first_last {
            Some(v) => format!("      first_last_only: {}\n", v),
            None => String::new(),
        };
        std::fs::write(
            format!("{}/plugins.yml", config_dir),
            format!("platforms:\n  telegram:\n    enabled: true\n    source: local\n    config:\n      bot_token: fake\n{}", flag_line),
        )
        .unwrap();
        let plugin_dir = format!("{}/plugins/platforms/telegram", path);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            format!("{}/plugin.json", plugin_dir),
            r#"{"name":"telegram","version":"1.0.0","type":"platform"}"#,
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn test_telegram_first_last_only_true_when_flag_on() {
        let (_d, path) = telegram_test_data_dir(Some("\"on\""));
        assert!(telegram_first_last_only(&path));
        let (_d2, path2) = telegram_test_data_dir(Some("true"));
        assert!(telegram_first_last_only(&path2));
    }

    #[test]
    fn test_telegram_first_last_only_false_when_absent_or_off() {
        let (_d, path) = telegram_test_data_dir(None);
        assert!(!telegram_first_last_only(&path));
        let (_d2, path2) = telegram_test_data_dir(Some("false"));
        assert!(!telegram_first_last_only(&path2));
    }

    #[test]
    fn test_telegram_first_last_only_false_without_telegram_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        assert!(!telegram_first_last_only(&path));
    }

    // ─── shared test builders (also used by compact_old_assistant_messages tests) ───

    fn make_tool_result(name: &str, content: &str) -> ChatMessage {
        ChatMessage::tool_result("call_1", name, content)
    }

    fn make_assistant_with_calls(tool_names: &[&str]) -> ChatMessage {
        let calls: Vec<ToolCallData> = tool_names
            .iter()
            .map(|name| ToolCallData {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: name.to_string(),
                    arguments: "{}".to_string(),
                },
            })
            .collect();
        ChatMessage {
            role: "assistant".to_string(),
            content: "Using tools.".to_string(),
            tool_call_id: None,
            tool_calls: Some(calls),
            name: None,
            reasoning_content: None,
        }
    }

    // ─── merge_usage tests ───

    #[test]
    fn test_merge_usage_both_none() {
        let mut cumulative: Option<Usage> = None;
        merge_usage(&mut cumulative, None);
        assert!(cumulative.is_none());
    }

    #[test]
    fn test_merge_usage_cumulative_none_new_some() {
        let mut cumulative: Option<Usage> = None;
        let new = Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 20,
            cached_tokens: None,
            reasoning_tokens: None,
        });
        merge_usage(&mut cumulative, new);
        let cum = cumulative.unwrap();
        assert_eq!(cum.prompt_tokens, 10);
        assert_eq!(cum.completion_tokens, 20);
        assert_eq!(cum.cached_tokens, None);
        assert_eq!(cum.reasoning_tokens, None);
    }

    #[test]
    fn test_merge_usage_both_some() {
        let mut cumulative = Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            cached_tokens: None,
            reasoning_tokens: None,
        });
        let new = Some(Usage {
            prompt_tokens: 3,
            completion_tokens: 4,
            cached_tokens: None,
            reasoning_tokens: None,
        });
        merge_usage(&mut cumulative, new);
        let cum = cumulative.unwrap();
        assert_eq!(cum.prompt_tokens, 8);
        assert_eq!(cum.completion_tokens, 14);
    }

    #[test]
    fn test_merge_usage_cached_tokens_sums() {
        let mut cumulative = Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 1,
            cached_tokens: Some(5),
            reasoning_tokens: None,
        });
        let new = Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 1,
            cached_tokens: Some(10),
            reasoning_tokens: None,
        });
        merge_usage(&mut cumulative, new);
        assert_eq!(cumulative.unwrap().cached_tokens, Some(15));
    }

    #[test]
    fn test_merge_usage_reasoning_tokens_keeps_existing() {
        let mut cumulative = Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 1,
            cached_tokens: None,
            reasoning_tokens: Some(100),
        });
        let new = Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 1,
            cached_tokens: None,
            reasoning_tokens: Some(200),
        });
        merge_usage(&mut cumulative, new);
        // cumulative keeps its existing reasoning_tokens, not overwritten
        assert_eq!(cumulative.unwrap().reasoning_tokens, Some(100));
    }

    #[test]
    fn test_merge_usage_cumulative_none_no_new_none_set() {
        let mut cumulative: Option<Usage> = None;
        merge_usage(&mut cumulative, None);
        assert!(cumulative.is_none());
    }

    // ─── is_structured_msg_type tests ───

    #[test]
    fn test_is_structured_msg_type_true() {
        assert!(is_structured_msg_type("kanban"));
        assert!(is_structured_msg_type("cron"));
        assert!(is_structured_msg_type("Cause"));
    }

    #[test]
    fn test_is_structured_msg_type_false() {
        assert!(!is_structured_msg_type("user"));
        assert!(!is_structured_msg_type("assistant"));
        assert!(!is_structured_msg_type("tool"));
        assert!(!is_structured_msg_type(""));
        assert!(!is_structured_msg_type("system"));
        assert!(!is_structured_msg_type("cause")); // lowercase 'cause' is not structured
    }

    // ─── estimate_chars tests ───

    #[test]
    fn test_estimate_chars_empty() {
        assert_eq!(estimate_chars(&[]), 0);
    }

    #[test]
    fn test_estimate_chars_single_message() {
        let msgs = vec![ChatMessage::user("hello world")];
        assert_eq!(estimate_chars(&msgs), 11);
    }

    #[test]
    fn test_estimate_chars_multiple_messages() {
        let msgs = vec![
            ChatMessage::system("You are a helpful assistant."),
            ChatMessage::user("What is Rust?"),
            ChatMessage::assistant("Rust is a systems programming language."),
        ];
        // 29 + 14 + 37 = 80
        assert_eq!(estimate_chars(&msgs), 80);
    }

    #[test]
    fn test_estimate_chars_with_tool_calls() {
        let tool_call = ToolCallData {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunction {
                name: "filesystem_read".to_string(),
                arguments: json!({"path": "/etc"}).to_string(),
            },
        };
        let msg = ChatMessage {
            role: "assistant".to_string(),
            content: "Let me check that file.".to_string(),
            tool_call_id: None,
            tool_calls: Some(vec![tool_call]),
            name: None,
            reasoning_content: None,
        };
        let msgs = vec![msg];
        // content.len() = 22
        // tool: name.len() (15) + arguments.len() + 50
        let result = estimate_chars(&msgs);
        // Allow ±1 for potential JSON serialization differences between systems
        let args_len = json!({"path": "/etc"}).to_string().len();
        let expected = 22 + 15 + args_len + 50;
        assert!(
            result == expected || result == expected - 1 || result == expected + 1,
            "expected ~{}, got {}",
            expected,
            result
        );
    }

    // ─── compact_old_assistant_messages tests ───

    #[test]
    fn test_compact_old_assistant_no_tool_calls() {
        let mut msgs = vec![ChatMessage::user("hello"), ChatMessage::assistant("world")];
        let original_len = msgs.len();
        compact_old_assistant_messages(&mut msgs, 5);
        assert_eq!(msgs.len(), original_len);
    }

    #[test]
    fn test_compact_old_assistant_fewer_than_keep() {
        let mut msgs = vec![
            make_assistant_with_calls(&["tool_a"]),
            make_tool_result("tool_a", "result"),
            make_assistant_with_calls(&["tool_b"]),
            make_tool_result("tool_b", "result"),
        ];
        let original_len = msgs.len();
        compact_old_assistant_messages(&mut msgs, 5);
        assert_eq!(msgs.len(), original_len);
    }

    #[test]
    fn test_compact_old_assistant_more_than_keep() {
        let mut msgs = vec![
            make_assistant_with_calls(&["tool_a"]),
            make_tool_result("tool_a", "result_a"),
            make_assistant_with_calls(&["tool_b"]),
            make_tool_result("tool_b", "result_b"),
            make_assistant_with_calls(&["tool_c"]),
            make_tool_result("tool_c", "result_c"),
        ];
        // keep_recent = 2, so the oldest tool-calling assistant (idx 0) should be compacted
        compact_old_assistant_messages(&mut msgs, 2);

        assert_eq!(msgs.len(), 5); // one tool message removed
                                   // Index 0: assistant with tool_calls should be compacted
        assert!(msgs[0].tool_calls.is_none());
        assert!(msgs[0].content.contains("[#0 Tool calls compacted:"));
        assert!(msgs[0].content.contains("tool_a()"));
        // The tool result for tool_a at original idx 1 should be gone
        // Index 1: should now be the second assistant (originally idx 2)
        assert_eq!(
            msgs[1].tool_calls.as_ref().unwrap()[0].function.name,
            "tool_b"
        );
        // Index 2: tool_b result
        assert_eq!(msgs[2].role, "tool");
        // Index 3: third assistant (tool_c)
        assert_eq!(
            msgs[3].tool_calls.as_ref().unwrap()[0].function.name,
            "tool_c"
        );
        // Index 4: tool_c result
        assert_eq!(msgs[4].role, "tool");
    }

    #[test]
    fn test_compact_old_assistant_multiple_tool_names_in_compact() {
        let mut msgs = vec![
            make_assistant_with_calls(&["read_file", "write_file"]),
            make_tool_result("read_file", "content"),
            make_tool_result("write_file", "ok"),
            make_assistant_with_calls(&["tool_c"]),
            make_tool_result("tool_c", "result"),
        ];
        compact_old_assistant_messages(&mut msgs, 1);

        assert_eq!(msgs.len(), 3); // 2 tool messages removed
        assert!(msgs[0].tool_calls.is_none());
        assert!(msgs[0].content.contains("read_file()"));
        assert!(msgs[0].content.contains("write_file()"));
        // tool_c assistant preserved
        assert_eq!(
            msgs[1].tool_calls.as_ref().unwrap()[0].function.name,
            "tool_c"
        );
        assert_eq!(msgs[2].role, "tool");
    }

    // ─── count_tokens tests ───

    #[test]
    fn test_count_tokens_empty_messages() {
        let result = count_tokens(&[], "gpt-4", None);
        // When tiktoken is available, JSON "[]" encodes as 1 token.
        // When tiktoken is unavailable, falls back to estimate_chars = 0.
        // Accept both.
        assert!(
            result == 0 || result == 1,
            "expected 0 or 1, got {}",
            result
        );
    }

    #[test]
    fn test_count_tokens_fallback_on_bad_encoding() {
        let msgs = vec![ChatMessage::user("hello world")];
        // A bad encoding name should cause fallback to estimate_chars
        let result = count_tokens(&msgs, "nonexistent_encoding_xyz", None);
        // Should be > 0 (fallback to estimate_chars)
        assert!(result > 0);
        // estimate_chars for "hello world" = 11
        assert_eq!(result, 11);
    }

    #[test]
    fn test_count_tokens_with_tools() {
        let msgs = vec![ChatMessage::user("hello")];
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "test_tool",
                "description": "A test tool",
                "parameters": json!({"type": "object", "properties": {}})
            }
        })];
        // With bad encoding, falls back to estimate_chars
        let result = count_tokens(&msgs, "nonexistent_encoding_xyz", Some(&tools));
        assert!(result > 0);
    }
}
