//! DB-focused structs using only primitive types compatible with sql-forge's
//! compile-time validation. Each struct mirrors a domain model but stores
//! complex types (DateTime, JSON) as plain strings. Conversion to
//! domain types is done explicitly in Rust — no SQL type casting.
//!
//! This file serves as the backward-compatible re-export hub: all query functions
//! have been split into domain-specific sub-modules, but `use crate::db::types as queries;`
//! still works because everything is re-exported here via `pub use`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Thread DB struct (for SELECT results)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ThreadDb {
    pub id: i64,
    pub status: String,
    pub cause: String,
    pub channel_id: i64,
    pub profile: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<i32>,
    pub cached_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub duration_ms: Option<i32>,
    pub created_at: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub terminal: bool,
    pub task_id: Option<String>,
    pub schedule_task_id: Option<String>,
    pub planning_mode: String,
    pub parent_id: Option<i64>,
    pub iterations: i32,
}

impl TryFrom<ThreadDb> for Thread {
    type Error = crate::error::Error;

    fn try_from(db: ThreadDb) -> Result<Self, Self::Error> {
        Ok(Self {
            id: db.id,
            status: db.status,
            cause: db.cause,
            channel_id: db.channel_id,
            profile: db.profile,
            provider: db.provider,
            model: db.model,
            input_tokens: db.input_tokens.unwrap_or(0),
            cached_tokens: db.cached_tokens.unwrap_or(0),
            output_tokens: db.output_tokens.unwrap_or(0),
            duration_ms: db.duration_ms.unwrap_or(0),
            created_at: db
                .created_at
                .as_deref()
                .unwrap_or("")
                .parse::<DateTime<Utc>>()
                .map_err(|e| {
                    crate::error::Error::Message(format!(
                        "Invalid timestamp '{}': {}",
                        db.created_at.as_deref().unwrap_or("?"),
                        e
                    ))
                })?,
            started_at: if let Some(ref s) = db.started_at {
                if !s.is_empty() {
                    Some(s.parse::<DateTime<Utc>>().map_err(|e| {
                        crate::error::Error::Message(format!("Invalid timestamp '{}': {}", s, e))
                    })?)
                } else {
                    None
                }
            } else {
                None
            },
            ended_at: if let Some(ref s) = db.ended_at {
                if !s.is_empty() {
                    Some(s.parse::<DateTime<Utc>>().map_err(|e| {
                        crate::error::Error::Message(format!("Invalid timestamp '{}': {}", s, e))
                    })?)
                } else {
                    None
                }
            } else {
                None
            },
            terminal: db.terminal,
            task_id: db.task_id,
            schedule_task_id: db.schedule_task_id,
            planning_mode: db.planning_mode,
            parent_id: db.parent_id,
            iterations: db.iterations,
        })
    }
}

// ---------------------------------------------------------------------------
// Message DB struct (for SELECT results) — simplified without per-thread fields
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MessageDb {
    pub id: i64,
    pub thread_id: i64,
    pub role: String,
    pub content: String,
    pub thread_sequence: i32,
    pub external_id: Option<String>,
    pub metadata: Option<String>,
    pub embedding: Option<String>,
    pub summary_text: Option<String>,
    pub is_summary: bool,
    pub msg_type: String,
    pub msg_subtype: Option<String>,
    pub created_at: Option<String>,
    pub iteration_number: i32,
}

impl TryFrom<MessageDb> for Message {
    type Error = crate::error::Error;

    fn try_from(db: MessageDb) -> Result<Self, Self::Error> {
        Ok(Self {
            id: db.id,
            thread_id: db.thread_id,
            role: db.role,
            content: db.content,
            thread_sequence: db.thread_sequence,
            external_id: db.external_id,
            metadata: db
                .metadata
                .as_deref()
                .map(|s| serde_json::from_str(s).unwrap_or_default())
                .unwrap_or_default(),
            embedding: db.embedding,
            summary_text: db.summary_text,
            is_summary: db.is_summary,
            msg_type: db.msg_type,
            msg_subtype: db.msg_subtype,
            created_at: db
                .created_at
                .as_deref()
                .unwrap_or("")
                .parse::<DateTime<Utc>>()
                .map_err(|e| {
                    crate::error::Error::Message(format!(
                        "Invalid timestamp '{}': {}",
                        db.created_at.as_deref().unwrap_or("?"),
                        e
                    ))
                })?,
            iteration_number: db.iteration_number,
        })
    }
}

