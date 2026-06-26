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
//! - `GET /actions` — list saved actions
//! - `POST /actions` — create a new action
//! - `PUT /actions/:id` — update an action
//! - `DELETE /actions/:id` — delete an action
//! - `POST /actions/:id/run` — execute an action (call its MCP tool)
//! - `POST /run-cron/{schedule_id}` — manually trigger a cron job (proxied from dashboard)

mod settings;
mod secrets;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put, delete},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use anyhow::{Context, Result};
use tracing::{error, info};

use crate::db::types as queries;
use crate::llm::{resolve_llm_api_key, ChatMessage, CompletionRequest, LLMClient};
use crate::mcp::{AppContext, McpRegistry, McpToolCall};
use crate::prompt_builder::{build_planning_prompt, build_system_prompt, MemoryStore, PlanningPromptParams};

pub mod plugins;
mod diagnostic;
/// Shared application state for the HTTP server.
#[derive(Clone)]
pub(crate) struct AppState {
    pool: PgPool,
    cancel_tokens: Arc<Mutex<HashMap<i64, CancellationToken>>>,
    data_dir: String,
    /// Path to the .env file for settings API
    env_path: String,
    /// MCP tool registry for executing actions
    mcp_registry: McpRegistry,
    /// Application context for MCP tool execution
    app_context: AppContext,
}

/// Configuration for the HTTP server.
#[derive(Clone)]
pub struct ServerConfig {
    pub pool: PgPool,
    pub host: String,
    pub port: u16,
    pub cancel_tokens: Arc<Mutex<HashMap<i64, CancellationToken>>>,
    pub data_dir: String,
    pub mcp_registry: McpRegistry,
    pub app_context: AppContext,
}

