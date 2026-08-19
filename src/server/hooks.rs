//! Hooks CRUD + fire API (event-driven hooks).
//!
//! Mirrors the schedule (cron) API, but hooks are triggered by events
//! (thread_started / thread_finished / new_message) instead of time.
//!
//! Definitions live in `{data_dir}/config/tasks.yml` (`hooks:` key — the
//! git-tracked source of truth); every handler reads/writes it (parsed fresh
//! per request). The only runtime state is the per-hook JSON counter in the
//! `hook_counters` table, surfaced through the `counter` response field.
//! The API response shape mirrors tasks.yml (id = yml key, name = key, and the
//! bare yml field names such as `channel`/`profile`/`plan`).
//!
//! - `GET    /hooks`            : list hooks (optional ?event= / ?enabled=)
//! - `GET    /hooks/{id}`       : single hook detail
//! - `POST   /hooks`            : create hook
//! - `PATCH  /hooks/{id}`       : update hook fields
//! - `PATCH  /hooks/{id}/toggle`: toggle enabled state
//! - `DELETE /hooks/{id}`       : delete hook (also removes its counter row)
//! - `POST   /hooks/{id}/fire`  : manually trigger a hook (no counter)

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::error;

use super::{err_json, ok_json, AppState};
use crate::hooks::default_counter;
use crate::tasks_yaml::{self, HookDef};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn hooks_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/hooks", get(list_hooks_handler))
        .route("/hooks/{id}", get(get_hook_handler))
        .route("/hooks", post(create_hook_handler))
        .route("/hooks/{id}", patch(update_hook_handler))
        .route("/hooks/{id}/toggle", patch(toggle_hook_handler))
        .route("/hooks/{id}", delete(delete_hook_handler))
        .route("/hooks/{id}/fire", post(fire_hook_handler))
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct HookResponse {
    id: String,
    name: String,
    display_name: String,
    event: String,
    scope: String,
    target: Option<String>,
    counter: serde_json::Value,
    count: i32,
    mode: String,
    prompt: Option<String>,
    action_id: Option<String>,
    profile: Option<String>,
    channel: Option<String>,
    plan: Option<bool>,
    template: Option<String>,
    enabled: bool,
    created_at: Option<String>,
    updated_at: Option<String>,
}

