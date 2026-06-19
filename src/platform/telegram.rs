//! Telegram platform — outbound message delivery to Telegram.
//!
//! **Inbound** (receiving messages from Telegram) is disabled by design.
//! The user will add a webhook endpoint, websocket relay, or another
//! mechanism later.
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

use anyhow::Result;
use async_trait::async_trait;
use sql_forge::sql_forge;
use sqlx::PgPool;
use std::collections::HashMap;
use tokio::time::{sleep, Duration};

use crate::platform::Platform;

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

        // editMessageText returns the edited message on success
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
            // Deleting a message that doesn't exist is fine — ignore
            tracing::warn!("deleteMessage returned {}: {}", status, text);
        }
        Ok(())
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
                // Tool results are never user-facing — skip entirely.
                state.last_processed_id = msg_id;
                continue;
            }

            "tool" => {
                // Tool call — edit the thread's tool message.
                let tool_name = msg.msg_subtype.as_deref().unwrap_or("unknown");
                let line = format!("🔧 tool:{}", tool_name);

                // First tool call in this thread — send a new message.
                // Subsequent tool calls — append to the existing message.
                if let Some(edit_msg_id) = state.tool_edit_ids.get(&thread_id) {
                    // Edit the existing message to append this tool call.
                    // We store the accumulated tool list in the edit message.
                    // Fetch the current text, append the new tool.
                    // Actually, we track tools in a per-thread list.
                    match bot.edit_message_text(
                        &state.chat_id,
                        *edit_msg_id,
                        &line,
                        None,
                    )
                    .await
                    {
                        Ok(_) => {
                            state.tool_edit_ids.insert(thread_id, *edit_msg_id);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to edit tool message for thread {}: {:?}",
                                thread_id,
                                e
                            );
                            // Message may have been deleted — send fresh.
                            match bot
                                .send_message(&state.chat_id, &line, None, None, true)
                                .await
                            {
                                Ok(new_id) => {
                                    state.tool_edit_ids.insert(thread_id, new_id);

                                    // Progress flag: first tool call in a fresh thread
                                    if state.progress_enabled
                                        && !state.progress_msg_ids.contains_key(&thread_id)
                                    {
                                        send_progress(
                                            bot,
                                            state,
                                            thread_id,
                                            1,
                                        )
                                        .await;
                                    }
                                }
                                Err(e2) => {
                                    tracing::error!(
                                        "Failed to send tool message for thread {}: {:?}",
                                        thread_id,
                                        e2
                                    );
                                }
                            }
                        }
                    }
                } else {
                    // First tool call in this thread — send new message.
                    match bot
                        .send_message(&state.chat_id, &line, None, None, true)
                        .await
                    {
                        Ok(new_id) => {
                            state.tool_edit_ids.insert(thread_id, new_id);

                            // Progress flag: first iteration
                            if state.progress_enabled {
                                send_progress(bot, state, thread_id, 1).await;
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to send initial tool message for thread {}: {:?}",
                                thread_id,
                                e
                            );
                        }
                    }
                }
                state.last_processed_id = msg_id;
            }

            "reasoning" => {
                // Send as new isolated message (quiet).
                if !msg.content.trim().is_empty() {
                    if let Err(e) = bot
                        .send_message(&state.chat_id, &msg.content, None, None, true)
                        .await
                    {
                        tracing::warn!(
                            "Failed to send reasoning for thread {}: {:?}",
                            thread_id,
                            e
                        );
                    }
                }
                state.last_processed_id = msg_id;
            }

            "message" | "plan" => {
                // Send as new isolated message (notifiable).
                if !msg.content.trim().is_empty() {
                    if let Err(e) = bot
                        .send_message(&state.chat_id, &msg.content, None, None, false)
                        .await
                    {
                        tracing::warn!(
                            "Failed to send {} for thread {}: {:?}",
                            msg.msg_type,
                            thread_id,
                            e
                        );
                    }
                }
                state.last_processed_id = msg_id;
            }

            "summary" => {
                // Send as reply to the original user message.
                if let Some(ref reply_external_id) = msg.cause_external_id {
                    let reply_id: i64 = match reply_external_id.parse() {
                        Ok(id) => id,
                        Err(_) => {
                            tracing::warn!(
                                "Invalid cause external_id '{}' for thread {}",
                                reply_external_id,
                                thread_id
                            );
                            state.last_processed_id = msg_id;
                            continue;
                        }
                    };

                    // Clean up progress message if enabled
                    if state.progress_enabled {
                        if let Some(progress_id) = state.progress_msg_ids.remove(&thread_id) {
                            let _ = bot.delete_message(&state.chat_id, progress_id).await;
                        }
                    }

                    // Clean up tool edit message (no longer needed)
                    state.tool_edit_ids.remove(&thread_id);

                    if !msg.content.trim().is_empty() {
                        if let Err(e) = bot
                            .send_message(
                                &state.chat_id,
                                &msg.content,
                                None,
                                Some(reply_id),
                                false,
                            )
                            .await
                        {
                            tracing::warn!(
                                "Failed to send summary reply for thread {}: {:?}",
                                thread_id,
                                e
                            );
                        }
                    }
                } else {
                    // No reply target — send as isolated message.
                    if !msg.content.trim().is_empty() {
                        if let Err(e) = bot
                            .send_message(&state.chat_id, &msg.content, None, None, false)
                            .await
                        {
                            tracing::warn!(
                                "Failed to send summary (no reply target) for thread {}: {:?}",
                                thread_id,
                                e
                            );
                        }
                    }
                }
                state.last_processed_id = msg_id;
            }

            "error" => {
                // Send as new isolated message (notifiable).
                if !msg.content.trim().is_empty() {
                    if let Err(e) = bot
                        .send_message(&state.chat_id, &msg.content, None, None, false)
                        .await
                    {
                        tracing::warn!(
                            "Failed to send error for thread {}: {:?}",
                            thread_id,
                            e
                        );
                    }
                }
                state.last_processed_id = msg_id;
            }

            _ => {
                // Unknown type — skip but advance cursor.
                tracing::debug!(
                    "Skipping unsupported msg_type '{}' for message {}",
                    msg.msg_type,
                    msg_id
                );
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
        // Edit existing progress message
        match bot
            .edit_message_text(&state.chat_id, *progress_id, &text, None)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    "Failed to edit progress for thread {}: {:?}",
                    thread_id,
                    e
                );
            }
        }
    } else {
        // Send new progress message
        match bot
            .send_message(&state.chat_id, &text, None, None, true)
            .await
        {
            Ok(new_id) => {
                state.progress_msg_ids.insert(thread_id, new_id);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to send progress for thread {}: {:?}",
                    thread_id,
                    e
                );
            }
        }
    }
}

