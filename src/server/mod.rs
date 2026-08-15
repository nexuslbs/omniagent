//! HTTP server for external control (stop, close, open, status, health)
//!
//! Provides endpoints:
//! - `GET /health`: health check
//! - `POST|GET /stop/{channel_id}`: skip pending/processing threads (no channel state change)
//! - `POST|GET /close/{channel_id}`: close channel (skip threads, cancel handler)
//! - `POST|GET /open/{channel_id}`: open channel (allow handler to start)
//! - `GET /status/{channel_id}`: channel status info
//! - `GET /prompt/{channel_name}`: show system prompt for a channel
//! - `POST /prompt-preview/{channel_name}`: preview full prompt (no DB writes), optionally plan
//! - `POST /run-cron/{schedule_id}`: manually trigger a cron job (proxied from dashboard)

pub(crate) mod actions;
pub(crate) mod channels;
pub(crate) mod hooks;
pub(crate) mod kanban;
pub(crate) mod llm_proxy;
pub(crate) mod memory;
pub(crate) mod messages;
pub(crate) mod overview;
pub(crate) mod platforms;
pub(crate) mod schedule;
mod secrets;
pub(crate) mod settings;
pub(crate) mod threads;
use crate::error::{AppResult, ErrorContext};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
/// Row type: kanban task status + thread_status (transition lookups).
#[derive(sqlx::FromRow)]
struct TaskStatusRow {
    status: Option<String>,
    thread_status: Option<String>,
}

/// Row type: thread id + optional kanban task id (+ channel for lookups).
#[derive(sqlx::FromRow)]
struct ThreadTaskRow {
    id: i64,
    channel_id: String,
    task_id: Option<String>,
    /// Thread status at stop time. stop_thread_handler uses it to decide
    /// whether to cancel the channel handler; stop/close carry it along.
    status: Option<String>,
}

use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::agent::config::AgentConfig;
use crate::agent::kanban_updater::transition_with_comment;
use crate::agent::plugin_manager::PluginManager;
use crate::db::types as queries;
use crate::llm::{ChatMessage, CompletionRequest, LLMClient};
use crate::mcp::{AppContext, McpToolCall};
use parking_lot::RwLock;

mod diagnostic;

// ── Shared response helpers ────────────────────────────────────────────────
// Used by threads.rs, channels.rs, etc. for consistent JSON response format.
// Existing modules (messages.rs, secrets.rs) have their own copies.

/// Wrap success data: `{ "success": true, "data": ... }`
pub(crate) fn ok_json<T: Serialize>(data: T) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "success": true, "data": data })),
    )
}

/// Wrap error: `{ "success": false, "error": "..." }`
pub(crate) fn err_json(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "success": false, "error": msg })),
    )
}
pub mod plugins;
pub mod plugins_compile;
pub mod plugins_delete;
pub mod plugins_enable;
pub mod plugins_env;
pub mod plugins_install;
pub mod plugins_listing;
pub mod plugins_reload;
pub mod plugins_setup;
pub mod plugins_types;

/// Type alias for the platform restart signals map.
/// Each entry: (restart_count, stopped_flag, notify)
pub(crate) type PlatformRestartSignals =
    Arc<Mutex<HashMap<String, (Arc<AtomicU64>, Arc<AtomicBool>, Arc<Notify>)>>>;

/// Shared application state for the HTTP server.
#[derive(Clone)]
pub(crate) struct AppState {
    pool: PgPool,
    cancel_tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
    data_dir: String,
    /// Default profile name (from global config default_profile setting)
    default_profile: String,
    /// Path to the .env file for settings API
    env_path: String,
    /// Application context for MCP tool execution
    app_context: AppContext,
    /// Shared mutable config for hot-reload support
    shared_config: Arc<RwLock<AgentConfig>>,
    /// Per-platform restart signal flags + notify (keyed by plugin name)
    platform_restart_signals: PlatformRestartSignals,
    /// Plugin manager — single authority for all plugin lifecycle operations
    plugin_manager: Arc<dyn PluginManager>,
}

/// Configuration for the HTTP server.
#[derive(Clone)]
pub struct ServerConfig {
    pub pool: PgPool,
    pub host: String,
    pub port: u16,
    pub cancel_tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
    pub data_dir: String,
    pub default_profile: String,
    pub app_context: AppContext,
    pub shared_config: Arc<RwLock<AgentConfig>>,
    pub platform_restart_signals: PlatformRestartSignals,
    pub plugin_manager: Arc<dyn PluginManager>,
}

