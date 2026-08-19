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
pub(crate) mod context_builder;
pub mod executor;
pub(crate) mod fail_thread;
pub use fail_thread::{manual_review_decision, validate_review_decision, ReviewOutcome};
pub mod context_dump;
pub mod helpers;
pub mod kanban_updater;
pub(crate) mod main_loop;
pub mod plugin_manager;
pub(crate) mod response_handler;
pub mod summary_trigger;
pub mod task_registry;

use parking_lot::RwLock;
use sql_forge::sql_forge;
use sqlx::FromRow;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::agent::executor::process_thread;
use crate::agent::plugin_manager::PluginManager;
use crate::db::types as queries;
use crate::db::types::CompleteThreadStats;
use crate::llm::LLMClient;
use crate::mcp::AppContext;

// Re-export commonly used types (from config submodule).
pub use config::AgentConfig;
pub use config::AgentContext;

/// The core agent that supervises per-channel message processing.
pub struct Agent {
    pub pool: PgPool,
    pub config: Arc<RwLock<AgentConfig>>,
    pub llm: Arc<LLMClient>,
    pub ctx: AppContext,
    pub plugin_manager: Arc<dyn PluginManager>,
}

impl Agent {
    /// Create a new agent from a database pool and shared mutable configuration.
    ///
    /// An LLM client is built from the agent config, falling back to
    /// environment-level defaults for any unset values.
    pub fn new(
        pool: PgPool,
        config: Arc<RwLock<AgentConfig>>,
        ctx: AppContext,
        plugin_manager: Arc<dyn PluginManager>,
    ) -> Self {
        let env_cfg = crate::llm::LLMConfig::from_env();
        // Read config fields inside a scope so the borrow is dropped before
        // moving `config` into the struct.
        let (default_provider, llm_api_key, max_tokens, temperature) = {
            let cfg_read = config.read();
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
            max_tokens: max_tokens.unwrap_or(8192),
            temperature,
            supports_reasoning: crate::llm::PROVIDER_METADATA
                .read()
                .get(&provider_name)
                .map(|m| m.supports_reasoning)
                .unwrap_or(false),
        };
        let llm = Arc::new(LLMClient::new(llm_config));
        Self {
            pool,
            config,
            llm,
            ctx,
            plugin_manager,
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
    pub async fn run(self, cancel_tokens: Arc<Mutex<HashMap<String, CancellationToken>>>) {
        let agent_ctx = AgentContext {
            pool: self.pool,
            llm: self.llm,
            config: self.config,
            ctx: self.ctx,
            plugin_manager: self.plugin_manager,
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
            let channel_ids: Vec<String> = channels.iter().map(|c| c.id.clone()).collect();

            // Spawn handlers for channels not yet being processed
            for channel_id in &channel_ids {
                if let std::collections::hash_map::Entry::Vacant(e) =
                    tokens.entry(channel_id.clone())
                {
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
                    let cid = channel_id.clone();

                    tokio::spawn(async move {
                        channel_handler(cfg, cid, handler_token).await;
                    });

                    info!(
                        "Spawned channel handler for channel {} ({})",
                        channel_id,
                        channels
                            .iter()
                            .find(|c| &c.id == channel_id)
                            .map(|c| c.name.as_str())
                            .unwrap_or("unknown")
                    );
                }
            }

            // Cancel handlers for channels that have been stopped
            let stopped_ids: Vec<String> = tokens.keys().cloned().collect();
            for channel_id in &stopped_ids {
                if let Some(token) = tokens.get(channel_id) {
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
            let active_ids: Vec<String> = channels.iter().map(|c| c.id.clone()).collect();
            tokens.retain(|k, _| active_ids.contains(k));

            drop(tokens);
            sleep(Duration::from_secs(5)).await;
        }
    }
}

/// Cancel every in-flight task for all threads of a channel.
///
/// When a channel is stopped/closed (`/stop`, `/stop-thread`, `/close`) or the
/// supervisor cancels a channel handler, the handler's futures are dropped —
/// that already kills FOREGROUND tool calls via the MCP client's drop-cancel
/// guard. But the agent's BACKGROUND tool tasks are detached `tokio::spawn`ed
/// tasks that only stop when the task registry abort fires; without this
/// cleanup they keep the plugin request alive and the `docker compose exec …`
/// child keeps running with no consumer (thread 73, Aug 2026: cargo chain
/// still alive 6+ min after thread end).
async fn cancel_in_flight_for_channel(cfg: &AgentContext, channel_id: &str) {
    let registry = crate::agent::task_registry::TASK_REGISTRY.get().cloned();
    let Some(registry) = registry else {
        return; // registry not initialized — nothing to cancel
    };
    let thread_ids: Vec<i64> = match sql_forge!(
        scalar i64,
        "SELECT id FROM threads WHERE channel_id = :channel_id",
        ( :channel_id = channel_id )
    )
    .fetch_all(&cfg.pool)
    .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(
                "[supervisor] Failed to list threads for channel {} cleanup: {:?}",
                channel_id,
                e
            );
            return;
        }
    };
    for tid in thread_ids {
        let n = registry.cancel_all_for_thread(tid).await;
        if n > 0 {
            info!(
                "Channel {} cancelled {n} in-flight task(s) for thread {tid}",
                channel_id
            );
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
async fn channel_handler(cfg: AgentContext, channel_id: String, cancel: CancellationToken) {
    info!("Channel handler started for channel {}", channel_id);

    // The thread this handler is actively processing, shared with the
    // cancellation branch. Updated synchronously right after claim_thread
    // succeeds (before any await, so select! cannot drop the loop body in
    // between) and cleared after process_thread completes. When the handler
    // is cancelled mid-processing, the cancel branch uses it to skip the
    // orphaned thread (idempotent: no-op once it is already terminal).
    let active_thread = std::sync::Arc::new(std::sync::Mutex::new(None::<i64>));

    loop {
        // Use tokio::select! so cancellation is prompt rather than
        // waiting for the next iteration boundary.
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Channel {} handler cancelled", channel_id);
                // Safety net: never leave a `processing` thread ownerless. If
                // the handler was dropped mid-processing, the thread it was
                // actively processing is still `processing` — skip it (the
                // skip is a no-op when it already reached a terminal state).
                let active_id = active_thread.lock().unwrap().take();
                if let Some(active_id) = active_id {
                    if let Err(e) = queries::skip_thread(&cfg.pool, active_id).await {
                        tracing::warn!(
                            "[supervisor] Failed to skip active thread {} on cancel: {:?}",
                            active_id, e
                        );
                    } else {
                        info!(
                            "Channel {} handler cancelled: skipped in-flight thread {}",
                            channel_id, active_id
                        );
                    }
                }
                // Kill any tool-spawned subprocesses still running for this
                // channel's threads: the agent's BACKGROUND tool tasks are
                // detached and only stop when the registry abort fires —
                // without this, /stop-thread and /close would strand
                // `docker compose exec …` children (thread 73, Aug 2026).
                // (Foreground calls are already killed by dropping the
                // handler futures below.)
                cancel_in_flight_for_channel(&cfg, &channel_id).await;
                // Don't skip pending threads here: stop_thread_handler already marked the
                // specific thread as skipped before cancelling. Remaining pending threads
                // should survive and be picked up when the supervisor respawns this handler.
                break;
            }
            _ = async {
                // Check if the channel has been closed in the DB
                if let Ok(true) = queries::is_channel_closed(&cfg.pool, &channel_id).await {
                    info!("Channel {} is closed in DB, handler exiting", channel_id);
                    if let Err(e) = queries::skip_channel_threads(&cfg.pool, &channel_id).await {
                        tracing::warn!("[supervisor] Failed to skip threads for channel {}: {:?}", channel_id, e);
                    }
                    cancel_in_flight_for_channel(&cfg, &channel_id).await;
                    return;
                }

                // Fetch pending threads for this channel
                let threads = match queries::find_pending_threads_by_channel(&cfg.pool, &channel_id).await {
                    Ok(threads) => threads,
                    Err(e) => {
                        error!("Error fetching pending threads for channel {}: {:?}", channel_id, e);
                        return;
                    }
                };

                for thread in &threads {
                    // Best-effort cancellation check before each thread
                    if cancel.is_cancelled() {
                        cancel_in_flight_for_channel(&cfg, &channel_id).await;
                        // Don't skip pending threads: stop_thread_handler already handled
                        // the target thread. The supervisor will respawn the handler.
                        return;
                    }

                    // Check if the channel was closed between batches
                    if let Ok(true) = queries::is_channel_closed(&cfg.pool, &channel_id).await {
                        info!("Channel {} closed during batch processing", channel_id);
                        if let Err(e) = queries::skip_channel_threads(&cfg.pool, &channel_id).await {
                            tracing::warn!("[supervisor] Failed to skip threads for channel {}: {:?}", channel_id, e);
                        }
                        cancel_in_flight_for_channel(&cfg, &channel_id).await;
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
                                original_thread_id: None,
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
                            // Kanban-linked task must not stay "running" when the
                            // thread dies without a cause message.
                            crate::agent::kanban_updater::update_kanban_status(&cfg, thread, "failed").await;
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
                                original_thread_id: None,
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
                            crate::agent::kanban_updater::update_kanban_status(&cfg, thread, "failed").await;
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
                            crate::agent::kanban_updater::update_kanban_status(&cfg, thread, "skipped").await;
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
                    // Track the thread we're about to process: the cancellation
                    // branch skips it if the handler is dropped mid-flight.
                    *active_thread.lock().unwrap() = Some(thread.id);

                    // If this thread is linked to a kanban task, mark it as running
                                        if let Some(ref task_id) = thread.task_id {
                        // Map the thread's workflow step to the kanban status on pickup; the
                        // task status is already correct for re-run threads, and legacy
                        // (non-workflow) threads are plain "running". thread_status flips
                        // 'scheduled' -> 'running' (thread_status lifecycle, spec §5).
                        let target = match thread.workflow_step.as_deref() {
                        Some("testing") => "testing",
                        Some("review") => "review",
                        _ => "running",
                        };
                        if let Err(e) = queries::update_kanban_task_status(&cfg.pool, task_id, target).await {
                        tracing::warn!(
                        "[workflow] Failed to set kanban task {} to {}: {:?}",
                        task_id,
                        target,
                        e
                        );
                        }
                        if let Err(e) = queries::update_kanban_task_thread_status(&cfg.pool, task_id, "running").await
                        {
                        tracing::warn!(
                        "[workflow] Failed to set kanban task {} thread_status=running: {:?}",
                        task_id,
                        e
                        );
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
                            original_thread_id: None,
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
                    // process_thread finished (Ok or Err): the thread is now
                    // terminal — clear the active marker so a later
                    // cancellation skips nothing.
                    *active_thread.lock().unwrap() = None;
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
pub async fn skip_on_startup(pool: &PgPool, data_dir: &str) -> crate::error::AppResult<u64> {
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

        let c = queries::skip_all_pending_threads(pool, data_dir).await?;
        if c > 0 {
            info!(
                "[startup] Skipped {} pending/processing threads on startup",
                c
            );
        }
        c
    };

    // ── Phase 6 (R3): unified redispatch instead of resetting ──
    // skip_all_pending_threads marks every pending/processing thread terminal
    // and then redispatches every kanban task sitting in a workflow column
    // (running/testing/review) without an active thread: a fresh role thread
    // is created, thread_status = 'scheduled', kanban status UNCHANGED, NO
    // retry consumed. Tasks are never moved back to todo/blocked here.
    if count > 0 {
        info!(
            "[startup] Skipped {} pending/processing threads on startup; \
             kanban tasks in workflow columns were redispatched \
             (fresh thread, thread_status='scheduled', status unchanged)",
            count
        );
    }

    Ok(count)
}
