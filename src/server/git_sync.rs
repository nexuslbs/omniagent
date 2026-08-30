//! POST /git/sync: the canonical git sync entrypoint.
//!
//! Executes the configurable sync tool (settings `git_sync_tool`, default
//! `git_sync` from the builtin git plugin) via the MCP registry. This is the
//! SAME call the dashboard explorer sync button (bottom of the left panel)
//! and the toolbox backup/restore hooks use, so the whole stack shares one
//! sync implementation and one token-recovery path.

use axum::{extract::State, http::StatusCode, response::IntoResponse};
use std::sync::Arc;

use super::{err_json, ok_json, AppState};
use crate::mcp::McpToolCall;

/// Fallback tool name when the `git_sync_tool` setting is unset or empty.
pub(crate) fn fallback_sync_tool_name(raw: Option<&str>) -> String {
    match raw {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => "git_sync".to_string(),
    }
}

/// Resolve the configured git sync tool name (settings `git_sync_tool`,
/// default `git_sync`), resolving `$env:`/`$secret:` refs against the DB.
pub(crate) async fn resolve_sync_tool_name(data_dir: &str, pool: &sqlx::PgPool) -> String {
    let raw = crate::server::settings::load_settings_file(data_dir)
        .get("git_sync_tool")
        .cloned();
    let value = match raw {
        Some(v) => crate::server::settings::resolve_setting_value(&v, pool).await,
        None => "git_sync".to_string(),
    };
    fallback_sync_tool_name(Some(&value))
}

/// POST /git/sync: run the configured sync tool (fetch/pull --rebase/push).
///
/// The tool runs with empty arguments: `git_sync` defaults to the omni_dir
/// config repo (the same repo the explorer syncs). Token regeneration on
/// expired/revoked tokens happens inside the git plugin itself, so a 401
/// mid-sync is retried transparently and the caller sees a 200.
pub(crate) async fn sync_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let tool_name = resolve_sync_tool_name(&state.data_dir, &state.pool).await;
    let registry = state.plugin_manager.snapshot_registry().await;
    let available: Vec<String> = registry.all().iter().map(|t| t.name.clone()).collect();

    if !available.iter().any(|n| n == &tool_name) {
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(
                "Sync tool '{}' not found in the tool registry (available: {}). \
                 Is the git plugin enabled? The sync tool name is configurable via the \
                 'git_sync_tool' setting.",
                tool_name,
                available.join(", ")
            ),
        );
    }

    let mcp_call = McpToolCall {
        id: "git-sync".to_string(),
        name: tool_name,
        arguments: serde_json::json!({}),
    };

    match registry.execute(&mcp_call, state.app_context.clone()).await {
        Ok(r) if !r.is_error => ok_json(serde_json::json!({
            "success": true,
            "tool": mcp_call.name,
            "output": r.content,
        })),
        Ok(r) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Sync failed: {}", r.content),
        ),
        Err(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Sync failed: {}", e),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_defaults_to_git_sync() {
        assert_eq!(fallback_sync_tool_name(None), "git_sync");
        assert_eq!(fallback_sync_tool_name(Some("")), "git_sync");
        assert_eq!(fallback_sync_tool_name(Some("   ")), "git_sync");
    }

    #[test]
    fn fallback_keeps_configured_tool() {
        assert_eq!(fallback_sync_tool_name(Some("git_sync")), "git_sync");
        assert_eq!(
            fallback_sync_tool_name(Some("my_plugin_sync")),
            "my_plugin_sync"
        );
        assert_eq!(
            fallback_sync_tool_name(Some("  custom_sync  ")),
            "custom_sync"
        );
    }
}