// ---------------------------------------------------------------------------
// Channel DB struct (for SELECT results) — unchanged
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelDb {
    pub id: i64,
    pub name: String,
    pub platform: Option<String>,
    pub resource_identifier: Option<String>,
    pub external_id: Option<String>,
    pub cause: String,
    pub current_profile: String,
    pub current_model: Option<String>,
    pub current_provider: Option<String>,
    pub readonly: bool,
    pub closed: Option<bool>,
    pub metadata: Option<String>,
    pub template: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl TryFrom<ChannelDb> for Channel {
    type Error = crate::error::Error;

    fn try_from(db: ChannelDb) -> Result<Self, Self::Error> {
        Ok(Self {
            id: db.id,
            name: db.name,
            platform: db.platform,
            resource_identifier: db.resource_identifier,
            external_id: db.external_id,
            cause: db.cause,
            current_profile: db.current_profile,
            current_model: db.current_model,
            current_provider: db.current_provider,
            readonly: db.readonly,
            closed: db.closed.unwrap_or(false),
            metadata: db
                .metadata
                .as_deref()
                .map(|s| serde_json::from_str(s).unwrap_or_default())
                .unwrap_or_default(),
            template: db.template.filter(|t| !t.is_empty()),
            created_at: db
                .created_at
                .as_deref()
                .unwrap_or("")
                .parse::<DateTime<Utc>>()
                .map_err(|e| {
                    crate::error::Error::Message(format!(
                        "Invalid timestamp '{}': {}",
                        db.created_at.as_deref().unwrap_or("?"),
                        e
                    ))
                })?,
            updated_at: db
                .updated_at
                .as_deref()
                .unwrap_or("")
                .parse::<DateTime<Utc>>()
                .map_err(|e| {
                    crate::error::Error::Message(format!(
                        "Invalid timestamp '{}': {}",
                        db.updated_at.as_deref().unwrap_or("?"),
                        e
                    ))
                })?,
        })
    }
}

// ---------------------------------------------------------------------------
// Summary DB struct (for SELECT results)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SummaryDb {
    pub id: i64,
    #[allow(dead_code)]
    pub channel_id: i64,
    pub next_thread_id: i64,
    pub content: String,
    #[allow(dead_code)]
    pub created_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Subscription DB struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SubscriptionDb {
    #[allow(dead_code)]
    pub id: i64,
    #[allow(dead_code)]
    pub channel_id: i64,
    pub subscriber_platform: String,
    pub subscriber_resource: String,
    #[allow(dead_code)]
    pub created_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Thread parameter structs
// ---------------------------------------------------------------------------

/// Parameters for [`create_thread`]. Collects all fields beyond
/// pool / cause / channel_id / profile into a single struct.
#[derive(Debug, Clone)]
pub struct CreateThreadParams {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub task_id: Option<String>,
    pub schedule_task_id: Option<String>,
    pub planning_mode: String,
    pub parent_id: Option<i64>,
}

/// Stats for completing a thread.
#[derive(Debug, Clone)]
pub struct CompleteThreadStats {
    #[allow(dead_code)]
    pub input_tokens: i32,
    #[allow(dead_code)]
    pub cached_tokens: i32,
    #[allow(dead_code)]
    pub output_tokens: i32,
    #[allow(dead_code)]
    pub duration_ms: i32,
}

/// Parameters for [`create_thread_with_cause`]. Collects all fields beyond
/// pool / cause / channel_id / profile into a single struct.
#[derive(Debug, Clone)]
pub struct ThreadCauseParams {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub task_id: Option<String>,
    pub schedule_task_id: Option<String>,
    pub content: String,
    pub external_id: Option<String>,
    pub parent_external_id: Option<String>,
    pub metadata: serde_json::Value,
    pub msg_type: String,
    pub msg_subtype: Option<String>,
    pub task_planning_mode: String,
}

// ---------------------------------------------------------------------------
// Channel parameter/helper structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CreateChannelParams {
    pub name: String,
    pub platform: String,
    pub external_id: String,
    pub cause: String,
    pub resource_identifier: String,
}

/// Old channel info returned by `update_channel_platform`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OldChannelInfo {
    pub old_platform: Option<String>,
    pub old_resource_identifier: Option<String>,
}

/// Status info for a channel: open/closed, thread counts, config.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelStatus {
    pub channel_id: i64,
    pub name: String,
    pub platform: String,
    pub closed: bool,
    pub current_profile: String,
    pub current_model: Option<String>,
    pub current_provider: Option<String>,
    pub pending_threads: i64,
    pub processing_threads: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelSeq0Message {
    pub id: i64,
    pub content: String,
    #[allow(dead_code)]
    pub role: String,
    #[allow(dead_code)]
    pub msg_type: String,
}

// ---------------------------------------------------------------------------
// Action DB struct and CRUD functions
// ---------------------------------------------------------------------------

