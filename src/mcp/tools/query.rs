//! Read-only database query tool for OmniAgent.
//!
//! Provides four operations via a single MCP tool:
//! - `search_messages` — ILIKE text search with optional channel filter (sql_forge! validated)
//! - `search_thread_messages` — all messages from a thread (runtime sqlx)
//! - `search_channel_prompts` — all seq-0 (prompt) messages from a channel (sql_forge! validated)
//! - `query` — direct SQL query (runtime sqlx; writes blocked by read-only DB user)
//!
//! All operations use a read-only PostgreSQL user (`omniagent_readonly`). Any
//! INSERT/UPDATE/DELETE/ALTER/DROP statement is rejected at the database level.
//!
//! # Database Schema
//!
//! ## channels
//! | Column | Type | Description |
//! |--------|------|-------------|
//! | id | BIGSERIAL PK | Auto-incrementing channel ID |
//! | name | TEXT NOT NULL | Channel name (e.g. "user-lucas", "cron-daily-backup") |
//! | platform | TEXT NOT NULL | Platform type ("telegram", "cron", "cli", etc.) |
//! | external_id | TEXT NOT NULL | Platform-specific identifier |
//! | cause | TEXT NOT NULL | How created: "user" or "cron" |
//! | current_profile | TEXT | Active profile for this channel |
//! | current_model | TEXT | Model override |
//! | current_provider | TEXT | Provider override |
//! | metadata | JSONB | Arbitrary metadata |
//! | created_at | TIMESTAMPTZ | When created |
//! | updated_at | TIMESTAMPTZ | When updated |
//!
//! ## messages
//! | Column | Type | Description |
//! |--------|------|-------------|
//! | id | BIGSERIAL PK | Auto-incrementing message ID |
//! | channel_id | BIGINT FK | Channel this message belongs to |
//! | role | TEXT | "user", "agent", "system" |
//! | content | TEXT | Message body |
//! | status | TEXT | "pending", "processing", "completed", "failed", "skipped" |
//! | thread_id | BIGINT | Groups related messages; NULL for seq-0 until normalized |
//! | thread_sequence | INT | Order within thread (0 = prompt/root) |
//! | external_id | TEXT | Platform-specific message ID |
//! | metadata | JSONB | Arbitrary metadata |
//! | embedding | TEXT | Vector embedding (cast to vector(1536) for pgvector ops) |
//! | summary_text | TEXT | Cached summary |
//! | is_summary | BOOL | Whether this is a summary record |
//! | msg_type | TEXT | "message", "reasoning", "tool", "tool_result", "summary" |
//! | msg_subtype | TEXT | Tool name for tool types, NULL otherwise |
//! | iteration_count | INT | Which agent turn in the thread |
//! | profile | TEXT | Profile used for processing this message |
//! | provider | TEXT | LLM provider used |
//! | model | TEXT | LLM model used |
//! | processing_time_ms | INT | Milliseconds spent processing |
//! | token_usage | JSONB | Token usage breakdown |
//! | iterations | INT | Total LLM calls for this processing run |
//! | created_at | TIMESTAMPTZ | When created |
//!
//! ## channel_stops
//! | Column | Type | Description |
//! |--------|------|-------------|
//! | id | BIGSERIAL PK | Auto-incrementing |
//! | channel_id | BIGINT FK | Channel that was stopped |
//! | stopped_at | TIMESTAMPTZ | When it was stopped |
//!
//! ## summaries
//! | Column | Type | Description |
//! |--------|------|-------------|
//! | id | BIGSERIAL PK | Auto-incrementing |
//! | channel_id | BIGINT FK | Channel this summary belongs to |
//! | next_thread_id | BIGINT | Pivot thread ID for window sliding |
//! | content | TEXT | Summary content |
//! | created_at | TIMESTAMPTZ | When created |
//!
//! ## kanban_tasks, cron_jobs, profiles
//! See the project's schema for these auxiliary tables.

