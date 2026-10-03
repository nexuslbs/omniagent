//! Core read-only database query API.
//!
//! This module is the SINGLE SOURCE OF TRUTH for read-only SQL execution
//! against the agent database. It exists so the dashboard Database page (and
//! any other core consumer) never depends on an MCP plugin being installed and
//! enabled:
//!
//! * `POST /db/query` (`crate::server::db_query`) and `GET /db/tables` call
//!   [`execute_readonly_query`] directly on the core pool.
//! * the `search_database` MCP tool (plugin `search`) DELEGATES to that HTTP
//!   endpoint instead of owning its own pool + guard.
//!
//! Guard (unchanged semantics, previously duplicated in the plugin):
//! 1. the statement must START with `SELECT` or `WITH` (token-level check);
//! 2. write/DDL keywords are rejected ANYWHERE in the statement, after
//!    stripping comments and string literals, which also blocks data-modifying
//!    CTEs such as `WITH x AS (DELETE FROM messages RETURNING *) SELECT ...`;
//! 3. the statement runs inside an explicit `BEGIN TRANSACTION READ ONLY`
//!    (the read-only transaction is the database-level backstop);
//! 4. every statement is bounded by `SET LOCAL statement_timeout = 8000`;
//! 5. the row set is capped at [`MAX_QUERY_ROWS`] regardless of the caller's
//!    LIMIT.
//!
//! No second, divergent copy of this logic may exist.

use serde_json::Value;
use sqlx::types::chrono::{DateTime, NaiveDate, Utc};
use sqlx::types::Uuid;
use sqlx::{Column, PgPool, Row, TypeInfo};

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

/// Default max rows returned by a read-only query (settings
/// `db_readonly_max_rows`).
pub const MAX_QUERY_ROWS: usize = 1000;

/// Default statement timeout (ms) applied to every read-only statement via SET
/// LOCAL (settings `db_readonly_timeout_ms`).
///
/// Free-form queries are for structured aggregations only: message-content
/// lookups belong to `search_messages` (tsvector over `messages.search_tsv`).
/// An `ILIKE '%term%'` scan over `messages.content` has no usable index and
/// costs ~30 s per call, so this cap makes that mistake fail fast with a
/// hint instead of blocking the caller.
pub const STATEMENT_TIMEOUT_MS: i64 = 8000;

/// Default slow-query log threshold (ms): any read-only statement slower than
/// this is logged (with its SQL) so costly scans stay visible (settings
/// `db_readonly_slow_query_ms`).
pub const SLOW_QUERY_LOG_MS: u128 = 2000;

/// Effective read-only guard limits (settings with the documented defaults).
///
/// Every value is used VERBATIM: an operator value is never clamped to a
/// narrower one (defect class A5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOnlyLimits {
    /// Row cap applied to every result set (defence in depth).
    pub max_rows: usize,
    /// Per-statement `SET LOCAL statement_timeout`, in ms.
    pub statement_timeout_ms: i64,
    /// Statements slower than this are logged with their SQL, in ms.
    pub slow_query_log_ms: u128,
}

impl Default for ReadOnlyLimits {
    fn default() -> Self {
        Self {
            max_rows: MAX_QUERY_ROWS,
            statement_timeout_ms: STATEMENT_TIMEOUT_MS,
            slow_query_log_ms: SLOW_QUERY_LOG_MS,
        }
    }
}

impl ReadOnlyLimits {
    /// Read the guard limits from `settings.yml` (`db_readonly_max_rows`,
    /// `db_readonly_timeout_ms`, `db_readonly_slow_query_ms`), falling back to
    /// the documented code defaults (audit HV-B6).
    pub fn from_settings() -> Self {
        Self {
            max_rows: crate::runtime_settings::get_usize("db_readonly_max_rows", MAX_QUERY_ROWS),
            statement_timeout_ms: crate::runtime_settings::get_i64(
                "db_readonly_timeout_ms",
                STATEMENT_TIMEOUT_MS,
            ),
            slow_query_log_ms: crate::runtime_settings::get_u64(
                "db_readonly_slow_query_ms",
                SLOW_QUERY_LOG_MS as u64,
            ) as u128,
        }
    }
}

