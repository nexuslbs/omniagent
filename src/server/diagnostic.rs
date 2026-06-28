//! Diagnostic endpoints for isolating the hang issue.

use super::AppState;
use axum::extract::State;
use std::sync::Arc;
use std::time::Duration;
use sql_forge::sql_forge;
use sqlx::FromRow;

#[derive(FromRow)]
struct OneRow {
    id: Option<i32>,
}

/// Test endpoint that uses State but returns immediately — no DB calls.
pub async fn check_state(State(_state): State<Arc<AppState>>) -> &'static str {
    "state ok"
}

/// Test DB pool — simple query with a short timeout.
pub async fn check_db(State(state): State<Arc<AppState>>) -> String {
    match tokio::time::timeout(
        Duration::from_secs(5),
        sql_forge!(
            OneRow,
            "SELECT 1 AS id"
        )
        .fetch_one(&state.pool),
    )
    .await
    {
        Ok(Ok(val)) => format!("db ok: {}", val.id.unwrap_or(0)),
        Ok(Err(e)) => format!("db error: {}", e),
        Err(_) => "db timeout after 5s".to_string(),
    }
}

/// Call plugins_yaml::list_plugins directly (YAML-based, no DB).
pub async fn check_list_plugins(State(state): State<Arc<AppState>>) -> String {
    match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || crate::plugins_yaml::list_plugins(&state.data_dir)),
    )
    .await
    {
        Ok(Ok(Ok(details))) => format!("list_plugins ok: {} details", details.len()),
        Ok(Ok(Err(e))) => format!("list_plugins error: {}", e),
        Ok(Err(_)) => "list_plugins join error".to_string(),
        Err(_) => "list_plugins timeout after 5s".to_string(),
    }
}

/// Test: enrich + json construction (isolating the hang)
pub async fn check_enrich_json(State(state): State<Arc<AppState>>) -> String {
    let t0 = std::time::Instant::now();
    let details = match crate::plugins_yaml::list_plugins(&state.data_dir) {
        Ok(d) => d,
        Err(e) => return format!("list error: {} ({}ms)", e, t0.elapsed().as_millis()),
    };
    let t1 = std::time::Instant::now();

    // Serialize to json with per-item timing
    for (i, detail) in details.iter().enumerate() {
        let rt = std::time::Instant::now();
        let _json = serde_json::to_value(detail);
        let elapsed = rt.elapsed();
        if elapsed.as_millis() > 50 {
            return format!(
                "HANG at item {}: serialize took {}ms",
                i,
                elapsed.as_millis()
            );
        }
    }

    format!(
        "enrich+json ok: {} details, list={}ms, total={}ms",
        details.len(),
        t1.duration_since(t0).as_millis(),
        t0.elapsed().as_millis()
    )
}

/// Check environment variables (for debugging env resolution).
pub async fn check_env_read(State(state): State<Arc<AppState>>) -> String {
    let vars = [
        "OMNI_DATA_DIR",
        "WORKSPACE_DIR",
        "LLM_PROVIDER",
        "LLM_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "DEEPSEEK_API_KEY",
        "OPENCODE_GO_API_KEY",
        "HOST",
        "PORT",
    ];
    let mut result = String::new();
    for var in &vars {
        let val = std::env::var(var).unwrap_or_else(|_| "(not set)".to_string());
        result.push_str(&format!("{}={}\n", var, val));
    }
    result.push_str(&format!("env_path={}\n", state.env_path));
    result
}
