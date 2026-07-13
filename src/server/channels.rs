//! Channels API - list, detail, and update channels.
//!
//! Replaces the dashboard's direct PostgreSQL queries at
//! `omni-dashboard/repo/server/routes/channels.ts`.
//!
//! - `GET  /channels`       - list all channels
//! - `GET  /channels/{id}`  - get single channel detail
//! - `PATCH /channels/{id}` - update channel fields (NULLIF pattern)

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use tracing::error;

use super::{err_json, ok_json, AppState};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn channels_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/channels", get(list_channels_handler))
        .route("/channels/{id}", get(get_channel_handler))
        .route("/channels/{id}", patch(update_channel_handler))
}

// ---------------------------------------------------------------------------
// Types - Response
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ChannelEntry {
    pub id: i64,
    pub name: String,
    pub platform: Option<String>,
    pub resource_identifier: Option<String>,
    pub closed: bool,
    pub current_profile: String,
    pub current_provider: Option<String>,
    pub current_model: Option<String>,
    pub readonly: bool,
    pub plan: bool,
    pub template: Option<String>,
}

// ---------------------------------------------------------------------------
// Types - Row types for sqlx / sql_forge
// ---------------------------------------------------------------------------

#[derive(FromRow)]
struct ChannelRow {
    id: i64,
    name: String,
    platform: Option<String>,
    resource_identifier: Option<String>,
    closed: Option<bool>,
    current_profile: String,
    current_provider: Option<String>,
    current_model: Option<String>,
    readonly: bool,
    plan: bool,
    template: Option<String>,
}

#[derive(FromRow)]
struct ChannelReadonlyRow {
    readonly: bool,
    closed: bool,
    plan: bool,
}

// ---------------------------------------------------------------------------
// Types - PATCH request body
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UpdateChannelRequest {
    pub name: Option<String>,
    pub current_profile: Option<String>,
    pub current_provider: Option<String>,
    pub current_model: Option<String>,
    pub closed: Option<bool>,
    pub plan: Option<bool>,
    pub template: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /channels - list all channels, ordered by name.
async fn list_channels_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let channels = match sql_forge!(
        ChannelRow,
        r#"
        SELECT
            id,
            name,
            platform,
            resource_identifier,
            closed,
            current_profile,
            current_provider,
            current_model,
            readonly,
            plan,
            template
        FROM channels
        ORDER BY name
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| ChannelEntry {
                id: r.id,
                name: r.name,
                platform: r.platform,
                resource_identifier: r.resource_identifier,
                closed: r.closed.unwrap_or(false),
                current_profile: r.current_profile,
                current_provider: r.current_provider,
                current_model: r.current_model,
                readonly: r.readonly,
                plan: r.plan,
                template: r.template,
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[channels] list query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch channels",
            );
        }
    };

    ok_json(channels)
}

/// GET /channels/{id} - get a single channel by id.
async fn get_channel_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let channel = match sql_forge!(
        ChannelRow,
        r#"
        SELECT
            id,
            name,
            platform,
            resource_identifier,
            closed,
            current_profile,
            current_provider,
            current_model,
            readonly,
            plan,
            template
        FROM channels
        WHERE id = :id
        "#,
        ( :id = id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(r)) => ChannelEntry {
            id: r.id,
            name: r.name,
            platform: r.platform,
            resource_identifier: r.resource_identifier,
            closed: r.closed.unwrap_or(false),
            current_profile: r.current_profile,
            current_provider: r.current_provider,
            current_model: r.current_model,
            readonly: r.readonly,
            plan: r.plan,
            template: r.template,
        },
        Ok(None) => {
            return err_json(StatusCode::NOT_FOUND, "Channel not found");
        }
        Err(e) => {
            error!("[channels/{}] get query failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch channel");
        }
    };

    ok_json(channel)
}

