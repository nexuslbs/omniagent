//! Platforms API - platform names, channels, subscriptions.
//!
//! - `GET /platforms` - distinct platform names
//! - `GET /platforms/{name}/channels` - channels for a specific platform
//! - `GET /platforms/{name}/subscriptions` - subscriptions for a platform
//! - `POST /platforms/subscriptions` - add a channel subscription
//! - `DELETE /platforms/subscriptions/{id}` - remove a channel subscription

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
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

pub fn platforms_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/platforms", get(list_platforms_handler))
        .route("/platforms/{name}/channels", get(platform_channels_handler))
        .route(
            "/platforms/{name}/subscriptions",
            get(platform_subscriptions_handler),
        )
        .route("/platforms/subscriptions", post(add_subscription_handler))
        .route(
            "/platforms/subscriptions/{id}",
            delete(remove_subscription_handler),
        )
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

#[derive(Debug, Serialize)]
pub struct PlatformSubscriptionEntry {
    pub id: i64,
    pub channel_id: i64,
    pub channel_name: Option<String>,
    pub platform: Option<String>,
    pub resource_identifier: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddSubscriptionBody {
    pub channel_id: i64,
    pub platform: String,
}

#[derive(Debug, Serialize)]
pub struct SubscriptionResult {
    pub id: i64,
    pub channel_id: i64,
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
struct PlatformSubscriptionRow {
    id: i64,
    channel_id: i64,
    channel_name: Option<String>,
    platform: Option<String>,
    resource_identifier: Option<String>,
}

#[derive(FromRow)]
struct InsertedSubscriptionRow {
    id: i64,
    channel_id: i64,
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

/// GET /platforms - distinct platform names
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

/// GET /platforms/{name}/channels - channels for a platform
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

/// GET /platforms/{name}/subscriptions - subscriptions for a platform
async fn platform_subscriptions_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let subscriptions = match sql_forge!(
        PlatformSubscriptionRow,
        r#"
        SELECT
            s.id,
            s.channel_id,
            c.name AS channel_name,
            c.platform,
            c.resource_identifier
        FROM channel_subscriptions s
        JOIN channels c ON c.id = s.channel_id
        WHERE c.platform = :platform
        ORDER BY c.name
        "#,
        ( :platform = &name )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[platforms/{}/subscriptions] query failed: {:?}", name, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch subscriptions",
            );
        }
    };

    ok_json(subscriptions)
}

/// POST /platforms/subscriptions - add a channel subscription (ON CONFLICT DO NOTHING)
async fn add_subscription_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddSubscriptionBody>,
) -> impl IntoResponse {
    let result = match sql_forge!(
        InsertedSubscriptionRow,
        r#"
        INSERT INTO channel_subscriptions (channel_id, subscriber_platform)
        VALUES (:channel_id, :platform)
        RETURNING id, channel_id
        "#,
        ( :channel_id = body.channel_id,
          :platform = &body.platform )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => SubscriptionResult {
            id: row.id,
            channel_id: row.channel_id,
        },
        Ok(None) => {
            return err_json(StatusCode::CONFLICT, "Subscription already exists");
        }
        Err(e) => {
            error!("[platforms/subscriptions] insert failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to add subscription",
            );
        }
    };

    ok_json(result)
}

/// DELETE /platforms/subscriptions/{id} - remove a channel subscription
async fn remove_subscription_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match sql_forge!(
        r#"DELETE FROM channel_subscriptions WHERE id = :id"#,
        ( :id = id )
    )
    .execute(&state.pool)
    .await
    {
        Ok(result) if result.rows_affected() > 0 => (),
        Ok(_) => {
            return err_json(StatusCode::NOT_FOUND, "Subscription not found");
        }
        Err(e) => {
            error!("[platforms/subscriptions/{}] delete failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to remove subscription",
            );
        }
    };

    ok_json(serde_json::json!({ "deleted": true }))
}

/// GET /channels/all - all channels (for subscription UI)
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
