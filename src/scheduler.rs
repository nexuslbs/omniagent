//! Cron scheduler: polls `{data_dir}/config/tasks.yml` (`schedules:` key) and
//! fires due jobs by creating threads with cause='cron' and a cause message,
//! then setting them pending for the executor to pick up.
//!
//! Definitions live in the yml (git-tracked source of truth), NOT in the
//! (dormant) `cron_jobs` table. Runtime cadence is tracked in the minimal
//! `task_runs (task_key, last_fired_at)` bookkeeping table, updated on every
//! fire (agentic, action and silent modes alike), so a schedule fires at its
//! cron cadence, never twice for the same due time, and not on every tick.
//!
//! The scheduler runs as a background tokio task, polling every 30 seconds.
//! Concurrency is enforced by an atomic claim in `task_runs`: the fire record
//! is upserted with `WHERE last_fired_at IS NOT DISTINCT FROM :last_seen`, so
//! only one tick can win the claim for a given due time. Runs themselves are
//! NOT stored beyond the cadence marker — they are observable via the threads
//! each schedule creates (`threads.schedule_task_id = <yml key>`).

use crate::err_msg;
use crate::error::{AppResult, Error};
use crate::tasks_yaml;
use chrono::{DateTime, Utc};
use cron::Schedule;
use sqlx::FromRow;
use sqlx::PgPool;
use std::str::FromStr;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

use crate::db::types as queries;
use crate::mcp::{AppContext, McpToolCall};

#[derive(Debug, FromRow)]
struct CronJobDueRow {
    id: String,
    name: Option<String>,
    display_name: String,
    schedule: String,
    prompt: Option<String>,
    channel_id: Option<String>,
    profile: Option<String>,
    mode: Option<String>,
    action_id: Option<String>,
    silent: Option<bool>,
    template: Option<String>,
    plan: Option<bool>,
}

impl CronJobDueRow {
    /// Build a due-row from a tasks.yml schedule definition (channel NAME is
    /// resolved to an id by the caller; unknown → None = default channel).
    fn from_yml(key: &str, def: &tasks_yaml::ScheduleDef, channel_id: Option<String>) -> Self {
        Self {
            id: key.to_string(),
            name: Some(key.to_string()),
            display_name: def.display_name.clone().unwrap_or_else(|| key.to_string()),
            schedule: def.cron.clone(),
            prompt: def.prompt.clone(),
            channel_id,
            profile: def.profile.clone(),
            mode: Some(def.mode()),
            action_id: def.action.clone(),
            silent: def.silent,
            template: def.template.clone(),
            plan: def.plan(),
        }
    }
}

/// Spawn the cron scheduler loop as a background task.
pub fn spawn(
    pool: PgPool,
    data_dir: String,
    plugin_manager: Arc<dyn crate::agent::plugin_manager::PluginManager>,
    app_context: AppContext,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("[cron-scheduler] Starting cron scheduler loop");

        loop {
            if let Err(e) = tick(&pool, &data_dir, &plugin_manager, &app_context).await {
                error!("[cron-scheduler] Tick failed: {:?}", e);
            }
            sleep(Duration::from_secs(30)).await;
        }
    })
}

