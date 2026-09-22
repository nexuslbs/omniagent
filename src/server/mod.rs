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
pub(crate) mod db_query;
pub(crate) mod events;
pub(crate) mod hooks;
pub(crate) mod kanban;
pub(crate) mod kanban_ids;
pub(crate) mod llm_proxy;
pub(crate) mod memory;
pub(crate) mod messages;
pub(crate) mod models;
pub(crate) mod overview;
pub(crate) mod platforms;
pub(crate) mod profiles;
pub(crate) mod prompt_api;
pub(crate) mod schedule;
mod secrets;
pub(crate) mod settings;
pub(crate) mod threads;
pub(crate) mod tool_errors;
pub(crate) mod toolsets;
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
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::agent::config::AgentConfig;
use crate::agent::kanban_updater::transition_with_comment;
use crate::agent::plugin_manager::PluginManager;
use crate::db::types as queries;
use crate::mcp::AppContext;
use parking_lot::RwLock;

mod diagnostic;
mod git_sync;

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
pub(crate) mod retention;

/// Type alias for the platform restart signals map.
/// Each entry: (restart_count, stopped_flag, notify)
pub(crate) type PlatformRestartSignals =
    Arc<Mutex<HashMap<String, (Arc<AtomicU64>, Arc<AtomicBool>, Arc<Notify>)>>>;

/// Deserialize a TRI-STATE field: an absent key -> `None` (leave the stored
/// value unchanged), an explicit JSON `null` -> `Some(None)` (clear it), any
/// other value -> `Some(Some(value))` (set it).
///
/// With a plain `Option<T>` an explicit `null` is indistinguishable from an
/// absent key, so "clear this field back to Default" was silently dropped
/// (profiles PATCH regression, 2026-09-12: the previous provider stayed in
/// effect). Every PATCH body that must tell "unchanged" apart from "clear"
/// uses this deserializer plus [`apply_tri_state_string`].
pub(crate) fn deserialize_double_option<'de, D, T>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

/// Apply a tri-state string PATCH field to a stored `Option<String>`:
/// `None` (key absent) leaves it unchanged, `Some(None)` (explicit `null`)
/// and `Some(Some(blank))` (empty/whitespace) clear it, `Some(Some(value))`
/// stores the trimmed value.
pub(crate) fn apply_tri_state_string(
    current: &mut Option<String>,
    incoming: &Option<Option<String>>,
) {
    if let Some(value) = incoming {
        let trimmed = value.as_deref().map(str::trim).unwrap_or("");
        *current = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }
}

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
    /// Plugin manager - single authority for all plugin lifecycle operations
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

    // Eagerly start enabled plugins at boot. Without this, provider subprocesses
    // (and enabled MCP tool servers) only spawn when /api/reload or a plugin
    // enable/disable/restart call runs reload_plugins - on a cold stack (fresh
    // deploy, container restart) an enabled provider has NO running subprocess
    // until some unrelated API call happens to trigger a reload, so the first
    // LLM completion falls through to HTTP and fails ("builder error" for
    // subprocess-only providers like noop). Run it in a spawned task so startup
    // is not blocked; reload_plugins is idempotent (skips already-running).
    {
        let boot_state = app_state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            match crate::server::plugins_env::reload_plugins(boot_state).await {
                Ok((started, stopped, errors)) => {
                    tracing::info!(
                        "Startup reload complete: started={} stopped={} errors={:?}",
                        started,
                        stopped,
                        errors
                    );
                }
                Err(e) => tracing::error!("Startup reload failed: {:?}", e),
            }
        });
    }

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
        .route("/prompt/{channel_name}", get(prompt_api::prompt_handler))
        .route(
            "/prompt-preview/{channel_name}",
            post(prompt_api::prompt_preview_handler),
        )
        .route("/mcp/tools", get(list_mcp_tools_handler))
        .route("/mcp/tools/invalid", get(list_invalid_mcp_tools_handler))
        .route("/mcp/execute", post(execute_mcp_tool_handler))
        // Core read-only DB API: executed by core directly, so the dashboard
        // Database page works with NO plugin installed or enabled.
        .route("/db/query", post(db_query::db_query_handler))
        .route("/db/tables", get(db_query::db_tables_handler))
        // ── Context preview (section [3] only, no messages written) ──
        .route(
            "/api/context/{channel_name}",
            get(prompt_api::context_preview_handler),
        )
        // ── Plugin management routes ──
        .route("/api/plugins/ping", get(|| async { "pong" }))
        // ── Models (config/models.yml) API ──
        .route(
            "/api/models",
            get(models::get_models_handler).put(models::put_models_handler),
        )
        // ── Toolsets (config/toolsets.yml) API ──
        .route(
            "/api/toolsets",
            get(toolsets::get_toolsets_handler).put(toolsets::put_toolsets_handler),
        )
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
        // ── Data retention: imperative soft/hard delete triggers + status ──
        .route("/api/retention/status", get(retention::status_handler))
        .route(
            "/api/retention/soft-delete",
            post(retention::soft_delete_handler),
        )
        .route(
            "/api/retention/hard-delete",
            post(retention::hard_delete_handler),
        )
        // ── Git sync: canonical sync entrypoint (explorer + backup/restore) ──
        .route("/git/sync", post(git_sync::sync_handler))
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
        // ── Profiles API routes (config/profiles.yml) ──
        .merge(profiles::profiles_router())
        // ── Kanban API routes ──
        .merge(kanban::kanban_router())
        // ── Schedule API routes (replaces dashboard schedule.ts) ──
        .merge(schedule::schedule_router())
        .merge(hooks::hooks_router())
        // ── Published-event bus routes (publish / wait / describe) ──
        .merge(events::events_router())
        // ── Actions CRUD routes (backed by actions.yml) ──
        .route("/actions", get(actions::list_actions_handler))
        .route("/actions", post(actions::create_action_handler))
        .route("/actions/{id}", put(actions::update_action_handler))
        .route("/actions/{id}", delete(actions::delete_action_handler))
        .route("/actions/{id}/run", post(actions::run_action_handler))
        // ── Cron run endpoint ──
        .route("/run-cron/{schedule_id}", post(run_cron_handler))
        .with_state(app_state)
        .layer(axum::middleware::from_fn(timing_middleware));

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