/// PATCH /channels/{id} - update channel fields.
///
/// Uses the NULLIF pattern to convert empty strings to NULL, and only
/// updates fields that are explicitly provided in the request body.
/// Checks the readonly constraint before updating.
async fn update_channel_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateChannelRequest>,
) -> impl IntoResponse {
    // ── 1. Check if channel exists and get readonly status ──
    let existing = match sql_forge!(
        ChannelReadonlyRow,
        r#"
        SELECT readonly, closed, plan
        FROM channels
        WHERE id = :id
        "#,
        ( :id = id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return err_json(StatusCode::NOT_FOUND, "Channel not found");
        }
        Err(e) => {
            error!("[channels/{}] check query failed: {:?}", id, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to check channel");
        }
    };

    // ── 2. Enforce readonly constraints ──
    // Readonly channels can only update: closed, current_profile, current_provider, current_model.
    // They cannot be renamed (name) or have plan/template changed.
    if existing.readonly {
        let allowed = body.closed.is_some()
            || body.current_profile.is_some()
            || body.current_provider.is_some()
            || body.current_model.is_some();
        let blocked =
            body.name.is_some() || body.plan.is_some() || body.template.is_some();
        if !allowed || blocked {
            return err_json(
                StatusCode::FORBIDDEN,
                "Permanent channels can only update status, profile, provider, and model",
            );
        }
    }

    // ── 3. Apply updates using NULLIF pattern ──
    // Each field uses NULLIF to convert empty string → NULL.
    // Fields not provided in the request body receive the current DB value
    // via COALESCE, preserving existing data.
    //
    // Note: boolean fields (closed, plan) don't use the NULLIF
    // pattern since they are not nullable text columns.
    if let Err(e) = sql_forge!(
        r#"
        UPDATE channels
        SET
            name = CASE
                WHEN :name = '' THEN name
                ELSE NULLIF(:name, '')::text
            END,
            current_profile = CASE
                WHEN :current_profile = '' THEN current_profile
                ELSE NULLIF(:current_profile, '')::text
            END,
            current_provider = CASE
                WHEN :current_provider = '' THEN current_provider
                ELSE NULLIF(:current_provider, '')::text
            END,
            current_model = CASE
                WHEN :current_model = '' THEN current_model
                ELSE NULLIF(:current_model, '')::text
            END,
            closed = :closed,
            plan = :plan,
            template = CASE
                WHEN :template = '' THEN template
                ELSE NULLIF(:template, '')::text
            END,
            updated_at = NOW()
        WHERE id = :id
        "#,
        ( :name = body.name.as_deref().unwrap_or(""),
          :current_profile = body.current_profile.as_deref().unwrap_or(""),
          :current_provider = body.current_provider.as_deref().unwrap_or(""),
          :current_model = body.current_model.as_deref().unwrap_or(""),
          :closed = body.closed.unwrap_or(existing.closed),
          :plan = body.plan.unwrap_or(existing.plan),
          :template = body.template.as_deref().unwrap_or(""),
          :id = id )
    )
    .execute(&state.pool)
    .await
    {
        error!("[channels/{}] update failed: {:?}", id, e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update channel",
        );
    }

    // ── 4. Return the updated channel ──
    // Re-fetch the channel using the detail query
    let updated = match sql_forge!(
        ChannelRow,
        r#"
        SELECT
            id,
            name,
            platform,
            resource_identifier,
            closed,
            current_profile,
            current_provider,
            current_model,
            readonly,
            plan,
            template
        FROM channels
        WHERE id = :id
        "#,
        ( :id = id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(r)) => ChannelEntry {
            id: r.id,
            name: r.name,
            platform: r.platform,
            resource_identifier: r.resource_identifier,
            closed: r.closed.unwrap_or(false),
            current_profile: r.current_profile,
            current_provider: r.current_provider,
            current_model: r.current_model,
            readonly: r.readonly,
            plan: r.plan,
            template: r.template,
        },
        Ok(None) => {
            return err_json(StatusCode::NOT_FOUND, "Channel not found after update");
        }
        Err(e) => {
            error!("[channels/{}] re-fetch after update failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch updated channel",
            );
        }
    };

    ok_json(updated)
}
