//! Agent module — parallel channel processing supervisor.
//!
//! The agent supervisor runs a loop that:
//! 1. Recovers stale `processing` messages on startup.
//! 2. Lists all channels and spawns a dedicated `channel_handler` task for
//!    each channel that isn't already running.
//! 3. Checks for stopped channels and cancels their handlers via
//!    `CancellationToken`.
//! 4. Sleeps 5 seconds between iterations.
//!
//! Each `channel_handler` independently polls its channel for pending
//! messages, processes them via the LLM, and respects cancellation
//! requests from the `/stop` HTTP endpoint.

use anyhow::Result;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::db::queries;
use crate::llm::{ChatMessage, CompletionRequest, LLMClient};
use crate::models::{Message, MessageNew, MessageStatus};

/// Configuration for the agent's LLM interactions.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub llm_api_key: String,
    pub llm_model: String,
    pub llm_provider: String,
    pub llm_base_url: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub summarize_after_days: u32,
    pub max_iterations: u32,
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
            llm_api_key: std::env::var("LLM_API_KEY").unwrap_or_default(),
            llm_model: std::env::var("LLM_MODEL")
                .unwrap_or_else(|_| "gpt-4".to_string()),
            llm_provider: std::env::var("LLM_PROVIDER")
                .unwrap_or_else(|_| "openai".to_string()),
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
        })
    }
}

/// The core agent that supervises per-channel message processing.
pub struct Agent {
    pub pool: PgPool,
    pub config: AgentConfig,
    pub llm: Arc<LLMClient>,
}

impl Agent {
    /// Create a new agent from a database pool and configuration.
    ///
    /// An LLM client is built from the agent config, falling back to
    /// environment-level defaults for any unset values.
    pub fn new(pool: PgPool, config: AgentConfig) -> Self {
        let env_cfg = crate::llm::LLMConfig::from_env();
        let llm_config = crate::llm::LLMConfig {
            provider: config
                .llm_provider
                .parse()
                .unwrap_or(env_cfg.provider),
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
        Self { pool, config, llm }
    }

    /// Run the agent supervisor loop.
    ///
    /// This method:
    /// 1. Recovers stale `processing` messages on startup.
    /// 2. Continuously polls all channels.
    /// 3. Spawns a [`channel_handler`] for each new channel.
    /// 4. Cancels handlers for stopped channels.
    /// 5. Sleeps 5 seconds between iterations.
    ///
    /// The `cancel_tokens` map is shared with the HTTP server so the
    /// `/stop/{channel_id}` endpoint can cancel channel handlers.
    pub async fn run(
        self,
        cancel_tokens: Arc<Mutex<HashMap<i64, CancellationToken>>>,
    ) {
        // Recover any messages stuck in 'processing' for >5 minutes
        if let Err(e) = recover_stale_processing(&self.pool).await {
            error!("Failed to recover stale processing messages: {:?}", e);
        }

        let pool = self.pool;
        let llm = self.llm;
        let config = self.config;

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
                if !tokens.contains_key(&channel_id) {
                    let token = CancellationToken::new();
                    let handler_token = token.clone();
                    tokens.insert(channel_id, token);

                    let pool = pool.clone();
                    let llm = llm.clone();
                    let config = config.clone();

                    tokio::spawn(async move {
                        channel_handler(pool, llm, config, channel_id, handler_token).await;
                    });

                    info!(
                        "Spawned channel handler for channel {} ({})",
                        channel_id,
                        channels.iter().find(|c| c.id == channel_id).map(|c| c.name.as_str()).unwrap_or("unknown")
                    );
                }
            }

            // Cancel handlers for channels that have been stopped
            let stopped_ids: Vec<i64> = tokens.keys().copied().collect();
            for &channel_id in &stopped_ids {
                if let Some(token) = tokens.get(&channel_id) {
                    if !token.is_cancelled() {
                        if let Ok(Some(_)) =
                            queries::find_stopped_channel(&pool, channel_id).await
                        {
                            info!("Channel {} has been stopped, cancelling handler", channel_id);
                            token.cancel();
                        }
                    }
                }
            }

            // Prune tokens for channels that no longer exist in the DB
            let active_ids: Vec<i64> = channels.iter().map(|c| c.id).collect();
            tokens.retain(|k, _| active_ids.contains(k));

            drop(tokens);
            sleep(Duration::from_secs(5)).await;
        }
    }
}

