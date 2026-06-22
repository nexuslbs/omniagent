//! Mattermost Platform Plugin for OmniAgent.
//!
//! Standalone binary that implements the platform plugin protocol over stdio.
//! Communicates with the Mattermost REST API to send, edit, and delete messages.
//!
//! Protocol: JSON-lines over stdin/stdout.
//!
//! Methods:
//!   - initialize:      Return plugin info and capabilities
//!   - deliver:         Send a message to a Mattermost channel
//!   - edit_message:    Edit an existing Mattermost post
//!   - delete_message:  Delete a Mattermost post
//!
//! Inbound (when MATTERMOST_POLLING_ENABLED=true):
//!   Polls channels for new posts and sends inbound_message notifications to stdout.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------------
// Mattermost REST API Client
// ---------------------------------------------------------------------------

struct MattermostClient {
    http_client: reqwest::Client,
    api_base: String,
    auth_header: String,
}

impl MattermostClient {
    fn new(server_url: &str, access_token: &str) -> Self {
        let api_base = server_url.trim_end_matches('/').to_string();
        Self {
            http_client: reqwest::Client::new(),
            api_base,
            auth_header: format!("Bearer {}", access_token),
        }
    }

    /// Create a new post in a channel.
    /// If `root_id` is Some, the post is a reply in that thread.
    async fn create_post(
        &self,
        channel_id: &str,
        message: &str,
        root_id: Option<&str>,
    ) -> Result<String> {
        let mut body = serde_json::json!({
            "channel_id": channel_id,
            "message": message,
        });
        if let Some(rid) = root_id {
            body["root_id"] = serde_json::json!(rid);
        }

        let resp = self
            .http_client
            .post(format!("{}/api/v4/posts", self.api_base))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;

        Self::extract_post_id(resp, "createPost").await
    }

    /// Update an existing post (replace message text).
    async fn update_post(&self, post_id: &str, message: &str) -> Result<String> {
        let body = serde_json::json!({
            "id": post_id,
            "message": message,
        });

        let resp = self
            .http_client
            .put(format!("{}/api/v4/posts/{}", self.api_base, post_id))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;

        Self::extract_post_id(resp, "updatePost").await
    }

    /// Delete a post.
    async fn delete_post(&self, post_id: &str) -> Result<bool> {
        let resp = self
            .http_client
            .delete(format!("{}/api/v4/posts/{}", self.api_base, post_id))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() {
            Ok(true)
        } else if status.as_u16() == 404 {
            Ok(false) // already deleted
        } else {
            let text = resp.text().await.unwrap_or_default();
            Err(anyhow::anyhow!(
                "Mattermost deletePost failed ({}): {}",
                status,
                text
            ))
        }
    }

    /// Get the authenticated user (to verify token and get bot ID).
    async fn get_me(&self) -> Result<MattermostUser> {
        let resp = self
            .http_client
            .get(format!("{}/api/v4/users/me", self.api_base))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse getMe response")?;

        if !status.is_success() {
            let msg = body["message"].as_str().unwrap_or("unknown error");
            return Err(anyhow::anyhow!("Mattermost getMe failed ({}): {}", status, msg));
        }

        let user: MattermostUser = serde_json::from_value(body)
            .context("Failed to parse Mattermost user")?;

        Ok(user)
    }

    /// Get a user by ID (for bot detection).
    async fn get_user(&self, user_id: &str) -> Result<MattermostUser> {
        let resp = self
            .http_client
            .get(format!("{}/api/v4/users/{}", self.api_base, user_id))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse getUser response")?;

        if !status.is_success() {
            let msg = body["message"].as_str().unwrap_or("unknown error");
            return Err(anyhow::anyhow!(
                "Mattermost getUser failed for {} ({}): {}",
                user_id,
                status,
                msg
            ));
        }

        let user: MattermostUser = serde_json::from_value(body)
            .context("Failed to parse Mattermost user")?;

        Ok(user)
    }

