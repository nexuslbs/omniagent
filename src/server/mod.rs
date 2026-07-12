//! HTTP server for external control (stop, close, open, status, health)
//!
//! Provides endpoints:
//! - `GET /health` — health check
//! - `POST|GET /stop/{channel_id}` — skip pending/processing threads (no channel state change)
//! - `POST|GET /close/{channel_id}` — close channel (skip threads, cancel handler)
//! - `POST|GET /open/{channel_id}` — open channel (allow handler to start)
//! - `GET /status/{channel_id}` — channel status info
//! - `GET /prompt/{channel_name}` — show system prompt for a channel
//! - `POST /prompt-preview/{channel_name}` — preview full prompt (no DB writes), optionally plan
//! - `POST /run-cron/{schedule_id}` — manually trigger a cron job (proxied from dashboard)

pub(crate) mod actions;
pub(crate) mod channels;
pub(crate) mod kanban;
pub(crate) mod llm_proxy;
pub(crate) mod memory;
pub(crate) mod messages;
pub(crate) mod overview;
pub(crate) mod platforms;
pub(crate) mod schedule;
mod secrets;
mod settings;
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
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::agent::config::AgentConfig;
use crate::agent::helpers;
use crate::db::types as queries;
use crate::llm::{ChatMessage, CompletionRequest, LLMClient};
use crate::mcp::{AppContext, McpRegistry, McpToolCall};
use sql_forge::sql_forge;
use std::sync::RwLock;

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

/// Type alias for the platform restart signals map.
type PlatformRestartSignals = Arc<Mutex<HashMap<String, (Arc<AtomicBool>, Arc<Notify>)>>>;

/// Shared application state for the HTTP server.
#[derive(Clone)]
pub(crate) struct AppState {
    pool: PgPool,
    cancel_tokens: Arc<Mutex<HashMap<i64, CancellationToken>>>,
    data_dir: String,
    /// Workspace directory for bundled plugin discovery
    workspace_dir: String,
    /// Default profile name (from DEFAULT_PROFILE env var)
    default_profile: String,
    /// Path to the .env file for settings API
    env_path: String,
    /// MCP tool registry for executing actions (shared with agent)
    tool_registry: Arc<tokio::sync::RwLock<crate::mcp::McpRegistry>>,
    /// Application context for MCP tool execution
    app_context: AppContext,
    /// Shared mutable config for hot-reload support
    shared_config: Arc<RwLock<AgentConfig>>,
    /// Per-platform restart signal flags + notify (keyed by plugin name)
    platform_restart_signals: PlatformRestartSignals,
}

/// Configuration for the HTTP server.
#[derive(Clone)]
pub struct ServerConfig {
    pub pool: PgPool,
    pub host: String,
    pub port: u16,
    pub cancel_tokens: Arc<Mutex<HashMap<i64, CancellationToken>>>,
    pub data_dir: String,
    pub workspace_dir: String,
    pub default_profile: String,
    pub tool_registry: Arc<tokio::sync::RwLock<McpRegistry>>,
    pub app_context: AppContext,
    pub shared_config: Arc<RwLock<AgentConfig>>,
    pub platform_restart_signals: PlatformRestartSignals,
}