/// Start the HTTP server on the given host and port.
pub async fn start_server(config: ServerConfig) -> AppResult<()> {
    let app_state = Arc::new(AppState {
        pool: config.pool,
        cancel_tokens: config.cancel_tokens,
        data_dir: config.data_dir.clone(),
        default_profile: config.default_profile.clone(),
        env_path: format!("{}/.env", config.data_dir),
        // plugin_manager replaces tool_registry
        app_context: config.app_context,
        shared_config: config.shared_config,
        platform_restart_signals: config.platform_restart_signals,
        plugin_manager: config.plugin_manager,
    });

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/stop/{channel_id}", post(stop_handler))
        .route("/stop/{channel_id}", get(stop_handler))
        .route("/stop-thread/{thread_id}", post(stop_thread_handler))
        .route("/close/{channel_id}", post(close_handler))
        .route("/close/{channel_id}", get(close_handler))
        .route("/open/{channel_id}", post(open_handler))
        .route("/open/{channel_id}", get(open_handler))
        .route("/status/{channel_id}", get(status_handler))
        .route("/prompt/{channel_name}", get(prompt_handler))
        .route(
            "/prompt-preview/{channel_name}",
            post(prompt_preview_handler),
        )
        .route("/mcp/tools", get(list_mcp_tools_handler))
        .route("/mcp/execute", post(execute_mcp_tool_handler))
        // ── Context preview (section [3] only, no messages written) ──
        .route("/api/context/{channel_name}", get(context_preview_handler))
        // ── Plugin management routes ──
        .route("/api/plugins/ping", get(|| async { "pong" }))
        .route("/api/plugins/check-state", get(diagnostic::check_state))
        .route("/api/plugins/check-db", get(diagnostic::check_db))
        .route(
            "/api/plugins/check-list",
            get(diagnostic::check_list_plugins),
        )
        .route("/api/plugins/check-env", get(diagnostic::check_env_read))
        .route(
            "/api/plugins/check-enrich",
            get(diagnostic::check_enrich_json),
        )
        // ── Plugin CRUD routes (from plugin_router) ──
        .merge(plugins::plugin_router())
        // ── Env reload (hot-reload .env without restart) ──
        .route("/api/reload", post(plugins::reload_env_handler))
        // ── Plugin restart (disable + enable cycle) ──
        .route(
            "/api/plugins/{type}/{source}/{name}/restart",
            post(plugins::restart_plugin_handler),
        )
        // ── LLM Proxy (allows MCP plugins to use provider infrastructure) ──
        .route("/api/llm/chat", post(llm_proxy::llm_chat_handler))
        // ── Settings routes ──
        .route("/settings", get(settings::get_settings_handler))
        .route("/settings", put(settings::update_settings_handler))
        // ── Secrets routes ──
        .merge(secrets::secrets_router())
        // ── Messages API routes ──
        .merge(messages::messages_router())
        // ── Threads API routes ──
        .merge(threads::threads_router())
        // ── Channels API routes ──
        .merge(channels::channels_router())
        // ── Overview / Dashboard routes ──
        .merge(overview::overview_router())
        // ── Memory API routes (stats + search) ──
        .merge(memory::memory_router())
        // ── Platforms API routes ──
        .merge(platforms::platforms_router())
        // ── Kanban API routes ──
        .merge(kanban::kanban_router())
        // ── Schedule API routes (replaces dashboard schedule.ts) ──
        .merge(schedule::schedule_router())
        .merge(hooks::hooks_router())
        // ── Actions CRUD routes (backed by actions.yml) ──
        .route("/actions", get(actions::list_actions_handler))
        .route("/actions", post(actions::create_action_handler))
        .route("/actions/{id}", put(actions::update_action_handler))
        .route("/actions/{id}", delete(actions::delete_action_handler))
        .route("/actions/{id}/run", post(actions::run_action_handler))
        // ── Cron run endpoint ──
        .route("/run-cron/{schedule_id}", post(run_cron_handler))
        .with_state(app_state);

    let addr = format!("{}:{}", config.host, config.port);
    info!("Starting HTTP server on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .ctx("Failed to bind HTTP server address")?;

    axum::serve(listener, app)
        .await
        .ctx("HTTP server exited with error")?;

    Ok(())
}

/// Simple health check: returns "ok".
async fn health_handler() -> &'static str {
    "ok"
}

/// Pure decision: must `stop-thread` cancel the channel handler?
///
/// Only when the target thread was actively `processing`: the handler
/// processes one thread at a time per channel, so a `pending` target (or any
/// other state) means the handler is running a DIFFERENT thread or is idle —
/// cancelling it would silently kill that unrelated thread.
fn stop_thread_cancels_handler(target_status: Option<&str>) -> bool {
    matches!(target_status, Some("processing"))
}

/// Pure decision: the thread_status value to persist on the kanban task after
/// an explicit stop. `Block` with the clear flag drops the marker; `Block`
/// without it keeps the current marker. `Noop` always drops the marker when
/// one is set (the task must not keep pointing at a stopped thread) while
/// leaving the task's own status untouched.
fn stop_recovery_thread_status(
    recovery: &queries::StopRecovery,
    current: Option<&str>,
) -> Option<String> {
    match recovery {
        queries::StopRecovery::Block {
            clear_thread_status: true,
            ..
        } => None,
        queries::StopRecovery::Block {
            clear_thread_status: false,
            ..
        } => current.map(String::from),
        queries::StopRecovery::Noop => None,
    }
}

