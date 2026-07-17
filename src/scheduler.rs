//! Cron scheduler: polls `cron_jobs` table and fires due jobs by creating
//! threads with cause='cron' and a cause message, then setting them pending
//! for the executor to pick up.
//!
//! The scheduler runs as a background tokio task, polling every 30 seconds.
//! Concurrency is enforced atomically at the DB level:
//! - Job is claimed with `UPDATE ... WHERE NOT running`
//! - If 0 rows affected, another tick already claimed it → skip
//! - After firing, `running` is cleared and timestamps updated

use crate::err_msg;
use crate::error::{AppResult, Error};
use chrono::{DateTime, Utc};
use cron::Schedule;
use sql_forge::sql_forge;
use sqlx::FromRow;
use sqlx::PgPool;
use std::str::FromStr;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

use crate::db::types as queries;
use crate::mcp::{AppContext, McpRegistry, McpToolCall};

#[derive(Debug, FromRow)]
struct CronJobDueRow {
    id: String,
    name: Option<String>,
    display_name: String,
    schedule: String,
    prompt: Option<String>,
    channel_id: Option<i64>,
    profile: Option<String>,
    mode: Option<String>,
    action_id: Option<String>,
    silent: Option<bool>,
    template: Option<String>,
    plan: Option<bool>,
}

/// Spawn the cron scheduler loop as a background task.
pub fn spawn(
    pool: PgPool,
    data_dir: String,
    mcp_registry: Arc<tokio::sync::RwLock<McpRegistry>>,
    app_context: AppContext,
) -> tokio::task::JoinHandle<()> {
    // Clear stale running flags from previous process life (crash/restart)
    let pool2 = pool.clone();
    tokio::spawn(async move {
        match sql_forge!(
            "UPDATE cron_jobs SET running = false, updated_at = NOW() WHERE running = true"
        )
        .execute(&pool2)
        .await
        {
            Ok(res) => {
                if res.rows_affected() > 0 {
                    info!(
                        "[cron-scheduler] Cleared {} stale running flag(s) from previous process life",
                        res.rows_affected()
                    );
                }
            }
            Err(e) => error!(
                "[cron-scheduler] Failed to clear stale running flags: {:?}",
                e
            ),
        }
    });

    tokio::spawn(async move {
        info!("[cron-scheduler] Starting cron scheduler loop");

        loop {
            if let Err(e) = tick(&pool, &data_dir, &mcp_registry, &app_context).await {
                error!("[cron-scheduler] Tick failed: {:?}", e);
            }
            sleep(Duration::from_secs(30)).await;
        }
    })
}

