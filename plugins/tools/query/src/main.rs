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

use anyhow::Result;
use mcp_server_util::*;
use mcp_server_util::{vector_to_string, HashVectorizer};
use omniagent::db;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::types::chrono::{DateTime, NaiveDate, Utc};
use sqlx::types::Uuid;
use sqlx::{Column, FromRow, PgPool, Row, TypeInfo};
use std::sync::Arc;
use tokio::sync::RwLock;

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

/// Write/DDL SQL keywords that are forbidden in read-only queries. Matching is
/// done on whole tokens after stripping comments and string literals.
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT",
    "UPDATE",
    "DELETE",
    "DROP",
    "ALTER",
    "CREATE",
    "TRUNCATE",
    "GRANT",
    "REVOKE",
    "MERGE",
    "CALL",
    "COPY",
    "LOCK",
    "COMMENT",
    "SET",
    "RESET",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "END",
    "DO",
    "VACUUM",
    "ANALYZE",
    "REINDEX",
    "CLUSTER",
    "NOTIFY",
    "LISTEN",
    "UNLISTEN",
    "PREPARE",
    "EXECUTE",
    "DEALLOCATE",
    "SECURITY",
    "IMPORT",
    "REFRESH",
    "DISCARD",
    "CHECKPOINT",
    "DECLARE",
    "FETCH",
    "MOVE",
    "CLOSE",
    "OPEN",
    "LOAD",
];

/// Strips SQL comments and string/identifier literals, replacing their contents
/// with spaces so keywords inside them can't create false positives (or hide).
fn strip_sql_literals_and_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while i < bytes.len() {
        if !in_line_comment
            && !in_block_comment
            && bytes[i] == b'-'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'-'
        {
            in_line_comment = true;
            out.extend_from_slice(b"  ");
            i += 2;
            continue;
        }
        if !in_line_comment
            && !in_block_comment
            && bytes[i] == b'/'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'*'
        {
            in_block_comment = true;
            out.extend_from_slice(b"  ");
            i += 2;
            continue;
        }
        if in_line_comment {
            if bytes[i] == b'\n' {
                in_line_comment = false;
                out.push(b'\n');
            } else {
                out.push(b' ');
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                out.extend_from_slice(b"  ");
                i += 2;
            } else {
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'\'' {
            out.push(b' ');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        out.extend_from_slice(b"  ");
                        i += 2;
                        continue;
                    }
                    out.push(b' ');
                    i += 1;
                    break;
                }
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'"' {
            out.push(b' ');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                        out.extend_from_slice(b"  ");
                        i += 2;
                        continue;
                    }
                    out.push(b' ');
                    i += 1;
                    break;
                }
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        // Dollar-quoted string: $tag$ ... $tag$
        if bytes[i] == b'$' {
            if let Some(rel) = sql[i + 1..].find('$') {
                let tag_end = i + 1 + rel;
                let end_tag = format!("${}$", &sql[i + 1..tag_end]);
                if let Some(body_rel) = sql[tag_end + 1..].find(&end_tag) {
                    let abs_end = tag_end + 1 + body_rel + end_tag.len();
                    out.extend(std::iter::repeat_n(b' ', abs_end - i));
                    i = abs_end;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| sql.to_string())
}

/// Returns the first forbidden keyword found in the cleaned SQL, if any.
fn find_write_keyword(cleaned: &str) -> Option<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in cleaned.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch.to_ascii_uppercase());
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
        .into_iter()
        .find(|t| WRITE_KEYWORDS.contains(&t.as_str()))
}

/// query: direct SQL (runtime only, must be SELECT).
const MAX_QUERY_ROWS: usize = 1000;

/// Decode a single result cell by its PostgreSQL column type so timestamps,
/// UUIDs, JSONB, bytea and arrays serialize as real values instead of NULL.
fn decode_column_value(row: &sqlx::postgres::PgRow, i: usize) -> serde_json::Value {
    let type_name = row.column(i).type_info().name();
    match type_name {
        "TIMESTAMPTZ" | "TIMESTAMP" => match row.try_get::<Option<DateTime<Utc>>, _>(i) {
            Ok(Some(dt)) => serde_json::Value::String(dt.to_rfc3339()),
            _ => serde_json::Value::Null,
        },
        "DATE" => match row.try_get::<Option<NaiveDate>, _>(i) {
            Ok(Some(d)) => serde_json::Value::String(d.to_string()),
            _ => serde_json::Value::Null,
        },
        "UUID" => match row.try_get::<Option<Uuid>, _>(i) {
            Ok(Some(u)) => serde_json::Value::String(u.to_string()),
            _ => serde_json::Value::Null,
        },
        "JSONB" | "JSON" => match row.try_get::<Option<serde_json::Value>, _>(i) {
            Ok(Some(v)) => v,
            _ => serde_json::Value::Null,
        },
        "BYTEA" => match row.try_get::<Option<Vec<u8>>, _>(i) {
            Ok(Some(b)) => serde_json::Value::String(
                b.iter().map(|byte| format!("{:02x}", byte)).collect::<String>(),
            ),
            _ => serde_json::Value::Null,
        },
        // Arrays: PostgreSQL type names start with an underscore.
        _ if type_name.starts_with('_') => decode_array_value(row, i),
        // Everything else: try scalar decodes in order of likelihood.
        _ => {
            if let Ok(s) = row.try_get::<&str, _>(i) {
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
            }
        }
    }
}

