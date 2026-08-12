//! Hooks CRUD + fire API (event-driven hooks).
//!
//! Mirrors the schedule (cron) API, but hooks are triggered by events
//! (thread_started / thread_finished / new_message) instead of time.
//!
//! - `GET    /hooks`            : list hooks (optional ?event= / ?enabled=)
//! - `GET    /hooks/{id}`       : single hook detail
//! - `POST   /hooks`            : create hook
//! - `PATCH  /hooks/{id}`       : update hook fields (NULLIF pattern)
//! - `PATCH  /hooks/{id}/toggle`: toggle enabled state
//! - `DELETE /hooks/{id}`       : delete hook
//! - `POST   /hooks/{id}/fire`  : manually trigger a hook (no counter)

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use tracing::error;

use super::{err_json, ok_json, AppState};
use crate::hooks::{default_counter, VALID_EVENTS, VALID_MODES, VALID_SCOPES};

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
// Row + response types
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
struct HookRow {
    id: String,
    name: String,
    display_name: String,
    event: String,
    scope: String,
    target: Option<String>,
    counter: Option<String>,
    count: i32,
    mode: String,
    prompt: Option<String>,
    action_id: Option<String>,
    profile: Option<String>,
    channel_id: Option<i64>,
    planning_mode: Option<String>,
    plan: Option<bool>,
    template: Option<String>,
    enabled: bool,
    created_at: Option<String>,
    updated_at: Option<String>,
}

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
    channel_id: Option<i64>,
    planning_mode: Option<String>,
    plan: Option<bool>,
    template: Option<String>,
    enabled: bool,
    created_at: Option<String>,
    updated_at: Option<String>,
}

