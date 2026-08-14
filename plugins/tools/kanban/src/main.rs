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

async fn handle_create(
    _pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
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
        "status": args["status"].as_str().unwrap_or("backlog"),
        "priority": args["priority"].as_i64().unwrap_or(0),
        "assignee": args["assignee"].as_str().unwrap_or(""),
        "template": args["template"].as_str().unwrap_or(""),
        "workflow_id": args["workflow_id"].as_str().unwrap_or(""),
    });
    let channel_id = args["channel_id"]
        .as_str()
        .map(String::from)
        .or_else(|| meta.and_then(|m| m.channel_id.clone()));
    if let Some(cid) = channel_id {
        req["channel_id"] = serde_json::json!(cid);
    }
    let profile = args["profile"]
        .as_str()
        .map(String::from)
        .or_else(|| meta.and_then(|m| m.profile_name.clone()));
    if let Some(p) = profile {
        req["profile"] = serde_json::json!(p);
    }
    let resp = api_call(reqwest::Method::POST, "/kanban/tasks", Some(&req)).await?;
    let id = resp["data"]["id"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing 'id' in create response"))?;
    let status = args["status"].as_str().unwrap_or("backlog");
    Ok((
        format!("Kanban task '{title}' created with id '{id}' and status '{status}'"),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: list_kanban_tasks
// ---------------------------------------------------------------------------

async fn handle_list(_pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let _status_filter = args["status"].as_str().unwrap_or("");
    let resp = api_call(
        reqwest::Method::GET,
        "/kanban/tasks?show_archived=true",
        None,
    )
    .await?;
    let tasks = resp["data"]
        .as_array()
        .ok_or_else(|| anyhow!("Unexpected list response from kanban API"))?;
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
            lines.push(format!(
                "  {}. **{}** (`{}`)\n     - Priority: {} | Assignee: {} | Created: {}",
                i + 1,
                title,
                id,
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

async fn handle_update(_pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let id = args["id"]
        .as_str()
        .ok_or_else(|| anyhow!("Missing required argument: 'id'"))?;
    let mut req = serde_json::json!({});
    for field in [
        "title",
        "body",
        "status",
        "priority",
        "assignee",
        "channel_id",
        "profile",
        "archived",
        "workflow_id",
    ] {
        if let Some(v) = args.get(field) {
            req[field] = v.clone();
        }
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
/// Plugin config — received via configure message.
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
// Tool: kanban_review_task (MANUAL/API only — spec §8 R12)
// ---------------------------------------------------------------------------

/// Manual/API-only review decision with the same validation as POST /review:
/// decision whitelist (approve | rework | retest | block) + R5 target
/// validation + retry guards — all enforced by
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

#[tokio::main]
async fn main() -> Result<()> {
    // Shared pool — populated by configure callback before any tool call
    let pool: Arc<RwLock<Option<PgPool>>> = Arc::new(RwLock::new(None));

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
    let update_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_update.clone();
        Box::pin(async move {
            let guard = p.read().await;

            let pool = guard.as_ref().expect("Pool not initialized").clone();

            handle_update(&pool, &args).await
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
                    "Create a new kanban task. Adds a task to the kanban board with optional body, status, priority, and assignee."
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
                            "description": "Optional status (default: 'backlog'). One of: backlog, todo, running, testing, review, blocked, done",
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
                        "channel_id": { "type": "string", "description": "Optional channel name for thread/cause creation (default: current channel)" },
                        "profile": {
                            "type": "string",
                            "description": "Optional profile name for the task (default: current profile)"
                        },
                        "template": {
                            "type": "string",
                            "description": "Optional template file name (without .md) to use for execution context"
                        },
                        "workflow_id": {
                            "type": "string",
                            "description": "Optional workflow key (e.g. exec-test-review) this task belongs to"
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
                        }
                    }
                }),
            },
            handler: list_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "update_kanban_task".to_string(),
                description: "Update an existing kanban task. Only provided fields are updated. Status changes are recorded in history.".to_string(),
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
                        "channel_id": { "type": "string", "description": "New channel name" },
                        "profile": {
                            "type": "string",
                            "description": "New profile name"
                        },
                        "archived": {
                            "type": "boolean",
                            "description": "Set to true to archive, false to unarchive"
                        },
                        "workflow_id": {
                            "type": "string",
                            "description": "Set the task workflow key (e.g. exec-test-review)"
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
                    "MANUAL/API-only review decision for a kanban task. Decision: approve (task done), rework (back to running with a new executor thread), retest (back to testing with a new tester thread), block (task blocked). Invalid decisions and invalid targets (e.g. retest without a tester role in the workflow) are rejected. The reviewer AGENT never calls this tool — it signals approve via normal completion and issues via fail-thread."
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
    let server_info = ServerInfo {
        name: "mcp-server-kanban".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server_with_config(server_info, tools, {
        let p = pool.clone();
        Some(move |params: serde_json::Value| {
            let config = PluginConfig::from_json(&params);
            let _ = BASE_URL.set(config.base_url.clone());
            tokio::task::block_in_place(|| {
                let rt = tokio::runtime::Handle::current();
                let new_pool = rt
                    .block_on(db::connect(&config.database_url))
                    .expect("Failed to connect to database");
                *p.blocking_write() = Some(new_pool);
            });
            tracing::info!("Kanban plugin configured with database_url");
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