/// Stop: mark all pending/processing threads as skipped and cancel
/// the channel's executor so it restarts fresh.
/// Phase 6b: apply the explicit-stop outcome for one kanban-linked thread.
///
/// The thread has already been (or will be) skipped; this only decides whether
/// its kanban task should move to `blocked` (with thread_status cleared).
/// Non-kanban threads (task_id NULL) and terminal/manual-review tasks are left
/// untouched - no retry is consumed and no re-run thread is created. Returns
/// true when the task was moved to blocked.
async fn apply_stop_recovery(
    pool: &PgPool,
    thread_id: i64,
    task_id: Option<&str>,
    operator: &str,
) -> Result<bool, String> {
    // Non-kanban thread (task_id NULL): skip only, no task transition.
    let Some(task_id) = task_id else {
        return Ok(false);
    };

    // Fetch the task's current status + thread_status to decide the outcome.
    let task = sql_forge!(
        TaskStatusRow,
        "SELECT status, thread_status FROM kanban_tasks WHERE id = :task_id",
        ( :task_id = task_id )
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;

    // Task gone: nothing to transition.
    let Some(task) = task else {
        return Ok(false);
    };

    match queries::stop_thread_recovery(task.status.as_deref(), task.thread_status.as_deref()) {
        queries::StopRecovery::Block {
            new_status,
            clear_thread_status,
        } => {
            let comment = format!(
                "Task blocked: thread #{} stopped explicitly (operator {})",
                thread_id, operator
            );
            let thread_status = stop_recovery_thread_status(
                &queries::StopRecovery::Block {
                    new_status,
                    clear_thread_status,
                },
                task.thread_status.as_deref(),
            );
            transition_with_comment(
                pool,
                task_id,
                new_status,
                thread_status.as_deref(),
                &comment,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(true)
        }
        queries::StopRecovery::Noop => {
            // The task is NOT moved (todo/backlog/done/manual review stay put),
            // but its thread_status must not keep pointing at the stopped
            // thread: clear the marker when one is set (task status untouched).
            if task.thread_status.is_some() {
                sql_forge!(
                    "UPDATE kanban_tasks SET thread_status = NULL WHERE id = :task_id AND thread_status IS NOT NULL",
                    ( :task_id = task_id )
                )
                .execute(pool)
                .await
                .map_err(|e| e.to_string())?;
            }
            Ok(false)
        }
    }
}

/// Stop: explicitly stop all pending/processing threads for a channel.
///
/// Phase 6b: unlike a failure (which re-schedules), an explicit stop BLOCKS the
/// kanban tasks of the skipped threads and clears their thread_status - no
/// retry is consumed and no re-run thread is created. The channel stays open.
async fn stop_handler(
    Path(channel_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // 1. Collect pending/processing threads (id + kanban task) BEFORE skipping
    let threads = match     sql_forge!(
        ThreadTaskRow,
        "SELECT id, channel_id, task_id, status FROM threads WHERE channel_id = :channel_id AND status IN ('pending', 'processing')",
        ( :channel_id = channel_id.as_str() )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!(
                "Stop: failed to list threads for channel {}: {:?}",
                channel_id, e
            );
            return Json(serde_json::json!({
                "status": "error",
                "error": e.to_string(),
                "channel_id": channel_id,
            }));
        }
    };

    // 2. Mark them all as skipped (plain skip - no reschedule, no re-run thread).
    //    Every terminal write funnels through queries::mark_thread_terminal so
    //    the terminal=true invariant holds on the skipped rows.
    let mut skipped = 0u64;
    for row in &threads {
        match queries::mark_thread_terminal(&state.pool, row.id, "skipped").await {
            Ok(n) => skipped += n,
            Err(e) => {
                error!(
                    "Stop: failed to skip thread {} for channel {}: {:?}",
                    row.id, channel_id, e
                );
                return Json(serde_json::json!({
                    "status": "error",
                    "error": e.to_string(),
                    "channel_id": channel_id,
                }));
            }
        }
    }
    info!(
        "Stop: skipped {} pending/processing threads for channel {}",
        skipped, channel_id
    );

    // 3. Phase 6b: block the kanban tasks of the skipped threads
    let mut blocked = 0u32;
    for row in &threads {
        match apply_stop_recovery(&state.pool, row.id, row.task_id.as_deref(), "stop").await {
            Ok(true) => blocked += 1,
            Ok(false) => {}
            Err(e) => error!(
                "Stop: failed to apply recovery for thread {}: {}",
                row.id, e
            ),
        }
    }
    if blocked > 0 {
        info!(
            "Stop: blocked {} kanban task(s) for channel {}",
            blocked, channel_id
        );
    }

    // 4. Cancel the channel's processing task (if running)
    let mut tokens = state.cancel_tokens.lock().await;
    let has_handler = if let Some(token) = tokens.remove(&channel_id) {
        token.cancel();
        info!("Stop: cancelled processing task for channel {}", channel_id);
        true
    } else {
        false
    };

    Json(serde_json::json!({
        "action": "stop",
        "channel_id": channel_id,
        "skipped_threads": skipped,
        "blocked_tasks": blocked,
        "handler_cancelled": has_handler,
    }))
}

/// Stop-thread: explicitly stop a single thread.
///
/// Phase 6b: the thread is skipped (no retry consumed) and, if it is
/// kanban-linked with an active status, its task moves to blocked with
/// thread_status cleared.
async fn stop_thread_handler(
    Path(thread_id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // 1. Look up the thread's channel id + kanban task id
    let (channel_id, task_id, status) = match sql_forge!(
        ThreadTaskRow,
        "SELECT id, channel_id, task_id, status FROM threads WHERE id = :thread_id",
        ( :thread_id = thread_id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => (row.channel_id, row.task_id, row.status),
        Ok(None) => {
            return Json(serde_json::json!({
                "status": "error",
                "error": format!("thread {} not found", thread_id),
            }))
        }
        Err(e) => {
            error!(
                "Stop-thread: failed to look up thread {}: {:?}",
                thread_id, e
            );
            return Json(serde_json::json!({
                "status": "error",
                "error": e.to_string(),
                "thread_id": thread_id,
            }));
        }
    };

    // 2. Skip the thread (plain skip - no retry consumed, no re-run)
    let skipped = match queries::skip_thread(&state.pool, thread_id).await {
        Ok(count) => count,
        Err(e) => {
            error!("Stop-thread: failed to skip thread {}: {:?}", thread_id, e);
            return Json(serde_json::json!({
                "status": "error",
                "error": e.to_string(),
                "thread_id": thread_id,
                "channel_id": channel_id,
            }));
        }
    };
    info!("Stop-thread: skipped thread {}", thread_id);

    // 3. Platform reaction handling is done by the platforms themselves when
    //    they see the thread skipped; fetching the cause message preserves the
    //    original behavior.
    if skipped > 0 {
        let _ = crate::db::threads::get_cause_message(&state.pool, thread_id).await;
    }

    // 4. Phase 6b: block the thread's kanban task (if any)
    let blocked = match apply_stop_recovery(
        &state.pool,
        thread_id,
        task_id.as_deref(),
        "stop-thread",
    )
    .await
    {
        Ok(true) => {
            info!(
                "Stop-thread: blocked kanban task {} for thread {}",
                task_id.as_deref().unwrap_or(""),
                thread_id
            );
            true
        }
        Ok(false) => false,
        Err(e) => {
            error!(
                "Stop-thread: failed to apply recovery for thread {}: {}",
                thread_id, e
            );
            false
        }
    };

    // 5. Cancel the channel's processing task ONLY when the target thread was
    //    the one actively being processed (status 'processing' at lookup time).
    //    The handler processes one thread at a time per channel, so any other
    //    target state means the handler is running a DIFFERENT thread — or is
    //    idle — and must NOT be cancelled (stopping one thread must never kill
    //    an unrelated in-flight thread). The skip in step 2 already made the
    //    target terminal, so the handler can no longer claim it and the
    //    supervisor keeps the channel handler running for remaining threads.
    let mut tokens = state.cancel_tokens.lock().await;
    let has_handler = if stop_thread_cancels_handler(status.as_deref()) {
        if let Some(token) = tokens.remove(&channel_id) {
            token.cancel();
            info!(
                "Stop-thread: cancelled processing task for channel {}",
                channel_id
            );
            true
        } else {
            false
        }
    } else {
        info!(
            "Stop-thread: thread {} was not processing; channel {} handler left running",
            thread_id, channel_id
        );
        false
    };

    Json(serde_json::json!({
        "action": "stop-thread",
        "thread_id": thread_id,
        "channel_id": channel_id,
        "skipped": skipped,
        "task_blocked": blocked,
        "handler_cancelled": has_handler,
    }))
}

/// Close: explicitly stop all pending/processing threads for a channel and
/// mark the channel closed.
///
/// Phase 6b: like stop, the kanban tasks of the skipped threads move to blocked
/// (thread_status cleared) - no retry consumed, no re-run thread.
async fn close_handler(
    Path(channel_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // 1. Collect pending/processing threads (id + kanban task) BEFORE skipping
    let threads = match     sql_forge!(
        ThreadTaskRow,
        "SELECT id, channel_id, task_id, status FROM threads WHERE channel_id = :channel_id AND status IN ('pending', 'processing')",
        ( :channel_id = channel_id.as_str() )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!(
                "Close: failed to list threads for channel {}: {:?}",
                channel_id, e
            );
            return Json(serde_json::json!({
                "status": "error",
                "error": e.to_string(),
                "channel_id": channel_id,
            }));
        }
    };

    // 2. Mark them all as skipped (plain skip - no reschedule, no re-run thread).
    //    Every terminal write funnels through queries::mark_thread_terminal so
    //    the terminal=true invariant holds on the skipped rows.
    let mut skipped = 0u64;
    for row in &threads {
        match queries::mark_thread_terminal(&state.pool, row.id, "skipped").await {
            Ok(n) => skipped += n,
            Err(e) => {
                error!(
                    "Close: failed to skip thread {} for channel {}: {:?}",
                    row.id, channel_id, e
                );
                return Json(serde_json::json!({
                    "status": "error",
                    "error": e.to_string(),
                    "channel_id": channel_id,
                }));
            }
        }
    }
    info!(
        "Close: skipped {} pending/processing threads for channel {}",
        skipped, channel_id
    );

    // 3. Phase 6b: block the kanban tasks of the skipped threads
    let mut blocked = 0u32;
    for row in &threads {
        match apply_stop_recovery(&state.pool, row.id, row.task_id.as_deref(), "close").await {
            Ok(true) => blocked += 1,
            Ok(false) => {}
            Err(e) => error!(
                "Close: failed to apply recovery for thread {}: {}",
                row.id, e
            ),
        }
    }
    if blocked > 0 {
        info!(
            "Close: blocked {} kanban task(s) for channel {}",
            blocked, channel_id
        );
    }

    // 4. Set channel as closed
    if let Err(e) = queries::close_channel(&state.pool, &channel_id).await {
        error!("Close: failed to close channel {}: {:?}", channel_id, e);
        return Json(serde_json::json!({
            "status": "error",
            "error": e.to_string(),
            "channel_id": channel_id,
        }));
    }

    // 5. Cancel the channel's processing task (if running)
    let mut tokens = state.cancel_tokens.lock().await;
    let has_handler = if let Some(token) = tokens.remove(&channel_id) {
        token.cancel();
        info!(
            "Close: cancelled processing task for channel {}",
            channel_id
        );
        true
    } else {
        false
    };

    Json(serde_json::json!({
        "action": "close",
        "channel_id": channel_id,
        "closed": true,
        "skipped_threads": skipped,
        "blocked_tasks": blocked,
        "handler_cancelled": has_handler,
    }))
}

/// Open: reopen a closed channel so the supervisor can spawn a handler.
async fn open_handler(
    Path(channel_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    match queries::open_channel(&state.pool, &channel_id).await {
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

/// Status: show channel info and thread counts.
async fn status_handler(
    Path(channel_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match queries::get_channel_status(&state.pool, &channel_id).await {
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
            error!(
                "Status: failed to get status for channel {}: {:?}",
                channel_id, e
            );
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
        Ok(Some(ch)) => Some(ch),
        Ok(None) => {
            // Channel not found: build system prompt using the default profile
            None
        }
        Err(e) => {
            error!("Failed to look up channel '{}': {:?}", channel_name, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", e),
            );
        }
    };

    let profile_name = match channel.as_ref() {
        Some(ch) if !ch.current_profile.is_empty() => &ch.current_profile,
        _ => &state.default_profile,
    };

    let profile_path = format!("{}/profiles/{}", state.data_dir, profile_name);
    let memories_dir = std::path::Path::new(&profile_path).join("memories");
    let memory_raw = if memories_dir.join("MEMORY.md").exists() {
        std::fs::read_to_string(memories_dir.join("MEMORY.md")).unwrap_or_default()
    } else {
        String::new()
    };
    let user_raw = if memories_dir.join("USER.md").exists() {
        std::fs::read_to_string(memories_dir.join("USER.md")).unwrap_or_default()
    } else {
        String::new()
    };

    let _platform = channel
        .as_ref()
        .and_then(|c| c.platform.as_deref())
        .unwrap_or("");
    let tool_names: Vec<String> = state
        .plugin_manager
        .snapshot_registry()
        .await
        .all()
        .iter()
        .map(|t| t.full_name.clone())
        .collect();
    let mut segments: Vec<String> = Vec::new();

    // Stable tier: simple identity + tool guidance
    let tool_list = if tool_names.is_empty() {
        String::new()
    } else {
        tool_names.join(", ")
    };
    segments.push(format!("You are OmniAgent: precise, efficient, autonomous. Your tools: {tool_list}. Use minimum roundtrips. If a tool fails, move on: don't retry more than twice. HONESTY RULE: if you cannot complete the task, your final summary MUST clearly state that you gave up and why, and what remains undone — NEVER claim the task was completed unless every requested step was actually done and verified."));
    segments.push(format!("Active Hermes profile: {profile_name}."));

    // Volatile tier: memory/soul placeholders
    let separator = "═".repeat(46);
    let mut locked_entries: Vec<String> = Vec::new();

    if !memory_raw.is_empty() {
        locked_entries.push(format!(
            "{}\n## MEMORY (your personal notes)\n{}\n\n<<memory>>",
            separator, separator
        ));
    }
    if !user_raw.is_empty() {
        locked_entries.push(format!(
            "{}\n## USER PROFILE (who the user is)\n{}\n\n<<soul>>",
            separator, separator
        ));
    }

    if !locked_entries.is_empty() {
        let locked_content = locked_entries.join("\n\n");
        segments.push(format!(
            "═══ LOCKED INSTRUCTIONS (FOLLOW EXACTLY) ═══\n{}",
            locked_content
        ));
    }

    let template = segments.join("\n\n");
    (StatusCode::OK, template)
}

// ── Prompt preview endpoint ──

#[derive(Deserialize)]
struct PromptPreviewRequest {
    prompt: String,
    plan: bool,
}

#[derive(Serialize)]
#[allow(dead_code)]
struct PromptPreviewResponse {
    system_prompt: String,
    messages: Vec<serde_json::Value>,
    plan: Option<bool>,
}

async fn prompt_preview_handler(
    Path(channel_name): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<PromptPreviewRequest>,
) -> impl IntoResponse {
    let channel = match queries::get_channel_by_name(&state.pool, &channel_name).await {
        Ok(Some(ch)) => Some(ch),
        Ok(None) => {
            // Channel not found: build system prompt using the default profile
            None
        }
        Err(e) => {
            error!("Failed to look up channel '{}': {:?}", channel_name, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Database error: {}", e) })),
            );
        }
    };

    let profile_name = match channel.as_ref() {
        Some(ch) if !ch.current_profile.is_empty() => &ch.current_profile,
        _ => &state.default_profile,
    };

    let profile_path = format!("{}/profiles/{}", state.data_dir, profile_name);
    let memories_dir = std::path::Path::new(&profile_path).join("memories");
    let memory_raw = if memories_dir.join("MEMORY.md").exists() {
        std::fs::read_to_string(memories_dir.join("MEMORY.md")).unwrap_or_default()
    } else {
        String::new()
    };

    let platform = channel
        .as_ref()
        .and_then(|c| c.platform.as_deref())
        .unwrap_or("");
    let tool_names: Vec<String> = state
        .plugin_manager
        .snapshot_registry()
        .await
        .all()
        .iter()
        .map(|t| t.full_name.clone())
        .collect();
    let tool_list = if tool_names.is_empty() {
        String::new()
    } else {
        tool_names.join(", ")
    };
    let system_prompt = format!(
        "You are OmniAgent: precise, efficient, autonomous. Your tools: {tool_list}. Use minimum roundtrips. If a tool fails, move on: don't retry more than twice. HONESTY RULE: if you cannot complete the task, your final summary MUST clearly state that you gave up and why, and what remains undone — NEVER claim the task was completed unless every requested step was actually done and verified.\n\nActive profile: {profile_name}.\n\n{}",
        if !memory_raw.is_empty() { format!("## MEMORY (your personal notes)\n{memory_raw}") } else { String::new() }
    );

    let mut messages = vec![serde_json::json!({ "role": "system", "content": &system_prompt })];

    // ── Build the [3] Context section using the same logic as the agent ──
    // Uses the latest thread in the channel (if any) with the preview prompt
    // as the cause content, so the context reflects what would actually be
    // assembled when a real message is processed.
    if let Some(ch) = &channel {
        if let Ok(Some(latest)) = queries::get_latest_seq0_message(&state.pool, &ch.id).await {
            if let Ok(Some(tid)) = queries::get_message_thread(&state.pool, latest.id).await {
                let profile_registry = crate::profile::ProfileRegistry::new(&state.data_dir);
                let _prof = profile_registry
                    .get(profile_name)
                    .cloned()
                    .unwrap_or_else(|| crate::profile::Profile::default(profile_name));

                // Use the prompt tool for context (same tool the agent uses)
                let context_text = call_prompt_context(
                    &state.plugin_manager,
                    &state.app_context,
                    profile_name,
                    platform,
                    &body.prompt,
                    tid,
                    &ch.id,
                )
                .await;

                if !context_text.is_empty() {
                    messages.push(serde_json::json!({
                        "role": "system",
                        "content": format!("=== Additional Context ===\n{}", context_text)
                    }));
                }
            }
        }
    }

    // Add user prompt
    messages.push(serde_json::json!({ "role": "cause", "content": body.prompt }));

    let plan = if body.plan {
        // Resolve provider/model: channel > profile > env
        let profile_registry = crate::profile::ProfileRegistry::new(&state.data_dir);
        let prof = profile_registry
            .get(profile_name)
            .cloned()
            .unwrap_or_else(|| crate::profile::Profile::default(profile_name));

        let ch_provider = channel.as_ref().and_then(|ch| ch.current_provider.clone());
        let ch_model = channel.as_ref().and_then(|ch| ch.current_model.clone());

        let provider_name = match ch_provider
            .filter(|s| !s.is_empty())
            .or_else(|| prof.provider.clone().filter(|s| !s.is_empty()))
            .or_else(|| {
                crate::agent::config::get_global()
                    .map(|g| g.read().default_provider.clone())
                    .filter(|s| !s.is_empty())
            }) {
            Some(p) => p,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "No LLM provider configured: set LLM_PROVIDER env var or configure channel/provider profile"
                    })),
                );
            }
        };

        let model_name = match ch_model
            .filter(|s| !s.is_empty())
            .or_else(|| prof.model.clone().filter(|s| !s.is_empty()))
            .or_else(|| crate::llm::resolve_default_model(&provider_name))
        {
            Some(m) => m,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "No LLM model configured: channel, profile, or provider plugin default_model must define one"
                    })),
                );
            }
        };

        // Resolve provider enum for the resolved provider name
        let resolved_provider = crate::llm::ProviderId::new(&provider_name);

        // Build planning prompt inline
        let tool_list = if tool_names.is_empty() {
            String::new()
        } else {
            format!("Your available tools: {}.", tool_names.join(", "))
        };
        let planning_prompt = format!(
            "## Plan\nBefore responding, create a high-level plan with numbered steps. \
{tool_list}\nBe specific about which tool to use and what parameters to pass. \
Aim for the minimum number of steps to complete the task. \
Wrap your plan in a <plan> block. After delivering the final answer, \
evaluate: if the task was completed, call the completion tool.",
            tool_list = tool_list
        );

        // Create LLM client: resolve api_key from provider plugin config
        // (not from hardcoded {PROVIDER}_API_KEY env var names).
        let base_url = crate::llm::resolve_default_base_url(&provider_name);

        // Look up api_key from the provider's resolved plugin config
        let api_key = match crate::plugins_yaml::get_plugin(
            &state.data_dir,
            &provider_name,
            &crate::plugins_yaml::PluginYamlType::Provider,
        ) {
            Ok(Some(detail)) => detail
                .config
                .get("api_key")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let api_mode = crate::llm::ApiMode::resolve(&provider_name, &model_name);

        let llm_config = crate::llm::LLMConfig {
            provider: resolved_provider,
            api_key,
            base_url,
            model: model_name,
            api_mode,
            max_tokens: 1024,
            temperature: 0.3,
            supports_reasoning: crate::llm::PROVIDER_METADATA
                .read()
                .get(&provider_name)
                .map(|m| m.supports_reasoning)
                .unwrap_or(false),
        };
        let llm = LLMClient::new(llm_config);

        let plan_request = CompletionRequest {
            messages: vec![ChatMessage::system(&planning_prompt)],
            max_tokens: 1024,
            temperature: 0.3,
            stream: false,
            tools: None,
        };

        match llm.completion(plan_request).await {
            Ok(resp) => {
                let plan_content = resp.content;
                messages.push(serde_json::json!({ "role": "agent", "msg_type": "plan", "content": plan_content }));
                Some(plan_content)
            }
            Err(e) => {
                let err_msg = format!("Planning failed: {}", e);
                messages.push(
                    serde_json::json!({ "role": "agent", "msg_type": "plan", "content": err_msg }),
                );
                Some(err_msg)
            }
        }
    } else {
        None
    };

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "system_prompt": messages[0]["content"].as_str().unwrap_or(""),
            "messages": messages,
            "plan": plan,
        })),
    )
}

