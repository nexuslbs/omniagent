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

use crate::db::types as queries;
use crate::llm::{ChatMessage, CompletionRequest, LLMClient, Usage};
use crate::mcp::{AppContext, McpRegistry, McpToolCall};
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
    #[expect(dead_code)]
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
            provider: config.llm_provider.parse().unwrap_or(env_cfg.provider),
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
    /// 1. Recovers stale `processing` messages on startup.
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
                        if let Ok(Some(_)) = queries::find_stopped_channel(&pool, channel_id).await
                        {
                            info!(
                                "Channel {} has been stopped, cancelling handler",
                                channel_id
                            );
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

                    if let Err(e) = process_message(&pool, &llm, &config, &mcp, &ctx, msg).await {
                        error!("Failed to process message {}: {:?}", msg.id, e);
                        // Report error as a message in the same thread
                        let err_msg = MessageNew {
                            channel_id: msg.channel_id,
                            role: "system".to_string(),
                            content: format!(
                                "Error processing message {}: {}",
                                msg.id, e
                            ),
                            status: MessageStatus::Completed,
                            thread_id: Some(msg.thread_id),
                            thread_sequence: msg.thread_sequence + 1,
                            external_id: Some(format!("error:{}:{}", msg.id, chrono::Utc::now().timestamp())),
                            metadata: serde_json::json!({
                                "error_type": "processing",
                                "original_msg_id": msg.id,
                            }),
                            embedding: None,
                            summary_text: None,
                            is_summary: false,
                            msg_type: "tool".to_string(),
                            msg_subtype: Some("error".to_string()),
                            iteration_count: 0,
                            profile: msg.profile.clone(),
                            provider: None,
                            model: None,
                            processing_time_ms: None,
                            token_usage: None,
                        };
                        if let Err(e2) = crate::db::types::create_message(&pool, &err_msg).await {
                            error!("Failed to insert error message for {}: {:?}", msg.id, e2);
                        }
                        // Mark original message as failed
                        let _ = crate::db::types::update_message_status(
                            &pool, msg.id, &MessageStatus::Failed,
                        ).await;
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

/// Process a single pending message through the state machine:
///
/// 1. Update message status → `processing`
/// 2. Get current iteration count for the thread
/// 3. Resolve profile, provider, model from channel
/// 4. Call the LLM with system + user messages (and tools if enabled)
/// 5. If tool calls are returned, execute them and loop back to LLM
/// 6. If reasoning exists, save as a separate `reasoning` record
/// 7. Save the main agent response (msg_type: `message`)
/// 8. Generate a summary (outside iteration limit)
/// 9. Update original message status → `completed`, record processing_time_ms + token_usage
async fn process_message(
    pool: &PgPool,
    llm: &LLMClient,
    config: &AgentConfig,
    mcp: &McpRegistry,
    ctx: &AppContext,
    msg: &Message,
) -> Result<Message> {
    let start_time = std::time::Instant::now();

    // 1. Mark the message as 'processing'
    queries::update_message_status(pool, msg.id, &MessageStatus::Processing).await?;

    // 2. Get current iteration count for this thread
    let iterations = queries::count_thread_iterations(pool, msg.thread_id)
        .await
        .unwrap_or(0);
    let next_iteration = iterations + 1;

    // 3. Resolve profile, provider, model for this message
    let profile_name = if msg.profile.is_empty() {
        "default".to_string()
    } else {
        msg.profile.clone()
    };
    let provider_name = msg
        .provider
        .clone()
        .or_else(|| Some(config.llm_provider.clone()));
    let model_name = msg.model.clone().or_else(|| Some(config.llm_model.clone()));

    // 4. Build the initial message history with the structured system prompt
    let system_prompt = crate::prompt_builder::build_system_prompt(
        &ctx.memory_store,
        "",   // platform — will be enriched from channel metadata in the future
        None, // system_message
        &profile_name,
    );
    let mut messages = vec![
        ChatMessage::system(&system_prompt),
        ChatMessage::user(&msg.content),
    ];

    // 5. Get allowed tools for the profile and build tool definitions
    let profile = crate::profile::ProfileRegistry::new(&ctx.data_dir);
    let prof = profile.get(&profile_name).cloned().unwrap_or_else(|| {
        crate::profile::Profile::default("default")
    });
    let tools_def = mcp.to_openai_tools(&prof.allowed_tools);

    // 6. Tool-calling loop — max iterations controls total LLM calls
    let max_llm_calls = config.max_iterations.min(40); // safety cap
    let mut final_content = String::new();
    let mut final_reasoning: Option<String> = None;
    let mut final_tool_call: bool = false;
    let mut cumulative_usage: Option<Usage> = None;
    let mut limit_reached: bool = false;

    for turn in 0..max_llm_calls {
        let is_last_turn = turn == max_llm_calls - 1;
        if is_last_turn {
            // On the final allowed turn, hint to the model that it should
            // produce a final answer rather than more tool calls.
            messages.push(ChatMessage::system(
                "This is your last turn. You must provide your final answer now. \
                 Do not request additional tool calls.",
            ));
        }

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
        merge_usage(&mut cumulative_usage, response.usage);

        // Store reasoning if present
        if response.reasoning.is_some() {
            final_reasoning = response.reasoning.clone();
        }

        // Check for tool calls
        if response.tool_calls.is_empty() {
            // Normal text response — we're done
            final_content = response.content;
            final_tool_call = false;
            break;
        }

        // If we've reached the last turn and still got tool calls, force a response
        if is_last_turn {
            final_content = "I've completed the requested operations using my available tools, \
                            but reached the iteration limit. Please check the results above."
                .to_string();
            final_tool_call = false;
            limit_reached = true;
            break;
        }

        // We have tool calls — add assistant message with tool_calls
        final_tool_call = true;
        let mut assistant_msg = ChatMessage::assistant("");
        assistant_msg.tool_calls = Some(response.tool_calls.clone());
        messages.push(assistant_msg);

        // Execute each tool call
        for tc in &response.tool_calls {
            let mcp_call = McpToolCall {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                arguments: serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(serde_json::json!({})),
            };

            let result = mcp.execute(&mcp_call, ctx.clone());
            match result {
                Ok(res) => {
                    messages.push(ChatMessage::tool_result(
                        &tc.id,
                        &tc.function.name,
                        &res.content,
                    ));
                }
                Err(e) => {
                    let err_msg = format!("Error executing tool '{}': {}", tc.function.name, e);
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

    // 7. Serialize cumulative token usage to JSON for storage
    let token_usage_json = cumulative_usage.as_ref().map(|u| {
        serde_json::json!({
            "prompt_tokens": u.prompt_tokens,
            "completion_tokens": u.completion_tokens,
            "cached_tokens": u.cached_tokens,
            "reasoning_tokens": u.reasoning_tokens,
        })
    });

    // 8. If reasoning/thinking exists, save as its own record
    if let Some(ref reasoning_text) = final_reasoning {
        if !reasoning_text.is_empty() {
            let reasoning_msg = MessageNew {
                channel_id: msg.channel_id,
                role: "agent".to_string(),
                content: reasoning_text.clone(),
                status: MessageStatus::Completed,
                thread_id: Some(msg.thread_id),
                thread_sequence: msg.thread_sequence + 1,
                external_id: None,
                metadata: serde_json::json!({}),
                embedding: None,
                summary_text: None,
                is_summary: false,
                msg_type: "reasoning".to_string(),
                msg_subtype: None,
                iteration_count: next_iteration,
                profile: profile_name.clone(),
                provider: provider_name.clone(),
                model: model_name.clone(),
                processing_time_ms: None,
                token_usage: token_usage_json.clone(),
            };
            queries::create_message(pool, &reasoning_msg).await?;
        }
    }

    // 9. Save the main agent response
    let agent_msg = MessageNew {
        channel_id: msg.channel_id,
        role: "agent".to_string(),
        content: final_content,
        status: MessageStatus::Completed,
        thread_id: Some(msg.thread_id),
        thread_sequence: msg.thread_sequence + 1,
        external_id: None,
        metadata: serde_json::json!({}),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "message".to_string(),
        msg_subtype: None,
        iteration_count: next_iteration,
        profile: profile_name.clone(),
        provider: provider_name.clone(),
        model: model_name.clone(),
        processing_time_ms: None,
        token_usage: token_usage_json.clone(),
    };

    let saved = queries::create_message(pool, &agent_msg).await?;

    // 10. Generate a summary (outside the iteration limit)
    // Include the conversation context for the summarizer
    let mut summary_msgs = messages.clone();
    if limit_reached {
        summary_msgs.push(ChatMessage::system(&format!(
            "The iteration limit of {limit} was reached so the response may be incomplete. \
             Mention if the user needs to provide additional input or clarification. \
             Now summarize what was accomplished.",
            limit = max_llm_calls,
        )));
    } else {
        summary_msgs.push(ChatMessage::system("Now summarize what was accomplished."));
    }

    let summary_request = CompletionRequest {
        messages: summary_msgs,
        max_tokens: 512,
        temperature: 0.3,
        stream: false,
        tools: None,
    };

    let summary_text = match llm.completion(summary_request).await {
        Ok(resp) => {
            merge_usage(&mut cumulative_usage, resp.usage);
            resp.content
        }
        Err(e) => {
            warn!("Failed to generate summary: {:?}", e);
            format!("Summary generation failed: {}", e)
        }
    };

    // 11. Save the summary as its own record
    let summary_token_usage = cumulative_usage.as_ref().map(|u| {
        serde_json::json!({
            "prompt_tokens": u.prompt_tokens,
            "completion_tokens": u.completion_tokens,
            "cached_tokens": u.cached_tokens,
            "reasoning_tokens": u.reasoning_tokens,
        })
    });

    let summary_msg = MessageNew {
        channel_id: msg.channel_id,
        role: "agent".to_string(),
        content: summary_text,
        status: MessageStatus::Completed,
        thread_id: Some(msg.thread_id),
        thread_sequence: msg.thread_sequence + 2, // after reasoning (1) and message (1)
        external_id: None,
        metadata: serde_json::json!({}),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "summary".to_string(),
        msg_subtype: None,
        iteration_count: next_iteration,
        profile: profile_name.clone(),
        provider: provider_name.clone(),
        model: model_name.clone(),
        processing_time_ms: None,
        token_usage: summary_token_usage.clone(),
    };
    let _ = queries::create_message(pool, &summary_msg).await;

    // 12. Record processing time and cumulative token usage on the original prompt
    let elapsed_ms = start_time.elapsed().as_millis() as i32;
    sqlx::query(
        "UPDATE messages SET processing_time_ms = $1, token_usage = $2::jsonb, status = 'completed' WHERE id = $3 AND status = 'processing'",
    )
    .bind(elapsed_ms)
    .bind(&summary_token_usage)
    .bind(msg.id)
    .execute(pool)
    .await?;

    Ok(saved)
}

/// On startup, find any messages that are still `processing` but were created
/// more than 5 minutes ago — mark them as `failed` to unblock the channel.
///
/// Returns the number of recovered messages.
/// On startup, skip all messages left in pending or processing state.
/// Called from main.rs BEFORE spawning any concurrent tasks.
pub async fn skip_on_startup(pool: &PgPool) -> Result<u64> {
    // Debug: check specific message 122
    let specific: Result<(i64, String, String), _> =
        sqlx::query_as("SELECT id, status, msg_type FROM messages WHERE id = 122")
            .fetch_one(pool)
            .await;

    match &specific {
        Ok((id, status, msg_type)) => {
            info!(
                "[startup] DEBUG message {}: status={}, type={}",
                id, status, msg_type
            );
        }
        Err(e) => {
            info!("[startup] DEBUG message 122 not found: {}", e);
        }
    }

    // Debug: list ALL pending/processing messages before skipping
    let affected: Vec<(i64, String, String)> = sqlx::query_as(
        r#"
        SELECT id, status, msg_type
        FROM messages
        WHERE status IN ('pending', 'processing')
        ORDER BY id
        "#,
    )
    .fetch_all(pool)
    .await?;

    if affected.is_empty() {
        info!("[startup] No pending/processing messages to skip");
        return Ok(0);
    }

    for (id, status, msg_type) in &affected {
        info!(
            "[startup] Will skip message {} (status={}, type={})",
            id, status, msg_type
        );
    }

    let count = queries::skip_all_pending_processing(pool).await?;
    if count > 0 {
        info!(
            "[startup] Skipped {} pending/processing messages on startup",
            count
        );
    }
    Ok(count)
}
