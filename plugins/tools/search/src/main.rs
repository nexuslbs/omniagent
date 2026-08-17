//! mcp-server-search: standalone MCP server for searching messages, wiki,
//! database (read-only SQL), threads, channel prompts, channels and metrics.
//! Merged plugin: former `search` + `query` + `metrics` plugins consolidated
//! into one crate. Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools:
//! - search_messages: keyword (ILIKE) message search across channels
//! - search_wiki: keyword search over the active profile's wiki
//! - search_database: free-form SELECT SQL (read-only)
//! - search_thread_messages: all messages from a thread
//! - search_channel_prompts: all seq-0 (prompt) messages from a channel
//! - search_channels: list channels (id, name, platform, cause)
//! - search_metrics: agent metrics (token usage, latency, groundedness, ...)

use anyhow::Result;
use mcp_server_util::*;
use parking_lot::Mutex;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::types::chrono::{DateTime, NaiveDate, Utc};
use sqlx::types::Uuid;
use sqlx::{Column, FromRow, PgPool, Row, TypeInfo};
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Shared row types
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
struct SearchResult {
    id: i64,
    role: String,
    content: String,
}

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

/// Query result for token usage aggregation (metrics).
#[derive(Debug, sqlx::FromRow)]
struct TokenAggRow {
    profile: String,
    provider: Option<String>,
    model: Option<String>,
    total_prompt_tokens: Option<i64>,
    total_completion_tokens: Option<i64>,
    total_processing_ms: Option<i64>,
    message_count: Option<i64>,
    avg_processing_ms: Option<f64>,
}

// ---------------------------------------------------------------------------
// Plugin config — received via MCP configure message, not from env vars
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Config {
    database_url: String,
    omni_dir: String,
}

// ---------------------------------------------------------------------------
// Tool: search_messages (keyword / ILIKE)
// ---------------------------------------------------------------------------

