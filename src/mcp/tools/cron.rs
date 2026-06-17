use crate::mcp::{AppContext, McpTool, McpToolResult};
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

pub fn create_cron_job_tool() -> McpTool {
    McpTool {
        name: "create_cron_job".to_string(),
        description: "Create a new cron job. Schedules a recurring task with a cron expression and a prompt to execute.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "A unique name for this cron job (lowercase, hyphens/underscores)"
                },
                "schedule": {
                    "type": "string",
                    "description": "Cron schedule expression (e.g., '0 9 * * 1-5' for weekdays at 9am, '0 */6 * * *' every 6 hours)"
                },
                "prompt": {
                    "type": "string",
                    "description": "The prompt/message to execute when the cron job triggers"
                },
                "skills": {
                    "type": "string",
                    "description": "Optional comma-separated list of skill names to enable for this job"
                }
            },
            "required": ["name", "schedule", "prompt"]
        }),
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let name = args["name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;

            let schedule = args["schedule"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'schedule'"))?;

            let prompt = args["prompt"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'prompt'"))?;

            let skills_str = args["skills"].as_str().unwrap_or("");

            if name.is_empty() {
                anyhow::bail!("Job name must not be empty");
            }
            if schedule.is_empty() {
                anyhow::bail!("Schedule must not be empty");
            }
            if prompt.is_empty() {
                anyhow::bail!("Prompt must not be empty");
            }

            // Generate a unique ID
            let id = format!("cron_{:x}", {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            });

            // Parse skills as JSON array
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

            let pool = ctx.pool.clone();

            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    sqlx::query(
                        r#"
                        INSERT INTO cron_jobs (id, name, schedule, prompt, skills)
                        VALUES ($1, $2, $3, $4, $5::text::jsonb)
                        "#,
                    )
                    .bind(&id)
                    .bind(name)
                    .bind(schedule)
                    .bind(prompt)
                    .bind(skills_json.to_string())
                    .execute(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to create cron job: {}", e))?;

                    Ok::<_, anyhow::Error>(())
                })
            })?;

            Ok(McpToolResult {
                call_id: String::new(),
                content: format!(
                    "Cron job '{}' created with id '{}', schedule '{}'",
                    name, id, schedule
                ),
                is_error: false,
            })
        }),
    }
}

pub fn list_cron_jobs_tool() -> McpTool {
    McpTool {
        name: "list_cron_jobs".to_string(),
        description: "List all cron jobs with their schedule, status, and last/next run times.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {}
        }),
        handler: Arc::new(|_: Value, ctx: AppContext| -> Result<McpToolResult> {
            let pool = ctx.pool.clone();

            let rows = tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    sqlx::query_as::<_, (String, String, String, String, bool, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>, chrono::DateTime<chrono::Utc>)>(
                        "SELECT id, name, schedule, prompt, enabled, last_run_at, next_run_at, created_at FROM cron_jobs ORDER BY created_at DESC"
                    )
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to list cron jobs: {}", e))
                })
            })?;

            let jobs: Vec<serde_json::Value> = rows
                .into_iter()
                .map(
                    |(id, name, schedule, prompt, enabled, last_run_at, next_run_at, created_at)| {
                        serde_json::json!({
                            "id": id,
                            "name": name,
                            "schedule": schedule,
                            "prompt_preview": if prompt.len() > 100 {
                                format!("{}...", &prompt[..100])
                            } else {
                                prompt.clone()
                            },
                            "enabled": enabled,
                            "last_run_at": last_run_at,
                            "next_run_at": next_run_at,
                            "created_at": created_at.to_rfc3339(),
                        })
                    },
                )
                .collect();

            let output = serde_json::to_string_pretty(&serde_json::json!({ "jobs": jobs }))?;
            Ok(McpToolResult {
                call_id: String::new(),
                content: output,
                is_error: false,
            })
        }),
    }
}

pub fn delete_cron_job_tool() -> McpTool {
    McpTool {
        name: "delete_cron_job".to_string(),
        description: "Delete a cron job by its ID.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Cron job ID to delete"
                }
            },
            "required": ["id"]
        }),
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let id = args["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'id'"))?;

            let pool = ctx.pool.clone();
            let id_clone = id.to_string();

            let deleted = tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    sqlx::query("DELETE FROM cron_jobs WHERE id = $1")
                        .bind(&id_clone)
                        .execute(&pool)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to delete cron job: {}", e))
                })
            })?;

            if deleted.rows_affected() == 0 {
                anyhow::bail!("Cron job '{}' not found", id);
            }

            Ok(McpToolResult {
                call_id: String::new(),
                content: format!("Cron job '{}' deleted successfully", id),
                is_error: false,
            })
        }),
    }
}
