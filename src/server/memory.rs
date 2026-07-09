//! Memory API — provides stats and full-text search over stored conversations.
//!
//! Replaces the dashboard's /stats and /search-messages SQL queries so the
//! dashboard no longer needs direct database credentials.
//!
//! - `GET /memory/stats`   — aggregate counts (threads, messages, vectors)
//! - `GET /memory/search`  — full-text search over message content
//! - `GET /memory/text/{profile}/{type}`  — read MEMORY.md/USER.md file
//! - `POST /memory/upload/{profile}/{type}`  — write MEMORY.md/USER.md file

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use tokio::fs;
use tracing::error;

use super::{err_json, ok_json, AppState};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn memory_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/memory/stats", get(stats_handler))
        .route("/memory/search", get(search_handler))
        .route("/memory/text/{profile}/{type}", get(text_handler))
        .route("/memory/upload/{profile}/{type}", post(upload_handler))
}

// ---------------------------------------------------------------------------
// Types — Stats query / response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StatsQueryParams {
    pub profile: Option<String>,
    pub channel: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub threads: i64,
    pub threads_completed: i64,
    pub threads_failed: i64,
    pub messages: i64,
    pub vectors: i64,
}

// ---------------------------------------------------------------------------
// Types — Search query / response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchQueryParams {
    pub q: String,
    pub profile: Option<String>,
    pub channel: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct MessageSearchEntry {
    pub id: i64,
    pub thread_id: Option<i64>,
    pub role: String,
    pub content: Option<String>,
    pub thread_sequence: Option<i32>,
    pub external_id: Option<String>,
    pub metadata: Option<String>,
    pub created_at: String,
    pub msg_type: Option<String>,
    pub msg_subtype: Option<String>,
    pub channel_id: Option<i64>,
    pub status: Option<String>,
    pub profile: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thread_duration_ms: Option<i32>,
    pub thread_input_tokens: Option<i32>,
    pub thread_output_tokens: Option<i32>,
    pub thread_cached_tokens: Option<i32>,
    pub channel_name: Option<String>,
    pub processing_time_ms: Option<i32>,
    pub token_usage: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub messages: Vec<MessageSearchEntry>,
    pub total: usize,
}

// ---------------------------------------------------------------------------
// Types — Upload body
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UploadMemoryBody {
    pub content: String,
}

// ---------------------------------------------------------------------------
// Row types for sqlx (FROM clause -> struct mapping)
// ---------------------------------------------------------------------------

#[derive(FromRow)]
struct CountRow {
    cnt: Option<i64>,
}

