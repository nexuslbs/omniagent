//! mcp-server-actions — standalone MCP server for built-in action tools.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: kanban_dispatcher, hindsight_populator, relevance_indexer,
//!        setup_knowledge_pipeline
//!
//! Fully self-contained — no dependency on the omniagent crate.
//! Connects directly to Postgres via sqlx.

use anyhow::{Context, Result};
use mcp_server_util::*;
use parking_lot::Mutex;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Plugin config — received via MCP configure message, not from env vars
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Config {
    omni_dir: String,
    llm_provider: String,
    omniagent_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            omni_dir: "/opt/omni".to_string(),
            llm_provider: String::new(),
            omniagent_url: String::new(),
        }
    }
}

fn default_profile_name() -> String {
    "omni".to_string()
}

// Tool: kanban_dispatcher
// ---------------------------------------------------------------------------

/// Normalize the core's `POST /kanban/dispatch` response into an MCP tool result.
///
/// The core replies with the standard ok_json/err_json shape:
///   `{"success": true, "data": {"dispatched": bool, "task_id": .., "thread_id": .., "message": ..}}`
///   `{"success": false, "error": "..."}`
/// A bare data object (without the wrapper) is tolerated too.
fn format_dispatch_summary(body: &Value) -> (String, bool) {
    if body.get("success").and_then(|v| v.as_bool()) == Some(false) {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Core dispatch failed");
        return (err.to_string(), true);
    }
    let data = body.get("data").filter(|d| d.is_object()).unwrap_or(body);
    let dispatched = data
        .get("dispatched")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let task_id = data.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
    let thread_id = data.get("thread_id").and_then(|v| v.as_i64()).unwrap_or(0);
    let message = data.get("message").and_then(|v| v.as_str()).unwrap_or({
        if dispatched {
            "Dispatched kanban task"
        } else {
            "No eligible kanban tasks to dispatch"
        }
    });
    if !dispatched {
        return (message.to_string(), false);
    }
    let summary = match (task_id.is_empty(), thread_id) {
        (true, 0) => message.to_string(),
        (false, 0) => format!("{} (task {})", message, task_id),
        (true, tid) => format!("{} (thread {})", message, tid),
        (false, tid) => format!("{} (task {}, thread {})", message, task_id, tid),
    };
    (summary, false)
}

/// Thin HTTP caller of the core's `POST /kanban/dispatch` endpoint.
///
/// All dispatch logic (dependency checks, thread creation, status updates,
/// history) lives in the core; this plugin just forwards the request.
async fn handle_kanban_dispatcher(
    _pool: &PgPool,
    _args: &Value,
    config: &Config,
) -> Result<(String, bool)> {
    let base = if config.omniagent_url.is_empty() {
        "http://localhost:8080".to_string()
    } else {
        config.omniagent_url.clone()
    };
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/kanban/dispatch", base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to call core dispatch API: {}", e))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Bad response ({}): {}", status, e))?;
    Ok(format_dispatch_summary(&body))
}

// ---------------------------------------------------------------------------
// Tool: hindsight_populator
// ---------------------------------------------------------------------------