/// One tick: find due jobs, claim one atomically, fire it, release.
async fn tick(
    pool: &PgPool,
    data_dir: &str,
    mcp_registry: &Arc<tokio::sync::RwLock<McpRegistry>>,
    app_context: &AppContext,
) -> AppResult<()> {
    let jobs = fetch_due_jobs(pool).await?;

    for job in jobs {
        let now = Utc::now();
        let display_name = if job.display_name.is_empty() {
            job.name.as_deref().unwrap_or("cron-job")
        } else {
            &job.display_name
        };

        // ── Atomic claim: only one tick can claim this job ──
        let claimed = sql_forge!(
            r#"
            UPDATE cron_jobs
            SET running = true,
                updated_at = NOW()
            WHERE id = :id
              AND (
                NOT running
                OR (running = true AND updated_at <= NOW() - INTERVAL '10 minutes')
              )
            "#,
            ( :id = &job.id )
        )
        .execute(pool)
        .await?;

        if claimed.rows_affected() == 0 {
            info!(
                "[cron-scheduler] Job '{}' already claimed by another process, skipping",
                display_name
            );
            continue;
        }

        info!(
            "[cron-scheduler] Firing job '{}' (id={})",
            display_name, job.id
        );

        // ── Validate 5-field cron format ──
        if !validate_cron_schedule_5field(&job.schedule) {
            warn!(
                "[cron-scheduler] Job '{}' has invalid cron schedule '{}': expected 5 fields (min hour dom month dow), got {} fields. Job will be skipped.",
                display_name, job.schedule, job.schedule.split_whitespace().count()
            );
            let new_next = calculate_next_run(&job.schedule, &now);
            release_job(pool, &job.id, &now, &new_next).await?;
            continue;
        }

        // ── Check mode and silent flags ──
        let is_action = job.mode.as_deref() == Some("action");
        let is_silent = job.silent.unwrap_or(false);

        if is_action {
            // Action mode: execute the MCP tool directly via the registry.
            // Non-silent: creates a system thread with the result message.
            // Silent: executes silently, only creates a thread on failure.
            handle_action_mode(ActionModeCtx {
                pool,
                data_dir,
                mcp_registry,
                app_context,
                job: &job,
                display_name,
                now: &now,
                cause: "system",
            })
            .await;

            // Release the job claim and update timestamps
            let new_next = calculate_next_run(&job.schedule, &now);
            release_job(pool, &job.id, &now, &new_next).await?;
            continue;
        }

        if is_silent {
            // Silent (non-action) mode: no thread created, no messages saved.
            // Cannot execute an agentic prompt without a thread, so just release.
            info!(
                "[cron-scheduler] Silent job '{}' fired (no thread created for non-action silent job)",
                display_name
            );
            let new_next = calculate_next_run(&job.schedule, &now);
            release_job(pool, &job.id, &now, &new_next).await?;
            continue;
        }

        // ── Determine which channel to fire into ──
        let channel = if let Some(cid) = job.channel_id {
            match queries::find_channel_by_id(pool, cid).await {
                Ok(Some(ch)) => ch,
                _ => {
                    error!(
                        "[cron-scheduler] Job '{}' references channel_id {} which doesn't exist, falling back to default cron channel",
                        display_name, cid
                    );
                    ensure_cron_channel(pool).await?
                }
            }
        } else {
            ensure_cron_channel(pool).await?
        };

        // Resolve the profile for this message
        let profile_name = if let Some(ref p) = job.profile {
            p.clone()
        } else {
            channel.current_profile.clone()
        };

        // Resolve provider+model for stamping on the thread
        let profile_registry = crate::profile::ProfileRegistry::new(data_dir);
        let prof = profile_registry
            .get(&profile_name)
            .cloned()
            .unwrap_or_else(|| {
                let default_name = &profile_registry.default_profile;
                crate::profile::Profile::default(default_name)
            });

        // Use the shared resolution function for provider and model
        let resolved = resolve_thread_config(
            job.profile.as_deref(),
            &channel.current_profile,
            channel.current_provider.as_deref(),
            channel.current_model.as_deref(),
            prof.provider.as_deref(),
            prof.model.as_deref(),
        );
        let (provider, model) = match resolved {
            Some(cfg) => (Some(cfg.provider), Some(cfg.model)),
            None => (None, None),
        };

        // ── Create a thread with cause='system' (resolves planning mode internally) ──
        let subtype = job.name.clone().unwrap_or_default();
        let prompt_content = job.prompt.clone().unwrap_or_default();
        match queries::create_thread_with_cause(
            pool,
            data_dir,
            "system",
            channel.id,
            &profile_name,
            queries::ThreadCauseParams {
                provider,
                model,
                task_id: None,
                schedule_task_id: Some(job.id.clone()),
                content: prompt_content,
                external_id: Some(format!("cron:{}:{}", job.id, now.timestamp())),
                metadata: serde_json::json!({
                    "cron_job_id": job.id,
                    "cron_job_name": job.name,
                    "cron_display_name": display_name,
                    "scheduled_at": job.schedule,
                    "channel_id": channel.id,
                    "profile": profile_name,
                    "template": job.template.clone().filter(|t| !t.is_empty()).or_else(|| channel.template.clone()).unwrap_or_default(),
                }),
                msg_type: "cron".to_string(),
                msg_subtype: Some(subtype),
                task_plan: job.plan,
                parent_external_id: None,
            },
        )
        .await
        {
            Ok((thread, created)) => {
                info!(
                    "[cron-scheduler] Created thread {} / cause message {} for job '{}'",
                    thread.id, created.id, display_name
                );
            }
            Err(e) => {
                error!(
                    "[cron-scheduler] Failed to create thread for job '{}': {:?}",
                    display_name, e
                );
                let new_next = calculate_next_run(&job.schedule, &now);
                release_job(pool, &job.id, &now, &new_next).await?;
                continue;
            }
        }

        // ── Set thread status to 'pending' so the executor picks it up ──
        // (now handled inside create_cause_and_set_pending)

        // ── Release claim and update timestamps ──
        let new_next = calculate_next_run(&job.schedule, &now);
        release_job(pool, &job.id, &now, &new_next).await?;
    }

    Ok(())
}