/// The `SET LOCAL statement_timeout` statement for `ms`.
///
/// ONE source (audit HV-D1): the SQL is DERIVED from the effective value, so
/// the enforcement path and the configured value can never drift. sqlx only
/// accepts literal-safe SQL here and the value is an i64 from our own config,
/// never user input.
pub fn timeout_sql(ms: i64) -> String {
    format!("SET LOCAL statement_timeout = {ms}")
}

/// `GET /db/tables` payload: the public-schema table list, expressed as a
/// read-only query so it flows through the very same guard.
pub const TABLES_SQL: &str = "SELECT table_name, table_type FROM information_schema.tables \
    WHERE table_schema = 'public' ORDER BY table_name";

/// Failure classes of the read-only API, mapped to HTTP status + machine code
/// by the server layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyQueryError {
    /// Guard rejection (bad request): not SELECT/WITH, or a write/DDL keyword.
    Rejected(String),
    /// The statement-timeout guard fired (SQLSTATE 57014).
    Timeout(String),
    /// Anything else: pool/connection/statement failure.
    Failed(String),
}

impl ReadOnlyQueryError {
    /// HTTP status for this failure class.
    pub fn http_status(&self) -> u16 {
        match self {
            ReadOnlyQueryError::Rejected(_) => 400,
            ReadOnlyQueryError::Timeout(_) => 504,
            ReadOnlyQueryError::Failed(_) => 400,
        }
    }

    /// Machine-readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            ReadOnlyQueryError::Rejected(_) => "db_query_rejected",
            ReadOnlyQueryError::Timeout(_) => "db_statement_timeout",
            ReadOnlyQueryError::Failed(_) => "db_query_error",
        }
    }

    /// Human-readable message (what the dashboard/agent shows).
    pub fn message(&self) -> &str {
        match self {
            ReadOnlyQueryError::Rejected(m)
            | ReadOnlyQueryError::Timeout(m)
            | ReadOnlyQueryError::Failed(m) => m,
        }
    }

    /// Short remediation hint for the caller.
    pub fn remediation(&self) -> &'static str {
        match self {
            ReadOnlyQueryError::Rejected(_) => {
                "Send a single read-only SELECT (or WITH) statement; write/DDL keywords are not allowed."
            }
            ReadOnlyQueryError::Timeout(_) => {
                "Narrow the query (tighter WHERE, LIMIT) or use search_messages for message-content lookups."
            }
            ReadOnlyQueryError::Failed(_) => {
                "Check the SQL syntax, table/column names and the statement's WHERE clause."
            }
        }
    }
}

impl std::fmt::Display for ReadOnlyQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

/// Result of a successful read-only query.
#[derive(Debug, Clone)]
pub struct ReadOnlyQueryResult {
    pub rows: Vec<Value>,
    pub row_count: usize,
    /// Result columns in STATEMENT order (sqlx `Row::columns()`, i.e. the
    /// order the columns were written in the SELECT). The JSON row objects
    /// themselves serialize their keys in ALPHABETICAL order (serde_json
    /// without `preserve_order`), so consumers that must preserve the SELECT
    /// order (the dashboard Database page result table) have to use this list
    /// instead of `Object.keys(row)`.
    pub columns: Vec<String>,
}

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

