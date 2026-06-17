/// Profile database model.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A profile stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[expect(dead_code)]
pub struct ProfileRow {
    pub id: i64,
    pub name: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub max_tokens: Option<i32>,
    pub temperature: Option<f64>,
    pub allowed_tools: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Payload for creating/updating a profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[expect(dead_code)]
pub struct ProfileNew {
    pub name: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub max_tokens: Option<i32>,
    pub temperature: Option<f64>,
    pub allowed_tools: Vec<String>,
}
