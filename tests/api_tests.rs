//! Integration tests for the OmniAgent REST API.
//!
//! These tests connect to a running server at http://localhost:8080
//! and verify that all GET endpoints return the expected responses.

use std::time::Duration;

/// Base URL for the running server.
const BASE: &str = "http://localhost:8080";

/// Helper: perform a GET request with a reasonable timeout.
fn get(path: &str) -> reqwest::blocking::Response {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("Failed to build HTTP client");
    client.get(format!("{}{}", BASE, path)).send().unwrap()
}

// ---------------------------------------------------------------------------
// /health
// ---------------------------------------------------------------------------

#[test]
fn test_health() {
    let resp = get("/health");
    assert_eq!(resp.status(), 200);
    let body = resp.text().unwrap();
    assert_eq!(body, "ok");
}

// ---------------------------------------------------------------------------
// /messages/filters
// ---------------------------------------------------------------------------

#[test]
fn test_messages_filters() {
    let resp = get("/messages/filters");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

// ---------------------------------------------------------------------------
// /messages/events  (missing query params → defaults used)
// ---------------------------------------------------------------------------

#[test]
fn test_messages_events_no_params() {
    let resp = get("/messages/events");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

// ---------------------------------------------------------------------------
// /threads
// ---------------------------------------------------------------------------

#[test]
fn test_threads() {
    let resp = get("/threads");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

// ---------------------------------------------------------------------------
// /threads/filters
// ---------------------------------------------------------------------------

#[test]
fn test_threads_filters() {
    let resp = get("/threads/filters");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

// ---------------------------------------------------------------------------
// /channels
// ---------------------------------------------------------------------------

#[test]
fn test_channels() {
    let resp = get("/channels");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

// ---------------------------------------------------------------------------
// /overview
// ---------------------------------------------------------------------------

#[test]
fn test_overview() {
    let resp = get("/overview");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

// ---------------------------------------------------------------------------
// /overview/dashboard
// ---------------------------------------------------------------------------

#[test]
fn test_overview_dashboard() {
    let resp = get("/overview/dashboard");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

// ---------------------------------------------------------------------------
// /memory/stats
// ---------------------------------------------------------------------------

#[test]
fn test_memory_stats() {
    let resp = get("/memory/stats");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

// ---------------------------------------------------------------------------
// /platforms
// ---------------------------------------------------------------------------

#[test]
fn test_platforms() {
    let resp = get("/platforms");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

// ---------------------------------------------------------------------------
// /kanban/tasks
// ---------------------------------------------------------------------------

#[test]
fn test_kanban_tasks() {
    let resp = get("/kanban/tasks");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

// ---------------------------------------------------------------------------
// /schedule
// ---------------------------------------------------------------------------

#[test]
fn test_schedule() {
    let resp = get("/schedule");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

// ---------------------------------------------------------------------------
// /actions  (returns bare JSON array: no {success, data} wrapper)
// ---------------------------------------------------------------------------

#[test]
fn test_actions() {
    let resp = get("/actions");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    // /actions returns a plain array (Vec<ActionResponse>)
    assert!(json.is_array(), "expected an array for /actions");
}

// ---------------------------------------------------------------------------
// Edge cases: missing query params on handlers that accept them
// ---------------------------------------------------------------------------

#[test]
fn test_messages_events_with_bogus_params() {
    let resp = get("/messages/events?bogus=1&limit=abc");
    // The server should either gracefully default or return 400 for bad params
    assert!(
        resp.status() == 200 || resp.status() == 400,
        "expected 200 or 400, got {}",
        resp.status()
    );
    if resp.status() == 200 {
        let json: serde_json::Value = resp.json().unwrap();
        assert!(json["success"].as_bool().unwrap_or(false));
    }
}

#[test]
fn test_threads_with_bogus_params() {
    let resp = get("/threads?bogus=1&limit=abc&status=invalid");
    assert!(
        resp.status() == 200 || resp.status() == 400,
        "expected 200 or 400, got {}",
        resp.status()
    );
    if resp.status() == 200 {
        let json: serde_json::Value = resp.json().unwrap();
        assert!(json["success"].as_bool().unwrap_or(false));
        assert!(json.get("data").is_some());
    }
}

#[test]
fn test_kanban_tasks_with_bogus_params() {
    let resp = get("/kanban/tasks?bogus=1&status=invalid&limit=abc");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

#[test]
fn test_schedule_with_bogus_params() {
    let resp = get("/schedule?bogus=1&active=maybe");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

#[test]
fn test_memory_stats_with_bogus_params() {
    let resp = get("/memory/stats?bogus=1&channel=notanumber&profile=");
    assert!(
        resp.status() == 200 || resp.status() == 400,
        "expected 200 or 400, got {}",
        resp.status()
    );
    if resp.status() == 200 {
        let json: serde_json::Value = resp.json().unwrap();
        assert!(json["success"].as_bool().unwrap_or(false));
        assert!(json.get("data").is_some());
    }
}

// ---------------------------------------------------------------------------
// 404 on unknown routes
// ---------------------------------------------------------------------------

#[test]
fn test_unknown_route_returns_404() {
    let resp = get("/nonexistent-route");
    assert_eq!(resp.status(), 404);
}