/// Validate a statement against the read-only guard WITHOUT touching the
/// database. Public so callers (and tests) can pre-flight a statement and so
/// the guard has exactly one implementation.
pub fn validate_readonly_sql(sql: &str) -> Result<(), ReadOnlyQueryError> {
    let plain = strip_sql_literals_and_comments(sql);
    let first = plain.split_whitespace().next().unwrap_or("").to_uppercase();
    if first != "SELECT" && first != "WITH" {
        return Err(ReadOnlyQueryError::Rejected(
            "Only SELECT (or WITH) statements are allowed (statement must start with SELECT or WITH)."
                .to_string(),
        ));
    }
    if let Some(bad) = find_write_keyword(&plain) {
        return Err(ReadOnlyQueryError::Rejected(format!(
            "Query rejected: write/DDL keyword '{bad}' is not allowed in read-only queries."
        )));
    }
    Ok(())
}

/// True when a sqlx error text reports a PostgreSQL statement-timeout
/// cancellation (SQLSTATE 57014, "canceling statement due to statement
/// timeout").
fn is_statement_timeout_error(err_text: &str) -> bool {
    let lower = err_text.to_lowercase();
    lower.contains("statement timeout") || lower.contains("57014")
}

/// Hint returned when the statement-timeout guard fires: tells the caller to
/// use search_messages (tsvector) for content lookups instead of running
/// ILIKE scans over messages.content.
pub fn timeout_hint(statement_timeout_ms: i64) -> String {
    format!(
        "Query canceled after {statement_timeout_ms} ms by the statement-timeout guard ({} s). If \
         you were searching for message CONTENT, do not use ILIKE '%..%' over messages.content: \
         that full-table scan has no usable index and costs ~30 s per call. Use search_messages \
         instead: it searches the GIN-indexed messages.search_tsv tsvector column and returns in \
         well under 3 s. For structured aggregations, add a tighter WHERE clause and a LIMIT.",
        statement_timeout_ms / 1000
    )
}

/// Collapse a SQL statement to one bounded line for slow-query logs.
fn one_line_sql(sql: &str) -> String {
    let joined: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > 200 {
        let head: String = joined.chars().take(200).collect();
        format!("{head}...")
    } else {
        joined
    }
}

/// Decode a single result cell by its PostgreSQL column type so timestamps,
/// UUIDs, JSONB, bytea and arrays serialize as real values instead of NULL.
fn decode_column_value(row: &sqlx::postgres::PgRow, i: usize) -> Value {
    let type_name = row.column(i).type_info().name();
    match type_name {
        "TIMESTAMPTZ" | "TIMESTAMP" => match row.try_get::<Option<DateTime<Utc>>, _>(i) {
            Ok(Some(dt)) => Value::String(dt.to_rfc3339()),
            _ => Value::Null,
        },
        "DATE" => match row.try_get::<Option<NaiveDate>, _>(i) {
            Ok(Some(d)) => Value::String(d.to_string()),
            _ => Value::Null,
        },
        "UUID" => match row.try_get::<Option<Uuid>, _>(i) {
            Ok(Some(u)) => Value::String(u.to_string()),
            _ => Value::Null,
        },
        "JSONB" | "JSON" => match row.try_get::<Option<Value>, _>(i) {
            Ok(Some(v)) => v,
            _ => Value::Null,
        },
        "BYTEA" => match row.try_get::<Option<Vec<u8>>, _>(i) {
            Ok(Some(b)) => Value::String(
                b.iter()
                    .map(|byte| format!("{:02x}", byte))
                    .collect::<String>(),
            ),
            _ => Value::Null,
        },
        // Arrays: PostgreSQL type names start with an underscore.
        _ if type_name.starts_with('_') => decode_array_value(row, i),
        // Everything else: try scalar decodes in order of likelihood.
        _ => {
            if let Ok(s) = row.try_get::<&str, _>(i) {
                Value::String(s.to_string())
            } else if let Ok(n) = row.try_get::<i64, _>(i) {
                serde_json::json!(n)
            } else if let Ok(n) = row.try_get::<i32, _>(i) {
                // PostgreSQL INT4 (integer). sqlx's i64 decoder accepts INT8
                // only, so without this branch int4 columns silently decoded
                // as NULL (regression reported 2026-10-01).
                serde_json::json!(n)
            } else if let Ok(n) = row.try_get::<i16, _>(i) {
                // PostgreSQL INT2 (smallint); same i64-decoder limitation.
                serde_json::json!(n)
            } else if let Ok(n) = row.try_get::<f64, _>(i) {
                serde_json::json!(n)
            } else if let Ok(b) = row.try_get::<bool, _>(i) {
                serde_json::json!(b)
            } else {
                row.try_get::<Option<String>, _>(i)
                    .ok()
                    .flatten()
                    .map(Value::String)
                    .unwrap_or(Value::Null)
            }
        }
    }
}