    /// Get teams a user is a member of.
    async fn get_teams(&self, user_id: &str) -> Result<Vec<MattermostTeam>> {
        let resp = self
            .http_client
            .get(format!(
                "{}/api/v4/users/{}/teams",
                self.api_base, user_id
            ))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse getTeams response")?;

        if !status.is_success() {
            let msg = body["message"].as_str().unwrap_or("unknown error");
            return Err(anyhow::anyhow!(
                "Mattermost getTeams failed ({}): {}",
                status,
                msg
            ));
        }

        let teams: Vec<MattermostTeam> = serde_json::from_value(body)
            .context("Failed to parse Mattermost teams")?;

        Ok(teams)
    }

    /// Get channels a user is a member of in a specific team.
    async fn get_user_channels(&self, user_id: &str, team_id: &str) -> Result<Vec<MattermostChannel>> {
        let resp = self
            .http_client
            .get(format!(
                "{}/api/v4/users/{}/teams/{}/channels",
                self.api_base, user_id, team_id
            ))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse getUserChannels response")?;

        if !status.is_success() {
            let msg = body["message"].as_str().unwrap_or("unknown error");
            return Err(anyhow::anyhow!(
                "Mattermost getUserChannels failed ({}): {}",
                status,
                msg
            ));
        }

        let channels: Vec<MattermostChannel> = serde_json::from_value(body)
            .context("Failed to parse Mattermost channels")?;

        Ok(channels)
    }

    /// Get posts for a channel, ordered by create_at descending.
    /// Returns up to `per_page` posts.
    async fn get_channel_posts(
        &self,
        channel_id: &str,
        page: u32,
        per_page: u32,
    ) -> Result<Vec<MattermostPost>> {
        let resp = self
            .http_client
            .get(format!(
                "{}/api/v4/channels/{}/posts?page={}&per_page={}",
                self.api_base, channel_id, page, per_page
            ))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse channel posts response")?;

        if !status.is_success() {
            let msg = body["message"].as_str().unwrap_or("unknown error");
            return Err(anyhow::anyhow!(
                "Mattermost getChannelPosts failed ({}): {}",
                status,
                msg
            ));
        }

        // The response has "order" (ordered post IDs) and "posts" (map of id->post)
        let order = body["order"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let posts_map = body["posts"]
            .as_object()
            .cloned()
            .unwrap_or_default();

        let mut posts: Vec<MattermostPost> = Vec::with_capacity(order.len());
        for id in &order {
            if let Some(post_val) = posts_map.get(id) {
                if let Ok(post) = serde_json::from_value::<MattermostPost>(post_val.clone()) {
                    posts.push(post);
                }
            }
        }

        Ok(posts)
    }

    /// Extract the post ID from a Mattermost API response.
    async fn extract_post_id(resp: reqwest::Response, context: &str) -> Result<String> {
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context(format!("Failed to parse {} response JSON", context))?;

        if !status.is_success() {
            let msg = body["message"].as_str().unwrap_or("unknown error");
            let detail = body["detailed_error"].as_str().unwrap_or("");
            let full_msg = if detail.is_empty() {
                format!("Mattermost {} error ({}): {}", context, status, msg)
            } else {
                format!(
                    "Mattermost {} error ({}): {} - {}",
                    context, status, msg, detail
                )
            };
            return Err(anyhow::anyhow!(full_msg));
        }

        body["id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Mattermost {} response missing post id", context))
    }
}

