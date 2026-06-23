//! Agent module — parallel channel processing supervisor.
//!
//! The agent supervisor runs a loop that:
//! 1. Recovers stale `processing` threads on startup.
//! 2. Lists all channels and spawns a dedicated `channel_handler` task for
//!    each channel that isn't already running.
//! 3. Checks for stopped channels and cancels their handlers via
//!    `CancellationToken`.
//! 4. Sleeps 5 seconds between iterations.
//!
//! Each `channel_handler` independently polls its channel for pending
//! threads, processes them via the LLM, and respects cancellation
//! requests from the `/stop` HTTP endpoint.

use anyhow::Result;
use sql_forge::sql_forge;
use sqlx::FromRow;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::db::types as queries;
use crate::llm::{ChatMessage, CompletionRequest, LLMClient, Usage};
use crate::models::{Channel, Message, MessageNew, Thread};
use crate::platform::queue::OutboundEnvelope;
use crate::platform::enqueue_notification;

/// Maximum total characters of tool results in conversation history before
/// old tool results are pruned (Layer 3 compression).
const TOOL_RESULT_HISTORY_BUDGET: usize = 120_000;
/// Maximum number of times the LLM can try to end without completing all subtasks
/// before the thread is marked as failed.
use crate::context_builder::{BlockPriority, ContextAssemblyMeta, ContextBlock, ContextBuilder};
use crate::vectorizer::Vectorizer;
use crate::mcp::{
    truncate_content, AppContext, McpRegistry, McpToolCall, DEFAULT_MAX_TOOL_OUTPUT_CHARS,
};
use crate::prompt_builder::format_subtask_section;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub llm_api_key: String,
    pub llm_model: String,
    pub llm_provider: String,
    pub llm_base_url: String,
    pub max_tokens: u32,
    pub temperature: f32,
    #[expect(dead_code)]
    pub summarize_after_days: u32,
    pub max_iterations: u32,
    /// Number of threads per half-window for summary generation.
    /// A summary is generated every 2*summary_window completed threads.
    pub summary_window: u32,
    /// Max tokens for the summary generation LLM call.
    pub summary_tokens: u32,
    /// Days before old messages and summaries are deleted.
    pub delete_after_days: u32,
    /// When true, the agent generates a plan/context before execution.
    pub prompt_plan_enabled: bool,
    /// Max output tokens for the planning LLM call.
    pub prompt_plan_max_tokens: u32,
    /// Number of refinement iterations for the plan (0 = disabled, one-shot).
    pub prompt_graph_iterations: u32,
}

impl AgentConfig {
    /// Load agent configuration from environment variables.
    ///
    /// # Env vars
    /// - `LLM_API_KEY` — API key for the LLM provider
    /// - `LLM_MODEL` — Model name (default: "gpt-4")
    /// - `LLM_PROVIDER` — Provider name (default: "openai")
    /// - `LLM_BASE_URL` — Base URL for the API (optional per-provider default)
    /// - `MAX_TOKENS` — Max tokens per response (default: 4096)
    /// - `TEMPERATURE` — Sampling temperature (default: 0.7)
    /// - `SUMMARIZE_AFTER_DAYS` — Days before auto-summarization (default: 7)
    /// - `MAX_ITERATIONS` — Max agent turns per thread before skipping (default: 60)
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            llm_api_key: std::env::var("LLM_API_KEY")
                .or_else(|_| {
                    let provider = std::env::var("LLM_PROVIDER").unwrap_or_default();
                    if provider == "deepseek" {
                        std::env::var("DEEPSEEK_API_KEY")
                    } else {
                        Err(std::env::VarError::NotPresent)
                    }
                })
                .unwrap_or_default(),
            llm_model: std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4".to_string()),
            llm_provider: std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "openai".to_string()),
            llm_base_url: std::env::var("LLM_BASE_URL").unwrap_or_default(),
            max_tokens: std::env::var("MAX_TOKENS")
                .unwrap_or_else(|_| "4096".to_string())
                .parse()
                .unwrap_or(4096),
            temperature: std::env::var("TEMPERATURE")
                .unwrap_or_else(|_| "0.7".to_string())
                .parse()
                .unwrap_or(0.7),
            summarize_after_days: std::env::var("SUMMARIZE_AFTER_DAYS")
                .unwrap_or_else(|_| "7".to_string())
                .parse()
                .unwrap_or(7),
            max_iterations: std::env::var("MAX_ITERATIONS")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .unwrap_or(60),
            summary_window: std::env::var("SUMMARY_WINDOW")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .unwrap_or(10),
            summary_tokens: std::env::var("SUMMARY_TOKENS")
                .unwrap_or_else(|_| "4096".to_string())
                .parse()
                .unwrap_or(4096),
            delete_after_days: std::env::var("DELETE_AFTER_DAYS")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .unwrap_or(30),
            prompt_plan_enabled: std::env::var("PROMPT_PLAN_ENABLED")
                .unwrap_or_else(|_| "false".to_string())
                .parse::<bool>()
                .unwrap_or(false),
            prompt_plan_max_tokens: std::env::var("PROMPT_PLAN_MAX_TOKENS")
                .unwrap_or_else(|_| "2048".to_string())
                .parse()
                .unwrap_or(2048),
            prompt_graph_iterations: std::env::var("PROMPT_GRAPH_ITERATIONS")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .unwrap_or(0),
        })
    }
}

/// The core agent that supervises per-channel message processing.
pub struct Agent {
    pub pool: PgPool,
    pub config: AgentConfig,
    pub llm: Arc<LLMClient>,
    pub mcp: McpRegistry,
    pub ctx: AppContext,
}

impl Agent {
    /// Create a new agent from a database pool and configuration.
    ///
    /// An LLM client is built from the agent config, falling back to
    /// environment-level defaults for any unset values.
    pub fn new(pool: PgPool, config: AgentConfig, mcp: McpRegistry, ctx: AppContext) -> Self {
        let env_cfg = crate::llm::LLMConfig::from_env();
        let llm_config = crate::llm::LLMConfig {
            provider: if config.llm_provider.is_empty() {
                env_cfg.provider
            } else {
                crate::llm::ProviderId::new(&config.llm_provider)
            },
            api_key: if config.llm_api_key.is_empty() {
                env_cfg.api_key
            } else {
                config.llm_api_key.clone()
            },
            base_url: if config.llm_base_url.is_empty() {
                env_cfg.base_url
            } else {
                config.llm_base_url.clone()
            },
            model: config.llm_model.clone(),
            api_mode: env_cfg.api_mode,
            max_tokens: config.max_tokens,
            temperature: config.temperature,
        };
        let llm = Arc::new(LLMClient::new(llm_config));
        Self {
            pool,
            config,
            llm,
            mcp,
            ctx,
        }
    }