/// Fetch enabled jobs whose next_run_at is due (null or ≤ now).
async fn fetch_due_jobs(pool: &PgPool) -> AppResult<Vec<CronJobDueRow>> {
    let rows: Vec<CronJobDueRow> = sql_forge!(
        CronJobDueRow,
        r#"
        SELECT id, name, display_name, schedule, prompt, channel_id, profile, mode, action_id, silent, template, plan
        FROM cron_jobs
        WHERE enabled = true
          AND active = true
          AND (next_run_at IS NULL OR next_run_at <= NOW())
          AND 1 = :_one
        ORDER BY created_at ASC
        "#,
        ( :_one = 1i32 )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Release the running flag and update timestamps.
async fn release_job(
    pool: &PgPool,
    job_id: &str,
    last_run: &DateTime<Utc>,
    next_run: &DateTime<Utc>,
) -> AppResult<()> {
    sql_forge!(
        r#"
        UPDATE cron_jobs
        SET running = false,
            last_run_at = :last_run,
            next_run_at = :next_run,
            updated_at = NOW()
        WHERE id = :id
        "#,
        ( :last_run = *last_run, :next_run = *next_run, :id = job_id )
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Ensure a cron channel exists (upsert on conflict).
async fn ensure_cron_channel(pool: &PgPool) -> AppResult<crate::db::types::Channel> {
    // First try to find existing cron channel
    if let Ok(Some(ch)) =
        queries::get_channel_by_platform_and_resource(pool, "cron", "cron-session").await
    {
        return Ok(ch);
    }
    queries::create_channel(
        pool,
        queries::CreateChannelParams {
            name: "cron-session".to_string(),
            platform: "cron".to_string(),
            external_id: "cron-default".to_string(),
            cause: "cron".to_string(),
            resource_identifier: "cron-session".to_string(),
        },
    )
    .await
    .map_err(|e| Error::Message(format!("Failed to create cron channel: {:#}", e)))
}
/// Parse a cron expression and compute the next run after `now`.
/// The expression is expected in 5-field Linux format (min hour day month weekday).
/// We prepend "0 " (second=0) to convert to 6-field for the `cron` crate.
fn calculate_next_run(expression: &str, now: &DateTime<Utc>) -> DateTime<Utc> {
    let cron_expr = format!("0 {}", expression);
    match Schedule::from_str(&cron_expr) {
        Ok(schedule) => {
            if let Some(next) = schedule.after(now).next() {
                next
            } else {
                *now + chrono::Duration::hours(1)
            }
        }
        Err(e) => {
            warn!("Invalid cron expression '{}': {}", expression, e);
            *now + chrono::Duration::hours(1)
        }
    }
}

/// Validate that a cron schedule expression has exactly 5 whitespace-separated fields
/// (Linux crontab format: minute hour day-of-month month day-of-week).
/// Returns `true` if valid, `false` otherwise.
pub fn validate_cron_schedule_5field(schedule: &str) -> bool {
    let trimmed = schedule.trim();
    if trimmed.is_empty() {
        return false;
    }
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    fields.len() == 5
}

/// Resolve an action_id to an `McpToolCall` by loading {data_dir}/actions.yml.
/// Looks up the action entry and extracts tool_name + params. Returns an error
/// if the action is not found, disabled, or the file can't be read.
fn resolve_action(data_dir: &str, action_id: &str) -> AppResult<McpToolCall> {
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(Debug, Deserialize)]
    struct ActionsFile {
        actions: HashMap<String, ActionEntry>,
    }

    #[derive(Debug, Deserialize, Clone)]
    struct ActionEntry {
        enabled: bool,
        tool_name: String,
        #[serde(default)]
        params: serde_json::Value,
    }

    let path = format!("{}/actions.yml", data_dir);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| Error::Message(format!("Failed to read actions.yml: {}", e)))?;

    let file: ActionsFile = serde_yaml::from_str(&content)
        .map_err(|e| Error::Message(format!("Failed to parse actions.yml: {}", e)))?;

    let entry = file
        .actions
        .get(action_id)
        .ok_or_else(|| Error::ActionNotFound(action_id.to_string()))?;

    if !entry.enabled {
        return Err(Error::Message(format!(
            "Action '{}' is disabled",
            action_id
        )));
    }

    Ok(McpToolCall {
        id: format!("cron-action-{}", action_id),
        name: entry.tool_name.clone(),
        arguments: entry.params.clone(),
    })
}

// ─── Action mode helpers ────────────────────────────────────────────────────

/// Context for `handle_action_mode`: groups 8 params to stay under clippy's 7-arg limit.
struct ActionModeCtx<'a> {
    pool: &'a PgPool,
    data_dir: &'a str,
    mcp_registry: &'a Arc<tokio::sync::RwLock<McpRegistry>>,
    app_context: &'a AppContext,
    job: &'a CronJobDueRow,
    display_name: &'a str,
    now: &'a DateTime<Utc>,
    cause: &'a str,
}

/// Handle action mode cron job execution.
///
/// For non-silent jobs: executes the tool and creates a system thread
/// with the result. For silent jobs: executes silently, only creates
/// a thread on failure. Returns the thread_id if one was created.
async fn handle_action_mode(ctx: ActionModeCtx<'_>) -> Option<i64> {
    let is_silent = ctx.job.silent.unwrap_or(false);

    let action_id = match ctx.job.action_id {
        Some(ref id) => id.clone(),
        None => {
            error!(
                "[cron-action] Job '{}' has mode=action but no action_id set, skipping",
                ctx.display_name
            );
            return None;
        }
    };

    // Resolve the tool call from actions.yml
    let tool_call = match resolve_action(ctx.data_dir, &action_id) {
        Ok(tc) => tc,
        Err(e) => {
            error!(
                "[cron-action] Failed to resolve action '{}' for job '{}': {}",
                action_id, ctx.display_name, e
            );
            return None;
        }
    };

    info!(
        "[cron-action] Executing action job '{}' (tool: {}, action_id: {})",
        ctx.display_name, tool_call.name, action_id
    );

    // Execute the tool first, THEN create the thread with the result.
    // This avoids the executor picking up a pending thread before it's terminal.
    // Snapshot the registry under the lock; tokio::sync::RwLockReadGuard is Send.
    let mcp_snapshot = ctx.mcp_registry.read().await.clone();
    match mcp_snapshot
        .execute(&tool_call, ctx.app_context.clone())
        .await
    {
        Ok(result) => {
            let is_error = result.is_error;

            if is_error {
                error!(
                    "[cron-action] Action job '{}' (action_id={}) returned error: {}",
                    ctx.display_name, action_id, result.content
                );
            } else if !is_silent {
                info!(
                    "[cron-action] Action job '{}' (action_id={}) completed successfully",
                    ctx.display_name, action_id
                );
            }

            // Create thread if non-silent (always) OR silent with error
            if !is_silent || is_error {
                match create_action_thread(ActionThreadCtx {
                    pool: ctx.pool,
                    data_dir: ctx.data_dir,
                    job: ctx.job,
                    now: ctx.now,
                    display_name: ctx.display_name,
                    result_content: &result.content,
                    is_error,
                    cause: ctx.cause,
                })
                .await
                {
                    Ok(tid) => Some(tid),
                    Err(e) => {
                        error!(
                            "[cron-action] Failed to create action result thread: {:?}",
                            e
                        );
                        None
                    }
                }
            } else {
                // Silent success: no thread, no messages
                None
            }
        }
        Err(e) => {
            error!(
                "[cron-action] Action job '{}' (action_id={}) execution failed: {}",
                ctx.display_name, action_id, e
            );

            // Always create a failure thread for visible error trail
            let err_content = format!("Action execution failed: {}", e);
            match create_action_thread(ActionThreadCtx {
                pool: ctx.pool,
                data_dir: ctx.data_dir,
                job: ctx.job,
                now: ctx.now,
                display_name: ctx.display_name,
                result_content: &err_content,
                is_error: true,
                cause: ctx.cause,
            })
            .await
            {
                Ok(tid) => Some(tid),
                Err(e2) => {
                    error!(
                        "[cron-action] Failed to create action failure thread: {:?}",
                        e2
                    );
                    None
                }
            }
        }
    }
}

/// Context for `create_action_thread`: groups 8 params to stay under clippy's 7-arg limit.
struct ActionThreadCtx<'a> {
    pool: &'a PgPool,
    data_dir: &'a str,
    job: &'a CronJobDueRow,
    now: &'a DateTime<Utc>,
    display_name: &'a str,
    result_content: &'a str,
    is_error: bool,
    cause: &'a str,
}