/// GET /mcp/tools: list all registered MCP tools with their input schemas.
async fn list_mcp_tools_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let tools: Vec<serde_json::Value> = state
        .plugin_manager
        .snapshot_registry()
        .await
        .all()
        .iter()
        .map(|t| {
            serde_json::json!({
                "full_name": t.full_name,
                "description": t.description,
                "input_schema": t.input_schema,
                "server_name": t.server_name,
            })
        })
        .collect();
    Json(serde_json::json!(tools))
}

/// Request body for `POST /mcp/execute`.
#[derive(serde::Deserialize)]
struct McpExecuteRequest {
    name: String,
    arguments: Option<serde_json::Value>,
    /// Optional runtime context, mirroring the `_meta` the agent injects on
    /// every tool call (keys: channel_id, thread_id, channel_name,
    /// profile_name, platform). Accepts either `meta` or `_meta`.
    /// Any field NOT provided defaults to the default profile, platform "cli",
    /// and empty channel/thread.
    #[serde(default, alias = "_meta")]
    meta: Option<serde_json::Value>,
}

/// POST /mcp/execute: execute any registered MCP tool by name.
/// Stateless: accepts tool name + arguments (+ optional context), returns tool result.
/// Useful for testing stateless tools like compact_messages and
/// generate_initial_prompt without needing a channel or database.
async fn execute_mcp_tool_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<McpExecuteRequest>,
) -> Json<serde_json::Value> {
    let args = body.arguments.unwrap_or(serde_json::json!({}));
    let call = crate::mcp::McpToolCall {
        id: "api-exec".to_string(),
        name: body.name,
        arguments: args,
    };

    // Build the tool-call context the same way the agent loop does: start from
    // the shared app context, apply the caller-provided meta fields (if any),
    // then fill in DEFAULTS for anything still missing — the default profile,
    // platform "cli", and empty channel/thread. This keeps every tool call
    // consistent: plugins receive _meta with a profile/platform even when the
    // caller did not specify one.
    let mut ctx = state.app_context.clone();
    if let Some(meta_obj) = body.meta.as_ref().and_then(|v| v.as_object()) {
        if let Some(cid) = meta_obj.get("channel_id").and_then(|v| v.as_str()) {
            if !cid.is_empty() {
                ctx.current_channel_id = Some(cid.to_string());
            }
        }
        if let Some(tid) = meta_obj.get("thread_id").and_then(|v| v.as_i64()) {
            ctx.current_thread_id = Some(tid);
        }
        if let Some(pn) = meta_obj.get("profile_name").and_then(|v| v.as_str()) {
            if !pn.is_empty() {
                ctx.current_profile_name = Some(pn.to_string());
            }
        }
        if let Some(cn) = meta_obj.get("channel_name").and_then(|v| v.as_str()) {
            if !cn.is_empty() {
                ctx.current_channel_name = Some(cn.to_string());
            }
        }
        if let Some(pl) = meta_obj.get("platform").and_then(|v| v.as_str()) {
            if !pl.is_empty() {
                ctx.current_platform = Some(pl.to_string());
            }
        }
    }
    // Defaults when not informed: default profile, cli platform, empty channel/thread.
    ctx.current_profile_name
        .get_or_insert_with(|| state.default_profile.clone());
    ctx.current_platform
        .get_or_insert_with(|| "cli".to_string());

    // Default channel for CLI tool calls: when the caller did not provide a
    // channel, fall back to the `default_cli_channel` setting (a select over
    // the existing channels). Unknown/empty setting -> None (no channel).
    if ctx.current_channel_id.is_none() {
        if let Some(ch) = crate::channels_yaml::resolve_default_channel(None, "default_cli_channel")
        {
            ctx.current_channel_id = Some(ch);
        }
    }

    match state
        .plugin_manager
        .snapshot_registry()
        .await
        .execute(&call, ctx)
        .await
    {
        Ok(result) => Json(serde_json::json!({
            "success": true,
            "content": result.content,
            "is_error": result.is_error,
        })),
        Err(e) => Json(serde_json::json!({
            "success": false,
            "error": e.to_string(),
        })),
    }
}

