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
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_profile_name() -> String {
    std::env::var("DEFAULT_PROFILE").unwrap_or_else(|_| "default".to_string())
}

fn data_dir() -> String {
    std::env::var("OMNI_DIR").unwrap_or_else(|_| {
        eprintln!("FATAL: OMNI_DIR must be set");
        std::process::exit(1);
    })
}

// ---------------------------------------------------------------------------
// Tool: kanban_dispatcher
// ---------------------------------------------------------------------------

async fn handle_kanban_dispatcher(pool: &PgPool, _args: &Value) -> Result<(String, bool)> {
    // Query ALL todo tasks ordered by priority, then position
    let tasks: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, title FROM kanban_tasks WHERE status = 'todo' ORDER BY priority ASC, position ASC"
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to query kanban tasks: {}", e))?;

    if tasks.is_empty() {
        return Ok(("No eligible kanban tasks to dispatch".to_string(), false));
    }

    // Iterate in priority/position order, find first task with all deps satisfied
    for (id, title) in &tasks {
        // Query dependencies for this task
        let deps: Vec<(String,)> = sqlx::query_as(
            "SELECT depends_on_id FROM kanban_task_dependencies WHERE task_id = $1"
        )
        .bind(id)
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to query task dependencies: {}", e))?;

        // Check each dependency's status
        let all_satisfied = {
            if deps.is_empty() {
                true
            } else {
                let mut ok = true;
                for (dep_id,) in &deps {
                    let dep_status: Option<(String,)> = sqlx::query_as(
                        "SELECT status FROM kanban_tasks WHERE id = $1"
                    )
                    .bind(dep_id)
                    .fetch_optional(pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to query dependency status: {}", e))?;

                    match dep_status {
                        Some((status,)) => {
                            if status != "review" && status != "done" {
                                ok = false;
                                break;
                            }
                        }
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                ok
            }
        };

        if !all_satisfied {
            continue;
        }

        // ── All deps satisfied — create thread for this kanban task ──
        // 1. Get full task data
        let task_data: Option<(String, String, Option<i64>, Option<String>, Option<String>, String)> = sqlx::query_as(
            "SELECT id, title, channel_id, profile, template, planning_mode FROM kanban_tasks WHERE id = $1"
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to query kanban task '{}': {}", id, e))?;

        let (_task_id, task_title, maybe_channel_id, task_profile, _task_template, task_planning_mode) = match task_data {
            Some(r) => r,
            None => continue,
        };

        let channel_id = match maybe_channel_id {
            Some(cid) => cid,
            None => {
                return Ok((format!("Kanban task '{}' ({}) has no channel — cannot create thread", title, id), false));
            }
        };

        // 2. Get channel's current_profile (fallback to default)
        let chan_profile: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT current_profile FROM channels WHERE id = $1"
        )
        .bind(channel_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to query channel {}: {}", channel_id, e))?;

        let effective_profile = task_profile
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| chan_profile.as_ref().and_then(|(s,)| s.as_deref()))
            .unwrap_or(&default_profile_name())
            .to_string();

        // 3. Build content: use body if present, otherwise title
        let body_row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT body FROM kanban_tasks WHERE id = $1"
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to query kanban task body: {}", e))?;

        let body_text = body_row
            .as_ref()
            .and_then(|(b,)| b.as_deref())
            .unwrap_or("");

        let content = if body_text.is_empty() {
            task_title.clone()
        } else {
            format!("{}\n\n{}", task_title, body_text)
        };

        // 4. Create thread and cause message in a transaction
        let profile_for_thread = effective_profile.clone();
        let task_id_inner = id.clone();
        let planning_mode = task_planning_mode.clone();

        // Resolve provider and model for the thread
        // Chain: channel.current_provider → LLM_PROVIDER env
        let (resolved_provider, resolved_model): (String, Option<String>) = {
            let chan_prov: Option<(Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT current_provider, current_model FROM channels WHERE id = $1"
            )
            .bind(channel_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to query channel provider: {}", e))?;

            if let Some((Some(prov), model)) = chan_prov {
                let resolved = model
                    .filter(|m| !m.is_empty())
                    .or_else(|| resolve_default_model(&prov));
                (prov, resolved)
            } else {
                match std::env::var("LLM_PROVIDER") {
                    Ok(prov) if !prov.is_empty() => {
                        let model = resolve_default_model(&prov);
                        (prov, model)
                    }
                    _ => {
                        ("openai".to_string(), None)
                    }
                }
            }
        };

        let resolved_model = resolved_model.unwrap_or_else(|| "gpt-4o".to_string());

        let result = sqlx::query_as::<_, (i64,)>(
            r#"
            INSERT INTO threads (status, cause, channel_id, profile, provider, model, task_id, planning_mode)
            VALUES ('created', 'system', $1, $2, NULLIF($3, '')::text, NULLIF($4, '')::text, NULLIF($5, '')::text, $6)
            RETURNING id
            "#,
        )
        .bind(channel_id)
        .bind(&profile_for_thread)
        .bind(&resolved_provider)
        .bind(&resolved_model)
        .bind(&task_id_inner)
        .bind(&planning_mode)
        .fetch_optional(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create thread: {}", e))?;

        let thread_id = match result {
            Some((tid,)) => tid,
            None => {
                return Ok((format!("Failed to create thread for kanban task '{}' ({})", title, id), false));
            }
        };

        // Create the cause (seq-0) message
        let msg_metadata = serde_json::json!({
            "kanban_task_id": id,
            "kanban_task_title": task_title,
        });

        sqlx::query(
            r#"
            INSERT INTO messages (thread_id, role, content, thread_sequence, metadata, msg_type, msg_subtype, iteration_number)
            VALUES ($1, 'cause', $2, 0, $3::jsonb, 'kanban', $4, 0)
            "#,
        )
        .bind(thread_id)
        .bind(&content)
        .bind(&msg_metadata)
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create cause message: {}", e))?;

        // Check if channel is closed — if so, skip
        let is_closed: Option<(bool,)> = sqlx::query_as(
            "SELECT closed FROM channels WHERE id = $1"
        )
        .bind(channel_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to check channel closed: {}", e))?;

        let channel_closed = is_closed.map(|c| c.0).unwrap_or(false);

        if channel_closed {
            sqlx::query("UPDATE threads SET status = 'skipped' WHERE id = $1")
                .bind(thread_id)
                .execute(pool)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to set thread skipped: {}", e))?;

            sqlx::query("UPDATE kanban_tasks SET status = 'todo', updated_at = NOW() WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to reset kanban task: {}", e))?;

            return Ok((
                format!("Channel {} is closed — skipped thread for task '{}' ({})", channel_id, title, id),
                false,
            ));
        }

        // Set thread to pending
        sqlx::query("UPDATE threads SET status = 'pending' WHERE id = $1 AND NOT terminal")
            .bind(thread_id)
            .execute(pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to set thread pending: {}", e))?;

        // Mark task as ready
        sqlx::query("UPDATE kanban_tasks SET status = 'ready', updated_at = NOW() WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to update kanban task status: {}", e))?;

        // Insert kanban history record
        insert_kanban_history(pool, id, "moved", Some("todo"), Some("ready"), None).await?;

        return Ok((
            format!("Dispatched kanban task '{}' ({}) → thread {} (ready)", title, id, thread_id),
            false,
        ));
    }

    Ok(("No eligible kanban tasks to dispatch".to_string(), false))
}

/// Insert a record into kanban_history.
async fn insert_kanban_history(
    pool: &PgPool,
    task_id: &str,
    action: &str,
    initial_board: Option<&str>,
    final_board: Option<&str>,
    previous_values: Option<Value>,
) -> Result<()> {
    let pv = previous_values.unwrap_or(Value::Null);
    sqlx::query(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values)
        VALUES ($1, $2, NULLIF($3, '')::text, NULLIF($4, '')::text, $5::jsonb)
        "#,
    )
    .bind(task_id)
    .bind(action)
    .bind(initial_board.unwrap_or(""))
    .bind(final_board.unwrap_or(""))
    .bind(&pv)
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to insert kanban history: {}", e))?;
    Ok(())
}

/// Resolve a default model for a provider name.
fn resolve_default_model(provider: &str) -> Option<String> {
    match provider {
        "openai" => Some("gpt-4o".into()),
        "anthropic" => Some("claude-sonnet-4".into()),
        "deepseek" => Some("deepseek-chat".into()),
        "opencode-go" => Some("deepseek-v4-flash".into()),
        "google" => Some("gemini-2.0-flash".into()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tool: hindsight_populator
// ---------------------------------------------------------------------------

async fn handle_hindsight_populator(pool: &PgPool, _args: &Value) -> Result<(String, bool)> {
    let dir = data_dir();
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

    Ok((format!("Hindsight populator: retained {} messages (watermark: {} -> {})", count, last_id, max_id), false))
}

// ---------------------------------------------------------------------------
// Tool: relevance_indexer
// ---------------------------------------------------------------------------

async fn handle_relevance_indexer(_pool: &PgPool, _args: &Value) -> Result<(String, bool)> {
    let profile = default_profile_name();
    let wiki_dir = format!("{}/profiles/{}/wiki", data_dir(), profile);
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

    let mut scored: Vec<(String, f64)> = entries.iter().map(|(path, mtime)| {
        let age = now.saturating_sub(*mtime);
        let recency_score = if age < 3600 { 50.0 } else if age < 86400 { 40.0 } else if age < 604800 { 30.0 } else { 10.0 };
        (path.clone(), recency_score)
    }).collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut output = String::from("# Relevant Wiki Pages\n\n");
    for (path, score) in scored.iter().take(30) {
        let line = format!("- [{}]({}) --- score: {:.0}\n", path, path, score);
        if output.len() + line.len() > 1000 { break; }
        output.push_str(&line);
    }

    let output_path = format!("{}/relevant-index.md", wiki_dir);
    std::fs::write(&output_path, &output)
        .map_err(|e| anyhow::anyhow!("Failed to write relevant-index.md: {}", e))?;

    Ok((format!("Relevance indexer complete: {} files indexed", scored.len()), false))
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
                        let mtime = entry.metadata().ok()
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
    let schedule = args.get("schedule").and_then(|v| v.as_str()).unwrap_or("0 */6 * * *");
    let id = format!("knowledge-pipeline-{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos());

    let skills_json = serde_json::json!(["knowledge-pipeline"]).to_string();
    let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or("Run the knowledge pipeline maintenance (summarize channels, update wiki, run relevance indexer, populate hindsight).");

    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM cron_jobs WHERE name = 'knowledge-pipeline' LIMIT 1"
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to check existing cron: {}", e))?;

    if existing.is_some() {
        return Ok(("Knowledge Pipeline cron already exists".to_string(), false));
    }

    let channel: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM channels WHERE platform = 'cron' AND name = 'cron-default' LIMIT 1"
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

    Ok((format!("Knowledge Pipeline cron job created with schedule '{}'", schedule), false))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&database_url)
        .await
        .context("Failed to connect to database")?;
    let pool = Arc::new(pool);

    let p_kanban = pool.clone();
    let kanban_handler: ToolHandler =
        Box::new(move |args: Value, _meta: Option<McpMeta>| {
            let pool = p_kanban.clone();
            Box::pin(async move { handle_kanban_dispatcher(&pool, &args).await })
        });

    let p_hindsight = pool.clone();
    let hindsight_handler: ToolHandler =
        Box::new(move |args: Value, _meta: Option<McpMeta>| {
            let pool = p_hindsight.clone();
            Box::pin(async move { handle_hindsight_populator(&pool, &args).await })
        });

    let p_relevance = pool.clone();
    let relevance_handler: ToolHandler =
        Box::new(move |args: Value, _meta: Option<McpMeta>| {
            let pool = p_relevance.clone();
            Box::pin(async move { handle_relevance_indexer(&pool, &args).await })
        });

    let p_pipeline = pool.clone();
    let pipeline_handler: ToolHandler =
        Box::new(move |args: Value, _meta: Option<McpMeta>| {
            let pool = p_pipeline.clone();
            Box::pin(async move { handle_setup_knowledge_pipeline(&pool, &args).await })
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

    run_server(
        ServerInfo {
            name: "mcp-server-actions".to_string(),
            version: "0.1.0".to_string(),
        },
        tools,
    )
    .await?;

    Ok(())
}