/// Create a system/user thread with the action result saved as a message.
///
/// Creates a thread with the given cause ('system' for scheduled, 'user'
/// for manual run), a seq-0 cause message (msg_type='cron', msg_subtype
/// = cron job name), saves the tool result as a seq-1 message, then
/// marks the thread as terminal (system for success, failed for error).
async fn create_action_thread(ctx: ActionThreadCtx<'_>) -> AppResult<i64> {
    // Resolve the channel the same way as the agentic mode path
    let channel = if let Some(cid) = ctx.job.channel_id {
        match queries::find_channel_by_id(ctx.pool, cid).await {
            Ok(Some(ch)) => ch,
            _ => ensure_cron_channel(ctx.pool).await?,
        }
    } else {
        ensure_cron_channel(ctx.pool).await?
    };

    // Resolve profile
    let profile_name = if let Some(ref p) = ctx.job.profile {
        p.clone()
    } else {
        channel.current_profile.clone()
    };

    let subtype = ctx.job.name.clone().unwrap_or_default();
    let prompt_content = format!("Cron: {}", ctx.display_name);

    // Create the thread with the given cause and a seq-0 cause message (msg_type='cron')
    let (thread, _cause_msg) = queries::create_thread_with_cause(
        ctx.pool,
        ctx.data_dir,
        ctx.cause,
        channel.id,
        &profile_name,
        queries::ThreadCauseParams {
            provider: None,
            model: None,
            task_id: None,
            schedule_task_id: Some(ctx.job.id.clone()),
            content: prompt_content,
            external_id: Some(format!("cron:{}:{}", ctx.job.id, ctx.now.timestamp())),
            metadata: serde_json::json!({
                "cron_job_id": ctx.job.id,
                "cron_job_name": ctx.job.name,
                "cron_display_name": ctx.display_name,
                "scheduled_at": ctx.job.schedule,
                "channel_id": channel.id,
                "profile": profile_name,
                "template": ctx.job.template.clone().filter(|t| !t.is_empty()).or_else(|| channel.template.clone()).unwrap_or_default(),
            }),
            msg_type: "cron".to_string(),
            msg_subtype: Some(subtype),
            task_plan: ctx.job.plan,
            parent_external_id: None,
        },
    )
    .await?;

    // Save the tool result as a seq-1 message (role='agent', msg_type='tool-result')
    let result_msg = queries::MessageNew {
        thread_id: thread.id,
        role: "agent".to_string(),
        content: ctx.result_content.to_string(),
        thread_sequence: 1,
        external_id: Some(format!(
            "cron:{}:{}:result",
            ctx.job.id,
            ctx.now.timestamp()
        )),
        metadata: serde_json::json!({
            "cron_job_id": ctx.job.id,
            "is_error": ctx.is_error,
        }),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "tool-result".to_string(),
        msg_subtype: None,
        iteration_number: 0,
    };
    let _ = queries::create_message(ctx.pool, &result_msg).await;

    // Mark thread as terminal (system for success, failed for error)
    if ctx.is_error {
        queries::set_thread_failed(ctx.pool, thread.id).await?;
        info!(
            "[cron-action] Created failure thread {} for action job '{}'",
            thread.id, ctx.display_name
        );
    } else {
        queries::set_thread_system(ctx.pool, thread.id).await?;
        info!(
            "[cron-action] Created result thread {} for action job '{}'",
            thread.id, ctx.display_name
        );
    }

    Ok(thread.id)
}

// ─── Resolved thread config ───

/// Resolved profile, provider, and model for thread creation.
/// The chain is: explicit → channel → profile → env fallback.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedThreadConfig {
    pub profile_name: String,
    pub provider: String,
    pub model: String,
}

