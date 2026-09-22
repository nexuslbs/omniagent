//! HTTP handlers for the generic PUBLISHED-EVENT bus (see `crate::events`).
//!
//! Endpoints:
//! - `POST /events/publish`: publish a named event (fan-out to every listener
//!   hook bound to that event name) and return the per-listener deliveries.
//! - `POST /events/wait`: bounded wait for the terminal event of one
//!   correlation id -> `solved` | `aborted` | `timeout` | `pending`.
//! - `GET  /events`: list every known interaction (newest first).
//! - `GET  /events/{correlation_id}`: describe one interaction.
//!
//! These endpoints are channel agnostic: nothing here knows about telegram,
//! email or any other delivery mechanism (that is a listener's job).

use axum::{
    extract::Path,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use super::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct PublishRequest {
    /// Event name, e.g. `solve-captcha` / `solve-captcha-resolved`.
    pub(crate) event: String,
    /// Correlation id: generated when omitted, echoed back in the response.
    #[serde(default)]
    pub(crate) correlation_id: Option<String>,
    /// Bounded JSON payload (session_id, url, reason, access hint, timeout_s...).
    #[serde(default)]
    pub(crate) payload: Value,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WaitRequest {
    pub(crate) correlation_id: String,
    /// Bound for THIS call (default 60 s); the interaction deadline (payload
    /// `timeout_s`, default 900 s) publishes the `-timeout` terminal event.
    #[serde(default)]
    pub(crate) timeout_s: Option<i64>,
}

pub(crate) fn events_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/events", get(list_handler))
        .route("/events/publish", post(publish_handler))
        .route("/events/wait", post(wait_handler))
        .route("/events/{correlation_id}", get(describe_handler))
}

async fn publish_handler(Json(req): Json<PublishRequest>) -> impl IntoResponse {
    let payload = if req.payload.is_null() {
        json!({})
    } else {
        req.payload
    };
    match crate::events::publish(&req.event, req.correlation_id, payload).await {
        Ok(result) => (
            StatusCode::OK,
            Json(serde_json::to_value(result).unwrap_or(Value::Null)),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{:#}", e) })),
        ),
    }
}

async fn wait_handler(Json(req): Json<WaitRequest>) -> impl IntoResponse {
    let timeout_s = req.timeout_s.filter(|s| *s > 0).or(Some(60));
    let outcome = crate::events::wait(&req.correlation_id, timeout_s).await;
    (
        StatusCode::OK,
        Json(serde_json::to_value(outcome).unwrap_or(Value::Null)),
    )
}

async fn list_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({ "interactions": crate::events::list() })),
    )
}

async fn describe_handler(Path(correlation_id): Path<String>) -> impl IntoResponse {
    match crate::events::describe(&correlation_id) {
        Some(interaction) => (
            StatusCode::OK,
            Json(json!({ "interaction": interaction })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown correlation_id", "correlation_id": correlation_id })),
        ),
    }
}
