//! Request/Response types for the plugin management API.
//!
//! Extracted from `plugins.rs` for separation of concerns.

use axum::{http::StatusCode, Json};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Request/Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct UpdateConfigRequest {
    pub config: serde_json::Value,
}

/// Validate a source string from the URL path.
/// Returns an error response if invalid.
pub(crate) fn validate_source(source: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    match source {
        "built-in" | "bundled" | "remote" => Ok(()),
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Invalid source '{}': must be 'built-in', 'bundled', or 'remote'", source)
            })),
        )),
    }
}

/// Validate a plugin type string from the URL path.
pub(crate) fn validate_plugin_type(p_type: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    match p_type {
        "tools" | "platforms" | "providers" => Ok(()),
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Invalid plugin type '{}': must be 'tools', 'platforms', or 'providers'", p_type)
            })),
        )),
    }
}

/// Return a 400 error for operations not allowed on built-in plugins.
pub(crate) fn reject_builtin_operation(source: &str, action: &str, name: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if source == "built-in" {
        Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Cannot {} built-in plugin '{}': only enable and disable are allowed", action, name)
            })),
        ))
    } else {
        Ok(())
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
