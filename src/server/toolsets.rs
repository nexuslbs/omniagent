//! Toolsets API: GET/PUT `/api/toolsets` - read/write `config/toolsets.yml`.
//!
//! A toolset is a named, reusable tool allow-list. `config/toolsets.yml` is a
//! pure definition file (no plugin code), exactly like `config/models.yml`:
//!
//! ```yaml
//! toolsets:
//!   toolset_1_empty: []
//!   toolset_2: [my_plugin_1__tool_1, my_plugin_2__tool_1]
//! ```
//!
//! - `GET /api/toolsets`: returns the parsed toolsets.yml content
//! - `PUT /api/toolsets`: validates + atomically writes toolsets.yml. Validation
//!   rejects a blank toolset id and blank tool names; an EMPTY list is valid and
//!   means "no tool allowed".

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;
use std::sync::Arc;

use super::{err_json, ok_json, AppState};
use crate::toolsets::{toolsets_path, ToolsetsFile};

pub async fn get_toolsets_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match ToolsetsFile::load_or_empty(&state.data_dir) {
        Ok(file) => ok_json(file),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("toolsets.yml: {}", e),
        ),
    }
}

pub async fn put_toolsets_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ToolsetsFile>,
) -> impl IntoResponse {
    // Validate BEFORE the atomic write: malformed input is rejected with a
    // clear message and toolsets.yml is left untouched.
    if let Err(e) = body.validate() {
        return err_json(StatusCode::BAD_REQUEST, &format!("toolsets.yml: {}", e));
    }
    if let Err(e) = body.save(&toolsets_path(&state.data_dir)) {
        return err_json(StatusCode::BAD_REQUEST, &format!("toolsets.yml: {}", e));
    }
    ok_json(json!({ "ok": true }))
}
