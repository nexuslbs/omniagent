//! Telegram platform — outbound + optional inbound message delivery.
//!
//! **Outbound** (sending agent responses to Telegram) runs in the `start()`
//! loop.  It polls the `messages` table for new rows belonging to the
//! Telegram channel and delivers them according to `msg_type`:
//!
//! | DB msg_type     | Telegram action                                    |
//! |-----------------|----------------------------------------------------|
//! | `tool`          | Edit the thread's tool message — append 🔧 tool:   |
//! | `tool_result`   | Skipped (not user-facing)                          |
//! | `message`       | New isolated message                               |
//! | `reasoning`     | New isolated message                               |
//! | `summary`       | Reply to the original user message (external_id)   |
//! | `plan`          | New isolated message                               |
//! | `error`         | New isolated message                               |
//!
//! **Telegram progress flag** (`channel.metadata.telegram_progress == true`):
//!   - On first tool call: send "⌛ 1/N"
//!   - On subsequent tool calls in same thread: edit to "⌛ N/M"
//!   - When summary arrives: delete progress message (summary is already a reply)
//!
//! **Inbound** is OFF by default.  Two transport options are implemented but
//! disabled behind env-var gates for future use:
//!
//! 1. **Long polling** (`TELEGRAM_POLLING_ENABLED=true`): polls `getUpdates`
//!    and inserts new messages as pending threads.
//! 2. **WebSocket** (`TELEGRAM_WS_URL` set): connects to a WebSocket bridge
//!    that relays Telegram updates (e.g. via tdlib or a bot-api gateway).

use anyhow::Result;
use async_trait::async_trait;
use sql_forge::sql_forge;
use sqlx::PgPool;
use std::collections::HashMap;
use tokio::time::{sleep, Duration};

use crate::platform::{Platform, OutboundReceiver};

// ---------------------------------------------------------------------------
// TelegramBotClient — thin HTTP wrapper around the Telegram Bot API
// ---------------------------------------------------------------------------

struct TelegramBotClient {
    http_client: reqwest::Client,
    api_base: String,
}