/// Process start instant, used for /health uptime reporting.
static SERVER_START: once_cell::sync::Lazy<std::time::Instant> =
    once_cell::sync::Lazy::new(std::time::Instant::now);

/// Health payload: status + the release version (Cargo.toml, baked at build
/// time via CARGO_PKG_VERSION) + process uptime in seconds.
fn health_payload() -> serde_json::Value {
    serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime": SERVER_START.elapsed().as_secs(),
    })
}

/// Simple health check: returns JSON with status, version and uptime.
async fn health_handler() -> impl IntoResponse {
    Json(health_payload())
}

/// Spawn a minimal readiness HTTP server that serves GET /health on the real
/// API host:port while startup migrations run (incident 2026-09-05: the
/// v0.1.9 schema upgrade rewrote the messages table for 8+ minutes with the
/// HTTP API not yet bound -> production 500/502). The full API router binds
/// the same address afterwards, so the caller MUST abort the returned task
/// (and await it so the listener actually closes) right before starting the
/// real server. Returns None when the address cannot be bound (the real
/// server then reports a bind error of its own later).
pub async fn spawn_readiness_server(host: &str, port: u16) -> Option<tokio::task::JoinHandle<()>> {
    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await.ok()?;
    tracing::info!(
        "Readiness /health server listening on {} while startup migrations run",
        addr
    );
    let app = Router::new().route("/health", get(health_handler));
    Some(tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("Readiness /health server error: {}", e);
        }
    }))
}

/// Pure decision: must `stop-thread` cancel the channel handler?
///
/// Only when the target thread was actively `processing`: the handler
/// processes one thread at a time per channel, so a `pending` target (or any
/// other state) means the handler is running a DIFFERENT thread or is idle -
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

// ---------------------------------------------------------------------------
// Shared explicit-stop core (HTTP endpoints + inbound stop prompt command)
// ---------------------------------------------------------------------------

/// Process-wide channel cancellation-token registry.
///
/// The agent supervisor registers one token per channel here; the HTTP
/// `/stop/{channel_id}` endpoint AND the inbound `stop` prompt command
/// (`$stop` on Mattermost, `/stop` on Telegram) cancel through this SAME map,
/// so both surfaces stop the very same processing task. Lazily initialised so
/// the binary, the HTTP server and the platform clients share one instance.
static CANCEL_TOKENS: OnceLock<Arc<Mutex<HashMap<String, CancellationToken>>>> = OnceLock::new();

/// The process-wide channel cancellation-token registry.
pub fn cancel_registry() -> &'static Arc<Mutex<HashMap<String, CancellationToken>>> {
    CANCEL_TOKENS.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