/// Resolve the profile, provider, and model for a thread using the chain:
///    profile_name: task/override → channel.current_profile
///    provider:     channel.current_provider → profile.provider → LLM_PROVIDER env
///    model:        derived from where the provider came from:
///                    channel level → channel.current_model or provider default_model
///                    profile level → profile.model or provider default_model
///                    env var level → always provider default_model
///
/// Returns `None` when the resolved profile name is empty, or no model resolved.
pub(crate) fn resolve_thread_config(
    explicit_profile: Option<&str>,
    channel_profile: &str,
    channel_provider: Option<&str>,
    channel_model: Option<&str>,
    profile_provider: Option<&str>,
    profile_model: Option<&str>,
) -> Option<ResolvedThreadConfig> {
    let profile_name = explicit_profile
        .filter(|s| !s.is_empty())
        .unwrap_or(channel_profile)
        .to_string();

    if profile_name.is_empty() {
        return None;
    }

    // Provider chain: channel → profile → LLM_PROVIDER env
    // Model depends on where provider came from:
    //   channel → channel model or provider default
    //   profile → profile model or provider default
    //   env     → provider default
    let (provider, model) = {
        // Channel level
        if let Some(prov) = channel_provider.filter(|s| !s.is_empty()) {
            let m = channel_model
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .or_else(|| crate::llm::resolve_default_model(prov));
            (prov.to_string(), m)
        }
        // Profile level
        else if let Some(prov) = profile_provider.filter(|s| !s.is_empty()) {
            let m = profile_model
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .or_else(|| crate::llm::resolve_default_model(prov));
            (prov.to_string(), m)
        }
        // Global config level: default_provider from settings.yml
        else {
            let default_provider = crate::agent::config::get_global()
                .map(|g| g.read().unwrap().default_provider.clone())
                .unwrap_or_else(|| "openai".to_string());
            if !default_provider.is_empty() {
                let m = crate::llm::resolve_default_model(&default_provider);
                (default_provider, m)
            } else {
                return None;
            }
        }
    };

    let model = model?; // Return None if no model resolved

    Some(ResolvedThreadConfig {
        profile_name,
        provider,
        model,
    })
}

/// Extract an error code from an error message string we generated.
/// Only matches our own error patterns: "error (<code>):"
///
/// Examples that match:
///   "MCP tool call error (-32603): Internal error" → "-32603"
///   "MCP initialize error (0): ..."                 → "0"
///   "Plugin 'name' initialize error (-1): Failed"   → "-1"
fn extract_error_code(err_msg: &str) -> Option<String> {
    if let Some(start) = err_msg.find("error (") {
        let after = &err_msg[start + 7..];
        let code: String = after
            .chars()
            .take_while(|c| *c == '-' || c.is_ascii_digit())
            .collect();
        if !code.is_empty() {
            return Some(code);
        }
    }
    None
}