/// Decode a PostgreSQL array column into a JSON array. Tries the common
/// element types in order; falls back to NULL for exotic element types.
fn decode_array_value(row: &sqlx::postgres::PgRow, i: usize) -> serde_json::Value {
    if let Ok(Some(v)) = row.try_get::<Option<Vec<String>>, _>(i) {
        return serde_json::Value::Array(v.into_iter().map(serde_json::Value::String).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<i64>>, _>(i) {
        return serde_json::Value::Array(v.into_iter().map(|n| serde_json::json!(n)).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<f64>>, _>(i) {
        return serde_json::Value::Array(v.into_iter().map(|n| serde_json::json!(n)).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<bool>>, _>(i) {
        return serde_json::Value::Array(v.into_iter().map(|b| serde_json::json!(b)).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<Uuid>>, _>(i) {
        return serde_json::Value::Array(
            v.into_iter().map(|u| serde_json::Value::String(u.to_string())).collect(),
        );
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<DateTime<Utc>>>, _>(i) {
        return serde_json::Value::Array(
            v.into_iter()
                .map(|dt| serde_json::Value::String(dt.to_rfc3339()))
                .collect(),
        );
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<serde_json::Value>>, _>(i) {
        return serde_json::Value::Array(v);
    }
    serde_json::Value::Null
}

async fn handle_query(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let sql_owned = args["sql"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("'sql' is required for query operation"))?
        .to_string();

    // ── Read-only enforcement (defense in depth) ──────────────────────────
    // 1) The statement must START with SELECT or WITH (token-level check).
    // 2) Write/DDL keywords are rejected ANYWHERE in the statement, after
    //    stripping comments and string literals. This blocks data-modifying
    //    CTEs such as `WITH x AS (DELETE FROM messages RETURNING *) SELECT ...`
    //    that previously slipped past the starts-with check.
    // 3) `AssertSqlSafe` is a sqlx MARKER type, not a semicolon validator;
    //    multi-statement SQL is rejected by the extended query protocol.
    // 4) The statement runs inside an explicit `BEGIN TRANSACTION READ ONLY`,
    //    so PostgreSQL itself refuses any write even if this role were granted
    //    extra privileges in the future.
    let plain = strip_sql_literals_and_comments(&sql_owned);
    let first = plain.split_whitespace().next().unwrap_or("").to_uppercase();
    if first != "SELECT" && first != "WITH" {
        anyhow::bail!(
            "Only SELECT (or WITH) statements are allowed (statement must start with SELECT or WITH)."
        );
    }
    if let Some(bad) = find_write_keyword(&plain) {
        anyhow::bail!(
            "Query rejected: write/DDL keyword '{bad}' is not allowed in read-only queries."
        );
    }

    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to acquire connection: {e}"))?;
    sqlx::query("BEGIN TRANSACTION READ ONLY")
        .execute(&mut *conn)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to begin read-only transaction: {e}"))?;

    let results: Vec<serde_json::Value> = {
        let rows = match sqlx::query(sqlx::AssertSqlSafe(sql_owned.as_str()))
            .fetch_all(&mut *conn)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                return Err(anyhow::anyhow!("Query failed: {e}"));
            }
        };

        let mut json_rows: Vec<serde_json::Value> = Vec::new();
        for row in &rows {
            let mut map = serde_json::Map::new();
            for (i, col) in row.columns().iter().enumerate() {
                let name = col.name();
                let value = decode_column_value(row, i);
                map.insert(name.to_string(), value);
            }
            json_rows.push(serde_json::Value::Object(map));
        }
        json_rows
    };

    // Defense in depth: cap the result set regardless of the caller's LIMIT.
    let results = results.into_iter().take(MAX_QUERY_ROWS).collect::<Vec<_>>();

    sqlx::query("COMMIT")
        .execute(&mut *conn)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to commit read-only transaction: {e}"))?;

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

// ── Plugin config hook ─────────────────────────────────────────────────────

/// Plugin config — received via configure message.
#[derive(Debug, Clone)]
struct PluginConfig {
    pub database_url: String,
}

impl PluginConfig {
    fn from_json(v: &serde_json::Value) -> Self {
        Self {
            database_url: v
                .get("database_url")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_default(),
        }
    }
}

// ── Main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Shared pool — populated by configure callback before any tool call
    let pool: Arc<RwLock<Option<PgPool>>> = Arc::new(RwLock::new(None));

    let p_query = pool.clone();
    let query_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_query.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = match guard.as_ref() {
                Some(p) => p.clone(),
                None => return Ok(("Query database pool not configured. The plugin may need a database_url in its config.".to_string(), true)),
            };

            handle_query_database(&pool, &args).await
        })
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

    run_server_with_config(server_info, tools, {
        let p = pool.clone();
        Some(move |params: serde_json::Value| {
            let config = PluginConfig::from_json(&params);
            // Connect with the MAIN database user via the plugin's database_url
            // config field. The framework resolves the "$env:DATABASE_URL"
            // default before sending the configure message, so the plugin never
            // reads env vars directly. Read-only is enforced per-query by
            // BEGIN TRANSACTION READ ONLY in handle_query (PostgreSQL refuses
            // any write inside a read-only transaction, SQLSTATE 25006).
            let url = config.database_url.clone();
            tokio::task::block_in_place(|| {
                let rt = tokio::runtime::Handle::current();
                let new_pool = rt.block_on(db::connect(&url));
                match new_pool {
                    Ok(pool) => {
                        *p.blocking_write() = Some(pool);
                    }
                    Err(e) => {
                        tracing::error!("Query plugin failed to connect to database: {:#}", e);
                    }
                }
            });
            tracing::info!("Query plugin configured with database_url");
        })
    })
    .await
}