impl HookResponse {
    fn from_row(r: HookRow) -> Self {
        let counter = r
            .counter
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(default_counter);
        Self {
            id: r.id,
            name: r.name,
            display_name: r.display_name,
            event: r.event,
            scope: r.scope,
            target: r.target,
            counter,
            count: r.count,
            mode: r.mode,
            prompt: r.prompt,
            action_id: r.action_id,
            profile: r.profile,
            channel_id: r.channel_id,
            planning_mode: r.planning_mode,
            plan: r.plan,
            template: r.template,
            enabled: r.enabled,
            created_at: r.created_at,
            updated_at: r.updated_at,
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
    channel_id: Option<i64>,
    #[serde(default)]
    planning_mode: Option<String>,
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
    channel_id: Option<i64>,
    #[serde(default)]
    planning_mode: Option<String>,
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
// Validation
// ---------------------------------------------------------------------------

fn validate_hook(
    event: &str,
    scope: Option<&str>,
    mode: Option<&str>,
    count: Option<i32>,
    action_id: Option<&str>,
) -> Option<String> {
    if !VALID_EVENTS.contains(&event) {
        return Some(format!(
            "Invalid event '{}': must be one of {:?}",
            event, VALID_EVENTS
        ));
    }
    if let Some(s) = scope {
        if !s.is_empty() && !VALID_SCOPES.contains(&s) {
            return Some(format!(
                "Invalid scope '{}': must be one of {:?}",
                s, VALID_SCOPES
            ));
        }
    }
    if let Some(m) = mode {
        if !m.is_empty() && !VALID_MODES.contains(&m) {
            return Some(format!(
                "Invalid mode '{}': must be one of {:?}",
                m, VALID_MODES
            ));
        }
    }
    if let Some(c) = count {
        if c < 1 {
            return Some("count must be >= 1".to_string());
        }
    }
    let mode_eff = mode.unwrap_or("agentic");
    if mode_eff == "action" && action_id.map(str::trim).unwrap_or("").is_empty() {
        return Some("mode=action requires an action_id".to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_hooks_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListHooksQuery>,
) -> impl IntoResponse {
    let rows: Result<Vec<HookRow>, _> = match (&q.event, q.enabled) {
        (Some(ev), Some(en)) => sql_forge!(
            HookRow,
            r#"
            SELECT id, name, display_name, event, scope, target,
                   counter::text AS counter, count, mode,
                   prompt, action_id, profile, channel_id, planning_mode, plan, template, enabled,
                   COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
                   COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
            FROM hooks
            WHERE event = :event AND enabled = :enabled
            ORDER BY created_at ASC, id ASC
            "#,
            ( :event = ev, :enabled = en )
        )
        .fetch_all(&state.pool)
        .await,
        (Some(ev), None) => sql_forge!(
            HookRow,
            r#"
            SELECT id, name, display_name, event, scope, target,
                   counter::text AS counter, count, mode,
                   prompt, action_id, profile, channel_id, planning_mode, plan, template, enabled,
                   COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
                   COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
            FROM hooks
            WHERE event = :event
            ORDER BY created_at ASC, id ASC
            "#,
            ( :event = ev )
        )
        .fetch_all(&state.pool)
        .await,
        (None, Some(en)) => sql_forge!(
            HookRow,
            r#"
            SELECT id, name, display_name, event, scope, target,
                   counter::text AS counter, count, mode,
                   prompt, action_id, profile, channel_id, planning_mode, plan, template, enabled,
                   COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
                   COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
            FROM hooks
            WHERE enabled = :enabled
            ORDER BY created_at ASC, id ASC
            "#,
            ( :enabled = en )
        )
        .fetch_all(&state.pool)
        .await,
        (None, None) => sql_forge!(
            HookRow,
            r#"
            SELECT id, name, display_name, event, scope, target,
                   counter::text AS counter, count, mode,
                   prompt, action_id, profile, channel_id, planning_mode, plan, template, enabled,
                   COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
                   COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
            FROM hooks
            ORDER BY created_at ASC, id ASC
            "#,
        )
        .fetch_all(&state.pool)
        .await,
    };

    match rows {
        Ok(rows) => {
            let data: Vec<HookResponse> = rows.into_iter().map(HookResponse::from_row).collect();
            ok_json(data)
        }
        Err(e) => {
            error!("[hooks] list failed: {:?}", e);
            err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to list hooks")
        }
    }
}

async fn get_hook_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let row: Result<Option<HookRow>, _> = sql_forge!(
        HookRow,
        r#"
        SELECT id, name, display_name, event, scope, target,
               counter::text AS counter, count, mode,
               prompt, action_id, profile, channel_id, planning_mode, plan, template, enabled,
               COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
               COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM hooks
        WHERE id = :id
        "#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await;

    match row {
        Ok(Some(r)) => ok_json(HookResponse::from_row(r)),
        Ok(None) => err_json(StatusCode::NOT_FOUND, "Hook not found"),
        Err(e) => {
            error!("[hooks] get '{}' failed: {:?}", id, e);
            err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to load hook")
        }
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
    if let Some(err) = validate_hook(
        &event,
        body.scope.as_deref(),
        body.mode.as_deref(),
        body.count,
        body.action_id.as_deref(),
    ) {
        return err_json(StatusCode::BAD_REQUEST, &err);
    }

    let id = body
        .id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("hook-{}", chrono::Utc::now().timestamp_millis()));
    let scope = body.scope.unwrap_or_else(|| "global".to_string());
    let mode = body.mode.unwrap_or_else(|| "agentic".to_string());
    let count = body.count.unwrap_or(1);
    let display_name = body.display_name.unwrap_or_else(|| body.name.clone());
    let enabled = body.enabled.unwrap_or(true);
    let plan = body.plan.unwrap_or(false);
    let counter = default_counter();

    let row: Result<HookRow, _> = sql_forge!(
        HookRow,
        r#"
        INSERT INTO hooks (
            id, name, display_name, event, scope, target, counter, count, mode,
            prompt, action_id, profile, channel_id, planning_mode, plan, template, enabled
        )
        VALUES (
            :id, :name, :display_name, :event, :scope, NULLIF(:target, '')::text, :counter::jsonb, :count, :mode,
            NULLIF(:prompt, '')::text, NULLIF(:action_id, '')::text, NULLIF(:profile, '')::text,
            NULLIF(:channel_id, 0::bigint), NULLIF(:planning_mode, '')::text, :plan, NULLIF(:template, '')::text, :enabled
        )
        RETURNING
            id, name, display_name, event, scope, target,
            counter::text AS counter, count, mode,
            prompt, action_id, profile, channel_id, planning_mode, plan, template, enabled,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        "#,
        (
            :id = &id,
            :name = &body.name,
            :display_name = &display_name,
            :event = &event,
            :scope = &scope,
            :target = body.target.as_deref().unwrap_or(""),
            :counter = &counter,
            :count = count,
            :mode = &mode,
            :prompt = body.prompt.as_deref().unwrap_or(""),
            :action_id = body.action_id.as_deref().unwrap_or(""),
            :profile = body.profile.as_deref().unwrap_or(""),
            :channel_id = body.channel_id.unwrap_or(0),
            :planning_mode = body.planning_mode.as_deref().unwrap_or(""),
            :plan = plan,
            :template = body.template.as_deref().unwrap_or(""),
            :enabled = enabled,
        )
    )
    .fetch_one(&state.pool)
    .await;

    match row {
        Ok(r) => ok_json(HookResponse::from_row(r)),
        Err(e) => {
            error!("[hooks] create '{}' failed: {:?}", id, e);
            err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create hook")
        }
    }
}

async fn update_hook_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateHookRequest>,
) -> impl IntoResponse {
    // Validate when present (event defaults to a valid placeholder when the
    // patch does not touch the event: only scope/mode/count/action_id matter).
    if let Some(event) = body.event.as_deref() {
        if let Some(err) = validate_hook(
            event,
            body.scope.as_deref(),
            body.mode.as_deref(),
            body.count,
            body.action_id.as_deref(),
        ) {
            return err_json(StatusCode::BAD_REQUEST, &err);
        }
    } else if let Some(err) = validate_hook(
        crate::hooks::EVENT_THREAD_STARTED,
        body.scope.as_deref(),
        body.mode.as_deref(),
        body.count,
        body.action_id.as_deref(),
    ) {
        return err_json(StatusCode::BAD_REQUEST, &err);
    }

    let row: Result<Option<HookRow>, _> = sql_forge!(
        HookRow,
        r#"
        UPDATE hooks SET
            name = COALESCE(NULLIF(:name, ''), name),
            display_name = COALESCE(NULLIF(:display_name, ''), display_name),
            event = COALESCE(NULLIF(:event, ''), event),
            scope = COALESCE(NULLIF(:scope, ''), scope),
            target = NULLIF(:target, ''),
            count = COALESCE(:count, count),
            mode = COALESCE(NULLIF(:mode, ''), mode),
            prompt = COALESCE(NULLIF(:prompt, ''), prompt),
            action_id = COALESCE(NULLIF(:action_id, ''), action_id),
            profile = COALESCE(NULLIF(:profile, ''), profile),
            channel_id = NULLIF(:channel_id, 0::bigint),
            planning_mode = NULLIF(:planning_mode, ''),
            plan = COALESCE(:plan, plan),
            template = COALESCE(NULLIF(:template, ''), template),
            enabled = COALESCE(:enabled, enabled),
            updated_at = NOW()
        WHERE id = :id
        RETURNING
            id, name, display_name, event, scope, target,
            counter::text AS counter, count, mode,
            prompt, action_id, profile, channel_id, planning_mode, plan, template, enabled,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        "#,
        (
            :id = &id,
            :name = body.name.as_deref().unwrap_or(""),
            :display_name = body.display_name.as_deref().unwrap_or(""),
            :event = body.event.as_deref().unwrap_or(""),
            :scope = body.scope.as_deref().unwrap_or(""),
            :target = body.target.as_deref().unwrap_or(""),
            :count = body.count.unwrap_or(-1),
            :mode = body.mode.as_deref().unwrap_or(""),
            :prompt = body.prompt.as_deref().unwrap_or(""),
            :action_id = body.action_id.as_deref().unwrap_or(""),
            :profile = body.profile.as_deref().unwrap_or(""),
            :channel_id = body.channel_id.unwrap_or(0),
            :planning_mode = body.planning_mode.as_deref().unwrap_or(""),
            :plan = body.plan.unwrap_or(false),
            :template = body.template.as_deref().unwrap_or(""),
            :enabled = body.enabled.unwrap_or(true),
        )
    )
    .fetch_optional(&state.pool)
    .await;

    match row {
        Ok(Some(r)) => ok_json(HookResponse::from_row(r)),
        Ok(None) => err_json(StatusCode::NOT_FOUND, "Hook not found"),
        Err(e) => {
            error!("[hooks] update '{}' failed: {:?}", id, e);
            err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update hook")
        }
    }
}

async fn toggle_hook_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let enabled: Result<Option<bool>, _> = sql_forge!(
        scalar bool,
        r#"
        UPDATE hooks
        SET enabled = NOT enabled, updated_at = NOW()
        WHERE id = :id
        RETURNING enabled
        "#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await;

    match enabled {
        Ok(Some(en)) => ok_json(serde_json::json!({ "id": id, "enabled": en })),
        Ok(None) => err_json(StatusCode::NOT_FOUND, "Hook not found"),
        Err(e) => {
            error!("[hooks] toggle '{}' failed: {:?}", id, e);
            err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to toggle hook")
        }
    }
}

async fn delete_hook_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let result = sql_forge!(
        "DELETE FROM hooks WHERE id = :id",
        ( :id = &id )
    )
    .execute(&state.pool)
    .await;

    match result {
        Ok(res) if res.rows_affected() > 0 => ok_json(serde_json::json!({ "deleted": id })),
        Ok(_) => err_json(StatusCode::NOT_FOUND, "Hook not found"),
        Err(e) => {
            error!("[hooks] delete '{}' failed: {:?}", id, e);
            err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to delete hook")
        }
    }
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
