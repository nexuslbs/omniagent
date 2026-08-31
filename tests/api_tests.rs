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
#[ignore]
fn test_health() {
    let resp = get("/health");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert_eq!(json["status"], "ok");
    assert!(json["version"].as_str().map_or(false, |v| !v.is_empty()));
    assert!(json["uptime"].as_u64().is_some());
}

// ---------------------------------------------------------------------------
// /messages/filters
// ---------------------------------------------------------------------------

#[test]
#[ignore]
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
#[ignore]
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
#[ignore]
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
#[ignore]
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
#[ignore]
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
#[ignore]
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
#[ignore]
fn test_overview_dashboard() {
    let resp = get("/overview/dashboard");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    let data = json.get("data").expect("data must be present");
    assert!(data.is_object(), "data must be an object");

    // Task gate 3: Token Trend must cover 14 days and each day must carry
    // the 3-series breakdown (input cache hit / cache miss / output) with
    // tokens == hit + miss + output (data consistency).
    let trend = data["token_trend"]
        .as_array()
        .expect("token_trend must be an array");
    assert_eq!(trend.len(), 14, "token_trend must cover exactly 14 days");
    for day in trend {
        let day_str = day["day"].as_str().unwrap_or_default();
        assert!(
            !day_str.is_empty(),
            "each trend entry must carry a real day, got {day}"
        );
        let hit = day["input_cache_hit"]
            .as_i64()
            .expect("input_cache_hit must be a number");
        let miss = day["input_cache_miss"]
            .as_i64()
            .expect("input_cache_miss must be a number");
        let out = day["output_tokens"]
            .as_i64()
            .expect("output_tokens must be a number");
        let total = day["tokens"].as_i64().expect("tokens must be a number");
        assert!(
            hit >= 0 && miss >= 0 && out >= 0,
            "breakdown fields must be non-negative, got {day}"
        );
        assert_eq!(
            total,
            hit + miss + out,
            "tokens must equal input_cache_hit + input_cache_miss + output_tokens, got {day}"
        );
    }

    // Task gate 1: Top Tools Used must list real tool names (no "unknown").
    let tools = data["top_tools"]
        .as_array()
        .expect("top_tools must be an array");
    for t in tools {
        let name = t["tool"].as_str().unwrap_or_default();
        let count = t["count"].as_i64().unwrap_or(0);
        assert!(!name.is_empty(), "tool name must not be empty, got {t}");
        assert!(
            !name.eq_ignore_ascii_case("unknown"),
            "tool name must not be 'unknown', got {t}"
        );
        assert!(count > 0, "tool count must be positive, got {t}");
    }

    // Task gate 2: Kanban Snapshot = last status-changed tasks, newest first,
    // each row carrying board / task_id / title / status / tags / changed_at.
    let snap = data["kanban_snapshot"]
        .as_array()
        .expect("kanban_snapshot must be an array");
    for s in snap {
        for field in ["board", "task_id", "title", "status", "changed_at"] {
            assert!(
                !s[field].as_str().unwrap_or_default().is_empty(),
                "snapshot entry must carry {field}, got {s}"
            );
        }
        assert!(s["tags"].is_array(), "tags must be an array, got {s}");
    }
    for pair in snap.windows(2) {
        assert!(
            pair[0]["changed_at"].as_str().unwrap_or_default()
                >= pair[1]["changed_at"].as_str().unwrap_or_default(),
            "kanban snapshot must be ordered newest-first, got {:?} then {:?}",
            pair[0]["changed_at"],
            pair[1]["changed_at"]
        );
    }
}

// ---------------------------------------------------------------------------
// /memory/stats
// ---------------------------------------------------------------------------

