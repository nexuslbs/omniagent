//! Platforms API: platform names and channels.
//!
//! - `GET /platforms`: distinct platform names
//! - `GET /platforms/{name}/channels`: channels for a specific platform

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::Serialize;
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use tracing::error;

use super::{err_json, ok_json, AppState};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn platforms_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/platforms", get(list_platforms_handler))
        .route("/platforms/{name}/channels", get(platform_channels_handler))
        .route("/channels/all", get(all_channels_handler))
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct PlatformNameEntry {
    pub platform: String,
}

#[derive(Debug, Serialize)]
pub struct PlatformChannelEntry {
    pub id: i64,
    pub name: String,
    pub resource_identifier: Option<String>,
    pub closed: bool,
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(FromRow)]
struct PlatformNameRow {
    platform: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
struct PlatformChannelRow {
    id: i64,
    name: String,
    resource_identifier: Option<String>,
    closed: bool,
}

#[derive(Debug, Serialize, FromRow)]
struct ChannelRow {
    id: i64,
    name: String,
    platform: Option<String>,
    resource_identifier: Option<String>,
    closed: bool,
    current_profile: Option<String>,
    current_provider: Option<String>,
    current_model: Option<String>,
    readonly: bool,
    plan: bool,
    template: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /platforms: distinct platform names
async fn list_platforms_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let platforms = match sql_forge!(
        PlatformNameRow,
        r#"SELECT DISTINCT platform FROM channels WHERE platform IS NOT NULL AND platform != '' ORDER BY platform"#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|r| r.platform.map(|p| PlatformNameEntry { platform: p }))
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[platforms] list query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch platforms",
            );
        }
    };

    ok_json(platforms)
}

/// GET /platforms/{name}/channels: channels for a platform
async fn platform_channels_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let channels = match sql_forge!(
        PlatformChannelRow,
        r#"
        SELECT id, name, resource_identifier, closed
        FROM channels
        WHERE platform = :platform
        ORDER BY name
        "#,
        ( :platform = &name )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[platforms/{}/channels] query failed: {:?}", name, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch channels for platform",
            );
        }
    };

    ok_json(channels)
}

/// GET /channels/all: all channels
async fn all_channels_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let channels = match sql_forge!(
        ChannelRow,
        r#"
        SELECT id, name, platform, resource_identifier, closed,
               current_profile, current_provider, current_model,
               readonly, plan, template
        FROM channels
        ORDER BY name
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[channels/all] query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch channels",
            );
        }
    };

    ok_json(channels)
}