#[derive(FromRow)]
struct SearchMessageRow {
    id: i64,
    thread_id: Option<i64>,
    role: String,
    content: Option<String>,
    thread_sequence: Option<i32>,
    external_id: Option<String>,
    metadata: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    msg_type: Option<String>,
    msg_subtype: Option<String>,
    channel_id: Option<i64>,
    status: Option<String>,
    profile: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    thread_duration_ms: Option<i32>,
    thread_input_tokens: Option<i32>,
    thread_output_tokens: Option<i32>,
    thread_cached_tokens: Option<i32>,
    channel_name: Option<String>,
    processing_time_ms: Option<i32>,
    token_usage: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fmt_ts(ts: &chrono::DateTime<chrono::Utc>) -> String {
    ts.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Parse a JSON string column into a Value, returning None for null/empty/invalid.
fn parse_token_usage(raw: Option<String>) -> Option<serde_json::Value> {
    let s = raw.as_deref()?;
    if s.is_empty() || s == "null" {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(s).ok()
}

// ---------------------------------------------------------------------------
// Handler: GET /memory/stats
// ---------------------------------------------------------------------------

async fn stats_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsQueryParams>,
) -> impl IntoResponse {
    let profile = params.profile.unwrap_or_default();
    let channel_id = params.channel.unwrap_or(0);

    // ── Threads count (all statuses) ──
    let threads = match sql_forge!(
        CountRow,
        r#"
        SELECT COUNT(*) as cnt
        FROM threads
        WHERE 1=1
          AND (:profile = '' OR profile = :profile)
          AND (:channel_id = 0::bigint OR channel_id = :channel_id)
        "#,
        ( :profile = &profile,
          :channel_id = channel_id )
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => row.cnt.unwrap_or(0),
        Err(e) => {
            error!("[memory/stats] threads count failed: {:?}", e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to count threads");
        }
    };

    // ── Completed threads ──
    let threads_completed = match sql_forge!(
        CountRow,
        r#"
        SELECT COUNT(*) as cnt
        FROM threads
        WHERE 1=1
          AND (:profile = '' OR profile = :profile)
          AND (:channel_id = 0::bigint OR channel_id = :channel_id)
          AND status = 'completed'
        "#,
        ( :profile = &profile,
          :channel_id = channel_id )
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => row.cnt.unwrap_or(0),
        Err(e) => {
            error!("[memory/stats] completed threads count failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to count completed threads",
            );
        }
    };

    // ── Failed threads ──
    let threads_failed = match sql_forge!(
        CountRow,
        r#"
        SELECT COUNT(*) as cnt
        FROM threads
        WHERE 1=1
          AND (:profile = '' OR profile = :profile)
          AND (:channel_id = 0::bigint OR channel_id = :channel_id)
          AND status = 'failed'
        "#,
        ( :profile = &profile,
          :channel_id = channel_id )
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => row.cnt.unwrap_or(0),
        Err(e) => {
            error!("[memory/stats] failed threads count failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to count failed threads",
            );
        }
    };

    // ── Messages count (via thread_id IN subquery) ──
    let messages = match sql_forge!(
        CountRow,
        r#"
        SELECT COUNT(*) as cnt
        FROM messages
        WHERE thread_id IN (
            SELECT id FROM threads
            WHERE 1=1
              AND (:profile = '' OR profile = :profile)
              AND (:channel_id = 0::bigint OR channel_id = :channel_id)
        )
        "#,
        ( :profile = &profile,
          :channel_id = channel_id )
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => row.cnt.unwrap_or(0),
        Err(e) => {
            error!("[memory/stats] messages count failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to count messages",
            );
        }
    };

    // ── Vectors count (messages with non-empty embedding) ──
    let vectors = match sql_forge!(
        CountRow,
        r#"
        SELECT COUNT(*) as cnt
        FROM messages
        WHERE embedding IS NOT NULL AND embedding != ''
          AND thread_id IN (
            SELECT id FROM threads
            WHERE 1=1
              AND (:profile = '' OR profile = :profile)
              AND (:channel_id = 0::bigint OR channel_id = :channel_id)
        )
        "#,
        ( :profile = &profile,
          :channel_id = channel_id )
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => row.cnt.unwrap_or(0),
        Err(e) => {
            error!("[memory/stats] vectors count failed: {:?}", e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to count vectors");
        }
    };

    ok_json(StatsResponse {
        threads,
        threads_completed,
        threads_failed,
        messages,
        vectors,
    })
}

// ---------------------------------------------------------------------------
// Handler: GET /memory/search
// ---------------------------------------------------------------------------

async fn search_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchQueryParams>,
) -> impl IntoResponse {
    let q = params.q.trim().to_string();
    if q.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "Query parameter 'q' is required");
    }

    let profile = params.profile.unwrap_or_default();
    let channel_id = params.channel.unwrap_or(0);
    let limit = params.limit.unwrap_or(10).clamp(1, 500);
    let pattern = format!("%{}%", q);

    let rows = match sql_forge!(
        SearchMessageRow,
        r#"
        SELECT
            m.id,
            m.thread_id,
            m.role,
            m.content,
            m.thread_sequence,
            m.external_id,
            m.metadata::text AS metadata,
            m.created_at,
            m.msg_type,
            m.msg_subtype,
            t.channel_id,
            t.status,
            t.profile,
            t.provider,
            t.model,
            t.duration_ms AS thread_duration_ms,
            t.input_tokens AS thread_input_tokens,
            t.output_tokens AS thread_output_tokens,
            t.cached_tokens AS thread_cached_tokens,
            c.name AS channel_name,
            t.duration_ms AS processing_time_ms,
            jsonb_build_object(
                'input_tokens', t.input_tokens,
                'output_tokens', t.output_tokens,
                'cached_tokens', t.cached_tokens
            )::text AS token_usage
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        JOIN channels c ON c.id = t.channel_id
        WHERE m.content ILIKE :pattern
          AND (:profile = '' OR t.profile = :profile)
          AND (:channel_id = 0::bigint OR t.channel_id = :channel_id)
        ORDER BY m.id DESC
        LIMIT :limit
        "#,
        ( :pattern = &pattern,
          :profile = &profile,
          :channel_id = channel_id,
          :limit = limit )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[memory/search] query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to search messages",
            );
        }
    };

    let messages: Vec<MessageSearchEntry> = rows
        .into_iter()
        .map(|r| MessageSearchEntry {
            id: r.id,
            thread_id: r.thread_id,
            role: r.role,
            content: r.content,
            thread_sequence: r.thread_sequence,
            external_id: r.external_id,
            metadata: r.metadata,
            created_at: fmt_ts(&r.created_at),
            msg_type: r.msg_type,
            msg_subtype: r.msg_subtype,
            channel_id: r.channel_id,
            status: r.status,
            profile: r.profile,
            provider: r.provider,
            model: r.model,
            thread_duration_ms: r.thread_duration_ms,
            thread_input_tokens: r.thread_input_tokens,
            thread_output_tokens: r.thread_output_tokens,
            thread_cached_tokens: r.thread_cached_tokens,
            channel_name: r.channel_name,
            processing_time_ms: r.processing_time_ms,
            token_usage: parse_token_usage(r.token_usage),
        })
        .collect();

    let total = messages.len();

    ok_json(SearchResponse { messages, total })
}

