//! HTTP API for data retention: imperative soft/hard delete triggers + status.
//!
//! Routes (registered in src/server/mod.rs):
//!
//! * `POST /api/retention/soft-delete` - run the SOFT delete ONCE, on demand.
//! * `POST /api/retention/hard-delete` - run the HARD delete ONCE, on demand.
//! * `GET  /api/retention/status`      - configured horizon + last run per op.
//!
//! Both triggers call the SAME code path as the in-process daily schedule
//! ([`crate::retention::run_soft_delete`] / [`crate::retention::run_hard_delete`]),
//! so scheduled and imperative runs share one implementation and one semantics:
//!
//! * independent of the daily schedule (a trigger neither disables, reschedules
//!   nor double-fires the daily run, and the daily run never depends on a call);
//! * idempotent and bounded (batched deletes: a second immediate call deletes
//!   nothing new);
//! * serialized by the shared run lock, so a trigger arriving while the daily
//!   run (or another trigger) is in flight waits instead of deleting the same
//!   rows twice concurrently;
//! * `0` or empty/unset for the setting DISABLES the operation: the trigger is a
//!   clean no-op returning `status: "disabled"` with 0 rows deleted (never an
//!   error), and disabling one operation never affects the other.
//!
//! The response body is the per-table result summary
//! (`operation`, `status`, `enabled`, `days`, `rows_deleted`, `total_deleted`,
//! `duration_ms`).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use super::AppState;

/// Live configured soft-delete horizon (None = empty/unset = disabled).
fn soft_days(state: &AppState) -> Option<u32> {
    state.shared_config.read().delete_after_days_soft
}

/// Live configured hard-delete horizon (None = empty/unset = disabled).
fn hard_days(state: &AppState) -> Option<u32> {
    state.shared_config.read().delete_after_days_hard
}

/// `POST /api/retention/soft-delete` - imperative soft-delete trigger.
pub async fn soft_delete_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let days = soft_days(&state);
    match crate::retention::run_soft_delete(&state.pool, days).await {
        Ok(report) => Json(serde_json::json!({
            "trigger": "manual",
            "result": report,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "trigger": "manual",
                "operation": "soft_delete",
                "status": "error",
                "error": e.to_string(),
            })),
        )
            .into_response(),
    }
}

/// `POST /api/retention/hard-delete` - imperative hard-delete trigger.
pub async fn hard_delete_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let days = hard_days(&state);
    match crate::retention::run_hard_delete(&state.pool, days).await {
        Ok(report) => Json(serde_json::json!({
            "trigger": "manual",
            "result": report,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "trigger": "manual",
                "operation": "hard_delete",
                "status": "error",
                "error": e.to_string(),
            })),
        )
            .into_response(),
    }
}

/// `GET /api/retention/status` - schedule (one run/op/day) + last run per op.
pub async fn status_handler(
    State(state): State<Arc<AppState>>,
) -> Json<crate::retention::RetentionStatus> {
    let status = crate::retention::status(soft_days(&state), hard_days(&state));
    Json(status)
}
