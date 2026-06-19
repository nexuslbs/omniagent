use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A full message record as stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Message {
    pub id: i64,
    pub thread_id: i64,
    pub role: String,
    pub content: String,
    pub thread_sequence: i32,
    pub external_id: Option<String>,
    pub metadata: serde_json::Value,
    pub embedding: Option<String>,
    pub summary_text: Option<String>,
    pub is_summary: bool,
    pub msg_type: String,
    pub msg_subtype: Option<String>,
    pub created_at: DateTime<Utc>,
    pub processing_time_ms: Option<i32>,
    pub token_usage: Option<serde_json::Value>,
}

/// Payload for creating a new message (without server-assigned fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageNew {
    pub thread_id: i64,
    pub role: String,
    pub content: String,
    pub thread_sequence: i32,
    pub external_id: Option<String>,
    pub metadata: serde_json::Value,
    pub embedding: Option<String>,
    pub summary_text: Option<String>,
    pub is_summary: bool,
    pub msg_type: String,
    pub msg_subtype: Option<String>,
    pub processing_time_ms: Option<i32>,
    pub token_usage: Option<serde_json::Value>,
}