/// GET /api/context/{channel_name}: preview section [3] Context, read-only.
///
/// Assembles the same ContextBuilder blocks that would be injected into the
/// prompt for the latest thread in this channel. No messages are written.
/// Returns the full context text as a string.
async fn context_preview_handler(
    Path(channel_name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let channel = match queries::get_channel_by_name(&state.pool, &channel_name).await {
        Ok(Some(ch)) => ch,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    serde_json::json!({ "error": format!("Channel '{}' not found", channel_name) }),
                ),
            );
        }
        Err(e) => {
            error!("Failed to look up channel '{}': {:?}", channel_name, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Database error: {}", e) })),
            );
        }
    };

    let profile_name = if channel.current_profile.is_empty() {
        &state.default_profile
    } else {
        &channel.current_profile
    };
    let platform = channel.platform.as_deref().unwrap_or("");

    // Get the latest seq-0 message in this channel to use as the cause
    // (so retrieval/search context is based on real content).
    let (cause_id, cause_content) = match queries::get_latest_seq0_message(&state.pool, &channel.id)
        .await
    {
        Ok(Some(msg)) => (msg.id, msg.content),
        Ok(None) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({ "context": "", "info": "No messages in this channel" })),
            );
        }
        Err(e) => {
            error!(
                "Failed to get latest message for channel {}: {:?}",
                channel.id, e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Database error: {}", e) })),
            );
        }
    };

    // Get the thread this message belongs to
    let thread_id = match queries::get_message_thread(&state.pool, cause_id).await {
        Ok(Some(tid)) => tid,
        Ok(None) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({ "context": "", "info": "Message has no thread" })),
            );
        }
        Err(e) => {
            error!("Failed to get thread for message {}: {:?}", cause_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Database error: {}", e) })),
            );
        }
    };

    // Resolve profile
    let profile_registry = crate::profile::ProfileRegistry::new(&state.data_dir);
    let _prof = profile_registry
        .get(profile_name)
        .cloned()
        .unwrap_or_else(|| crate::profile::Profile::default(profile_name));

    // Use the prompt tool for context (same tool the agent uses)
    let context_text = call_prompt_context(
        &state.plugin_manager,
        &state.app_context,
        profile_name,
        platform,
        &cause_content,
        thread_id,
        &channel.id,
    )
    .await;

    (
        StatusCode::OK,
        Json(serde_json::json!({ "context": context_text })),
    )
}

