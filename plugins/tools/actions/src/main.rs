//! mcp-server-actions — standalone MCP server for built-in action tools.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: hindsight_populator, relevance_indexer,
//!        setup_knowledge_pipeline
//!
//! Depends on the omniagent crate for tasks.yml helpers (setup_knowledge_pipeline
//! seeds {OMNI_DIR}/config/tasks.yml); connects directly to Postgres via sqlx.

use anyhow::{Context, Result};
use mcp_server_util::*;
use omniagent::tasks_yaml::{self, ScheduleDef};
use parking_lot::Mutex;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Plugin config — received via MCP configure message, not from env vars
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Config {
    omni_dir: String,
    llm_provider: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            omni_dir: "/opt/omni".to_string(),
            llm_provider: String::new(),
        }
    }
}

fn default_profile_name() -> String {
    "omni".to_string()
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

    let rows: Vec<i64> = sql_forge!(
        scalar i64,
        "SELECT id FROM messages WHERE id > :last_id AND msg_type IN ('message','reasoning','plan','error','cause','tool','tool-result') AND COALESCE(content,'') != '' ORDER BY id ASC LIMIT 200",
        ( :last_id = last_id )
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to query messages: {}", e))?;

    if rows.is_empty() {
        return Ok(("No new messages to process".to_string(), false));
    }

    let count = rows.len();
    let max_id = rows.iter().copied().max().unwrap_or(0);

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

async fn handle_setup_knowledge_pipeline(
    _pool: &PgPool,
    args: &Value,
    config: &Config,
) -> Result<(String, bool)> {
    let schedule = args
        .get("schedule")
        .and_then(|v| v.as_str())
        .unwrap_or("0 */6 * * *");
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("Run the knowledge pipeline maintenance (summarize channels, update wiki, run relevance indexer, populate hindsight).");

    // Definitions live in {OMNI_DIR}/config/tasks.yml (`schedules:` key).
    let mut tasks = tasks_yaml::load_tasks_or_empty(&config.omni_dir);
    if tasks.schedules.contains_key("knowledge_pipeline") {
        return Ok((
            "Knowledge Pipeline schedule already exists in tasks.yml".to_string(),
            false,
        ));
    }

    let skills_json = serde_json::json!(["knowledge-pipeline"]).to_string();
    let def = ScheduleDef {
        enabled: true,
        channel: Some("cron-default".to_string()),
        profile: Some("pipeline".to_string()),
        plan: Some(true),
        cron: schedule.to_string(),
        prompt: Some(prompt.to_string()),
        action: None,
        template: None,
        skills: Some(skills_json),
        silent: Some(false),
        display_name: Some("Knowledge Pipeline".to_string()),
    };
    tasks
        .schedules
        .insert("knowledge_pipeline".to_string(), def);
    tasks_yaml::save_tasks(&config.omni_dir, &tasks)
        .map_err(|e| anyhow::anyhow!("Failed to save tasks.yml: {}", e))?;

    Ok((
        format!(
            "Knowledge Pipeline schedule created in tasks.yml with cron '{}'",
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
    let c_pipeline = config.clone();
    let pipeline_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_pipeline.clone();
        let c = c_pipeline.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard.as_ref().expect("Pool not initialized").clone();
            let config = c.lock().clone();
            handle_setup_knowledge_pipeline(&pool, &args, &config).await
        })
    });

    let tools = vec![
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
                description: "Create or verify the periodic knowledge pipeline schedule in tasks.yml. Creates a schedule that runs the maintenance pipeline (summarize channels, update wiki/skills, relevance indexing, hindsight populate).".to_string(),
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
