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

/// WS-4b: FNV-1a hash of a tool call's raw argument JSON — stable within the
/// process, no imports needed.
pub fn hash_tool_args(args: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in args.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// WS-3/WS-4c: replace-or-append a marker-prefixed system message (budget
/// hint, working notes, compaction notice). Keeps the message list bounded
/// while making the latest value always visible to the LLM.
pub fn upsert_system_message(messages: &mut Vec<ChatMessage>, marker: &str, content: String) {
    messages.retain(|m| m.role != "system" || !m.content.starts_with(marker));
    messages.push(ChatMessage::system(&content));
}

/// Tools whose results ARE the agent's working memory (file contents, listings,
/// search hits, query rows). These are preserved aggressively: the last
/// `read_keep_last` (PruneConfig) are kept in full and older ones keep a
/// generous excerpt (`read_excerpt_chars`). Zeroing them forces the agent to
/// re-read the same files — the #1 budget killer (observed: thread 700 burned
/// 117 docker_compose+sed windows of the SAME line ranges because prune zeroed
/// every earlier read from context). All limits come from settings.yml.
fn is_read_type_tool(name: &str) -> bool {
    name.starts_with("filesystem_read")
        || name.starts_with("filesystem_list")
        || name.starts_with("filesystem_search")
        || name.starts_with("filesystem_info")
        || name.starts_with("query_database")
        || name.starts_with("search_messages")
        || name.starts_with("search_wiki")
        || name.starts_with("skills_view")
        || name.starts_with("git_status")
        || name.starts_with("git_run-command")
}

/// Auto-note a read-type tool result into the thread's durable
/// `auto-notes.md` before pruning removes it from context. The main loop
/// re-injects `auto-notes.md` (tail, most recent first) every iteration, so
/// the content survives compaction/pruning even when the model never calls
/// `prompt_note-write` itself (observed: thread 700 wrote ZERO notes, then
/// re-read the same file ranges 117 times because its context had been
/// emptied). The model's own `notes.md` stays untouched — engine entries
/// live in a separate file so they never crowd out hand-written notes.
///
/// Entries are appended with a `[engine:auto-note]` prefix and the file is
/// capped at `auto_note_max_chars` (oldest entries dropped).
fn auto_note_read(
    dir: &std::path::Path,
    tool: &str,
    content: &str,
    entry_chars: usize,
    max_chars: usize,
) {
    if content.trim().is_empty() {
        return;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let file = dir.join("auto-notes.md");
    let entry = format!(
        "## [engine:auto-note {tool}]\n{}\n",
        content.chars().take(entry_chars).collect::<String>()
    );
    let existing = std::fs::read_to_string(&file).unwrap_or_default();
    let mut merged = format!("{existing}{entry}");
    // Cap: drop oldest auto-note blocks (only entries carrying the
    // [engine:auto-note] marker are candidates — hand-written notes live in
    // notes.md, a different file, so they are never touched here).
    while merged.chars().count() > max_chars {
        match merged.find("## [engine:auto-note") {
            Some(idx) => {
                let after = &merged[idx..];
                let next = after.find("\n## [engine:auto-note").map(|i| i + 1);
                match next {
                    Some(end) => merged = format!("{}{}", &merged[..idx], &after[end..]),
                    None => {
                        // Only one auto-note block left and still over: trim its tail.
                        let keep: String = after.chars().take(entry_chars).collect();
                        merged = format!("{}{}", &merged[..idx], keep);
                        break;
                    }
                }
            }
            None => break,
        }
    }
    let _ = std::fs::write(&file, merged);
}

/// Pruning limits, all sourced from settings.yml — no hardcoded limits in
/// code. `hard_budget` triggers a pruning pass; `soft_budget` is the target
/// size compaction stops at. Read-type tool results (the agent's working
/// memory) keep the last `read_keep_last` in full; older ones are excerpted
/// to `read_excerpt_chars` and auto-noted into the thread's `auto-notes.md`
/// (capped at `auto_note_max_chars`).
#[derive(Debug, Clone, Copy)]
pub struct PruneConfig {
    pub hard_budget: usize,
    pub soft_budget: usize,
    pub read_keep_last: usize,
    pub read_excerpt_chars: usize,
    pub auto_note_max_chars: usize,
    pub auto_note_entry_chars: usize,
}

pub fn prune_old_tool_results(
    messages: &mut [ChatMessage],
    current_iter: u32,
    thread_dir: Option<&std::path::Path>,
    cfg: PruneConfig,
) {
    // Budget gate: prune ONLY when the hard threshold is exceeded, then
    // compact until the size drops below the soft threshold — keeping as
    // much recent content as possible. (Previously this zeroed ALL old tool
    // results at iteration 16+, which emptied the agent's memory of what it
    // had read and caused the re-read death spiral: thread 700 burned 117
    // docker_compose+sed windows of the SAME line ranges, zero commits.)
    let current_size: usize = messages.iter().map(|m| m.content.len()).sum();
    if current_size <= cfg.hard_budget {
        return;
    }

    // WS-2: durable context dump — before this pass destroys/truncates any
    // tool result bodies, append digests of the prune-window candidates.
    if let Some(dir) = thread_dir {
        let keep_from = messages
            .iter()
            .rposition(|m| m.role == "assistant" && m.tool_calls.is_some())
            .map(|i| i + 1)
            .unwrap_or(0);
        for msg in messages.iter().take(keep_from) {
            if msg.role == "tool" && !msg.content.is_empty() {
                let tool = msg.name.clone().unwrap_or_default();
                crate::agent::context_dump::append(dir, current_iter, &tool, "", &msg.content);
            }
        }
    }
    // Find the index of the last assistant message with tool_calls: this
    // marks the most recent turn boundary. Tool results after it are kept.
    let last_tool_turn_idx = messages
        .iter()
        .rposition(|m| m.role == "assistant" && m.tool_calls.is_some());

    let keep_from = last_tool_turn_idx.unwrap_or(0);

    // Pass 1 (NEW → OLD): mark the LAST cfg.read_keep_last read-type tool results
    // as "keep full" so the most recent reads always survive pruning.
    let mut read_kept = 0usize;
    let mut keep_read: Vec<usize> = Vec::new();
    for idx in (0..keep_from).rev() {
        if messages[idx].role != "tool" {
            continue;
        }
        if is_read_type_tool(&messages[idx].name.clone().unwrap_or_default()) {
            keep_read.push(idx);
            read_kept += 1;
            if read_kept >= cfg.read_keep_last {
                break;
            }
        }
    }

    // Pass 2 (OLD → NEW): prune oldest first until the total size drops below
    // the soft budget. Kept read results (most recent) are skipped.
    for idx in 0..keep_from {
        if messages[idx].role != "tool" {
            continue;
        }
        if keep_read.contains(&idx) {
            continue; // keep the most recent read results in full
        }
        let tool_name = messages[idx].name.clone().unwrap_or_default();
        let is_read = is_read_type_tool(&tool_name);
        // Auto-note read content BEFORE truncation so it survives in the
        // durable auto-notes.md even after context pruning removes it.
        if is_read {
            if let Some(dir) = thread_dir {
                auto_note_read(
                    dir,
                    &tool_name,
                    &messages[idx].content,
                    cfg.auto_note_entry_chars,
                    cfg.auto_note_max_chars,
                );
            }
            let total = messages[idx].content.chars().count();
            if total > cfg.read_excerpt_chars {
                let half = cfg.read_excerpt_chars / 2;
                let head: String = messages[idx].content.chars().take(half).collect();
                let tail: String = messages[idx]
                    .content
                    .chars()
                    .rev()
                    .take(half)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                messages[idx].content =
                    format!("[Pruned tool result: was {total} chars] {head}\n...\n{tail}");
            }
        } else {
            messages[idx].content = format!(
                "[Tool result for `{}`: {} total chars, omitted]",
                tool_name,
                messages[idx].content.len()
            );
        }
        let after_size: usize = messages.iter().map(|m| m.content.len()).sum();
        if after_size <= cfg.soft_budget {
            break;
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
        is_summary: false,
        is_user_thread: false,
    };

    if let Err(e) = sender.try_send(envelope) {
        tracing::warn!("Failed to enqueue reaction: {:?}", e);
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
        is_summary: false,
        is_user_thread: false,
    };

    if let Err(e) = sender.try_send(envelope) {
        tracing::warn!("Failed to enqueue typing: {:?}", e);
    }
}

#[cfg(test)]
mod tests {
    /// Test-only prune config builder (hardcoded values are fine in tests;
    /// production limits come from settings.yml).
    fn test_prune_cfg(hard: usize, soft: usize) -> super::PruneConfig {
        super::PruneConfig {
            hard_budget: hard,
            soft_budget: soft,
            read_keep_last: 3,
            read_excerpt_chars: 2000,
            auto_note_max_chars: 24_000,
            auto_note_entry_chars: 3000,
        }
    }

    #[test]
    fn prune_dumps_digest_before_destroy() {
        let tmp = std::env::temp_dir().join(format!("prune-dump-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let mut messages = vec![
            ChatMessage::tool_result("call-1", "filesystem_read", &"x".repeat(20000)),
            ChatMessage {
                role: "assistant".to_string(),
                content: "[tool calls]".to_string(),
                tool_call_id: None,
                tool_calls: Some(vec![]),
                name: None,
                reasoning_content: None,
            },
        ];
        super::prune_old_tool_results(&mut messages, 7, Some(&tmp), test_prune_cfg(10_000, 5_000));
        let dump_path = tmp.join("context-7.json");
        let content = std::fs::read_to_string(&dump_path).unwrap_or_default();
        assert!(
            content.contains("filesystem_read"),
            "dump missing tool name; content: {:?}",
            &content[..content.len().min(200)]
        );
        for line in content.lines() {
            let v: serde_json::Value =
                serde_json::from_str(line).expect("dump line must be valid JSON");
            assert!(v["tool"].as_str().is_some());
            assert!(v["head"].as_str().is_some());
            assert!(v["tail"].as_str().is_some());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    use crate::llm::{ChatMessage, ToolCallData, ToolCallFunction, Usage};
    use serde_json::json;

    use super::*;

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

    // ─── prune_old_tool_results tests ───

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

    #[test]
    fn test_prune_old_tool_results_under_hard_budget_no_pruning() {
        // Budget gate: when the total size is under the hard threshold,
        // nothing is pruned regardless of iteration.
        let mut msgs = vec![
            ChatMessage::user("do something"),
            make_assistant_with_calls(&["tool_a"]),
            make_tool_result("tool_a", "a".repeat(2000).as_str()),
            ChatMessage::assistant("Done."),
        ];
        let original_len = msgs.len();
        let original_contents: Vec<String> = msgs.iter().map(|m| m.content.clone()).collect();
        prune_old_tool_results(&mut msgs, 100, None, test_prune_cfg(1_000_000, 500_000));
        // Same length and content unchanged (total size is well under hard)
        assert_eq!(msgs.len(), original_len);
        for (i, m) in msgs.iter().enumerate() {
            assert_eq!(m.content, original_contents[i]);
        }
    }

    #[test]
    fn test_prune_old_tool_results_over_hard_compacts_to_soft() {
        // When over the hard budget, old non-read results are pruned until
        // the total drops below the soft budget.
        let long_content = "a".repeat(2000);
        let mut msgs = vec![
            ChatMessage::user("do something"),
            make_assistant_with_calls(&["tool_a"]),
            make_tool_result("tool_a", &long_content),
            make_assistant_with_calls(&["tool_b"]),
            make_tool_result("tool_b", "short"),
            ChatMessage::assistant("Done."),
        ];
        prune_old_tool_results(&mut msgs, 8, None, test_prune_cfg(1000, 500));

        // tool_a result at index 2 is before keep_from (3) and over budget,
        // so it gets truncated to a stub.
        assert!(msgs[2].content.starts_with("[Tool result for `tool_a`"));
        // tool_b result at index 4 is after keep_from, so it stays unchanged
        assert_eq!(msgs[4].content, "short");
        // Total size is back under the soft budget.
        let total: usize = msgs.iter().map(|m| m.content.len()).sum();
        assert!(total <= 500 + 500); // soft + slack for the new stub text
    }

    #[test]
    fn test_prune_old_tool_results_non_tool_messages_unchanged() {
        let mut msgs = vec![
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi"),
            ChatMessage::system("beep"),
        ];
        let original_len = msgs.len();
        prune_old_tool_results(&mut msgs, 10, None, test_prune_cfg(10, 5));
        assert_eq!(msgs.len(), original_len);
    }

    #[test]
    fn test_prune_old_tool_results_no_assistant_with_calls() {
        let mut msgs = vec![
            ChatMessage::user("hello"),
            make_tool_result("tool_a", "some result"),
            ChatMessage::assistant("Done."),
        ];
        let original_len = msgs.len();
        prune_old_tool_results(&mut msgs, 10, None, test_prune_cfg(10, 5));
        // No assistant with tool_calls, so last_tool_turn_idx is None, keep_from = 0
        // Nothing is before index 0, so nothing happens
        assert_eq!(msgs.len(), original_len);
    }

    #[test]
    fn test_prune_keeps_recent_read_results_in_full() {
        // Read-type tool results in the last 3 positions before the current
        // turn boundary must survive intact even when over budget.
        let mut msgs = vec![
            ChatMessage::user("read files"),
            make_assistant_with_calls(&["filesystem_read"]),
            make_tool_result("filesystem_read", "alpha content"),
            make_assistant_with_calls(&["filesystem_read"]),
            make_tool_result("filesystem_read", "beta content"),
            make_assistant_with_calls(&["filesystem_read"]),
            make_tool_result("filesystem_read", "gamma content"),
            make_assistant_with_calls(&["docker_compose"]),
            make_tool_result("docker_compose", "current turn output"),
            ChatMessage::assistant("Done."),
        ];
        // Tiny budgets force pruning of everything before the last turn,
        // but the three recent reads must be preserved in full.
        prune_old_tool_results(&mut msgs, 30, None, test_prune_cfg(10, 5));
        let contents: Vec<String> = msgs
            .iter()
            .filter(|m| m.role == "tool")
            .map(|m| m.content.clone())
            .collect();
        assert!(contents.contains(&"alpha content".to_string()));
        assert!(contents.contains(&"beta content".to_string()));
        assert!(contents.contains(&"gamma content".to_string()));
        assert!(contents.contains(&"current turn output".to_string()));
    }

    #[test]
    fn test_prune_excerpts_old_read_results_over_budget() {
        // Read results OLDER than the last 3 get a head+tail excerpt instead
        // of a zero-content stub. FIVE reads in the window (indices 2, 4, 6,
        // 8, 10): the 3 most recent survive in full, the 2 oldest are
        // excerpted.
        let long_read = "x".repeat(5000);
        let mut msgs = vec![
            ChatMessage::user("read old file"),
            make_assistant_with_calls(&["filesystem_read"]),
            make_tool_result("filesystem_read", &long_read),
            make_assistant_with_calls(&["filesystem_read"]),
            make_tool_result("filesystem_read", "recent read 1"),
            make_assistant_with_calls(&["filesystem_read"]),
            make_tool_result("filesystem_read", "recent read 2"),
            make_assistant_with_calls(&["filesystem_read"]),
            make_tool_result("filesystem_read", "recent read 3"),
            make_assistant_with_calls(&["filesystem_read"]),
            make_tool_result("filesystem_read", "recent read 4"),
            ChatMessage::assistant("Done."),
        ];
        prune_old_tool_results(&mut msgs, 30, None, test_prune_cfg(100, 50));
        // keep_from = index of last assistant with calls = 9. The oldest read
        // at index 2 is pruned to an excerpt; the 3 most recent reads in the
        // window (4, 6, 8) are kept intact, as is the current-turn read at 10.
        let old = &msgs[2];
        assert!(old
            .content
            .starts_with("[Pruned tool result: was 5000 chars]"));
        assert!(old.content.contains("...\n"));
        assert!(old.content.contains("xxxxx")); // head preserved
        assert_eq!(msgs[4].content, "recent read 1");
        assert_eq!(msgs[6].content, "recent read 2");
        assert_eq!(msgs[8].content, "recent read 3");
        assert_eq!(msgs[10].content, "recent read 4");
    }

    #[test]
    fn test_prune_still_zeroes_old_non_read_results() {
        // Non-read tools over budget get the zero-stub treatment (their
        // content is not working memory).
        let long_content = "y".repeat(3000);
        let mut msgs = vec![
            ChatMessage::user("run command"),
            make_assistant_with_calls(&["docker_compose"]),
            make_tool_result("docker_compose", &long_content),
            make_assistant_with_calls(&["git_status"]),
            make_tool_result("git_status", "short"),
            ChatMessage::assistant("Done."),
        ];
        prune_old_tool_results(&mut msgs, 30, None, test_prune_cfg(100, 50));
        // docker_compose is NOT a read-type tool (in the conservative list),
        // so over budget it is zeroed to the stub label.
        assert!(msgs[2]
            .content
            .starts_with("[Tool result for `docker_compose`"));
        assert!(msgs[2].content.contains("omitted"));
        assert_eq!(msgs[4].content, "short");
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

    // ─── build_message_metadata_block tests ───

    #[test]
    fn test_build_message_metadata_block_empty() {
        let result = build_message_metadata_block(&[], 0);
        assert!(result.is_empty());
    }

    #[test]
    fn test_build_message_metadata_block_single_user() {
        let msgs = vec![ChatMessage::user("hello")];
        let result = build_message_metadata_block(&msgs, 0);
        assert!(result.starts_with("==== Old Messages Compacted ===="));
        assert!(result.contains("#0 user"));
        assert!(result.contains("hello"));
    }

    #[test]
    fn test_build_message_metadata_block_assistant_with_tool_calls() {
        let msg = make_assistant_with_calls(&["filesystem_read", "search_files"]);
        let msgs = vec![msg];
        let result = build_message_metadata_block(&msgs, 0);
        assert!(result.contains("tool_call"));
        assert!(result.contains("filesystem_read, search_files"));
    }

    #[test]
    fn test_build_message_metadata_block_tool_result() {
        let msgs = vec![make_tool_result("my_tool", "some output")];
        let result = build_message_metadata_block(&msgs, 0);
        assert!(result.contains("tool-result"));
    }

    #[test]
    fn test_build_message_metadata_block_with_offset() {
        let msgs = vec![ChatMessage::user("hi"), ChatMessage::assistant("hello")];
        let result = build_message_metadata_block(&msgs, 5);
        assert!(result.contains("#5 user"));
        assert!(result.contains("#6 assistant"));
    }

    // ─── condense_messages tests ───

    #[test]
    fn test_condense_messages_system_too_large() {
        let large_system = ChatMessage::system(&"x".repeat(1000));
        let msgs = vec![large_system, ChatMessage::user("hello")];
        let result = condense_messages(msgs, 100, 2, 500);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Always-keep messages"));
    }

    #[test]
    fn test_condense_messages_empty_conversation() {
        let msgs = vec![ChatMessage::system("You are a bot.")];
        let result = condense_messages(msgs.clone(), 100, 2, 1000).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "You are a bot.");
    }

    #[test]
    fn test_condense_messages_normal_condensation() {
        let mut msgs = vec![ChatMessage::system("You are a bot.")];
        // Add some conversation messages, no tool_calls so none are "turns"
        msgs.push(ChatMessage::user("hello"));
        msgs.push(ChatMessage::assistant("hi there"));
        msgs.push(ChatMessage::user("what is rust?"));
        msgs.push(ChatMessage::assistant("a language"));

        let result = condense_messages(msgs, 1000, 2, 10000).unwrap();
        // Since there are no assistant messages with tool_calls, keep_turns doesn't find any
        // So keep_from = 0, and early_conv is empty, metadata is empty
        // Nothing is condensed
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn test_condense_messages_with_tool_turns() {
        let mut msgs = vec![ChatMessage::system("You are a bot.")];
        msgs.push(ChatMessage::user("first request"));
        msgs.push(make_assistant_with_calls(&["tool_a"]));
        msgs.push(make_tool_result("tool_a", "result1"));
        msgs.push(ChatMessage::assistant("First done."));
        msgs.push(ChatMessage::user("second request"));
        msgs.push(make_assistant_with_calls(&["tool_b"]));
        msgs.push(make_tool_result("tool_b", "result2"));
        msgs.push(ChatMessage::assistant("Second done."));

        let result = condense_messages(msgs, 10000, 1, 100000).unwrap();
        // keep_turns=1, so the last assistant with tool_calls (idx 6) is kept
        // Everything before idx 6 is condensed into metadata block
        // System message is kept
        assert!(result[0].role == "system");
        // There should be a system message with metadata (condensed content)
        assert!(result.len() < 9); // condensed
    }

    #[test]
    fn test_condense_messages_budget_exceeded_additional_trimming() {
        let mut msgs = vec![ChatMessage::system("sys")];
        // Add a lot of user/assistant messages
        for i in 0..5 {
            msgs.push(ChatMessage::user(&format!("user message {}", i)));
            msgs.push(make_assistant_with_calls(&[&format!("tool_{}", i)]));
            msgs.push(make_tool_result(&format!("tool_{}", i), &"x".repeat(500)));
            msgs.push(ChatMessage::assistant(&format!("done {}", i)));
        }

        let result = condense_messages(msgs, 100, 1, 10000).unwrap();
        // With old_msg_budget=100, the old messages will need additional trimming
        assert!(!result.is_empty());
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