impl HookResponse {
    fn from_def(
        key: &str,
        def: &HookDef,
        channel: Option<String>,
        counter: serde_json::Value,
    ) -> Self {
        let plan = def.plan();
        Self {
            id: key.to_string(),
            name: key.to_string(),
            display_name: def.display_name.clone().unwrap_or_else(|| key.to_string()),
            event: def.event.clone(),
            scope: def.scope.clone(),
            target: def.target.clone(),
            counter,
            count: def.count,
            mode: def.mode(),
            prompt: def.prompt.clone(),
            action_id: def.action.clone(),
            profile: def.profile.clone(),
            channel,
            plan,
            template: def.template.clone(),
            enabled: def.enabled,
            created_at: None,
            updated_at: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateHookRequest {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    event: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    count: Option<i32>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    action_id: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    plan: Option<bool>,
    #[serde(default)]
    template: Option<String>,
    #[serde(default = "default_true")]
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct UpdateHookRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    count: Option<i32>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    action_id: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    plan: Option<bool>,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ListHooksQuery {
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

fn default_true() -> Option<bool> {
    Some(true)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Load all hook counters from `hook_counters` (missing → default 0 counter).
async fn load_counters(pool: &PgPool) -> HashMap<String, serde_json::Value> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT hook_key, counter::text AS counter FROM hook_counters")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    rows.into_iter()
        .map(|(hook_key, counter)| {
            let value = serde_json::from_str(&counter).unwrap_or_else(|_| default_counter());
            (hook_key, value)
        })
        .collect()
}

/// Load a single hook's counter (missing → default 0 counter).
async fn load_counter(pool: &PgPool, key: &str) -> serde_json::Value {
    sqlx::query_scalar::<_, String>(
        "SELECT counter::text AS counter FROM hook_counters WHERE hook_key = $1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .and_then(|counter| serde_json::from_str(&counter).ok())
    .unwrap_or_else(default_counter)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_hooks_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListHooksQuery>,
) -> impl IntoResponse {
    let tasks = match tasks_yaml::load_tasks(&state.data_dir) {
        Ok(t) => t,
        Err(e) => {
            error!("[hooks] load tasks.yml failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load hooks from tasks.yml",
            );
        }
    };
    let counters = load_counters(&state.pool).await;

    let mut keys: Vec<&String> = tasks
        .hooks
        .iter()
        .filter(|(_, def)| {
            if let Some(ev) = &q.event {
                if &def.event != ev {
                    return false;
                }
            }
            if let Some(en) = q.enabled {
                if def.enabled != en {
                    return false;
                }
            }
            true
        })
        .map(|(k, _)| k)
        .collect();
    keys.sort();

    let mut data: Vec<HookResponse> = Vec::with_capacity(keys.len());
    for key in keys {
        let def = &tasks.hooks[key];
        let channel = tasks_yaml::resolve_channel_id(&state.pool, def.channel.as_deref()).await;
        let counter = counters.get(key).cloned().unwrap_or_else(default_counter);
        data.push(HookResponse::from_def(key, def, channel, counter));
    }
    ok_json(data)
}

async fn get_hook_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let tasks = match tasks_yaml::load_tasks(&state.data_dir) {
        Ok(t) => t,
        Err(e) => {
            error!("[hooks] load tasks.yml failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load hooks from tasks.yml",
            );
        }
    };
    match tasks.hooks.get(&id) {
        Some(def) => {
            let channel = tasks_yaml::resolve_channel_id(&state.pool, def.channel.as_deref()).await;
            let counter = load_counter(&state.pool, &id).await;
            ok_json(HookResponse::from_def(&id, def, channel, counter))
        }
        None => err_json(StatusCode::NOT_FOUND, "Hook not found"),
    }
}

async fn create_hook_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateHookRequest>,
) -> impl IntoResponse {
    if body.name.trim().is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "name is required");
    }
    let event = body.event.trim().to_string();
    if event.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "event is required");
    }
    let id = body
        .id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("hook-{}", chrono::Utc::now().timestamp_millis()));

    let mut def = HookDef {
        event,
        scope: body.scope.unwrap_or_else(|| "global".to_string()),
        target: body.target.clone().filter(|t| !t.trim().is_empty()),
        count: body.count.unwrap_or(1),
        prompt: body.prompt.clone().filter(|p| !p.trim().is_empty()),
        profile: body.profile.clone().filter(|p| !p.trim().is_empty()),
        template: body.template.clone().filter(|t| !t.trim().is_empty()),
        display_name: body.display_name.clone().filter(|d| !d.trim().is_empty()),
        ..Default::default()
    };
    if let Some(action_id) = body.action_id.clone().filter(|a| !a.trim().is_empty()) {
        def.action = Some(action_id);
    }
    if let Some(mode) = body.mode.clone().filter(|m| !m.trim().is_empty()) {
        def.mode = Some(mode);
    }
    def.plan = body.plan;
    def.enabled = body.enabled.unwrap_or(true);
    def.channel = tasks_yaml::channel_name_for_id(&state.pool, body.channel).await;

    if let Err(err) = tasks_yaml::validate_hook(&id, &def) {
        return err_json(StatusCode::BAD_REQUEST, &err);
    }

    let mut tasks = match tasks_yaml::load_tasks(&state.data_dir) {
        Ok(t) => t,
        Err(e) => {
            error!("[hooks] load tasks.yml failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load hooks from tasks.yml",
            );
        }
    };
    tasks.hooks.insert(id.clone(), def);
    if let Err(e) = tasks_yaml::save_tasks(&state.data_dir, &tasks) {
        error!("[hooks] save tasks.yml failed: {:?}", e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save hook to tasks.yml",
        );
    }
    ok_json(serde_json::json!({ "success": true, "id": id }))
}