/// Start the HTTP server on the given host and port.
pub async fn start_server(config: ServerConfig) -> AppResult<()> {
    let app_state = Arc::new(AppState {
        pool: config.pool,
        cancel_tokens: config.cancel_tokens,
        data_dir: config.data_dir.clone(),
        workspace_dir: config.workspace_dir.clone(),
        default_profile: config.default_profile.clone(),
        env_path: format!("{}/.env", config.data_dir),
        tool_registry: config.tool_registry,
        app_context: config.app_context,
        shared_config: config.shared_config,
        platform_restart_signals: config.platform_restart_signals,
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
        .route(
            "/api/plugins/install-git",
            post(plugins::install_git_handler),
        )
        .route(
            "/api/plugins/install-url",
            post(plugins::install_url_handler),
        )
        .route("/api/plugins", get(plugins::list_plugins_handler))
        .route("/api/plugins/{name}", get(plugins::get_plugin_handler))
        .route(
            "/api/plugins/{name}/config",
            post(plugins::update_config_handler),
        )
        .route(
            "/api/plugins/{name}/enable",
            post(plugins::enable_plugin_handler),
        )
        .route(
            "/api/plugins/{name}/disable",
            post(plugins::disable_plugin_handler),
        )
        .route(
            "/api/plugins/{name}/reinstall",
            post(plugins::reinstall_plugin_handler),
        )
        .route(
            "/api/plugins/{name}/install",
            post(plugins::install_plugin_handler),
        )
        .route(
            "/api/plugins/{name}/refresh-models",
            post(plugins::refresh_models_handler),
        )
        .route(
            "/api/plugins/{name}/setup",
            post(plugins::setup_plugin_handler),
        )
        .route(
            "/api/plugins/{name}/download",
            post(plugins::download_plugin_handler),
        )
        .route(
            "/api/plugins/{name}/rename",
            post(plugins::rename_plugin_handler),
        )
        .route(
            "/api/plugins/{name}",
            delete(plugins::delete_plugin_handler),
        )
        // ── Env reload (hot-reload .env without restart) ──
        .route("/api/reload", post(plugins::reload_env_handler))
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

/// Simple health check — returns "ok".
async fn health_handler() -> &'static str {
    "ok"
}

/// Stop — mark all pending/processing threads as skipped and cancel
/// the channel's executor so it restarts fresh.
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
            error!(
                "Stop: failed to skip threads for channel {}: {:?}",
                channel_id, e
            );
            0
        }
    };

    // Cancel the channel's executor so it restarts fresh
    let mut tokens = state.cancel_tokens.lock().await;
    let handler_cancelled = if let Some(token) = tokens.remove(&channel_id) {
        token.cancel();
        info!("Stop: cancelled executor for channel {}", channel_id);
        true
    } else {
        false
    };

    Json(serde_json::json!({
        "action": "stop",
        "channel_id": channel_id,
        "skipped_threads": skipped,
        "handler_cancelled": handler_cancelled,
    }))
}

