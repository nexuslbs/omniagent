//! Agent module: parallel channel processing supervisor.
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

pub mod config;
pub mod executor;
pub mod helpers;
pub mod kanban_updater;
pub mod summary_trigger;
pub mod task_registry;
pub(crate) mod context_builder;
pub(crate) mod fail_thread;
pub(crate) mod main_loop;
pub(crate) mod response_handler;

use sql_forge::sql_forge;
use sqlx::FromRow;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::agent::executor::process_thread;
use crate::db::types as queries;
use crate::db::types::CompleteThreadStats;
use crate::llm::LLMClient;
use crate::mcp::{AppContext, McpRegistry};

// Re-export commonly used types (from config submodule).
pub use config::AgentConfig;
pub use config::AgentContext;

/// The core agent that supervises per-channel message processing.
pub struct Agent {
    pub pool: PgPool,
    pub config: Arc<RwLock<AgentConfig>>,
    pub llm: Arc<LLMClient>,
    pub mcp: Arc<tokio::sync::RwLock<McpRegistry>>,
    pub ctx: AppContext,
}

impl Agent {
    /// Create a new agent from a database pool and shared mutable configuration.
    ///
    /// An LLM client is built from the agent config, falling back to
    /// environment-level defaults for any unset values.
    pub fn new(
        pool: PgPool,
        config: Arc<RwLock<AgentConfig>>,
        mcp: Arc<tokio::sync::RwLock<McpRegistry>>,
        ctx: AppContext,
    ) -> Self {
        let env_cfg = crate::llm::LLMConfig::from_env();
        // Read config fields inside a scope so the borrow is dropped before
        // moving `config` into the struct.
        let (default_provider, llm_api_key, max_tokens, temperature) = {
            let cfg_read = config.read().expect("GlobalConfig lock poisoned");
            (
                if cfg_read.default_provider.is_empty() {
                    env_cfg.provider.clone()
                } else {
                    crate::llm::ProviderId::new(&cfg_read.default_provider)
                },
                if cfg_read.llm_api_key.is_empty() {
                    env_cfg.api_key.clone()
                } else {
                    cfg_read.llm_api_key.clone()
                },
                cfg_read.max_tokens,
                cfg_read.temperature,
            )
        };
        let provider_name = default_provider.0.clone();
        let llm_config = crate::llm::LLMConfig {
            provider: default_provider,
            api_key: llm_api_key,
            base_url: env_cfg.base_url,
            model: env_cfg.model,
            api_mode: env_cfg.api_mode,
            max_tokens,
            temperature,
            supports_reasoning: crate::llm::PROVIDER_METADATA
                .get(&provider_name)
                .map(|m| m.supports_reasoning)
                .unwrap_or(false),
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
        let agent_ctx = AgentContext {
            pool: self.pool,
            llm: self.llm,
            config: self.config,
            mcp: self.mcp,
            ctx: self.ctx,
        };

        loop {
            let channels = match queries::find_all_channels(&agent_ctx.pool).await {
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
                    // Skip spawning if the channel is closed: it will be spawned
                    // when the channel is opened via the /open endpoint
                    if let Ok(true) = queries::is_channel_closed(&agent_ctx.pool, channel_id).await
                    {
                        continue;
                    }

                    let token = CancellationToken::new();
                    let handler_token = token.clone();
                    e.insert(token);

                    let cfg = agent_ctx.clone();

                    tokio::spawn(async move {
                        channel_handler(cfg, channel_id, handler_token).await;
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
                        if let Ok(true) =
                            queries::is_channel_closed(&agent_ctx.pool, channel_id).await
                        {
                            info!("Channel {} has been closed, cancelling handler", channel_id);
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
async fn channel_handler(cfg: AgentContext, channel_id: i64, cancel: CancellationToken) {
    info!("Channel handler started for channel {}", channel_id);

    loop {
        // Use tokio::select! so cancellation is prompt rather than
        // waiting for the next iteration boundary.
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Channel {} handler cancelled", channel_id);
                // Don't skip pending threads here: stop_thread_handler already marked the
                // specific thread as skipped before cancelling. Remaining pending threads
                // should survive and be picked up when the supervisor respawns this handler.
                break;
            }
            _ = async {
                // Check if the channel has been closed in the DB
                if let Ok(true) = queries::is_channel_closed(&cfg.pool, channel_id).await {
                    info!("Channel {} is closed in DB, handler exiting", channel_id);
                    if let Err(e) = queries::skip_channel_threads(&cfg.pool, channel_id).await {
                        tracing::warn!("[supervisor] Failed to skip threads for channel {}: {:?}", channel_id, e);
                    }
                    return;
                }

                // Fetch pending threads for this channel
                let threads = match queries::find_pending_threads_by_channel(&cfg.pool, channel_id).await {
                    Ok(threads) => threads,
                    Err(e) => {
                        error!("Error fetching pending threads for channel {}: {:?}", channel_id, e);
                        return;
                    }
                };

                for thread in &threads {
                    // Best-effort cancellation check before each thread
                    if cancel.is_cancelled() {
                        // Don't skip pending threads: stop_thread_handler already handled
                        // the target thread. The supervisor will respawn the handler.
                        return;
                    }

                    // Check if the channel was closed between batches
                    if let Ok(true) = queries::is_channel_closed(&cfg.pool, channel_id).await {
                        info!("Channel {} closed during batch processing", channel_id);
                        if let Err(e) = queries::skip_channel_threads(&cfg.pool, channel_id).await {
                            tracing::warn!("[supervisor] Failed to skip threads for channel {}: {:?}", channel_id, e);
                        }
                        return;
                    }

                    info!("Processing thread {} in channel {}", thread.id, channel_id);

                    // Get the cause message for this thread
                    let cause_msg = match queries::get_cause_message(&cfg.pool, thread.id).await {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            error!("Thread {} has no cause message, skipping", thread.id);
                            // Insert an error message so the user sees what happened
                            let next_seq = queries::get_max_thread_sequence(&cfg.pool, thread.id).await.unwrap_or(0) + 1;
                            let err_msg = queries::MessageNew {
                                thread_id: thread.id,
                                role: "agent".to_string(),
                                content: "The thread has no cause message and was marked as failed.".to_string(),
                                thread_sequence: next_seq,
                                external_id: None,
                                metadata: serde_json::json!({}),
                                embedding: None,
                                summary_text: None,
                                is_summary: false,
                                msg_type: "error".to_string(),
                                msg_subtype: Some("no_cause".to_string()),
                                iteration_number: 0,
                                duration_ms: 0,
                                token_usage: serde_json::json!({}),
                            };
                            if let Err(e) = queries::create_message(&cfg.pool, &err_msg).await {
                                tracing::warn!("[supervisor] Failed to create no-cause error msg for thread {}: {:?}", thread.id, e);
                            }
                            // Mark thread as failed
                            if let Err(e) = queries::complete_thread(&cfg.pool, thread.id, "failed", CompleteThreadStats { input_tokens: 0, cached_tokens: 0, output_tokens: 0, duration_ms: 0 }).await {
                                tracing::warn!("[supervisor] Failed to mark thread {} failed (no-cause): {:?}", thread.id, e);
                            }
                            continue;
                        }
                        Err(e) => {
                            error!("Failed to get cause message for thread {}: {:?}", thread.id, e);
                            let next_seq = queries::get_max_thread_sequence(&cfg.pool, thread.id).await.unwrap_or(0) + 1;
                            let err_msg = queries::MessageNew {
                                thread_id: thread.id,
                                role: "agent".to_string(),
                                content: format!("Failed to look up the thread's cause message: {}", e),
                                thread_sequence: next_seq,
                                external_id: None,
                                metadata: serde_json::json!({}),
                                embedding: None,
                                summary_text: None,
                                is_summary: false,
                                msg_type: "error".to_string(),
                                msg_subtype: Some("unknown_error".to_string()),
                                iteration_number: 0,
                                duration_ms: 0,
                                token_usage: serde_json::json!({}),
                            };
                            if let Err(e) = queries::create_message(&cfg.pool, &err_msg).await {
                                tracing::warn!("[supervisor] Failed to create error msg for thread {}: {:?}", thread.id, e);
                            }
                            if let Err(e) = queries::complete_thread(&cfg.pool, thread.id, "failed", CompleteThreadStats { input_tokens: 0, cached_tokens: 0, output_tokens: 0, duration_ms: 0 }).await {
                                tracing::warn!("[supervisor] Failed to mark thread {} failed (no-cause): {:?}", thread.id, e);
                            }
                            continue;
                        }
                    };

                    // Check message count limit before claiming the thread
                    // Take a config snapshot for consistent values during this check + processing
                    let cfg_snapshot = cfg.config_snapshot();
                    let max_iter = queries::max_iterations_for_plan(&cfg_snapshot, thread.plan);
                    match queries::count_thread_messages(&cfg.pool, thread.id).await {
                        Ok(count) if count >= max_iter as i32 => {
                            info!(
                                "Thread {} has reached message limit ({}/{}), skipping",
                                thread.id, count, max_iter
                            );
                            if let Err(e) = queries::complete_thread(&cfg.pool, thread.id, "skipped", CompleteThreadStats { input_tokens: 0, cached_tokens: 0, output_tokens: 0, duration_ms: 0 }).await {
                                tracing::warn!("[supervisor] Failed to mark thread {} skipped: {:?}", thread.id, e);
                            }
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
                    if !queries::claim_thread(&cfg.pool, thread.id).await {
                        debug!(
                            "Thread {} was already claimed by another worker, skipping",
                            thread.id
                        );
                        continue;
                    }

                    // If this thread is linked to a kanban task, mark it as running
                    if let Some(ref task_id) = thread.task_id {
                        if let Err(e) = queries::update_kanban_task_status(&cfg.pool, task_id, "running").await {
                            tracing::warn!("[supervisor] Failed to set kanban task {} running: {:?}", task_id, e);
                        }
                    }

                    if let Err(e) = process_thread(&cfg, thread, &cause_msg).await {
                        error!("Failed to process thread {}: {:?}", thread.id, e);
                        // Insert an error message with details
                        let next_seq = queries::get_max_thread_sequence(&cfg.pool, thread.id).await.unwrap_or(0) + 1;
                        let err_msg = queries::MessageNew {
                            thread_id: thread.id,
                            role: "agent".to_string(),
                            content: format!("Thread processing failed: {}", e),
                            thread_sequence: next_seq,
                            external_id: None,
                            metadata: serde_json::json!({}),
                            embedding: None,
                            summary_text: None,
                            is_summary: false,
                            msg_type: "error".to_string(),
                                                    msg_subtype: Some("spam".to_string()),
                                                    iteration_number: 0,
                                                    duration_ms: 0,
                                                    token_usage: serde_json::json!({}),
                                                };
                        if let Err(e) = queries::create_message(&cfg.pool, &err_msg).await {
                            tracing::warn!("[supervisor] Failed to create error msg for failed thread {}: {:?}", thread.id, e);
                        }
                        // Mark thread as failed
                        if let Err(e) = queries::complete_thread(&cfg.pool, thread.id, "failed", CompleteThreadStats { input_tokens: 0, cached_tokens: 0, output_tokens: 0, duration_ms: 0 }).await {
                            tracing::warn!("[supervisor] Failed to mark thread {} failed: {:?}", thread.id, e);
                        }
                        // If this thread is linked to a kanban task, mark it as blocked
                        if let Some(ref task_id) = thread.task_id {
                            if let Err(e) = queries::update_kanban_task_status(&cfg.pool, task_id, "blocked").await {
                                tracing::warn!("[supervisor] Failed to set kanban task {} blocked for failed thread {}: {:?}", task_id, thread.id, e);
                            }
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

/// On startup, find any threads that are still `processing` and mark them as `failed`.
/// Also skip all pending/processing threads.
/// Returns the number of recovered threads.
pub async fn skip_on_startup(pool: &PgPool) -> crate::error::AppResult<u64> {
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
            info!("[startup] DEBUG message {}: type={}", row.id, row.msg_type);
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
    // Record history before the update
    if let Err(e) = sql_forge!(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board)
        SELECT id, 'moved', 'ready', 'todo' FROM kanban_tasks WHERE status = 'ready'
        "#,
    )
    .execute(pool)
    .await {
        tracing::warn!("[startup] Failed to record kanban ready→todo history: {:?}", e);
    }

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
    // Record history before the update
    if let Err(e) = sql_forge!(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board)
        SELECT id, 'moved', 'running', 'blocked' FROM kanban_tasks WHERE status = 'running'
        "#,
    )
    .execute(pool)
    .await
    {
        tracing::warn!("[agent] Failed to record kanban history: {:?}", e);
    }

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
