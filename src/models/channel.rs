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
    pub readonly: bool,
    pub closed: bool,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            platform: String::new(),
            external_id: String::new(),
            cause: String::new(),
            current_profile: String::new(),
            current_model: None,
            current_provider: None,
            readonly: false,
            closed: false,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            created_at: DateTime::from_timestamp(0, 0).unwrap_or(DateTime::UNIX_EPOCH),
            updated_at: DateTime::from_timestamp(0, 0).unwrap_or(DateTime::UNIX_EPOCH),
        }
    }
}

/// Tracks channels that have been stopped (paused).
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelStop {
    pub id: i64,
    pub channel_id: i64,
    pub stopped_at: DateTime<Utc>,
}
