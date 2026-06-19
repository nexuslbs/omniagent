//! HTTP server for external control (stop, close, open, status, health)
//!
//! Provides endpoints:
//! - `GET /health` — health check
//! - `POST|GET /stop/{channel_id}` — skip pending/processing threads (no channel state change)
//! - `POST|GET /close/{channel_id}` — close channel (skip threads, cancel handler)
//! - `POST|GET /open/{channel_id}` — open channel (allow handler to start)
//! - `GET /status/{channel_id}` — channel status info
//! - `GET /prompt/{channel_name}` — show system prompt for a channel

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::db::types as queries;
use crate::prompt_builder::{build_system_prompt, MemoryStore};

/// Shared application state for the HTTP server.
#[derive(Clone)]
struct AppState {
    pool: PgPool,
    cancel_tokens: Arc<Mutex<HashMap<i64, CancellationToken>>>,
    data_dir: String,
}

/// Start the HTTP server on the given host and port.
pub async fn start_server(
    pool: PgPool,
    host: String,
    port: u16,
    cancel_tokens: Arc<Mutex<HashMap<i64, CancellationToken>>>,
    data_dir: String,
) {
    let app_state = Arc::new(AppState {
        pool,
        cancel_tokens,
        data_dir,
    });

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/stop/:channel_id", post(stop_handler))
        .route("/stop/:channel_id", get(stop_handler))
        .route("/close/:channel_id", post(close_handler))
        .route("/close/:channel_id", get(close_handler))
        .route("/open/:channel_id", post(open_handler))
        .route("/open/:channel_id", get(open_handler))
        .route("/status/:channel_id", get(status_handler))
        .route("/prompt/:channel_name", get(prompt_handler))
        .with_state(app_state);

    let addr = format!("{}:{}", host, port);
    info!("Starting HTTP server on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind HTTP server address");

    axum::serve(listener, app)
        .await
        .expect("HTTP server exited with error");
}

/// Simple health check — returns "ok".
async fn health_handler() -> &'static str {
    "ok"
}

/// Stop — mark all pending/processing threads as skipped.
/// Does NOT change channel state (open/closed). The handler continues running
/// but will find no pending threads on its next iteration.
async fn stop_handler(
    Path(channel_id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let skipped = match queries::skip_channel_threads(&state.pool, channel_id).await {
        Ok(count) => {
            info!(
                "Stop: skipped {} pending/processing threads for channel {}",
                count, channel_id
            );
            count
        }
        Err(e) => {
            error!("Stop: failed to skip threads for channel {}: {:?}", channel_id, e);
            0
        }
    };

    Json(serde_json::json!({
        "action": "stop",
        "channel_id": channel_id,
        "skipped_threads": skipped,
    }))
}

/// Close — close the channel (skips threads, cancels handler).
/// The supervisor will not spawn a new handler until the channel is opened again.
async fn close_handler(
    Path(channel_id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // 1. Mark all pending/processing threads as skipped
    let skipped = match queries::skip_channel_threads(&state.pool, channel_id).await {
        Ok(count) => {
            info!(
                "Close: skipped {} pending/processing threads for channel {}",
                count, channel_id
            );
            count
        }
        Err(e) => {
            error!("Close: failed to skip threads for channel {}: {:?}", channel_id, e);
            0
        }
    };

    // 2. Set channel as closed
    if let Err(e) = queries::close_channel(&state.pool, channel_id).await {
        error!("Close: failed to close channel {}: {:?}", channel_id, e);
        return Json(serde_json::json!({
            "status": "error",
            "error": e.to_string(),
            "channel_id": channel_id,
        }));
    }

    // 3. Cancel the channel's processing task (if running)
    let mut tokens = state.cancel_tokens.lock().await;
    let has_handler = if let Some(token) = tokens.remove(&channel_id) {
        token.cancel();
        info!("Close: cancelled processing task for channel {}", channel_id);
        true
    } else {
        false
    };

    Json(serde_json::json!({
        "action": "close",
        "channel_id": channel_id,
        "closed": true,
        "skipped_threads": skipped,
        "handler_cancelled": has_handler,
    }))
}

/// Open — reopen a closed channel so the supervisor can spawn a handler.
async fn open_handler(
    Path(channel_id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    match queries::open_channel(&state.pool, channel_id).await {
        Ok(_) => {
            info!("Open: reopened channel {}", channel_id);
            Json(serde_json::json!({
                "action": "open",
                "channel_id": channel_id,
                "closed": false,
            }))
        }
        Err(e) => {
            error!("Open: failed to open channel {}: {:?}", channel_id, e);
            Json(serde_json::json!({
                "status": "error",
                "error": e.to_string(),
                "channel_id": channel_id,
            }))
        }
    }
}

/// Status — show channel info and thread counts.
async fn status_handler(
    Path(channel_id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match queries::get_channel_status(&state.pool, channel_id).await {
        Ok(Some(status)) => {
            let has_handler = {
                let tokens = state.cancel_tokens.lock().await;
                tokens.contains_key(&channel_id)
            };
            Json(serde_json::json!({
                "channel_id": status.channel_id,
                "name": status.name,
                "platform": status.platform,
                "closed": status.closed,
                "handler_running": has_handler,
                "current_profile": status.current_profile,
                "current_model": status.current_model,
                "current_provider": status.current_provider,
                "pending_threads": status.pending_threads,
                "processing_threads": status.processing_threads,
            }))
        }
        Ok(None) => Json(serde_json::json!({
            "status": "not_found",
            "channel_id": channel_id,
        })),
        Err(e) => {
            error!("Status: failed to get status for channel {}: {:?}", channel_id, e);
            Json(serde_json::json!({
                "status": "error",
                "error": e.to_string(),
                "channel_id": channel_id,
            }))
        }
    }
}

/// Show the system prompt for a channel, using `<<<prompt>>>` as the
/// placeholder for where the user's actual message would go.
async fn prompt_handler(
    Path(channel_name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let channel = match queries::get_channel_by_name(&state.pool, &channel_name).await {
        Ok(Some(ch)) => ch,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                format!("Channel '{}' not found", channel_name),
            );
        }
        Err(e) => {
            error!("Failed to look up channel '{}': {:?}", channel_name, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", e),
            );
        }
    };

    let profile_name = if channel.current_profile.is_empty() {
        "default"
    } else {
        &channel.current_profile
    };

    let profile_path = format!("{}/profiles/{}", state.data_dir, profile_name);
    let mut memory_store = MemoryStore::new(&profile_path);
    memory_store.load_from_disk();

    let platform = channel.platform.as_deref().unwrap_or("");
    let system_prompt = build_system_prompt(&memory_store, platform, None, profile_name);

    let result = format!(
        "System Prompt:\n{}\n\n---\n\nMessages sent to LLM:\n\n{{\n  \"role\": \"system\",\n  \"content\": \"\"\"\n{}\n  \"\"\"\n}},\n{{\n  \"role\": \"user\",\n  \"content\": \"<<<prompt>>>\"\n}}",
        system_prompt, system_prompt
    );

    (StatusCode::OK, result)
}