use crate::mcp::{truncate_content, AppContext, McpTool, McpToolResult, DEFAULT_MAX_TOOL_OUTPUT_CHARS};
use anyhow::Result;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::Column;
use sqlx::FromRow;
use sqlx::Row;
use std::sync::Arc;
use crate::vectorizer::{HashVectorizer, Vectorizer, vector_to_string};

// ── Result structs ─────────────────────────────────────────────────────────

#[derive(Debug, FromRow)]
#[allow(dead_code)]
struct MessageResult {
    id: i64,
    role: String,
    content: String,
    msg_type: String,
    msg_subtype: Option<String>,
    thread_id: Option<i64>,
    thread_sequence: i32,
    created_at: Option<String>,
}

// ── Tool factory ───────────────────────────────────────────────────────────

pub fn query_database_tool(ctx: &AppContext) -> McpTool {
    let pool = ctx.readonly_pool.clone();

    McpTool {
        name: "query_database".to_string(),
        description: "QUERY THE DATABASE with one of four operations. \
All operations run against a read-only PostgreSQL user — writes are blocked at the database level.

Operations:
- **search_messages** (sql_forge validated): Full-text ILIKE search on message content. \
Parameters: query (required), channel_id (optional), limit (default 10, max 50). \
Example SQL used internally: SELECT id, role, content, msg_type, msg_subtype, \
channel_id, thread_id, thread_sequence, profile, TO_CHAR(created_at, ...) FROM messages \
WHERE content ILIKE :pattern [AND channel_id = :channel_id] ORDER BY created_at DESC LIMIT :limit.

- **search_thread_messages** (runtime only): All messages from a thread, ordered by \
thread_sequence ASC. Parameters: thread_id (required), limit (optional, default 100, max 200). \
Returns the prompt (seq=0) + up to N-1 subsequent messages.

- **search_channel_prompts** (sql_forge validated): All seq-0 (prompt) messages from \
a channel, newest first. Parameters: channel_id (required), limit (optional, default 10, max 50). \
Example SQL: SELECT id, role, content, msg_type, channel_id, thread_id, profile, \
TO_CHAR(created_at, ...) FROM messages WHERE channel_id = :channel_id AND \
thread_sequence = 0 ORDER BY id DESC LIMIT :limit.

- **query** (runtime only — for any custom SELECT): Run any SELECT SQL. \
Parameters: sql (required) — must be a SELECT statement. INSERT/UPDATE/DELETE/DROP/ALTER \
are rejected by the read-only database user. Use this for custom aggregations: \
COUNT(*), GROUP BY, SUM, JOIN across tables. \
You MUST include the full schema reference in your query. Available tables: \
messages, channels, channel_stops, summaries, kanban_tasks, cron_jobs, profiles.

Database schema is documented in the tool description above for reference.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["search_messages", "search_thread_messages", "search_channel_prompts", "query"],
                    "description": "Which database operation to perform"
                },
                "query": {
                    "type": "string",
                    "description": "Search text (for search_messages) or raw SQL (for query operation)"
                },
                "sql": {
                    "type": "string",
                    "description": "Raw SELECT SQL (only for operation='query')"
                },
                "channel_id": {
                    "type": "integer",
                    "description": "Channel ID filter"
                },
                "thread_id": {
                    "type": "integer",
                    "description": "Thread ID (for search_thread_messages)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (default varies by operation)",
                    "default": 10
                }
            },
            "required": ["operation"]
        }),
        handler: Arc::new(move |args: Value, _ctx: AppContext| -> Result<McpToolResult> {
            let operation = args["operation"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing 'operation' argument"))?;

            let pool = pool.clone();

            match operation {
                "search_messages" => handle_search_messages(pool.clone(), &args),
                "search_thread_messages" => handle_search_thread_messages(pool.clone(), &args),
                "search_channel_prompts" => handle_search_channel_prompts(pool.clone(), &args),
                "query" => handle_query(pool.clone(), &args),
                other => anyhow::bail!("Unknown operation: '{}'", other),
            }
        }),
    }
}

// ── Handlers ───────────────────────────────────────────────────────────────