async fn update_hook_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateHookRequest>,
) -> impl IntoResponse {
    let mut tasks = match tasks_yaml::load_tasks(&state.data_dir) {
        Ok(t) => t,
        Err(e) => {
            error!("[hooks] load tasks.yml failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load hooks from tasks.yml",
            );
        }
    };
    let def = match tasks.hooks.get_mut(&id) {
        Some(d) => d,
        None => return err_json(StatusCode::NOT_FOUND, "Hook not found"),
    };

    if let Some(ev) = body.event.clone() {
        if !ev.is_empty() {
            def.event = ev;
        }
    }
    if let Some(s) = body.scope.clone() {
        if !s.is_empty() {
            def.scope = s;
        }
    }
    if let Some(t) = body.target.clone() {
        def.target = if t.is_empty() { None } else { Some(t) };
    }
    if let Some(c) = body.count {
        def.count = c;
    }
    if let Some(p) = body.prompt.clone() {
        def.prompt = if p.is_empty() { None } else { Some(p) };
    }
    if let Some(p) = body.profile.clone() {
        def.profile = if p.is_empty() { None } else { Some(p) };
    }
    if let Some(t) = body.template.clone() {
        def.template = if t.is_empty() { None } else { Some(t) };
    }
    if let Some(d) = body.display_name.clone() {
        def.display_name = if d.is_empty() { None } else { Some(d) };
    }
    if let Some(mode) = body.mode.clone() {
        def.mode = if mode.is_empty() { None } else { Some(mode) };
    }
    if let Some(action_id) = body.action_id.clone() {
        def.action = if action_id.is_empty() {
            None
        } else {
            Some(action_id)
        };
    }
    if let Some(cid) = body.channel {
        if cid.trim().is_empty() {
            def.channel = None;
        } else {
            def.channel = tasks_yaml::channel_name_for_id(&state.pool, Some(cid)).await;
        }
    }
    if body.plan.is_some() {
        def.plan = body.plan;
    }
    if let Some(en) = body.enabled {
        def.enabled = en;
    }

    if let Err(err) = tasks_yaml::validate_hook(&id, def) {
        return err_json(StatusCode::BAD_REQUEST, &err);
    }
    if let Err(e) = tasks_yaml::save_tasks(&state.data_dir, &tasks) {
        error!("[hooks] save tasks.yml failed: {:?}", e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save hook to tasks.yml",
        );
    }
    ok_json(serde_json::json!({ "success": true }))
}

async fn toggle_hook_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mut tasks = match tasks_yaml::load_tasks(&state.data_dir) {
        Ok(t) => t,
        Err(e) => {
            error!("[hooks] load tasks.yml failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load hooks from tasks.yml",
            );
        }
    };
    match tasks.hooks.get_mut(&id) {
        Some(def) => {
            def.enabled = !def.enabled;
            let en = def.enabled;
            if let Err(e) = tasks_yaml::save_tasks(&state.data_dir, &tasks) {
                error!("[hooks] save tasks.yml failed: {:?}", e);
                return err_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to save hook to tasks.yml",
                );
            }
            ok_json(serde_json::json!({ "id": id, "enabled": en }))
        }
        None => err_json(StatusCode::NOT_FOUND, "Hook not found"),
    }
}

async fn delete_hook_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mut tasks = match tasks_yaml::load_tasks(&state.data_dir) {
        Ok(t) => t,
        Err(e) => {
            error!("[hooks] load tasks.yml failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load hooks from tasks.yml",
            );
        }
    };
    if tasks.hooks.remove(&id).is_none() {
        return err_json(StatusCode::NOT_FOUND, "Hook not found");
    }
    // Clean up runtime counter state for the deleted hook.
    if let Err(e) = sqlx::query("DELETE FROM hook_counters WHERE hook_key = $1")
        .bind(&id)
        .execute(&state.pool)
        .await
    {
        error!("[hooks] delete counter row '{}' failed: {:?}", id, e);
    }
    if let Err(e) = tasks_yaml::save_tasks(&state.data_dir, &tasks) {
        error!("[hooks] save tasks.yml failed: {:?}", e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save hook to tasks.yml",
        );
    }
    ok_json(serde_json::json!({ "deleted": id }))
}

/// Manually trigger a hook (no counter increment, no reset).
async fn fire_hook_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match crate::hooks::fire_hook_by_id(
        &state.pool,
        &state.data_dir,
        &state.plugin_manager,
        &state.app_context,
        &id,
    )
    .await
    {
        Ok(thread_id) => ok_json(serde_json::json!({ "id": id, "thread_id": thread_id })),
        Err(e) => {
            error!("[hooks] fire '{}' failed: {:?}", id, e);
            err_json(
                StatusCode::BAD_REQUEST,
                &format!("Failed to fire hook: {}", e),
            )
        }
    }
}
