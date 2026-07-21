//! mcp-server-query: standalone MCP server for read-only database queries.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Provides a single tool `query_database` with 4 operations:
//! - search_messages: vector-embedding semantic search
//! - search_thread_messages: all messages from a thread
//! - search_channel_prompts: all seq-0 (prompt) messages from a channel
//! - query: direct SELECT SQL
//!
//! All operations use a read-only PostgreSQL user. Writes are blocked at the DB level.

use anyhow::{Context, Result};
use mcp_server_util::*;
use omniagent::db;
use mcp_server_util::{vector_to_string, HashVectorizer};
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::{Column, FromRow, PgPool, Row};
use std::sync::Arc;

// ── Result structs ─────────────────────────────────────────────────────────

#[derive(Debug, FromRow)]
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

// ── Operations ─────────────────────────────────────────────────────────────

/// search_messages: semantic (vector embedding) search with optional channel filter.
async fn handle_search_messages(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let query_text = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("'query' is required for search_messages"))?
        .to_string();
    let channel_id = args["channel_id"].as_i64();
    let limit = args["limit"].as_i64().unwrap_or(10).min(50);

    let hash_vec = HashVectorizer;

    let rows: Vec<MessageResult> = {
        let embedding = hash_vec.generate_embedding(&query_text).await;
        let emb_str = vector_to_string(&embedding);
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
            .fetch_all(pool)
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
            .fetch_all(pool)
            .await
            .map_err(|e| anyhow::anyhow!(e))
        }
    }?;

    Ok(format_results("search_messages", &rows, rows.len() as i64))
}

/// search_thread_messages: all messages from a thread (sql_forge! validated).
async fn handle_search_thread_messages(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let thread_id = args["thread_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("'thread_id' is required for search_thread_messages"))?;
    let limit = args["limit"].as_i64().unwrap_or(100).min(200);

    let rows: Vec<MessageResult> = {
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
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!(e))
    }?;

    Ok(format_results(
        "search_thread_messages",
        &rows,
        rows.len() as i64,
    ))
}

/// search_channel_prompts: all seq-0 (prompt) messages from a channel (sql_forge!).
async fn handle_search_channel_prompts(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let channel_id = args["channel_id"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("'channel_id' is required for search_channel_prompts"))?;
    let limit = args["limit"].as_i64().unwrap_or(10).min(50);

    let results: Vec<MessageResult> = {
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
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!(e))
    }?;

    Ok(format_results(
        "search_channel_prompts",
        &results,
        results.len() as i64,
    ))
}

/// query: direct SQL (runtime only, must be SELECT).
async fn handle_query(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
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

    let results: Vec<serde_json::Value> = {
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql_owned.as_str()))
            .fetch_all(pool)
            .await
            .map_err(|e| anyhow::anyhow!("Query failed: {e}"))?;

        let mut json_rows: Vec<serde_json::Value> = Vec::new();
        for row in &rows {
            let mut map = serde_json::Map::new();
            for (i, col) in row.columns().iter().enumerate() {
                let name = col.name();
                let value: serde_json::Value = if let Ok(s) = row.try_get::<&str, _>(i) {
                    serde_json::Value::String(s.to_string())
                } else if let Ok(n) = row.try_get::<i64, _>(i) {
                    serde_json::json!(n)
                } else if let Ok(n) = row.try_get::<f64, _>(i) {
                    serde_json::json!(n)
                } else if let Ok(b) = row.try_get::<bool, _>(i) {
                    serde_json::json!(b)
                } else {
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
        json_rows
    };

    let output = serde_json::to_string_pretty(&results)?;
    Ok((output, false))
}

// ── Dispatch ───────────────────────────────────────────────────────────────

async fn handle_query_database(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let operation = args["operation"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'operation' argument"))?;

    match operation {
        "search_messages" => handle_search_messages(pool, args).await,
        "search_thread_messages" => handle_search_thread_messages(pool, args).await,
        "search_channel_prompts" => handle_search_channel_prompts(pool, args).await,
        "query" => handle_query(pool, args).await,
        other => anyhow::bail!("Unknown operation: '{}'", other),
    }
}

// ── Formatting ─────────────────────────────────────────────────────────────

/// Format a list of MessageResult into a readable string.
fn format_results(operation: &str, results: &[MessageResult], total_count: i64) -> (String, bool) {
    if results.is_empty() {
        return (format!("[{}] No results found.", operation), false);
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
        let preview = if r.content.len() > 300 {
            let truncate_to = r
                .content
                .char_indices()
                .nth(300)
                .map(|(i, _)| i)
                .unwrap_or(r.content.len());
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

        let created_display = r
            .created_at
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|t| format!(" @{}", t))
            .unwrap_or_default();

        lines.push(format!(
            "#{} [{}]{} {}{}: {}",
            r.id, r.role, type_info, thread_info, created_display, preview
        ));
    }

    let output = lines.join("\n");
    (output, false)
}

// ── Main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = std::env::var("QUERY_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .context("QUERY_DATABASE_URL or DATABASE_URL must be set")?;
    let pool = db::connect(&database_url)
        .await
        .context("Failed to connect to database")?;
    let pool = Arc::new(pool);

    let p_query = pool.clone();
    let query_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_query.clone();
        Box::pin(async move { handle_query_database(&p, &args).await })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "query_database".to_string(),
                description: "QUERY THE DATABASE with one of four operations. \
All operations run against a read-only PostgreSQL user: writes are blocked at the database level.

Operations:
- **search_messages** (runtime sqlx: pgvector <=>): Semantic (vector embedding) search on message content. \
Parameters: query (required), channel_id (optional), limit (default 10, max 50).

- **search_thread_messages** (sql_forge validated): All messages from a thread, ordered by \
thread_sequence ASC. Parameters: thread_id (required), limit (optional, default 100, max 200). \
Returns the prompt (seq=0) + up to N-1 subsequent messages.

- **search_channel_prompts** (sql_forge validated): All seq-0 (prompt) messages from \
a channel, newest first. Parameters: channel_id (required), limit (optional, default 10, max 50).

- **query** (runtime only: for any custom SELECT): Run any SELECT SQL. \
Parameters: sql (required): must be a SELECT statement. INSERT/UPDATE/DELETE/DROP/ALTER \
are rejected by the read-only database user. Use this for custom aggregations: \
COUNT(*), GROUP BY, SUM, JOIN across tables. \
You MUST include the full schema reference in your query. Available tables: \
messages, channels, channel_stops, summaries, kanban_tasks, cron_jobs, profiles.

Database schema is documented in the tool description for reference.".to_string(),
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
            },
            handler: query_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-query".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server(server_info, tools).await
}