/// search_messages — semantic (vector embedding) search with optional channel filter (runtime sqlx — pgvector <=>).
fn handle_search_messages(pool: sqlx::PgPool, args: &Value) -> Result<McpToolResult> {
    let query_text = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("'query' is required for search_messages"))?
        .to_string();
    let channel_id = args["channel_id"].as_i64();
    let limit = args["limit"].as_i64().unwrap_or(10).min(50);

    // Generate a hash-based embedding from the query text for vector similarity
    let hash_vec = HashVectorizer;

    let rows: Vec<MessageResult> = tokio::task::block_in_place(|| {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(async {
            let embedding = hash_vec.generate_embedding(&query_text).await;
            let emb_str = vector_to_string(&embedding);
            // Runtime-only sqlx::query_as — pgvector <=> operator not supported by sqlx compile-time macros
            // Two-stage decay: HNSW-indexed ANN search → recency re-rank
            // Fallback to TEXT cast when embedding_vec column doesn't exist
            let has_vec_column: bool = sqlx::query_scalar(
                r#"SELECT EXISTS (
                    SELECT 1 FROM information_schema.columns
                    WHERE table_name = 'messages' AND column_name = 'embedding_vec'
                )"#,
            )
            .fetch_one(&pool)
            .await
            .unwrap_or(false);

            if has_vec_column {
                if let Some(cid) = channel_id {
                    sqlx::query_as::<_, MessageResult>(
                        r#"
                        WITH vector_candidates AS (
                            SELECT m.id, m.created_at,
                                   (m.embedding_vec <=> $2::vector(1536)) AS distance_raw
                            FROM messages m
                            JOIN threads t ON t.id = m.thread_id
                            WHERE t.channel_id = $1
                              AND m.embedding_vec IS NOT NULL
                              AND m.role IN ('user', 'agent')
                            ORDER BY m.embedding_vec <=> $2::vector(1536)
                            LIMIT 100
                        )
                        SELECT
                            m.id, m.role, m.content, m.msg_type, m.msg_subtype,
                            m.thread_id, m.thread_sequence,
                            COALESCE(TO_CHAR(m.created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
                        FROM messages m
                        JOIN vector_candidates vc ON vc.id = m.id
                        ORDER BY vc.distance_raw * (1 + EXTRACT(EPOCH FROM (NOW() - vc.created_at)) / 86400)
                        LIMIT $3
                        "#,
                    )
                    .bind(cid)
                    .bind(&emb_str)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
                } else {
                    sqlx::query_as::<_, MessageResult>(
                        r#"
                        WITH vector_candidates AS (
                            SELECT m.id, m.created_at,
                                   (m.embedding_vec <=> $1::vector(1536)) AS distance_raw
                            FROM messages m
                            WHERE m.embedding_vec IS NOT NULL
                              AND m.role IN ('user', 'agent')
                            ORDER BY m.embedding_vec <=> $1::vector(1536)
                            LIMIT 100
                        )
                        SELECT
                            m.id, m.role, m.content, m.msg_type, m.msg_subtype,
                            m.thread_id, m.thread_sequence,
                            COALESCE(TO_CHAR(m.created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
                        FROM messages m
                        JOIN vector_candidates vc ON vc.id = m.id
                        ORDER BY vc.distance_raw * (1 + EXTRACT(EPOCH FROM (NOW() - vc.created_at)) / 86400)
                        LIMIT $2
                        "#,
                    )
                    .bind(&emb_str)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
                }
            } else {
                // Fallback: TEXT cast approach (pgvector extension not available)
                if let Some(cid) = channel_id {
                    sqlx::query_as::<_, MessageResult>(
                        r#"
                        SELECT
                            m.id, m.role, m.content, m.msg_type, m.msg_subtype,
                            m.thread_id, m.thread_sequence,
                            COALESCE(TO_CHAR(m.created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
                        FROM messages m
                        JOIN threads t ON t.id = m.thread_id
                        WHERE t.channel_id = $1
                          AND m.embedding IS NOT NULL
                          AND m.embedding != ''
                          AND m.role IN ('user', 'agent')
                        ORDER BY m.embedding::vector(1536) <=> $2::vector(1536)
                        LIMIT $3
                        "#,
                    )
                    .bind(cid)
                    .bind(&emb_str)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
                } else {
                    sqlx::query_as::<_, MessageResult>(
                        r#"
                        SELECT
                            m.id, m.role, m.content, m.msg_type, m.msg_subtype,
                            m.thread_id, m.thread_sequence,
                            COALESCE(TO_CHAR(m.created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
                        FROM messages m
                        WHERE m.embedding IS NOT NULL
                          AND m.embedding != ''
                          AND m.role IN ('user', 'agent')
                        ORDER BY m.embedding::vector(1536) <=> $1::vector(1536)
                        LIMIT $2
                        "#,
                    )
                    .bind(&emb_str)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
                }
            }
        })
    })?;

    format_results("search_messages", &rows, rows.len() as i64)
}

/// search_thread_messages — all messages from a thread (sql_forge! validated).
fn handle_search_thread_messages(pool: sqlx::PgPool, args: &Value) -> Result<McpToolResult> {
    let thread_id = args["thread_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("'thread_id' is required for search_thread_messages"))?;
    let limit = args["limit"].as_i64().unwrap_or(100).min(200);

    let rows: Vec<MessageResult> = tokio::task::block_in_place(|| {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(async {
            sql_forge!(
                MessageResult,
                r#"
                SELECT
                    id, role, content, msg_type, msg_subtype,
                    thread_id, thread_sequence,
                    COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
                FROM messages
                WHERE thread_id = :thread_id
                ORDER BY thread_sequence ASC, created_at ASC
                LIMIT :limit
                "#,
                ( :thread_id = thread_id, :limit = limit )
            )
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!(e))
        })
    })?;

    format_results("search_thread_messages", &rows, rows.len() as i64)
}

/// search_channel_prompts — all seq-0 (prompt) messages from a channel (sql_forge!).
fn handle_search_channel_prompts(pool: sqlx::PgPool, args: &Value) -> Result<McpToolResult> {
    let channel_id = args["channel_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("'channel_id' is required for search_channel_prompts"))?;
    let limit = args["limit"].as_i64().unwrap_or(10).min(50);

    let results: Vec<MessageResult> = tokio::task::block_in_place(|| {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(async {
            sql_forge!(
                MessageResult,
                r#"
                SELECT
                    m.id, m.role, m.content, m.msg_type, m.msg_subtype,
                    m.thread_id, m.thread_sequence,
                    COALESCE(TO_CHAR(m.created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
                FROM messages m
                JOIN threads t ON t.id = m.thread_id
                WHERE t.channel_id = :channel_id
                  AND m.thread_sequence = 0
                ORDER BY id DESC
                LIMIT :limit
                "#,
                ( :channel_id = channel_id, :limit = limit )
            )
            .fetch_all(&pool)
            .await
            .map_err(|e| anyhow::anyhow!(e))
        })
    })?;

    format_results("search_channel_prompts", &results, results.len() as i64)
}

/// query — direct SQL (runtime only, must be SELECT).
fn handle_query(pool: sqlx::PgPool, args: &Value) -> Result<McpToolResult> {
    let sql_owned = args["sql"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("'sql' is required for query operation"))?
        .to_string();

    // Basic safety: reject non-SELECT statements at the application level
    let trimmed = sql_owned.trim().to_uppercase();
    if !trimmed.starts_with("SELECT") && !trimmed.starts_with("WITH") {
        anyhow::bail!(
            "Only SELECT (or WITH) statements are allowed. \
             INSERT/UPDATE/DELETE/DROP/ALTER are rejected by the read-only database user."
        );
    }

    let results: Vec<serde_json::Value> = tokio::task::block_in_place(|| {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(async {
            // Runtime-only sqlx::query — dynamic SQL cannot use sql_forge!
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql_owned.as_str()))
                .fetch_all(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("Query failed: {e}"))?;

            // Convert each row to a JSON value for the output
            let mut json_rows: Vec<serde_json::Value> = Vec::new();
            for row in &rows {
                let mut map = serde_json::Map::new();
                for (i, col) in row.columns().iter().enumerate() {
                    let name = col.name();
                    // Try types in priority order: String, i64, f64, bool, then null
                    let value: serde_json::Value = if let Ok(s) = row.try_get::<&str, _>(i) {
                        serde_json::Value::String(s.to_string())
                    } else if let Ok(n) = row.try_get::<i64, _>(i) {
                        serde_json::json!(n)
                    } else if let Ok(n) = row.try_get::<f64, _>(i) {
                        serde_json::json!(n)
                    } else if let Ok(b) = row.try_get::<bool, _>(i) {
                        serde_json::json!(b)
                    } else {
                        // Check if the column is truly NULL
                        row.try_get::<Option<String>, _>(i)
                            .ok()
                            .flatten()
                            .map(serde_json::Value::String)
                            .unwrap_or(serde_json::Value::Null)
                    };
                    map.insert(name.to_string(), value);
                }
                json_rows.push(serde_json::Value::Object(map));
            }
            Ok::<_, anyhow::Error>(json_rows)
        })
    })?;

    let output = serde_json::to_string_pretty(&results)?;
    Ok(McpToolResult {
        call_id: String::new(),
        content: truncate_content(&output, DEFAULT_MAX_TOOL_OUTPUT_CHARS),
        is_error: false,
    })
}

// ── Formatting ─────────────────────────────────────────────────────────────

/// Format a list of MessageResult into a readable string.
fn format_results(operation: &str, results: &[MessageResult], total_count: i64) -> Result<McpToolResult> {
    if results.is_empty() {
        return Ok(McpToolResult {
            call_id: String::new(),
            content: format!("[{}] No results found.", operation),
            is_error: false,
        });
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "[{}] {} result(s) (showing {}):",
        operation,
        total_count,
        results.len()
    ));
    lines.push(String::new());

    for r in results {
        // Format each message with key fields
        let preview = if r.content.len() > 300 {
            let truncate_to = r.content.char_indices().nth(300).map(|(i, _)| i).unwrap_or(r.content.len());
            format!("{}...", &r.content[..truncate_to])
        } else {
            r.content.clone()
        };

        let thread_info = match (r.thread_id, r.thread_sequence) {
            (Some(tid), seq) => format!(" thread={} seq={}", tid, seq),
            (None, 0) => " root".to_string(),
            (None, seq) => format!(" seq={}", seq),
        };

        let type_info = match r.msg_subtype.as_deref() {
            Some(sub) if r.msg_type == "tool" => format!(" [tool:{}]", sub),
            Some(sub) if r.msg_type == "tool_result" => format!(" [result:{}]", sub),
            _ if r.msg_type == "reasoning" => " [reasoning]".to_string(),
            _ if r.msg_type == "summary" => " [summary]".to_string(),
            _ => String::new(),
        };

        lines.push(format!(
            "#{} [{}]{} {}{}: {}",
            r.id, r.role, type_info, thread_info,
            r.createdat_ref().map(|t| format!(" @{}", t)).unwrap_or_default(),
            preview
        ));
    }

    let output = lines.join("\n");
    Ok(McpToolResult {
        call_id: String::new(),
        content: truncate_content(&output, DEFAULT_MAX_TOOL_OUTPUT_CHARS),
        is_error: false,
    })
}

/// Helper trait to extract created_at display text from MessageResult.
trait CreatedAtDisplay {
    fn createdat_ref(&self) -> Option<&str>;
}

impl CreatedAtDisplay for MessageResult {
    fn createdat_ref(&self) -> Option<&str> {
        self.created_at.as_deref().filter(|s| !s.is_empty())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_result_created_at() {
        let msg = MessageResult {
            id: 1,
            role: "user".to_string(),
            content: "hello".to_string(),
            msg_type: "message".to_string(),
            msg_subtype: None,
            thread_id: Some(1),
            thread_sequence: 0,
            created_at: Some("2026-06-18T12:00:00Z".to_string()),
        };
        assert_eq!(msg.createdat_ref(), Some("2026-06-18T12:00:00Z"));

        let empty = MessageResult {
            created_at: Some(String::new()),
            ..msg
        };
        assert!(empty.createdat_ref().is_none());
    }
}