async fn handle_search_messages(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;
    let limit = args["limit"].as_i64().unwrap_or(10).min(50);
    let channel_id = args["channel_id"].as_str().map(|s| s.to_string());

    let query_owned = query.to_string();
    let pool_ref = pool.clone();

    let results: Vec<SearchResult> = if let Some(cid) = channel_id {
        sql_forge!(
            SearchResult,
            r#"
            SELECT m.id, m.role, m.content FROM messages m
            JOIN threads t ON t.id = m.thread_id
            WHERE t.channel_id = :channel_id
              AND m.content ILIKE '%' || :query || '%'
            ORDER BY m.created_at DESC
            LIMIT :limit
            "#,
            ( :channel_id = cid.as_str(), :query = &query_owned, :limit = limit )
        )
        .fetch_all(&pool_ref)
        .await
        .map_err(|e: sqlx::Error| anyhow::anyhow!("Database query failed: {e}"))?
    } else {
        sql_forge!(
            SearchResult,
            r#"
            SELECT id, role, content FROM messages
            WHERE content ILIKE '%' || :query || '%'
            ORDER BY created_at DESC
            LIMIT :limit
            "#,
            ( :query = &query_owned, :limit = limit )
        )
        .fetch_all(&pool_ref)
        .await
        .map_err(|e: sqlx::Error| anyhow::anyhow!("Database query failed: {e}"))?
    };

    if results.is_empty() {
        return Ok(("No matching messages found.".to_string(), false));
    }

    let mut lines = Vec::new();
    for r in &results {
        let preview = if r.content.len() > 200 {
            let truncate_to = r
                .content
                .char_indices()
                .nth(200)
                .map(|(i, _)| i)
                .unwrap_or(r.content.len());
            format!("{}...", &r.content[..truncate_to])
        } else {
            r.content.clone()
        };
        lines.push(format!("#{} [{}]: {}", r.id, r.role, preview));
    }

    let output = format!("Found {} result(s):\n{}", results.len(), lines.join("\n\n"));
    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: search_wiki
// ---------------------------------------------------------------------------

fn handle_search_wiki(args: &Value, omni_dir: &str, profile_name: &str) -> Result<(String, bool)> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'query'"))?;
    let limit = args["limit"].as_i64().unwrap_or(10).min(30) as usize;
    // Profile comes from the AGENT's runtime context (_meta.profile_name,
    // injected by the MCP client on every tool call) — NOT from a tool
    // argument. Only fall back to the active profile when meta is absent
    // (e.g. manual testing outside the agent).
    let profile = if profile_name.trim().is_empty() {
        omniagent::profile::default_profile_name()
    } else {
        profile_name.trim().to_string()
    };

    let wiki_dir = format!("{}/profiles/{}/wiki", omni_dir, profile);
    let wiki_dir_path = std::path::Path::new(&wiki_dir);

    if !wiki_dir_path.exists() {
        return Ok((
            format!(
                "Wiki directory not found: {}. Is the profile correct? (active profile: {})",
                wiki_dir,
                omniagent::profile::default_profile_name()
            ),
            false,
        ));
    }

    let query_lower = query.to_lowercase();

    let mut results: Vec<(String, String)> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![wiki_dir_path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if results.len() >= limit {
                break;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().map(|e| e == "md").unwrap_or(false) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let lines: Vec<&str> = content.lines().collect();
                    let title_line = lines.first().unwrap_or(&"");
                    let title = title_line.trim_start_matches("# ").trim();
                    let preview_lines: Vec<&str> = lines
                        .iter()
                        .filter(|l| l.to_lowercase().contains(&query_lower))
                        .take(3)
                        .map(|l| l.trim())
                        .collect();
                    if !preview_lines.is_empty() || title.to_lowercase().contains(&query_lower) {
                        let rel = path.strip_prefix(wiki_dir_path).unwrap_or(&path);
                        let filename = rel.with_extension("").to_string_lossy().to_string();
                        let preview = if preview_lines.is_empty() {
                            "".to_string()
                        } else {
                            let truncated: Vec<&str> = preview_lines
                                .iter()
                                .map(|l| {
                                    if l.len() > 100 {
                                        let trunc_to = l
                                            .char_indices()
                                            .nth(100)
                                            .map(|(i, _)| i)
                                            .unwrap_or(l.len());
                                        &l[..trunc_to]
                                    } else {
                                        *l
                                    }
                                })
                                .collect();
                            format!("...{}...", truncated.join(" ... "))
                        };
                        results.push((filename, preview));
                    }
                }
            }
        }
    }

    if results.is_empty() {
        return Ok(("No matching wiki results found.".to_string(), false));
    }

    let output = results
        .iter()
        .map(|(name, preview)| {
            if preview.is_empty() {
                format!("[[{}]]", name)
            } else {
                format!("[[{}]]: {}", name, preview)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok((output, false))
}

// ---------------------------------------------------------------------------
// Tool: search_thread_messages
// ---------------------------------------------------------------------------

async fn handle_search_thread_messages(
    pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let thread_id = args["thread_id"]
        .as_i64()
        .or_else(|| meta.and_then(|m| m.thread_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "'thread_id' is required for search_thread_messages (no current thread in \
                 context). Pass thread_id explicitly."
            )
        })?;
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
        .map_err(|e| anyhow::anyhow!(e))?
    };

    Ok(format_results(
        "search_thread_messages",
        &rows,
        rows.len() as i64,
    ))
}

// ---------------------------------------------------------------------------
// Tool: search_channel_prompts
// ---------------------------------------------------------------------------

async fn handle_search_channel_prompts(
    pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let channel_id = args["channel_id"]
        .as_str()
        .map(String::from)
        .or_else(|| meta.and_then(|m| m.channel_id.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "'channel_id' is required for search_channel_prompts (no current channel in \
                 context). Pass channel_id explicitly or use search_channels to find it."
            )
        })?;
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
            ( :channel_id = &channel_id, :limit = limit )
        )
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!(e))?
    };

    Ok(format_results(
        "search_channel_prompts",
        &results,
        results.len() as i64,
    ))
}

// ---------------------------------------------------------------------------
// Tool: search_database (free-form read-only SELECT) — SQL safety helpers
// ---------------------------------------------------------------------------

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

/// Max rows returned by search_database.
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
                b.iter()
                    .map(|byte| format!("{:02x}", byte))
                    .collect::<String>(),
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
            v.into_iter()
                .map(|u| serde_json::Value::String(u.to_string()))
                .collect(),
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

