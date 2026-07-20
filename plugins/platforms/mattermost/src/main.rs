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
//! Inbound (via WebSocket or polling):
//!   Polls channels for new posts and sends inbound_message notifications to stdout.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
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

    /// Create a client from a session token (from login).
    #[allow(dead_code)]
    fn from_session(server_url: &str, session_token: &str) -> Self {
        let api_base = server_url.trim_end_matches('/').to_string();
        Self {
            http_client: reqwest::Client::new(),
            api_base,
            auth_header: format!("Bearer {}", session_token),
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

    /// Send typing indicator (shows "bot is typing..." in the channel/thread).
    async fn send_typing(&self, channel_id: &str, parent_id: Option<&str>) -> Result<bool> {
        let mut body = serde_json::json!({});
        if let Some(pid) = parent_id {
            body["parent_id"] = serde_json::json!(pid);
        }
        let resp = self
            .http_client
            .post(format!(
                "{}/api/v4/channels/{}/typing",
                self.api_base, channel_id
            ))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        Ok(resp.status().is_success())
    }

    /// Add a reaction (emoji) to a post.
    async fn create_reaction(&self, post_id: &str, user_id: &str, emoji: &str) -> Result<bool> {
        let body = serde_json::json!({
            "user_id": user_id,
            "post_id": post_id,
            "emoji_name": emoji,
        });
        let resp = self
            .http_client
            .post(format!("{}/api/v4/reactions", self.api_base))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(true)
        } else {
            let text = resp.text().await.unwrap_or_default();
            Err(anyhow::anyhow!(
                "Mattermost createReaction failed ({}): {}",
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

    /// Get file info (metadata) for a file ID.
    async fn get_file_info(&self, file_id: &str) -> Result<MattermostFileInfo> {
        let resp = self
            .http_client
            .get(format!(
                "{}/api/v4/files/{}/info",
                self.api_base, file_id
            ))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse file info response")?;

        if !status.is_success() {
            let msg = body["message"].as_str().unwrap_or("unknown error");
            return Err(anyhow::anyhow!(
                "Mattermost getFileInfo failed ({}): {}",
                status,
                msg
            ));
        }

        let info: MattermostFileInfo = serde_json::from_value(body)
            .context("Failed to parse Mattermost file info")?;

        Ok(info)
    }

    /// Download the actual content of a file.
    /// Returns the raw bytes: caller should check MIME type and size.
    async fn get_file_content(&self, file_id: &str) -> Result<Vec<u8>> {
        let resp = self
            .http_client
            .get(format!(
                "{}/api/v4/files/{}",
                self.api_base, file_id
            ))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Mattermost getFileContent failed ({}): {}",
                status,
                text
            ));
        }

        let bytes = resp.bytes().await?;
        Ok(bytes.to_vec())
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
// MattermostClient: Setup API methods
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// MattermostClient: Setup API methods
// ---------------------------------------------------------------------------

impl MattermostClient {
    /// Create a new user account.
    async fn create_user(&self, username: &str, password: &str, email: &str) -> Result<Value> {
        let body = serde_json::json!({
            "username": username,
            "password": password,
            "email": email,
        });
        let resp = self
            .http_client
            .post(format!("{}/api/v4/users", self.api_base))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() || status.as_u16() == 409 {
            Ok(serde_json::from_str(&text).unwrap_or(serde_json::json!({})))
        } else {
            Err(anyhow::anyhow!("Mattermost createUser failed ({}): {}", status, text))
        }
    }

    /// Create a new team.
    async fn create_team(&self, name: &str, display_name: &str) -> Result<Value> {
        let body = serde_json::json!({
            "name": name,
            "display_name": display_name,
            "type": "O",
        });
        let resp = self
            .http_client
            .post(format!("{}/api/v4/teams", self.api_base))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() || status.as_u16() == 409 {
            Ok(serde_json::from_str(&text).unwrap_or(serde_json::json!({})))
        } else {
            Err(anyhow::anyhow!("Mattermost createTeam failed ({}): {}", status, text))
        }
    }

    /// Get all teams (for finding existing teams).
    #[allow(dead_code)]
    async fn get_teams_all(&self) -> Result<Vec<Value>> {
        let resp = self
            .http_client
            .get(format!("{}/api/v4/teams", self.api_base))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(serde_json::from_str(&text).unwrap_or_default())
        } else {
            Err(anyhow::anyhow!("Mattermost getTeamsAll failed ({}): {}", status, text))
        }
    }

    /// Get all users.
    async fn get_users_all(&self) -> Result<Vec<Value>> {
        let resp = self
            .http_client
            .get(format!("{}/api/v4/users", self.api_base))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(serde_json::from_str(&text).unwrap_or_default())
        } else {
            Err(anyhow::anyhow!("Mattermost getUsersAll failed ({}): {}", status, text))
        }
    }

    /// Add a user to a team.
    async fn add_team_member(&self, team_id: &str, user_id: &str) -> Result<bool> {
        let body = serde_json::json!({"team_id": team_id, "user_id": user_id});
        let resp = self
            .http_client
            .post(format!("{}/api/v4/teams/{}/members", self.api_base, team_id))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 409 {
            Ok(true)
        } else {
            let text = resp.text().await.unwrap_or_default();
            Err(anyhow::anyhow!("Mattermost addTeamMember failed ({}): {}", status, text))
        }
    }

    /// Create a new channel in a team.
    async fn create_channel(&self, team_id: &str, name: &str, display_name: &str) -> Result<Value> {
        let body = serde_json::json!({
            "team_id": team_id,
            "name": name,
            "display_name": display_name,
            "type": "O",
        });
        let resp = self
            .http_client
            .post(format!("{}/api/v4/channels", self.api_base))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() || status.as_u16() == 409 {
            Ok(serde_json::from_str(&text).unwrap_or(serde_json::json!({})))
        } else {
            Err(anyhow::anyhow!("Mattermost createChannel failed ({}): {}", status, text))
        }
    }

    /// Add a user to a channel.
    async fn add_channel_member(&self, channel_id: &str, user_id: &str) -> Result<bool> {
        let body = serde_json::json!({"user_id": user_id});
        let resp = self
            .http_client
            .post(format!("{}/api/v4/channels/{}/members", self.api_base, channel_id))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 409 {
            Ok(true)
        } else {
            let text = resp.text().await.unwrap_or_default();
            Err(anyhow::anyhow!("Mattermost addChannelMember failed ({}): {}", status, text))
        }
    }

    /// Create a bot account from an existing user.
    async fn create_bot(&self, user_id: &str, display_name: &str, description: &str) -> Result<Value> {
        let body = serde_json::json!({
            "user_id": user_id,
            "display_name": display_name,
            "description": description,
        });
        let resp = self
            .http_client
            .post(format!("{}/api/v4/bots", self.api_base))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() || status.as_u16() == 409 {
            Ok(serde_json::from_str(&text).unwrap_or(serde_json::json!({})))
        } else {
            Err(anyhow::anyhow!("Mattermost createBot failed ({}): {}", status, text))
        }
    }

    /// Create a personal access token for a user.
    async fn create_user_token(&self, user_id: &str, description: &str) -> Result<String> {
        let body = serde_json::json!({"description": description});
        let resp = self
            .http_client
            .post(format!("{}/api/v4/users/{}/tokens", self.api_base, user_id))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            let val: Value = serde_json::from_str(&text)?;
            val["token"]
                .as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow::anyhow!("Mattermost createUserToken response missing token field"))
        } else {
            Err(anyhow::anyhow!("Mattermost createUserToken failed ({}): {}", status, text))
        }
    }

    /// List existing tokens for a user.
    #[allow(dead_code)]
    async fn get_user_tokens(&self, user_id: &str) -> Result<Vec<Value>> {
        let resp = self
            .http_client
            .get(format!("{}/api/v4/users/{}/tokens", self.api_base, user_id))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(serde_json::from_str(&text).unwrap_or_default())
        } else {
            Err(anyhow::anyhow!("Mattermost getUserTokens failed ({}): {}", status, text))
        }
    }

    /// Update user password (admin-only).
    async fn update_user_password(&self, user_id: &str, new_password: &str) -> Result<bool> {
        let body = serde_json::json!({"new_password": new_password});
        let resp = self
            .http_client
            .put(format!("{}/api/v4/users/{}/password", self.api_base, user_id))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(true)
        } else {
            let text = resp.text().await.unwrap_or_default();
            Err(anyhow::anyhow!("Mattermost updateUserPassword failed ({}): {}", status, text))
        }
    }

    /// Get team members.
    #[allow(dead_code)]
    async fn get_team_members(&self, team_id: &str) -> Result<Vec<Value>> {
        let resp = self
            .http_client
            .get(format!("{}/api/v4/teams/{}/members", self.api_base, team_id))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(serde_json::from_str(&text).unwrap_or_default())
        } else {
            Err(anyhow::anyhow!("Mattermost getTeamMembers failed ({}): {}", status, text))
        }
    }

    /// Find team by name.
    async fn find_team_by_name(&self, name: &str) -> Result<Option<String>> {
        let resp = self
            .http_client
            .get(format!("{}/api/v4/teams/name/{}", self.api_base, name))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let team: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            return Ok(team["id"].as_str().map(|s| s.to_string()));
        }
        let text = resp.text().await.unwrap_or_default();
        Err(anyhow::anyhow!("Mattermost findTeamByName failed ({}): {}", status, text))
    }

    /// Find user by username.
    async fn find_user_by_username(&self, username: &str) -> Result<Option<(String, bool)>> {
        let users = self.get_users_all().await?;
        for u in &users {
            if u["username"].as_str() == Some(username) {
                let uid = u["id"].as_str().unwrap_or("").to_string();
                let is_bot = u["is_bot"].as_bool().unwrap_or(false);
                return Ok(Some((uid, is_bot)));
            }
        }
        Ok(None)
    }

    /// Create a bot account and obtain/refresh a personal access token.
    /// Returns the token string.
    async fn setup_bot_token(&self, bot_user_id: &str) -> Result<String> {
        // Always create a new token. The Mattermost GET tokens API returns only
        // token IDs (not values), so we cannot meaningfully reuse existing tokens.
        self.create_user_token(bot_user_id, "OmniAgent bot access token").await
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
    /// Mattermost post type (empty string for regular user messages).
    /// System messages have types like "system_join_channel", "system_add_to_channel",
    /// "system_join_team", "system_leave_channel", etc.
    #[serde(rename = "type")]
    #[serde(default)]
    post_type: String,
    /// File attachments on this post.
    #[serde(default)]
    file_ids: Vec<String>,
}