/// Fire a cron job by schedule_id: used by the HTTP run-cron endpoint.
/// This reuses the same scheduler logic (channel resolution, profile/provider/model resolution,
/// thread creation) so the manual Run button goes through exactly the same code path as the
/// scheduled tick.
pub async fn fire_cron_job_by_id(
    pool: &PgPool,
    data_dir: &str,
    mcp_registry: &Arc<tokio::sync::RwLock<McpRegistry>>,
    app_context: &AppContext,
    schedule_id: &str,
    force: bool,
) -> AppResult<Option<i64>> {
    let jobs: Vec<CronJobDueRow> = sql_forge!(
        CronJobDueRow,
        r#"
        SELECT id, name, display_name, schedule, prompt, channel_id, profile, mode, action_id, silent, template, plan
        FROM cron_jobs
        WHERE id = :id
        LIMIT 1
        "#,
        ( :id = schedule_id )
    )
    .fetch_all(pool)
    .await?;

    let job = jobs
        .into_iter()
        .next()
        .ok_or_else(|| Error::Message(format!("Cron job '{}' not found", schedule_id)))?;

    // Check active status (skip if force)
    let active: bool = sql_forge!(
        scalar bool,
        r#"SELECT active FROM cron_jobs WHERE id = :id"#,
        ( :id = schedule_id )
    )
    .fetch_one(pool)
    .await?;

    if !active && !force {
        err_msg!(
            "Job '{}' is not active. Use force=true to run anyway.",
            schedule_id
        );
    }

    // Validate 5-field cron format
    if !validate_cron_schedule_5field(&job.schedule) {
        let j_name = job.display_name.as_str();
        err_msg!(
            "Invalid cron schedule '{}' for job '{}': expected exactly 5 fields (min hour dom month dow), got {} fields. Use standard Linux crontab format, e.g. '0 9 * * 1-5' for weekdays at 9am.",
            job.schedule, j_name, job.schedule.split_whitespace().count()
        );
    }

    let now = Utc::now();
    let display_name = if job.display_name.is_empty() {
        job.name.as_deref().unwrap_or("cron-job")
    } else {
        &job.display_name
    };

    // ── Handle mode='action' ──
    if job.mode.as_deref() == Some("action") {
        let tid = handle_action_mode(ActionModeCtx {
            pool,
            data_dir,
            mcp_registry,
            app_context,
            job: &job,
            display_name,
            now: &now,
            cause: "user",
        })
        .await;
        return Ok(tid);
    }

    let is_silent = job.silent.unwrap_or(false);
    if is_silent {
        // Silent (non-action) mode: no thread created, no messages saved.
        info!(
            "[cron-run] Silent job '{}' fired (no thread created for non-action silent job)",
            display_name
        );
        return Ok(None);
    }

    // Standard agentic mode: same logic as the scheduler tick
    let channel = if let Some(cid) = job.channel_id {
        match queries::find_channel_by_id(pool, cid).await {
            Ok(Some(ch)) => ch,
            _ => ensure_cron_channel(pool).await?,
        }
    } else {
        ensure_cron_channel(pool).await?
    };

    let profile_name = if let Some(ref p) = job.profile {
        p.clone()
    } else {
        channel.current_profile.clone()
    };

    let profile_registry = crate::profile::ProfileRegistry::new(data_dir);
    let prof = profile_registry
        .get(&profile_name)
        .cloned()
        .unwrap_or_else(|| {
            let default_name = &profile_registry.default_profile;
            crate::profile::Profile::default(default_name)
        });

    let resolved = resolve_thread_config(
        job.profile.as_deref(),
        &channel.current_profile,
        channel.current_provider.as_deref(),
        channel.current_model.as_deref(),
        prof.provider.as_deref(),
        prof.model.as_deref(),
    );
    let (provider, model) = match resolved {
        Some(cfg) => (Some(cfg.provider), Some(cfg.model)),
        None => (None, None),
    };

    let subtype = job.name.clone().unwrap_or_default();
    let prompt_content = job.prompt.clone().unwrap_or_default();
    let (thread, _created) = queries::create_thread_with_cause(
        pool,
        data_dir,
        "user",
        channel.id,
        &profile_name,
        queries::ThreadCauseParams {
            provider,
            model,
            task_id: None,
            schedule_task_id: Some(job.id.clone()),
            content: prompt_content,
            external_id: Some(format!("cron:{}:{}", job.id, now.timestamp())),
            metadata: serde_json::json!({
                "cron_job_id": job.id,
                "cron_job_name": job.name,
                "cron_display_name": display_name,
                "scheduled_at": job.schedule,
                "channel_id": channel.id,
                "profile": profile_name,
                "template": job.template.clone().filter(|t| !t.is_empty()).or_else(|| channel.template.clone()).unwrap_or_default(),
            }),
            msg_type: "cron".to_string(),
            msg_subtype: Some(subtype),
            task_plan: job.plan,
            parent_external_id: None,
        },
    )
    .await?;

    info!(
        "[cron-run] Created thread {} for job '{}' (manual run)",
        thread.id, display_name
    );

    Ok(Some(thread.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    // ─── Profile resolution ───

    #[test]
    fn test_profile_from_task() {
        let cfg = resolve_thread_config(
            Some("task-profile"),
            "channel-profile",
            Some("deepseek"),
            Some("deepseek-v4-flash"),
            None,
            None,
        );
        assert_eq!(cfg.unwrap().profile_name, "task-profile");
    }

    #[test]
    fn test_profile_from_channel_when_task_none() {
        let cfg = resolve_thread_config(
            None,
            "channel-profile",
            Some("deepseek"),
            Some("deepseek-v4-flash"),
            None,
            None,
        );
        assert_eq!(cfg.unwrap().profile_name, "channel-profile");
    }

    #[test]
    fn test_profile_from_channel_when_task_empty() {
        let cfg = resolve_thread_config(
            Some(""),
            "channel-profile",
            Some("deepseek"),
            Some("deepseek-v4-flash"),
            None,
            None,
        );
        assert_eq!(cfg.unwrap().profile_name, "channel-profile");
    }

    #[test]
    fn test_profile_empty_returns_none() {
        let cfg = resolve_thread_config(None, "", None, None, None, None);
        assert!(cfg.is_none());
    }

    #[test]
    fn test_profile_empty_channel_with_empty_task_returns_none() {
        let cfg = resolve_thread_config(Some(""), "", None, None, None, None);
        assert!(cfg.is_none());
    }

    // ─── Provider resolution ───

    #[test]
    fn test_provider_from_channel() {
        let cfg = resolve_thread_config(
            None,
            "default",
            Some("deepseek"),
            Some("deepseek-v4-flash"),
            Some("anthropic"),
            None,
        );
        assert_eq!(cfg.unwrap().provider, "deepseek");
    }

    #[test]
    fn test_provider_falls_back_to_profile() {
        let cfg = resolve_thread_config(
            None,
            "default",
            None,
            None,
            Some("anthropic"),
            Some("claude-sonnet-4"),
        );
        assert_eq!(cfg.unwrap().provider, "anthropic");
    }

    #[test]
    fn test_provider_skip_empty_channel() {
        let cfg = resolve_thread_config(
            None,
            "default",
            Some(""),
            None,
            Some("anthropic"),
            Some("claude-sonnet-4"),
        );
        assert_eq!(cfg.unwrap().provider, "anthropic");
    }

    #[test]
    fn test_provider_channel_overrides_profile() {
        let cfg = resolve_thread_config(
            None,
            "default",
            Some("deepseek"),
            Some("deepseek-v4-flash"),
            Some("anthropic"),
            None,
        );
        assert_eq!(cfg.unwrap().provider, "deepseek");
    }

    // ─── Model resolution ───

    #[test]
    fn test_model_from_channel() {
        let cfg = resolve_thread_config(
            None,
            "default",
            Some("deepseek"),
            Some("deepseek-v4-flash"),
            Some("anthropic"),
            Some("claude-3"),
        );
        assert_eq!(cfg.unwrap().model, "deepseek-v4-flash");
    }

    #[test]
    fn test_model_falls_back_to_profile() {
        let cfg = resolve_thread_config(
            None,
            "default",
            None,
            None,
            Some("anthropic"),
            Some("claude-3"),
        );
        assert_eq!(cfg.unwrap().model, "claude-3");
    }

    #[test]
    fn test_model_channel_overrides_profile() {
        let cfg = resolve_thread_config(
            None,
            "default",
            Some("deepseek"),
            Some("deepseek-v4-flash"),
            Some("anthropic"),
            Some("claude-3"),
        );
        assert_eq!(cfg.unwrap().model, "deepseek-v4-flash");
    }

    #[test]
    fn test_model_skip_empty_channel() {
        let cfg = resolve_thread_config(
            None,
            "default",
            None,
            Some(""),
            Some("anthropic"),
            Some("claude-3"),
        );
        assert_eq!(cfg.unwrap().model, "claude-3");
    }

    // ─── Combined scenarios ───

    #[test]
    fn test_full_resolution_chain() {
        let cfg = resolve_thread_config(
            Some("my-profile"),
            "channel-profile",
            Some("deepseek"),
            Some("deepseek-v4-flash"),
            None,
            None,
        );
        let c = cfg.unwrap();
        assert_eq!(c.profile_name, "my-profile");
        assert_eq!(c.provider, "deepseek");
        assert_eq!(c.model, "deepseek-v4-flash");
    }

    #[test]
    fn test_full_fallback_all_from_channel() {
        let cfg = resolve_thread_config(
            None,
            "chan-profile",
            Some("deepseek"),
            Some("deepseek-v4-flash"),
            None,
            None,
        );
        let c = cfg.unwrap();
        assert_eq!(c.profile_name, "chan-profile");
        assert_eq!(c.provider, "deepseek");
        assert_eq!(c.model, "deepseek-v4-flash");
    }

    #[test]
    fn test_full_fallback_all_from_profile() {
        let cfg = resolve_thread_config(
            None,
            "prof-profile",
            None,
            None,
            Some("anthropic"),
            Some("claude-3"),
        );
        let c = cfg.unwrap();
        assert_eq!(c.profile_name, "prof-profile");
        assert_eq!(c.provider, "anthropic");
        assert_eq!(c.model, "claude-3");
    }

    #[test]
    fn test_provider_and_model_fallthrough_together() {
        // Both come from channel, ignoring profile values
        let cfg = resolve_thread_config(
            None,
            "default",
            Some("deepseek"),
            Some("deepseek-v4-flash"),
            Some("anthropic"),
            Some("claude-3"),
        );
        let c = cfg.unwrap();
        assert_eq!(c.provider, "deepseek");
        assert_eq!(c.model, "deepseek-v4-flash");
    }

    // ─── Agent validation scenario: thread without provider/model ───

    #[test]
    fn test_thread_without_provider_rejected() {
        // Simulates the scenario that caused the bug:
        // thread created with provider=None, model=None should be rejected
        // resolve_thread_config now returns None when it cannot resolve
        // a provider and model at any level (channel → profile → env)
        let cfg = resolve_thread_config(None, "default", None, None, None, None);
        assert!(
            cfg.is_none(),
            "should return None when no provider/model can be resolved"
        );
    }

    // ─── calculate_next_run ───────────────────────────────────────────────

    #[test]
    fn test_calculate_next_run_valid() {
        let now = Utc::now();
        // Standard cron: every 5 minutes
        let next = calculate_next_run("*/5 * * * *", &now);
        assert!(next > now, "next run must be after now");
        // Must produce a value different from the invalid-fallback (now + 1h)
        let fallback = now + chrono::Duration::hours(1);
        assert!(
            next < fallback,
            "next run for */5 should be within the hour, not fallback"
        );
        let diff = next - now;
        assert!(
            diff.num_seconds() > 0 && diff.num_seconds() <= 300,
            "next run for */5 should be within 5 minutes, got {}s",
            diff.num_seconds()
        );
    }

    #[test]
    fn test_calculate_next_run_invalid() {
        let now = Utc::now();
        let next = calculate_next_run("not-a-cron", &now);
        let diff = next - now;
        assert!(
            (diff.num_seconds() - 3600).abs() < 5,
            "invalid cron should fall back to now + 1h, got {}s",
            diff.num_seconds()
        );
    }

    #[test]
    fn test_calculate_next_run_empty_string() {
        let now = Utc::now();
        let next = calculate_next_run("", &now);
        let diff = next - now;
        assert!(
            (diff.num_seconds() - 3600).abs() < 5,
            "empty cron should fall back to now + 1h, got {}s",
            diff.num_seconds()
        );
    }

    #[test]
    fn test_calculate_next_run_daily() {
        let now = Utc::now();
        // 5-field: daily at 09:00
        let next = calculate_next_run("0 9 * * *", &now);
        assert!(next > now, "daily cron must produce a future timestamp");
        let diff = next - now;
        assert!(
            diff.num_hours() <= 24,
            "daily cron should be within 24h, got {}h",
            diff.num_hours()
        );
    }

    #[test]
    fn test_calculate_next_run_hourly() {
        let now = Utc::now();
        // 5-field: fire at minute 0 of every hour
        let next = calculate_next_run("0 * * * *", &now); // min=0, hour=*, dom=*, month=*, dow=*
        assert!(next > now);
        let diff = next - now;
        assert!(
            diff.num_minutes() <= 60,
            "hourly cron should be within 60m, got {}m",
            diff.num_minutes()
        );
        assert_eq!(next.minute(), 0, "hourly cron should fire at minute 0");
    }

    #[test]
    fn test_calculate_next_run_weekly() {
        let now = Utc::now();
        // 5-field: Sunday at midnight (min=0, hour=0, dom=*, month=*, dow=0)
        let next = calculate_next_run("0 0 * * 0", &now);
        assert!(next > now);
        let diff = next - now;
        assert!(
            diff.num_days() <= 8,
            "weekly cron should be within 8 days, got {}d",
            diff.num_days()
        );
    }

    #[test]
    fn test_calculate_next_run_every_30min() {
        let now = Utc::now();
        // 5-field: every 30 minutes (min=*/30, hour=*, dom=*, month=*, dow=*)
        let next = calculate_next_run("*/30 * * * *", &now);
        assert!(next > now);
        let diff = next - now;
        assert!(
            diff.num_minutes() <= 30,
            "*/30 cron should fire within 30m, got {}m",
            diff.num_minutes()
        );
    }

    #[test]
    fn test_calculate_next_run_every_minute() {
        let now = Utc::now();
        // 5-field: every minute (min=*, hour=*, dom=*, month=*, dow=*)
        let next = calculate_next_run("* * * * *", &now);
        assert!(next > now);
        let diff = next - now;
        assert!(
            diff.num_minutes() <= 1,
            "* * * * * cron should fire within 1m, got {}s",
            diff.num_seconds()
        );
    }

    // ─── extract_error_code ───────────────────────────────────────────────

    #[test]
    fn test_extract_error_code_mcp_tool_error() {
        let code = extract_error_code("MCP tool call error (-32603): Internal error");
        assert_eq!(code, Some("-32603".to_string()));
    }

    #[test]
    fn test_extract_error_code_mcp_init_error() {
        let code = extract_error_code("MCP initialize error (0): something went wrong");
        assert_eq!(code, Some("0".to_string()));
    }

    #[test]
    fn test_extract_error_code_plugin_error() {
        let code = extract_error_code("Plugin 'name' initialize error (-1): Failed");
        assert_eq!(code, Some("-1".to_string()));
    }

    #[test]
    fn test_extract_error_code_negative_four_digits() {
        let code = extract_error_code("error (-1234): some error");
        assert_eq!(code, Some("-1234".to_string()));
    }

    #[test]
    fn test_extract_error_code_positive_code() {
        let code = extract_error_code("error (42): answer found");
        assert_eq!(code, Some("42".to_string()));
    }

    #[test]
    fn test_extract_error_code_no_code() {
        let code = extract_error_code("General error without code");
        assert_eq!(code, None);
    }

    #[test]
    fn test_extract_error_code_empty_string() {
        let code = extract_error_code("");
        assert_eq!(code, None);
    }

    #[test]
    fn test_extract_error_code_error_without_parentheses() {
        let code = extract_error_code("error occurred");
        assert_eq!(code, None);
    }

    #[test]
    fn test_extract_error_code_error_at_start() {
        let code = extract_error_code("error (5): something happened");
        assert_eq!(code, Some("5".to_string()));
    }

    #[test]
    fn test_extract_error_code_multiple_errors() {
        // Should match the first "error ("
        let code = extract_error_code("error (1): first error and then error (2)");
        assert_eq!(code, Some("1".to_string()));
    }

    #[test]
    fn test_extract_error_code_code_within_text_no_parens() {
        let code = extract_error_code("error_code_5");
        assert_eq!(code, None);
    }

    // ─── validate_cron_schedule_5field ─────────────────────────────────

    #[test]
    fn test_validate_cron_5field_valid() {
        assert!(validate_cron_schedule_5field("* * * * *"));
        assert!(validate_cron_schedule_5field("0 9 * * 1-5"));
        assert!(validate_cron_schedule_5field("*/15 * * * *"));
        assert!(validate_cron_schedule_5field("30 6 * * *"));
        assert!(validate_cron_schedule_5field("0 0 1 * *"));
    }

    #[test]
    fn test_validate_cron_5field_too_few_fields() {
        assert!(!validate_cron_schedule_5field("* * * *"));
        assert!(!validate_cron_schedule_5field("* * *"));
        assert!(!validate_cron_schedule_5field(""));
    }

    #[test]
    fn test_validate_cron_5field_too_many_fields() {
        // 6-field should fail validation (we only accept 5)
        assert!(!validate_cron_schedule_5field("0 * * * * *"));
        assert!(!validate_cron_schedule_5field("0 0 9 * * *"));
    }

    #[test]
    fn test_validate_cron_5field_with_whitespace() {
        assert!(validate_cron_schedule_5field("  0 9 * * *  "));
    }

    // ─── Notes about integration tests ─────────────────────────────────
    //
    // Tests for create_action_thread and handle_action_mode require a
    // live PgPool (cron_jobs table, channels, etc.) and are located in
    // the integration test suite at tests/integration/action_thread.rs.
    //
    // These integration tests verify:
    //   - Scheduled action mode → thread cause="system", msg_type="cron"
    //   - Manual run action mode → thread cause="user", msg_type="cron"
    //   - Silent action with errors → error thread created
    //   - Silent action without errors → no thread created
}