/// Fetch messages that haven't been sent to Telegram yet.
///
/// Returns messages with `id > last_processed_id` that belong to the
/// telegram channel, ordered by `id ASC`.  Each row also carries the
/// cause message's `external_id` so we know which Telegram message to
/// reply to for summaries.
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

/// Find or create the telegram channel in the DB.
async fn find_or_create_channel(
    pool: &PgPool,
    chat_id: &str,
) -> Result<i64> {
    // Try to find existing channel
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

    // Create new channel
    let channel = crate::db::types::create_channel(
        pool,
        &format!("telegram-{}", chat_id),
        "telegram",
        chat_id,
        "user",
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

/// Telegram outbound delivery platform.
///
/// The platform stays alive in `start()` and polls the DB for new completed
/// messages in the Telegram channel.  It does NOT poll the Telegram API for
/// inbound messages — that will be added later via webhook/websocket.
pub struct TelegramPlatform {
    /// Bot API token.  Empty = disabled (stub mode).
    bot_token: String,
    /// Chat ID to serve (e.g. "-1001234567890").
    /// If empty, the platform stays in stub mode.
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

    async fn start(&self, pool: PgPool) -> Result<()> {
        if !self.is_enabled() {
            tracing::info!(
                "Telegram platform not configured (set TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID) — staying alive as stub"
            );
            futures::future::pending::<()>().await;
            return Ok(());
        }

        tracing::info!(
            "Telegram platform starting — serving chat_id '{}'",
            self.chat_id
        );

        let bot = TelegramBotClient::new(&self.bot_token);

        // Find or create the channel in DB
        let db_channel_id = match find_or_create_channel(&pool, &self.chat_id).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(
                    "Failed to find/create telegram channel: {:?}",
                    e
                );
                return Err(e);
            }
        };

        // Load channel metadata
        let channel = crate::db::types::get_channel_by_id(&pool, db_channel_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Channel {} disappeared", db_channel_id))?;

        // Load max_iterations from agent config
        let max_iterations = std::env::var("MAX_ITERATIONS")
            .unwrap_or_else(|_| "60".to_string())
            .parse::<u32>()
            .unwrap_or(60);

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

        // Outbound loop — poll DB for new messages and deliver to Telegram.
        loop {
            if let Err(e) = process_outbound(&bot, &pool, &mut state).await {
                tracing::error!("Telegram outbound processing error: {:?}", e);
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    async fn send_response(&self, _pool: &PgPool, _message_id: i64) -> Result<()> {
        // Outbound delivery is handled by the polling loop in start().
        // This method is a no-op.
        Ok(())
    }
}