/// One tick: find due schedules in tasks.yml, atomically claim each one in
/// `task_runs`, then fire it (action mode, silent, or agentic thread).
async fn tick(
    pool: &PgPool,
    data_dir: &str,
    plugin_manager: &Arc<dyn crate::agent::plugin_manager::PluginManager>,
    app_context: &AppContext,
) -> AppResult<()> {
    let jobs = fetch_due_jobs(pool, data_dir).await?;

    for job in jobs {
        let now = Utc::now();
        let display_name = if job.display_name.is_empty() {
            job.name.as_deref().unwrap_or("cron-job")
        } else {
            &job.display_name
        };

        // ── Validate 5-field cron format ──
        if !validate_cron_schedule_5field(&job.schedule) {
            warn!(
                "[cron-scheduler] Schedule '{}' has invalid cron expression '{}': expected 5 fields (min hour dom month dow), got {} fields. Schedule will be skipped.",
                display_name, job.schedule, job.schedule.split_whitespace().count()
            );
            continue;
        }

        // ── Atomic claim: only one tick can fire this schedule occurrence ──
        let last_fire = last_fired_at(pool, &job.id).await?;
        if !is_due(&job.schedule, last_fire, now) {
            continue; // another tick already claimed this occurrence
        }
        if !claim_fire(pool, &job.id, last_fire, now).await? {
            info!(
                "[cron-scheduler] Schedule '{}' already claimed by another process, skipping",
                display_name
            );
            continue;
        }

        info!(
            "[cron-scheduler] Firing schedule '{}' (id={})",
            display_name, job.id
        );

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
                plugin_manager,
                app_context,
                job: &job,
                display_name,
                now: &now,
                cause: "system",
            })
            .await;
            continue;
        }

        if is_silent {
            // Silent (non-action) mode: no thread created, no messages saved.
            info!(
                "[cron-scheduler] Silent schedule '{}' fired (no thread created for non-action silent schedule)",
                display_name
            );
            continue;
        }

        // ── Determine which channel to fire into ──
        // Resolution chain: explicit channel -> default_schedule_channel -> ''
        // (empty = the thread is created and then failed with "no channel
        // defined"; the record is kept for audit).
        let channel_id = crate::channels_yaml::resolve_default_channel(
            job.channel_id.as_deref(),
            "default_schedule_channel",
        )
        .unwrap_or_default();
        let channel = if channel_id.is_empty() {
            None
        } else {
            queries::find_channel_by_id(pool, &channel_id)
                .await
                .ok()
                .flatten()
        };

        // Resolve the profile for this message
        let profile_name = if let Some(ref p) = job.profile {
            p.clone()
        } else if let Some(ch) = &channel {
            ch.current_profile.clone()
        } else {
            crate::profile::ProfileRegistry::new(data_dir)
                .default_profile
                .clone()
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
            channel
                .as_ref()
                .map(|c| c.current_profile.as_str())
                .unwrap_or(""),
            channel.as_ref().and_then(|c| c.current_provider.as_deref()),
            channel.as_ref().and_then(|c| c.current_model.as_deref()),
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
            &channel_id,
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
                    "channel_id": channel_id,
                    "profile": profile_name,
                    "template": job.template.clone().filter(|t| !t.is_empty()).or_else(|| channel.as_ref().and_then(|c| c.template.clone())).unwrap_or_default(),
                }),
                msg_type: "cron".to_string(),
                msg_subtype: Some(subtype),
                task_plan: job.plan,
                template: job.template.clone().filter(|t| !t.is_empty()).or_else(|| channel.as_ref().and_then(|c| c.template.clone())),
                parent_external_id: None,
            workflow_id: None,
            workflow_step: None,
            hook_caused: false,
            },
        )
        .await
        {
            Ok((thread, created)) => {
                info!(
                    "[cron-scheduler] Created thread {} / cause message {} for schedule '{}'",
                    thread.id, created.id, display_name
                );
            }
            Err(e) => {
                error!(
                    "[cron-scheduler] Failed to create thread for schedule '{}': {:?}",
                    display_name, e
                );
            }
        }
    }

    Ok(())
}

/// Fetch due enabled schedules from tasks.yml (parsed fresh on every tick so
/// file edits take effect without restart).
///
/// Due-ness: a schedule is due when it has never fired, or when the next run
/// computed from its last fire (`task_runs.last_fired_at`) is <= now.
async fn fetch_due_jobs(pool: &PgPool, data_dir: &str) -> AppResult<Vec<CronJobDueRow>> {
    let tasks = tasks_yaml::load_tasks_or_empty(data_dir);
    let now = Utc::now();
    let mut due = Vec::new();
    for (key, def) in &tasks.schedules {
        if !def.enabled {
            continue;
        }
        if !validate_cron_schedule_5field(&def.cron) {
            continue; // invalid cron: warn+skip happens in tick
        }
        let last_fire = last_fired_at(pool, key).await?;
        if !is_due(&def.cron, last_fire, now) {
            continue;
        }
        let channel_id = tasks_yaml::resolve_channel_id(pool, def.channel.as_deref()).await;
        due.push(CronJobDueRow::from_yml(key, def, channel_id));
    }
    Ok(due)
}