async fn handle_search_database(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let sql_owned = args["sql"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("'sql' is required for search_database operation"))?
        .to_string();

    // ── Read-only enforcement (defense in depth) ──────────────────────────
    // 1) The statement must START with SELECT or WITH (token-level check).
    // 2) Write/DDL keywords are rejected ANYWHERE in the statement, after
    //    stripping comments and string literals. This blocks data-modifying
    //    CTEs such as `WITH x AS (DELETE FROM messages RETURNING *) SELECT ...`.
    // 3) `AssertSqlSafe` is a sqlx MARKER type, not a semicolon validator;
    //    multi-statement SQL is rejected by the extended query protocol.
    // 4) The statement runs inside an explicit `BEGIN TRANSACTION READ ONLY`.
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

// ---------------------------------------------------------------------------
// Tool: search_channels
// ---------------------------------------------------------------------------

async fn handle_search_channels(_pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let limit = args["limit"].as_i64().unwrap_or(50).min(200) as usize;

    // Channels live in {data_dir}/config/channels.yml now (id == name).
    let channels = omniagent::channels_yaml::find_all()
        .map_err(|e| anyhow::anyhow!("Failed to load channels.yml: {e}"))?;

    if channels.is_empty() {
        return Ok(("[search_channels] No channels found.".to_string(), false));
    }

    let mut lines = vec![format!("[search_channels] {} channel(s):", channels.len())];
    for (name, def) in channels.into_iter().take(limit) {
        lines.push(format!(
            "#{} {} (platform: {}, cause: {})",
            name,
            name,
            def.platform.as_deref().unwrap_or(""),
            def.cause
        ));
    }
    Ok((lines.join("\n"), false))
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Tool: search_metrics
// ---------------------------------------------------------------------------

/// Aggregate metrics from the messages table.
async fn aggregate_metrics(
    pool: &PgPool,
    hours: i64,
    profile_filter: &str,
) -> Result<Vec<TokenAggRow>> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    let rows: Vec<TokenAggRow> = sql_forge!(
        TokenAggRow,
        r#"
        SELECT
            t.profile,
            t.provider,
            t.model,
            SUM(t.input_tokens)::bigint AS total_prompt_tokens,
            SUM(t.output_tokens)::bigint AS total_completion_tokens,
            SUM(t.duration_ms)::bigint AS total_processing_ms,
            COUNT(*)::bigint AS message_count,
            AVG(t.duration_ms)::float AS avg_processing_ms
        FROM threads t
        JOIN messages m ON m.thread_id = t.id
        WHERE m.role = 'agent'
          AND m.msg_type IN ('message', 'summary')
          AND m.created_at >= :cutoff
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        GROUP BY t.profile, t.provider, t.model
        ORDER BY total_processing_ms DESC
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Count how many agent responses have evidence/grounding metadata.
async fn count_grounded_responses(
    pool: &PgPool,
    hours: i64,
    profile_filter: &str,
) -> Result<(i64, i64)> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    let total: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        SELECT COUNT(*)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE m.role = 'agent'
          AND m.msg_type IN ('message', 'summary')
          AND m.created_at >= :cutoff
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    let grounded: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        SELECT COUNT(*)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE m.role = 'agent'
          AND m.msg_type IN ('message', 'summary')
          AND m.created_at >= :cutoff
          AND (m.metadata->'context'->>'total_chars') IS NOT NULL
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    Ok((total.unwrap_or(0), grounded.unwrap_or(0)))
}

/// Count retrieval events (how often search tools were called).
async fn count_retrieval_events(pool: &PgPool, hours: i64, profile_filter: &str) -> Result<i64> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    let count: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        SELECT COUNT(*)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE m.role = 'agent'
          AND m.msg_type = 'tool_call'
          AND m.msg_subtype IN ('search_messages', 'search_wiki')
          AND m.created_at >= :cutoff
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    Ok(count.unwrap_or(0))
}

/// Count user corrections (proxies for hallucination).
async fn count_corrections(pool: &PgPool, hours: i64, profile_filter: &str) -> Result<i64> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    let count: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        WITH agent_responses AS (
            SELECT m.id, t.channel_id, m.thread_id, m.created_at
            FROM messages m
            JOIN threads t ON t.id = m.thread_id
            WHERE m.role = 'agent'
              AND m.msg_type IN ('message', 'summary')
              AND m.created_at >= :cutoff
              AND (:profile_filter = '' OR t.profile = :profile_filter)
        )
        SELECT COUNT(DISTINCT m.id)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        INNER JOIN agent_responses a
            ON t.channel_id = a.channel_id
            AND m.thread_id = a.thread_id
            AND m.created_at > a.created_at
            AND m.created_at <= a.created_at + INTERVAL '5 minutes'
        WHERE m.role = 'user'
          AND (
              LOWER(m.content) LIKE '%wrong%'
              OR LOWER(m.content) LIKE '%incorrect%'
              OR LOWER(m.content) LIKE '%that''s not%'
              OR LOWER(m.content) LIKE '%actually%'
              OR LOWER(m.content) LIKE '%no,%'
              OR LOWER(m.content) LIKE '%not what%'
              OR LOWER(m.content) LIKE '%try again%'
          )
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    Ok(count.unwrap_or(0))
}

