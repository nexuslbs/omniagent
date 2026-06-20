//! Telegram Platform Plugin for OmniAgent.
//!
//! Standalone binary that implements the platform plugin protocol over stdio.
//! Communicates with the Telegram Bot API to send, edit, and delete messages.
//!
//! Protocol: JSON-lines over stdin/stdout.
//!
//! Methods:
//!   - initialize:    Return plugin info and capabilities
//!   - deliver:       Send a message to a Telegram chat
//!   - edit_message:  Edit an existing Telegram message
//!   - delete_message: Delete a Telegram message
//!
//! Inbound (optional, TELEGRAM_POLLING_ENABLED=true):
//!   Polls getUpdates and sends inbound_message notifications to stdout.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// ---------------------------------------------------------------------------
// Telegram Bot API Client
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

        Self::extract_message_id(resp).await
    }

    /// Delete a message from a chat.
    async fn delete_message(&self, chat_id: &str, message_id: i64) -> Result<bool> {
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
        if status.is_success() {
            Ok(true)
        } else {
            // 400 Bad Request means the message was already deleted or invalid
            if status.as_u16() == 400 {
                Ok(false)
            } else {
                let text = resp.text().await.unwrap_or_default();
                Err(anyhow::anyhow!("Telegram deleteMessage failed ({}): {}", status, text))
            }
        }
    }

    /// Extract the message_id from a Telegram API response.
    async fn extract_message_id(resp: reqwest::Response) -> Result<i64> {
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse Telegram API response JSON")?;

        if !body["ok"].as_bool().unwrap_or(false) {
            let desc = body["description"]
                .as_str()
                .unwrap_or("unknown error");
            // If it's a "message not modified" error, return 0 (not fatal)
            if desc.contains("message is not modified") {
                return Ok(0);
            }
            return Err(anyhow::anyhow!(
                "Telegram API error ({}): {}",
                status,
                desc
            ));
        }

        body["result"]["message_id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("Telegram API response missing message_id"))
    }

    /// Fetch updates via long polling for inbound messages.
    async fn get_updates(&self, offset: &mut i64, timeout: i64) -> Result<Vec<TelegramUpdate>> {
        let body = serde_json::json!({
            "offset": offset,
            "timeout": timeout,
            "allowed_updates": ["message", "callback_query"],
        });

        let resp = self
            .http_client
            .post(format!("{}/getUpdates", self.api_base))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse getUpdates response")?;

        if !body["ok"].as_bool().unwrap_or(false) {
            return Err(anyhow::anyhow!(
                "Telegram getUpdates failed ({}): {}",
                status,
                body["description"].as_str().unwrap_or("unknown")
            ));
        }

        let updates: Vec<TelegramUpdate> =
            serde_json::from_value(body["result"].clone())
                .context("Failed to parse updates array")?;

        // Update offset to acknowledge received updates
        if let Some(max_id) = updates.iter().map(|u| u.update_id).max() {
            *offset = max_id + 1;
        }

        Ok(updates)
    }
}

