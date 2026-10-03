//! Core read-only database API (`/db/query`, `/db/tables`).
//!
//! The operator requirement (telegram 2039/2040, 2026-09-15): the dashboard
//! Database page must ALWAYS work and must NOT depend on a plugin being
//! installed and enabled. These two endpoints are executed directly by core
//! against the agent database through the single read-only guard in
//! [`crate::db::readonly`] (the same guard the `search_database` MCP tool used
//! to own; that tool now DELEGATES to `POST /db/query`).
//!
//! Errors are structured and actionable: each carries a machine-readable code,
//! the requested SQL context, a remediation hint, and a matching HTTP status.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppState;
use crate::db::readonly::{
    execute_readonly_query, list_public_tables, ReadOnlyLimits, ReadOnlyQueryError,
};

#[derive(Debug, Deserialize)]
pub(crate) struct DbQueryRequest {
    /// Raw read-only SQL (must start with SELECT or WITH).
    #[serde(default)]
    pub sql: Option<String>,
}

/// Map a read-only failure to `(status, body)` with a structured payload.
fn error_response(err: &ReadOnlyQueryError) -> (StatusCode, Json<Value>) {
    let status = StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
    (
        status,
        Json(json!({
            "success": false,
            "error": err.message(),
            "error_code": err.code(),
            "remediation": err.remediation(),
        })),
    )
}

/// `POST /db/query` - run a read-only SQL statement against the agent database.
///
/// Body: `{"sql": "SELECT ..."}` -> `{"success": true, "rows": [...],
/// "columns": [...], "row_count": N}` where `columns` is the result column
/// list in STATEMENT order (sqlx `Row::columns()`); the row objects' JSON keys
/// are alphabetical and MUST NOT be used to reconstruct the SELECT order. The
/// statement is guarded by
/// [`crate::db::readonly::execute_readonly_query`]: read-only transaction,
/// SELECT/WITH only, write/DDL keywords rejected, 8 s statement timeout,
/// 1000-row cap.
pub(crate) async fn db_query_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DbQueryRequest>,
) -> (StatusCode, Json<Value>) {
    let sql = match body.sql.as_deref().map(str::trim) {
        Some(sql) if !sql.is_empty() => sql.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "success": false,
                    "error": "'sql' is required (a read-only SELECT/WITH statement).",
                    "error_code": "db_query_missing_sql",
                    "remediation": "Send {\"sql\": \"SELECT ...\"} with a single read-only statement.",
                })),
            );
        }
    };

    // Effective guard limits: operator settings with the documented code
    // defaults (audit HV-B6); read per request so a settings change is picked
    // up without a rebuild.
    let limits = ReadOnlyLimits::from_settings();
    match execute_readonly_query(&state.pool, &sql, &limits).await {
        Ok(result) => (
            StatusCode::OK,
            Json(json!({
                "success": true,
                "columns": result.columns,
                "rows": result.rows,
                "row_count": result.row_count,
            })),
        ),
        Err(err) => error_response(&err),
    }
}

/// `GET /db/tables` - the public-schema table list, read directly by core.
///
/// Requires no plugin: this is the endpoint the dashboard Database page uses
/// for its table browser.
pub(crate) async fn db_tables_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<Value>) {
    let limits = ReadOnlyLimits::from_settings();
    match list_public_tables(&state.pool, &limits).await {
        Ok(result) => (
            StatusCode::OK,
            Json(json!({
                "success": true,
                "tables": result.rows,
                "count": result.row_count,
            })),
        ),
        Err(err) => error_response(&err),
    }
}