/// Structured file attachment to pass to omniagent (instead of inline formatting).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileAttachment {
    name: String,
    size: i64,
    mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,  // base64-encoded raw bytes
}

/// File metadata returned by Mattermost file info API.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MattermostFileInfo {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    extension: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    post_id: String,
    #[serde(default)]
    create_at: i64,
    #[serde(default)]
    has_preview_image: bool,
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
    thread_sequence: i32,
    #[serde(default)]
    cause_external_id: Option<String>,
    /// If the cause message was itself a reply in a thread, use this as the
    /// reply target instead of cause_external_id: Mattermost doesn't allow
    /// nested threads, so all replies must reference the thread root.
    #[serde(default)]
    cause_root_id: Option<String>,
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

/// Parameters for the typing indicator method.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TypingParams {
    resource_identifier: String,
    #[serde(default)]
    parent_id: Option<String>,
}

/// Parameters for the react method.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReactParams {
    resource_identifier: String,
    external_id: String,
    emoji: String,
}

/// Parameters for the setup method.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SetupParams {
    #[serde(default)]
    setup_team: String,
    #[serde(default)]
    setup_channel: String,
    #[serde(default)]
    bot_user: String,
    #[serde(default)]
    admin_user: String,
    #[serde(default)]
    admin_password: String,
    #[serde(default)]
    test_user: String,
    #[serde(default)]
    test_password: String,
    #[serde(default)]
    bot_password: String,
}

/// Operational config received via configure message.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginConfig {
    #[serde(default = "default_server_url")]
    server_url: String,
    #[serde(default)]
    access_token_name: Option<String>,
    #[serde(default = "default_connection_mode")]
    connection_mode: String,
    #[serde(default = "default_polling_enabled")]
    polling_enabled: bool,
    #[serde(default = "default_polling_interval", deserialize_with = "deserialize_u64_from_string_or_number")]
    polling_interval: u64,
    // channel_ids removed: plugin auto-discovers channels from omniagent channel records
    #[serde(default)]
    setup_team: String,
    #[serde(default = "default_setup_channel")]
    setup_channel: String,
    #[serde(default = "default_bot_user")]
    bot_user: String,
    #[serde(default)]
    bot_password: Option<String>,
    #[serde(default)]
    admin_user: Option<String>,
    #[serde(default)]
    admin_password: Option<String>,
    #[serde(default)]
    test_user: String,
    #[serde(default)]
    test_password: Option<String>,
    #[serde(default = "default_env_path")]
    env_path: String,
    #[serde(default = "default_max_download_bytes", deserialize_with = "deserialize_u64_from_string_or_number")]
    max_download_bytes: u64,
}

fn default_connection_mode() -> String {
    "websocket".to_string()
}

fn default_polling_interval() -> u64 {
    15
}


fn default_server_url() -> String {
    "http://mattermost:8065".to_string()
}

fn default_env_path() -> String {
    std::env::var("OMNI_DIR").map(|d| format!("{}/.env", d))
        .unwrap_or_else(|_| { eprintln!("FATAL: OMNI_DIR must be set"); std::process::exit(1); })
}

fn default_max_download_bytes() -> u64 {
    10 * 1024 * 1024 // 10 MB
}

fn default_polling_enabled() -> bool {
    true
}

fn default_setup_channel() -> String {
    "setup".to_string()
}

fn default_bot_user() -> String {
    "omniagent".to_string()
}