/// Decode an ARRAY column into a JSON array, trying the common element types.
fn decode_array_value(row: &sqlx::postgres::PgRow, i: usize) -> Value {
    if let Ok(Some(v)) = row.try_get::<Option<Vec<String>>, _>(i) {
        return Value::Array(v.into_iter().map(|s| serde_json::json!(s)).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<i64>>, _>(i) {
        return Value::Array(v.into_iter().map(|n| serde_json::json!(n)).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<f64>>, _>(i) {
        return Value::Array(v.into_iter().map(|n| serde_json::json!(n)).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<bool>>, _>(i) {
        return Value::Array(v.into_iter().map(|b| serde_json::json!(b)).collect());
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<Uuid>>, _>(i) {
        return Value::Array(
            v.into_iter()
                .map(|u| Value::String(u.to_string()))
                .collect(),
        );
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<DateTime<Utc>>>, _>(i) {
        return Value::Array(
            v.into_iter()
                .map(|dt| Value::String(dt.to_rfc3339()))
                .collect(),
        );
    }
    if let Ok(Some(v)) = row.try_get::<Option<Vec<Value>>, _>(i) {
        return Value::Array(v);
    }
    Value::Null
}

/// Execute a read-only query against the agent database.
///
/// The ONLY entry point for free-form read-only SQL in the agent: the core
/// `/db/query` endpoint and the `search_database` MCP tool (which delegates to
/// that endpoint) both end up here.
pub async fn execute_readonly_query(
    pool: &PgPool,
    sql: &str,
    limits: &ReadOnlyLimits,
) -> Result<ReadOnlyQueryResult, ReadOnlyQueryError> {
    validate_readonly_sql(sql)?;

    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| ReadOnlyQueryError::Failed(format!("Failed to acquire connection: {e}")))?;
    sqlx::query("BEGIN TRANSACTION READ ONLY")
        .execute(&mut *conn)
        .await
        .map_err(|e| {
            ReadOnlyQueryError::Failed(format!("Failed to begin read-only transaction: {e}"))
        })?;

    // Latency guard: SET LOCAL statement_timeout bounds every single statement
    // to the configured limit (transaction-scoped, rolled back with it).
    if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(timeout_sql(
        limits.statement_timeout_ms,
    )))
    .execute(&mut *conn)
    .await
    {
        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
        return Err(ReadOnlyQueryError::Failed(format!(
            "Failed to set statement timeout: {e}"
        )));
    }

    let query_started = std::time::Instant::now();

    let rows = match sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(&mut *conn)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            let err_text = e.to_string();
            let elapsed_ms = query_started.elapsed().as_millis();
            if is_statement_timeout_error(&err_text) {
                tracing::warn!(
                    "read-only query statement timeout after {elapsed_ms} ms: {}",
                    one_line_sql(sql)
                );
                return Err(ReadOnlyQueryError::Timeout(format!(
                    "{}\n\nOriginal error: {}",
                    timeout_hint(limits.statement_timeout_ms),
                    err_text
                )));
            }
            if elapsed_ms > limits.slow_query_log_ms {
                tracing::warn!(
                    "read-only failed slow query ({elapsed_ms} ms): {}",
                    one_line_sql(sql)
                );
            }
            return Err(ReadOnlyQueryError::Failed(format!(
                "Query failed: {err_text}"
            )));
        }
    };

    // Column order of the statement (empty when the statement returned no row:
    // then there is no row description to read).
    let columns: Vec<String> = rows
        .first()
        .map(|row| row.columns().iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default();

    let mut json_rows: Vec<Value> = Vec::new();
    for row in &rows {
        let mut map = serde_json::Map::new();
        for (i, col) in row.columns().iter().enumerate() {
            let name = col.name();
            let value = decode_column_value(row, i);
            map.insert(name.to_string(), value);
        }
        json_rows.push(Value::Object(map));
    }

    // Defense in depth: cap the result set regardless of the caller's LIMIT.
    json_rows.truncate(limits.max_rows);
    let row_count = json_rows.len();

    sqlx::query("COMMIT")
        .execute(&mut *conn)
        .await
        .map_err(|e| {
            ReadOnlyQueryError::Failed(format!("Failed to commit read-only transaction: {e}"))
        })?;

    let elapsed_ms = query_started.elapsed().as_millis();
    if elapsed_ms > limits.slow_query_log_ms {
        tracing::warn!(
            "read-only slow query ({elapsed_ms} ms): {}",
            one_line_sql(sql)
        );
    }

    Ok(ReadOnlyQueryResult {
        rows: json_rows,
        row_count,
        columns,
    })
}

/// The public-schema table list, executed through the same guard.
pub async fn list_public_tables(
    pool: &PgPool,
    limits: &ReadOnlyLimits,
) -> Result<ReadOnlyQueryResult, ReadOnlyQueryError> {
    execute_readonly_query(pool, TABLES_SQL, limits).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The enforced `SET LOCAL statement_timeout` is DERIVED from the effective
    /// limit: changing the setting changes the statement (audit HV-D1 - the
    /// value used to be encoded twice, so changing one left the other stale).
    #[test]
    fn timeout_sql_is_derived_from_the_effective_limit() {
        let sql = timeout_sql(STATEMENT_TIMEOUT_MS);
        assert!(
            sql.contains(&STATEMENT_TIMEOUT_MS.to_string()),
            "sql: {sql}"
        );
        assert_eq!(timeout_sql(1500), "SET LOCAL statement_timeout = 1500");
        let custom = ReadOnlyLimits {
            statement_timeout_ms: 1500,
            ..ReadOnlyLimits::default()
        };
        assert!(timeout_sql(custom.statement_timeout_ms).contains("1500"));
    }

    /// The documented defaults ARE the shipped behaviour when nothing is
    /// configured.
    #[test]
    fn default_limits_match_the_documented_values() {
        let l = ReadOnlyLimits::default();
        assert_eq!(l.max_rows, MAX_QUERY_ROWS);
        assert_eq!(l.statement_timeout_ms, STATEMENT_TIMEOUT_MS);
        assert_eq!(l.slow_query_log_ms, SLOW_QUERY_LOG_MS);
    }

    #[test]
    fn accepts_select_and_with() {
        assert!(validate_readonly_sql("SELECT 1").is_ok());
        assert!(validate_readonly_sql("  select * from messages limit 5").is_ok());
        assert!(validate_readonly_sql("WITH x AS (SELECT 1) SELECT * FROM x").is_ok());
    }

    #[test]
    fn rejects_write_statements() {
        for sql in [
            "INSERT INTO messages (role) VALUES ('x')",
            "UPDATE messages SET role = 'x'",
            "DELETE FROM messages",
            "DROP TABLE messages",
            "ALTER TABLE messages ADD COLUMN x int",
            "CREATE TABLE t (a int)",
            "TRUNCATE messages",
        ] {
            assert!(
                matches!(
                    validate_readonly_sql(sql),
                    Err(ReadOnlyQueryError::Rejected(_))
                ),
                "expected rejection for: {sql}"
            );
        }
    }

    #[test]
    fn rejects_data_modifying_cte() {
        let sql = "WITH x AS (DELETE FROM messages RETURNING *) SELECT * FROM x";
        assert!(matches!(
            validate_readonly_sql(sql),
            Err(ReadOnlyQueryError::Rejected(_))
        ));
    }

    #[test]
    fn ignores_keywords_inside_comments_and_literals() {
        assert!(validate_readonly_sql("SELECT 'DELETE' AS k").is_ok());
        assert!(validate_readonly_sql("SELECT 1 -- UPDATE t SET a=1").is_ok());
        assert!(validate_readonly_sql("SELECT 1 /* DROP TABLE t */").is_ok());
        assert!(validate_readonly_sql("SELECT $$DROP$$ AS k").is_ok());
    }

    #[test]
    fn error_codes_and_statuses() {
        let rejected = ReadOnlyQueryError::Rejected("nope".to_string());
        assert_eq!(rejected.code(), "db_query_rejected");
        assert_eq!(rejected.http_status(), 400);
        let timeout = ReadOnlyQueryError::Timeout("slow".to_string());
        assert_eq!(timeout.code(), "db_statement_timeout");
        assert_eq!(timeout.http_status(), 504);
        let failed = ReadOnlyQueryError::Failed("boom".to_string());
        assert_eq!(failed.code(), "db_query_error");
        assert_eq!(failed.http_status(), 400);
    }

    #[test]
    fn tables_sql_passes_the_guard() {
        assert!(validate_readonly_sql(TABLES_SQL).is_ok());
    }

    /// DB-backed test: runs only when DATABASE_URL is set (dev container).
    #[tokio::test]
    async fn db_backed_guard_and_result_decoding() {
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = match crate::db::connect(&db_url).await {
            Ok(p) => p,
            Err(_) => return,
        };
        let _guard = crate::db::DB_TEST_LOCK.lock().await;
        let limits = ReadOnlyLimits::default();

        let res =
            execute_readonly_query(&pool, "SELECT COUNT(*)::int8 AS n FROM messages", &limits)
                .await
                .expect("read-only SELECT must succeed");
        assert_eq!(res.row_count, 1);
        assert!(res.rows[0].get("n").is_some());

        // Column order follows the SELECT statement (sqlx row description),
        // NOT the alphabetically sorted JSON keys of the row objects (the
        // dashboard Database page renders its headers from this list).
        let ordered = execute_readonly_query(&pool, "SELECT 1 AS zz, 2 AS aa", &limits)
            .await
            .expect("ordered SELECT must succeed");
        assert_eq!(
            ordered.columns,
            vec!["zz".to_string(), "aa".to_string()],
            "columns must be in SELECT order, not alphabetical JSON key order"
        );

        // int4 (integer) and int2 (smallint) columns must decode as JSON
        // numbers: sqlx's i64 decoder accepts INT8 only, and the missing
        // int4/int2 branches made those columns come back as NULL.
        let ints = execute_readonly_query(
            &pool,
            "SELECT 353::int4 AS id, 823808::int4 AS cached, 7::int2 AS small",
            &limits,
        )
        .await
        .expect("int decode SELECT must succeed");
        assert_eq!(ints.rows[0]["id"], serde_json::json!(353));
        assert_eq!(ints.rows[0]["cached"], serde_json::json!(823808));
        assert_eq!(ints.rows[0]["small"], serde_json::json!(7));

        // Write attempt is rejected before touching the database.
        assert!(matches!(
            execute_readonly_query(&pool, "DELETE FROM messages", &limits).await,
            Err(ReadOnlyQueryError::Rejected(_))
        ));

        // Table list flows through the same guard.
        let tables = list_public_tables(&pool, &limits)
            .await
            .expect("table list");
        assert!(tables.row_count > 0);
    }
}