/// Start the HTTP server on the given host and port.
pub async fn start_server(config: ServerConfig) -> Result<()> {
    let app_state = Arc::new(AppState {
        pool: config.pool,
        cancel_tokens: config.cancel_tokens,
        data_dir: config.data_dir.clone(),
        env_path: format!("{}/.env", config.data_dir),
        mcp_registry: config.mcp_registry,
        app_context: config.app_context,
    });

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/stop/{channel_id}", post(stop_handler))
        .route("/stop/{channel_id}", get(stop_handler))
        .route("/close/{channel_id}", post(close_handler))
        .route("/close/{channel_id}", get(close_handler))
        .route("/open/{channel_id}", post(open_handler))
        .route("/open/{channel_id}", get(open_handler))
        .route("/status/{channel_id}", get(status_handler))
        .route("/prompt/{channel_name}", get(prompt_handler))
        .route("/prompt-preview/{channel_name}", post(prompt_preview_handler))
        .route("/actions", get(list_actions_handler))
        .route("/actions", post(create_action_handler))
        .route("/actions/{id}", put(update_action_handler))
        .route("/actions/{id}", delete(delete_action_handler))
        .route("/actions/{id}/run", post(run_action_handler))
        .route("/mcp/tools", get(list_mcp_tools_handler))
        // ── Context preview (section [3] only, no messages written) ──
        .route("/api/context/{channel_name}", get(context_preview_handler))
        // ── Plugin management routes ──
        .route("/api/plugins/ping", get(|| async { "pong" }))
        .route("/api/plugins/check-state", get(diagnostic::check_state))
        .route("/api/plugins/check-db", get(diagnostic::check_db))
        .route("/api/plugins/check-list", get(diagnostic::check_list_plugins))
        .route("/api/plugins/check-env", get(diagnostic::check_env_read))
        .route("/api/plugins/check-enrich", get(diagnostic::check_enrich_json))
        .route("/api/plugins", get(plugins::list_plugins_handler))
        .route("/api/plugins/{name}", get(plugins::get_plugin_handler))
        .route("/api/plugins/{name}/config", post(plugins::update_config_handler))
        .route("/api/plugins/{name}/enable", post(plugins::enable_plugin_handler))
        .route("/api/plugins/{name}/disable", post(plugins::disable_plugin_handler))
        .route("/api/plugins/{name}/reinstall", post(plugins::reinstall_plugin_handler))
        .route("/api/plugins/{name}/refresh-models", post(plugins::refresh_models_handler))
        .route("/api/plugins/{name}", delete(plugins::delete_plugin_handler))
        .route("/api/plugins/install-url", post(plugins::install_url_handler))
        // ── Settings routes ──
        .route("/settings", get(settings::get_settings_handler))
        .route("/settings", put(settings::update_settings_handler))
        // ── Secrets routes ──
        .merge(secrets::secrets_router())
        // ── Cron run endpoint ──
        .route("/run-cron/{schedule_id}", post(run_cron_handler))
        .with_state(app_state);

    let addr = format!("{}:{}", config.host, config.port);
    info!("Starting HTTP server on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .context("Failed to bind HTTP server address")?;

    axum::serve(listener, app)
        .await
        .context("HTTP server exited with error")?;

    Ok(())
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
        _ => "default",
    };

    let profile_path = format!("{}/profiles/{}", state.data_dir, profile_name);
    let mut memory_store = MemoryStore::new(&profile_path);
    memory_store.load_from_disk();

    let platform = channel.as_ref().and_then(|c| c.platform.as_deref()).unwrap_or("");
    let system_prompt = build_system_prompt(&memory_store, platform, None, profile_name);

    let result = format!(
        "System Prompt:\n{}\n\n---\n\nMessages sent to LLM:\n\n{{\n  \"role\": \"system\",\n  \"content\": \"\"\"\n{}\n  \"\"\"\n}},\n{{\n  \"role\": \"cause\",\n  \"content\": \"<<<prompt>>>\"\n}}",
        system_prompt, system_prompt
    );

    (StatusCode::OK, result)
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
    plan: Option<String>,
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
        _ => "default",
    };

    let profile_path = format!("{}/profiles/{}", state.data_dir, profile_name);
    let mut memory_store = MemoryStore::new(&profile_path);
    memory_store.load_from_disk();

    let platform = channel.as_ref().and_then(|c| c.platform.as_deref()).unwrap_or("");
    let system_prompt = build_system_prompt(&memory_store, platform, None, profile_name);

    let mut messages = vec![
        serde_json::json!({ "role": "system", "content": system_prompt }),
    ];

    // Add recent seq-0 messages from the same channel (last 5), if channel exists
    if let Some(ch) = &channel {
        match queries::get_recent_channel_seq0_messages(&state.pool, ch.id, 5).await {
            Ok(msgs) if !msgs.is_empty() => {
                let recent_text: String = msgs.iter().rev().map(|msg| {
                    format!("[msg {}] {}", msg.id, msg.content.chars().take(200).collect::<String>())
                }).collect::<Vec<_>>().join("\n");
                messages.push(serde_json::json!({ "role": "system", "content": format!("Recent conversations in this channel:\n{}", recent_text) }));
            }
            _ => {}
        }
    }

    // Add skills from profile
    let skills_dir = format!("{}/profiles/{}/skills", state.data_dir, profile_name);
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        let mut skills = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
                    let first_line = content.lines().next().unwrap_or("").trim();
                    let desc = if first_line.starts_with('#') {
                        first_line.trim_start_matches('#').trim()
                    } else {
                        first_line
                    };
                    skills.push(format!("- {}: {}", name, desc));
                }
            }
        }
        if !skills.is_empty() {
            messages.push(serde_json::json!({ "role": "system", "content": format!("Available skills:\n{}", skills.join("\n")) }));
        }
    }

    // Add user prompt
    messages.push(serde_json::json!({ "role": "cause", "content": body.prompt }));

    let plan = if body.plan {
        // Resolve provider/model: channel > profile > env
        let profile_registry = crate::profile::ProfileRegistry::new(&state.data_dir);
        let prof = profile_registry.get(profile_name).cloned()
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

        // Build planning prompt
        let planning_prompt = build_planning_prompt(
            &memory_store,
            PlanningPromptParams {
                platform,
                profile_name,
                user_message: &body.prompt,
                plan_iteration: 0,
                max_iterations: 0,
                previous_plan: None,
                use_json_plan: false, // preview route doesn't need JSON plan output
            },
        );

        // Create LLM client — match how the agent resolves config
        // (AgentConfig::from_env() tries LLM_API_KEY, then {PROVIDER}_API_KEY).
        let base_url = crate::llm::resolve_default_base_url(&provider_name);
        let api_key = resolve_llm_api_key(Some(&std::env::var(
            format!("{}_API_KEY", provider_name.to_uppercase().replace('-', "_"))
        ).unwrap_or_default()));
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
                messages.push(serde_json::json!({ "role": "agent", "msg_type": "plan", "content": err_msg }));
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

// ── Action request/response types ──

#[derive(Deserialize)]
struct CreateActionRequest {
    name: String,
    tool_name: String,
    #[serde(default = "default_params")]
    params: serde_json::Value,
}

#[derive(Deserialize)]
struct UpdateActionRequest {
    name: String,
    tool_name: String,
    #[serde(default = "default_params")]
    params: serde_json::Value,
    #[serde(default)]
    enabled: Option<bool>,
}

fn default_params() -> serde_json::Value {
    serde_json::json!({})
}

// ── Action handlers ──

/// GET /actions — list all saved actions (from YAML), including disabled.
async fn list_actions_handler(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    match crate::actions::load_all_actions(&state.data_dir) {
        Ok(actions) => Json(serde_json::json!(actions)),
        Err(e) => {
            error!("Failed to list actions: {:?}", e);
            Json(serde_json::json!({ "error": e.to_string() }))
        }
    }
}

/// POST /actions — create a new action (writes to YAML).
async fn create_action_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateActionRequest>,
) -> impl IntoResponse {
    match crate::actions::add_action(&state.data_dir, &body.name, &body.tool_name, &body.params) {
        Ok(action) => (StatusCode::CREATED, Json(serde_json::json!(action))).into_response(),
        Err(e) => {
            error!("Failed to create action: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

/// PUT /actions/:id — update an action (writes to YAML).
async fn update_action_handler(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<UpdateActionRequest>,
) -> impl IntoResponse {
    match crate::actions::update_action(&state.data_dir, &id, &body.tool_name, &body.params, body.enabled) {
        Ok(action) => Json(serde_json::json!(action)).into_response(),
        Err(e) => {
            error!("Failed to update action {}: {:?}", id, e);
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": format!("Action '{}' not found", id) })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
                    .into_response()
            }
        }
    }
}

/// DELETE /actions/:id — delete an action (removes from YAML).
async fn delete_action_handler(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // Proceed with deletion
    match crate::actions::delete_action(&state.data_dir, &id) {
        Ok(true) => (StatusCode::NO_CONTENT, "".to_string()).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Action '{}' not found", id) })),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to delete action {}: {:?}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

/// POST /actions/:id/run — execute an action by calling its MCP tool (reads from YAML).
async fn run_action_handler(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // 1. Look up the action from YAML (including disabled, so we can error if disabled)
    let action = match crate::actions::get_action_unfiltered(&state.data_dir, &id) {
        Ok(Some(a)) => a,
        Ok(None) => {
            return Json(serde_json::json!({
                "error": format!("Action '{}' not found", id)
            }));
        }
        Err(e) => {
            error!("Failed to get action {}: {:?}", id, e);
            return Json(serde_json::json!({ "error": e.to_string() }));
        }
    };

    // 2. Reject if the action is disabled
    if !action.enabled {
        return Json(serde_json::json!({
            "error": format!("Action '{}' is disabled", action.name),
            "is_error": true,
        }));
    }

    // 2. Create an MCP tool call with the stored params
    let mcp_call = McpToolCall {
        id: format!("run-{}", action.id),
        name: action.tool_name.clone(),
        arguments: action.params.clone(),
    };

    // 3. Execute via MCP registry
    let ctx = state.app_context.clone();
    match state.mcp_registry.execute(&mcp_call, ctx).await {
        Ok(result) => {
            Json(serde_json::json!({
                "action_id": action.id,
                "name": action.name,
                "tool_name": action.tool_name,
                "result": result.content,
                "is_error": false,
            }))
        }
        Err(e) => {
            error!("Failed to execute action {} (tool: {}): {:?}", action.id, action.tool_name, e);
            Json(serde_json::json!({
                "action_id": action.id,
                "name": action.name,
                "tool_name": action.tool_name,
                "result": e.to_string(),
                "is_error": true,
            }))
        }
    }
}

/// GET /mcp/tools — list all registered MCP tools with their input schemas.
async fn list_mcp_tools_handler(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let tools: Vec<serde_json::Value> = state
        .mcp_registry
        .all()
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect();
    Json(serde_json::json!(tools))
}

/// POST /run-cron/{schedule_id} — manually trigger a cron job (fire immediately).
///
/// This reuses the scheduler's internal thread-creation logic so manual runs
/// go through exactly the same code path as scheduled ticks.
///
/// Query params:
///   force=true — run even if the job is not enabled
///
/// Returns the created thread_id.
async fn run_cron_handler(
    Path(schedule_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let force = params.get("force").map(|s| s == "true").unwrap_or(false);

    match crate::scheduler::fire_cron_job_by_id(
        &state.pool,
        &state.data_dir,
        &state.mcp_registry,
        &state.app_context,
        &schedule_id,
        force,
    )
    .await
    {
        Ok(thread_id) => {
            info!(
                "[run-cron] Successfully fired cron job '{}', thread {}",
                schedule_id, thread_id
            );
            Json(serde_json::json!({
                "success": true,
                "thread_id": thread_id,
            }))
        }
        Err(e) => {
            error!("[run-cron] Failed to fire cron job '{}': {:?}", schedule_id, e);
            let msg = format!("{:#}", e);
            Json(serde_json::json!({
                "success": false,
                "error": msg,
            }))
        }
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
                Json(serde_json::json!({ "error": format!("Channel '{}' not found", channel_name) })),
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
        "default"
    } else {
        &channel.current_profile
    };

    // Get the latest seq-0 message in this channel to use as the cause
    // (so retrieval/search context is based on real content).
    let (cause_id, cause_content) = match queries::get_latest_seq0_message(&state.pool, channel.id).await {
        Ok(Some(msg)) => (msg.id, msg.content),
        Ok(None) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({ "context": "", "info": "No messages in this channel" })),
            );
        }
        Err(e) => {
            error!("Failed to get latest message for channel {}: {:?}", channel.id, e);
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
    let prof = profile_registry.get(profile_name).cloned()
        .unwrap_or_else(|| crate::profile::Profile::default(profile_name));

    // Build context — same function the agent uses
    let qdrant_url = std::env::var("QDRANT_URL").ok();
    let (context_text, _meta) = crate::context_builder::build_thread_context(
        &state.pool,
        &crate::context_builder::ThreadContextIdentifiers {
            thread_id,
            channel_id: channel.id,
            cause_msg_id: cause_id,
        },
        &crate::context_builder::ThreadContextConfig {
            cause_content: &cause_content,
            profile_name,
            data_dir: &state.data_dir,
            qdrant_url: qdrant_url.as_deref(),
            prompt_budget: prof.prompt_budget.unwrap_or(crate::profile::PROMPT_BUDGET_DEFAULT),
            auto_retrieval_enabled: prof.auto_retrieval_enabled,
            retrieval_aggressiveness: prof.retrieval_aggressiveness,
        },
    ).await;

    (StatusCode::OK, Json(serde_json::json!({ "context": context_text })))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── default_params ──────────────────────────────────────────────────

    #[test]
    fn test_default_params_returns_empty_object() {
        let params = default_params();
        assert_eq!(params, serde_json::json!({}));
    }

    // ─── CreateActionRequest serde ───────────────────────────────────────

    #[test]
    fn test_create_action_request_full() {
        let json = serde_json::json!({
            "name": "test-action",
            "tool_name": "test_tool",
            "params": { "key": "value" }
        });
        let req: CreateActionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.name, "test-action");
        assert_eq!(req.tool_name, "test_tool");
        assert_eq!(req.params, serde_json::json!({ "key": "value" }));
    }

    #[test]
    fn test_create_action_request_default_params() {
        let json = serde_json::json!({
            "name": "test-action",
            "tool_name": "test_tool"
        });
        let req: CreateActionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.name, "test-action");
        assert_eq!(req.tool_name, "test_tool");
        // params should default to {}
        assert_eq!(req.params, serde_json::json!({}));
    }

    #[test]
    fn test_create_action_request_null_params() {
        let json = serde_json::json!({
            "name": "test-action",
            "tool_name": "test_tool",
            "params": null
        });
        let req: CreateActionRequest = serde_json::from_value(json).unwrap();
        // #[serde(default)] only applies when field is absent, not when null
        assert_eq!(req.params, serde_json::Value::Null);
    }

    #[test]
    fn test_create_action_request_missing_name() {
        let json = serde_json::json!({
            "tool_name": "test_tool"
        });
        let result: Result<CreateActionRequest, _> = serde_json::from_value(json);
        assert!(result.is_err(), "missing 'name' should fail deserialization");
    }

    // ─── UpdateActionRequest serde ───────────────────────────────────────

    #[test]
    fn test_update_action_request_full() {
        let json = serde_json::json!({
            "name": "updated-action",
            "tool_name": "updated_tool",
            "params": { "new_key": "new_value" }
        });
        let req: UpdateActionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.name, "updated-action");
        assert_eq!(req.tool_name, "updated_tool");
        assert_eq!(req.params, serde_json::json!({ "new_key": "new_value" }));
    }

    #[test]
    fn test_update_action_request_default_params() {
        let json = serde_json::json!({
            "name": "updated-action",
            "tool_name": "updated_tool"
        });
        let req: UpdateActionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.params, serde_json::json!({}));
    }

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
            messages: vec![
                serde_json::json!({ "role": "system", "content": "test" }),
            ],
            plan: Some("my plan".to_string()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["system_prompt"], "test system");
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["plan"], "my plan");
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
