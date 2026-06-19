use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Thread {
    pub id: i64,
    pub status: String,
    pub cause: String,
    pub channel_id: i64,
    pub profile: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub input_tokens: i32,
    pub cached_tokens: i32,
    pub output_tokens: i32,
    pub duration_ms: i32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadNew {
    pub cause: String,
    pub channel_id: i64,
    pub profile: String,
    pub provider: Option<String>,
    pub model: Option<String>,
}