    /// Run the agent supervisor loop.
    ///
    /// This method:
    /// 1. Recovers stale `processing` threads on startup.
    /// 2. Continuously polls all channels.
    /// 3. Spawns a [`channel_handler`] for each new channel.
    /// 4. Cancels handlers for stopped channels.
    /// 5. Sleeps 5 seconds between iterations.
    ///
    /// The `cancel_tokens` map is shared with the HTTP server so the
    /// `/stop/{channel_id}` endpoint can cancel channel handlers.
    pub async fn run(self, cancel_tokens: Arc<Mutex<HashMap<i64, CancellationToken>>>) {
        let pool = self.pool;
        let llm = self.llm;
        let config = self.config;
        let mcp = self.mcp;
        let ctx = self.ctx;

        loop {
            let channels = match queries::find_all_channels(&pool).await {
                Ok(ch) => ch,
                Err(e) => {
                    error!("Failed to list channels: {:?}", e);
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            let mut tokens = cancel_tokens.lock().await;

            // Collect channel IDs before iterating to avoid borrow conflicts
            let channel_ids: Vec<i64> = channels.iter().map(|c| c.id).collect();

            // Spawn handlers for channels not yet being processed
            for &channel_id in &channel_ids {
                if let std::collections::hash_map::Entry::Vacant(e) = tokens.entry(channel_id) {
                    // Skip spawning if the channel is closed — it will be spawned
                    // when the channel is opened via the /open endpoint
                    if let Ok(true) = queries::is_channel_closed(&pool, channel_id).await {
                        continue;
                    }

                    let token = CancellationToken::new();
                    let handler_token = token.clone();
                    e.insert(token);

                    let pool = pool.clone();
                    let llm = llm.clone();
                    let config = config.clone();
                    let mcp_clone = mcp.clone();
                    let ctx_clone = ctx.clone();

                    tokio::spawn(async move {
                        channel_handler(
                            pool,
                            llm,
                            config,
                            mcp_clone,
                            ctx_clone,
                            channel_id,
                            handler_token,
                        )
                        .await;
                    });

                    info!(
                        "Spawned channel handler for channel {} ({})",
                        channel_id,
                        channels
                            .iter()
                            .find(|c| c.id == channel_id)
                            .map(|c| c.name.as_str())
                            .unwrap_or("unknown")
                    );
                }
            }

            // Cancel handlers for channels that have been stopped
            let stopped_ids: Vec<i64> = tokens.keys().copied().collect();
            for &channel_id in &stopped_ids {
                if let Some(token) = tokens.get(&channel_id) {
                    if !token.is_cancelled() {
                        if let Ok(true) = queries::is_channel_closed(&pool, channel_id).await
                        {
                            info!(
                                "Channel {} has been closed, cancelling handler",
                                channel_id
                            );
                            token.cancel();
                        }
                    }
                }
            }

            // Remove cancelled tokens so the next iteration can spawn fresh handlers
            // for channels that are no longer stopped.
            tokens.retain(|_, t| !t.is_cancelled());

            // Prune tokens for channels that no longer exist in the DB
            let active_ids: Vec<i64> = channels.iter().map(|c| c.id).collect();
            tokens.retain(|k, _| active_ids.contains(k));

            drop(tokens);
            sleep(Duration::from_secs(5)).await;
        }
    }
}

/// Per-channel thread processing loop.
///
/// This function runs as a separate tokio task for each channel. It:
/// 1. Checks cancellation at the start of each iteration.
/// 2. Checks if the channel has been stopped.
/// 3. Fetches pending threads for this channel.
/// 4. Processes each thread via [`process_thread`].
/// 5. Sleeps 1 second between iterations.
///
/// The loop exits cleanly when the cancellation token is triggered or
/// when the channel is marked as stopped in the database.
async fn channel_handler(
    pool: PgPool,
    llm: Arc<LLMClient>,
    config: AgentConfig,
    mcp: McpRegistry,
    ctx: AppContext,
    channel_id: i64,
    cancel: CancellationToken,
) {
    info!("Channel handler started for channel {}", channel_id);

    loop {
        // Use tokio::select! so cancellation is prompt rather than
        // waiting for the next iteration boundary.
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Channel {} handler cancelled", channel_id);
                let _ = queries::skip_channel_threads(&pool, channel_id).await;
                break;
            }
            _ = async {
                // Check if the channel has been closed in the DB
                if let Ok(true) = queries::is_channel_closed(&pool, channel_id).await {
                    info!("Channel {} is closed in DB, handler exiting", channel_id);
                    let _ = queries::skip_channel_threads(&pool, channel_id).await;
                    return;
                }

                // Fetch pending threads for this channel
                let threads = match queries::find_pending_threads_by_channel(&pool, channel_id).await {
                    Ok(threads) => threads,
                    Err(e) => {
                        error!("Error fetching pending threads for channel {}: {:?}", channel_id, e);
                        return;
                    }
                };

                for thread in &threads {
                    // Best-effort cancellation check before each thread
                    if cancel.is_cancelled() {
                        let _ = queries::skip_channel_threads(&pool, channel_id).await;
                        return;
                    }

                    // Check if the channel was closed between batches
                    if let Ok(true) = queries::is_channel_closed(&pool, channel_id).await {
                        info!("Channel {} closed during batch processing", channel_id);
                        let _ = queries::skip_channel_threads(&pool, channel_id).await;
                        return;
                    }

                    info!("Processing thread {} in channel {}", thread.id, channel_id);

                    // Get the cause message for this thread
                    let cause_msg = match queries::get_cause_message(&pool, thread.id).await {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            error!("Thread {} has no cause message, skipping", thread.id);
                            // Mark thread as interrupted
                            let _ = queries::complete_thread(&pool, thread.id, "interrupted", 0, 0, 0, 0).await;
                            continue;
                        }
                        Err(e) => {
                            error!("Failed to get cause message for thread {}: {:?}", thread.id, e);
                            continue;
                        }
                    };

                    // Check message count limit before claiming the thread
                    match queries::count_thread_messages(&pool, thread.id).await {
                        Ok(count) if count >= config.max_iterations as i32 => {
                            info!(
                                "Thread {} has reached message limit ({}/{}), skipping",
                                thread.id, count, config.max_iterations
                            );
                            let _ = queries::complete_thread(&pool, thread.id, "skipped", 0, 0, 0, 0).await;
                            continue;
                        }
                        Ok(_) => {} // under limit, proceed
                        Err(e) => {
                            error!("Failed to count thread messages: {:?}", e);
                        }
                    }

                    // Anti-double-execute guard: atomically claim this thread by
                    // updating its status to 'processing' only if it's still 'pending'.
                    // If another agent instance claimed it first, skip.
                    if !queries::claim_thread(&pool, thread.id).await {
                        debug!(
                            "Thread {} was already claimed by another worker, skipping",
                            thread.id
                        );
                        continue;
                    }

                    // If this thread is linked to a kanban task, mark it as running
                    if let Some(ref task_id) = thread.task_id {
                        let _ = queries::update_kanban_status(&pool, task_id, "running").await;
                    }

                    if let Err(e) = process_thread(&pool, &llm, &config, &mcp, &ctx, thread, &cause_msg).await {
                        error!("Failed to process thread {}: {:?}", thread.id, e);
                        // Mark thread as failed
                        let _ = queries::complete_thread(&pool, thread.id, "failed", 0, 0, 0, 0).await;
                        // If this thread is linked to a kanban task, mark it as blocked
                        if let Some(ref task_id) = thread.task_id {
                            let _ = queries::update_kanban_status(&pool, task_id, "blocked").await;
                        }
                    }
                }

                // Brief pause between polling iterations
                tokio::time::sleep(Duration::from_secs(1)).await;
            } => {}
        }
    }

    info!("Channel handler finished for channel {}", channel_id);
}