// ---------------------------------------------------------------------------
// Telegram Update types (for inbound polling)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<TelegramMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat: Option<TelegramChat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<TelegramUser>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramChat {
    id: Value, // can be i64 or string
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    chat_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramUser {
    id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_bot: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Plugin Protocol Types
// ---------------------------------------------------------------------------

/// A request received from the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

/// A response sent to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum PluginResponse {
    Success {
        id: u64,
        result: Value,
    },
    Error {
        id: u64,
        error: PluginError,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginError {
    code: i64,
    message: String,
}

/// A notification sent from the plugin to the agent (no id).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginNotification {
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

// ---------------------------------------------------------------------------
// Params types for each method
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeliverParams {
    resource_identifier: String,
    content: String,
    msg_type: String,
    #[serde(default)]
    msg_subtype: Option<String>,
    #[serde(default)]
    thread_id: i64,
    #[serde(default)]
    cause_external_id: Option<String>,
    #[serde(default)]
    is_summary: bool,
    #[serde(default)]
    is_user_thread: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EditParams {
    resource_identifier: String,
    external_id: String,
    content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeleteParams {
    resource_identifier: String,
    external_id: String,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("Telegram platform plugin starting");

    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN")
        .context("TELEGRAM_BOT_TOKEN environment variable is required")?;

    let polling_enabled = std::env::var("TELEGRAM_POLLING_ENABLED")
        .unwrap_or_default()
        .to_lowercase()
        == "true";

    let client = TelegramBotClient::new(&bot_token);

    // Set up stdin/stdout for the protocol
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    let stdout = tokio::io::stdout();
    let mut writer = tokio::io::BufWriter::new(stdout);

    // ── Inbound polling (optional) ─────────────────────────────────────
    let poll_handle = if polling_enabled {
        let client = TelegramBotClient::new(&bot_token);
        let mut offset: i64 = 0;
        Some(tokio::spawn(async move {
            loop {
                match client.get_updates(&mut offset, 30).await {
                    Ok(updates) => {
                        for update in updates {
                            if let Some(msg) = update.message {
                                if let (Some(text), Some(chat)) = (&msg.text, &msg.chat) {
                                    let chat_id = match &chat.id {
                                        Value::Number(n) => n.to_string(),
                                        Value::String(s) => s.clone(),
                                        _ => continue,
                                    };

                                    // Skip bot's own messages
                                    if msg.from.as_ref().and_then(|f| f.is_bot).unwrap_or(false) {
                                        continue;
                                    }

                                    let notification = PluginNotification {
                                        method: "inbound_message".to_string(),
                                        params: Some(serde_json::json!({
                                            "resource_identifier": chat_id,
                                            "text": text,
                                            "external_id": msg.message_id.to_string(),
                                            "metadata": {},
                                        })),
                                    };

                                    let line = serde_json::to_string(&notification)
                                        .unwrap_or_default();
                                    let mut out = tokio::io::stdout();
                                    let _ = out.write_all(line.as_bytes()).await;
                                    let _ = out.write_all(b"\n").await;
                                    let _ = out.flush().await;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Polling error: {:?}", e);
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        }))
    } else {
        None
    };

    // ── Main request-response loop ─────────────────────────────────────
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                tracing::info!("stdin closed, shutting down");
                break;
            }
            Err(e) => {
                tracing::error!("Error reading stdin: {:?}", e);
                break;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        let request: PluginRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Failed to parse request: {:?}", e);
                continue;
            }
        };

        let req_id = match request.id {
            Some(id) => id,
            None => {
                // Notification from agent — not expected for this plugin
                tracing::debug!("Ignoring notification from agent: {}", request.method);
                continue;
            }
        };

        tracing::debug!("Received request: method={}, id={}", request.method, req_id);

        let response = match request.method.as_str() {
            "initialize" => handle_initialize(req_id, &client).await,
            "deliver" => {
                if let Some(params) = request.params {
                    match serde_json::from_value::<DeliverParams>(params) {
                        Ok(p) => handle_deliver(req_id, &client, &p).await,
                        Err(e) => make_error(req_id, -1, &format!("Invalid deliver params: {}", e)),
                    }
                } else {
                    make_error(req_id, -1, "Missing params for deliver")
                }
            }
            "edit_message" => {
                if let Some(params) = request.params {
                    match serde_json::from_value::<EditParams>(params) {
                        Ok(p) => handle_edit(req_id, &client, &p).await,
                        Err(e) => make_error(req_id, -1, &format!("Invalid edit params: {}", e)),
                    }
                } else {
                    make_error(req_id, -1, "Missing params for edit_message")
                }
            }
            "delete_message" => {
                if let Some(params) = request.params {
                    match serde_json::from_value::<DeleteParams>(params) {
                        Ok(p) => handle_delete(req_id, &client, &p).await,
                        Err(e) => make_error(req_id, -1, &format!("Invalid delete params: {}", e)),
                    }
                } else {
                    make_error(req_id, -1, "Missing params for delete_message")
                }
            }
            _ => make_error(req_id, -1, &format!("Unknown method: {}", request.method)),
        };

        let response_line = serde_json::to_string(&response).unwrap_or_default();
        writer.write_all(response_line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    // Cleanup
    if let Some(handle) = poll_handle {
        handle.abort();
    }

    tracing::info!("Telegram platform plugin shutting down");
    Ok(())
}

// ---------------------------------------------------------------------------
// Handler Functions
// ---------------------------------------------------------------------------

async fn handle_initialize(id: u64, _client: &TelegramBotClient) -> PluginResponse {
    let result = serde_json::json!({
        "name": "telegram",
        "capabilities": {
            "inbound": true,
            "outbound": true,
        }
    });
    make_success(id, result)
}

async fn handle_deliver(id: u64, client: &TelegramBotClient, params: &DeliverParams) -> PluginResponse {
    let chat_id = &params.resource_identifier;
    let content = &params.content;

    match params.msg_type.as_str() {
        // Tool/plan/reasoning messages → send as silent notification
        "tool" | "plan" | "reasoning" => {
            let prefix = match params.msg_type.as_str() {
                "tool" => "🔧",
                "plan" => "📋",
                "reasoning" => "💭",
                _ => "",
            };
            let display = if let Some(subtype) = &params.msg_subtype {
                format!("{} {}: {}", prefix, subtype, content)
            } else {
                format!("{} {}", prefix, content)
            };

            match client.send_message(chat_id, &display, Some("HTML"), None, true).await {
                Ok(msg_id) => make_success(id, serde_json::json!({
                    "delivered": true,
                    "external_id": msg_id.to_string(),
                })),
                Err(e) => make_error(id, -1, &format!("Failed to send tool message: {}", e)),
            }
        }

        // Summary: reply to the cause message if cause_external_id exists
        "summary" => {
            let reply_to = params.cause_external_id.as_ref()
                .and_then(|eid| eid.parse::<i64>().ok());

            match client.send_message(chat_id, content, Some("HTML"), reply_to, false).await {
                Ok(msg_id) => {
                    // If there was a progress indicator, try to delete it
                    // Progress tracking would be handled internally
                    make_success(id, serde_json::json!({
                        "delivered": true,
                        "external_id": msg_id.to_string(),
                    }))
                }
                Err(e) => make_error(id, -1, &format!("Failed to send summary: {}", e)),
            }
        }

        // Regular messages
        "message" | "error" => {
            let parse_mode = if params.is_user_thread { Some("HTML") } else { None };

            match client.send_message(chat_id, content, parse_mode, None, false).await {
                Ok(msg_id) => make_success(id, serde_json::json!({
                    "delivered": true,
                    "external_id": msg_id.to_string(),
                })),
                Err(e) => make_error(id, -1, &format!("Failed to send message: {}", e)),
            }
        }

        // Notification → send as silent message
        "notification" => {
            match client.send_message(chat_id, content, Some("HTML"), None, true).await {
                Ok(msg_id) => make_success(id, serde_json::json!({
                    "delivered": true,
                    "external_id": msg_id.to_string(),
                })),
                Err(e) => make_error(id, -1, &format!("Failed to send notification: {}", e)),
            }
        }

        // Fallback for unknown types
        _ => {
            match client.send_message(chat_id, content, Some("HTML"), None, false).await {
                Ok(msg_id) => make_success(id, serde_json::json!({
                    "delivered": true,
                    "external_id": msg_id.to_string(),
                })),
                Err(e) => make_error(id, -1, &format!("Failed to send message: {}", e)),
            }
        }
    }
}

async fn handle_edit(id: u64, client: &TelegramBotClient, params: &EditParams) -> PluginResponse {
    let chat_id = &params.resource_identifier;
    let external_id = match params.external_id.parse::<i64>() {
        Ok(id) => id,
        Err(_) => {
            return make_error(id, -1, &format!("Invalid external_id: {}", params.external_id));
        }
    };

    match client.edit_message_text(chat_id, external_id, &params.content, Some("HTML")).await {
        Ok(_) => make_success(id, serde_json::json!({"edited": true})),
        Err(e) => make_error(id, -1, &format!("Failed to edit message: {}", e)),
    }
}

async fn handle_delete(id: u64, client: &TelegramBotClient, params: &DeleteParams) -> PluginResponse {
    let chat_id = &params.resource_identifier;
    let external_id = match params.external_id.parse::<i64>() {
        Ok(id) => id,
        Err(_) => {
            return make_error(id, -1, &format!("Invalid external_id: {}", params.external_id));
        }
    };

    match client.delete_message(chat_id, external_id).await {
        Ok(_) => make_success(id, serde_json::json!({"deleted": true})),
        Err(e) => make_error(id, -1, &format!("Failed to delete message: {}", e)),
    }
}

// ---------------------------------------------------------------------------
// Response builders
// ---------------------------------------------------------------------------

fn make_success(id: u64, result: Value) -> PluginResponse {
    PluginResponse::Success { id, result }
}

fn make_error(id: u64, code: i64, message: &str) -> PluginResponse {
    PluginResponse::Error {
        id,
        error: PluginError {
            code,
            message: message.to_string(),
        },
    }
}
