//! Models API: GET/PUT `/api/models` - read/write `config/models.yml`
//! (provider/model overrides via a pure definition file, no plugin code).
//!
//! - `GET /api/models`: returns the parsed models.yml content
//! - `PUT /api/models`: validates + atomically writes models.yml, then
//!   rebuilds provider metadata so overrides apply without a restart.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;
use std::sync::Arc;

use super::{err_json, ok_json, AppState};
use crate::models_yaml::{self, ModelsFile};

pub async fn get_models_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match models_yaml::load_models_file(&state.data_dir) {
        Ok(file) => ok_json(file),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("models.yml: {}", e),
        ),
    }
}

pub async fn put_models_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ModelsFile>,
) -> impl IntoResponse {
    // Validate BEFORE the atomic write: malformed input is rejected with a
    // clear message and models.yml is left untouched.
    if let Err(e) = models_yaml::validate_models_file(&body) {
        return err_json(StatusCode::BAD_REQUEST, &format!("models.yml: {}", e));
    }
    if let Err(e) = models_yaml::save_models_file(&state.data_dir, &body) {
        return err_json(StatusCode::BAD_REQUEST, &format!("models.yml: {}", e));
    }
    // Rebuild PROVIDER_METADATA so provider overrides / plugin-less providers
    // take effect immediately (no restart needed).
    crate::llm::refresh_provider_metadata();
    ok_json(json!({ "ok": true }))
}
