//! mcp-server-cron: standalone MCP server for cron job management.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: create_cron_job, list_cron_jobs, delete_cron_job, update_cron_job

use anyhow::{Context, Result};
use mcp_server_util::*;
use omniagent::db;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::PgPool;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Tool: create_cron_job
// ---------------------------------------------------------------------------

async fn handle_create(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let name = args["name"].as_str().unwrap_or("");
    let schedule = args["schedule"].as_str().unwrap_or("");
    let prompt = args["prompt"].as_str();
    let display_name = args["display_name"].as_str().unwrap_or(name);
    let skills_str = args["skills"].as_str().unwrap_or("");
    let channel_id_arg = args["channel_id"].as_i64();
    let profile_arg = args["profile"].as_str();
    let mode = args["mode"].as_str().unwrap_or("agentic");
    let action_id = args["action_id"].as_str();
    let silent = args["silent"].as_bool();

    if name.is_empty() {
        return Err(anyhow::anyhow!("Job name must not be empty"));
    }
    if schedule.is_empty() {
        return Err(anyhow::anyhow!("Schedule must not be empty"));
    }
    // Validate 5-field cron
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(anyhow::anyhow!(
            "Invalid cron expression '{}': expected 5 fields (min hour day month weekday), got {} fields",
            schedule, fields.len()
        ));
    }
    let cron_expr = format!("0 {}", schedule);
    if let Err(e) = cron::Schedule::from_str(&cron_expr) {
        return Err(anyhow::anyhow!(
            "Invalid cron expression '{}': {}",
            schedule,
            e
        ));
    }
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

    let resolved_channel_id = if let Some(cid) = channel_id_arg {
        cid
    } else {
        match db::types::get_channel_by_platform_name(pool, "cron", "cron").await {
            Ok(Some(ch)) => ch.id,
            _ => anyhow::bail!("No default cron channel found. Create a channel with platform='cron' and name='cron' first."),
        }
    };

    sql_forge!(
        r#"
        INSERT INTO cron_jobs (id, name, display_name, schedule, prompt, skills, channel_id, profile, mode, action_id, silent)
        VALUES (:id, :name, :display_name, :schedule, NULLIF(:prompt, '')::text, :skills, :channel_id, NULLIF(:profile, '')::text, :mode, NULLIF(:action_id, '')::text, :silent)
        "#,
        ( :id = &id, :name = name, :display_name = display_name, :schedule = schedule, :prompt = prompt.unwrap_or(""), :skills = skills_json.to_string(), :channel_id = resolved_channel_id, :profile = profile_arg.unwrap_or(""), :mode = mode, :action_id = action_id.unwrap_or(""), :silent = silent.unwrap_or(false) )
    )
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to create cron job: {}", e))?;

    Ok((
        format!("✅ Created cron job **{}** (`{}`)", display_name, name),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: list_cron_jobs
// ---------------------------------------------------------------------------

use chrono::{DateTime, Utc};
use sqlx::FromRow;

#[derive(Debug, FromRow)]
#[allow(dead_code)]
struct CronJobRow {
    id: String,
    name: Option<String>,
    schedule: String,
    prompt: Option<String>,
    enabled: Option<bool>,
    active: Option<bool>,
    mode: Option<String>,
    action_id: Option<String>,
    last_run_at: Option<DateTime<Utc>>,
    next_run_at: Option<DateTime<Utc>>,
    created_at: Option<DateTime<Utc>>,
    silent: Option<bool>,
}

async fn handle_list(pool: &PgPool, _args: &Value) -> Result<(String, bool)> {
    let rows: Vec<CronJobRow> = sql_forge!(
        CronJobRow,
        r#"
        SELECT id, name, schedule, prompt, enabled, active, mode, action_id,
               last_run_at, next_run_at, created_at, silent
        FROM cron_jobs
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to list cron jobs: {}", e))?;

    if rows.is_empty() {
        return Ok(("_No cron jobs configured._".to_string(), false));
    }

    let mut lines = vec!["**Cron Jobs:**".to_string()];
    for (i, row) in rows.iter().enumerate() {
        let status = if row.active.unwrap_or(false) {
            "🟢"
        } else {
            "🔴"
        };
        let name_display = row.name.as_deref().unwrap_or(&row.id);
        let mode_display = row.mode.as_deref().unwrap_or("agentic");
        let last = row
            .last_run_at
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "never".to_string());
        let next = row
            .next_run_at
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let prompt_preview = row
            .prompt
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>();
        lines.push(format!(
            "{}. {} **{}** (`{}`)\n   - Schedule: `{}` | Mode: {} | Active: {}\n   - Last: {} | Next: {}\n   - Prompt: {}",
            i + 1, status, name_display, row.id, row.schedule, mode_display, status, last, next, prompt_preview
        ));
    }

    Ok((lines.join("\n"), false))
}

// ---------------------------------------------------------------------------
// Tool: delete_cron_job
// ---------------------------------------------------------------------------

async fn handle_delete(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let job_id = args["job_id"].as_str().unwrap_or("");
    if job_id.is_empty() {
        return Err(anyhow::anyhow!("Missing required argument: 'job_id'"));
    }

    sql_forge!(
        r#"DELETE FROM cron_jobs WHERE id = :job_id"#,
        ( :job_id = job_id )
    )
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to delete cron job: {}", e))?;

    Ok((format!("🗑️ Deleted cron job `{}`", job_id), false))
}

// ---------------------------------------------------------------------------
// Tool: update_cron_job
// ---------------------------------------------------------------------------

async fn handle_update(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let job_id = args["job_id"].as_str().unwrap_or("");
    if job_id.is_empty() {
        return Err(anyhow::anyhow!("Missing required argument: 'job_id'"));
    }

    // Build UPDATE with only the fields that are provided
    if let Some(schedule) = args["schedule"].as_str() {
        // Validate 5-field cron
        let fields: Vec<&str> = schedule.split_whitespace().collect();
        if fields.len() != 5 {
            anyhow::bail!("Invalid cron expression '{}': expected 5 fields", schedule);
        }
        let cron_expr = format!("0 {}", schedule);
        cron::Schedule::from_str(&cron_expr)
            .map_err(|e| anyhow::anyhow!("Invalid cron expression '{}': {}", schedule, e))?;

        sql_forge!(
            r#"UPDATE cron_jobs SET schedule = :schedule, updated_at = NOW() WHERE id = :job_id"#,
            ( :schedule = schedule, :job_id = job_id )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update cron job schedule: {}", e))?;
    }
    if let Some(prompt) = args["prompt"].as_str() {
        sql_forge!(
            r#"UPDATE cron_jobs SET prompt = NULLIF(:prompt, '')::text, mode = 'agentic', updated_at = NOW() WHERE id = :job_id"#,
            ( :prompt = prompt, :job_id = job_id )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update cron job prompt: {}", e))?;
    }
    if let Some(active) = args["active"].as_bool() {
        sql_forge!(
            r#"UPDATE cron_jobs SET active = :active, updated_at = NOW() WHERE id = :job_id"#,
            ( :active = active, :job_id = job_id )
        )
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to update cron job active: {}", e))?;
    }

    Ok((format!("✅ Updated cron job `{}`", job_id), false))
}

// ---------------------------------------------------------------------------
// Plugin config hook
// ---------------------------------------------------------------------------

/// Plugin config — received via configure message.
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
    // Shared pool — populated by configure callback before any tool call
    let pool = Arc::new(RwLock::new(None::<PgPool>));

    // Wrap each handler to capture a clone of the shared pool
    let p_cron = pool.clone();
    let create_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let p = p_cron.clone();
        Box::pin(async move {
            let guard = p.read().await;
            let pool = guard.as_ref().expect("Pool not initialized").clone();
            handle_create(&pool, &args).await
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
                        "channel_id": { "type": "integer", "description": "Optional channel ID" },
                        "profile": { "type": "string", "description": "Optional profile name" },
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
                description: "List all cron jobs with their schedule, status, and last/next run times.".to_string(),
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