impl TelegramBotClient {
    fn new(bot_token: &str) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            api_base: format!("https://api.telegram.org/bot{}", bot_token),
        }
    }

    // ── Outbound ─────────────────────────────────────────────────────────

    /// Send a new text message to a chat.
    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        parse_mode: Option<&str>,
        reply_to_message_id: Option<i64>,
        disable_notification: bool,
    ) -> Result<i64> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "disable_notification": disable_notification,
        });
        if let Some(mode) = parse_mode {
            body["parse_mode"] = serde_json::json!(mode);
        }
        if let Some(reply_id) = reply_to_message_id {
            body["reply_to_message_id"] = serde_json::json!(reply_id);
        }

        let resp = self
            .http_client
            .post(format!("{}/sendMessage", self.api_base))
            .json(&body)
            .send()
            .await?;

        Self::extract_message_id(resp).await
    }

    /// Edit an existing message in a chat (replaces text).
    async fn edit_message_text(
        &self,
        chat_id: &str,
        message_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<i64> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
        });
        if let Some(mode) = parse_mode {
            body["parse_mode"] = serde_json::json!(mode);
        }

        let resp = self
            .http_client
            .post(format!("{}/editMessageText", self.api_base))
            .json(&body)
            .send()
            .await?;

        Self::extract_message_id(resp).await
    }

    /// Delete a message.
    #[allow(dead_code)]
    async fn delete_message(&self, chat_id: &str, message_id: i64) -> Result<()> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
        });

        let resp = self
            .http_client
            .post(format!("{}/deleteMessage", self.api_base))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            tracing::warn!("deleteMessage returned {}: {}", status, text);
        }
        Ok(())
    }

    // ── Inbound (polling) ────────────────────────────────────────────────

    /// Poll `getUpdates` for new messages since the given offset.
    /// Returns a list of Telegram Update objects.
    /// The `offset` is the last processed `update_id` + 1 (paging mechanism).
    #[allow(dead_code)]
    async fn get_updates(&self, offset: Option<i64>, timeout_secs: u32) -> Result<Vec<serde_json::Value>> {
        let mut body = serde_json::json!({
            "timeout": timeout_secs,
            "allowed_updates": ["message"],
        });
        if let Some(off) = offset {
            body["offset"] = serde_json::json!(off);
        }

        let resp = self
            .http_client
            .post(format!("{}/getUpdates", self.api_base))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;

        if !status.is_success() || !body["ok"].as_bool().unwrap_or(false) {
            let desc = body["description"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Telegram getUpdates error ({}): {}", status, desc);
        }

        let result = body["result"].as_array().cloned().unwrap_or_default();
        Ok(result)
    }

    /// Extract the `message_id` from a Telegram Bot API response.
    async fn extract_message_id(resp: reqwest::Response) -> Result<i64> {
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;

        if !status.is_success() || !body["ok"].as_bool().unwrap_or(false) {
            let desc = body["description"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Telegram API error ({}): {}", status, desc);
        }

        let msg_id = body["result"]["message_id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("Telegram response missing message_id"))?;

        Ok(msg_id)
    }
}

// ---------------------------------------------------------------------------
// Outbound message processor
// ---------------------------------------------------------------------------

/// In-memory state for a single Telegram channel being served by this platform.
struct ChannelState {
    /// Telegram chat_id (as string — can be negative for groups/channels).
    chat_id: String,
    /// DB channel_id.
    db_channel_id: i64,
    /// Max iterations for progress tracking.
    max_iterations: u32,
    /// Whether the progress indicator (⌛) is enabled.
    progress_enabled: bool,
    /// thread_id → Telegram message_id for tool-call edit messages.
    tool_edit_ids: HashMap<i64, i64>,
    /// thread_id → Telegram message_id for progress (⌛) indicator.
    progress_msg_ids: HashMap<i64, i64>,
    /// Highest DB messages.id we have processed.
    last_processed_id: i64,
}

impl ChannelState {
    fn new(chat_id: String, db_channel_id: i64, max_iterations: u32, metadata: &serde_json::Value) -> Self {
        let progress_enabled = metadata
            .get("telegram_progress")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Self {
            chat_id,
            db_channel_id,
            max_iterations,
            progress_enabled,
            tool_edit_ids: HashMap::new(),
            progress_msg_ids: HashMap::new(),
            last_processed_id: 0,
        }
    }
}

/// Describes an unsent message from the DB.
#[derive(Debug, sqlx::FromRow)]
struct UnsentMessage {
    id: i64,
    thread_id: i64,
    content: String,
    msg_type: String,
    msg_subtype: Option<String>,
    /// external_id on the cause message (seq-0) — the original Telegram message ID
    cause_external_id: Option<String>,
}

/// Process all unsent messages for a channel and deliver them to Telegram.
async fn process_outbound(
    bot: &TelegramBotClient,
    pool: &PgPool,
    state: &mut ChannelState,
) -> Result<()> {
    let msgs = fetch_unsent_messages(pool, state.db_channel_id, state.last_processed_id).await?;

    for msg in &msgs {
        let msg_id = msg.id;
        let thread_id = msg.thread_id;

        match msg.msg_type.as_str() {
            "tool_result" => {
                state.last_processed_id = msg_id;
                continue;
            }

            "tool" | "plan" | "reasoning" => {
                let line = match msg.msg_type.as_str() {
                    "plan" => "🔧 tool:planned".to_string(),
                    "reasoning" => "💭 reasoning".to_string(),
                    _ => {
                        let tool_name = msg.msg_subtype.as_deref().unwrap_or("unknown");
                        format!("🔧 tool:{}", tool_name)
                    }
                };

                if let Some(edit_msg_id) = state.tool_edit_ids.get(&thread_id) {
                    match bot.edit_message_text(&state.chat_id, *edit_msg_id, &line, None).await {
                        Ok(_) => {
                            state.tool_edit_ids.insert(thread_id, *edit_msg_id);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to edit tool message for thread {}: {:?}", thread_id, e);
                            match bot.send_message(&state.chat_id, &line, None, None, true).await {
                                Ok(new_id) => {
                                    state.tool_edit_ids.insert(thread_id, new_id);
                                    if state.progress_enabled && !state.progress_msg_ids.contains_key(&thread_id) {
                                        send_progress(bot, state, thread_id, 1).await;
                                    }
                                }
                                Err(e2) => {
                                    tracing::error!("Failed to send tool message for thread {}: {:?}", thread_id, e2);
                                }
                            }
                        }
                    }
                } else {
                    match bot.send_message(&state.chat_id, &line, None, None, true).await {
                        Ok(new_id) => {
                            state.tool_edit_ids.insert(thread_id, new_id);
                            if state.progress_enabled {
                                send_progress(bot, state, thread_id, 1).await;
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to send initial tool message for thread {}: {:?}", thread_id, e);
                        }
                    }
                }
                state.last_processed_id = msg_id;
            }

            "message" => {
                if !msg.content.trim().is_empty() {
                    if let Err(e) = bot.send_message(&state.chat_id, &msg.content, None, None, false).await {
                        tracing::warn!("Failed to send {} for thread {}: {:?}", msg.msg_type, thread_id, e);
                    }
                }
                state.last_processed_id = msg_id;
            }

            "summary" => {
                if let Some(ref reply_external_id) = msg.cause_external_id {
                    let reply_id: i64 = match reply_external_id.parse() {
                        Ok(id) => id,
                        Err(_) => {
                            tracing::warn!("Invalid cause external_id '{}' for thread {}", reply_external_id, thread_id);
                            state.last_processed_id = msg_id;
                            continue;
                        }
                    };

                    if state.progress_enabled {
                        if let Some(progress_id) = state.progress_msg_ids.remove(&thread_id) {
                            let _ = bot.delete_message(&state.chat_id, progress_id).await;
                        }
                    }
                    state.tool_edit_ids.remove(&thread_id);

                    if !msg.content.trim().is_empty() {
                        if let Err(e) = bot.send_message(&state.chat_id, &msg.content, None, Some(reply_id), false).await {
                            tracing::warn!("Failed to send summary reply for thread {}: {:?}", thread_id, e);
                        }
                    }
                } else {
                    if !msg.content.trim().is_empty() {
                        if let Err(e) = bot.send_message(&state.chat_id, &msg.content, None, None, false).await {
                            tracing::warn!("Failed to send summary (no reply target) for thread {}: {:?}", thread_id, e);
                        }
                    }
                }
                state.last_processed_id = msg_id;
            }

            "error" => {
                if !msg.content.trim().is_empty() {
                    if let Err(e) = bot.send_message(&state.chat_id, &msg.content, None, None, false).await {
                        tracing::warn!("Failed to send error for thread {}: {:?}", thread_id, e);
                    }
                }
                state.last_processed_id = msg_id;
            }

            _ => {
                tracing::debug!("Skipping unsupported msg_type '{}' for message {}", msg.msg_type, msg_id);
                state.last_processed_id = msg_id;
            }
        }
    }

    Ok(())
}

/// Send or update the progress indicator (⌛ N/M).
async fn send_progress(bot: &TelegramBotClient, state: &mut ChannelState, thread_id: i64, current: u32) {
    let total = state.max_iterations;
    let text = format!("⌛ {}/{}", current, total);

    if let Some(progress_id) = state.progress_msg_ids.get(&thread_id) {
        match bot.edit_message_text(&state.chat_id, *progress_id, &text, None).await {
            Ok(_) => {}
            Err(e) => tracing::warn!("Failed to edit progress for thread {}: {:?}", thread_id, e),
        }
    } else {
        match bot.send_message(&state.chat_id, &text, None, None, true).await {
            Ok(new_id) => { state.progress_msg_ids.insert(thread_id, new_id); }
            Err(e) => tracing::warn!("Failed to send progress for thread {}: {:?}", thread_id, e),
        }
    }
}

/// Fetch messages that haven't been sent to Telegram yet.
async fn fetch_unsent_messages(
    pool: &PgPool,
    channel_id: i64,
    last_processed_id: i64,
) -> Result<Vec<UnsentMessage>> {
    #[derive(Debug, sqlx::FromRow)]
    struct UnsentRow {
        id: i64,
        thread_id: i64,
        content: String,
        msg_type: String,
        msg_subtype: Option<String>,
        cause_external_id: Option<String>,
    }

    let rows: Vec<UnsentRow> = sql_forge!(
        UnsentRow,
        r#"
        SELECT
            m.id,
            m.thread_id,
            m.content,
            m.msg_type,
            m.msg_subtype,
            cause_ext.external_id AS "cause_external_id"
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        LEFT JOIN messages cause_ext ON cause_ext.thread_id = m.thread_id
            AND cause_ext.thread_sequence = 0
        WHERE t.channel_id = :channel_id
          AND m.id > :last_id
          AND t.status = 'completed'
        ORDER BY m.id ASC
        "#,
        ( :channel_id = channel_id, :last_id = last_processed_id )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|r| UnsentMessage {
        id: r.id,
        thread_id: r.thread_id,
        content: r.content,
        msg_type: r.msg_type,
        msg_subtype: r.msg_subtype,
        cause_external_id: r.cause_external_id,
    }).collect())
}

// ---------------------------------------------------------------------------
// Inbound: Long polling (getUpdates)
// ---------------------------------------------------------------------------

/// Process inbound messages via Telegram Bot API long polling.
///
/// Runs in a loop, calling `getUpdates` with a long timeout.  Each new
/// message is looked up by (platform, resource_identifier).  If a matching
/// channel exists, the message is inserted as a new pending thread.
/// If no channel is found for the chat, a notification is sent back to
/// inform the user that the chat is not configured.
///
/// **Disabled by default** — enable via `TELEGRAM_POLLING_ENABLED=true`.
#[allow(dead_code)]
async fn inbound_polling_loop(
    bot: TelegramBotClient,
    pool: PgPool,
) {
    tracing::info!("Telegram inbound polling started");

    let mut offset: Option<i64> = None;

    loop {
        match bot.get_updates(offset, 30).await {
            Ok(updates) => {
                for update in &updates {
                    let update_id = update["update_id"].as_i64().unwrap_or(0);

                    // Extract the message from the update
                    let msg = match update.get("message") {
                        Some(m) => m,
                        None => {
                            offset = Some(update_id + 1);
                            continue;
                        }
                    };

                    let msg_chat_id = msg["chat"]["id"].as_i64().unwrap_or(0);
                    let msg_chat_id_str = msg_chat_id.to_string();

                    // Extract text content
                    let text = msg["text"].as_str().unwrap_or("").to_string();
                    if text.is_empty() {
                        offset = Some(update_id + 1);
                        continue;
                    }

                    let telegram_msg_id = msg["message_id"].as_i64().unwrap_or(0);

                    // Look up channel by (platform, resource_identifier)
                    match crate::db::types::get_channel_by_platform_and_resource(
                        &pool,
                        "telegram",
                        &msg_chat_id_str,
                    )
                    .await
                    {
                        Ok(Some(channel)) => {
                            tracing::info!(
                                "Inbound Telegram message from chat {} (channel {}): {}",
                                msg_chat_id,
                                channel.id,
                                text.chars().take(100).collect::<String>()
                            );

                            // Check for /model command
                            if text.starts_with("/model") {
                                if let Err(e) = handle_telegram_model_command(
                                    &bot,
                                    &pool,
                                    &msg_chat_id_str,
                                    channel.id,
                                    &text,
                                ).await {
                                    tracing::error!(
                                        "Failed to handle /model command from {}: {:?}",
                                        msg_chat_id_str,
                                        e
                                    );
                                }
                                continue;
                            }

                            // Insert as a new thread into the DB
                            if let Err(e) = insert_inbound_message(&pool, channel.id, &text, telegram_msg_id).await {
                                tracing::error!("Failed to insert inbound message: {:?}", e);
                            }
                        }
                        Ok(None) => {
                            // No channel found — orphan message, send notification
                            tracing::info!(
                                "Orphan inbound message from unknown chat {} — sending notification",
                                msg_chat_id
                            );
                            let notification = format!(
                                "This chat is not configured for the agent. No active channel found."
                            );
                            if let Err(e) = bot
                                .send_message(&msg_chat_id_str, &notification, None, None, false)
                                .await
                            {
                                tracing::warn!(
                                    "Failed to send orphan notification to {}: {:?}",
                                    msg_chat_id_str,
                                    e
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to look up channel for chat {}: {:?}",
                                msg_chat_id_str,
                                e
                            );
                        }
                    }

                    offset = Some(update_id + 1);
                }
            }
            Err(e) => {
                tracing::error!("Telegram inbound polling error: {:?}", e);
                sleep(Duration::from_secs(5)).await;
            }
        }

        sleep(Duration::from_millis(200)).await;
    }
}

/// Insert a message received from Telegram as a new pending thread.
async fn insert_inbound_message(
    pool: &PgPool,
    channel_id: i64,
    text: &str,
    telegram_msg_id: i64,
) -> Result<()> {
    // Get the channel's current_profile for stamping
    let channel = crate::db::types::get_channel_by_id(pool, channel_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Channel {} not found", channel_id))?;

    let profile_name = channel.current_profile;
    let provider = channel.current_provider.unwrap_or_else(|| {
        std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "opencode-go".to_string())
    });
    let model = channel.current_model.unwrap_or_else(|| {
        std::env::var("LLM_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string())
    });

    // Create a new thread
    let thread = crate::db::types::create_thread(
        pool,
        "user",
        channel_id,
        &profile_name,
        Some(&provider),
        Some(&model),
        None,
        None,
    )
    .await?;

    // Insert the seq-0 user/cause message
    let msg = crate::models::MessageNew {
        thread_id: thread.id,
        role: "cause".to_string(),
        content: text.to_string(),
        thread_sequence: 0,
        external_id: Some(telegram_msg_id.to_string()),
        metadata: serde_json::json!({
            "telegram_chat_id": channel.external_id,
            "telegram_msg_id": telegram_msg_id,
        }),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "message".to_string(),
        msg_subtype: None,
        processing_time_ms: None,
        token_usage: None,
    };

    crate::db::types::create_cause_and_set_pending(pool, &msg).await?;

    tracing::info!(
        "Created thread {} for inbound Telegram msg_id={}",
        thread.id,
        telegram_msg_id
    );

    Ok(())
}

/// Handle a `/model` command received via Telegram.
async fn handle_telegram_model_command(
    bot: &TelegramBotClient,
    pool: &sqlx::PgPool,
    chat_id: &str,
    channel_id: i64,
    text: &str,
) -> anyhow::Result<()> {
    let parsed = match crate::commands::parse_model_command(text) {
        Ok(cmd) => cmd,
        Err(e) => {
            let msg = format!("Error: {}", e);
            if let Err(send_err) = bot.send_message(chat_id, &msg, None, None, false).await {
                tracing::warn!("Failed to send error reply to {}: {:?}", chat_id, send_err);
            }
            return Ok(());
        }
    };

    let reply = match parsed.action {
        crate::commands::ModelAction::Show => {
            match crate::db::types::get_channel_by_id(pool, channel_id).await? {
                Some(ch) => crate::commands::format_model_status(
                    ch.current_provider.as_deref(),
                    ch.current_model.as_deref(),
                ),
                None => "Channel not found.".to_string(),
            }
        }
        crate::commands::ModelAction::Set { provider, model } => {
            // Validate provider if provided
            if let Some(ref p) = provider {
                if !p.is_empty() {
                    if let Err(e) = crate::commands::validate_provider(pool, p).await {
                        let msg = format!("Error: {}", e);
                        if let Err(send_err) = bot.send_message(chat_id, &msg, None, None, false).await {
                            tracing::warn!("Failed to send error reply to {}: {:?}", chat_id, send_err);
                        }
                        return Ok(());
                    }
                }
            }

            let update_provider = provider.as_deref();
            let update_model = model.as_deref();
            crate::db::types::update_channel_model(pool, channel_id, update_provider, update_model).await?;

            let provider_display = update_provider.unwrap_or("(unchanged)");
            let model_display = update_model.unwrap_or("(unchanged)");
            format!(
                "✅ Channel updated — provider: {}, model: {}",
                provider_display, model_display
            )
        }
        crate::commands::ModelAction::Reset { provider, model } => {
            let update_provider = if provider { Some("") } else { None };
            let update_model = if model { Some("") } else { None };
            crate::db::types::update_channel_model(pool, channel_id, update_provider, update_model).await?;

            let parts = vec![
                if provider { "provider" } else { "" },
                if model { "model" } else { "" },
            ];
            let parts: Vec<&str> = parts.into_iter().filter(|s| !s.is_empty()).collect();
            format!(
                "✅ Channel {} reset — will fall back to profile/env defaults.",
                parts.join(" and ")
            )
        }
    };

    if let Err(e) = bot.send_message(chat_id, &reply, None, None, false).await {
        tracing::warn!("Failed to send /model reply to {}: {:?}", chat_id, e);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Inbound: WebSocket bridge
// ---------------------------------------------------------------------------

/// WebSocket inbound loop.
///
/// Connects to a WebSocket bridge that relays Telegram message events.
/// The bridge is expected to send JSON-encoded messages matching the
/// Telegram Bot API Update schema on each frame.
///
/// **Disabled by default** — configure via `TELEGRAM_WS_URL` env var.
///
/// This is a stub/placeholder.  A real implementation would:
/// 1. Connect to the WS endpoint using tokio-tungstenite.
/// 2. Read JSON frames and convert them into inbound message inserts.
/// 3. Reconnect on disconnection with exponential backoff.
#[allow(dead_code)]
async fn inbound_websocket_loop(
    _pool: PgPool,
    _db_channel_id: i64,
    ws_url: String,
) {
    tracing::info!(
        "Telegram WebSocket inbound configured (url={}) — not yet implemented, staying alive",
        ws_url
    );

    // Keep the task alive as a placeholder.
    // When implementing: use tokio_tungstenite::connect_async() and
    // read messages from the stream, calling insert_inbound_message() for each.
    futures::future::pending::<()>().await;
}

// ---------------------------------------------------------------------------
// Find or create the Telegram channel in the DB
// ---------------------------------------------------------------------------

async fn find_or_create_channel(pool: &PgPool, chat_id: &str) -> Result<i64> {
    // First try by (platform, resource_identifier)
    if let Ok(Some(ch)) = crate::db::types::get_channel_by_platform_and_resource(pool, "telegram", chat_id).await {
        return Ok(ch.id);
    }

    // Fall back to old lookup by (platform, external_id) for backward compat
    #[derive(Debug, sqlx::FromRow)]
    struct ChannelRow {
        id: i64,
    }

    let existing: Option<ChannelRow> = sql_forge!(
        ChannelRow,
        r#"
        SELECT id FROM channels
        WHERE platform = 'telegram' AND external_id = :chat_id
        LIMIT 1
        "#,
        ( :chat_id = chat_id )
    )
    .fetch_optional(pool)
    .await?;

    if let Some(row) = existing {
        return Ok(row.id);
    }

    let channel = crate::db::types::create_channel(
        pool,
        &format!("telegram-{}", chat_id),
        "telegram",
        chat_id,
        "user",
        chat_id,
    )
    .await?;

    tracing::info!(
        "Created telegram channel '{}' (id={}) in DB",
        channel.name,
        channel.id
    );
    Ok(channel.id)
}

// ---------------------------------------------------------------------------
// TelegramPlatform
// ---------------------------------------------------------------------------

/// Telegram platform for outbound delivery (always active when configured)
/// and optional inbound via polling or WebSocket (both disabled by default).
pub struct TelegramPlatform {
    bot_token: String,
    chat_id: String,
}

impl TelegramPlatform {
    pub fn new() -> Self {
        let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
        let chat_id = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();
        Self { bot_token, chat_id }
    }

    fn is_enabled(&self) -> bool {
        !self.bot_token.is_empty() && !self.chat_id.is_empty()
    }
}

#[async_trait]
impl Platform for TelegramPlatform {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn start(&self, pool: PgPool, receiver: OutboundReceiver) -> Result<()> {
        if !self.is_enabled() {
            tracing::info!(
                "Telegram platform not configured (set TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID) — staying alive as stub"
            );
            // Drop receiver to close the channel so senders don't block
            drop(receiver);
            futures::future::pending::<()>().await;
            return Ok(());
        }

        tracing::info!("Telegram platform starting — serving chat_id '{}'", self.chat_id);

        let bot = TelegramBotClient::new(&self.bot_token);

        let db_channel_id = match find_or_create_channel(&pool, &self.chat_id).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to find/create telegram channel: {:?}", e);
                return Err(e);
            }
        };

        let channel = crate::db::types::get_channel_by_id(&pool, db_channel_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Channel {} disappeared", db_channel_id))?;

        let max_iterations = std::env::var("MAX_ITERATIONS")
            .unwrap_or_else(|_| "60".to_string())
            .parse::<u32>()
            .unwrap_or(60);

        // ── Optional: Spawn inbound polling ──────────────────────────────
        let polling_enabled = std::env::var("TELEGRAM_POLLING_ENABLED")
            .unwrap_or_default()
            .to_lowercase() == "true";

        if polling_enabled {
            let bot_clone = TelegramBotClient::new(&self.bot_token);
            let pool_clone = pool.clone();
            tokio::spawn(async move {
                inbound_polling_loop(bot_clone, pool_clone).await;
            });
            tracing::info!("Telegram inbound polling enabled");
        } else {
            tracing::info!("Telegram inbound polling disabled (set TELEGRAM_POLLING_ENABLED=true to enable)");
        }

        // ── Optional: Spawn inbound websocket ────────────────────────────
        let ws_url = std::env::var("TELEGRAM_WS_URL").unwrap_or_default();
        if !ws_url.is_empty() {
            let pool_clone = pool.clone();
            tokio::spawn(async move {
                inbound_websocket_loop(pool_clone, db_channel_id, ws_url).await;
            });
            tracing::info!("Telegram WebSocket inbound configured via TELEGRAM_WS_URL");
        } else {
            tracing::debug!("Telegram WebSocket inbound not configured");
        }

        // ── Outbound state ───────────────────────────────────────────────
        let mut state = ChannelState::new(
            self.chat_id.clone(),
            db_channel_id,
            max_iterations,
            &channel.metadata,
        );

        tracing::info!(
            "Telegram platform active — channel_id={}, progress_enabled={}",
            db_channel_id,
            state.progress_enabled,
        );

        // ── Outbound loop — drain notification envelopes, then poll DB ────
        let mut receiver = receiver;
        loop {
            // Drain notification envelopes from the outbound queue (non-blocking)
            loop {
                match receiver.try_recv() {
                    Ok(envelope) => {
                        if envelope.msg_type == "notification" && !envelope.content.trim().is_empty() {
                            let chat_id = &envelope.resource_identifier;
                            tracing::info!(
                                "Delivering notification to chat_id={}: {}",
                                chat_id,
                                envelope.content.chars().take(80).collect::<String>()
                            );
                            if let Err(e) = bot
                                .send_message(chat_id, &envelope.content, None, None, false)
                                .await
                            {
                                tracing::warn!(
                                    "Failed to send notification to {}: {:?}",
                                    chat_id,
                                    e
                                );
                            }
                        } else {
                            tracing::debug!(
                                "Ignoring envelope msg_type='{}' in notification drain",
                                envelope.msg_type
                            );
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        tracing::warn!("Telegram outbound channel disconnected");
                        break;
                    }
                }
            }

            // Poll DB for new unsent messages
            if let Err(e) = process_outbound(&bot, &pool, &mut state).await {
                tracing::error!("Telegram outbound processing error: {:?}", e);
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    async fn send_response(&self, _pool: &PgPool, _message_id: i64) -> Result<()> {
        Ok(())
    }
}