// ---------------------------------------------------------------------------
// Mattermost API types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MattermostUser {
    id: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    is_bot: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MattermostTeam {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MattermostChannel {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    display_name: String,
    #[serde(rename = "type")]
    #[serde(default)]
    channel_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MattermostPost {
    id: String,
    #[serde(default)]
    channel_id: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    root_id: String,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    create_at: i64,
    #[serde(default)]
    delete_at: i64,
    #[serde(default)]
    props: Value,
}

// ---------------------------------------------------------------------------
// Plugin Protocol Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum PluginResponse {
    Success { id: u64, result: Value },
    Error { id: u64, error: PluginError },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginError {
    code: i64,
    message: String,
}

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Mattermost platform plugin starting");

    let server_url = std::env::var("MATTERMOST_SERVER_URL")
        .context("MATTERMOST_SERVER_URL environment variable is required")?;

    let access_token = std::env::var("MATTERMOST_ACCESS_TOKEN")
        .context("MATTERMOST_ACCESS_TOKEN environment variable is required")?;

    let polling_enabled = std::env::var("MATTERMOST_POLLING_ENABLED")
        .unwrap_or_default()
        .to_lowercase()
        == "true";

    let connection_mode = std::env::var("MATTERMOST_CONNECTION_MODE")
        .unwrap_or_default()
        .to_lowercase();

    let polling_interval_secs: u64 = std::env::var("MATTERMOST_POLLING_INTERVAL")
        .unwrap_or_else(|_| "15".to_string())
        .parse()
        .unwrap_or(15);

    let _bot_username = std::env::var("MATTERMOST_BOT_USERNAME")
        .unwrap_or_else(|_| "omniagent".to_string());

    // Optional manual channel overrides (merged with auto-discovered channels)
    let manual_channel_ids: Vec<String> = std::env::var("MATTERMOST_CHANNEL_IDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let client = MattermostClient::new(&server_url, &access_token);

    // Verify token by fetching bot user info
    let bot_user = match client.get_me().await {
        Ok(u) => {
            tracing::info!(
                "Authenticated as Mattermost user: {} ({})",
                u.username,
                u.id
            );
            u
        }
        Err(e) => {
            tracing::error!("Failed to authenticate with Mattermost: {:?}", e);
            return Err(e);
        }
    };

    // Auto-discover channels the bot is a member of
    let initial_channels = discover_channels(&client, &bot_user.id).await;
    let mut channel_ids: Vec<String> = initial_channels;
    // Merge manual overrides
    for ch_id in &manual_channel_ids {
        if !channel_ids.contains(ch_id) {
            channel_ids.push(ch_id.clone());
        }
    }

    if !channel_ids.is_empty() {
        tracing::info!(
            "Watching {} channel(s): {}",
            channel_ids.len(),
            channel_ids.join(", ")
        );
    }

    // Stdin/stdout for the protocol
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    let stdout = tokio::io::stdout();
    let mut writer = tokio::io::BufWriter::new(stdout);

    // ── Inbound (polling or WebSocket) ──────────────────────────────────
    let use_websocket = connection_mode == "websocket";
    let inbound_handle: Option<tokio::task::JoinHandle<()>> = if use_websocket {
        let bot_id = bot_user.id.clone();
        Some(tokio::spawn(async move {
            ws_event_loop(
                server_url,
                access_token,
                channel_ids,
                bot_id,
            ).await;
        }))
    } else if polling_enabled {
        let client = MattermostClient::new(&server_url, &access_token);
        let bot_id = bot_user.id.clone();

        // Manual override channels for periodic re-merge
        let manual_ids = manual_channel_ids;

        Some(tokio::spawn(async move {
            // Shared state: the channel list, periodically refreshed
            let mut current_ids: Vec<String> = channel_ids;
            let mut last_discovery: Vec<String> = current_ids.clone();

            // Cursor tracking: oldest create_at seen per channel
            let mut last_create_at: HashMap<String, i64> = HashMap::new();
            let mut bot_cache: HashMap<String, bool> = HashMap::new();
            bot_cache.insert(bot_id.clone(), true);

            // Initialize cursors for all channels
            for ch_id in &current_ids {
                init_channel_cursor(&client, ch_id, &mut last_create_at).await;
            }

            let mut refresh_counter: u64 = 0;
            let refresh_interval: u64 = 4; // refresh discovery every N poll cycles

            loop {
                tokio::time::sleep(std::time::Duration::from_secs(polling_interval_secs)).await;

                // Periodically refresh channel discovery (every N cycles)
                refresh_counter += 1;
                if refresh_counter >= refresh_interval {
                    refresh_counter = 0;

                    // Discover channels the bot is currently a member of
                    let discovered = discover_channels(&client, &bot_id).await;

                    // Merge with manual overrides
                    let mut merged = discovered.clone();
                    for ch_id in &manual_ids {
                        if !merged.contains(ch_id) {
                            merged.push(ch_id.clone());
                        }
                    }

                    // Detect new channels since last refresh
                    for ch_id in &merged {
                        if !last_discovery.contains(ch_id) {
                            tracing::info!(
                                "Discovered new channel {}, initializing cursor",
                                ch_id
                            );
                            if !last_create_at.contains_key(ch_id.as_str()) {
                                init_channel_cursor(&client, ch_id, &mut last_create_at).await;
                            }
                        }
                    }

                    // Detect removed channels
                    for ch_id in &last_discovery {
                        if !merged.contains(ch_id) && !manual_ids.contains(ch_id) {
                            tracing::info!("Channel {} no longer accessible, removing", ch_id);
                            last_create_at.remove(ch_id.as_str());
                        }
                    }

                    current_ids = merged;
                    last_discovery = current_ids.clone();
                }

                // Poll all known channels
                for ch_id in &current_ids {
                    let count = poll_channel(
                        &client, ch_id, &bot_id,
                        &mut last_create_at, &mut bot_cache,
                    ).await;
                    if count > 0 {
                        tracing::debug!(
                            "Polling: processed {} new post(s) in channel {}",
                            count, ch_id
                        );
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
                tracing::debug!("Ignoring notification from agent: {}", request.method);
                continue;
            }
        };

        tracing::debug!(
            "Received request: method={}, id={}",
            request.method,
            req_id
        );

        let response = match request.method.as_str() {
            "initialize" => handle_initialize(req_id).await,
            "deliver" => {
                if let Some(params) = request.params {
                    match serde_json::from_value::<DeliverParams>(params) {
                        Ok(p) => handle_deliver(req_id, &client, &p).await,
                        Err(e) => {
                            make_error(req_id, -1, &format!("Invalid deliver params: {}", e))
                        }
                    }
                } else {
                    make_error(req_id, -1, "Missing params for deliver")
                }
            }
            "edit_message" => {
                if let Some(params) = request.params {
                    match serde_json::from_value::<EditParams>(params) {
                        Ok(p) => handle_edit(req_id, &client, &p).await,
                        Err(e) => {
                            make_error(req_id, -1, &format!("Invalid edit params: {}", e))
                        }
                    }
                } else {
                    make_error(req_id, -1, "Missing params for edit_message")
                }
            }
            "delete_message" => {
                if let Some(params) = request.params {
                    match serde_json::from_value::<DeleteParams>(params) {
                        Ok(p) => handle_delete(req_id, &client, &p).await,
                        Err(e) => {
                            make_error(req_id, -1, &format!("Invalid delete params: {}", e))
                        }
                    }
                } else {
                    make_error(req_id, -1, "Missing params for delete_message")
                }
            }
            _ => make_error(
                req_id,
                -1,
                &format!("Unknown method: {}", request.method),
            ),
        };

        let response_line = serde_json::to_string(&response).unwrap_or_default();
        writer.write_all(response_line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    // Cleanup
    if let Some(handle) = inbound_handle {
        handle.abort();
    }

    tracing::info!("Mattermost platform plugin shutting down");
    Ok(())
}

// ---------------------------------------------------------------------------
// Handler Functions
// ---------------------------------------------------------------------------

async fn handle_initialize(id: u64) -> PluginResponse {
    let result = serde_json::json!({
        "name": "mattermost",
        "capabilities": {
            "inbound": true,
            "outbound": true,
        }
    });
    make_success(id, result)
}

async fn handle_deliver(
    id: u64,
    client: &MattermostClient,
    params: &DeliverParams,
) -> PluginResponse {
    let channel_id = &params.resource_identifier;
    let content = &params.content;

    // Determine if this is a threaded reply.
    // Mattermost uses root_id for threading. We use cause_external_id as the
    // parent post ID. Non-summary messages are top-level (no thread context
    // unless cause_external_id is set).
    let root_id = if params.is_summary {
        // Summary messages reply to the user's original post
        params.cause_external_id.as_deref()
    } else if params.msg_type == "message"
        || params.msg_type == "error"
        || params.msg_type == "notification"
    {
        params.cause_external_id.as_deref()
    } else {
        None
    };

    match params.msg_type.as_str() {
        "tool" | "plan" | "reasoning" => {
            let prefix = match params.msg_type.as_str() {
                "tool" => ":wrench:",
                "plan" => ":clipboard:",
                "reasoning" => ":thought_balloon:",
                _ => "",
            };
            let display = if let Some(subtype) = &params.msg_subtype {
                format!("{} {}: {}", prefix, subtype, content)
            } else {
                format!("{} {}", prefix, content)
            };

            match client.create_post(channel_id, &display, None).await {
                Ok(post_id) => make_success(
                    id,
                    serde_json::json!({
                        "delivered": true,
                        "external_id": post_id,
                    }),
                ),
                Err(e) => {
                    make_error(id, -1, &format!("Failed to send tool message: {}", e))
                }
            }
        }

        "summary" => {
            match client
                .create_post(channel_id, content, root_id)
                .await
            {
                Ok(post_id) => make_success(
                    id,
                    serde_json::json!({
                        "delivered": true,
                        "external_id": post_id,
                    }),
                ),
                Err(e) => {
                    make_error(id, -1, &format!("Failed to send summary: {}", e))
                }
            }
        }

        "message" | "error" | "notification" => {
            match client
                .create_post(channel_id, content, root_id)
                .await
            {
                Ok(post_id) => make_success(
                    id,
                    serde_json::json!({
                        "delivered": true,
                        "external_id": post_id,
                    }),
                ),
                Err(e) => {
                    make_error(id, -1, &format!("Failed to send message: {}", e))
                }
            }
        }

        _ => {
            match client.create_post(channel_id, content, None).await {
                Ok(post_id) => make_success(
                    id,
                    serde_json::json!({
                        "delivered": true,
                        "external_id": post_id,
                    }),
                ),
                Err(e) => {
                    make_error(id, -1, &format!("Failed to send message: {}", e))
                }
            }
        }
    }
}

async fn handle_edit(
    id: u64,
    client: &MattermostClient,
    params: &EditParams,
) -> PluginResponse {
    match client.update_post(&params.external_id, &params.content).await {
        Ok(_) => make_success(id, serde_json::json!({"edited": true})),
        Err(e) => make_error(id, -1, &format!("Failed to edit message: {}", e)),
    }
}

async fn handle_delete(
    id: u64,
    client: &MattermostClient,
    params: &DeleteParams,
) -> PluginResponse {
    match client.delete_post(&params.external_id).await {
        Ok(_) => make_success(id, serde_json::json!({"deleted": true})),
        Err(e) => make_error(id, -1, &format!("Failed to delete message: {}", e)),
    }
}

// ---------------------------------------------------------------------------
// Shared inbound helpers (used by both polling and WebSocket)
// ---------------------------------------------------------------------------

/// Check if a user is a bot via the Mattermost API, using a cache.
async fn is_bot_user(
    client: &MattermostClient,
    cache: &mut HashMap<String, bool>,
    user_id: &str,
) -> bool {
    if let Some(&is_bot) = cache.get(user_id) {
        return is_bot;
    }
    match client.get_user(user_id).await {
        Ok(user) => {
            tracing::debug!("Cached user {} as is_bot={}", user_id, user.is_bot);
            cache.insert(user_id.to_string(), user.is_bot);
            user.is_bot
        }
        Err(e) => {
            tracing::warn!(
                "Failed to check if user {} is a bot: {:?}. Skipping post to be safe.",
                user_id,
                e
            );
            true // fail-safe: skip on API error
        }
    }
}

/// Send an inbound_message notification to stdout (shared by polling and WS).
async fn send_inbound_notification(post: &MattermostPost, ch_id: &str) {
    let root_id = if post.root_id.is_empty() {
        None
    } else {
        Some(post.root_id.as_str())
    };

    let thread_id = root_id.unwrap_or(&post.id);

    tracing::info!(
        "Inbound message from channel {}: {}",
        ch_id,
        post.message.chars().take(50).collect::<String>()
    );

    let notification = PluginNotification {
        method: "inbound_message".to_string(),
        params: Some(serde_json::json!({
            "resource_identifier": ch_id,
            "text": post.message,
            "external_id": post.id,
            "metadata": {
                "root_id": root_id,
                "thread_id": thread_id,
                "user_id": post.user_id,
                "channel_id": ch_id,
            },
        })),
    };

    let line = serde_json::to_string(&notification).unwrap_or_default();
    let mut out = tokio::io::stdout();
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.write_all(b"\n").await;
    let _ = out.flush().await;
}

/// Fetch and process new posts for a single channel since the last known cursor.
/// Returns the number of new posts processed.
async fn poll_channel(
    client: &MattermostClient,
    ch_id: &str,
    bot_id: &str,
    last_create_at: &mut HashMap<String, i64>,
    bot_cache: &mut HashMap<String, bool>,
) -> u32 {
    let mut count = 0u32;
    let last_ts = last_create_at.get(ch_id).copied().unwrap_or(0);

    match client.get_channel_posts(ch_id, 0, 60).await {
        Ok(posts) => {
            let mut newest_ts = last_ts;

            // Posts are newest-first from the API. We iterate in reverse
            // so we process oldest to newest (create_at is monotonic in rev()).
            for post in posts.iter().rev() {
                // Skip already-seen posts
                if post.create_at <= last_ts {
                    continue;
                }

                if post.create_at > newest_ts {
                    newest_ts = post.create_at;
                }

                // Skip deleted posts
                if post.delete_at != 0 {
                    continue;
                }

                // Skip bot's own posts
                if post.user_id == *bot_id {
                    continue;
                }

                // Verify this isn't a bot user via API check
                if is_bot_user(client, bot_cache, &post.user_id).await {
                    continue;
                }

                // This is a new post from a human user
                send_inbound_notification(&post, ch_id).await;
                count += 1;
            }

            // Advance cursor to the newest post on the server
            if newest_ts > last_ts {
                last_create_at.insert(ch_id.to_string(), newest_ts);
            }
        }
        Err(e) => {
            tracing::error!("poll_channel error for channel {}: {:?}", ch_id, e);
        }
    }

    count
}

/// Initialize the cursor for a single channel: store the latest post timestamp.
async fn init_channel_cursor(
    client: &MattermostClient,
    ch_id: &str,
    last_create_at: &mut HashMap<String, i64>,
) {
    match client.get_channel_posts(ch_id, 0, 1).await {
        Ok(posts) => {
            if let Some(latest) = posts.first() {
                last_create_at.insert(ch_id.to_string(), latest.create_at);
            }
        }
        Err(e) => {
            tracing::error!(
                "Failed to init cursor for channel {}: {:?}",
                ch_id,
                e
            );
        }
    }
}

/// Auto-discover all channels the bot is a member of across all teams.
async fn discover_channels(client: &MattermostClient, bot_id: &str) -> Vec<String> {
    let mut channel_ids: Vec<String> = Vec::new();

    let teams = match client.get_teams(bot_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Failed to discover teams: {:?}", e);
            return channel_ids;
        }
    };

    for team in &teams {
        match client.get_user_channels(bot_id, &team.id).await {
            Ok(channels) => {
                for ch in &channels {
                    channel_ids.push(ch.id.clone());
                }
            }
            Err(e) => {
                tracing::error!(
                    "Failed to discover channels for team {} ({}): {:?}",
                    team.display_name,
                    team.id,
                    e
                );
            }
        }
    }

    channel_ids.sort();
    channel_ids.dedup();
    channel_ids
}

/// Build the WebSocket URL from the HTTP(S) server URL.
fn ws_api_url(server_url: &str) -> String {
    let base = server_url.trim_end_matches('/');
    base.replacen("http", "ws", 1).to_string() + "/api/v4/websocket"
}

// ---------------------------------------------------------------------------
// Per-channel debounce state for WebSocket event processing
// ---------------------------------------------------------------------------

/// Tracks whether a channel is currently being polled and whether a
/// re-poll is needed after the current one finishes. Multiple rapid WS
/// events coalesce into a single pending flag — the cursor-based catch-up
/// will find everything when it eventually runs.
struct ChannelDebounce {
    is_processing: bool,
    pending: bool,
}

/// Process a channel event with debounce: if a poll is already in-flight
/// for this channel, just mark `pending` (coalesces N events into 1).
/// After the current poll finishes, if `pending` is true, wait 5s then
/// re-poll (catches up via cursor), looping until the channel goes quiet.
async fn process_channel_event(
    client: &MattermostClient,
    ch_id: &str,
    bot_id: &str,
    last_create_at: &mut HashMap<String, i64>,
    bot_cache: &mut HashMap<String, bool>,
    debounce: &mut HashMap<String, ChannelDebounce>,
) {
    let state = debounce
        .entry(ch_id.to_string())
        .or_insert(ChannelDebounce {
            is_processing: false,
            pending: false,
        });

    if state.is_processing {
        state.pending = true;
        return;
    }

    state.is_processing = true;

    loop {
        state.pending = false;

        let count = poll_channel(client, ch_id, bot_id, last_create_at, bot_cache).await;
        if count > 0 {
            tracing::info!("WS event: processed {} new post(s) in channel {}", count, ch_id);
        }

        if state.pending {
            // More WS events arrived while we were polling. Wait briefly
            // (allows more events to coalesce), then re-poll via cursor
            // to catch up everything at once.
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        break;
    }

    state.is_processing = false;
}

// ---------------------------------------------------------------------------
// WebSocket Event Loop (event-driven trigger for poll_channel)
// ---------------------------------------------------------------------------

/// Connect to the Mattermost WebSocket and listen for `posted` events.
/// On connect, does a full catch-up for all channels. On `posted` event,
/// triggers poll_channel for that channel (cursor-based, same as polling).
/// Reconnects with exponential backoff.
async fn ws_event_loop(
    server_url: String,
    access_token: String,
    channel_ids: Vec<String>,
    bot_id: String,
) {
    let mut last_create_at: HashMap<String, i64> = HashMap::new();
    let mut bot_cache: HashMap<String, bool> = HashMap::new();
    bot_cache.insert(bot_id.clone(), true);

    let watch_all = channel_ids.is_empty();
    let channel_set: std::collections::HashSet<String> =
        channel_ids.into_iter().collect();

    let mut backoff = 1u64;
    let mut debounce: HashMap<String, ChannelDebounce> = HashMap::new();

    loop {
        let url = ws_api_url(&server_url);
        tracing::info!("Connecting to Mattermost WebSocket: {}", url);

        match connect_async(&url).await {
            Ok((ws_stream, _response)) => {
                tracing::info!("WebSocket connected, authenticating...");
                backoff = 1;

                // ── Catch-up on connect: process any missed messages ──
                let client = MattermostClient::new(&server_url, &access_token);
                // Initialize cursors for channels that don't have one yet
                // (first connect) or need catch-up (reconnect)
                let channels: Vec<String> = if watch_all {
                    // In watch_all mode with WS, we don't know what channels exist
                    // until we receive events. Cursors are initialized lazily.
                    vec![]
                } else {
                    channel_set.iter().cloned().collect()
                };
                for ch_id in &channels {
                    if !last_create_at.contains_key(ch_id.as_str()) {
                        init_channel_cursor(&client, ch_id, &mut last_create_at).await;
                    }
                }
                // Do a full poll for all known channels (catches missed messages)
                for ch_id in &channels {
                    let count = poll_channel(
                        &client, ch_id, &bot_id,
                        &mut last_create_at, &mut bot_cache,
                    ).await;
                    if count > 0 {
                        tracing::info!(
                            "WS catch-up: processed {} new post(s) in channel {}",
                            count, ch_id
                        );
                    }
                }

                let (mut write, mut read) = ws_stream.split();

                // Send authentication challenge
                let auth_msg = serde_json::json!({
                    "seq": 1,
                    "action": "authentication_challenge",
                    "data": { "token": access_token }
                });
                let auth_text = serde_json::to_string(&auth_msg).unwrap_or_default();
                if let Err(e) = write.send(Message::Text(auth_text.into())).await {
                    tracing::error!("Failed to send WS auth: {:?}", e);
                    continue;
                }

                // Event loop — WS events just trigger poll_channel for that channel
                loop {
                    match read.next().await {
                        Some(Ok(Message::Text(text))) => {
                            let event: Value = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };

                            let event_type = match event.get("event").and_then(|e| e.as_str()) {
                                Some(t) => t,
                                None => continue,
                            };

                            match event_type {
                                "hello" => {
                                    tracing::info!("WebSocket authenticated successfully");
                                }
                                "posted" => {
                                    // Extract channel_id from the event to trigger a poll
                                    let ch_id = match event
                                        .pointer("/data/channel_id")
                                        .and_then(|v| v.as_str())
                                    {
                                        Some(c) => c.to_string(),
                                        None => {
                                            tracing::debug!("posted event missing channel_id");
                                            continue;
                                        }
                                    };

                                    // Check if we should process this channel
                                    if !watch_all && !channel_set.contains(&ch_id) {
                                        continue;
                                    }

                                    // Initialize cursor lazily for new channels
                                    if !last_create_at.contains_key(&ch_id) {
                                        init_channel_cursor(&client, &ch_id, &mut last_create_at).await;
                                    }

                                    // Trigger cursor-based processing for this channel
                                    // with debounce: if already processing, coalesces into
                                    // a single re-poll after current run + 5s wait.
                                    process_channel_event(
                                        &client, &ch_id, &bot_id,
                                        &mut last_create_at, &mut bot_cache,
                                        &mut debounce,
                                    ).await;
                                }
                                _ => {
                                    // Ignore other event types
                                }
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            let _ = write.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(frame))) => {
                            tracing::warn!(
                                "WebSocket closed by server: {:?}",
                                frame.map(|f| f.reason.to_string())
                            );
                            break;
                        }
                        Some(Err(e)) => {
                            tracing::error!("WebSocket error: {:?}", e);
                            break;
                        }
                        None => {
                            tracing::warn!("WebSocket stream ended");
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to connect WebSocket: {:?}", e);
            }
        }

        let delay = Duration::from_secs(backoff.min(60));
        tracing::info!("Reconnecting WebSocket in {}s...", delay.as_secs());
        tokio::time::sleep(delay).await;
        backoff = backoff.saturating_mul(2);
    }
}

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