/// Call the prompt_generate MCP tool to build context: same tool the agent executor uses.
/// Falls back to empty string if the tool is not registered or fails.
async fn call_prompt_context(
    plugin_manager: &Arc<dyn PluginManager>,
    app_context: &AppContext,
    profile_name: &str,
    platform: &str,
    user_message: &str,
    thread_id: i64,
    channel_id: &str,
) -> String {
    let prompt_tool_name = crate::agent::config::get_global()
        .map(|g| g.read().prompt_tool_name.clone())
        .unwrap_or_else(|| "prompt_generate".to_string());

    // Collect all available tool names (same as the executor does)
    let tool_names: Vec<String> = plugin_manager
        .snapshot_registry()
        .await
        .all()
        .iter()
        .map(|t| t.full_name.clone())
        .collect();
    let mcp_call = McpToolCall {
        id: "preview-context".to_string(),
        name: prompt_tool_name,
        arguments: serde_json::json!({
            "profile_name": profile_name,
            "platform": platform,
            "user_message": user_message,
            "tool_names": tool_names,
            "thread_id": thread_id,
            "channel_id": channel_id,
        }),
    };

    let result = plugin_manager
        .snapshot_registry()
        .await
        .execute(&mcp_call, app_context.clone())
        .await;

    match result {
        Ok(r) if !r.is_error => serde_json::from_str::<serde_json::Value>(&r.content)
            .ok()
            .and_then(|v| v["context"].as_str().map(String::from))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// POST /run-cron/{schedule_id}: manually fire a cron job.
///
/// Accepts an optional `?force=true` query parameter. When force is true,
/// the job is executed even if it's marked inactive.
/// Returns the created thread ID on success.
async fn run_cron_handler(
    Path(schedule_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Query(_params): Query<RunCronParams>,
) -> impl IntoResponse {
    match crate::scheduler::fire_cron_job_by_id(
        &state.pool,
        &state.data_dir,
        &state.plugin_manager,
        &state.app_context,
        &schedule_id,
        false,
    )
    .await
    {
        Ok(thread_id) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ok",
                "schedule_id": schedule_id,
                "thread_id": thread_id,
            })),
        ),
        Err(e) => {
            let msg = e.to_string();
            error!("[run-cron] Failed for schedule '{}': {}", schedule_id, msg);

            // Map domain errors to appropriate HTTP status codes
            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else if msg.contains("not active") {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };

            (
                status,
                Json(serde_json::json!({
                    "status": "error",
                    "error": msg,
                    "schedule_id": schedule_id,
                })),
            )
        }
    }
}