/// Per-channel message processing loop.
///
/// This function runs as a separate tokio task for each channel. It:
/// 1. Checks cancellation at the start of each iteration.
/// 2. Checks if the channel has been stopped.
/// 3. Fetches pending messages for this channel.
/// 4. Processes each message via [`process_message`].
/// 5. Sleeps 1 second between iterations.
///
/// The loop exits cleanly when the cancellation token is triggered or
/// when the channel is marked as stopped in the database.
async fn channel_handler(
    pool: PgPool,
    llm: Arc<LLMClient>,
    config: AgentConfig,
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
                let _ = queries::skip_pending_messages(&pool, channel_id).await;
                break;
            }
            _ = async {
                // Check if the channel has been stopped in the DB
                if let Ok(Some(_)) = queries::find_stopped_channel(&pool, channel_id).await {
                    info!("Channel {} is stopped in DB, handler exiting", channel_id);
                    let _ = queries::skip_pending_messages(&pool, channel_id).await;
                    return;
                }

                // Fetch pending messages for this channel
                let messages = match queries::find_pending_messages(&pool, channel_id).await {
                    Ok(msgs) => msgs,
                    Err(e) => {
                        error!("Error fetching pending messages for channel {}: {:?}", channel_id, e);
                        return;
                    }
                };

                for msg in &messages {
                    // Best-effort cancellation check before each message
                    if cancel.is_cancelled() {
                        let _ = queries::skip_pending_messages(&pool, channel_id).await;
                        return;
                    }

                    // Check if the channel was stopped between batches
                    if let Ok(Some(_)) = queries::find_stopped_channel(&pool, channel_id).await {
                        info!("Channel {} stopped during batch processing", channel_id);
                        let _ = queries::skip_pending_messages(&pool, channel_id).await;
                        return;
                    }

                    info!("Processing message {} in channel {}", msg.id, channel_id);

                    // Check iteration limit before processing
                    match queries::count_thread_iterations(&pool, msg.thread_id).await {
                        Ok(count) if count >= config.max_iterations as i32 => {
                            info!(
                                "Thread {} has reached iteration limit ({}/{}), skipping message {}",
                                msg.thread_id, count, config.max_iterations, msg.id
                            );
                            let _ = queries::update_message_status(
                                &pool, msg.id, &MessageStatus::Skipped,
                            ).await;
                            continue;
                        }
                        Ok(_) => {} // under limit, proceed
                        Err(e) => {
                            error!("Failed to count thread iterations: {:?}", e);
                        }
                    }

                    if let Err(e) = process_message(&pool, &llm, &config, msg).await {
                        error!("Failed to process message {}: {:?}", msg.id, e);
                    }
                }

                // Brief pause between polling iterations
                tokio::time::sleep(Duration::from_secs(1)).await;
            } => {}
        }
    }

    info!("Channel handler finished for channel {}", channel_id);
}

/// Process a single pending message through the state machine:
///
/// 1. Update message status → `processing`
/// 2. Get current iteration count for the thread
/// 3. Call the LLM with system + user messages
/// 4. If reasoning exists, save as a separate `reasoning` record
/// 5. Save the main agent response (status: `completed`, msg_type: `message`)
/// 6. Update original message status → `completed`
/// 7. Return the saved response message
async fn process_message(
    pool: &PgPool,
    llm: &LLMClient,
    config: &AgentConfig,
    msg: &Message,
) -> Result<Message> {
    // 1. Mark the message as 'processing'
    queries::update_message_status(pool, msg.id, &MessageStatus::Processing).await?;

    // 2. Get current iteration count for this thread
    let iterations = queries::count_thread_iterations(pool, msg.thread_id).await.unwrap_or(0);
    let next_iteration = iterations + 1;

    // 3. Build LLM request
    let system_msg = ChatMessage {
        role: "system".to_string(),
        content: "You are OmniAgent, a helpful AI assistant.".to_string(),
    };
    let user_msg = ChatMessage {
        role: "user".to_string(),
        content: msg.content.clone(),
    };

    let request = CompletionRequest {
        messages: vec![system_msg, user_msg],
        max_tokens: config.max_tokens,
        temperature: config.temperature,
        stream: false,
    };

    let response = match llm.completion(request).await {
        Ok(resp) => resp,
        Err(e) => {
            error!("LLM call failed: {:?}", e);
            queries::update_message_status(pool, msg.id, &MessageStatus::Failed).await?;
            return Err(e);
        }
    };

    // 4. Build metadata with usage info
    let mut metadata = serde_json::json!({});
    if let Some(usage) = &response.usage {
        metadata["usage"] = serde_json::json!({
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "cached_tokens": usage.cached_tokens,
            "reasoning_tokens": usage.reasoning_tokens,
        });
    }

    // 5. If reasoning/thinking exists, save as its own record
    if let Some(reasoning_text) = &response.reasoning {
        if !reasoning_text.is_empty() {
            let reasoning_metadata = match metadata.get("usage") {
                Some(u) => serde_json::json!({"usage": u}),
                None => serde_json::json!({}),
            };
            let reasoning_msg = MessageNew {
                channel_id: msg.channel_id,
                role: "agent".to_string(),
                content: reasoning_text.clone(),
                status: MessageStatus::Completed,
                thread_id: msg.thread_id,
                thread_sequence: msg.thread_sequence + 1,
                external_id: None,
                metadata: reasoning_metadata,
                embedding: None,
                summary_text: None,
                is_summary: false,
                msg_type: "reasoning".to_string(),
                msg_subtype: None,
                iteration_count: next_iteration,
            };
            queries::create_message(pool, &reasoning_msg).await?;
        }
    }

    // 6. Save the main agent response
    let agent_msg = MessageNew {
        channel_id: msg.channel_id,
        role: "agent".to_string(),
        content: response.content,
        status: MessageStatus::Completed,
        thread_id: msg.thread_id,
        thread_sequence: msg.thread_sequence + 1,
        external_id: None,
        metadata,
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "message".to_string(),
        msg_subtype: None,
        iteration_count: next_iteration,
    };

    let saved = queries::create_message(pool, &agent_msg).await?;

    // 7. Mark the original message as 'completed'
    queries::update_message_status(pool, msg.id, &MessageStatus::Completed).await?;

    Ok(saved)
}

/// On startup, find any messages that are still `processing` but were created
/// more than 5 minutes ago — mark them as `failed` to unblock the channel.
///
/// Returns the number of recovered messages.
pub async fn recover_stale_processing(pool: &PgPool) -> Result<u64> {
    let five_min_ago = chrono::Utc::now() - chrono::Duration::minutes(5);
    let stale = queries::find_processing_older_than(pool, five_min_ago).await?;
    let count = stale.len() as u64;

    for msg in &stale {
        warn!(
            "Recovering stale processing message {} (created at {})",
            msg.id, msg.created_at
        );
        queries::update_message_status(pool, msg.id, &MessageStatus::Failed).await?;
    }

    if count > 0 {
        info!("Recovered {} stale processing messages", count);
    }

    Ok(count)
}