/// What an explicit stop targets.
#[derive(Debug, Clone)]
pub enum StopScope {
    /// Every pending/processing thread of the channel (HTTP `/stop/{channel_id}`
    /// and a top-level `$stop` / `/stop` prompt).
    Channel(String),
    /// The prompt-command family form: the parent thread itself
    /// (`id == parent_id`) plus the threads attached to it
    /// (`parent_id == parent_id`), all inside `channel_id`.
    Family { channel_id: String, parent_id: i64 },
}

/// Outcome of an explicit stop, shared by the HTTP endpoint and the prompt
/// command so both report identical numbers.
#[derive(Debug, Default, Clone)]
pub struct StopOutcome {
    /// Threads flipped to terminal `skipped` by this call.
    pub skipped: usize,
    /// Kanban tasks moved to `blocked` by the explicit-stop recovery.
    pub blocked_tasks: u32,
    /// True when the channel's processing task was cancelled.
    pub handler_cancelled: bool,
    /// True when the scope was limited to a thread family (prompt command).
    pub scoped: bool,
    /// Number of pending/processing threads the scope selected before skipping.
    pub target_threads: usize,
    /// Channel the stop applied to, when known.
    pub channel_id: Option<String>,
}

/// Explicitly stop the threads selected by `scope`.
///
/// This is the single implementation behind BOTH `POST|GET /stop/{channel_id}`
/// and the inbound `stop` prompt command: list the pending/processing targets,
/// mark each one terminal `skipped` (through the `mark_thread_terminal` choke
/// point, so pending subtasks are cancelled and the terminal invariant holds),
/// fire the terminal hooks, apply the kanban explicit-stop recovery and cancel
/// the channel's processing task through the shared cancellation registry.
pub async fn stop_threads(pool: &PgPool, scope: StopScope) -> Result<StopOutcome, String> {
    let (rows, scoped, channel_id): (Vec<ThreadTaskRow>, bool, String) = match &scope {
        StopScope::Channel(channel_id) => (
            sql_forge!(
                ThreadTaskRow,
                "SELECT id, channel_id, task_id, status FROM threads WHERE channel_id = :channel_id AND status IN ('pending', 'processing')",
                ( :channel_id = channel_id.as_str() )
            )
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?,
            false,
            channel_id.clone(),
        ),
        StopScope::Family {
            channel_id,
            parent_id,
        } => (
            sql_forge!(
                ThreadTaskRow,
                "SELECT id, channel_id, task_id, status FROM threads WHERE channel_id = :channel_id AND (parent_id = :parent_id OR id = :parent_id) AND status IN ('pending', 'processing')",
                ( :channel_id = channel_id.as_str(), :parent_id = *parent_id )
            )
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?,
            true,
            channel_id.clone(),
        ),
    };

    let mut outcome = StopOutcome {
        scoped,
        target_threads: rows.len(),
        channel_id: Some(channel_id.clone()),
        ..StopOutcome::default()
    };

    // 1. Mark every target terminal 'skipped' (plain skip - no reschedule, no
    //    re-run thread). Every terminal write funnels through
    //    mark_thread_terminal so the terminal=true invariant holds.
    let mut skipped_ids: Vec<i64> = Vec::new();
    for row in &rows {
        match queries::mark_thread_terminal(pool, row.id, "skipped").await {
            Ok(n) => {
                outcome.skipped += n as usize;
                if n > 0 {
                    skipped_ids.push(row.id);
                }
            }
            Err(e) => {
                error!(
                    "Stop: failed to skip thread {} for channel {}: {:?}",
                    row.id, channel_id, e
                );
                return Err(e.to_string());
            }
        }
    }

    // 2. Event-driven hooks: every thread this stop flipped to terminal
    //    'skipped' emits the terminal lifecycle events (thread_skipped +
    //    thread_terminated), fire-and-forget.
    for id in skipped_ids {
        crate::hooks::fire_thread_terminated(id, "skipped");
    }

    // 3. Phase 6b: block the kanban tasks of the skipped threads.
    for row in &rows {
        match apply_stop_recovery(pool, row.id, row.task_id.as_deref(), "stop").await {
            Ok(true) => outcome.blocked_tasks += 1,
            Ok(false) => {}
            Err(e) => error!(
                "Stop: failed to apply recovery for thread {}: {}",
                row.id, e
            ),
        }
    }

    // 4. Cancel the channel's processing task (if running).
    //    - channel scope: always (the whole channel is being stopped);
    //    - family scope: only when a target was actively `processing` - a
    //      pending-only family means the handler is running a DIFFERENT thread
    //      and must not be killed.
    let cancel_handler = match &scope {
        StopScope::Channel(_) => true,
        StopScope::Family { .. } => rows
            .iter()
            .any(|row| stop_thread_cancels_handler(row.status.as_deref())),
    };
    if cancel_handler {
        let mut tokens = cancel_registry().lock().await;
        if let Some(token) = tokens.remove(&channel_id) {
            token.cancel();
            outcome.handler_cancelled = true;
        }
    }

    info!(
        "Stop: skipped {} pending/processing threads for channel {} (scope={}{})",
        outcome.skipped,
        channel_id,
        if scoped { "family" } else { "channel" },
        if outcome.handler_cancelled {
            ", handler cancelled"
        } else {
            ""
        }
    );

    Ok(outcome)
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
    // Delegated to the shared explicit-stop core so the HTTP endpoint and the
    // inbound `stop` prompt command ($stop / /stop) can never diverge.
    match stop_threads(&state.pool, StopScope::Channel(channel_id.clone())).await {
        Ok(outcome) => Json(serde_json::json!({
            "action": "stop",
            "channel_id": channel_id,
            "skipped_threads": outcome.skipped,
            "blocked_tasks": outcome.blocked_tasks,
            "handler_cancelled": outcome.handler_cancelled,
        })),
        Err(e) => {
            error!("Stop: failed for channel {}: {}", channel_id, e);
            Json(serde_json::json!({
                "status": "error",
                "error": e,
                "channel_id": channel_id,
            }))
        }
    }
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
    //    target state means the handler is running a DIFFERENT thread - or is
    //    idle - and must NOT be cancelled (stopping one thread must never kill
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
    let mut skipped_ids: Vec<i64> = Vec::new();
    for row in &threads {
        match queries::mark_thread_terminal(&state.pool, row.id, "skipped").await {
            Ok(n) => {
                skipped += n;
                if n > 0 {
                    skipped_ids.push(row.id);
                }
            }
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

    // Event-driven hooks: every thread this close flipped to terminal
    // 'skipped' emits the terminal lifecycle events (thread_skipped +
    // thread_terminated), fire-and-forget.
    for id in skipped_ids {
        crate::hooks::fire_thread_terminated(id, "skipped");
    }

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
                "profile": status.current_profile,
                "model": status.current_model,
                "provider": status.current_provider,
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
                "name": t.name,
                // Back-compat alias: older dashboard code reads "full_name".
                "full_name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
                "server_name": t.server_name,
            })
        })
        .collect();
    Json(serde_json::json!(tools))
}