#[derive(Deserialize)]
struct RunCronParams {
    force: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── PromptPreviewRequest serde ─────────────────────────────────────

    #[test]
    fn test_prompt_preview_request() {
        let json = serde_json::json!({
            "prompt": "Hello world",
            "plan": true
        });
        let req: PromptPreviewRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.prompt, "Hello world");
        assert!(req.plan);
    }

    #[test]
    fn test_prompt_preview_request_no_plan() {
        let json = serde_json::json!({
            "prompt": "Hello world",
            "plan": false
        });
        let req: PromptPreviewRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.prompt, "Hello world");
        assert!(!req.plan);
    }

    // ─── AppState ────────────────────────────────────────────────────────

    #[test]
    fn test_app_state_impl_clone() {
        // Compile-time check: AppState derives Clone
        fn assert_clone<T: Clone>() {}
        assert_clone::<AppState>();
    }

    // ─── ServerConfig ────────────────────────────────────────────────────

    #[test]
    fn test_server_config_impl_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<ServerConfig>();
    }

    // ─── Health handler ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_health_handler_returns_ok() {
        let response = health_handler().await;
        assert_eq!(response, "ok");
    }

    // ─── PromptPreviewResponse ──────────────────────────────────────────

    #[test]
    fn test_prompt_preview_response_serialize() {
        let resp = PromptPreviewResponse {
            system_prompt: "test system".to_string(),
            messages: vec![serde_json::json!({ "role": "system", "content": "test" })],
            plan: Some(true),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["system_prompt"], "test system");
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["plan"], true);
    }

    #[test]
    fn test_prompt_preview_response_no_plan() {
        let resp = PromptPreviewResponse {
            system_prompt: "test".to_string(),
            messages: vec![],
            plan: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["plan"].is_null());
    }

    // ─── Stop-thread surgical cancellation decisions ──────────────────────

    #[test]
    fn stop_thread_cancels_handler_only_when_target_was_processing() {
        // A pending target: the handler is processing a DIFFERENT thread (or
        // idle) — it must NOT be cancelled, or the unrelated thread dies.
        assert!(!stop_thread_cancels_handler(None));
        assert!(!stop_thread_cancels_handler(Some("pending")));
        assert!(!stop_thread_cancels_handler(Some("completed")));
        assert!(!stop_thread_cancels_handler(Some("skipped")));
        // Only the actively-processing target justifies handler cancellation.
        assert!(stop_thread_cancels_handler(Some("processing")));
    }

    #[test]
    fn stop_recovery_clears_thread_status_in_block_and_noop() {
        // Block with the clear flag (current decision table) drops the marker.
        assert_eq!(
            stop_recovery_thread_status(
                &queries::StopRecovery::Block {
                    new_status: "blocked",
                    clear_thread_status: true,
                },
                Some("running"),
            ),
            None
        );
        // Block without the flag keeps the current marker.
        assert_eq!(
            stop_recovery_thread_status(
                &queries::StopRecovery::Block {
                    new_status: "blocked",
                    clear_thread_status: false,
                },
                Some("running"),
            ),
            Some("running".to_string())
        );
        // Noop drops the marker when one is set — the task status itself is
        // untouched (apply_stop_recovery does not transition the task there).
        assert_eq!(
            stop_recovery_thread_status(&queries::StopRecovery::Noop, Some("scheduled")),
            None
        );
        assert_eq!(
            stop_recovery_thread_status(&queries::StopRecovery::Noop, None),
            None
        );
    }
}