/// Last recorded fire time for a schedule key (None = never fired).
async fn last_fired_at(pool: &PgPool, key: &str) -> AppResult<Option<DateTime<Utc>>> {
    Ok(sqlx::query_scalar::<_, DateTime<Utc>>(
        "SELECT last_fired_at FROM task_runs WHERE task_key = $1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?)
}

/// Atomically record `now` as the last fire for `key` — but ONLY if the
/// schedule is still at the same last-fire state we computed due from
/// (`last_fire`). Returns true when this tick won the claim (proceed to
/// fire); false when a concurrent tick already claimed this occurrence.
async fn claim_fire(
    pool: &PgPool,
    key: &str,
    last_fire: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> AppResult<bool> {
    let res = sqlx::query(
        "INSERT INTO task_runs (task_key, last_fired_at) VALUES ($1, $2) \
         ON CONFLICT (task_key) DO UPDATE SET last_fired_at = EXCLUDED.last_fired_at \
         WHERE task_runs.last_fired_at IS NOT DISTINCT FROM $3",
    )
    .bind(key)
    .bind(now)
    .bind(last_fire)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Pure due-ness check: no last fire → due now; otherwise due when the next
/// run after the last fire is at or before `now`.
pub fn is_due(cron_expr: &str, last_fire: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match last_fire {
        None => true,
        Some(last) => calculate_next_run(cron_expr, &last) <= now,
    }
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
pub(crate) fn resolve_action(data_dir: &str, action_id: &str) -> AppResult<McpToolCall> {
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

    let path = crate::config_path::config_path(data_dir, "actions.yml");
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
    plugin_manager: &'a Arc<dyn crate::agent::plugin_manager::PluginManager>,
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
                "[cron-action] Schedule '{}' has mode=action but no action_id set, skipping",
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
                "[cron-action] Failed to resolve action '{}' for schedule '{}': {}",
                action_id, ctx.display_name, e
            );
            return None;
        }
    };

    info!(
        "[cron-action] Executing action schedule '{}' (tool: {}, action_id: {})",
        ctx.display_name, tool_call.name, action_id
    );

    // Execute the tool first, THEN create the thread with the result.
    // This avoids the executor picking up a pending thread before it's terminal.
    // Snapshot the registry under the lock; tokio::sync::RwLockReadGuard is Send.
    let mcp_snapshot = ctx.plugin_manager.snapshot_registry().await;
    match mcp_snapshot
        .execute(&tool_call, ctx.app_context.clone())
        .await
    {
        Ok(result) => {
            let is_error = result.is_error;

            if is_error {
                error!(
                    "[cron-action] Action schedule '{}' (action_id={}) returned error: {}",
                    ctx.display_name, action_id, result.content
                );
            } else if !is_silent {
                info!(
                    "[cron-action] Action schedule '{}' (action_id={}) completed successfully",
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
                "[cron-action] Action schedule '{}' (action_id={}) execution failed: {}",
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
    // explicit channel -> default_schedule_channel -> '' (fail-with-record).
    let channel_id = crate::channels_yaml::resolve_default_channel(
        ctx.job.channel_id.as_deref(),
        "default_schedule_channel",
    )
    .unwrap_or_default();
    let channel = if channel_id.is_empty() {
        None
    } else {
        queries::find_channel_by_id(ctx.pool, &channel_id)
            .await
            .ok()
            .flatten()
    };

    // Resolve profile
    let profile_name = if let Some(ref p) = ctx.job.profile {
        p.clone()
    } else if let Some(ch) = &channel {
        ch.current_profile.clone()
    } else {
        crate::profile::ProfileRegistry::new(ctx.data_dir)
            .default_profile
            .clone()
    };

    let subtype = ctx.job.name.clone().unwrap_or_default();
    let prompt_content = format!("Cron: {}", ctx.display_name);

    // Create the thread with the given cause and a seq-0 cause message (msg_type='cron')
    let (thread, _cause_msg) = queries::create_thread_with_cause(
        ctx.pool,
        ctx.data_dir,
        ctx.cause,
        &channel_id,
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
                "channel_id": channel_id,
                "profile": profile_name,
                "template": ctx.job.template.clone().filter(|t| !t.is_empty()).or_else(|| channel.as_ref().and_then(|c| c.template.clone())).unwrap_or_default(),
            }),
            template: ctx.job.template.clone().filter(|t| !t.is_empty()).or_else(|| channel.as_ref().and_then(|c| c.template.clone())),
            msg_type: "cron".to_string(),
            msg_subtype: Some(subtype),
            task_plan: ctx.job.plan,
            parent_external_id: None,
        workflow_id: None,
        workflow_step: None,
        hook_caused: false,
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
        duration_ms: ctx.is_error as i32,
        token_usage: serde_json::json!({}),
    };
    if let Err(e) = queries::create_message(ctx.pool, &result_msg).await {
        tracing::warn!("[scheduler] Failed to persist cron result message: {:?}", e);
    }

    // Mark thread as terminal (system for success, failed for error)
    if ctx.is_error {
        queries::set_thread_failed(ctx.pool, thread.id).await?;
        info!(
            "[cron-action] Created failure thread {} for action schedule '{}'",
            thread.id, ctx.display_name
        );
    } else {
        queries::set_thread_system(ctx.pool, thread.id).await?;
        info!(
            "[cron-action] Created result thread {} for action schedule '{}'",
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
                .map(|g| g.read().default_provider.clone())
                .unwrap_or_default(); // Empty string hits the None path below
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

/// Fire a cron schedule by yml key: used by the HTTP run-cron endpoint.
/// This reuses the same scheduler logic (channel resolution, profile/provider/model resolution,
/// thread creation) so the manual Run button goes through exactly the same code path as the
/// scheduled tick. Reads the definition from tasks.yml (no DB write).
pub async fn fire_cron_job_by_id(
    pool: &PgPool,
    data_dir: &str,
    plugin_manager: &Arc<dyn crate::agent::plugin_manager::PluginManager>,
    app_context: &AppContext,
    schedule_id: &str,
    force: bool,
) -> AppResult<Option<i64>> {
    let tasks = tasks_yaml::load_tasks(data_dir)?;
    let def = tasks
        .schedules
        .get(schedule_id)
        .ok_or_else(|| Error::Message(format!("Cron job '{}' not found", schedule_id)))?;

    if !def.enabled && !force {
        err_msg!(
            "Job '{}' is not active. Use force=true to run anyway.",
            schedule_id
        );
    }

    // Validate 5-field cron format
    if !validate_cron_schedule_5field(&def.cron) {
        let j_name = def
            .display_name
            .clone()
            .unwrap_or_else(|| schedule_id.to_string());
        err_msg!(
            "Invalid cron schedule '{}' for job '{}': expected exactly 5 fields (min hour dom month dow), got {} fields. Use standard Linux crontab format, e.g. '0 9 * * 1-5' for weekdays at 9am.",
            def.cron, j_name, def.cron.split_whitespace().count()
        );
    }

    let channel_id = tasks_yaml::resolve_channel_id(pool, def.channel.as_deref()).await;
    let job = CronJobDueRow::from_yml(schedule_id, def, channel_id);
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
            plugin_manager,
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
    // explicit channel -> default_schedule_channel -> '' (fail-with-record).
    let channel_id = crate::channels_yaml::resolve_default_channel(
        job.channel_id.as_deref(),
        "default_schedule_channel",
    )
    .unwrap_or_default();
    let channel = if channel_id.is_empty() {
        None
    } else {
        queries::find_channel_by_id(pool, &channel_id)
            .await
            .ok()
            .flatten()
    };

    let profile_name = if let Some(ref p) = job.profile {
        p.clone()
    } else if let Some(ch) = &channel {
        ch.current_profile.clone()
    } else {
        crate::profile::ProfileRegistry::new(data_dir)
            .default_profile
            .clone()
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
        channel
            .as_ref()
            .map(|c| c.current_profile.as_str())
            .unwrap_or(""),
        channel.as_ref().and_then(|c| c.current_provider.as_deref()),
        channel.as_ref().and_then(|c| c.current_model.as_deref()),
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
        &channel_id,
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
                "channel_id": channel_id,
                "profile": profile_name,
                "template": job.template.clone().filter(|t| !t.is_empty()).or_else(|| channel.as_ref().and_then(|c| c.template.clone())).unwrap_or_default(),
            }),
            template: job.template.clone().filter(|t| !t.is_empty()).or_else(|| channel.as_ref().and_then(|c| c.template.clone())),
            msg_type: "cron".to_string(),
            msg_subtype: Some(subtype),
            task_plan: job.plan,
            parent_external_id: None,
        workflow_id: None,
        workflow_step: None,
        hook_caused: false,
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

    // ─── is_due (scheduler cadence from task_runs) ───

    #[test]
    fn test_is_due_never_fired() {
        let now = Utc::now();
        assert!(is_due("*/5 * * * *", None, now), "never fired → due now");
    }

    #[test]
    fn test_is_due_recent_fire_not_due() {
        let now = Utc::now();
        // Fired 1 minute ago with a daily cron → next run is ~24h away.
        let last = now - chrono::Duration::minutes(1);
        assert!(!is_due("0 9 * * *", Some(last), now));
    }

    #[test]
    fn test_is_due_past_next_occurrence() {
        let now = Utc::now();
        // Fired 10 minutes ago on an every-minute cron → the next minute
        // boundary has already passed → due again.
        let last = now - chrono::Duration::minutes(10);
        assert!(is_due("* * * * *", Some(last), now));
    }

    #[test]
    fn test_is_due_exact_occurrence() {
        // Fixed timestamps: fired at 12:00:00, due window opens at 12:01:00.
        let fired = DateTime::parse_from_rfc3339("2026-08-13T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let at_occurrence = DateTime::parse_from_rfc3339("2026-08-13T12:01:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(is_due("* * * * *", Some(fired), at_occurrence));
        let before = at_occurrence - chrono::Duration::seconds(1);
        assert!(!is_due("* * * * *", Some(fired), before));
    }

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
        // Full-precision lower bound: `schedule.after(now)` guarantees a
        // strictly-after result, but when `now` is within 1s of the next
        // `*/5` match (e.g. 23:39:59.7 → 23:40:00) the diff is sub-second
        // and `num_seconds()` truncates it to 0, falsely failing the check.
        assert!(
            diff > chrono::Duration::zero() && diff.num_seconds() <= 300,
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
}