#[test]
#[ignore]
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
#[ignore]
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
#[ignore]
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
#[ignore]
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
#[ignore]
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
#[ignore]
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
#[ignore]
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
#[ignore]
fn test_kanban_tasks_with_bogus_params() {
    let resp = get("/kanban/tasks?bogus=1&status=invalid&limit=abc");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

#[test]
#[ignore]
fn test_schedule_with_bogus_params() {
    let resp = get("/schedule?bogus=1&active=maybe");
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().unwrap();
    assert!(json["success"].as_bool().unwrap_or(false));
    assert!(json.get("data").is_some());
}

#[test]
#[ignore]
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
#[ignore]
fn test_unknown_route_returns_404() {
    let resp = get("/nonexistent-route");
    assert_eq!(resp.status(), 404);
}

// ---------------------------------------------------------------------------
// /secrets multiline round-trip (multiline values must be stored verbatim)
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_secrets_multiline_roundtrip() {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("Failed to build HTTP client");

    // PEM-shaped multiline value (the GITHUB_APP_KEY use case).
    let name = format!("test-multiline-{}", std::process::id());
    let value = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFA\nline2-with-padding\n-----END PRIVATE KEY-----\n";

    // Create
    let resp = client
        .post(format!("{}/secrets", BASE))
        .json(&serde_json::json!({ "name": name, "fieldType": "password", "value": value }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "POST /secrets should succeed");
    let json: serde_json::Value = resp.json().unwrap();
    assert_eq!(
        json["data"]["current_value"].as_str(),
        Some(value),
        "multiline value must be stored verbatim after create"
    );

    // Read back
    let resp = client
        .get(format!("{}/secrets/{}", BASE, name))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "GET /secrets/{name} should succeed");
    let json: serde_json::Value = resp.json().unwrap();
    assert_eq!(
        json["data"]["current_value"].as_str(),
        Some(value),
        "multiline value must round-trip through GET"
    );

    // Update with another multiline value
    let value2 =
        "-----BEGIN PRIVATE KEY-----\nupdated-line-1\nupdated-line-2\n-----END PRIVATE KEY-----\n";
    let resp = client
        .put(format!("{}/secrets/{}", BASE, name))
        .json(&serde_json::json!({ "value": value2 }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "PUT /secrets/{name} should succeed");
    let json: serde_json::Value = resp.json().unwrap();
    assert_eq!(
        json["data"]["current_value"].as_str(),
        Some(value2),
        "multiline value must be stored verbatim after update"
    );

    // Cleanup
    client
        .delete(format!("{}/secrets/{}", BASE, name))
        .send()
        .unwrap();
}

// ---------------------------------------------------------------------------
// /kanban tags + dependency history: every tag add/remove and dependency
// add/remove must produce a durable kanban_history entry.
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_kanban_tags_and_dependency_history() {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("Failed to build HTTP client");

    let suffix = std::process::id();
    // 1. Create a task (optionally pre-tagged).
    let resp = client
        .post(format!("{}/kanban/tasks", BASE))
        .json(&serde_json::json!({
            "title": format!("tags-roundtrip-{}", suffix),
            "board": "plain",
            "tags": ["v1.0.0", "dashboard"],
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "POST /kanban/tasks should succeed");
    let json: serde_json::Value = resp.json().unwrap();
    let task_id = json["data"]["id"].as_str().expect("task id").to_string();

    // 2. Task detail carries the initial tags.
    let resp = client
        .get(format!("{}/kanban/tasks/{}", BASE, task_id))
        .send()
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "GET /kanban/tasks/{{id}} should succeed"
    );
    let json: serde_json::Value = resp.json().unwrap();
    let tags = json["data"]["tags"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect::<Vec<_>>()
    });
    assert!(
        tags.as_ref().is_some_and(|t| {
            t.contains(&"v1.0.0".to_string()) && t.contains(&"dashboard".to_string())
        }),
        "created task must carry its initial tags, got {:?}",
        tags
    );

    // 3. Add a tag via the dedicated endpoint.
    let resp = client
        .post(format!("{}/kanban/tasks/{}/tags", BASE, task_id))
        .json(&serde_json::json!({ "tag": "infra" }))
        .send()
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "POST /kanban/tasks/{{id}}/tags should succeed"
    );

    // 4. Remove a tag.
    let resp = client
        .delete(format!("{}/kanban/tasks/{}/tags/infra", BASE, task_id))
        .send()
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "DELETE /kanban/tasks/{{id}}/tags/infra should succeed"
    );

    // 5. Dependencies: add + remove.
    let resp = client
        .post(format!("{}/kanban/tasks", BASE))
        .json(&serde_json::json!({ "title": format!("tags-roundtrip-dep-{}", suffix), "board": "plain" }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "POST dep task should succeed");
    let json: serde_json::Value = resp.json().unwrap();
    let dep_id = json["data"]["id"]
        .as_str()
        .expect("dep task id")
        .to_string();

    let resp = client
        .post(format!("{}/kanban/tasks/{}/dependencies", BASE, task_id))
        .json(&serde_json::json!({ "depends_on_id": dep_id }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "POST dependency should succeed");

    let resp = client
        .delete(format!(
            "{}/kanban/tasks/{}/dependencies/{}",
            BASE, task_id, dep_id
        ))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "DELETE dependency should succeed");

    // 6. History must contain entries for every operation.
    let resp = client
        .get(format!("{}/kanban/tasks/{}/history", BASE, task_id))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "GET history should succeed");
    let json: serde_json::Value = resp.json().unwrap();
    let actions: Vec<String> = json["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v["action"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    for expected in [
        "created",
        "tag_added",
        "tag_added",
        "tag_removed",
        "dependency_added",
        "dependency_removed",
    ] {
        assert!(
            actions.contains(&expected.to_string()),
            "history must contain '{expected}', got {actions:?}"
        );
    }

    // 7. Cleanup.
    client
        .delete(format!("{}/kanban/tasks/{}", BASE, task_id))
        .send()
        .unwrap();
    client
        .delete(format!("{}/kanban/tasks/{}", BASE, dep_id))
        .send()
        .unwrap();
}