/// Merge cumulative usage with a new usage value.
fn merge_usage(cumulative: &mut Option<Usage>, new_usage: Option<Usage>) {
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
enum CreateMessageResult {
    Success(Message),
    FkViolation,
    OtherError(anyhow::Error),
}

async fn persist_or_abort(
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
            let _ = queries::complete_thread(pool, thread_id, "failed", 0, 0, 0, 0).await;
            CreateMessageResult::FkViolation
        }
        Err(e) => CreateMessageResult::OtherError(e),
    }
}

/// Prune old tool results from the conversation history when the total
///
/// Keeps the most recent turn's results intact and strips old tool result
/// bodies, replacing them with a short summary, while preserving all
/// user, assistant, and system messages unchanged.
fn prune_old_tool_results(messages: &mut [ChatMessage]) {
    let total_tool_chars: usize = messages
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| m.content.len())
        .sum();

    if total_tool_chars <= TOOL_RESULT_HISTORY_BUDGET {
        return;
    }

    // Find the index of the last assistant message with tool_calls — this
    // marks the most recent turn boundary. Tool results after it are kept.
    let last_tool_turn_idx = messages
        .iter()
        .rposition(|m| m.role == "assistant" && m.tool_calls.is_some());

    let keep_from = last_tool_turn_idx.unwrap_or(0);

    for msg in messages.iter_mut().take(keep_from) {
        if msg.role == "tool" && msg.content.len() > 500 {
            let preview = if msg.content.len() > 200 {
                let truncate_to = msg
                    .content
                    .char_indices()
                    .nth(200)
                    .map(|(i, _)| i)
                    .unwrap_or(msg.content.len());
                format!("{}...", &msg.content[..truncate_to])
            } else {
                msg.content.clone()
            };
            msg.content = format!(
                "[Pruned tool result — was {} chars] {preview}",
                msg.content.len(),
            );
        }
    }
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
async fn check_and_generate_summary(
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

/// Process a single pending thread through the state machine:
///
/// 1. Claim the thread (status → 'processing')
/// 2. Get current message count for the thread
/// 3. Resolve profile, provider, model from thread
/// 4. Call the LLM with system + user messages (and tools if enabled)
/// 5. If tool calls are returned, execute them and loop back to LLM
/// 6. If reasoning exists, save as a separate `reasoning` record
/// 7. Save the main agent response (msg_type: `message`)
/// 8. Generate a per-turn summary (outside iteration limit)
/// 9. Update thread status → completed/failed, record token_usage + duration
/// 10. Trigger cross-thread summary if enough threads have accumulated
async fn process_thread(
    pool: &PgPool,
    llm: &LLMClient,
    config: &AgentConfig,
    mcp: &McpRegistry,
    ctx: &AppContext,
    thread: &Thread,
    cause_msg: &Message,
) -> Result<Message> {
    let start_time = std::time::Instant::now();

    // 1. Mark the thread as 'processing' (already done by claim_thread, but verify)
    // The claim_thread function already set status='processing' and started_at=NOW()

    // 2. Get current message count for this thread
    let current_msg_count = queries::count_thread_messages(pool, thread.id)
        .await
        .unwrap_or(0);

    // 3. Read profile, provider, model from the thread (not from messages)
    let profile_name = thread.profile.clone();
    let provider_name = thread.provider.clone();
    let model_name = thread.model.clone();

    let profile_registry = crate::profile::ProfileRegistry::new(&ctx.data_dir);

    // 3a. Check profile name is present
    if profile_name.is_empty() {
        let err_msg = MessageNew {
            thread_id: thread.id,
            role: "system".to_string(),
            content: format!(
                "Invalid configuration: profile='{}', provider={:?}, model={:?} — profile name is empty. Set a profile on the channel or thread.",
                profile_name, provider_name, model_name
            ),
            thread_sequence: cause_msg.thread_sequence + 1,
            external_id: Some(format!("validation-error:{}:{}", thread.id, chrono::Utc::now().timestamp())),
            metadata: serde_json::json!({
                "error_type": "configuration",
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "error".to_string(),
            msg_subtype: Some("no-profile".to_string()),
            processing_time_ms: None,
            token_usage: None,
        };
        let saved = queries::create_message(pool, &err_msg).await?;
        let _ = queries::complete_thread(pool, thread.id, "failed", 0, 0, 0, 0).await;
        // Deliver the error message back to the user's platform
        if let Ok(Some(channel)) = queries::get_channel_by_id(pool, thread.channel_id).await {
            enqueue_delivery(ctx, &saved, &channel, thread, cause_msg.external_id.clone()).await;
        }
        return Ok(saved);
    }

    // 3b. Check profile exists
    if profile_registry.get(&profile_name).is_none() {
        let err_msg = MessageNew {
            thread_id: thread.id,
            role: "system".to_string(),
            content: format!(
                "Invalid configuration: profile='{}' does not exist.",
                profile_name
            ),
            thread_sequence: cause_msg.thread_sequence + 1,
            external_id: Some(format!("validation-error:{}:{}", thread.id, chrono::Utc::now().timestamp())),
            metadata: serde_json::json!({
                "error_type": "configuration",
                "original_thread_id": thread.id,
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "error".to_string(),
            msg_subtype: Some("invalid-profile".to_string()),
            processing_time_ms: None,
            token_usage: None,
        };
        let saved = queries::create_message(pool, &err_msg).await?;
        let _ = queries::complete_thread(pool, thread.id, "failed", 0, 0, 0, 0).await;
        return Ok(saved);
    }

    // 3c. Check provider is set on the thread
    if provider_name.as_ref().is_none_or(|s| s.is_empty()) {
        let err_msg = MessageNew {
            thread_id: thread.id,
            role: "system".to_string(),
            content: format!(
                "Invalid configuration: provider is not set on thread {}. Ensure the thread has a provider stamped at creation time. Check channel.current_provider, profile provider, or LLM_PROVIDER env var.",
                thread.id
            ),
            thread_sequence: cause_msg.thread_sequence + 1,
            external_id: Some(format!("validation-error:{}:{}", thread.id, chrono::Utc::now().timestamp())),
            metadata: serde_json::json!({
                "error_type": "configuration",
                "original_thread_id": thread.id,
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "error".to_string(),
            msg_subtype: Some("no-provider".to_string()),
            processing_time_ms: None,
            token_usage: None,
        };
        let saved = queries::create_message(pool, &err_msg).await?;
        let _ = queries::complete_thread(pool, thread.id, "failed", 0, 0, 0, 0).await;
        // Deliver the error message back to the user's platform
        if let Ok(Some(channel)) = queries::get_channel_by_id(pool, thread.channel_id).await {
            enqueue_delivery(ctx, &saved, &channel, thread, cause_msg.external_id.clone()).await;
        }
        return Ok(saved);
    }

    // 3d. Check model is set on the thread
    if model_name.as_ref().is_none_or(|s| s.is_empty()) {
        let err_msg = MessageNew {
            thread_id: thread.id,
            role: "system".to_string(),
            content: format!(
                "Invalid configuration: model is not set on thread {}. Ensure the thread has a model stamped at creation time. Check channel.current_model, profile model, or LLM_MODEL env var.",
                thread.id
            ),
            thread_sequence: cause_msg.thread_sequence + 1,
            external_id: Some(format!("validation-error:{}:{}", thread.id, chrono::Utc::now().timestamp())),
            metadata: serde_json::json!({
                "error_type": "configuration",
                "original_thread_id": thread.id,
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "error".to_string(),
            msg_subtype: Some("no-model".to_string()),
            processing_time_ms: None,
            token_usage: None,
        };
        let saved = queries::create_message(pool, &err_msg).await?;
        let _ = queries::complete_thread(pool, thread.id, "failed", 0, 0, 0, 0).await;
        // Deliver the error message back to the user's platform
        if let Ok(Some(channel)) = queries::get_channel_by_id(pool, thread.channel_id).await {
            enqueue_delivery(ctx, &saved, &channel, thread, cause_msg.external_id.clone()).await;
        }
        return Ok(saved);
    }

    // Validation passed — load the profile for its settings (auto_retrieval_enabled, etc.)
    let prof = profile_registry.get(&profile_name).cloned().unwrap_or_else(|| {
        crate::profile::Profile::default(&profile_name)
    });

    // Use provider/model directly from the thread stamp (no fallback chain)
    let _provider_name = provider_name;
    let _model_name = model_name;

    // 4. Build the initial message history with the structured system prompt
    let system_prompt = crate::prompt_builder::build_system_prompt(
        &ctx.memory_store,
        "",   // platform — will be enriched from channel metadata in the future
        None, // system_message
        &profile_name,
    );

    // 4a. Inject subtask context if the thread has subtasks
    let subtask_section: Option<String> = match crate::subtask::list_subtasks(pool, thread.id).await
    {
        Ok(subtask_rows) => {
            if subtask_rows.is_empty() {
                None
            } else {
                let thread_subtasks: Vec<crate::prompt_builder::ThreadSubtask> = subtask_rows
                    .iter()
                    .enumerate()
                    .map(|(i, row)| {
                        let status = match row.status.as_str() {
                            "completed" => crate::prompt_builder::SubtaskStatus::Completed,
                            "cancelled" => crate::prompt_builder::SubtaskStatus::Cancelled,
                            _ => crate::prompt_builder::SubtaskStatus::Pending,
                        };
                        crate::prompt_builder::ThreadSubtask {
                            name: row.description.clone(),
                            status,
                            step_index: i,
                            total_steps: subtask_rows.len(),
                        }
                    })
                    .collect();
                let section = format_subtask_section(&thread_subtasks, thread.id);
                if section.is_some() {
                    info!(
                        "Injected subtask section into system prompt for thread {}",
                        thread.id
                    );
                }
                section
            }
        }
        Err(e) => {
            warn!("Failed to query subtasks for thread {}: {:?}", thread.id, e);
            None
        }
    };

    // 4b. Load template from cause message metadata (for kanban/cron tasks)
    let template_section: Option<String> = {
        let msg_type = cause_msg.msg_type.as_str();
        if msg_type == "kanban" || msg_type == "cron" {
            let template_name = cause_msg.metadata
                .get("template")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    cause_msg.metadata
                        .get("instruction_file")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                });
            if let Some(template) = template_name {
                let content = crate::prompt_builder::load_template(&ctx.data_dir, &profile_name, template);
                if let Some(ref tmpl) = content {
                    info!(
                        "Loaded template '{}' for thread {} ({} chars)",
                        template, thread.id, tmpl.len()
                    );
                    Some(format!(
                        "=== Task Template ===\nThe following template provides structured guidance for this task type:\n\n{}",
                        tmpl
                    ))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    };

    // 4c. Assemble additional context blocks via ContextBuilder
    let ctx_assembly_meta: Option<ContextAssemblyMeta>;
    let context_messages = {
        let (context_text, meta) = crate::context_builder::build_thread_context(
            pool,
            thread.id,
            thread.channel_id,
            cause_msg.id,
            &cause_msg.content,
            &profile_name,
            &ctx.data_dir,
            ctx.qdrant_url.as_deref(),
            prof.prompt_budget.unwrap_or(crate::profile::PROMPT_BUDGET_DEFAULT),
            prof.auto_retrieval_enabled,
            prof.retrieval_aggressiveness,
        ).await;
        ctx_assembly_meta = Some(meta);
        context_text
    };

    // Track cumulative token usage across all LLM calls
    let mut cumulative_usage: Option<Usage> = None;
    let mut force_failed: bool = false;

    // ── Planning Phase ──
    // Determine planning mode: channel metadata > global PLANNING_MODE env var
    let channel = queries::get_channel_by_id(pool, thread.channel_id).await?.unwrap_or_default();
    let mut planning_mode = channel.metadata.get("planning_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if planning_mode.is_empty() {
        planning_mode = std::env::var("PLANNING_MODE").unwrap_or_else(|_| "auto_subtasks".to_string());
    }

    // Whether subtask creation and enforcement are enabled
    let enable_subtasks = planning_mode == "auto_subtasks";

    // Classify message complexity for adaptive behavior
    let complexity = crate::context_builder::classify_complexity(
        &cause_msg.content,
        &cause_msg.msg_type,
        cause_msg.metadata.get("kanban_task_id").or_else(|| cause_msg.metadata.get("cron_job_id")).map(|_| cause_msg.content.len()),
    );

    // Determine if we should run the planning phase
    let should_plan = match planning_mode.as_str() {
        "never" | "prompt_only" => false,
        "always" => true,
        "auto_plan" | "auto_subtasks" => {
            if complexity == crate::context_builder::Complexity::Simple {
                false
            } else {
                // Auto: plan if message > 100 chars AND is first in thread
                config.prompt_plan_enabled 
                    && cause_msg.content.len() > 100
                    && cause_msg.thread_sequence == 0
            }
        }
        _ => {
            // Unknown mode — fall back to auto behavior
            if complexity == crate::context_builder::Complexity::Simple {
                false
            } else {
                config.prompt_plan_enabled 
                    && cause_msg.content.len() > 100
                    && cause_msg.thread_sequence == 0
            }
        }
    };

    let plan_content: Option<String> = if should_plan {
        let max_iter = config.prompt_graph_iterations.max(1); // at least 1
        let max_tokens = config.prompt_plan_max_tokens;
        let mut last_plan: Option<String> = None;
        let mut accepted = false;
        let mut json_failure_count: u32 = 0;
        let mut json_error_msg: Option<String> = None;

        for iter in 0..(max_iter + 1) {
            // Build the planning prompt (lightweight — no tools, no heavy context)
            let planning_prompt = crate::prompt_builder::build_planning_prompt(
                &ctx.memory_store,
                "",   // platform
                &profile_name,
                &cause_msg.content,
                iter,
                max_iter,
                last_plan.as_deref(),
                enable_subtasks,
            );

            let planning_messages = if let Some(ref err) = json_error_msg {
                vec![
                    ChatMessage::system(&planning_prompt),
                    ChatMessage::system(err),
                ]
            } else {
                vec![
                    ChatMessage::system(&planning_prompt),
                ]
            };

            let plan_request = CompletionRequest {
                messages: planning_messages,
                max_tokens,
                temperature: 0.3,
                stream: false,
                tools: None,
            };

            match llm.completion(plan_request).await {
                Ok(resp) => {
                    merge_usage(&mut cumulative_usage, resp.usage.clone());
                    let content = resp.content;

                    // Check if the LLM accepted the plan (refinement mode)
                    if content.trim() == "PLAN_ACCEPTED" {
                        info!(
                            "[plan] Plan accepted after {} iteration(s) for thread {}",
                            iter, thread.id
                        );
                        accepted = true;
                        break;
                    }

                    info!(
                        "[plan] Generated plan for thread {} ({} chars, iteration {}/{})",
                        thread.id,
                        content.len(),
                        iter + 1,
                        max_iter + 1,
                    );

                    // Convert Usage to JSON value for token_usage field
                    let plan_token_usage = resp.usage.map(|u| {
                        serde_json::json!({
                            "prompt_tokens": u.prompt_tokens,
                            "completion_tokens": u.completion_tokens,
                            "cached_tokens": u.cached_tokens,
                            "reasoning_tokens": u.reasoning_tokens,
                        })
                    });
                    let plan_duration_ms = Some(resp.duration_ms as i32);

                    // Save the plan as a plan-type message
                    let plan_msg = MessageNew {
                        thread_id: thread.id,
                        role: "agent".to_string(),
                        content: content.clone(),
                        thread_sequence: cause_msg.thread_sequence + 1,
                        external_id: None,
                        metadata: serde_json::json!({
                            "plan_iteration": iter,
                            "plan_accepted": iter == 0 && max_iter == 0,
                        }),
                        embedding: None,
                        summary_text: None,
                        is_summary: false,
                        msg_type: "plan".to_string(),
                        msg_subtype: Some("markdown".to_string()),
                        processing_time_ms: plan_duration_ms,
                        token_usage: plan_token_usage,
                    };
                    match queries::create_message(pool, &plan_msg).await {
                        Ok(_) => {},
                        Err(e) => warn!("[plan] Failed to persist plan for thread {}: {:?}", thread.id, e),
                    }

                    // For complex tasks, auto-create subtasks from JSON plan content
                    // No fallback — invalid JSON triggers retry or thread failure
                    if enable_subtasks && complexity == crate::context_builder::Complexity::Complex && content.len() > 100 {
                        let max_json_retries: u32 = std::env::var("MAX_UNFINISHED_SUBTASK_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
                        match serde_json::from_str::<serde_json::Value>(&content) {
                            Ok(plan_json) => {
                                if let Some(steps) = plan_json.get("steps").and_then(|v| v.as_array()) {
                                    // Valid JSON with steps — create subtasks
                                    let total = steps.len().min(6);
                                    for (i, step_val) in steps.iter().enumerate().take(6) {
                                        if let Some(step) = step_val.as_str() {
                                            let clean = step.trim().trim_end_matches(|c: char| c == '*' || c == '`').trim();
                                            if !clean.is_empty() {
                                                let priority = (total - i) as i32;
                                                if let Err(e) = crate::subtask::add_subtask(pool, thread.id, clean, priority).await {
                                                    warn!("[plan] Failed to create subtask '{}': {:?}", clean, e);
                                                } else {
                                                    info!("[plan] Created subtask '{}' for complex thread {}", clean, thread.id);
                                                }
                                            }
                                        }
                                    }
                                    json_error_msg = None;
                                } else {
                                    // JSON valid but missing "steps" field
                                    json_failure_count += 1;
                                    if json_failure_count > max_json_retries {
                                        warn!("[plan] JSON validation exhausted — missing 'steps' field after {} retries for thread {}", max_json_retries, thread.id);
                                        force_failed = true;
                                        break;
                                    }
                                    json_error_msg = Some(format!(
                                        "ERROR: Your plan JSON is missing the required \"steps\" array. \
                                         You MUST return a JSON object with \"description\" (string) and \"steps\" (array of strings). \
                                         No surrounding markdown, no backticks, no extra text. \
                                         Attempt {}/{} — fix the JSON or the thread will fail.",
                                        json_failure_count, max_json_retries
                                    ));
                                  last_plan = Some(content.clone());
                                    continue;
                                }
                            }
                            Err(e) => {
                                // Invalid JSON syntax
                                json_failure_count += 1;
                                if json_failure_count > max_json_retries {
                                    warn!("[plan] JSON validation exhausted — invalid JSON after {} retries for thread {}: {}", max_json_retries, thread.id, e);
                                    force_failed = true;
                                    break;
                                }
                                json_error_msg = Some(format!(
                                    "ERROR: Your response was not valid JSON. Parsing error: {}. \
                                     You MUST return a valid JSON object with \"description\" and \"steps\" fields. \
                                     No surrounding markdown, no backticks, no extra text. \
                                     Attempt {}/{} — fix the JSON or the thread will fail.",
                                    e, json_failure_count, max_json_retries
                                ));
                              last_plan = Some(content.clone());
                                continue;
                            }
                        }
                    }

                    last_plan = Some(content);

                    // If no refinement iterations configured, one shot is enough
                    if config.prompt_graph_iterations == 0 {
                        break;
                    }
                }
                Err(e) => {
                    warn!("[plan] Failed to generate plan for thread {}: {:?}", thread.id, e);
                    break;
                }
            }
        }

        if accepted {
            last_plan
        } else {
            // If we have a last plan (even if not explicitly accepted), use it
            if last_plan.is_some() {
                last_plan
            } else {
                None
            }
        }
    } else {
        None
    };

    let mut messages = vec![
        ChatMessage::system(&system_prompt),
    ];

    // Inject subtask context section if the thread has active subtasks
    if let Some(ref subtask_section) = subtask_section {
        messages.push(ChatMessage::system(subtask_section));
    }

    // Inject task template if the thread has a template from kanban/cron metadata
    if let Some(ref template_section) = template_section {
        messages.push(ChatMessage::system(template_section));
    }

    // Add context blocks as system messages (before the user message)
    if !context_messages.is_empty() {
        messages.push(ChatMessage::system(&format!(
            "=== Additional Context ===\n{}",
            context_messages
        )));
    }

    // Inject the plan as execution context if one was generated
    if let Some(ref plan) = plan_content {
        messages.push(ChatMessage::system(&format!(
            "=== Generated Plan (use as guidance) ===\n\
             A plan was generated for the current task. Follow it unless tool results \
             contradict it. Do NOT explore alternative approaches that the plan already \
             considered — adapt only when necessary.\n\n{}",
            plan
        )));
        info!(
            "[plan] Injected plan as context for thread {} ({} chars)",
            thread.id,
            plan.len(),
        );
    }

    // Add the user message (from the cause message)
    messages.push(ChatMessage::user(&cause_msg.content));

    // 5. Build tool definitions from the profile's allowed tools
    let tools_def = mcp.to_openai_tools(&prof.allowed_tools);

    // 6. Tool-calling loop — max iterations controls total LLM calls
    let remaining = config.max_iterations as i32 - current_msg_count;
    let max_llm_calls = remaining.clamp(0, 25) as u32; // safety cap — 25 max
    let mut final_content = String::new();
    let mut final_reasoning: Option<String> = None;
    let mut final_tool_call: bool = false;
    let mut limit_reached: bool = false;
    let mut current_iter = current_msg_count;
    let mut unfinished_subtask_retries: u32 = 0;

    for _turn in 0..max_llm_calls {
        current_iter += 1;  // increment before each LLM call

        // If this LLM call will reach the iteration limit, hint to the model
        // to produce a final answer rather than more tool calls.
        if current_iter >= config.max_iterations as i32 {
            messages.push(ChatMessage::system(
                "This is your last turn. You must provide your final answer now. \
                 Do not request additional tool calls.",
            ));
        }

        // Layer 3: prune old tool results from conversation history if over budget
        prune_old_tool_results(&mut messages);

        let request = CompletionRequest {
            messages: messages.clone(),
            max_tokens: config.max_tokens,
            temperature: config.temperature,
            stream: false,
            tools: if tools_def.is_empty() {
                None
            } else {
                Some(tools_def.clone())
            },
        };

        let response = match llm.completion(request).await {
            Ok(resp) => resp,
            Err(e) => {
                error!("LLM call failed: {:?}", e);
                final_content = format!("I encountered an error: {}", e);
                break;
            }
        };

        // Track cumulative token usage
        merge_usage(&mut cumulative_usage, response.usage.clone());

        // Store reasoning if present
        if response.reasoning.is_some() {
            final_reasoning = response.reasoning.clone();
        }

        // Check for tool calls
        if response.tool_calls.is_empty() {
            // Subtask enforcement: only when subtask mode is active
            if enable_subtasks {
                // Check if all subtasks are completed/cancelled before allowing final answer
                let pending_subtasks = match crate::subtask::list_subtasks(pool, thread.id).await {
                    Ok(list) => list.into_iter().filter(|st| st.status == "pending" || st.status == "in_progress").collect::<Vec<_>>(),
                    Err(_) => Vec::new(),
                };

                if !pending_subtasks.is_empty() && unfinished_subtask_retries < std::env::var("MAX_UNFINISHED_SUBTASK_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(3u32) {
                    unfinished_subtask_retries += 1;
                    let names: Vec<String> = pending_subtasks.iter()
                        .map(|st| format!("#{}: {}", st.id, st.description))
                        .collect();
                    let feedback = format!(
                        "[System] You have {} unfinished subtask(s) that must be completed or cancelled before you can deliver your final answer:\n\n{}\n\nUse `manage_subtasks(thread_id={}, action=\"update\", subtask_id=N, status=\"completed\")` \
                         for each finished subtask, or status=\"cancelled\" if a subtask is no longer relevant. \
                         Then respond with your final summary when all subtasks are resolved.",
                        pending_subtasks.len(),
                        names.join("\n"),
                        thread.id,
                    );
                    messages.push(ChatMessage::system(&feedback));
                    info!(
                        "[subtask] Enforcement: LLM tried to end with {} unfinished subtask(s) (retry {}/{})",
                        pending_subtasks.len(),
                        unfinished_subtask_retries,
                        std::env::var("MAX_UNFINISHED_SUBTASK_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(3u32),
                    );
                    // Don't consume from the iteration budget — this is enforcement overhead
                    current_iter -= 1;
                    continue;
                }

                if !pending_subtasks.is_empty() {
                    // Exhausted retries — force the thread to fail
                    warn!(
                        "[subtask] Enforcement exhausted after {} retries — {} subtask(s) still unfinished for thread {}",
                        std::env::var("MAX_UNFINISHED_SUBTASK_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(3u32),
                        pending_subtasks.len(),
                        thread.id,
                    );
                    final_content = format!(
                        "I ran out of attempts to complete all subtasks. The following remain unfinished:\n{}",
                        pending_subtasks.iter().map(|st| format!("- #{}: {} ({})", st.id, st.description, st.status)).collect::<Vec<_>>().join("\n"),
                    );
                    final_tool_call = false;
                    force_failed = true;
                    break;
                }
            }

            // Normal text response — all subtasks done (or subtask mode off)
            final_content = if response.content.is_empty() {
                response.reasoning.clone().unwrap_or_default()
            } else {
                response.content
            };
            final_tool_call = false;
            break;
        }

        // If iterations will equal the max after this call, flag interruption
        if current_iter >= config.max_iterations as i32 {
            limit_reached = true;
            break;
        }

        // We have tool calls — add assistant message with tool_calls
        final_tool_call = true;
        let mut assistant_msg = ChatMessage::assistant("");
        assistant_msg.tool_calls = Some(response.tool_calls.clone());
        messages.push(assistant_msg);

        // Per-message token/time from the LLM response that generated these tool calls
        let tool_token_usage = response.usage.map(|u| {
            serde_json::json!({
                "prompt_tokens": u.prompt_tokens,
                "completion_tokens": u.completion_tokens,
                "cached_tokens": u.cached_tokens,
                "reasoning_tokens": u.reasoning_tokens,
            })
        });
        let tool_duration_ms = Some(response.duration_ms as i32);
        let is_multi_tool = response.tool_calls.len() > 1;

        // If multiple tool calls, persist a multi-tool message first (holds time/tokens)
        if is_multi_tool {
            let multi_content = response.tool_calls
                .iter()
                .map(|tc| format!("{}: {}", tc.function.name, tc.function.arguments))
                .collect::<Vec<_>>()
                .join("\n");
            let multi_msg = MessageNew {
                thread_id: thread.id,
                role: "agent".to_string(),
                content: multi_content,
                thread_sequence: cause_msg.thread_sequence + 1,
                external_id: None,
                metadata: serde_json::json!({}),
                embedding: None,
                summary_text: None,
                is_summary: false,
                msg_type: "multi-tool".to_string(),
                msg_subtype: None,
                processing_time_ms: tool_duration_ms,
                token_usage: tool_token_usage.clone(),
            };
            match persist_or_abort(pool, &multi_msg, thread.id).await {
                CreateMessageResult::FkViolation => {
                    anyhow::bail!("FK violation — thread {} no longer exists", thread.id)
                }
                CreateMessageResult::OtherError(e) => {
                    error!("Failed to persist multi-tool message: {:?}", e)
                }
                CreateMessageResult::Success(saved) => {
                    enqueue_delivery(ctx, &saved, &channel, thread, cause_msg.external_id.clone()).await;
                }
            }
        }

        // Execute each tool call
        for tc in &response.tool_calls {
            let tool_name = tc.function.name.clone();
            let tool_args = tc.function.arguments.clone();

            // Single tool: attach time/tokens to the tool message.
            // Multi-tool: time/tokens are on the multi-tool message, tools get null.
            let (tool_ptime, tool_tu) = if is_multi_tool {
                (None, None)
            } else {
                (tool_duration_ms, tool_token_usage.clone())
            };

            // Persist the tool call as an agent message with msg_type="tool"
            let tool_call_msg = MessageNew {
                thread_id: thread.id,
                role: "agent".to_string(),
                content: tool_args,
                thread_sequence: cause_msg.thread_sequence + 1,
                external_id: None,
                metadata: serde_json::json!({}),
                embedding: None,
                summary_text: None,
                is_summary: false,
                msg_type: "tool".to_string(),
                msg_subtype: Some(tool_name.clone()),
                processing_time_ms: tool_ptime,
                token_usage: tool_tu,
            };
            match persist_or_abort(pool, &tool_call_msg, thread.id).await {
                CreateMessageResult::FkViolation => {
                    anyhow::bail!("FK violation — thread {} no longer exists", thread.id)
                }
                CreateMessageResult::OtherError(e) => {
                    error!("Failed to persist tool call '{}': {:?}", tool_name, e)
                }
                CreateMessageResult::Success(saved) => {
                    enqueue_delivery(ctx, &saved, &channel, thread, cause_msg.external_id.clone()).await;
                }
            }

            let mcp_call = McpToolCall {
                id: tc.id.clone(),
                name: tool_name.clone(),
                arguments: serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(serde_json::json!({})),
            };

            let tool_start = std::time::Instant::now();
            let result = mcp.execute(&mcp_call, ctx.clone()).await;
            let tool_elapsed_ms = tool_start.elapsed().as_millis() as i32;

            match result {
                Ok(res) => {
                    // Layer 2: truncate first — DB stores what the LLM will see
                    let content = truncate_content(&res.content, DEFAULT_MAX_TOOL_OUTPUT_CHARS);

                    // Persist the tool result as an agent message with msg_type="tool_result"
                    let tool_result_msg = MessageNew {
                        thread_id: thread.id,
                        role: "agent".to_string(),
                        content: content.clone(),
                        thread_sequence: cause_msg.thread_sequence + 1,
                        external_id: None,
                        metadata: serde_json::json!({}),
                        embedding: None,
                        summary_text: None,
                        is_summary: false,
                        msg_type: "tool_result".to_string(),
                        msg_subtype: Some(tool_name.clone()),
                        processing_time_ms: Some(tool_elapsed_ms),
                        token_usage: None,
                    };
                    match persist_or_abort(pool, &tool_result_msg, thread.id).await {
                        CreateMessageResult::FkViolation => anyhow::bail!("FK violation — thread {} no longer exists", thread.id),
                        CreateMessageResult::OtherError(e) => error!("Failed to persist tool result '{}': {:?}", tool_name, e),
                        CreateMessageResult::Success(_) => {}
                    }

                    messages.push(ChatMessage::tool_result(
                        &tc.id,
                        &tc.function.name,
                        &content,
                    ));
                }
                Err(e) => {
                    let err_msg = format!("Error executing tool '{}': {}", tool_name, e);

                    // Persist error as tool result
                    let tool_result_msg = MessageNew {
                        thread_id: thread.id,
                        role: "agent".to_string(),
                        content: err_msg.clone(),
                        thread_sequence: cause_msg.thread_sequence + 1,
                        external_id: None,
                        metadata: serde_json::json!({}),
                        embedding: None,
                        summary_text: None,
                        is_summary: false,
                        msg_type: "tool_result".to_string(),
                        msg_subtype: Some(tool_name.clone()),
                        processing_time_ms: Some(tool_elapsed_ms),
                        token_usage: None,
                    };
                    match persist_or_abort(pool, &tool_result_msg, thread.id).await {
                        CreateMessageResult::FkViolation => anyhow::bail!("FK violation — thread {} no longer exists", thread.id),
                        CreateMessageResult::OtherError(e2) => error!("Failed to persist tool error '{}': {:?}", tool_name, e2),
                        CreateMessageResult::Success(_) => {}
                    }

                    messages.push(ChatMessage::tool_result(
                        &tc.id,
                        &tc.function.name,
                        &err_msg,
                    ));
                }
            }
        }
    }

    // If we exited the loop without a final text response, provide a fallback
    if final_content.is_empty() && !final_tool_call {
        final_content =
            "I've completed the requested operations using my available tools.".to_string();
    }

    // 7. Serialize cumulative token usage
    let token_usage_json = cumulative_usage.as_ref().map(|u| {
        serde_json::json!({
            "prompt_tokens": u.prompt_tokens,
            "completion_tokens": u.completion_tokens,
            "cached_tokens": u.cached_tokens,
            "reasoning_tokens": u.reasoning_tokens,
        })
    });

    // Build evidence metadata from context assembly
    let evidence_metadata = {
        let mut meta = serde_json::json!({
            "context": {
                "selected_message_ids": [],
                "wiki_files": [],
                "block_counts": {},
                "dropped_blocks": [],
                "total_chars": 0,
            },
            "grounding": {
                "policy_applied": true,
            }
        });
        if let Some(ref assembly) = ctx_assembly_meta {
            meta["context"]["selected_message_ids"] = serde_json::json!(assembly.selected_message_ids);
            meta["context"]["wiki_files"] = serde_json::json!(assembly.wiki_files);
            meta["context"]["block_counts"] = serde_json::json!(assembly.block_counts);
            meta["context"]["dropped_blocks"] = serde_json::json!(assembly.dropped_blocks);
            meta["context"]["total_chars"] = serde_json::json!(assembly.total_chars);
        }
        meta
    };

    // 8. If reasoning/thinking exists, save as its own record
    if let Some(ref reasoning_text) = final_reasoning {
        if !reasoning_text.is_empty() {
            let reasoning_msg = MessageNew {
                thread_id: thread.id,
                role: "agent".to_string(),
                content: reasoning_text.clone(),
                thread_sequence: cause_msg.thread_sequence + 1,
                external_id: None,
                metadata: serde_json::json!({
                    "context": evidence_metadata["context"],
                    "grounding": evidence_metadata["grounding"],
                }),
                embedding: None,
                summary_text: None,
                is_summary: false,
                msg_type: "reasoning".to_string(),
                msg_subtype: None,
                processing_time_ms: None,
                token_usage: None,
            };
            let reasoning_saved = queries::create_message(pool, &reasoning_msg).await?;
            enqueue_delivery(
                        ctx,
                &reasoning_saved,
                &channel,
                thread,
                cause_msg.external_id.clone(),
            ).await;
        }
    }

    // 9. Save the main agent response
    let agent_elapsed_ms = start_time.elapsed().as_millis() as i32;
    let agent_msg = MessageNew {
        thread_id: thread.id,
        role: "agent".to_string(),
        content: final_content,
        thread_sequence: cause_msg.thread_sequence + 1,
        external_id: None,
        metadata: serde_json::json!({
            "context": evidence_metadata["context"],
            "grounding": evidence_metadata["grounding"],
        }),
        embedding: None,
        summary_text: None,
        is_summary: !limit_reached,
        msg_type: if limit_reached { "message".to_string() } else { "summary".to_string() },
        msg_subtype: None,
        processing_time_ms: Some(agent_elapsed_ms),
        token_usage: token_usage_json.clone(),
    };

    let saved = queries::create_message(pool, &agent_msg).await?;

    enqueue_delivery(
        ctx,
        &saved,
        &channel,
        thread,
        cause_msg.external_id.clone(),
    ).await;

    // ── Summary generation (only when interrupted / iteration limit reached) ──
    // When the thread completed normally, the agent's final message IS the summary
    // (the prompt instructs it to end with a summary). No separate LLM call needed.
    if limit_reached {
        // Strip tool results from summary context — the summary only needs
        // the conversation flow (user requests + agent responses), not raw tool
        // outputs. This keeps summary tokens low and avoids silent failures
        // from oversized context windows.
        let mut summary_msgs: Vec<ChatMessage> = messages
            .iter()
            .filter(|m| m.role != "tool")
            .map(|m| {
                let mut cloned = m.clone();
                // Remove tool_calls from assistant messages since we removed
                // the corresponding tool results — DeepSeek requires tool_call
                // chains to be complete.
                if cloned.role == "assistant" && cloned.tool_calls.is_some() {
                    cloned.tool_calls = None;
                }
                cloned
            })
            .collect();
        summary_msgs.push(ChatMessage::system(
            "The iteration limit was reached so the task may be incomplete. \
             Summarize what was accomplished and inform the user they can request to continue.",
        ));

        let summary_request = CompletionRequest {
            messages: summary_msgs,
            max_tokens: 512,
            temperature: 0.3,
            stream: false,
            tools: None,
        };

        let summary_start = std::time::Instant::now();
        let (summary_text, summary_token_usage) = match llm.completion(summary_request).await {
            Ok(resp) => {
                let usage = resp.usage.clone();
                merge_usage(&mut cumulative_usage, resp.usage);
                let tokens = usage.as_ref().map(|u| {
                    serde_json::json!({
                        "prompt_tokens": u.prompt_tokens,
                        "completion_tokens": u.completion_tokens,
                        "cached_tokens": u.cached_tokens,
                        "reasoning_tokens": u.reasoning_tokens,
                    })
                });
                info!(
                    "[summary] Generated summary for thread {} ({} chars, limit_reached={})",
                    thread.id,
                    resp.content.len(),
                    limit_reached,
                );
                (resp.content, tokens)
            }
            Err(e) => {
                warn!("[summary] Failed to generate summary for thread {}: {:?}", thread.id, e);
                (format!("Summary generation failed: {}", e), None)
            }
        };
        let summary_elapsed_ms = summary_start.elapsed().as_millis() as i32;

        let summary_msg = MessageNew {
            thread_id: thread.id,
            role: "agent".to_string(),
            content: summary_text,
            thread_sequence: cause_msg.thread_sequence + 2,
            external_id: None,
            metadata: serde_json::json!({}),
            embedding: None,
            summary_text: None,
            is_summary: true,
            msg_type: "summary".to_string(),
            msg_subtype: None,
            processing_time_ms: Some(summary_elapsed_ms),
            token_usage: summary_token_usage,
        };

        match queries::create_message(pool, &summary_msg).await {
            Ok(summary_saved) => {
                info!("[summary] Saved summary message for thread {}", thread.id,);
                enqueue_delivery(
                    ctx,
                    &summary_saved,
                    &channel,
                    thread,
                    cause_msg.external_id.clone(),
                ).await;
            }
            Err(e) => warn!(
                "[summary] Failed to save summary for thread {}: {:?}",
                thread.id, e
            ),
        }
    }
    // Define final status before potential early return
    let final_status = if force_failed {
        "failed"
    } else if limit_reached {
        "interrupted"
    } else {
        "completed"
    };

    // Post-loop subtask enforcement: if any subtasks remain pending/in_progress
    // after the tool-calling loop ends (regardless of why it ended), fail the thread.
    // Subtasks must only be marked completed/cancelled by the LLM via manage_subtasks tool.
    if enable_subtasks && !force_failed && final_status == "completed" {
        if let Ok(post_subtasks) = crate::subtask::list_subtasks(pool, thread.id).await {
            let unfinished: Vec<_> = post_subtasks.iter()
                .filter(|st| st.status == "pending" || st.status == "in_progress")
                .collect();
            if !unfinished.is_empty() {
                warn!(
                    "[subtask] Post-loop enforcement: {} subtask(s) still unfinished for thread {} — forcing failure",
                    unfinished.len(),
                    thread.id,
                );
                force_failed = true;
                let names: Vec<String> = unfinished.iter()
                    .map(|st| format!("- #{}: {} ({})", st.id, st.description, st.status))
                    .collect();
                final_content = format!(
                    "The thread was ended with {} unfinished subtask(s) that were never completed or cancelled by the LLM:\n\n{}\n\nAll subtasks must be explicitly completed or cancelled via the manage_subtasks tool.",
                    unfinished.len(),
                    names.join("\n"),
                );
            }
        }
    }

    // Recompute final status after post-loop enforcement
    let final_status = if force_failed {
        "failed"
    } else if limit_reached {
        "interrupted"
    } else {
        "completed"
    };

    queries::complete_thread(pool, thread.id, final_status, 0, 0, 0, 0).await?;

    // If this thread is linked to a kanban task, update its status
    if let Some(ref task_id) = thread.task_id {
        let kanban_status = if final_status == "completed" {
            "review"
        } else {
            "blocked"
        };
        let _ = queries::update_kanban_status(pool, task_id, kanban_status).await;
    }

    // 11. Trigger cross-thread summary check
    check_and_generate_summary(pool, llm, config, thread.channel_id).await;

    Ok(saved)
}

/// On startup, find any threads that are still `processing` and mark them as `failed`.
/// Also skip all pending/processing threads.
/// Returns the number of recovered threads.
pub async fn skip_on_startup(pool: &PgPool) -> Result<u64> {
    // Debug: check specific message 122 still works (for backward compat)
    #[derive(Debug, FromRow)]
    struct MsgRow {
        id: i64,
        msg_type: String,
    }

    let specific: Result<MsgRow, _> = sql_forge!(
        MsgRow,
        "SELECT id, msg_type FROM messages WHERE id = :msg_id",
        ( :msg_id = 122i64 )
    )
    .fetch_one(pool)
    .await;

    match &specific {
        Ok(row) => {
            info!(
                "[startup] DEBUG message {}: type={}",
                row.id, row.msg_type
            );
        }
        Err(e) => {
            info!("[startup] DEBUG message 122 not found: {}", e);
        }
    }

    // Debug: list ALL pending/processing threads before skipping
    #[derive(Debug, FromRow)]
    struct PendingThreadRow {
        id: i64,
        status: String,
    }

    let affected: Vec<PendingThreadRow> = sql_forge!(
        PendingThreadRow,
        r#"
        SELECT id, status
        FROM threads
        WHERE 1 = :_one
          AND status IN ('pending', 'processing')
        ORDER BY id
        "#,
        ( :_one = 1i32 )
    )
    .fetch_all(pool)
    .await?;

    let count = if affected.is_empty() {
        info!("[startup] No pending/processing threads to skip");
        0
    } else {
        for row in &affected {
            info!(
                "[startup] Will skip thread {} (status={})",
                row.id, row.status
            );
        }

        let c = queries::skip_all_pending_threads(pool).await?;
        if c > 0 {
            info!(
                "[startup] Skipped {} pending/processing threads on startup",
                c
            );
        }
        c
    };

    // ── Reset kanban tasks on startup ──
    // Move "ready" tasks back to "todo" so they get re-processed
    let ready_result = sql_forge!(
        r#"UPDATE kanban_tasks SET status = 'todo', updated_at = NOW() WHERE status = 'ready'"#,
    )
    .execute(pool)
    .await?;
    let ready_count = ready_result.rows_affected();
    if ready_count > 0 {
        info!(
            "[startup] Reset {} kanban tasks from ready → todo",
            ready_count
        );
    }

    // Move "running" tasks to "blocked" since the agent restarted mid-execution
    let running_result = sql_forge!(
        r#"UPDATE kanban_tasks SET status = 'blocked', updated_at = NOW() WHERE status = 'running'"#,
    )
    .execute(pool)
    .await?;
    let running_count = running_result.rows_affected();
    if running_count > 0 {
        info!(
            "[startup] Reset {} kanban tasks from running → blocked",
            running_count
        );
    }

    if ready_count + running_count == 0 {
        info!("[startup] No kanban tasks to reset");
    }

    Ok(count)
}

/// Enqueue a message for delivery to its platform.
async fn enqueue_delivery(
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
