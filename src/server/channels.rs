//! Channels API: list, detail, and update channels.
//!
//! Channel definitions AND runtime state live in `{data_dir}/config/channels.yml`
//! (the `channels` database table is DROPPED — see db-migrations). The channel
//! NAME (the yml key) is the stable identifier used everywhere: API ids,
//! `threads.channel_id`, `messages.channel_id`, `kanban_tasks.channel_id`,
//! `summaries.channel_id` and tasks.yml `channel:` references.
//!
//! - `GET  /channels`      : list all channels
//! - `GET  /channels/{id}` : get single channel detail (id == name)
//! - `PATCH /channels/{id}`: update runtime fields (current_profile /
//!   current_provider / current_model / closed / readonly / plan / template) —
//!   persisted atomically to channels.yml. Definition fields
//!   (platform / resource_identifier / cause) are NOT editable via the API;
//!   they change only by editing the yml (or via the `update_channel_platform`
//!   identity-change path used when a plugin's resource identifier changes).
//!
//! Response shape keeps the legacy `current_*` names + `plan` for dashboard
//! compatibility (the yml itself uses bare `profile`/`model`/`provider`).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::error;

use super::{err_json, ok_json, AppState};
use crate::db::channels;
use crate::db::types::Channel;
use crate::error::Error;

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
// Types: Response
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ChannelEntry {
    pub id: String,
    pub name: String,
    pub platform: Option<String>,
    pub resource_identifier: Option<String>,
    pub external_id: Option<String>,
    pub cause: String,
    pub closed: bool,
    pub current_profile: String,
    pub current_provider: Option<String>,
    pub current_model: Option<String>,
    pub readonly: bool,
    pub plan: bool,
    pub planning_mode: Option<String>,
    pub template: Option<String>,
}

impl From<Channel> for ChannelEntry {
    fn from(c: Channel) -> Self {
        let rid = c
            .resource_identifier
            .clone()
            .or_else(|| c.external_id.clone());
        Self {
            id: c.id.clone(),
            name: c.name,
            platform: c.platform,
            resource_identifier: rid.clone(),
            // external_id was always equal to resource_identifier; derive for
            // response compatibility (NOT stored in the yml).
            external_id: rid,
            cause: c.cause,
            closed: c.closed,
            current_profile: c.current_profile,
            current_provider: c.current_provider,
            current_model: c.current_model,
            readonly: c.readonly,
            plan: c.plan,
            planning_mode: None,
            template: c.template,
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_channels_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let all = match channels::find_all_channels(&state.pool).await {
        Ok(chs) => chs,
        Err(e) => {
            error!("Failed to list channels: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load channels from channels.yml",
            );
        }
    };
    let entries: Vec<ChannelEntry> = all.into_iter().map(ChannelEntry::from).collect();
    ok_json(entries)
}

async fn get_channel_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let channel = match channels::get_channel_by_name(&state.pool, &id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return err_json(
                StatusCode::NOT_FOUND,
                &format!("Channel '{}' not found", id),
            );
        }
        Err(e) => {
            error!("Failed to load channel '{}': {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load channel from channels.yml",
            );
        }
    };
    ok_json(ChannelEntry::from(channel))
}

// PATCH body — runtime-mutable fields only (NULLIF-style partial updates:
// None = leave unchanged, Some("") = clear, Some(v) = set).
#[derive(Debug, Deserialize)]
pub struct UpdateChannelRequest {
    pub name: Option<String>,
    pub current_profile: Option<String>,
    pub current_provider: Option<String>,
    pub current_model: Option<String>,
    pub closed: Option<bool>,
    pub readonly: Option<bool>,
    pub plan: Option<bool>,
    pub template: Option<String>,
}

async fn update_channel_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateChannelRequest>,
) -> impl IntoResponse {
    // Load current definition (also gives the readonly flag).
    let current = match channels::get_channel_by_name(&state.pool, &id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return err_json(
                StatusCode::NOT_FOUND,
                &format!("Channel '{}' not found", id),
            );
        }
        Err(e) => {
            error!("Failed to load channel '{}': {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load channel from channels.yml",
            );
        }
    };

    // Readonly channels: only closed / profile / provider / model may change
    // (name / plan / template are definition-level; the old DB rule).
    if current.readonly
        && (body.name.is_some()
            || body.plan.is_some()
            || body.template.is_some()
            || body.readonly.is_some())
    {
        return err_json(
            StatusCode::BAD_REQUEST,
            "Readonly channels only allow updating closed/profile/provider/model",
        );
    }

    if let Err(e) = channels::mutate_channel(&id, |existing| {
        let mut d = existing
            .cloned()
            .ok_or_else(|| Error::Message(format!("Channel '{}' not found", id)))?;
        if let Some(name) = body.name.as_deref() {
            if !name.trim().is_empty() {
                return Err(Error::Message(
                    "Channel name is the yml key and cannot be renamed via PATCH".to_string(),
                ));
            }
        }
        if let Some(profile) = body.current_profile.as_deref() {
            d.profile = (!profile.trim().is_empty()).then(|| profile.to_string());
        }
        if let Some(provider) = body.current_provider.as_deref() {
            d.provider = (!provider.trim().is_empty()).then(|| provider.to_string());
        }
        if let Some(model) = body.current_model.as_deref() {
            d.model = (!model.trim().is_empty()).then(|| model.to_string());
        }
        if let Some(closed) = body.closed {
            d.closed = Some(closed);
        }
        if let Some(readonly) = body.readonly {
            d.readonly = Some(readonly);
        }
        if let Some(plan) = body.plan {
            d.plan = Some(plan);
        }
        if let Some(template) = body.template.as_deref() {
            d.template = (!template.trim().is_empty()).then(|| template.to_string());
        }
        Ok(d)
    }) {
        error!("Failed to update channel '{}': {:?}", id, e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to persist channel update: {e}"),
        );
    }

    match channels::get_channel_by_name(&state.pool, &id).await {
        Ok(Some(c)) => ok_json(ChannelEntry::from(c)),
        Ok(None) => err_json(
            StatusCode::NOT_FOUND,
            &format!("Channel '{}' not found", id),
        ),
        Err(e) => {
            error!("Failed to reload channel '{}': {:?}", id, e);
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to reload channel from channels.yml",
            )
        }
    }
}
