//! Platforms API: platform names and channels.
//!
//! - `GET /platforms`: distinct platform names (from channels.yml)
//! - `GET /platforms/{name}/channels`: channels for a specific platform
//! - `GET /channels/all`: all channels (dashboard compat)
//!
//! Channels live in `{data_dir}/config/channels.yml` (the `channels` database
//! table is DROPPED). Channel ids are the channel NAMES (yml keys).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::Serialize;
use std::sync::Arc;
use tracing::error;

use super::{err_json, ok_json, AppState};
use crate::db::channels;

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
    pub id: String,
    pub name: String,
    pub resource_identifier: Option<String>,
    pub closed: bool,
}

#[derive(Debug, Serialize)]
pub struct ChannelEntry {
    pub id: String,
    pub name: String,
    pub platform: Option<String>,
    pub resource_identifier: Option<String>,
    pub closed: bool,
    pub current_profile: Option<String>,
    pub current_provider: Option<String>,
    pub current_model: Option<String>,
    pub readonly: bool,
    pub plan: bool,
    pub template: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /platforms: distinct platform names
async fn list_platforms_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let all = match channels::find_all_channels(&state.pool).await {
        Ok(chs) => chs,
        Err(e) => {
            error!("[platforms] load channels.yml failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch platforms",
            );
        }
    };
    let mut platforms: Vec<String> = all
        .iter()
        .filter_map(|c| c.platform.clone())
        .filter(|p| !p.is_empty())
        .collect();
    platforms.sort();
    platforms.dedup();
    let entries: Vec<PlatformNameEntry> = platforms
        .into_iter()
        .map(|platform| PlatformNameEntry { platform })
        .collect();
    ok_json(entries)
}

/// GET /platforms/{name}/channels: channels for a platform
async fn platform_channels_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let all = match channels::find_all_channels(&state.pool).await {
        Ok(chs) => chs,
        Err(e) => {
            error!("[platforms/{}/channels] load failed: {:?}", name, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch channels for platform",
            );
        }
    };
    let entries: Vec<PlatformChannelEntry> = all
        .into_iter()
        .filter(|c| c.platform.as_deref() == Some(name.as_str()))
        .map(|c| PlatformChannelEntry {
            id: c.id,
            name: c.name,
            resource_identifier: c.resource_identifier,
            closed: c.closed,
        })
        .collect();
    ok_json(entries)
}

/// GET /channels/all: all channels (dashboard compat)
async fn all_channels_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let all = match channels::find_all_channels(&state.pool).await {
        Ok(chs) => chs,
        Err(e) => {
            error!("[channels/all] load failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch channels",
            );
        }
    };
    let entries: Vec<ChannelEntry> = all
        .into_iter()
        .map(|c| ChannelEntry {
            id: c.id,
            name: c.name,
            platform: c.platform,
            resource_identifier: c.resource_identifier,
            closed: c.closed,
            current_profile: (!c.current_profile.is_empty()).then_some(c.current_profile),
            current_provider: c.current_provider,
            current_model: c.current_model,
            readonly: c.readonly,
            plan: c.plan,
            template: c.template,
        })
        .collect();
    ok_json(entries)
}
