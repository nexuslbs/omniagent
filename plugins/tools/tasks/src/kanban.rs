#![allow(dead_code, unused_imports)]
//! mcp-server-kanban: standalone MCP server for kanban task management.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: create_kanban_task, list_kanban_tasks, update_kanban_task,
//!        delete_kanban_task, add_kanban_dependency, remove_kanban_dependency

use anyhow::{anyhow, Result};
use mcp_server_util::*;
use omniagent::agent::{manual_review_decision, validate_review_decision};
use omniagent::db;
use serde_json::Value;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Thin HTTP callers of the core kanban API - task CRUD/status/deps live in the
// omniagent server; this plugin only talks to it over HTTP. The DB is core's
// business: no SQL is issued from this plugin (kanban_review_task keeps using
// the omniagent library for decision validation/history).
// ---------------------------------------------------------------------------

/// Core server base URL (configure message `base_url`; default localhost:8080).
static BASE_URL: OnceLock<String> = OnceLock::new();

/// Set the core API base URL (from the plugin configure message).
pub fn set_base_url(url: String) {
    let _ = BASE_URL.set(url);
}

fn api_url(path: &str) -> String {
    let base = BASE_URL
        .get()
        .map(String::as_str)
        .unwrap_or("http://localhost:8080");
    format!("{base}{path}")
}

async fn api_call(method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value> {
    let client = reqwest::Client::new();
    let mut req = client.request(method, api_url(path));
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow!("Kanban API error: {e}"))?;
    let status = resp.status();
    let json: Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("Invalid kanban API response: {e}"))?;
    if !status.is_success() || json.get("success").and_then(|s| s.as_bool()) == Some(false) {
        let msg = json["error"].as_str().unwrap_or("operation failed");
        anyhow::bail!("Kanban API error: {msg}");
    }
    Ok(json)
}

/// Percent-encode a query-string value (RFC 3986 unreserved set kept).
fn encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Forward an optional `board` argument into a request body (pure + unit-tested).
///
/// Semantics: the value is forwarded VERBATIM, and only when it is a non-empty
/// string. Absent / `null` / `""` / whitespace all mean "the caller did not
/// choose a board": the field is omitted so the core API applies its own rule
/// (it REQUIRES the board while `boards.yml` is present and rejects an explicit
/// clear) and its error surfaces verbatim through `api_call`. The tool never
/// invents a default board - a silent default would hide the operator's board
/// choice.
fn forward_board(req: &mut Value, args: &Value) {
    if let Some(board) = args.get("board").and_then(|b| b.as_str()) {
        if !board.trim().is_empty() {
            (*req)["board"] = serde_json::json!(board);
        }
    }
}

/// The `&board=` query suffix for `GET /kanban/tasks` (empty = no filter).
fn board_query_suffix(board: &str) -> String {
    let board = board.trim();
    if board.is_empty() {
        String::new()
    } else {
        format!("&board={}", encode_query_value(board))
    }
}

/// Caller-supplied channel/profile for a NEW task.
///
/// The calling thread's context is deliberately NOT consulted (operator
/// 2026-09-27): an omitted channel/profile must stay empty so the task stores
/// NULL and the BOARD (boards.yml) supplies the effective values at dispatch
/// (`resolve_task_defaults`). Inheriting the caller's channel/profile SHADOWED
/// the board fallback - a task created by the main agent from a telegram
/// thread was stored with `channel_id='telegram'` + `profile='omni'`, and the
/// board's own channel/profile were then ignored. An explicitly requested
/// channel/profile still wins (it is forwarded verbatim).
fn create_context_fields(args: &Value) -> (&str, &str) {
    let channel = args["channel_id"]
        .as_str()
        .or_else(|| args["channel"].as_str())
        .unwrap_or("");
    let profile = args["profile"].as_str().unwrap_or("");
    (channel, profile)
}