/// Stop-thread — mark a single pending/processing thread as skipped and
/// cancel the channel's executor so it restarts and picks up remaining
/// pending threads.
async fn stop_thread_handler(
    Path(thread_id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // 1. Find the channel this thread belongs to
    let channel_id: Option<i64> = sql_forge!(
        scalar Option<i64>,
        "SELECT channel_id FROM threads WHERE id = :id",
        ( :id = thread_id )
    )
    .fetch_one(&state.pool)
    .await
    .ok()
    .flatten();

    let channel_id = match channel_id {
        Some(id) => id,
        None => {
            return Json(serde_json::json!({
                "action": "stop-thread",
                "thread_id": thread_id,
                "status": "error",
                "error": "Thread not found",
            }));
        }
    };

    // 2. Skip the thread
    let skipped = match queries::skip_thread(&state.pool, thread_id).await {
        Ok(count) => {
            info!(
                "Stop-thread: skipped thread {} ({} rows affected)",
                thread_id, count
            );
            count
        }
        Err(e) => {
            error!("Stop-thread: failed to skip thread {}: {:?}", thread_id, e);
            0
        }
    };

    // 3. Cancel the channel's executor so it restarts fresh (channel stays open)
    let mut tokens = state.cancel_tokens.lock().await;
    let handler_cancelled = if let Some(token) = tokens.remove(&channel_id) {
        token.cancel();
        info!(
            "Stop-thread: cancelled executor for channel {} (thread {})",
            channel_id, thread_id
        );
        true
    } else {
        false
    };

    // 4. Send :o: reaction to the platform if the thread has a cause message with an external_id
    if skipped > 0 {
        if let Ok(Some(cause_msg)) =
            crate::db::threads::get_cause_message(&state.pool, thread_id).await
        {
            if let Some(ref ext_id) = cause_msg.external_id {
                if let Ok(Some(channel)) =
                    crate::db::channels::get_channel_by_id(&state.pool, channel_id).await
                {
                    if let (Some(ref platform), Some(ref resource)) =
                        (channel.platform, channel.resource_identifier)
                    {
                        helpers::enqueue_reaction(
                            &state.app_context,
                            platform,
                            resource,
                            ext_id,
                            ":o:",
                        )
                        .await;
                        info!(
                            "Stop-thread: sent :o: reaction for thread {} on {}",
                            thread_id, platform
                        );
                    }
                }
            }
        }
    }

    Json(serde_json::json!({
        "action": "stop-thread",
        "thread_id": thread_id,
        "channel_id": channel_id,
        "skipped": skipped,
        "handler_cancelled": handler_cancelled,
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
            error!(
                "Close: failed to skip threads for channel {}: {:?}",
                channel_id, e
            );
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
            // Channel not found — build system prompt using the default profile
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

    let platform = channel
        .as_ref()
        .and_then(|c| c.platform.as_deref())
        .unwrap_or("");
    let tool_names: Vec<String> = state
        .tool_registry
        .read()
        .await
        .all()
        .iter()
        .map(|t| t.name.clone())
        .collect();
    // Build system prompt TEMPLATE — stable (identity + guidance) + volatile (memory/soul) placeholders
    let mut segments: Vec<String> = Vec::new();

    // Stable tier: simple identity + tool guidance
    let tool_list = if tool_names.is_empty() { String::new() } else { tool_names.join(", ") };
    segments.push(format!("You are OmniAgent — precise, efficient, autonomous. Your tools: {tool_list}. Use minimum roundtrips. If a tool fails, move on — don't retry more than twice."));
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
            // Channel not found — build system prompt using the default profile
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
    let user_raw = if memories_dir.join("USER.md").exists() {
        std::fs::read_to_string(memories_dir.join("USER.md")).unwrap_or_default()
    } else {
        String::new()
    };

    let platform = channel
        .as_ref()
        .and_then(|c| c.platform.as_deref())
        .unwrap_or("");
    let tool_names: Vec<String> = state
        .tool_registry
        .read()
        .await
        .all()
        .iter()
        .map(|t| t.name.clone())
        .collect();
    let system_prompt = format!(
        "You are OmniAgent — precise, efficient, autonomous.\n\nActive Hermes profile: {profile_name}.\n\n{}",
        if !memory_raw.is_empty() { format!("## MEMORY (your personal notes)\n{memory_raw}") } else { String::new() }
    );

    let mut messages = vec![serde_json::json!({ "role": "system", "content": &system_prompt })];

    // ── Build the [3] Context section using the same logic as the agent ──
    // Uses the latest thread in the channel (if any) with the preview prompt
    // as the cause content, so the context reflects what would actually be
    // assembled when a real message is processed.
    if let Some(ch) = &channel {
        if let Ok(Some(latest)) = queries::get_latest_seq0_message(&state.pool, ch.id).await {
            if let Ok(Some(tid)) = queries::get_message_thread(&state.pool, latest.id).await {
                let profile_registry = crate::profile::ProfileRegistry::new(&state.data_dir);
                let prof = profile_registry
                    .get(profile_name)
                    .cloned()
                    .unwrap_or_else(|| crate::profile::Profile::default(profile_name));
                let qdrant_url = std::env::var("QDRANT_URL").ok();

                // Look up parent_id for context scoping (preview shows thread-isolated context)
                #[derive(Debug, sqlx::FromRow)]
                struct PreviewParentRow {
                    parent_id: Option<i64>,
                }
                let pp_row: Option<PreviewParentRow> = sql_forge!(
                    PreviewParentRow,
                    "SELECT parent_id FROM threads WHERE id = :id",
                    ( :id = tid )
                )
                .fetch_optional(&state.pool)
                .await
                .ok()
                .flatten();
                let preview_parent_id = pp_row.and_then(|r| r.parent_id);

                let (context_text, _meta) = crate::context_builder::build_thread_context(
                    &state.pool,
                    &crate::context_builder::ThreadContextIdentifiers {
                        thread_id: tid,
                        channel_id: ch.id,
                        cause_msg_id: latest.id,
                        parent_id: preview_parent_id,
                    },
                    &crate::context_builder::ThreadContextConfig {
                        cause_content: &body.prompt,
                        profile_name,
                        data_dir: &state.data_dir,
                        qdrant_url: qdrant_url.as_deref(),
                        prompt_budget: prof
                            .prompt_budget
                            .unwrap_or(crate::profile::PROMPT_BUDGET_DEFAULT),
                        auto_retrieval_enabled: prof.auto_retrieval_enabled,
                        retrieval_aggressiveness: prof.retrieval_aggressiveness,
                        task_context: false,
                    },
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
            .or_else(|| std::env::var("LLM_PROVIDER").ok().filter(|s| !s.is_empty()))
        {
            Some(p) => p,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "No LLM provider configured — set LLM_PROVIDER env var or configure channel/provider profile"
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
                        "error": "No LLM model configured — channel, profile, or provider plugin default_model must define one"
                    })),
                );
            }
        };

        // Resolve provider enum for the resolved provider name
        let resolved_provider = crate::llm::ProviderId::new(&provider_name);

        // Build planning prompt inline
        let tool_list = if tool_names.is_empty() { String::new() } else { format!("Your available tools: {}.", tool_names.join(", ")) };
        let planning_prompt = format!(
            "## Plan\nBefore responding, create a high-level plan with numbered steps. \
{tool_list}\nBe specific about which tool to use and what parameters to pass. \
Aim for the minimum number of steps to complete the task. \
Wrap your plan in a <plan> block. After delivering the final answer, \
evaluate: if the task was completed, call the completion tool.",
            tool_list = tool_list
        );

        // Create LLM client — resolve api_key from provider plugin config
        // (not from hardcoded {PROVIDER}_API_KEY env var names).
        let base_url = crate::llm::resolve_default_base_url(&provider_name);

        // Look up api_key from the provider's resolved plugin config
        let api_key = match crate::plugins_yaml::get_plugin(&state.data_dir, &provider_name) {
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

/// GET /mcp/tools — list all registered MCP tools with their input schemas.
async fn list_mcp_tools_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let tools: Vec<serde_json::Value> = state
        .tool_registry
        .read()
        .await
        .all()
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "full_name": tool.full_name,
                "description": tool.description,
                "input_schema": tool.input_schema,
                "server_name": tool.server_name,
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
}

/// POST /mcp/execute — execute any registered MCP tool by name.
/// Stateless: accepts tool name + arguments, returns tool result.
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

    match state
        .tool_registry
        .read()
        .await
        .execute(&call, state.app_context.clone())
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

/// GET /api/context/{channel_name} — preview section [3] Context, read-only.
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

    // Get the latest seq-0 message in this channel to use as the cause
    // (so retrieval/search context is based on real content).
    let (cause_id, cause_content) = match queries::get_latest_seq0_message(&state.pool, channel.id)
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
    let prof = profile_registry
        .get(profile_name)
        .cloned()
        .unwrap_or_else(|| crate::profile::Profile::default(profile_name));

    // Build context — same function the agent uses
    let qdrant_url = std::env::var("QDRANT_URL").ok();
    // Look up parent_id for context scoping (preview shows thread-isolated context)
    #[derive(Debug, sqlx::FromRow)]
    struct PreviewParentRow {
        parent_id: Option<i64>,
    }
    let pp_row: Option<PreviewParentRow> = sql_forge!(
        PreviewParentRow,
        "SELECT parent_id FROM threads WHERE id = :id",
        ( :id = thread_id )
    )
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let preview_parent_id = pp_row.and_then(|r| r.parent_id);
    let (context_text, meta) = crate::context_builder::build_thread_context(
        &state.pool,
        &crate::context_builder::ThreadContextIdentifiers {
            thread_id,
            channel_id: channel.id,
            cause_msg_id: cause_id,
            parent_id: preview_parent_id,
        },
        &crate::context_builder::ThreadContextConfig {
            cause_content: &cause_content,
            profile_name,
            data_dir: &state.data_dir,
            qdrant_url: qdrant_url.as_deref(),
            prompt_budget: prof
                .prompt_budget
                .unwrap_or(crate::profile::PROMPT_BUDGET_DEFAULT),
            auto_retrieval_enabled: prof.auto_retrieval_enabled,
            retrieval_aggressiveness: prof.retrieval_aggressiveness,
            task_context: false,
        },
    )
    .await;

    (
        StatusCode::OK,
        Json(serde_json::json!({ "context": context_text, "timings": meta.step_timings_ms })),
    )
}

/// POST /run-cron/{schedule_id} — manually fire a cron job.
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
        &state.tool_registry,
        &state.app_context,
        &schedule_id,
        params.force.unwrap_or(false),
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
}