/// GET /mcp/tools/invalid: tools REJECTED by the exposed-name grammar.
///
/// A tool whose exposed name `{plugin}__{tool}` is empty, contains the reserved
/// separator inside a component, starts or ends with `_`, leaves the
/// `[A-Za-z0-9_-]` charset or exceeds 64 chars is NEVER registered, never sent
/// to a provider and never listed in the agent's available tools. This endpoint
/// is how the dashboard surfaces plugin, tool and the failed rule to the
/// operator.
async fn list_invalid_mcp_tools_handler(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let registry = state.plugin_manager.snapshot_registry().await;
    Json(serde_json::json!({
        "invalid": registry.invalid_tools(),
        "collisions": registry.collisions(),
    }))
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
/// Declared tools of CONFIGURED plugins, plus the set of known plugin names.
///
/// Feeds the structured "unavailable tool" classification so an unresolved
/// name is reported as disabled plugin / not installed / unknown instead of a
/// bare `Unknown tool: X`. Declaration comes from the plugin manifest
/// (`plugin.json` `tools[]`), falling back to the discovered tool names.
fn declared_tools(data_dir: &str) -> (Vec<tool_errors::DeclaredTool>, Vec<String>) {
    let mut declared: Vec<tool_errors::DeclaredTool> = Vec::new();
    let mut known: Vec<String> = Vec::new();
    let plugins = match crate::plugins_yaml::list_plugins(data_dir) {
        Ok(plugins) => plugins,
        Err(e) => {
            tracing::warn!("tool-error classifier: cannot list plugins: {e:?}");
            return (declared, known);
        }
    };
    for plugin in plugins {
        known.push(plugin.name.clone());
        if plugin.plugin_type != "tools" && plugin.plugin_type != "tool" {
            continue;
        }
        let mut names = plugin.tool_names.clone();
        if names.is_empty() {
            if let Some(arr) = plugin.manifest.get("tools").and_then(|v| v.as_array()) {
                for tool in arr {
                    if let Some(name) = tool.get("name").and_then(|n| n.as_str()) {
                        names.push(name.to_string());
                    }
                }
            }
        }
        for tool in names {
            declared.push((plugin.name.clone(), tool, plugin.status.clone()));
        }
    }
    (declared, known)
}

async fn execute_mcp_tool_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<McpExecuteRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        let failure = tool_errors::name_required();
        return (StatusCode::BAD_REQUEST, Json(failure.to_json()));
    }

    // Resolve the tool BEFORE executing. An unknown / unregistered / disabled
    // tool answers with a structured, plugin-aware error (status + code + tool
    // + reason + remediation) instead of a raw 502 `Unknown tool: X`.
    let registry = state.plugin_manager.snapshot_registry().await;
    if registry.get(&name).is_none() {
        let registered: Vec<String> = registry.all().iter().map(|t| t.name.clone()).collect();
        let (declared, known_plugins) = declared_tools(&state.data_dir);
        let failure = tool_errors::classify(&name, &registered, &declared, &known_plugins);
        let status = StatusCode::from_u16(failure.status).unwrap_or(StatusCode::NOT_FOUND);
        tracing::warn!(
            "mcp/execute: tool '{}' unavailable ({}): {}",
            name,
            failure.code,
            failure.reason
        );
        return (status, Json(failure.to_json()));
    }

    let args = body.arguments.unwrap_or(serde_json::json!({}));
    let call = crate::mcp::McpToolCall {
        id: "api-exec".to_string(),
        name,
        arguments: args,
    };

    // Build the tool-call context the same way the agent loop does: start from
    // the shared app context, apply the caller-provided meta fields (if any),
    // then fill in DEFAULTS for anything still missing - the default profile,
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

    match registry.execute(&call, ctx).await {
        Ok(result) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "content": result.content,
                "is_error": result.is_error,
            })),
        ),
        Err(e) => {
            // Execution failed (status 200 + success:false, unchanged for
            // existing callers) but the payload is now structured too.
            let failure = tool_errors::execution_failed(&call.name, &e.to_string());
            (StatusCode::OK, Json(failure.to_json()))
        }
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
    Query(params): Query<RunCronParams>,
) -> impl IntoResponse {
    match crate::scheduler::fire_cron_job_by_id(
        &state.pool,
        &state.data_dir,
        &state.plugin_manager,
        &state.app_context,
        &schedule_id,
        params.force.unwrap_or(false),
    )
    .await
    {
        Ok(outcome) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ok",
                "schedule_id": schedule_id,
                "thread_id": outcome.thread_id,
                "run_id": outcome.run_id,
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

// ---------------------------------------------------------------------------
// Internal latency budget + timing middleware
// ---------------------------------------------------------------------------

/// Internal server-side latency budget: every API call must be handled in
/// under this many milliseconds (DB + business logic + serialization,
/// excluding client network time). The middleware below measures it.
const LATENCY_BUDGET_MS: u128 = 500;

/// Measures the time spent INSIDE the omniagent process serving a request and
/// exposes it as the `x-response-time-ms` response header (server-side latency
/// without network effects - the measurement surface for the per-endpoint
/// latency inventory). Requests over [`LATENCY_BUDGET_MS`] are logged as
/// warnings so slow endpoints stay visible.
async fn timing_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = std::time::Instant::now();
    let mut response = next.run(request).await;
    let elapsed_ms = started.elapsed().as_millis();
    if let Ok(value) = axum::http::HeaderValue::from_str(&elapsed_ms.to_string()) {
        response.headers_mut().insert("x-response-time-ms", value);
    }
    if elapsed_ms > LATENCY_BUDGET_MS {
        tracing::warn!(
            "[latency] {} {} took {} ms (internal budget {} ms)",
            method,
            path,
            elapsed_ms,
            LATENCY_BUDGET_MS
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let payload = health_payload();
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["version"], env!("CARGO_PKG_VERSION"));
        assert!(payload["uptime"].as_u64().is_some());
    }

    // ─── Stop-thread surgical cancellation decisions ──────────────────────

    #[test]
    fn stop_thread_cancels_handler_only_when_target_was_processing() {
        // A pending target: the handler is processing a DIFFERENT thread (or
        // idle) - it must NOT be cancelled, or the unrelated thread dies.
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
        // Noop drops the marker when one is set - the task status itself is
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