async fn handle_create(
    _pool: &PgPool,
    args: &Value,
    _meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let title = args["title"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing required argument: 'title'"))?;
    if title.is_empty() {
        anyhow::bail!("Task title must not be empty");
    }
    let mut req = serde_json::json!({
        "title": title,
        "body": args["body"].as_str().unwrap_or(""),
        "status": args["status"].as_str().unwrap_or("todo"),
        "priority": args["priority"].as_i64().unwrap_or(0),
        "assignee": args["assignee"].as_str().unwrap_or(""),
        "template": args["template"].as_str().unwrap_or(""),
        // Server-side field is `workflow`; accept `workflow_id` as legacy alias.
        "workflow": args["workflow"]
            .as_str()
            .or_else(|| args["workflow_id"].as_str())
            .unwrap_or(""),
    });
    // Optional `board` (POST /kanban/tasks `board`): see `forward_board`.
    forward_board(&mut req, args);
    // Optional `toolset` / `plan` / `tags`: same fields the HTTP API accepts.
    if let Some(toolset) = args["toolset"].as_str() {
        req["toolset"] = serde_json::json!(toolset);
    }
    if let Some(plan) = args["plan"].as_bool() {
        req["plan"] = serde_json::json!(plan);
    }
    if let Some(tags) = args["tags"].as_array() {
        req["tags"] = serde_json::json!(tags);
    }
    // Explicit channel/profile win; omitted stays EMPTY (NULL in the task) so
    // the board supplies them. See `create_context_fields`.
    let (channel, profile) = create_context_fields(args);
    // The HTTP API field is `channel` (there is no `channel_id` field), so the
    // old name silently dropped the value.
    req["channel"] = serde_json::json!(channel);
    req["profile"] = serde_json::json!(profile);
    let resp = api_call(reqwest::Method::POST, "/kanban/tasks", Some(&req)).await?;
    let id = resp["data"]["id"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing 'id' in create response"))?;
    let status = args["status"].as_str().unwrap_or("todo");
    Ok((
        format!("Kanban task '{title}' created with id '{id}' and status '{status}'"),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: list_kanban_tasks
// ---------------------------------------------------------------------------

async fn handle_list(_pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let status_filter = args["status"].as_str().unwrap_or("");
    let board_filter = args["board"].as_str().unwrap_or("");
    let show_archived = args["show_archived"].as_bool().unwrap_or(true);
    let mut path = format!(
        "/kanban/tasks?show_archived={}",
        if show_archived { "true" } else { "false" }
    );
    path.push_str(&board_query_suffix(board_filter));
    let resp = api_call(reqwest::Method::GET, &path, None).await?;
    let tasks = resp["data"]
        .as_array()
        .ok_or_else(|| anyhow!("Unexpected list response from kanban API"))?;
    let tasks: Vec<&Value> = tasks
        .iter()
        // `status` used to be a declared-but-dead argument (the API has no
        // status filter); apply it here instead of silently ignoring it.
        .filter(|t| status_filter.is_empty() || t["status"].as_str() == Some(status_filter))
        .collect();
    if tasks.is_empty() {
        return Ok(("_No kanban tasks found._".to_string(), false));
    }
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for t in tasks {
        groups
            .entry(t["status"].as_str().unwrap_or("unknown").to_string())
            .or_default()
            .push(t);
    }
    let mut lines = vec!["**Kanban Tasks:**".to_string()];
    for (status, items) in &groups {
        lines.push(format!("\n**{}** ({} tasks):", status, items.len()));
        for (i, t) in items.iter().enumerate() {
            let title = t["title"].as_str().unwrap_or("(untitled)");
            let id = t["id"].as_str().unwrap_or("");
            let priority = t["priority"].as_i64().unwrap_or(0);
            let priority_label = match priority {
                5 => "\u{1f534} Critical",
                3 => "\u{1f7e0} High",
                1 => "\u{1f7e1} Medium",
                _ => "\u{26aa} Low",
            };
            let assignee = t["assignee"].as_str().unwrap_or("");
            let created = t["created_at"].as_str().unwrap_or("");
            let board = t["board"].as_str().unwrap_or("");
            lines.push(format!(
                "  {}. **{}** (`{}`)\n     - Board: {} | Priority: {} | Assignee: {} | Created: {}",
                i + 1,
                title,
                id,
                board,
                priority_label,
                assignee,
                created
            ));
        }
    }
    Ok((lines.join("\n"), false))
}

// ---------------------------------------------------------------------------
// Tool: update_kanban_task
// ---------------------------------------------------------------------------

async fn handle_update(
    _pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let id = args["id"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing required argument: 'id'"))?;
    let mut req = serde_json::json!({});
    for field in [
        "title", "body", "status", "priority", "assignee", "profile", "archived", "template",
        "toolset", "plan",
    ] {
        if let Some(v) = args.get(field) {
            req[field] = v.clone();
        }
    }
    // Channel: the HTTP API field is `channel`; `channel_id` remains accepted as
    // a legacy alias on input.
    if let Some(v) = args.get("channel").or_else(|| args.get("channel_id")) {
        req["channel"] = v.clone();
    }
    // Board: PATCH accepts `board` and MOVES the task between boards. Empty or
    // null means "unchanged" here, so the field is omitted rather than sent as
    // "" (the server rejects an explicit clear: boards are always enabled).
    forward_board(&mut req, args);
    // Server-side field is `workflow`; accept `workflow_id` as legacy alias.
    // Explicit empty string clears the workflow (board default applies).
    let wf = args.get("workflow").or_else(|| args.get("workflow_id"));
    if let Some(wf) = wf {
        req["workflow"] = wf.clone();
    }
    // Forward the calling thread id so the core can (a) reject a premature
    // self-close to done from the task's own still-processing serving thread
    // and (b) never stale-skip the closer thread performing this status change.
    if let Some(tid) = meta.and_then(|m| m.thread_id) {
        req["source_thread_id"] = serde_json::json!(tid);
    }
    let _resp = api_call(
        reqwest::Method::PATCH,
        &format!("/kanban/tasks/{id}"),
        Some(&req),
    )
    .await?;
    Ok((format!("Kanban task '{id}' updated successfully"), false))
}

// ---------------------------------------------------------------------------
// Tool: delete_kanban_task
// ---------------------------------------------------------------------------

async fn handle_delete(_pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let id = args["id"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing required argument: 'id'"))?;
    let _resp = api_call(
        reqwest::Method::DELETE,
        &format!("/kanban/tasks/{id}"),
        None,
    )
    .await?;
    Ok((format!("Kanban task '{id}' deleted successfully"), false))
}

// ---------------------------------------------------------------------------
// Tool: add_kanban_dependency
// ---------------------------------------------------------------------------

async fn handle_add_dependency(_pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let task_id = args["task_id"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing required argument: 'task_id'"))?;
    let depends_on_id = args["depends_on_id"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing required argument: 'depends_on_id'"))?;
    let body = serde_json::json!({ "depends_on_id": depends_on_id });
    let _resp = api_call(
        reqwest::Method::POST,
        &format!("/kanban/tasks/{task_id}/dependencies"),
        Some(&body),
    )
    .await?;
    Ok((
        format!("Dependency added: '{depends_on_id}' -> '{task_id}'"),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: remove_kanban_dependency
// ---------------------------------------------------------------------------

async fn handle_remove_dependency(_pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let task_id = args["task_id"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing required argument: 'task_id'"))?;
    let depends_on_id = args["depends_on_id"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing required argument: 'depends_on_id'"))?;
    let _resp = api_call(
        reqwest::Method::DELETE,
        &format!("/kanban/tasks/{task_id}/dependencies/{depends_on_id}"),
        None,
    )
    .await?;
    Ok((
        format!("Dependency removed: '{depends_on_id}' no longer blocks '{task_id}'"),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Plugin config hook
// ---------------------------------------------------------------------------

/// Callback invoked when the host sends configuration via configure message.
/// Plugin config - received via configure message.
#[derive(Debug, Clone)]
struct PluginConfig {
    pub database_url: String,
    pub base_url: String,
}

impl PluginConfig {
    fn from_json(v: &serde_json::Value) -> Self {
        Self {
            database_url: v
                .get("database_url")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    eprintln!("FATAL: database_url not in configure message");
                    std::process::exit(1);
                }),
            base_url: v
                .get("base_url")
                .and_then(|v| v.as_str())
                .unwrap_or("http://localhost:8080")
                .to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: kanban_review_task (MANUAL/API only - spec §8 R12)
// ---------------------------------------------------------------------------

/// Manual/API-only review decision with the same validation as POST /review:
/// decision whitelist (approve | rework | retest | block) + R5 target
/// validation + retry guards - all enforced by
/// `omniagent::agent::manual_review_decision`.
async fn handle_review(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let task_id = args["task_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'task_id'"))?;
    let decision = args["decision"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'decision'"))?;
    let comment = args["comment"].as_str();

    validate_review_decision(decision).map_err(|e| anyhow::anyhow!("{e}"))?;

    let data_dir = std::env::var("OMNI_DIR").unwrap_or_else(|_| "/opt/omni".to_string());

    match manual_review_decision(pool, &data_dir, task_id, decision, comment).await {
        Ok(outcome) => Ok((
            serde_json::json!({
                "success": true,
                "task_id": outcome.task_id,
                "status": outcome.status,
                "thread_id": outcome.thread_id,
                "comment": outcome.comment,
            })
            .to_string(),
            false,
        )),
        Err(e) => Ok((
            serde_json::json!({ "success": false, "error": e }).to_string(),
            true,
        )),
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

pub fn build_tools(pool: &Arc<RwLock<Option<PgPool>>>) -> Vec<McpToolEntry> {
    omniagent::channels_yaml::set_data_dir(&crate::data_dir());

    let p_create = pool.clone();
    let create_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_create.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_create(&pool, &args, meta.as_ref()).await
        })
    });

    let p_list = pool.clone();
    let list_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_list.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_list(&pool, &args).await
        })
    });

    let p_update = pool.clone();
    let update_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_update.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_update(&pool, &args, meta.as_ref()).await
        })
    });

    let p_delete = pool.clone();
    let delete_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_delete.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_delete(&pool, &args).await
        })
    });

    let p_add_dep = pool.clone();
    let add_dep_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_add_dep.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_add_dependency(&pool, &args).await
        })
    });

    let p_rm_dep = pool.clone();
    let rm_dep_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_rm_dep.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_remove_dependency(&pool, &args).await
        })
    });

    let p_review = pool.clone();
    let review_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_review.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_review(&pool, &args).await
        })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "create_kanban_task".to_string(),
                description:
                    "Create a new kanban task. Adds a task to the kanban board with optional body, status, priority, and assignee. Boards are ALWAYS enabled, so the target `board` is REQUIRED and must name an existing board - the API error is returned verbatim (no silent default board)."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Task title"
                        },
                        "body": {
                            "type": "string",
                            "description": "Optional task description/body"
                        },
                        "status": {
                            "type": "string",
                            "description": "Optional status (default: 'todo'). One of: backlog, todo, running, testing, review, blocked, done",
                            "enum": ["backlog", "todo", "running", "testing", "review", "blocked", "done"]
                        },
                        "priority": {
                            "type": "integer",
                            "description": "Optional priority (default: 0). 0=Low, 1=Med, 3=High, 5=Critical"
                        },
                        "assignee": {
                            "type": "string",
                            "description": "Optional assignee name"
                        },
                        "channel_id": { "type": "string", "description": "Optional explicit channel name for thread/cause creation. Leave unset (recommended) so the BOARD's channel applies (boards.yml fallback); the calling thread's channel is never inherited. Legacy alias of `channel`" },
                        "channel": { "type": "string", "description": "Optional explicit channel name for thread/cause creation. Leave unset (recommended) so the BOARD's channel applies (boards.yml fallback); the calling thread's channel is never inherited." },
                        "profile": {
                            "type": "string",
                            "description": "Optional explicit profile name for the task. Leave unset (recommended) so the BOARD's profile applies (boards.yml fallback); the calling thread's profile is never inherited."
                        },
                        "template": {
                            "type": "string",
                            "description": "Optional template file name (without .md) to use for execution context"
                        },
                        "workflow": {
                            "type": "string",
                            "description": "Optional workflow key (e.g. exec-test-review) this task belongs to; empty/null means the board default"
                        },
                        "workflow_id": {
                            "type": "string",
                            "description": "Deprecated alias for workflow (accepted for backward compatibility)"
                        },
                        "board": {
                            "type": "string",
                            "description": "Target board name. REQUIRED (boards are ALWAYS enabled) and must name an existing board; the API error is returned verbatim. When boards.yml is missing the built-in default board set (main) applies."
                        },
                        "toolset": {
                            "type": "string",
                            "description": "Optional toolset name for the task's threads"
                        },
                        "plan": {
                            "type": "boolean",
                            "description": "Optional: whether the task runs in plan mode (defaults to the board's plan setting)"
                        },
                        "tags": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional list of tags to attach to the new task"
                        }
                    },
                    "required": ["title"]
                }),
            },
            handler: create_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "list_kanban_tasks".to_string(),
                description: "List kanban tasks grouped by status.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "description": "Optional status filter. One of: backlog, todo, running, testing, review, blocked, done"
                        },
                        "board": {
                            "type": "string",
                            "description": "Optional board filter: when set, only tasks of that board are returned (maps to GET /kanban/tasks?board=)"
                        },
                        "show_archived": {
                            "type": "boolean",
                            "description": "Include archived tasks (default: true)"
                        }
                    }
                }),
            },
            handler: list_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "update_kanban_task".to_string(),
                description: "Update an existing kanban task. Only provided fields are updated. Status changes are recorded in history; `board` moves the task between boards.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Task ID to update"
                        },
                        "title": {
                            "type": "string",
                            "description": "New title"
                        },
                        "body": {
                            "type": "string",
                            "description": "New body/description"
                        },
                        "status": {
                            "type": "string",
                            "description": "New status. One of: backlog, todo, running, testing, review, blocked, done",
                            "enum": ["backlog", "todo", "running", "testing", "review", "blocked", "done"]
                        },
                        "priority": {
                            "type": "integer",
                            "description": "New priority. 0=Low, 1=Med, 3=High, 5=Critical"
                        },
                        "assignee": {
                            "type": "string",
                            "description": "New assignee"
                        },
                        "channel_id": { "type": "string", "description": "New channel name, or empty string to clear it so the BOARD's channel applies (boards.yml fallback). Legacy alias of `channel`" },
                        "channel": { "type": "string", "description": "New channel name, or empty string to clear it so the BOARD's channel applies (boards.yml fallback)" },
                        "template": { "type": "string", "description": "New template file name (without .md), or empty string to clear it" },
                        "toolset": { "type": "string", "description": "New toolset name, or empty string to clear it" },
                        "plan": { "type": "boolean", "description": "New plan-mode flag" },
                        "board": { "type": "string", "description": "Move the task to another board. Only sent when non-empty; empty/null means unchanged (the API rejects an explicit clear: boards are always enabled)" },
                        "profile": {
                            "type": "string",
                            "description": "New profile name, or empty string to clear it so the BOARD's profile applies (boards.yml fallback)"
                        },
                        "archived": {
                            "type": "boolean",
                            "description": "Set to true to archive, false to unarchive"
                        },
                        "workflow": {
                            "type": "string",
                            "description": "Set the task workflow key (e.g. exec-test-review); empty string or null clears it so the board default applies"
                        },
                        "workflow_id": {
                            "type": "string",
                            "description": "Deprecated alias for workflow (accepted for backward compatibility)"
                        }
                    },
                    "required": ["id"]
                }),
            },
            handler: update_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "delete_kanban_task".to_string(),
                description: "Delete a kanban task. The deletion is recorded in history.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Task ID to delete"
                        }
                    },
                    "required": ["id"]
                }),
            },
            handler: delete_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "add_kanban_dependency".to_string(),
                description: "Add a dependency between two kanban tasks.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "The task that depends on another"
                        },
                        "depends_on_id": {
                            "type": "string",
                            "description": "The task that must be completed first"
                        }
                    },
                    "required": ["task_id", "depends_on_id"]
                }),
            },
            handler: add_dep_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "remove_kanban_dependency".to_string(),
                description: "Remove a dependency between two kanban tasks.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "The task that depends on another"
                        },
                        "depends_on_id": {
                            "type": "string",
                            "description": "The task that must be completed first"
                        }
                    },
                    "required": ["task_id", "depends_on_id"]
                }),
            },
            handler: rm_dep_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "kanban_review_task".to_string(),
                description:
                    "MANUAL/API-only review decision for a kanban task. Decision: approve (task done), rework (back to running with a new executor thread), retest (back to testing with a new tester thread), block (task blocked). Invalid decisions and invalid targets (e.g. retest without a tester role in the workflow) are rejected. The reviewer AGENT never calls this tool - it signals approve via normal completion and issues via fail-thread."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Task ID to review"
                        },
                        "decision": {
                            "type": "string",
                            "description": "Review decision. One of: approve, rework, retest, block",
                            "enum": ["approve", "rework", "retest", "block"]
                        },
                        "comment": {
                            "type": "string",
                            "description": "Optional comment recorded in the task history"
                        }
                    },
                    "required": ["task_id", "decision"]
                }),
            },
            handler: review_handler,
        },
    ];

    // Start the MCP server
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(encode_query_value("plain-board_1.0"), "plain-board_1.0");
        assert_eq!(encode_query_value("v0.3.0 beta"), "v0.3.0%20beta");
        assert_eq!(encode_query_value("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn review_decision_whitelist_matches_server() {
        // Same whitelist as POST /review (spec §8 R12).
        for d in ["approve", "rework", "retest", "block"] {
            assert!(validate_review_decision(d).is_ok(), "'{d}' should be valid");
        }
        for d in ["", "approved", "reject", "REWORK", "done"] {
            assert!(
                validate_review_decision(d).is_err(),
                "'{d}' should be invalid"
            );
        }
    }

    /// The tool SCHEMAS must expose `board` on create/update/list - the exact
    /// drift the operator hit (schema without `board` while the API requires it).
    #[test]
    fn kanban_schemas_expose_board() {
        let pool: Arc<RwLock<Option<PgPool>>> = Arc::new(RwLock::new(None));
        let tools = build_tools(&pool);
        for name in [
            "create_kanban_task",
            "update_kanban_task",
            "list_kanban_tasks",
        ] {
            let tool = tools
                .iter()
                .find(|t| t.def.name == name)
                .unwrap_or_else(|| panic!("tool {name} missing"));
            let board = &tool.def.input_schema["properties"]["board"];
            assert_eq!(
                board["type"], "string",
                "{name} must expose a string board parameter: {board:?}"
            );
        }
        let create = tools
            .iter()
            .find(|t| t.def.name == "create_kanban_task")
            .expect("create_kanban_task present");
        assert_eq!(
            create.def.input_schema["required"],
            serde_json::json!(["title"]),
            "title stays the ONLY required parameter: board must be forwarded, never forced client-side"
        );
    }

    /// create: `board` is forwarded verbatim; absent/empty/null is NOT sent, so
    /// the API's "board is required" error surfaces.
    #[test]
    fn create_forwards_board_verbatim_and_only_when_present() {
        let mut req = serde_json::json!({});
        forward_board(&mut req, &serde_json::json!({"board": "omnidev"}));
        assert_eq!(req["board"], "omnidev");

        for args in [
            serde_json::json!({}),
            serde_json::json!({"board": null}),
            serde_json::json!({"board": ""}),
            serde_json::json!({"board": "   "}),
        ] {
            let mut req = serde_json::json!({});
            forward_board(&mut req, &args);
            assert!(
                req.get("board").is_none(),
                "board must not be invented or sent for {args}: {req}"
            );
        }
    }

    /// update: a board MOVES the task; empty means "unchanged" (the API rejects
    /// an explicit clear: boards are always enabled).
    #[test]
    fn update_forwards_board_and_empty_means_unchanged() {
        let mut req = serde_json::json!({});
        forward_board(&mut req, &serde_json::json!({"board": "omnidev"}));
        assert_eq!(req["board"], "omnidev");

        let mut req = serde_json::json!({});
        forward_board(&mut req, &serde_json::json!({"board": ""}));
        assert!(
            req.get("board").is_none(),
            "empty board = unchanged, not an explicit clear"
        );
    }

    /// list: the `&board=` filter is applied only when set and is percent-encoded.
    #[test]
    fn list_path_is_board_scoped_and_percent_encoded() {
        assert_eq!(board_query_suffix(""), "");
        assert_eq!(board_query_suffix("   "), "");
        assert_eq!(board_query_suffix("omnidev"), "&board=omnidev");
        assert_eq!(board_query_suffix(" v0.3.0 beta "), "&board=v0.3.0%20beta");
    }

    /// create: the calling thread's channel/profile are NEVER inherited. An
    /// omitted channel/profile stays EMPTY (the task stores NULL and the board
    /// supplies the effective values), while an explicit value is forwarded
    /// verbatim. `create_context_fields` takes no caller-context input at all,
    /// so a telegram/omni caller cannot leak into the created task.
    #[test]
    fn create_never_inherits_caller_context() {
        let empty = serde_json::json!({});
        let (channel, profile) = create_context_fields(&empty);
        assert_eq!(
            channel, "",
            "omitted channel must stay empty so the board fallback applies"
        );
        assert_eq!(
            profile, "",
            "omitted profile must stay empty so the board fallback applies"
        );

        // Explicit values still win; `channel_id` stays a working legacy alias.
        let explicit = serde_json::json!({"channel": "kanban", "profile": "custom"});
        let (channel, profile) = create_context_fields(&explicit);
        assert_eq!(channel, "kanban");
        assert_eq!(profile, "custom");
        let legacy = serde_json::json!({"channel_id": "legacy"});
        let (channel, _) = create_context_fields(&legacy);
        assert_eq!(channel, "legacy", "channel_id stays a working alias");
    }

    /// The create tool schema must document the BOARD fallback and must not
    /// promise caller-context inheritance ("default: current channel/profile").
    #[test]
    fn create_schema_documents_board_fallback() {
        let pool: Arc<RwLock<Option<PgPool>>> = Arc::new(RwLock::new(None));
        let tools = build_tools(&pool);
        let create = tools
            .iter()
            .find(|t| t.def.name == "create_kanban_task")
            .expect("create_kanban_task present");
        let props = &create.def.input_schema["properties"];
        for key in ["channel_id", "channel", "profile"] {
            let desc = props[key]["description"].as_str().unwrap_or("");
            assert!(
                !desc.contains("default: current"),
                "{key} must not promise caller-context inheritance: {desc}"
            );
            assert!(
                desc.contains("BOARD"),
                "{key} must state the board fallback: {desc}"
            );
        }
    }
}
