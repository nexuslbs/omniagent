//! HTTP handlers for action CRUD backed by actions.yml.
//! Actions are stored in a YAML file at {data_dir}/actions.yml.

use crate::error::AppResult;
use crate::mcp::McpToolCall;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::error;

use super::AppState;

// ── YAML format ──

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ActionsFile {
    actions: HashMap<String, ActionEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
struct ActionEntry {
    enabled: bool,
    tool_name: String,
    params: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_builtin: Option<bool>,
}

// ── API response shape ──

#[derive(Debug, Serialize, Clone)]
pub(crate) struct ActionResponse {
    id: String,
    name: String,
    tool_name: String,
    params: serde_json::Value,
    enabled: bool,
    is_builtin: bool,
    description: Option<String>,
}

// ── Request shapes ──

#[derive(Debug, Deserialize)]
pub(crate) struct CreateActionRequest {
    name: String,
    tool_name: String,
    params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateActionRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    params: Option<serde_json::Value>,
    #[serde(default)]
    enabled: Option<bool>,
}

// ── Helpers ──

fn actions_path(data_dir: &str) -> String {
    format!("{}/actions.yml", data_dir)
}

fn load_actions(data_dir: &str) -> ActionsFile {
    let path = actions_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            serde_yaml::from_str(&content).unwrap_or_else(|e| {
                error!("Failed to parse actions.yml: {:?}", e);
                ActionsFile {
                    actions: HashMap::new(),
                }
            })
        }
        Err(_) => ActionsFile {
            actions: HashMap::new(),
        },
    }
}

fn save_actions(data_dir: &str, file: &ActionsFile) -> AppResult<()> {
    let path = actions_path(data_dir);
    let content = serde_yaml::to_string(file)
        .map_err(|e| crate::error::Error::Message(format!("Failed to serialize actions.yml: {}", e)))?;
    // Atomic write with .tmp rename
    let tmp_path = format!("{}.tmp", path);
    std::fs::write(&tmp_path, &content)
        .map_err(|e| crate::error::Error::Message(format!("Failed to write actions.yml: {}", e)))?;
    std::fs::rename(&tmp_path, &path)
        .map_err(|e| crate::error::Error::Message(format!("Failed to rename actions.yml: {}", e)))?;
    Ok(())
}

fn entry_to_response(id: &str, entry: &ActionEntry) -> ActionResponse {
    let name = entry
        .description
        .as_deref()
        .and_then(|d| {
            if d.is_empty() { None } else { Some(d.to_string()) }
        })
        .unwrap_or_else(|| id.replace("builtin_", ""))
        .trim()
        .to_string();

    ActionResponse {
        id: id.to_string(),
        name,
        tool_name: entry.tool_name.clone(),
        params: entry.params.clone(),
        enabled: entry.enabled,
        is_builtin: entry.is_builtin.unwrap_or(false),
        description: entry.description.clone(),
    }
}

// ── Handlers ──

/// GET /actions — list all actions.
pub(crate) async fn list_actions_handler(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<ActionResponse>> {
    let file = load_actions(&state.data_dir);
    let mut list: Vec<ActionResponse> = file
        .actions
        .iter()
        .map(|(id, entry)| entry_to_response(id, entry))
        .collect();
    // Sort by name for consistent display
    list.sort_by(|a, b| a.name.cmp(&b.name));
    Json(list)
}

/// POST /actions — create a new action.
pub(crate) async fn create_action_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateActionRequest>,
) -> impl IntoResponse {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Name is required" })),
        );
    }
    if body.tool_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Tool name is required" })),
        );
    }

    let mut file = load_actions(&state.data_dir);

    // Generate a unique ID
    let id = {
        let mut counter = 0u64;
        loop {
            let candidate = format!("a{}", counter);
            if !file.actions.contains_key(&candidate) {
                break candidate;
            }
            counter += 1;
        }
    };

    let entry = ActionEntry {
        enabled: true,
        tool_name: body.tool_name,
        params: body.params.unwrap_or(serde_json::json!({})),
        description: None,
        is_builtin: None,
    };

    file.actions.insert(id.clone(), entry);

    match save_actions(&state.data_dir, &file) {
        Ok(()) => {
            let list = list_actions_handler(State(state)).await;
            (StatusCode::CREATED, Json(serde_json::json!(list.0)))
        }
        Err(e) => {
            error!("Failed to save action: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        }
    }
}

/// PUT /actions/{id} — update an action.
pub(crate) async fn update_action_handler(
    State(state): State<Arc<AppState>>,
    Path(action_id): Path<String>,
    Json(body): Json<UpdateActionRequest>,
) -> impl IntoResponse {
    let mut file = load_actions(&state.data_dir);

    let entry = match file.actions.get_mut(&action_id) {
        Some(e) => e,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Action '{}' not found", action_id) })),
            );
        }
    };

    if let Some(tool_name) = body.tool_name {
        if tool_name.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Tool name cannot be empty" })),
            );
        }
        entry.tool_name = tool_name;
    }
    if let Some(params) = body.params {
        entry.params = params;
    }
    if let Some(enabled) = body.enabled {
        entry.enabled = enabled;
    }

    // Don't allow changing name for builtins via the YAML
    // (the name field is derived from description or ID)

    match save_actions(&state.data_dir, &file) {
        Ok(()) => {
            let list = list_actions_handler(State(state)).await;
            (StatusCode::OK, Json(serde_json::json!(list.0)))
        }
        Err(e) => {
            error!("Failed to save action: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        }
    }
}

/// DELETE /actions/{id} — delete an action.
pub(crate) async fn delete_action_handler(
    State(state): State<Arc<AppState>>,
    Path(action_id): Path<String>,
) -> impl IntoResponse {
    let mut file = load_actions(&state.data_dir);

    match file.actions.remove(&action_id) {
        Some(_) => match save_actions(&state.data_dir, &file) {
            Ok(()) => {
                let list = list_actions_handler(State(state)).await;
                (StatusCode::OK, Json(serde_json::json!(list.0)))
            }
            Err(e) => {
                error!("Failed to save action: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
            }
        },
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Action '{}' not found", action_id) })),
        ),
    }
}

/// POST /actions/{id}/run — execute a saved action via the MCP registry.
pub(crate) async fn run_action_handler(
    State(state): State<Arc<AppState>>,
    Path(action_id): Path<String>,
) -> impl IntoResponse {
    let file = load_actions(&state.data_dir);

    let entry = match file.actions.get(&action_id) {
        Some(e) => e.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Action '{}' not found", action_id) })),
            );
        }
    };

    if !entry.enabled {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Action is disabled" })),
        );
    }

    let call = McpToolCall {
        id: format!("action-run-{}", action_id),
        name: entry.tool_name,
        arguments: entry.params,
    };

    // Clone the registry snapshot under the lock, then drop the lock
    // before the async execute call (RwLockReadGuard is !Send).
    let mcp_snapshot = state.mcp_registry.read().unwrap().clone();
    match mcp_snapshot.execute(&call, state.app_context.clone())
        .await
    {
        Ok(result) => {
            let response = serde_json::json!({
                "result": result.content,
                "is_error": result.is_error,
            });
            (StatusCode::OK, Json(response))
        }
        Err(e) => {
            error!("Failed to execute action '{}': {:?}", action_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        }
    }
}