/// Deserialize a u64 that may be a number, a string, or empty (use default).
fn deserialize_u64_from_string_or_number<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;
    struct U64OrString;
    impl<'de> de::Visitor<'de> for U64OrString {
        type Value = u64;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a u64, string, or empty value")
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> { Ok(v) }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            if v.is_empty() {
                Ok(default_polling_interval())
            } else {
                v.parse::<u64>().map_err(de::Error::custom)
            }
        }
    }
    deserializer.deserialize_any(U64OrString)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();

    // Check if we're in setup mode (invoked with "setup" argument)
    let args: Vec<String> = std::env::args().collect();
    let is_setup_mode = args.get(1).map(|s| s.as_str()) == Some("setup");

    // Stdio reader/writer
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    let stdout = tokio::io::stdout();
    let mut writer = tokio::io::BufWriter::new(stdout);

    //  1. Handle initialize (first message) 
    let first_line = lines.next_line().await?.unwrap_or_default();
    let request: PluginRequest = serde_json::from_str(&first_line)
        .context("Expected initialize request as first message")?;

    let id = request.id.unwrap_or(1);
    let response = match request.method.as_str() {
        "initialize" => handle_initialize(id).await,
        _ => {
            let err_resp = make_error(
                id,
                -1,
                &format!("Expected initialize, got: {}", request.method),
            );
            let response_line = serde_json::to_string(&err_resp)?;
            writer.write_all(response_line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            return Ok(());
        }
    };
    let response_line = serde_json::to_string(&response)?;
    writer.write_all(response_line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    //  2. Handle configure (second message) 
    let second_line = lines.next_line().await?.unwrap_or_default();
    let request: PluginRequest = serde_json::from_str(&second_line)
        .context("Expected configure request as second message")?;

    let id = request.id.unwrap_or(2);
    let config: PluginConfig = match request.method.as_str() {
        "configure" => match request.params.map(|p| serde_json::from_value(p)).transpose() {
            Ok(Some(cfg)) => cfg,
            Ok(None) => {
                let err_resp = make_error(id, -1, "Configure params missing");
                let response_line = serde_json::to_string(&err_resp)?;
                writer.write_all(response_line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
                return Ok(());
            }
            Err(e) => {
                let err_resp =
                    make_error(id, -1, &format!("Invalid configure params: {}", e));
                let response_line = serde_json::to_string(&err_resp)?;
                writer.write_all(response_line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
                return Ok(());
            }
        },
        _ => {
            let err_resp = make_error(
                id,
                -1,
                &format!("Expected configure, got: {}", request.method),
            );
            let response_line = serde_json::to_string(&err_resp)?;
            writer.write_all(response_line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            return Ok(());
        }
    };

    // Acknowledge configure
    let ack = make_success(id, serde_json::json!({"configured": true}));
    let response_line = serde_json::to_string(&ack)?;
    writer.write_all(response_line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    let server_url = config.server_url;
    let secret_name = config.access_token_name.as_deref().unwrap_or("").to_string();
    // Resolve access_token from secret name via omniagent secrets API
    let secrets_http = reqwest::Client::new();
    let access_token = if !secret_name.is_empty() {
        let mut tok_val = get_agent_secret(&secrets_http, &secret_name).await;
        if tok_val.as_ref().map_or(true, |s| s.is_empty()) {
            let env_name = format!("{}_ACCESS_TOKEN", "MATTERMOST");
            if let Ok(env_tok) = std::env::var(&env_name) {
                if !env_tok.is_empty() {
                    tracing::info!("Resolved access_token from env var '{}'", env_name);
                    tok_val = Some(env_tok);
                }
            }
            if tok_val.as_ref().map_or(true, |s| s.is_empty()) {
                if let Ok(env_tok) = std::env::var(&secret_name) {
                    if !env_tok.is_empty() {
                        tracing::info!("Resolved access_token from env var '{}'", secret_name);
                        tok_val = Some(env_tok);
                    }
                }
            }
        }
        if let Some(ref tok) = tok_val {
            if !tok.is_empty() {
                tracing::info!("Resolved access_token for '{}'", secret_name);
            } else {
                tracing::info!("Secret '{}' is empty - will generate during setup", secret_name);
                tok_val = None;
            }
        } else {
            tracing::info!("Secret '{}' not found - will generate during setup", secret_name);
        }
        tok_val
    } else {
        tracing::warn!("No access_token_name configured - plugin will not be able to connect");
        None
    };

    // In setup mode, access_token_name is mandatory
    if is_setup_mode && secret_name.is_empty() {
        let err_resp = make_error(3, -1, "access_token_name is required in plugin config for setup mode. Set it to the name of a secret that will hold the bot access token.");
        let response_line = serde_json::to_string(&err_resp)?;
        writer.write_all(response_line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        return Ok(());
    }

    if is_setup_mode {
        //  Setup mode: read setup request, process, exit 
        let third_line = lines.next_line().await?.unwrap_or_default();
        let request: PluginRequest = match serde_json::from_str(&third_line) {
            Ok(r) => r,
            Err(e) => {
                let err_resp =
                    make_error(3, -1, &format!("Invalid setup request: {}", e));
                let response_line = serde_json::to_string(&err_resp)?;
                writer.write_all(response_line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
                return Ok(());
            }
        };

        let params: SetupParams = match request
            .params
            .map(|p| serde_json::from_value(p))
            .transpose()
        {
            Ok(Some(p)) => p,
            Ok(None) => SetupParams::default(),
            Err(e) => {
                tracing::error!("Invalid setup params: {}", e);
                let err_resp = make_error(
                    request.id.unwrap_or(1),
                    -1,
                    &format!("Invalid setup params: {}", e),
                );
                let response_line = serde_json::to_string(&err_resp)?;
                writer.write_all(response_line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
                return Ok(());
            }
        };

        let id = request.id.unwrap_or(1);

        // Determine which client to use for setup.
        // Priority: admin credentials (full admin privileges) >
        //           default password fallback (handles password-changed scenario) >
        //           access token / bot PAT (limited: can't update passwords) >
        //           create first user (fresh DB only)
        //
        // This ordering ensures admin operations (password update, token creation,
        // config changes) always use an admin session, not a bot PAT which lacks
        // sufficient privileges.
        let client: MattermostClient = 'client: {
            //  1. Try admin login with configured credentials 
            if !params.admin_user.is_empty() && !params.admin_password.is_empty() {
                if let Some(adm) =
                    login_admin_client(&server_url, &params.admin_user, &params.admin_password).await
                {
                    tracing::info!("Logged in as admin '{}' for setup", params.admin_user);
                    break 'client adm;
                }
                tracing::warn!(
                    "Admin '{}' login failed with configured password ({} chars)",
                    params.admin_user,
                    params.admin_password.len()
                );
            }

                // 1b. Try empty password fallback - handles case where a previous
                //     buggy test run created admin user with empty password.
                if let Some(adm) =
                    login_admin_client(&server_url, &params.admin_user, "").await
                {
                    tracing::info!("Logged in as admin '{}' with empty password fallback", params.admin_user);
                    break 'client adm;
                }
            //  2. Try access token (bot PAT: valid auth but limited privileges) 
            if let Some(token) = &access_token {
                let test_client = MattermostClient::new(&server_url, token);
                match test_client.get_me().await {
                    Ok(me) => {
                        tracing::warn!(
                            "Access token is valid but '{}' is NOT an admin (role: system_user): admin operations will fail. Setup will be limited.",
                            me.username
                        );
                        break 'client test_client;
                    }
                    Err(_) => {
                        tracing::warn!("Access token is also stale, trying to create first admin user");
                    }
                }
            }

            //  4. Fresh DB: create first admin user (no auth needed) 
            if !params.admin_user.is_empty() {
                if params.admin_password.is_empty() {
                    tracing::error!("admin_password is empty: must provide a password to create admin user '{}'", params.admin_user);
                    let err_resp = serde_json::json!({
                        "id": id,
                        "error": { "code": -1, "message": format!(
                            "admin_password is required to create admin user '{}'. Set MM_USER_PASSWORD in your .env file.",
                            params.admin_user
                        )}
                    });
                    let mut raw_stdout = tokio::io::stdout();
                    raw_stdout.write_all(serde_json::to_string(&err_resp)?.as_bytes()).await?;
                    raw_stdout.write_all(b"\n").await?;
                    return Ok(());
                }
                let pw = &params.admin_password;
                tracing::info!("Attempting to create first admin user '{}' (fresh DB path)", params.admin_user);
                match create_first_user(&server_url, &params.admin_user, pw, &format!("{}@local.host", params.admin_user)).await {
                    Ok(_) => {
                        tracing::info!("Created first admin user, logging in");
                        if let Some(adm) = login_admin_client(&server_url, &params.admin_user, pw).await {
                            break 'client adm;
                        }
                        let err_resp = serde_json::json!({
                            "id": id,
                            "error": { "code": -1, "message": "Created admin user but login with those credentials failed" }
                        });
                        let mut raw_stdout = tokio::io::stdout();
                        raw_stdout.write_all(serde_json::to_string(&err_resp)?.as_bytes()).await?;
                        raw_stdout.write_all(b"\n").await?;
                        return Ok(());
                    }
                    Err(e) => {
                        // create_first_user failed: users already exist and all auth methods exhausted
                        tracing::error!("All authentication methods exhausted: admin login, default password, access token, and create_first_user all failed. Users likely exist with an unknown password.");
                        let err_resp = serde_json::json!({
                            "id": id,
                            "error": {
                                "code": -1,
                                "message": format!(
                                    "Cannot authenticate to Mattermost as admin. The admin user '{}' exists but none of the tried passwords match. \
                                    To fix: run the following command in the omniagent container (or any container with mmctl):\n\
                                    docker exec omm-mattermost /tmp/mmctl --local user change-password {} --password '<new-password>'\n\
                                    Then update MM_USER_PASSWORD in .env to match and run setup again.\n\
                                    Underlying error: {}",
                                    params.admin_user, params.admin_user, e
                                )
                            }
                        });
                        let mut raw_stdout = tokio::io::stdout();
                        raw_stdout.write_all(serde_json::to_string(&err_resp)?.as_bytes()).await?;
                        raw_stdout.write_all(b"\n").await?;
                        return Ok(());
                    }
                }
            }

            //  5. Nothing worked: error 
            let err_resp = serde_json::json!({
                "id": id,
                "error": { "code": -1, "message": "No valid access_token and no admin_user + admin_password provided for bootstrap" }
            });
            let mut raw_stdout = tokio::io::stdout();
            raw_stdout.write_all(serde_json::to_string(&err_resp)?.as_bytes()).await?;
            raw_stdout.write_all(b"\n").await?;
            return Ok(());
        };

        let response = handle_setup(id, &client, &server_url, &params, &access_token, &secret_name, &secrets_http).await;
        let response_line = serde_json::to_string(&response)?;
        writer.write_all(response_line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        tracing::info!("Setup mode complete");
        return Ok(());
    }

    tracing::info!("Mattermost platform plugin starting");

    let connection_mode = config.connection_mode.to_lowercase();
    let polling_enabled = connection_mode == "polling" && config.polling_enabled;

    let polling_interval_secs = config.polling_interval;
    let max_download_bytes = config.max_download_bytes;

    // Always create a client (it's just a wrapper around reqwest + auth header).
    // With a missing or invalid token, API calls will 401: they already have
    // proper error handling. The plugin stays alive regardless.
    let mut owned_access_token = access_token.unwrap_or_default();
    let mut client = MattermostClient::new(&server_url, &owned_access_token);

    // Verify token by fetching bot user info.
    // If the token is invalid/expired, attempt auto-recovery using admin credentials.
    let bot_user: Option<MattermostUser> = if !owned_access_token.is_empty() {
        match client.get_me().await {
            Ok(u) => {
                tracing::info!(
                    "Authenticated as Mattermost user: {} ({})",
                    u.username,
                    u.id
                );
                Some(u)
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to authenticate with Mattermost: {:?}. Attempting auto-recovery...",
                    e
                );
                //  Auto-recovery: create a new PAT using admin credentials 
                let recovered: Option<(MattermostClient, String, MattermostUser)> = 'recover: {
                    let admin_user = match config.admin_user {
                        Some(ref u) if !u.is_empty() => u.clone(),
                        _ => break 'recover None,
                    };
                    let admin_password = match config.admin_password {
                        Some(ref p) if !p.is_empty() => p.clone(),
                        _ => break 'recover None,
                    };
                    tracing::info!("Auto-recovery: logging in as admin '{}'", admin_user);
                    let admin_client = match login_admin_client(&server_url, &admin_user, &admin_password).await {
                        Some(c) => c,
                        None => {
                            tracing::warn!("Auto-recovery: admin login failed");
                            break 'recover None;
                        }
                    };
                    let bot_username = &config.bot_user;
                    tracing::info!("Auto-recovery: finding bot user '{}'", bot_username);
                    let (bot_user_id, _) = match admin_client.find_user_by_username(bot_username).await {
                        Ok(Some(result)) => result,
                        Ok(None) => {
                            tracing::warn!("Auto-recovery: bot user '{}' not found", bot_username);
                            break 'recover None;
                        }
                        Err(e) => {
                            tracing::warn!("Auto-recovery: failed to find bot user '{}': {:?}", bot_username, e);
                            break 'recover None;
                        }
                    };
                    tracing::info!("Auto-recovery: found bot user '{}' (id: {})", bot_username, bot_user_id);
                    let new_token = match admin_client.create_user_token(&bot_user_id, "OmniAgent bot access token (auto-recovered)").await {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!("Auto-recovery: failed to create new token: {:?}", e);
                            break 'recover None;
                        }
                    };
                    tracing::info!("Auto-recovery: created new access token for '{}'", bot_username);
                    // Persist the new token to the omniagent secret store
                    if !secret_name.is_empty() {
                        match set_agent_secret(&secrets_http, &secret_name, &new_token).await {
                            Ok(_) => tracing::info!("Auto-recovery: updated secret '{}' with new access token", secret_name),
                            Err(we) => tracing::warn!("Auto-recovery: failed to update secret '{}': {:?}", secret_name, we),
                        }
                    }
                    // Create a new client with the recovered token and verify it
                    let new_client = MattermostClient::new(&server_url, &new_token);
                    match new_client.get_me().await {
                        Ok(bot) => {
                            tracing::info!(
                                "Auto-recovery successful: authenticated as {} ({})",
                                bot.username, bot.id
                            );
                            Some((new_client, new_token, bot))
                        }
                        Err(e2) => {
                            tracing::warn!("Auto-recovery: new token also failed authentication: {:?}", e2);
                            None
                        }
                    }
                };
                match recovered {
                    Some((new_client, new_token, bot)) => {
                        client = new_client;
                        owned_access_token = new_token;
                        Some(bot)
                    }
                    None => {
                        tracing::warn!(
                            "Auto-recovery failed. Plugin will run without inbound capability."
                        );
                        None
                    }
                }
            }
        }
    } else {
        tracing::warn!(
            "No access_token provided. \
             Plugin will run without inbound capability until setup is run."
        );
        None
    };

    // Auto-discover channels the bot is a member of
    let channel_ids: Vec<String> = if let Some(ref bot) = bot_user {
        let ids = discover_channels(&client, &bot.id).await;
        if !ids.is_empty() {
            tracing::info!(
                "Watching {} channel(s): {}",
                ids.len(),
                ids.join(", ")
            );
        }
        ids
    } else {
        Vec::new()
    };


    //  Inbound (polling or WebSocket) 
    let use_websocket = connection_mode == "websocket";
    let inbound_handle: Option<tokio::task::JoinHandle<()>> = match bot_user.as_ref() {
        Some(bot) if use_websocket => {
            let bot_id = bot.id.clone();
            Some(tokio::spawn(async move {
                ws_event_loop(
                    server_url,
                    owned_access_token.clone(),
                    vec![],
                    bot_id,
                    max_download_bytes,
                ).await;
            }))
        }
        Some(bot) if polling_enabled => {
            let poll_client = MattermostClient::new(&server_url, &owned_access_token);
            let bot_id = bot.id.clone();
            let server_url_poll = server_url.clone();

            Some(tokio::spawn(async move {
                let mut current_ids: Vec<String> = channel_ids;
                let mut last_discovery: Vec<String> = current_ids.clone();
                let mut last_create_at: HashMap<String, i64> = HashMap::new();
                let mut bot_cache: HashMap<String, bool> = HashMap::new();
                bot_cache.insert(bot_id.clone(), true);
                let mut processed_posts: HashMap<String, HashSet<String>> = HashMap::new();

                for ch_id in &current_ids {
                    init_channel_cursor(&poll_client, ch_id, &bot_id, &mut last_create_at).await;
                }

                let mut refresh_counter: u64 = 0;
                let refresh_interval: u64 = 4;

                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(polling_interval_secs)).await;

                    refresh_counter += 1;
                    if refresh_counter >= refresh_interval {
                        refresh_counter = 0;

                        let discovered = discover_channels(&poll_client, &bot_id).await;
                        let merged = discovered.clone();

                        for ch_id in &merged {
                            if !last_discovery.contains(ch_id) {
                                tracing::info!(
                                    "Discovered new channel {}, initializing cursor",
                                    ch_id
                                );
                                if !last_create_at.contains_key(ch_id.as_str()) {
                                    init_channel_cursor(&poll_client, ch_id, &bot_id, &mut last_create_at).await;
                                }
                            }
                        }

                        for ch_id in &last_discovery {
                            if !merged.contains(ch_id) {
                                tracing::info!("Channel {} no longer accessible, removing", ch_id);
                                last_create_at.remove(ch_id.as_str());
                            }
                        }

                        current_ids = merged;
                        last_discovery = current_ids.clone();
                    }

                    for ch_id in &current_ids {
                        let count = poll_channel(
                            &poll_client, ch_id, &bot_id,
                            &mut last_create_at, &mut bot_cache,
                            &mut processed_posts,
                            &server_url_poll, max_download_bytes,
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
        }
        _ => None,
    };

    //  Main request-response loop 
    // Cache bot_user_id for the react handler
    let bot_user_id: Option<&str> = bot_user.as_ref().map(|u| u.id.as_str());

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
            "react" => {
                if let Some(params) = request.params {
                    match serde_json::from_value::<ReactParams>(params) {
                        Ok(p) => handle_react(req_id, &client, bot_user_id, &p).await,
                        Err(e) => {
                            make_error(req_id, -1, &format!("Invalid react params: {}", e))
                        }
                    }
                } else {
                    make_error(req_id, -1, "Missing params for react")
                }
            }
            "typing" => {
                if let Some(params) = request.params {
                    match serde_json::from_value::<TypingParams>(params) {
                        Ok(p) => handle_typing(req_id, &client, &p).await,
                        Err(e) => {
                            make_error(req_id, -1, &format!("Invalid typing params: {}", e))
                        }
                    }
                } else {
                    make_error(req_id, -1, "Missing params for typing")
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
            "setup": true,
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

    // Skip delivery of empty content: prevents posting blank plan/reasoning messages
    if content.trim().is_empty() {
        return make_success(
            id,
            serde_json::json!({
                "delivered": false,
                "reason": "empty_content",
            }),
        );
    }

    // Determine if this is a threaded reply.
    // Mattermost uses root_id for threading. When the user's message was
    // inside an existing thread (cause_root_id is set), use that as the
    // reply target: Mattermost doesn't allow nested threads, so all
    // replies must reference the thread root. Otherwise use cause_external_id.
    let root_id = if params.cause_root_id.as_ref().is_some_and(|r| !r.is_empty()) {
        params.cause_root_id.as_deref()
    } else if params.is_summary || params.cause_external_id.is_some() {
        params.cause_external_id.as_deref()
    } else {
        None
    };

    // Non-seq-0 messages MUST be in a thread. If we have no thread context,
    // skip delivery rather than posting to the channel directly.
    // Seq-0 messages (thread_sequence == 0) are allowed to create new posts.
    if root_id.is_none() && params.thread_sequence > 0 {
        tracing::warn!(
            "Skipping delivery of seq-{} message to '{}': no thread context available (cause_external_id={:?}, cause_root_id={:?})",
            params.thread_sequence,
            channel_id,
            params.cause_external_id,
            params.cause_root_id,
        );
        return make_success(
            id,
            serde_json::json!({
                "delivered": false,
                "reason": "no_thread_context",
            }),
        );
    }

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

            match client.create_post(channel_id, &display, root_id).await {
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
            match client.create_post(channel_id, content, root_id).await {
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

async fn handle_typing(
    id: u64,
    client: &MattermostClient,
    params: &TypingParams,
) -> PluginResponse {
    match client
        .send_typing(&params.resource_identifier, params.parent_id.as_deref())
        .await
    {
        Ok(sent) => make_success(id, serde_json::json!({"typing": sent})),
        Err(e) => make_error(id, -1, &format!("Failed to send typing: {}", e)),
    }
}

async fn handle_react(
    id: u64,
    client: &MattermostClient,
    bot_user_id: Option<&str>,
    params: &ReactParams,
) -> PluginResponse {
    let bot_user_id = match bot_user_id {
        Some(id) => id,
        None => return make_error(id, -1, "Cannot react: no authenticated Mattermost user (run setup first)"),
    };
    let emoji = params.emoji.trim_matches(':').to_string();
    match client.create_reaction(&params.external_id, bot_user_id, &emoji).await {
        Ok(_) => make_success(id, serde_json::json!({"reacted": true})),
        Err(e) => make_error(id, -1, &format!("Failed to react: {}", e)),
    }
}

/// Update or add a variable in a .env-style file.
/// If the key already exists (e.g. `MATTERMOST_ACCESS_TOKEN=...`), its value is
/// replaced. Otherwise the new entry is appended with a trailing newline.

async fn login_admin_client(server_url: &str, admin_user: &str, admin_password: &str) -> Option<MattermostClient> {
    let http_client = reqwest::Client::new();

    // Login
    let login_body = serde_json::json!({
        "login_id": admin_user,
        "password": admin_password,
    });

    let resp = http_client
        .post(format!("{}/api/v4/users/login", server_url))
        .json(&login_body)
        .send()
        .await
        .ok()?;

    let token = resp.headers().get("Token")?.to_str().ok()?.to_string();
    let session_auth = format!("Bearer {}", token);

    Some(MattermostClient {
        http_client,
        api_base: server_url.trim_end_matches('/').to_string(),
        auth_header: session_auth,
    })
}

/// Create the first admin user on a fresh Mattermost instance.
///
/// Mattermost does not require authentication for creating the first user.
/// The first user automatically becomes a system administrator.
async fn create_first_user(server_url: &str, username: &str, password: &str, email: &str) -> Result<Value> {
    let http_client = reqwest::Client::new();

    let body = serde_json::json!({
        "username": username,
        "password": password,
        "email": email,
        "allow_marketing": false,
    });

    let resp = http_client
        .post(format!("{}/api/v4/users", server_url.trim_end_matches('/')))
        .json(&body)
        .send()
        .await
        .context("Failed to create first admin user")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Create first user failed ({}): {}", status, body_text);
    }

    let user: Value = resp
        .json()
        .await
        .context("Failed to parse create user response")?;

    tracing::info!(
        "Created first admin user '{}' (id: {})",
        username,
        user["id"].as_str().unwrap_or("unknown")
    );

    Ok(user)
}

// ---------------------------------------------------------------------------
// Setup Handler
// ---------------------------------------------------------------------------

/// Run the Mattermost setup process: create team, channel, users, bot, token.
/// Validates required config fields, creates resources idempotently,
/// and returns team_id, channel_id, bot_token, etc.
async fn handle_setup(id: u64, client: &MattermostClient, server_url: &str, params: &SetupParams, access_token: &Option<String>, secret_name: &str, secrets_http: &reqwest::Client) -> PluginResponse {
    // Validate required fields
    if params.setup_team.is_empty() {
        return make_error(id, -1, "Missing required config: setup_team: set it in the plugin config");
    }
    if params.setup_channel.is_empty() {
        return make_error(id, -1, "Missing required config: setup_channel: set it in the plugin config");
    }
    if params.bot_user.is_empty() {
        return make_error(id, -1, "Missing required config: bot_user: set it in the plugin config");
    }

    tracing::info!(
        "Starting Mattermost setup: team={}, channel={}, bot_user={}",
        params.setup_team, params.setup_channel, params.bot_user
    );

    // 1. Verify auth
    let bot_me = match client.get_me().await {
        Ok(u) => u,
        Err(e) => {
            return make_error(id, -1, &format!("Authentication failed: check access_token and server_url: {}", e));
        }
    };

    // 2. Create or find team
    let team_id = match client.find_team_by_name(&params.setup_team).await {
        Ok(Some(tid)) => {
            tracing::info!("Team '{}' already exists", params.setup_team);
            tid
        }
        Ok(None) => {
            match client.create_team(&params.setup_team, &params.setup_team).await {
                Ok(t) => match t["id"].as_str().map(|s| s.to_string()) {
                    Some(tid) => {
                        tracing::info!("Created team '{}'", params.setup_team);
                        tid
                    }
                    None => return make_error(id, -1, &format!("Team '{}' created but no id returned", params.setup_team)),
                },
                Err(e) => return make_error(id, -1, &format!("Failed to create team '{}': {}", params.setup_team, e)),
            }
        }
        Err(e) => return make_error(id, -1, &format!("Failed to look up team '{}': {}", params.setup_team, e)),
    };

    // 3. Add bot to team
    let _ = client.add_team_member(&team_id, &bot_me.id).await;

    // 4. Create or find channel
    let channels = client.get_user_channels(&bot_me.id, &team_id).await.unwrap_or_default();
    let channel_id = match channels.iter().find(|c| c.name == params.setup_channel) {
        Some(c) => c.id.clone(),
        None => {
            // Create channel
            match client.create_channel(&team_id, &params.setup_channel, &params.setup_channel).await {
                Ok(c) => c["id"].as_str().unwrap_or("").to_string(),
                Err(_) => String::new(),
            }
        }
    };

    if channel_id.is_empty() {
        return make_error(id, -1, &format!("Failed to create channel '{}'", params.setup_channel));
    }
    let _ = client.add_channel_member(&channel_id, &bot_me.id).await;

    // 5. Admin user: password update needs admin privileges
    //    If the client is a bot PAT (not admin), this will fail and be logged
    let mut admin_id: Option<String> = None;
    if !params.admin_user.is_empty() && !params.admin_password.is_empty() {
        let pw = &params.admin_password;
        match client.find_user_by_username(&params.admin_user).await {
            Ok(Some((uid, _))) => {
                match client.update_user_password(&uid, pw).await {
                    Ok(_) => tracing::info!("Updated password for admin user '{}'", params.admin_user),
                    Err(e) => tracing::warn!(
                        "Could not update password for admin user '{}': {}. \
                         This is expected when the client is a bot PAT without admin privileges. \
                         The password will remain as previously set.",
                        params.admin_user, e
                    ),
                }
                let _ = client.add_team_member(&team_id, &uid).await;
                let _ = client.add_channel_member(&channel_id, &uid).await;
                admin_id = Some(uid);
            }
            Ok(None) => {
                if let Ok(u) = client.create_user(&params.admin_user, pw, &format!("{}@local.host", params.admin_user)).await {
                    if let Some(uid) = u["id"].as_str().map(|s| s.to_string()) {
                        let _ = client.add_team_member(&team_id, &uid).await;
                        let _ = client.add_channel_member(&channel_id, &uid).await;
                        admin_id = Some(uid);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to look up admin user '{}': {}", params.admin_user, e);
            }
        }
    }

    // 6. Test user: skip if no password provided
    if !params.test_user.is_empty() && !params.test_password.is_empty() {
        let pw = &params.test_password;
        match client.find_user_by_username(&params.test_user).await {
            Ok(Some((uid, _))) => {
                let _ = client.update_user_password(&uid, pw).await;
                let _ = client.add_team_member(&team_id, &uid).await;
                let _ = client.add_channel_member(&channel_id, &uid).await;
            }
            Ok(None) => {
                if let Ok(u) = client.create_user(&params.test_user, pw, &format!("{}@local.host", params.test_user)).await {
                    if let Some(uid) = u["id"].as_str().map(|s| s.to_string()) {
                        let _ = client.add_team_member(&team_id, &uid).await;
                        let _ = client.add_channel_member(&channel_id, &uid).await;
                    }
                }
            }
            Err(_) => {}
        }
    }

    // 7. Bot account + token
    match client.find_user_by_username(&params.bot_user).await {
        Ok(Some((uid, _))) => {
            // Register as bot if not already
            let _ = client.create_bot(&uid, "OmniAgent Bot", "Bot account for OmniAgent").await;
            let _ = client.add_team_member(&team_id, &uid).await;
            let _ = client.add_channel_member(&channel_id, &uid).await;

            // Get or create token: reuse existing access_token if valid,
            // otherwise create a new one (needs admin auth).
            let bot_token = {
                // Check if existing access_token is still valid for this bot user
                let mut reuse = None;
                if let Some(tok) = access_token {
                    if !tok.is_empty() {
                        let test = MattermostClient::new(server_url, tok);
                        if let Ok(me) = test.get_me().await {
                            if me.id == uid {
                                tracing::info!("Existing access_token valid for bot user '{}' — reusing", params.bot_user);
                                reuse = Some(tok.clone());
                            }
                        }
                    }
                }
                if let Some(tok) = reuse {
                    tok
                } else if !params.admin_user.is_empty() && !params.admin_password.is_empty() {
                    // Login as admin to create/manage tokens
                    let admin_client = login_admin_client(server_url, &params.admin_user, &params.admin_password).await;
                    match admin_client {
                        Some(adm) => {
                            match adm.create_user_token(&uid, "OmniAgent bot access token").await {
                                Ok(t) => {
                                    tracing::info!("Created new token for bot user");
                                    t
                                }
                                Err(e) => {
                                    tracing::warn!("Could not create token: {}. Trying to find existing token...", e);
                                    // List existing tokens and find one for this bot user
                                    match adm.get_user_tokens(&uid).await {
                                        Ok(tokens) => {
                                            let found = tokens.iter()
                                                .filter_map(|t| t["token"].as_str())
                                                .next()
                                                .map(|s| s.to_string());
                                            if let Some(tok) = found {
                                                tracing::info!("Found existing token for bot user");
                                                tok
                                            } else {
                                                tracing::warn!("No existing token found for bot user");
                                                String::new()
                                            }
                                        }
                                        Err(list_err) => {
                                            tracing::warn!("Failed to list tokens: {}", list_err);
                                            String::new()
                                        }
                                    }
                                }
                            }
                        }
                        None => {
                            tracing::warn!("Could not create admin client: token management may fail");
                            String::new()
                        }
                    }
                } else {
                    // No admin credentials: try using bot PAT directly
                    match client.setup_bot_token(&uid).await {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!("Could not get bot token: {}. The bot user exists but no new token was created.", e);
                            String::new()
                        }
                    }
                }
            };

            // Persist the bot_token to the omniagent secret store
            if !bot_token.is_empty() && !secret_name.is_empty() {
                if let Err(e) = set_agent_secret(&secrets_http, secret_name, &bot_token).await {
                    tracing::warn!("Failed to persist bot_token to secret '{}': {:?}", secret_name, e);
                } else {
                    tracing::info!("Persisted bot_token to secret '{}'", secret_name);
                }
            }

            let result = serde_json::json!({
                "success": !bot_token.is_empty(),
                "team_id": team_id,
                "team_name": params.setup_team,
                "channel_id": channel_id,
                "channel_name": params.setup_channel,
                "bot_user_id": uid,
                "bot_user": params.bot_user,
                "bot_token": bot_token,
                "admin_user_id": admin_id,
            });
            make_success(id, result)
        }
        Ok(None) => {
            // Create bot user: requires bot_password
            if params.bot_password.is_empty() {
                return make_error(id, -1, &format!(
                    "bot_password is required to create bot user '{}'. Set MM_BOT_PASSWORD in your .env file.",
                    params.bot_user
                ));
            }
            match client.create_user(&params.bot_user,
                &params.bot_password,
                &format!("{}@local.host", params.bot_user)).await {
                Ok(u) => {
                    if let Some(uid) = u["id"].as_str().map(|s| s.to_string()) {
                        let _ = client.create_bot(&uid, "OmniAgent Bot", "Bot account for OmniAgent").await;
                        let _ = client.add_team_member(&team_id, &uid).await;
                        let _ = client.add_channel_member(&channel_id, &uid).await;

                        match client.setup_bot_token(&uid).await {
                            Ok(token) => {
                                let _ = set_agent_secret(&secrets_http, secret_name, &token).await;
                                make_success(id, serde_json::json!({
                                "success": true,
                                "team_id": team_id,
                                "team_name": params.setup_team,
                                "channel_id": channel_id,
                                "channel_name": params.setup_channel,
                                "bot_user_id": uid,
                                "bot_user": params.bot_user,
                                "bot_token": token,
                                "admin_user_id": admin_id,
                            }))
                            },
                            Err(e) => make_error(id, -1, &format!("Failed to obtain bot token: {}", e)),
                        }
                    } else {
                        make_error(id, -1, &format!("Bot user '{}' created but no id returned", params.bot_user))
                    }
                }
                Err(e) => make_error(id, -1, &format!("Failed to create bot user '{}': {}", params.bot_user, e)),
            }
        }
        Err(e) => make_error(id, -1, &format!("Error looking up bot user '{}': {}", params.bot_user, e)),
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

/// Base64-encode raw bytes for transport in structured file attachments.
fn base64_encode(bytes: &[u8]) -> String {
    use base64::engine::general_purpose;
    use base64::Engine;
    general_purpose::STANDARD.encode(bytes)
}

/// Send an inbound_message notification to stdout (shared by polling and WS).
async fn send_inbound_notification(
    client: &MattermostClient,
    post: &MattermostPost,
    ch_id: &str,
    server_url: &str,
    max_download_bytes: u64,
) {
    let root_id = if post.root_id.is_empty() {
        None
    } else {
        Some(post.root_id.as_str())
    };

    let thread_id = root_id.unwrap_or(&post.id);

    // Build structured file attachments list
    let mut file_attachments: Vec<FileAttachment> = Vec::new();

    for file_id in &post.file_ids {
        match client.get_file_info(file_id).await {
            Ok(info) => {
                let file_content = if info.size > 0 && info.size <= max_download_bytes as i64 {
                    match client.get_file_content(file_id).await {
                        Ok(bytes) => Some(base64_encode(&bytes)),
                        Err(e) => {
                            tracing::warn!("Failed to download file {}: {:?}", file_id, e);
                            None
                        }
                    }
                } else {
                    None
                };

                let name = if info.name.is_empty() {
                    format!("file.{}", info.extension)
                } else {
                    info.name
                };

                file_attachments.push(FileAttachment {
                    name,
                    size: info.size,
                    mime_type: info.mime_type,
                    content: file_content,
                });
            }
            Err(e) => {
                tracing::warn!("Failed to get file info for {}: {:?}", file_id, e);
            }
        }
    }

    tracing::info!(
        "Inbound message from channel {}: {} ({} file attachments)",
        ch_id,
        post.message.chars().take(50).collect::<String>(),
        post.file_ids.len()
    );

    let notification = PluginNotification {
        method: "inbound_message".to_string(),
        params: Some(serde_json::json!({
            "resource_identifier": ch_id,
            "text": post.message,
            "external_id": post.id,
            "files": file_attachments,
            "metadata": {
                "root_id": root_id,
                "thread_id": thread_id,
                "user_id": post.user_id,
                "channel_id": ch_id,
                "file_ids": post.file_ids,
                "server_url": server_url,
            },
        })),
    };

    let line = serde_json::to_string(&notification).unwrap_or_default();
    let mut out = tokio::io::stdout();
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.write_all(b"\n").await;
    let _ = out.flush().await;
}

/// Send a message_deleted notification to stdout.
async fn send_delete_notification(ch_id: &str, post_id: &str) {
    let notification = PluginNotification {
        method: "message_deleted".to_string(),
        params: Some(serde_json::json!({
            "resource_identifier": ch_id,
            "external_id": post_id,
        })),
    };
    let line = serde_json::to_string(&notification).unwrap_or_default();
    let mut out = tokio::io::stdout();
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.write_all(b"\n").await;
    let _ = out.flush().await;
}

/// Send a message_edited notification to stdout.
async fn send_edit_notification(ch_id: &str, post_id: &str, new_message: &str) {
    let notification = PluginNotification {
        method: "message_edited".to_string(),
        params: Some(serde_json::json!({
            "resource_identifier": ch_id,
            "external_id": post_id,
            "text": new_message,
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
    processed_posts: &mut HashMap<String, std::collections::HashSet<String>>,
    server_url: &str,
    max_download_bytes: u64,
) -> u32 {
    let mut count = 0u32;
    let last_ts = last_create_at.get(ch_id).copied().unwrap_or(0);

    match client.get_channel_posts(ch_id, 0, 60).await {
        Ok(posts) => {
            let mut newest_ts = last_ts;
            let known = processed_posts.entry(ch_id.to_string()).or_default();

            // Posts are newest-first from the API. We iterate in reverse
            // so we process oldest to newest (create_at is monotonic in rev()).
            for post in posts.iter().rev() {
                //  Detect deleted posts that we previously processed 
                if post.delete_at != 0 {
                    if known.remove(&post.id) {
                        // This post was previously processed and is now deleted
                        send_delete_notification(ch_id, &post.id).await;
                        tracing::info!(
                            "Polling: detected deleted post {} in channel {}",
                            post.id, ch_id
                        );
                    } else if post.create_at > last_ts {
                        // Post was created AND deleted between polling cycles.
                        // It was never in `known` (never tracked as alive), but we
                        // still need to report the deletion for the omniagent to
                        // remove it from its own tracking.
                        send_delete_notification(ch_id, &post.id).await;
                        tracing::info!(
                            "Polling: detected cross-cycle deleted post {} in channel {}",
                            post.id, ch_id
                        );
                    }
                    continue;
                }

                // Skip already-seen posts (deletion check above runs regardless)
                if post.create_at <= last_ts {
                    continue;
                }

                if post.create_at > newest_ts {
                    newest_ts = post.create_at;
                }

                // Skip system messages (e.g. "joined the channel", "added to team")
                if !post.post_type.is_empty() {
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
                send_inbound_notification(client, &post, ch_id, server_url, max_download_bytes).await;
                known.insert(post.id.clone());
                count += 1;
            }

            // Advance cursor to the newest post on the server
            if newest_ts > last_ts {
                last_create_at.insert(ch_id.to_string(), newest_ts);
            }

            // Trim old tracked posts: keep only those within the current 60-post window
            if known.len() > 200 {
                known.clear();
            }
        }
        Err(e) => {
            tracing::error!("poll_channel error for channel {}: {:?}", ch_id, e);
        }
    }

    count
}

/// Initialize the cursor for a single channel: use latest HUMAN post timestamp.
/// Bot posts are skipped by poll_channel, so using them as cursor would miss
/// human posts made before the bot reply (create_at <= cursor).
async fn init_channel_cursor(
    client: &MattermostClient,
    ch_id: &str,
    bot_id: &str,
    last_create_at: &mut HashMap<String, i64>,
) {
    match client.get_channel_posts(ch_id, 0, 60).await {
        Ok(posts) => {
            for post in posts.iter() {
                if post.user_id != *bot_id {
                    last_create_at.insert(ch_id.to_string(), post.create_at - 1);
                    return;
                }
            }
            tracing::warn!(
                "No human posts in channel {}, polling from 0",
                ch_id
            );
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

/// Build the WebSocket URL from the HTTP(S) server URL with auth token as query parameter.
/// Some Mattermost versions (v10+) require the token in the URL for PAT-based auth.
fn ws_api_url(server_url: &str, access_token: &str) -> String {
    let base = server_url.trim_end_matches('/');
    let mut url = base.replacen("http", "ws", 1).to_string() + "/api/v4/websocket";
    url.push_str("?auth_token=");
    url.push_str(&urlencoding(&access_token));
    url
}

/// URL-encode a string for query parameters (minimal: only encode what's needed).
fn urlencoding(s: &str) -> String {
    s.replace('%', "%25")
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace('+', "%2B")
        .replace(' ', "%20")
}

// ---------------------------------------------------------------------------
// Per-channel debounce state for WebSocket event processing
// ---------------------------------------------------------------------------

/// Tracks whether a channel is currently being polled and whether a
/// re-poll is needed after the current one finishes. Multiple rapid WS
/// events coalesce into a single pending flag: the cursor-based catch-up
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
    processed_posts: &mut HashMap<String, HashSet<String>>,
    server_url: &str,
    max_download_bytes: u64,
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

        let count = poll_channel(client, ch_id, bot_id, last_create_at, bot_cache, processed_posts, server_url, max_download_bytes).await;
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
    max_download_bytes: u64,
) {
    let mut last_create_at: HashMap<String, i64> = HashMap::new();
    let mut bot_cache: HashMap<String, bool> = HashMap::new();
    bot_cache.insert(bot_id.clone(), true);

    let watch_all = channel_ids.is_empty();
    let channel_set: std::collections::HashSet<String> =
        channel_ids.into_iter().collect();

    let mut backoff = 1u64;
    let mut debounce: HashMap<String, ChannelDebounce> = HashMap::new();
    let mut processed_posts: HashMap<String, HashSet<String>> = HashMap::new();

    loop {
        let url = ws_api_url(&server_url, &access_token);
        tracing::info!("Connecting to Mattermost WebSocket: {}", url);

        match connect_async(&url).await {
            Ok((ws_stream, _response)) => {
                tracing::info!("WebSocket connected, authenticating...");
                backoff = 1;

                //  Catch-up on connect: process any missed messages 
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
                        init_channel_cursor(&client, ch_id, &bot_id, &mut last_create_at).await;
                    }
                }
                // Do a full poll for all known channels (catches missed messages)
                for ch_id in &channels {
                    let count = poll_channel(
                        &client, ch_id, &bot_id,
                        &mut last_create_at, &mut bot_cache,
                        &mut processed_posts,
                        &server_url, max_download_bytes,
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

                // Event loop: WS events just trigger poll_channel for that channel
                loop {
                    match read.next().await {
                        Some(Ok(Message::Text(text))) => {
                            let event: Value = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!(
                                        "WS: Failed to parse message JSON: {}: raw text (first 200 chars): {}",
                                        e,
                                        if text.len() > 200 { format!("{}...", &text[..200]) } else { text.to_string() }
                                    );
                                    continue;
                                }
                            };

                            let event_type = match event.get("event").and_then(|e| e.as_str()) {
                                Some(t) => t,
                                None => {
                                    tracing::debug!(
                                        "WS: Message has no 'event' field: keys: {:?}, raw (first 200): {}",
                                        event.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
                                        if text.len() > 200 { format!("{}...", &text[..200]) } else { text.to_string() }
                                    );
                                    continue;
                                }
                            };

                            match event_type {
                                "hello" => {
                                    tracing::info!("WebSocket authenticated successfully");
                                }
                                "posted" => {
                                    // Extract channel_id from broadcast field (data doesn't have channel_id)
                                    let ch_id = match event
                                        .pointer("/broadcast/channel_id")
                                        .and_then(|v| v.as_str())
                                    {
                                        Some(c) => c.to_string(),
                                        None => {
                                            tracing::debug!("posted event missing broadcast channel_id");
                                            continue;
                                        }
                                    };

                                    // Check if we should process this channel
                                    if !watch_all && !channel_set.contains(&ch_id) {
                                        continue;
                                    }

                                    // Initialize cursor lazily for new channels
                                    if !last_create_at.contains_key(&ch_id) {
                                        init_channel_cursor(&client, &ch_id, &bot_id, &mut last_create_at).await;
                                    }

                                    // Trigger cursor-based processing for this channel
                                    // with debounce: if already processing, coalesces into
                                    // a single re-poll after current run + 5s wait.
                                    process_channel_event(
                                        &client, &ch_id, &bot_id,
                                        &mut last_create_at, &mut bot_cache,
                                        &mut debounce, &mut processed_posts,
                                        &server_url, max_download_bytes,
                                    ).await;
                                }
                                "post_deleted" => {
                                    // A post was deleted. Extract the post_id and channel_id,
                                    // then send a message_deleted notification to the omniagent.
                                    let ch_id = match event
                                        .pointer("/broadcast/channel_id")
                                        .and_then(|v| v.as_str())
                                    {
                                        Some(c) => c.to_string(),
                                        None => {
                                            tracing::debug!("post_deleted event missing broadcast channel_id");
                                            continue;
                                        }
                                    };

                                    // Skip channels we don't watch
                                    if !watch_all && !channel_set.contains(&ch_id) {
                                        continue;
                                    }

                                    // The post field from Mattermost WS is a JSON-ENCODED STRING
                                    // like "{\"id\":\"post_id\",\"channel_id\":\"...\",...}".
                                    // We need to parse it as JSON to extract the actual ID.
                                    let post_id: Option<String> = event
                                        .pointer("/data/post")
                                        .and_then(|v| {
                                            // First try: it's already a plain string (just the ID)
                                            if let Some(s) = v.as_str() {
                                                // Try to parse it as JSON: if it works, extract the "id" field
                                                if let Ok(post_obj) = serde_json::from_str::<Value>(s) {
                                                    post_obj.get("id")
                                                        .and_then(|i| i.as_str())
                                                        .map(|id| id.to_string())
                                                } else {
                                                    // Not JSON: it's just a plain post ID string
                                                    Some(s.to_string())
                                                }
                                            } else if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                                                Some(id.to_string())
                                            } else {
                                                None
                                            }
                                        });

                                    if let Some(pid) = post_id {
                                        tracing::info!(
                                            "Post deleted in channel {}: post_id={}",
                                            ch_id, pid
                                        );
                                        send_delete_notification(&ch_id, &pid).await;
                                    } else {
                                        tracing::debug!(
                                            "post_deleted event missing post id in channel {}",
                                            ch_id
                                        );
                                    }
                                }
                                "post_edited" => {
                                    // A post was edited. Extract the post_id, channel_id, and new
                                    // message text, then send a message_edited notification to
                                    // the omniagent so it can update the message content if the
                                    // thread is still pending.
                                    let ch_id = match event
                                        .pointer("/broadcast/channel_id")
                                        .and_then(|v| v.as_str())
                                    {
                                        Some(c) => c.to_string(),
                                        None => {
                                            tracing::debug!("post_edited event missing broadcast channel_id");
                                            continue;
                                        }
                                    };

                                    // Skip channels we don't watch
                                    if !watch_all && !channel_set.contains(&ch_id) {
                                        continue;
                                    }

                                    // The post field is a JSON-ENCODED STRING containing the
                                    // updated post, including the new "message" field.
                                    let edit_info: Option<(String, String)> = event
                                        .pointer("/data/post")
                                        .and_then(|v| {
                                            if let Some(s) = v.as_str() {
                                                if let Ok(post_obj) = serde_json::from_str::<Value>(s) {
                                                    let pid = post_obj.get("id")
                                                        .and_then(|i| i.as_str())
                                                        .map(|s| s.to_string());
                                                    let msg = post_obj.get("message")
                                                        .and_then(|m| m.as_str())
                                                        .map(|s| s.to_string());
                                                    pid.zip(msg)
                                                } else {
                                                    None
                                                }
                                            } else {
                                                None
                                            }
                                        });

                                    if let Some((pid, new_message)) = edit_info {
                                        tracing::info!(
                                            "Post edited in channel {}: post_id={}, new message preview: {}",
                                            ch_id, pid,
                                            new_message.chars().take(50).collect::<String>()
                                        );
                                        send_edit_notification(&ch_id, &pid, &new_message).await;
                                    } else {
                                        tracing::debug!(
                                            "post_edited event missing post id or message in channel {}",
                                            ch_id
                                        );
                                    }
                                }
                                _ => {
                                    // Log unknown event types for debugging
                                    let data_summary = event.get("data")
                                        .map(|d| serde_json::to_string(d).unwrap_or_default())
                                        .unwrap_or_default();
                                    tracing::debug!(
                                        "WS: Unknown event type '{}': data: {}",
                                        event_type,
                                        if data_summary.len() > 200 { format!("{}...", &data_summary[..200]) } else { data_summary }
                                    );
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


// ---------------------------------------------------------------------------
// OmniAgent secrets API helpers
// ---------------------------------------------------------------------------

/// Get the omniagent HTTP API URL from env vars.
fn agent_api_url() -> String {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    format!("http://localhost:{}", port)
}

/// Retrieve a secret value from the omniagent secrets API by name.
async fn get_agent_secret(http_client: &reqwest::Client, secret_name: &str) -> Option<String> {
    let url = format!("{}/secrets/{}", agent_api_url(), secret_name);
    let resp = http_client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    match body.get("data")?.get("current_value")?.as_str() {
        Some(v) if !v.is_empty() => Some(v.to_string()),
        _ => None,
    }
}

/// Store or update a secret via the omniagent secrets API.
async fn set_agent_secret(http_client: &reqwest::Client, secret_name: &str, value: &str) -> anyhow::Result<()> {
    let base = agent_api_url();
    let payload = serde_json::json!({"value": value});

    let put_url = format!("{}/secrets/{}", base, secret_name);
    let resp = http_client
        .put(&put_url)
        .json(&payload)
        .send()
        .await
        .context("Failed to PUT agent secret")?;

    if resp.status().as_u16() == 404 {
        let post_url = format!("{}/secrets", base);
        let create_body = serde_json::json!({
            "name": secret_name,
            "fieldType": "password",
            "value": value
        });
        http_client
            .post(&post_url)
            .json(&create_body)
            .send()
            .await
            .context("Failed to POST agent secret")?;
    }

    Ok(())
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
