use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    pub platform: String,
    pub external_id: String,
    pub cause: String,
    pub current_profile: String,
    pub current_model: Option<String>,
    pub current_provider: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Tracks channels that have been stopped (paused).
///
/// When a channel is stopped, new messages are not processed until the
/// stop is cleared. This is used for channels that are rate-limited or
/// temporarily disabled.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelStop {
    pub id: i64,
    pub channel_id: i64,
    pub stopped_at: DateTime<Utc>,
}