async fn handle_search_metrics(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let hours = args.get("hours").and_then(|v| v.as_i64()).unwrap_or(24);
    let profile = args.get("profile").and_then(|v| v.as_str());
    let profile_owned = profile.map(|s| s.to_string()).unwrap_or_default();

    let usage = aggregate_metrics(pool, hours, &profile_owned).await?;
    let (total_responses, grounded_responses) =
        count_grounded_responses(pool, hours, &profile_owned).await?;
    let retrieval_count = count_retrieval_events(pool, hours, &profile_owned).await?;
    let correction_count = count_corrections(pool, hours, &profile_owned).await?;

    let mut report = format!(
        "# Agent Metrics Report\n\nPeriod: **last {} hour(s)**\n\n",
        hours
    );

    if let Some(p) = profile {
        report.push_str(&format!("Profile filter: **{}**\n\n", p));
    }

    // Summary
    let grounded_pct = if total_responses > 0 {
        (grounded_responses as f64 / total_responses as f64 * 100.0) as u32
    } else {
        0
    };

    report.push_str("## Summary\n\n");
    report.push_str(&format!(
        "- **Total agent responses**: {}\n",
        total_responses
    ));
    report.push_str(&format!(
        "- **Grounded response rate**: {}% ({} / {})\n",
        grounded_pct, grounded_responses, total_responses
    ));
    report.push_str(&format!(
        "- **Retrieval tool calls**: {}\n",
        retrieval_count
    ));
    report.push_str(&format!(
        "- **User corrections (proxy)**: {}\n\n",
        correction_count
    ));

    if usage.is_empty() {
        report.push_str("No metrics data found for this period.\n\n");
    } else {
        report.push_str("## By Profile / Provider / Model\n\n");
        report.push_str(
            "| Profile | Provider | Model | Messages | Prompt Tokens | Completion Tokens \
             | Total Time (ms) | Avg Time (ms) |\n",
        );
        report.push_str(
            "|---------|----------|-------|----------|---------------|-------------------\
             |-----------------|---------------|\n",
        );

        for row in &usage {
            report.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {:.0} |\n",
                row.profile,
                row.provider.as_deref().unwrap_or("-"),
                row.model.as_deref().unwrap_or("-"),
                row.message_count.unwrap_or(0),
                row.total_prompt_tokens.unwrap_or(0),
                row.total_completion_tokens.unwrap_or(0),
                row.total_processing_ms.unwrap_or(0),
                row.avg_processing_ms.unwrap_or(0.0),
            ));
        }

        // Totals
        let total_prompt: i64 = usage
            .iter()
            .map(|r| r.total_prompt_tokens.unwrap_or(0))
            .sum();
        let total_completion: i64 = usage
            .iter()
            .map(|r| r.total_completion_tokens.unwrap_or(0))
            .sum();
        let total_time: i64 = usage
            .iter()
            .map(|r| r.total_processing_ms.unwrap_or(0))
            .sum();

        report.push_str(&format!(
            "\n**Totals**: {} prompts | {} completion tokens | {} ms processing time\n\n",
            total_prompt, total_completion, total_time
        ));
    }

    // Hallucination metric explanation
    report.push_str("## Metrics Notes\n\n");
    report.push_str(
        "- **Grounded response rate**: Percentage of agent responses that include context \
         assembly metadata (evidence tracking)\n",
    );
    report.push_str(
        "- **Retrieval tool calls**: Number of times search_messages or search_wiki tools \
         were invoked\n",
    );
    report.push_str(
        "- **User corrections (proxy)**: Count of user messages containing correction \
         keywords (wrong, incorrect, etc.) within 5 minutes of an agent response: a proxy \
         for hallucination/quality issues\n",
    );

    Ok((report, false))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Plugin config — received via MCP configure message
    let config: Arc<Mutex<Config>> = Arc::new(Mutex::new(Config::default()));

    // Shared database pool — populated by configure callback before any tool call
    let pool: Arc<RwLock<Option<PgPool>>> = Arc::new(RwLock::new(None));

    // on_configure: called when omniagent sends the resolved plugin config
    let on_configure = {
        let config = config.clone();
        let pool = pool.clone();
        Some(move |params: Value| {
            let mut cfg = config.lock();
            if let Some(url) = params.get("database_url").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    cfg.database_url = url.to_string();

                    // Also initialize the database pool
                    let url_clone = url.to_string();
                    tokio::task::block_in_place(|| {
                        let rt = tokio::runtime::Handle::current();
                        let new_pool = rt
                            .block_on(omniagent::db::connect(&url_clone))
                            .expect("Failed to connect to database");
                        *pool.blocking_write() = Some(new_pool);
                    });
                }
            }
            if let Some(dir) = params.get("omni_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.omni_dir = dir.to_string();
                }
            }
            // Channels.yml data dir — needed by search_channels
            omniagent::channels_yaml::set_data_dir(&cfg.omni_dir);
            tracing::info!("Search plugin configured");
        })
    };

    let default_omni_dir = "/opt/omni".to_string();

    // ── search_messages (keyword) ─────────────────────────────────────────
    let p_search = pool.clone();
    let search_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_search.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!("Database pool not initialized. Configure plugin first.")
                })?
                .clone();
            handle_search_messages(&pool, &args).await
        })
    });

    // ── search_wiki ───────────────────────────────────────────────────────
    let c1 = config.clone();
    let d1 = default_omni_dir.clone();
    let wiki_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let c = c1.clone();
        let d = d1.clone();
        // Agent's profile from _meta (injected by the MCP client) — same
        // pattern as the skills plugin. Never requires a profile argument.
        let profile = meta
            .as_ref()
            .and_then(|m| m.profile_name.clone())
            .unwrap_or_default();
        Box::pin(async move {
            let cfg = c.lock();
            let omni_dir = if cfg.omni_dir.is_empty() {
                &d
            } else {
                &cfg.omni_dir
            };
            handle_search_wiki(&args, omni_dir, &profile)
        })
    });

    // Helper to fetch the shared pool; returns a soft error if not configured.
    fn pool_or_err(pool: &Arc<RwLock<Option<PgPool>>>) -> Result<PgPool, (String, bool)> {
        let guard = pool.try_read();
        match guard {
            Ok(g) => match g.as_ref() {
                Some(p) => Ok(p.clone()),
                None => Err((
                    "Search database pool not configured. The plugin may need a database_url in its config."
                        .to_string(),
                    true,
                )),
            },
            Err(_) => Err((
                "Search database pool lock poisoned or busy. Retry.".to_string(),
                true,
            )),
        }
    }

    // ── search_database: free-form read-only SELECT ───────────────────────
    let p_db = pool.clone();
    let db_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_db.clone();
        Box::pin(async move {
            let pool = match pool_or_err(&p) {
                Ok(pool) => pool,
                Err(e) => return Ok(e),
            };
            handle_search_database(&pool, &args).await
        })
    });

    // ── search_thread_messages: full thread retrieval ─────────────────────
    let p_tm = pool.clone();
    let thread_messages_handler: ToolHandler =
        Box::new(move |args: Value, meta: Option<McpMeta>| {
            let p = p_tm.clone();
            Box::pin(async move {
                let pool = match pool_or_err(&p) {
                    Ok(pool) => pool,
                    Err(e) => return Ok(e),
                };
                handle_search_thread_messages(&pool, &args, meta.as_ref()).await
            })
        });

    // ── search_channel_prompts: channel prompt history ────────────────────
    let p_cp = pool.clone();
    let channel_prompts_handler: ToolHandler =
        Box::new(move |args: Value, meta: Option<McpMeta>| {
            let p = p_cp.clone();
            Box::pin(async move {
                let pool = match pool_or_err(&p) {
                    Ok(pool) => pool,
                    Err(e) => return Ok(e),
                };
                handle_search_channel_prompts(&pool, &args, meta.as_ref()).await
            })
        });

    // ── search_channels: list channels ────────────────────────────────────
    let p_ch = pool.clone();
    let channels_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_ch.clone();
        Box::pin(async move {
            let pool = match pool_or_err(&p) {
                Ok(pool) => pool,
                Err(e) => return Ok(e),
            };
            handle_search_channels(&pool, &args).await
        })
    });

    // ── search_metrics: agent metrics ─────────────────────────────────────
    let p_m = pool.clone();
    let metrics_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_m.clone();
        Box::pin(async move {
            let pool = match pool_or_err(&p) {
                Ok(pool) => pool,
                Err(e) => return Ok(e),
            };
            handle_search_metrics(&pool, &args).await
        })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "search_messages".to_string(),
                description: "Search message history across all channels. Use this tool when the LLM needs to find information from past conversations. Use specific keywords and narrow the scope with channel_id when possible. Does NOT search wiki pages: use search_wiki for that.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query to find in messages"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results (max 50)",
                            "default": 10
                        },
                        "channel_id": { "type": "string", "description": "Optional channel name filter" }
                    },
                    "required": ["query"]
                }),
            },
            handler: search_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_wiki".to_string(),
                description: "Search wiki pages for relevant documentation. Use this to find documentation, guides, and notes. Searches the ACTIVE PROFILE's wiki automatically (the profile is injected by the runtime, no profile argument needed). Does NOT search message history: use search_messages for that.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query to find in wiki content and filenames"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results (max 30)",
                            "default": 10
                        }
                    },
                    "required": ["query"]
                }),
            },
            handler: wiki_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_database".to_string(),
                description: "Run any read-only SELECT SQL against the agent database. \
This is the FREE-QUERY tool: use it for custom aggregations (COUNT(*), GROUP BY, SUM, \
JOIN across tables) and structured lookups that the purpose-built search tools do not cover. \
The statement MUST start with SELECT or WITH; write/DDL keywords (INSERT/UPDATE/DELETE/DROP/\
ALTER/...) are rejected, and the query runs inside a read-only transaction, so writes are \
blocked at the database level.\n\n\
Available tables: messages, threads, summaries, kanban_tasks, \
profiles. Include the full table/column names in your SQL.\n\n\
For common lookups prefer the purpose-built tools: search_messages (keyword), \
search_thread-messages (thread contents), search_channel-prompts (channel prompt history), \
search_channels (channel ids)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "sql": {
                            "type": "string",
                            "description": "Raw SELECT (or WITH) SQL statement to execute"
                        }
                    },
                    "required": ["sql"]
                }),
            },
            handler: db_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_thread_messages".to_string(),
                description: "Read all messages in a conversation thread (the prompt + its \
replies), ordered by sequence. Defaults to the CURRENT thread; pass thread_id to read a \
different one. Use to reconstruct a past conversation or inspect what a thread contained."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "thread_id": {
                            "type": "integer",
                            "description": "Thread ID (default: current thread)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max messages (max 200)",
                            "default": 100
                        }
                    }
                }),
            },
            handler: thread_messages_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_channel_prompts".to_string(),
                description: "List the first message (prompt / seq-0) of every thread in a \
channel, newest first. Use to review what has been asked or started in a channel. Defaults \
to the CURRENT channel; pass channel_id for a different one."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "channel_id": { "type": "string", "description": "Channel name (default: current channel)" },
                        "limit": {
                            "type": "integer",
                            "description": "Max results (max 50)",
                            "default": 10
                        }
                    }
                }),
            },
            handler: channel_prompts_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_channels".to_string(),
                description: "List all channels with their id, name, platform and cause. \
Use to discover channel_id values needed by channel-scoped tools (search_channel-prompts, \
search_messages)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "description": "Max channels (max 200)",
                            "default": 50
                        }
                    }
                }),
            },
            handler: channels_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "search_metrics".to_string(),
                description: "Report agent performance metrics: token usage, latency, message counts, \
                 groundedness rate, retrieval hit rate, and hallucination proxy metrics. \
                 All metrics are aggregated from the messages table and can be filtered \
                 by time window and profile."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "hours": {
                            "type": "integer",
                            "description": "Lookback window in hours (default: 24)"
                        },
                        "profile": {
                            "type": "string",
                            "description": "Filter by profile name (default: all profiles)"
                        }
                    }
                }),
            },
            handler: metrics_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-search".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    run_server_with_config(server_info, tools, on_configure).await
}