/// Replica details from the messages table for the overview endpoint.
pub struct Channel {
    pub id: i64,
    pub name: String,
    /// Platform name ("telegram", "cli", etc.).  NULL means no-platform
    /// (e.g. cron/kanban channels that only exist for scheduling).
    pub platform: Option<String>,
    /// Identifier of the resource within the platform (chat_id, terminal
    /// session id, etc.).  NULL when there is no platform.
    pub resource_identifier: Option<String>,
    /// Legacy alias — kept for backward compatibility.  Same value as
    /// `resource_identifier` when platform is set.
    pub external_id: Option<String>,
    pub cause: String,
    pub current_profile: String,
    pub current_model: Option<String>,
    pub current_provider: Option<String>,
    pub readonly: bool,
    pub closed: bool,
    pub metadata: serde_json::Value,
    pub template: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            platform: None,
            resource_identifier: None,
            external_id: None,
            cause: String::new(),
            current_profile: String::new(),
            current_model: None,
            current_provider: None,
            readonly: false,
            closed: false,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            template: None,
            created_at: DateTime::from_timestamp(0, 0).unwrap_or(DateTime::UNIX_EPOCH),
            updated_at: DateTime::from_timestamp(0, 0).unwrap_or(DateTime::UNIX_EPOCH),
        }
    }
}

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
    pub iteration_number: i32,
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
    pub iteration_number: i32,
}

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
    pub task_id: Option<String>,
    pub schedule_task_id: Option<String>,
    pub planning_mode: String,
    pub parent_id: Option<i64>,
    pub iterations: i32,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadNew {
    pub cause: String,
    pub channel_id: i64,
    pub profile: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub task_id: Option<String>,
    pub schedule_task_id: Option<String>,
    pub planning_mode: String,
    pub parent_id: Option<i64>,
}

// ---------------------------------------------------------------------------
// Helper functions (non-DB, used by context_builder etc.)
// ---------------------------------------------------------------------------

/// Search wiki markdown files by text content.
pub fn search_wiki_text(
    wiki_dir: &str,
    query: &str,
    limit: usize,
) -> Vec<(String, String, String)> {
    use std::fs;
    let query_lower = query.to_lowercase();

    // Split query into individual search terms
    let terms: Vec<&str> = query_lower
        .split_whitespace()
        .filter(|t| t.len() > 2) // ignore very short words
        .collect();

    // If no meaningful terms, fall back to whole-query matching
    let use_terms = !terms.is_empty();
    let search_terms: Vec<&str> = if use_terms { terms } else { vec![&query_lower] };

    let mut scored: Vec<(String, String, usize, String)> = Vec::new();
    // results: Vec<(relative_path, title, max_score, Vec<(snippet_line, matching_term)>)>

    let walker = walkdir::WalkDir::new(wiki_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file() && e.path().extension().map(|ext| ext == "md").unwrap_or(false)
        });

    for entry in walker {
        let path = entry.path();
        let relative = path
            .strip_prefix(wiki_dir)
            .unwrap_or(path)
            .display()
            .to_string();
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let title = content
            .lines()
            .find(|l| l.starts_with("# "))
            .map(|l| l.trim_start_matches("# ").to_string())
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });
        // Count how many unique terms match at least one line
        let content_lower = content.to_lowercase();
        let match_count = search_terms
            .iter()
            .filter(|term| content_lower.contains(*term))
            .count();

        if match_count == 0 {
            continue;
        }

        // Find the best snippet (line with the most matching terms)
        let mut best_snippet = String::new();
        let mut best_snippet_score = 0usize;
        for line in content.lines() {
            let line_lower = line.to_lowercase();
            let line_score = search_terms
                .iter()
                .filter(|term| line_lower.contains(*term))
                .count();
            if line_score > best_snippet_score {
                best_snippet = line.trim().chars().take(200).collect();
                best_snippet_score = line_score;
            }
        }
        // Score = unique term matches + bonus for best snippet score
        let score = match_count * 100 + best_snippet_score;
        scored.push((relative, title, score, best_snippet));
    }

    // Sort by score descending, take top `limit`
    scored.sort_by_key(|b| std::cmp::Reverse(b.2));
    scored.truncate(limit);

    scored
        .into_iter()
        .map(|(path, title, _score, snippet)| (path, title, snippet))
        .collect()
}

/// Search wiki via Qdrant vector database.
pub async fn search_wiki_qdrant(
    qdrant_url: &str,
    embedding: &[f32],
    limit: usize,
) -> crate::error::AppResult<Vec<(String, String, f64)>> {
    use serde_json::json;

    let client = reqwest::Client::new();
    let payload = json!({
        "vector": embedding,
        "limit": limit as u64,
        "with_payload": true,
    });

    let resp = client
        .post(format!("{}/collections/wiki/points/search", qdrant_url))
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            crate::error::Error::Message(format!("Qdrant search request failed: {}", e))
        })?;

    let body: serde_json::Value = resp.json().await.map_err(|e| {
        crate::error::Error::Message(format!("Qdrant search response parse failed: {}", e))
    })?;

    let mut results = Vec::new();
    if let Some(points) = body["result"].as_array() {
        for point in points {
            let score = point["score"].as_f64().unwrap_or(0.0);
            let payload = &point["payload"];
            let path = payload["path"].as_str().unwrap_or("").to_string();
            let title = payload["title"].as_str().unwrap_or("").to_string();
            results.push((path, title, score));
        }
    }

    Ok(results)
}

// ---------------------------------------------------------------------------
// Re-exports from domain modules for backward compatibility
// All `use crate::db::types as queries;` imports continue to work because
// everything is re-exported here.
// ---------------------------------------------------------------------------

pub use crate::db::channels::*;
pub use crate::db::kanban::*;
pub use crate::db::messages::*;
pub use crate::db::subscriptions::*;
pub use crate::db::summaries::*;
pub use crate::db::threads::*;
