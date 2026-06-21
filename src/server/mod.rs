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

mod settings;

use axum::{
    extract::{Path, State},
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
use tracing::{error, info};

use crate::db::types as queries;
use crate::llm::{ChatMessage, CompletionRequest, LLMClient, LLMConfig};
use crate::mcp::{AppContext, McpRegistry, McpToolCall};
use crate::prompt_builder::{build_planning_prompt, build_system_prompt, MemoryStore};

pub mod plugins;
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

/// Start the HTTP server on the given host and port.
pub async fn start_server(
    pool: PgPool,
    host: String,
    port: u16,
    cancel_tokens: Arc<Mutex<HashMap<i64, CancellationToken>>>,
    data_dir: String,
    mcp_registry: McpRegistry,
    app_context: AppContext,
) {
    let app_state = Arc::new(AppState {
        pool,
        cancel_tokens,
        data_dir: data_dir.clone(),
        env_path: format!("{}/.env", data_dir),
        mcp_registry,
        app_context,
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
        .route("/prompt-preview/:channel_name", post(prompt_preview_handler))
        .route("/actions", get(list_actions_handler))
        .route("/actions", post(create_action_handler))
        .route("/actions/:id", put(update_action_handler))
        .route("/actions/:id", delete(delete_action_handler))
        .route("/actions/:id/run", post(run_action_handler))
        .route("/mcp/tools", get(list_mcp_tools_handler))
        .nest("/settings", settings::settings_router())
        .merge(plugins::plugin_router())
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

// ── Prompt preview endpoint ──

#[derive(Deserialize)]
struct PromptPreviewRequest {
    prompt: String,
    plan: bool,
}

#[derive(Serialize)]
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

    let profile_path = format!("{}/profiles/{}", state.data_dir, profile_name);
    let mut memory_store = MemoryStore::new(&profile_path);
    memory_store.load_from_disk();

    let platform = channel.platform.as_deref().unwrap_or("");
    let system_prompt = build_system_prompt(&memory_store, platform, None, profile_name);

    let mut messages = vec![
        serde_json::json!({ "role": "system", "content": system_prompt }),
    ];

    // Add recent seq-0 messages from the same channel (last 5)
    match queries::get_recent_channel_seq0_messages(&state.pool, channel.id, 5).await {
        Ok(msgs) if !msgs.is_empty() => {
            let recent_text: String = msgs.iter().rev().map(|msg| {
                format!("[msg {}] {}", msg.id, msg.content.chars().take(200).collect::<String>())
            }).collect::<Vec<_>>().join("\n");
            messages.push(serde_json::json!({ "role": "system", "content": format!("Recent conversations in this channel:\n{}", recent_text) }));
        }
        _ => {}
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
    messages.push(serde_json::json!({ "role": "user", "content": body.prompt }));

    let plan = if body.plan {
        // Resolve provider/model: channel > profile > env
        let profile_registry = crate::profile::ProfileRegistry::new(&state.data_dir);
        let prof = profile_registry.get(profile_name).cloned()
            .unwrap_or_else(|| crate::profile::Profile::default(profile_name));

        let provider_name = match channel.current_provider.clone()
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

        let model_name = match channel.current_model.clone()
            .filter(|s| !s.is_empty())
            .or_else(|| prof.model.clone().filter(|s| !s.is_empty()))
            .or_else(|| std::env::var("LLM_MODEL").ok().filter(|s| !s.is_empty()))
        {
            Some(m) => m,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "No LLM model configured — set LLM_MODEL env var or configure channel/model profile"
                    })),
                );
            }
        };

        // Resolve provider enum for the resolved provider name
        let resolved_provider: crate::llm::ProviderKind = match provider_name.parse() {
            Ok(p) => p,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!("Unknown LLM provider: '{}'", provider_name)
                    })),
                );
            }
        };

        // Build planning prompt
        let planning_prompt = build_planning_prompt(
            &memory_store,
            platform,
            profile_name,
            &body.prompt,
            0,
            0,
            None,
        );

        // Create LLM client — match how the agent resolves config.
        // The agent (Agent::new) uses config.llm_api_key (=LLM_API_KEY)
        // as primary, NOT DEEPSEEK_API_KEY. Only fall back to
        // DEEPSEEK_API_KEY if LLM_API_KEY is absent (matches the
        // AgentConfig::from_env() fallback logic).
        let base_url = std::env::var("LLM_BASE_URL").unwrap_or_else(|_| match resolved_provider {
            crate::llm::ProviderKind::OpenCodeGo => "https://opencode.ai/zen/go/v1".to_string(),
            crate::llm::ProviderKind::OpenAI => "https://api.openai.com/v1".to_string(),
            crate::llm::ProviderKind::Anthropic => "https://api.anthropic.com/v1".to_string(),
            crate::llm::ProviderKind::DeepSeek => "https://api.deepseek.com/v1".to_string(),
        });
        let api_key = std::env::var("LLM_API_KEY")
            .or_else(|_| {
                // agent's AgentConfig::from_env() fallback
                let provider = std::env::var("LLM_PROVIDER").unwrap_or_default();
                if provider == "deepseek" {
                    std::env::var("DEEPSEEK_API_KEY")
                } else {
                    Err(std::env::VarError::NotPresent)
                }
            })
            .unwrap_or_default();
        let api_mode = crate::llm::ApiMode::resolve(resolved_provider, &model_name);

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
}

fn default_params() -> serde_json::Value {
    serde_json::json!({})
}

// ── Action handlers ──

/// GET /actions — list all saved actions.
async fn list_actions_handler(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    match queries::list_actions(&state.pool).await {
        Ok(actions) => Json(serde_json::json!(actions)),
        Err(e) => {
            error!("Failed to list actions: {:?}", e);
            Json(serde_json::json!({ "error": e.to_string() }))
        }
    }
}

/// POST /actions — create a new action.
async fn create_action_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateActionRequest>,
) -> impl IntoResponse {
    match queries::create_action(&state.pool, &body.name, &body.tool_name, &body.params).await {
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

/// PUT /actions/:id — update an action.
async fn update_action_handler(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<UpdateActionRequest>,
) -> impl IntoResponse {
    match queries::update_action(&state.pool, &id, &body.name, &body.tool_name, &body.params).await {
        Ok(action) => Json(serde_json::json!(action)).into_response(),
        Err(e) => {
            error!("Failed to update action {}: {:?}", id, e);
            if e.to_string().contains("no rows") {
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

/// DELETE /actions/:id — delete an action.
async fn delete_action_handler(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // First check if the action exists and is builtin
    match queries::get_action(&state.pool, &id).await {
        Ok(Some(action)) => {
            if action.is_builtin {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("Cannot delete built-in action '{}'", action.name) })),
                )
                    .into_response();
            }
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Action '{}' not found", id) })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to get action {}: {:?}", id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    // Proceed with deletion
    match queries::delete_action(&state.pool, &id).await {
        Ok(count) if count > 0 => (StatusCode::NO_CONTENT, "".to_string()).into_response(),
        Ok(_) => (
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

/// POST /actions/:id/run — execute an action by calling its MCP tool.
async fn run_action_handler(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // 1. Look up the action
    let action = match queries::get_action(&state.pool, &id).await {
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

    // 2. Create an MCP tool call with the stored params
    let mcp_call = McpToolCall {
        id: format!("run-{}", action.id),
        name: action.tool_name.clone(),
        arguments: action.params.clone(),
    };

    // 3. Execute via MCP registry
    let ctx = state.app_context.clone();
    match state.mcp_registry.execute(&mcp_call, ctx) {
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
