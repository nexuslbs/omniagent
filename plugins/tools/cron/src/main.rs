//! mcp-server-cron: standalone MCP server for cron job management.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: create_cron_job, list_cron_jobs, delete_cron_job, update_cron_job
//!
//! Definitions live in {OMNI_DIR}/config/tasks.yml (`schedules:` key) - the
//! git-tracked source of truth. Runtime state (cadence) is tracked implicitly
//! via the threads each schedule creates (threads.schedule_task_id) and the
//! task_runs bookkeeping table.

use anyhow::Result;
use mcp_server_util::*;
use omniagent::db;
use omniagent::tasks_yaml::{self, ScheduleDef};
use serde_json::Value;
use sqlx::PgPool;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

/// OMNI_DIR (data_dir) - config files live in {data_dir}/config/.
fn data_dir() -> String {
    std::env::var("OMNI_DIR").unwrap_or_else(|_| "/opt/omni".to_string())
}

/// Resolve a channel id to its NAME - with string ids the id IS the name
/// (channels.yml key). Verified to exist in the yml; unknown -> None.
async fn channel_name_for_id(_pool: &PgPool, id: &str) -> Option<String> {
    omniagent::channels_yaml::exists(id).then(|| id.to_string())
}

fn validate_5field(schedule: &str) -> Result<()> {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(anyhow::anyhow!(
            "Invalid cron expression '{}': expected 5 fields (min hour day month weekday), got {} fields",
            schedule,
            fields.len()
        ));
    }
    let cron_expr = format!("0 {}", schedule);
    cron::Schedule::from_str(&cron_expr)
        .map_err(|e| anyhow::anyhow!("Invalid cron expression '{}': {}", schedule, e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool: create_cron_job
// ---------------------------------------------------------------------------

async fn handle_create(
    pool: &PgPool,
    args: &Value,
    meta: Option<&McpMeta>,
) -> Result<(String, bool)> {
    let name = args["name"].as_str().unwrap_or("");
    let schedule = args["schedule"].as_str().unwrap_or("");
    let prompt = args["prompt"].as_str();
    let display_name = args["display_name"].as_str().unwrap_or(name);
    let skills_str = args["skills"].as_str().unwrap_or("");
    // channel: explicit channel_id wins; else the CURRENT channel from the
    // agent's runtime context (_meta.channel_id); else no channel (default).
    let channel_id_arg = args["channel_id"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| meta.and_then(|m| m.channel_id.clone()));
    // profile: explicit argument wins; else the agent's ACTIVE profile from
    // _meta.profile_name (the job runs under that profile when fired).
    let profile_owned = args["profile"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| meta.and_then(|m| m.profile_name.clone()));
    let profile_arg = profile_owned.as_deref();
    let mode = args["mode"].as_str().unwrap_or("agentic");
    let action_id = args["action_id"].as_str();
    let silent = args["silent"].as_bool();

    if name.is_empty() {
        return Err(anyhow::anyhow!("Job name must not be empty"));
    }
    if schedule.is_empty() {
        return Err(anyhow::anyhow!("Schedule must not be empty"));
    }
    validate_5field(schedule)?;
    if mode == "agentic" && prompt.unwrap_or("").is_empty() {
        return Err(anyhow::anyhow!("Prompt must not be empty for agentic mode"));
    }
    if mode == "action" && action_id.unwrap_or("").is_empty() {
        return Err(anyhow::anyhow!("action_id is required for action mode"));
    }
    if mode != "agentic" && mode != "action" {
        return Err(anyhow::anyhow!(
            "Invalid mode '{}'. Must be 'agentic' or 'action'",
            mode
        ));
    }

    let id = format!("cron_{:x}", {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    });

    let skills_json: Value = if skills_str.is_empty() {
        serde_json::json!([])
    } else {
        let parts: Vec<String> = skills_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        serde_json::json!(parts)
    };

    // yml stores channel NAME - resolve from id (if given), else default (None).
    let channel_name = match channel_id_arg {
        Some(cid) => channel_name_for_id(pool, &cid).await,
        None => None,
    };

    let mut tasks = tasks_yaml::load_tasks_or_empty(&data_dir());
    if tasks.schedules.contains_key(&id) {
        return Err(anyhow::anyhow!("Cron job '{}' already exists", name));
    }

    let def = ScheduleDef {
        enabled: true,
        channel: channel_name,
        profile: profile_arg.map(|s| s.to_string()),
        plan: None,
        cron: schedule.to_string(),
        prompt: Some(prompt.unwrap_or("").to_string()),
        action: if mode == "action" {
            action_id.map(|s| s.to_string())
        } else {
            None
        },
        template: None,
        skills: Some(skills_json.to_string()),
        silent: Some(silent.unwrap_or(false)),
        display_name: Some(display_name.to_string()),
    };
    tasks.schedules.insert(id.clone(), def);
    tasks_yaml::save_tasks(&data_dir(), &tasks)
        .map_err(|e| anyhow::anyhow!("Failed to save tasks.yml: {}", e))?;

    Ok((
        format!("✅ Created cron job **{}** (`{}`)", display_name, name),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: list_cron_jobs
// ---------------------------------------------------------------------------

async fn handle_list(_pool: &PgPool, _args: &Value) -> Result<(String, bool)> {
    let tasks = tasks_yaml::load_tasks_or_empty(&data_dir());

    if tasks.schedules.is_empty() {
        return Ok(("_No cron jobs configured._".to_string(), false));
    }

    let mut lines = vec!["**Cron Jobs:**".to_string()];
    for (i, (id, row)) in tasks.schedules.iter().enumerate() {
        let enabled = row.enabled;
        let status = if enabled { "🟢" } else { "🔴" };
        let name_display = row.display_name.clone().unwrap_or_else(|| id.clone());
        let mode_display = if row.action.is_some() {
            "action"
        } else {
            "agentic"
        };
        let prompt_preview = row
            .prompt
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>();
        let channel = row.channel.clone().unwrap_or_else(|| "default".to_string());
        lines.push(format!(
            "{}. {} **{}** (`{}`)\n   - Schedule: `{}` | Mode: {} | Channel: {}\n   - Runs are visible via threads (schedule_task_id = `{}`)\n   - Prompt: {}",
            i + 1, status, name_display, id, row.cron, mode_display, channel, id, prompt_preview
        ));
    }

    Ok((lines.join("\n"), false))
}

// ---------------------------------------------------------------------------
// Tool: delete_cron_job
// ---------------------------------------------------------------------------

async fn handle_delete(_pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let job_id = args["job_id"].as_str().unwrap_or("");
    if job_id.is_empty() {
        return Err(anyhow::anyhow!("Missing required argument: 'job_id'"));
    }

    let mut tasks = tasks_yaml::load_tasks_or_empty(&data_dir());
    if tasks.schedules.remove(job_id).is_none() {
        return Err(anyhow::anyhow!("Cron job `{}` not found", job_id));
    }
    tasks_yaml::save_tasks(&data_dir(), &tasks)
        .map_err(|e| anyhow::anyhow!("Failed to save tasks.yml: {}", e))?;

    Ok((format!("🗑️ Deleted cron job `{}`", job_id), false))
}

// ---------------------------------------------------------------------------
// Tool: update_cron_job
// ---------------------------------------------------------------------------

async fn handle_update(_pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let job_id = args["job_id"].as_str().unwrap_or("");
    if job_id.is_empty() {
        return Err(anyhow::anyhow!("Missing required argument: 'job_id'"));
    }

    let mut tasks = tasks_yaml::load_tasks_or_empty(&data_dir());
    let entry = tasks
        .schedules
        .get_mut(job_id)
        .ok_or_else(|| anyhow::anyhow!("Cron job `{}` not found", job_id))?;

    if let Some(schedule) = args["schedule"].as_str() {
        validate_5field(schedule)?;
        entry.cron = schedule.to_string();
    }
    if let Some(prompt) = args["prompt"].as_str() {
        entry.prompt = Some(prompt.to_string());
        entry.action = None; // prompt switches mode to agentic
    }
    if let Some(active) = args["active"].as_bool() {
        entry.enabled = active;
    }

    tasks_yaml::save_tasks(&data_dir(), &tasks)
        .map_err(|e| anyhow::anyhow!("Failed to save tasks.yml: {}", e))?;

    Ok((format!("✅ Updated cron job `{}`", job_id), false))
}

// ---------------------------------------------------------------------------
// Plugin config hook
// ---------------------------------------------------------------------------

/// Plugin config - received via configure message.
#[derive(Debug, Clone)]
struct PluginConfig {
    pub database_url: String,
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
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Shared pool - populated by configure callback before any tool call
    // Channels live in {OMNI_DIR}/config/channels.yml - set the global data dir.
    omniagent::channels_yaml::set_data_dir(&data_dir());
    let pool = Arc::new(RwLock::new(None::<PgPool>));

    // Wrap each handler to capture a clone of the shared pool
    let p_cron = pool.clone();
    let create_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let p = p_cron.clone();
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
    let p_del = pool.clone();
    let delete_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_del.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard.as_ref().expect("Pool not initialized").clone();
            handle_delete(&pool, &args).await
        })
    });
    let p_upd = pool.clone();
    let update_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_upd.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard.as_ref().expect("Pool not initialized").clone();
            handle_update(&pool, &args).await
        })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "create_cron_job".to_string(),
                description:
                    "Create a new cron job. Schedules a recurring task with a cron expression and a prompt to execute. Provide a unique short name (lowercase, underscores, no spaces) as 'name', and optionally a human-readable 'display_name'.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "A unique short name for this cron job (lowercase, underscores, no spaces)" },
                        "display_name": { "type": "string", "description": "Optional human-readable display name" },
                        "schedule": { "type": "string", "description": "Cron schedule expression in 5-field Linux format (min hour day month weekday)" },
                        "prompt": { "type": "string", "description": "The prompt/message to execute when the cron job triggers" },
                        "skills": { "type": "string", "description": "Optional comma-separated list of skill names" },
                        "channel_id": { "type": "string", "description": "Optional channel name (default: current channel)" },
                        "profile": { "type": "string", "description": "Optional profile name (default: current profile)" },
                        "mode": { "type": "string", "description": "Job mode: 'agentic' (default) or 'action'" },
                        "action_id": { "type": "string", "description": "For mode='action': the action ID to execute" },
                        "silent": { "type": "boolean", "description": "When true and mode='action', no thread/messages on success" },
                    },
                    "required": ["name", "schedule"],
                }),
            },
            handler: create_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "list_cron_jobs".to_string(),
                description: "List all cron jobs with their schedule and status. Runs are visible via the threads each job creates.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                }),
            },
            handler: list_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "delete_cron_job".to_string(),
                description: "Delete a cron job by its job_id.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "job_id": { "type": "string", "description": "The ID of the cron job to delete" },
                    },
                    "required": ["job_id"],
                }),
            },
            handler: delete_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "update_cron_job".to_string(),
                description: "Update a cron job's schedule, prompt, or active status.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "job_id": { "type": "string", "description": "The ID of the cron job to update" },
                        "schedule": { "type": "string", "description": "New cron schedule in 5-field format" },
                        "prompt": { "type": "string", "description": "New prompt (switches mode to agentic)" },
                        "active": { "type": "boolean", "description": "Set to true/false to activate/deactivate" },
                    },
                    "required": ["job_id"],
                }),
            },
            handler: update_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-cron".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server_with_config(server_info, tools, {
        let p = pool.clone();
        Some(move |params: serde_json::Value| {
            let config = PluginConfig::from_json(&params);
            tokio::task::block_in_place(|| {
                let rt = tokio::runtime::Handle::current();
                let new_pool = rt
                    .block_on(db::connect(&config.database_url))
                    .expect("Failed to connect to database");
                *p.blocking_write() = Some(new_pool);
            });
            tracing::info!("Cron plugin configured with database_url");
        })
    })
    .await
}