async fn handle_hindsight_populator(
    pool: &PgPool,
    _args: &Value,
    config: &Config,
) -> Result<(String, bool)> {
    let dir = &config.omni_dir;
    let watermark_path = format!("{}/hindsight_watermark.json", dir);
    let last_id: i64 = match std::fs::read_to_string(&watermark_path) {
        Ok(content) => serde_json::from_str::<Value>(&content)
            .ok()
            .and_then(|v| v["last_message_id"].as_i64())
            .unwrap_or(0),
        Err(_) => 0,
    };

    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, role, content FROM messages WHERE id > $1 AND msg_type IN ('message','reasoning','plan','error','cause','tool','tool-result') AND COALESCE(content,'') != '' ORDER BY id ASC LIMIT 200"
    )
    .bind(last_id)
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to query messages: {}", e))?;

    if rows.is_empty() {
        return Ok(("No new messages to process".to_string(), false));
    }

    let count = rows.len();
    let max_id = rows.iter().map(|r| r.0).max().unwrap_or(0);

    let watermark = serde_json::json!({"last_message_id": max_id, "last_run_at": chrono::Utc::now().to_rfc3339()});
    std::fs::write(&watermark_path, serde_json::to_string_pretty(&watermark)?)
        .map_err(|e| anyhow::anyhow!("Failed to write watermark: {}", e))?;

    Ok((
        format!(
            "Hindsight populator: retained {} messages (watermark: {} -> {})",
            count, last_id, max_id
        ),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: relevance_indexer
// ---------------------------------------------------------------------------

async fn handle_relevance_indexer(
    _pool: &PgPool,
    _args: &Value,
    config: &Config,
) -> Result<(String, bool)> {
    let profile = default_profile_name();
    let wiki_dir = format!("{}/profiles/{}/wiki", config.omni_dir, profile);
    let wiki_path = std::path::Path::new(&wiki_dir);

    if !wiki_path.exists() {
        return Ok(("No wiki directory found".to_string(), false));
    }

    let mut entries = Vec::new();
    collect_md_files(wiki_path, &mut entries, "");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut scored: Vec<(String, f64)> = entries
        .iter()
        .map(|(path, mtime)| {
            let age = now.saturating_sub(*mtime);
            let recency_score = if age < 3600 {
                50.0
            } else if age < 86400 {
                40.0
            } else if age < 604800 {
                30.0
            } else {
                10.0
            };
            (path.clone(), recency_score)
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut output = String::from("# Relevant Wiki Pages\n\n");
    for (path, score) in scored.iter().take(30) {
        let line = format!("- [{}]({}) --- score: {:.0}\n", path, path, score);
        if output.len() + line.len() > 1000 {
            break;
        }
        output.push_str(&line);
    }

    let output_path = format!("{}/relevant-index.md", wiki_dir);
    std::fs::write(&output_path, &output)
        .map_err(|e| anyhow::anyhow!("Failed to write relevant-index.md: {}", e))?;

    Ok((
        format!("Relevance indexer complete: {} files indexed", scored.len()),
        false,
    ))
}

fn collect_md_files(dir: &std::path::Path, entries: &mut Vec<(String, u64)>, prefix: &str) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    collect_md_files(&path, entries, &format!("{}{}/", prefix, name));
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name != "relevant-index.md" {
                        let mtime = entry
                            .metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        entries.push((format!("{}{}", prefix, name), mtime));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: setup_knowledge_pipeline
// ---------------------------------------------------------------------------

async fn handle_setup_knowledge_pipeline(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let schedule = args
        .get("schedule")
        .and_then(|v| v.as_str())
        .unwrap_or("0 */6 * * *");
    let id = format!(
        "knowledge-pipeline-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    let skills_json = serde_json::json!(["knowledge-pipeline"]).to_string();
    let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or("Run the knowledge pipeline maintenance (summarize channels, update wiki, run relevance indexer, populate hindsight).");

    let existing: Option<(String,)> =
        sqlx::query_as("SELECT id FROM cron_jobs WHERE name = 'knowledge-pipeline' LIMIT 1")
            .fetch_optional(pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to check existing cron: {}", e))?;

    if existing.is_some() {
        return Ok(("Knowledge Pipeline cron already exists".to_string(), false));
    }

    let channel: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM channels WHERE platform = 'cron' AND name = 'cron-default' LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to get cron channel: {}", e))?;

    let channel_id = channel.map(|c| c.0);

    sqlx::query(
        r#"INSERT INTO cron_jobs (id, name, display_name, schedule, prompt, skills, channel_id, mode, planning_mode, profile, enabled, active)
           VALUES ($1, 'knowledge-pipeline', 'Knowledge Pipeline', $2, $3, $4, $5, 'agentic', 'plan_with_subtasks', 'pipeline', true, true)"#
    )
    .bind(&id)
    .bind(schedule)
    .bind(prompt)
    .bind(&skills_json)
    .bind(channel_id)
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to create knowledge pipeline cron: {}", e))?;

    Ok((
        format!(
            "Knowledge Pipeline cron job created with schedule '{}'",
            schedule
        ),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Shared pool — populated by configure callback before any tool call
    let pool: Arc<RwLock<Option<PgPool>>> = Arc::new(RwLock::new(None));
    let config: Arc<Mutex<Config>> = Arc::new(Mutex::new(Config::default()));

    let p_kanban = pool.clone();
    let c_kanban = config.clone();
    let kanban_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_kanban.clone();
        let c = c_kanban.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard.as_ref().expect("Pool not initialized").clone();
            let config = c.lock().clone();
            handle_kanban_dispatcher(&pool, &args, &config).await
        })
    });

    let p_hindsight = pool.clone();
    let c_hindsight = config.clone();
    let hindsight_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_hindsight.clone();
        let c = c_hindsight.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard.as_ref().expect("Pool not initialized").clone();
            let config = c.lock().clone();
            handle_hindsight_populator(&pool, &args, &config).await
        })
    });

    let p_relevance = pool.clone();
    let c_relevance = config.clone();
    let relevance_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_relevance.clone();
        let c = c_relevance.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard.as_ref().expect("Pool not initialized").clone();
            let config = c.lock().clone();
            handle_relevance_indexer(&pool, &args, &config).await
        })
    });

    let p_pipeline = pool.clone();
    let pipeline_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_pipeline.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard.as_ref().expect("Pool not initialized").clone();
            handle_setup_knowledge_pipeline(&pool, &args).await
        })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "kanban_dispatcher".to_string(),
                description: "Process pending kanban tasks: move 'todo' tasks to 'ready' by creating threads and messages, respecting dependencies and ordering by priority and position.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                }),
            },
            handler: kanban_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "hindsight_populator".to_string(),
                description: "Retain recent messages into Hindsight memory. Queries new messages since the last watermark and retains them for long-term persistent recall.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                }),
            },
            handler: hindsight_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "relevance_indexer".to_string(),
                description: "Update the wiki relevance index. Scans wiki files and updates relevant-index.md based on recency and reference count.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                }),
            },
            handler: relevance_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "setup_knowledge_pipeline".to_string(),
                description: "Create or verify the periodic knowledge pipeline cron job. Creates a cron job that runs the maintenance pipeline (summarize channels, update wiki/skills, relevance indexing, hindsight populate).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "schedule": {
                            "type": "string",
                            "description": "Optional cron schedule in 5-field Linux format. Default: '0 */6 * * *'."
                        },
                        "prompt": {
                            "type": "string",
                            "description": "Optional prompt override."
                        }
                    },
                    "required": [],
                }),
            },
            handler: pipeline_handler,
        },
    ];

    run_server_with_config(
        ServerInfo {
            name: "mcp-server-actions".to_string(),
            version: "0.1.0".to_string(),
        },
        tools,
        {
            let p = pool.clone();
            let c = config.clone();
            Some(move |params: serde_json::Value| {
                let database_url = params
                    .get("database_url")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| {
                        eprintln!("FATAL: database_url not in configure message");
                        std::process::exit(1);
                    });
                // Store config
                let mut cfg = c.lock();
                if let Some(dir) = params.get("omni_dir").and_then(|v| v.as_str()) {
                    if !dir.is_empty() {
                        cfg.omni_dir = dir.to_string();
                    }
                }
                if let Some(prov) = params.get("llm_provider").and_then(|v| v.as_str()) {
                    if !prov.is_empty() {
                        cfg.llm_provider = prov.to_string();
                    }
                }
                if let Some(url) = params.get("omniagent_url").and_then(|v| v.as_str()) {
                    if !url.is_empty() {
                        cfg.omniagent_url = url.to_string();
                    }
                }
                tokio::task::block_in_place(|| {
                    let rt = tokio::runtime::Handle::current();
                    let new_pool = rt
                        .block_on(async {
                            PgPoolOptions::new()
                                .max_connections(5)
                                .acquire_timeout(std::time::Duration::from_secs(30))
                                .connect(&database_url)
                                .await
                                .context("Failed to connect to database")
                        })
                        .expect("Failed to connect to database");
                    *p.blocking_write() = Some(new_pool);
                });
                tracing::info!("Actions plugin configured with database_url");
            })
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::format_dispatch_summary;
    use serde_json::json;

    #[test]
    fn success_dispatched_summary() {
        let body = json!({
            "success": true,
            "data": {
                "dispatched": true,
                "task_id": "task-abc",
                "thread_id": 42,
                "message": "Dispatched kanban task 'Build X' (task-abc) -> thread 42 (ready)"
            }
        });
        let (msg, is_err) = format_dispatch_summary(&body);
        assert!(!is_err);
        assert!(msg.contains("task-abc"));
        assert!(msg.contains("42"));
    }

    #[test]
    fn success_nothing_eligible() {
        let body = json!({"success": true, "data": {"dispatched": false}});
        let (msg, is_err) = format_dispatch_summary(&body);
        assert!(!is_err);
        assert!(msg.contains("No eligible kanban tasks to dispatch"));
    }

    #[test]
    fn success_no_thread_id() {
        let body = json!({"success": true, "data": {"dispatched": true, "task_id": "t1"}});
        let (msg, is_err) = format_dispatch_summary(&body);
        assert!(!is_err);
        assert!(msg.contains("t1"));
    }

    #[test]
    fn failure_reports_error() {
        let body = json!({"success": false, "error": "dispatch exploded"});
        let (msg, is_err) = format_dispatch_summary(&body);
        assert!(is_err);
        assert_eq!(msg, "dispatch exploded");
    }

    #[test]
    fn failure_missing_error_field() {
        let body = json!({"success": false});
        let (msg, is_err) = format_dispatch_summary(&body);
        assert!(is_err);
        assert!(msg.contains("Core dispatch failed"));
    }

    #[test]
    fn bare_data_object_tolerated() {
        // A raw data object without the ok_json wrapper also works.
        let body = json!({"dispatched": true, "task_id": "t9", "thread_id": 7});
        let (msg, is_err) = format_dispatch_summary(&body);
        assert!(!is_err);
        assert!(msg.contains("t9"));
        assert!(msg.contains("7"));
    }
}
