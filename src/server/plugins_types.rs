//! Request/Response types for the plugin management API.
//!
//! Extracted from `plugins.rs` for separation of concerns.

use axum::{
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::plugins_yaml;

// ---------------------------------------------------------------------------
// Request/Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct UpdateConfigRequest {
    pub config: serde_json::Value,
}

#[derive(Deserialize)]
pub(crate) struct PluginSourceRequest {
    /// Source identifier: "built-in", "bundled", or "remote".
    /// Required. The handler acts on this exact source.
    pub source: Option<String>,
    /// Optional remote config to set when enabling a remote source.
    /// When source is "remote" and this is provided, the remote URL/path
    /// is written to the YAML entry. Required when re-enabling a remote
    /// source after it was previously cleared (by switching to built-in
    /// or bundled).
    #[serde(default)]
    pub remote: Option<plugins_yaml::PluginRemote>,
}

/// Validate that a source was provided. Returns an error response if missing.
pub(crate) fn require_source(source: &Option<String>) -> Result<&str, (StatusCode, Json<serde_json::Value>)> {
    match source.as_deref() {
        Some(s) => Ok(s),
        None => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": "Source is required. Provide a `source` parameter: 'built-in', 'bundled', or 'remote'."
            })),
        )),
    }
}

#[derive(Deserialize)]
pub(crate) struct InstallUrlRequest {
    pub url: String,
}

#[derive(Deserialize)]
pub(crate) struct InstallGitRequest {
    pub url: String,
    /// Optional git ref (branch, tag, or commit SHA). Defaults to repo HEAD.
    pub git_ref: Option<String>,
    /// Optional name override. If not provided, extracted from plugin.json.
    pub name: Option<String>,
    /// Optional subdirectory path within the repo where plugin.json lives.
    /// Example: "tools/test-rust-tool" if plugin.json is not at the repo root.
    pub path: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct RenameRequest {
    /// The new name for the plugin.
    pub new_name: String,
}