// ---------------------------------------------------------------------------
// Helper: resolve memory file path
// ---------------------------------------------------------------------------

/// Resolve the file path for a memory type (memory → MEMORY.md, soul → USER.md).
/// First tries profile-specific path, then falls back to global memories directory.
fn resolve_memory_path(data_dir: &str, profile: &str, mem_type: &str) -> Result<String, String> {
    let file_name = match mem_type {
        "memory" => "MEMORY.md",
        "soul" => "USER.md",
        _ => {
            return Err(format!(
                "Type must be 'memory' or 'soul', got '{}'",
                mem_type
            ))
        }
    };

    let profile_path = format!("{}/profiles/{}/memories/{}", data_dir, profile, file_name);
    let global_path = format!("{}/memories/{}", data_dir, file_name);

    // Check profile-specific path first
    if std::path::Path::new(&profile_path).exists() {
        Ok(profile_path)
    } else if std::path::Path::new(&global_path).exists() {
        Ok(global_path)
    } else {
        // Default to profile path even if it doesn't exist (for upload)
        Ok(profile_path)
    }
}

// ---------------------------------------------------------------------------
// Handler: GET /memory/text/{profile}/{type}
// ---------------------------------------------------------------------------

/// Read the MEMORY.md or USER.md file for a given profile.
/// Falls back to global memories directory if profile-specific file doesn't exist.
async fn text_handler(
    State(state): State<Arc<AppState>>,
    Path((profile, mem_type)): Path<(String, String)>,
) -> impl IntoResponse {
    let file_path = match resolve_memory_path(&state.data_dir, &profile, &mem_type) {
        Ok(p) => p,
        Err(e) => return err_json(StatusCode::BAD_REQUEST, &e),
    };

    match fs::read_to_string(&file_path).await {
        Ok(content) => ok_json(serde_json::json!({ "content": content })),
        Err(e) => {
            error!(
                "[memory/text] failed to read {} for profile '{}': {:?}",
                file_path, profile, e
            );
            err_json(
                StatusCode::NOT_FOUND,
                &format!("{} file not found for profile '{}'", mem_type, profile),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Handler: POST /memory/upload/{profile}/{type}
// ---------------------------------------------------------------------------

/// Upload content to MEMORY.md or USER.md for a given profile.
/// Accepts JSON: { "content": "..." }
async fn upload_handler(
    State(state): State<Arc<AppState>>,
    Path((profile, mem_type)): Path<(String, String)>,
    Json(body): Json<UploadMemoryBody>,
) -> impl IntoResponse {
    let file_name = match mem_type.as_str() {
        "memory" => "MEMORY.md",
        "soul" => "USER.md",
        _ => {
            return err_json(
                StatusCode::BAD_REQUEST,
                &format!("Type must be 'memory' or 'soul', got '{}'", mem_type),
            )
        }
    };

    let dest_dir = format!("{}/profiles/{}/memories", state.data_dir, profile);
    let dest_path = format!("{}/{}", dest_dir, file_name);

    // Ensure directory exists
    if let Err(e) = fs::create_dir_all(&dest_dir).await {
        error!(
            "[memory/upload] failed to create directory {}: {:?}",
            dest_dir, e
        );
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create memories directory",
        );
    }

    // Write the file
    match fs::write(&dest_path, body.content.as_bytes()).await {
        Ok(_) => {
            let size = body.content.len();
            ok_json(serde_json::json!({ "success": true, "size": size, "path": dest_path }))
        }
        Err(e) => {
            error!("[memory/upload] failed to write {}: {:?}", dest_path, e);
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to write memory file",
            )
        }
    }
}
