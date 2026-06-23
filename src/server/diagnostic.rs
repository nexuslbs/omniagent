//! Diagnostic endpoints for isolating the hang issue.

use axum::{
    extract::State,
};
use std::sync::Arc;
use std::time::Duration;
use super::AppState;

/// Test endpoint that uses State but returns immediately — no DB calls.
pub async fn check_state(
    State(_state): State<Arc<AppState>>,
) -> &'static str {
    "state ok"
}

/// Test DB pool — simple query with a short timeout.
pub async fn check_db(
    State(state): State<Arc<AppState>>,
) -> String {
    match tokio::time::timeout(
        Duration::from_secs(5),
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&state.pool),
    )
    .await
    {
        Ok(Ok(val)) => format!("db ok: {}", val),
        Ok(Err(e)) => format!("db error: {}", e),
        Err(_) => "db timeout after 5s".to_string(),
    }
}

/// Call plugin::list_plugins directly.
pub async fn check_list_plugins(
    State(state): State<Arc<AppState>>,
) -> String {
    match tokio::time::timeout(
        Duration::from_secs(5),
        crate::plugin::list_plugins(&state.pool),
    )
    .await
    {
        Ok(Ok(rows)) => format!("list_plugins ok: {} rows", rows.len()),
        Ok(Err(e)) => format!("list_plugins error: {}", e),
        Err(_) => "list_plugins timeout after 5s".to_string(),
    }
}

/// Test: enrich + json construction (isolating the hang)
pub async fn check_enrich_json(
    State(state): State<Arc<AppState>>,
) -> String {
    let t0 = std::time::Instant::now();
    let rows = match crate::plugin::list_plugins(&state.pool).await {
        Ok(r) => r,
        Err(e) => return format!("list error: {} ({}ms)", e, t0.elapsed().as_millis()),
    };
    let t1 = std::time::Instant::now();

    // Enrich row by row with per-row timing
    let mut details = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let rt = std::time::Instant::now();
        let detail = crate::plugin::enrich_plugin(row);
        let elapsed = rt.elapsed();
        if elapsed.as_millis() > 50 {
            return format!("HANG at row {}: enrich took {}ms", i, elapsed.as_millis());
        }
        details.push(detail);
    }
    let t2 = std::time::Instant::now();

    // JSON construction
    let _val = serde_json::json!({
        "success": true,
        "data": details
    });
    let t3 = std::time::Instant::now();

    format!(
        "list={}ms enrich={}ms json={}ms total={}ms rows={}",
        (t1 - t0).as_millis(),
        (t2 - t1).as_millis(),
        (t3 - t2).as_millis(),
        t0.elapsed().as_millis(),
        details.len(),
    )
}

/// Read .env file using spawn_blocking (safe pattern).
pub async fn check_env_read(
    State(state): State<Arc<AppState>>,
) -> String {
    let env_path = state.env_path.clone();
    match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            std::fs::read_to_string(&env_path)
        }),
    )
    .await
    {
        Ok(Ok(Ok(content))) => format!("env read ok: {} chars", content.len()),
        Ok(Ok(Err(e))) => format!("env read io error: {}", e),
        Ok(Err(e)) => format!("env read join error: {}", e),
        Err(_) => "env read timeout after 5s".to_string(),
    }
}
