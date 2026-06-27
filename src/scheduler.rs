//! Cron scheduler — polls `cron_jobs` table and fires due jobs by creating
//! threads with cause='cron' and a cause message, then setting them pending
//! for the executor to pick up.
//!
//! The scheduler runs as a background tokio task, polling every 30 seconds.
//! Concurrency is enforced atomically at the DB level:
//! - Job is claimed with `UPDATE ... WHERE NOT running`
//! - If 0 rows affected, another tick already claimed it → skip
//! - After firing, `running` is cleared and timestamps updated

use crate::error::{Error, AppResult};
use crate::err_msg;
use chrono::{DateTime, Utc};
use cron::Schedule;
use sql_forge::sql_forge;
use sqlx::FromRow;
use sqlx::PgPool;
use std::str::FromStr;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

use crate::db::types as queries;
use crate::db::types::MessageNew;
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
    planning_mode: String,
}

/// Shared context for action execution and reporting.
pub(crate) struct ActionContext<'a> {
    pool: &'a PgPool,
    data_dir: &'a str,
    mcp_registry: &'a McpRegistry,
    app_context: &'a AppContext,
    job: &'a CronJobDueRow,
    display_name: &'a str,
}

/// Parameters for reporting an action failure.
struct ReportActionFailureParams<'a> {
    job: &'a CronJobDueRow,
    display_name: &'a str,
    action_name: &'a str,
    err_msg: String,
    thread_id: i64,
    now_ts: i64,
}

/// Spawn the cron scheduler loop as a background task.
pub fn spawn(
    pool: PgPool,
    data_dir: String,
    mcp_registry: McpRegistry,
    app_context: AppContext,
) -> tokio::task::JoinHandle<()> {
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
    mcp_registry: &McpRegistry,
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

        // ── Action mode: run built-in action by action_id ──
        // Writes result as terminal system messages (seq-0: action name, type=cron, subtype=job name).

        /// Helper: run a single action, producing terminal messages on the given thread.
        async fn run_action_and_report(
            ctx: ActionContext<'_>,
            action_id: &str,
            thread_id: i64,
            now_ts: i64,
        ) -> AppResult<()> {
            let (action_name, exec_result) = resolve_and_execute_action(&ctx, action_id).await;

            match exec_result {
                Ok(()) => {
                    // ── Success: system terminal + seq-0 ──
                    let cron_name = ctx.job.name.clone().unwrap_or_default();
                    let metadata = serde_json::json!({
                        "cron_job_id": ctx.job.id,
                        "cron_job_name": ctx.job.name,
                        "cron_display_name": ctx.display_name,
                        "scheduled_at": ctx.job.schedule,
                    });
                    queries::set_thread_system(ctx.pool, thread_id).await?;
                    let msg = MessageNew {
                        thread_id,
                        role: "system".to_string(),
                        content: action_name,
                        thread_sequence: 0,
                        external_id: Some(format!("cron:{}:{}", ctx.job.id, now_ts)),
                        metadata,
                        embedding: None,
                        summary_text: None,
                        is_summary: false,
                        msg_type: "cron".to_string(),
                        msg_subtype: Some(cron_name),
                        processing_time_ms: None,
                        token_usage: None,
                        iteration_number: 0,
                    };
                    queries::create_message(ctx.pool, &msg).await?;
                }
                Err(err_msg) => {
                    report_action_failure(
                        ctx.pool,
                        ReportActionFailureParams {
                            job: ctx.job,
                            display_name: ctx.display_name,
                            action_name: &action_name,
                            err_msg,
                            thread_id,
                            now_ts,
                        },
                    )
                    .await?;
                }
            }

            Ok(())
        }

        let job_mode = job.mode.as_deref().unwrap_or("agentic");
        if job_mode == "action" {
            if let Some(ref action_id) = job.action_id {
                let silent = job.silent.unwrap_or(false);
                if silent {
                    // Silent mode: run action first, only create thread on error
                    let action_ctx = ActionContext {
                        pool,
                        data_dir,
                        mcp_registry,
                        app_context,
                        job: &job,
                        display_name,
                    };
                    let (action_name, exec_result) =
                        resolve_and_execute_action(&action_ctx, action_id).await;

                    if let Err(err_msg) = exec_result {
                        // Action failed — now create thread + report error
                        let cron_channel = ensure_cron_channel(pool).await?;
                        let thread = queries::create_thread(
                            pool,
                            "cron",
                            cron_channel.id,
                            "cron",
                            queries::CreateThreadParams {
                                provider: None,
                                model: None,
                                task_id: None,
                                schedule_task_id: Some(job.id.clone()),
                                planning_mode: job.planning_mode.clone(),
                            },
                        )
                        .await?;
                        report_action_failure(
                            pool,
                            ReportActionFailureParams {
                                job: &job,
                                display_name,
                                action_name: &action_name,
                                err_msg,
                                thread_id: thread.id,
                                now_ts: now.timestamp(),
                            },
                        )
                        .await?;
                    }
                    // Success: nothing to do, no thread created
                } else {
                    // Normal mode: current behavior
                    let cron_channel = ensure_cron_channel(pool).await?;
                    let thread = queries::create_thread(
                        pool,
                        "cron",
                        cron_channel.id,
                        "cron",
                        queries::CreateThreadParams {
                            provider: None,
                            model: None,
                            task_id: None,
                            schedule_task_id: Some(job.id.clone()),
                            planning_mode: job.planning_mode.clone(),
                        },
                    )
                    .await?;

                    if let Err(e) = run_action_and_report(
                        ActionContext {
                            pool,
                            data_dir,
                            mcp_registry,
                            app_context,
                            job: &job,
                            display_name,
                        },
                        action_id,
                        thread.id,
                        now.timestamp(),
                    )
                    .await
                    {
                        error!(
                            "[cron-scheduler] Action reporting failed for job '{}': {:?}",
                            display_name, e
                        );
                    }
                }
            }
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
            .unwrap_or_else(|| crate::profile::Profile::default("default"));

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
                task_planning_mode: job.planning_mode.clone(),
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
        SELECT id, name, display_name, schedule, prompt, channel_id, profile, mode, action_id, silent, template, planning_mode
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

/// Resolve and execute an action, returning (action_name, result).
async fn resolve_and_execute_action(
    ctx: &ActionContext<'_>,
    action_id: &str,
) -> (String, Result<(), String>) {
    match action_id {
        "builtin_kanban_dispatcher" => {
            let name = "Kanban Dispatcher".to_string();
            let r = run_kanban_dispatcher(ctx.pool, ctx.data_dir)
                .await
                .map_err(|e| format!("{:#}", e));
            (name, r)
        }
        "builtin_relevance_indexer" => {
            let name = "Relevance Indexer".to_string();
            let r = crate::relevance::run_relevance_indexer(ctx.pool, ctx.data_dir)
                .await
                .map_err(|e| format!("{:#}", e));
            (name, r)
        }
        "builtin_hindsight_populator" => {
            let name = "Hindsight Populator".to_string();
            let r = crate::hindsight_populator::run_hindsight_populator(ctx.pool, ctx.data_dir)
                .await
                .map(|summary| {
                    tracing::info!("[hindsight-populator] {}", summary);
                })
                .map_err(|e| format!("{:#}", e));
            (name, r)
        }
        "builtin_setup_knowledge_pipeline" => {
            let name = "Setup Knowledge Pipeline".to_string();
            let r = setup_knowledge_pipeline(ctx.pool, ctx.data_dir, None, None).await;
            (name, r)
        }
        other => {
            // Read from YAML instead of DB
            match crate::actions::get_action(ctx.data_dir, other) {
                Ok(Some(action)) => {
                    let call = McpToolCall {
                        id: String::new(),
                        name: action.tool_name.clone(),
                        arguments: action.params.clone(),
                    };
                    let r = match ctx
                        .mcp_registry
                        .execute(&call, ctx.app_context.clone())
                        .await
                    {
                        Ok(result) => {
                            if result.is_error {
                                Err(result.content)
                            } else {
                                Ok(())
                            }
                        }
                        Err(e) => Err(format!("{:#}", e)),
                    };
                    (action.name, r)
                }
                Ok(None) => (
                    other.to_string(),
                    Err(format!("Action '{}' not found", other)),
                ),
                Err(e) => (
                    other.to_string(),
                    Err(format!("YAML lookup failed: {:#}", e)),
                ),
            }
        }
    }
}

/// Report an action failure by setting thread to failed and writing seq-0/seq-1 messages.
async fn report_action_failure(
    pool: &PgPool,
    p: ReportActionFailureParams<'_>,
) -> AppResult<()> {
    let cron_name = p.job.name.clone().unwrap_or_default();
    let err_subtype = if let Some(code) = extract_error_code(&p.err_msg) {
        Some(code)
    } else {
        None
    };
    let metadata = serde_json::json!({
        "cron_job_id": p.job.id,
        "cron_job_name": p.job.name,
        "cron_display_name": p.display_name,
        "scheduled_at": p.job.schedule,
    });

    sql_forge!(
        "UPDATE threads SET status = 'failed', terminal = true WHERE id = :id",
        ( :id = p.thread_id )
    )
    .execute(pool)
    .await?;

    // seq-0: action name
    let msg0 = MessageNew {
        thread_id: p.thread_id,
        role: "system".to_string(),
        content: p.action_name.to_string(),
        thread_sequence: 0,
        external_id: Some(format!("cron:{}:{:?}", p.job.id, p.now_ts)),
        metadata: metadata.clone(),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "cron".to_string(),
        msg_subtype: Some(cron_name.clone()),
        processing_time_ms: None,
        token_usage: None,
        iteration_number: 0,
    };
    queries::create_message(pool, &msg0).await?;

    // seq-1: error details
    let msg1 = MessageNew {
        thread_id: p.thread_id,
        role: "system".to_string(),
        content: p.err_msg,
        thread_sequence: 1,
        external_id: Some(format!("cron:{}:{:?}:error", p.job.id, p.now_ts)),
        metadata,
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "error".to_string(),
        msg_subtype: err_subtype,
        processing_time_ms: None,
        token_usage: None,
        iteration_number: 0,
    };
    queries::create_message(pool, &msg1).await?;

    Ok(())
}

/// Run the kanban_dispatcher: move all 'todo' tasks to 'ready' by creating
/// threads and messages for each, respecting dependencies and ordering by
/// priority DESC, position ASC.
pub async fn run_kanban_dispatcher(pool: &PgPool, data_dir: &str) -> AppResult<()> {
    #[derive(Debug, FromRow)]
    struct TodoTaskRow {
        id: String,
        title: String,
        body: Option<String>,
        template: Option<String>,
        channel_id: Option<i64>,
        profile: Option<String>,
    }

    let tasks: Vec<TodoTaskRow> = sql_forge!(
        TodoTaskRow,
        r#"
        SELECT id, title, body, template, channel_id, profile
        FROM kanban_tasks
        WHERE status = 'todo'
          AND NOT archived
        ORDER BY priority DESC NULLS LAST, position ASC NULLS LAST
        "#,
    )
    .fetch_all(pool)
    .await?;

    if tasks.is_empty() {
        info!("[kanban-dispatcher] No todo tasks to dispatch");
        return Ok(());
    }

    let mut dispatched = 0u64;
    let mut skipped_deps = 0u64;

    for t in &tasks {
        // Check dependencies
        let blocked_deps: i64 = sql_forge!(
            scalar i64,
            r#"
            SELECT COUNT(*)
            FROM kanban_task_dependencies d
            JOIN kanban_tasks t_dep ON t_dep.id = d.depends_on_id
            WHERE d.task_id = :task_id
              AND t_dep.status != 'done'
              AND NOT t_dep.archived
            "#,
            ( :task_id = &t.id )
        )
        .fetch_one(pool)
        .await?;

        if blocked_deps > 0 {
            info!(
                "[kanban-dispatcher] Task '{}' has {} unsatisfied dependencies — skipping",
                t.id, blocked_deps
            );
            skipped_deps += 1;
            continue;
        }

        // Resolve channel_id
        let task_channel_id = if let Some(cid) = t.channel_id {
            cid
        } else {
            match queries::get_channel_by_platform_name(pool, "kanban", "kanban").await {
                Ok(Some(ch)) => ch.id,
                _ => {
                    warn!(
                        "[kanban-dispatcher] No default cron channel found, skipping task '{}'",
                        t.id
                    );
                    continue;
                }
            }
        };

        // Load channel unconditionally (needed for profile, provider, model resolution)
        let channel = match queries::find_channel_by_id(pool, task_channel_id).await {
            Ok(Some(ch)) => ch,
            _ => {
                warn!(
                    "[kanban-dispatcher] Channel {} not found for task '{}'",
                    task_channel_id, t.id
                );
                continue;
            }
        };

        // Resolve profile: task → channel → default
        let profile_name = t.profile.clone().unwrap_or(channel.current_profile.clone());
        if profile_name.is_empty() {
            warn!(
                "[kanban-dispatcher] No profile resolved for task '{}' (channel {})",
                t.id, task_channel_id
            );
            continue;
        }

        // Resolve provider+model: channel → profile → env vars
        let profile_registry = crate::profile::ProfileRegistry::new(data_dir);
        let prof = profile_registry
            .get(&profile_name)
            .cloned()
            .unwrap_or_else(|| crate::profile::Profile::default("default"));

        let resolved = resolve_thread_config(
            t.profile.as_deref(),
            &channel.current_profile,
            channel.current_provider.as_deref(),
            channel.current_model.as_deref(),
            prof.provider.as_deref(),
            prof.model.as_deref(),
        );

        let (provider, model) = match resolved {
            Some(cfg) => (cfg.provider, cfg.model),
            None => {
                warn!(
                    "[kanban-dispatcher] Could not resolve config for task '{}' — empty profile or no default model for provider",
                    t.id
                );
                continue;
            }
        };

        // Create thread with cause message (resolves planning mode internally)
        let task_content = t.body.as_deref().unwrap_or(&t.title);
        let kanban_content = if task_content.is_empty() {
            format!("Execute kanban task: {}", t.title)
        } else {
            task_content.to_string()
        };
        let thread_id = match queries::create_thread_with_cause(
            pool,
            "system",
            task_channel_id,
            &profile_name,
            queries::ThreadCauseParams {
                provider: Some(provider.clone()),
                model: Some(model.clone()),
                task_id: Some(t.id.clone()),
                schedule_task_id: None,
                content: kanban_content,
                external_id: Some(format!("kanban:{}", t.id)),
                metadata: serde_json::json!({
                    "kanban_task_id": t.id,
                    "kanban_task_title": t.title,
                    "template": t.template.clone().filter(|t| !t.is_empty()).or_else(|| channel.template.clone()).unwrap_or_default(),
                }),
                msg_type: "kanban".to_string(),
                msg_subtype: Some(t.id.clone()),
                task_planning_mode: String::new(),
            },
        )
        .await
        {
            Ok((thread, created)) => {
                let tid = thread.id;
                info!(
                    "[kanban-dispatcher] Created cause message {} / thread {} for task '{}'",
                    created.id, tid, t.id
                );
                tid
            }
            Err(e) => {
                warn!(
                    "[kanban-dispatcher] Failed to create cause for task '{}': {:?}",
                    t.id, e
                );
                continue;
            }
        };

        // Advance to ready
        sql_forge!(
            "UPDATE kanban_tasks SET status = 'ready', updated_at = NOW() WHERE id = :id",
            ( :id = &t.id )
        )
        .execute(pool)
        .await?;

        info!(
            "[kanban-dispatcher] Task '{}' advanced to ready (thread {})",
            t.id, thread_id
        );
        dispatched += 1;
    }

    info!(
        "[kanban-dispatcher] Dispatched {} tasks ({} skipped due to deps, {} total todo)",
        dispatched,
        skipped_deps,
        tasks.len()
    );

    Ok(())
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
///    model:        channel.current_model → profile.model → default_model
///
/// `default_model` is the provider plugin's default model (resolved by caller).
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

    let provider = channel_provider
        .filter(|s| !s.is_empty())
        .or_else(|| profile_provider.filter(|s| !s.is_empty()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "opencode-go".to_string())
        });

    // Model: channel → profile → provider plugin default → error
    let model = channel_model
        .filter(|s| !s.is_empty())
        .or_else(|| profile_model.filter(|s| !s.is_empty()))
        .map(|s| s.to_string())
        .or_else(|| crate::llm::resolve_default_model(&provider));

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

/// Fire a cron job by schedule_id — used by the HTTP run-cron endpoint.
/// This reuses the same scheduler logic (channel resolution, profile/provider/model resolution,
/// thread creation) so the manual Run button goes through exactly the same code path as the
/// scheduled tick.
pub async fn fire_cron_job_by_id(
    pool: &PgPool,
    data_dir: &str,
    mcp_registry: &McpRegistry,
    app_context: &AppContext,
    schedule_id: &str,
    force: bool,
) -> AppResult<i64> {
    let jobs: Vec<CronJobDueRow> = sql_forge!(
        CronJobDueRow,
        r#"
        SELECT id, name, display_name, schedule, prompt, channel_id, profile, mode, action_id, silent, template, planning_mode
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
        err_msg!("Job '{}' is not active. Use force=true to run anyway.", schedule_id);
    }

    let now = Utc::now();
    let display_name = if job.display_name.is_empty() {
        job.name.as_deref().unwrap_or("cron-job")
    } else {
        &job.display_name
    };

    let job_mode = job.mode.as_deref().unwrap_or("agentic");

    // Action mode
    if job_mode == "action" {
        if let Some(ref action_id) = job.action_id {
            let cron_channel = ensure_cron_channel(pool).await?;
            let thread = queries::create_thread(
                pool,
                "cron",
                cron_channel.id,
                "cron",
                queries::CreateThreadParams {
                    provider: None,
                    model: None,
                    task_id: None,
                    schedule_task_id: Some(job.id.clone()),
                    planning_mode: job.planning_mode.clone(),
                },
            )
            .await?;

            // Execute the action via the scheduler's internal action logic
            let action_ctx = ActionContext {
                pool,
                data_dir,
                mcp_registry,
                app_context,
                job: &job,
                display_name,
            };
            let (action_name, exec_result) =
                resolve_and_execute_action(&action_ctx, action_id).await;

            match exec_result {
                Ok(()) => {
                    let cron_name = job.name.clone().unwrap_or_default();
                    let metadata = serde_json::json!({
                        "cron_job_id": job.id,
                        "cron_job_name": job.name,
                        "cron_display_name": display_name,
                        "scheduled_at": job.schedule,
                    });
                    queries::set_thread_system(pool, thread.id).await?;
                    let msg = queries::MessageNew {
                        thread_id: thread.id,
                        role: "system".to_string(),
                        content: action_name,
                        thread_sequence: 0,
                        external_id: Some(format!("cron:{}:{}", job.id, now.timestamp())),
                        metadata,
                        embedding: None,
                        summary_text: None,
                        is_summary: false,
                        msg_type: "cron".to_string(),
                        msg_subtype: Some(cron_name),
                        processing_time_ms: None,
                        token_usage: None,
                        iteration_number: 0,
                    };
                    queries::create_message(pool, &msg).await?;
                }
                Err(err_msg) => {
                    report_action_failure(
                        pool,
                        ReportActionFailureParams {
                            job: &job,
                            display_name,
                            action_name: &action_name,
                            err_msg,
                            thread_id: thread.id,
                            now_ts: now.timestamp(),
                        },
                    )
                    .await?;
                }
            }
        }
        return Ok(0); // action mode doesn't create an agent thread
    }

    // Agentic mode — same logic as the scheduler tick
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
        .unwrap_or_else(|| crate::profile::Profile::default("default"));

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
            task_planning_mode: job.planning_mode.clone(),
        },
    )
    .await?;

    info!(
        "[cron-run] Created thread {} for job '{}' (manual run)",
        thread.id, display_name
    );

    Ok(thread.id)
}

/// Set up the Knowledge Pipeline cron job (idempotent).
/// Creates a cron job that loads the knowledge-pipeline template and runs
/// the periodic maintenance pipeline.
///
/// - `schedule`: Optional cron schedule (default: `0 */6 * * *` = every 6 hours)
/// - `prompt`: Optional prompt override (default: "Execute the Knowledge Pipeline according to the task template above.")
pub async fn setup_knowledge_pipeline(
    pool: &PgPool,
    data_dir: &str,
    schedule: Option<String>,
    prompt: Option<String>,
) -> Result<(), String> {
    use sql_forge::sql_forge;

    // Defaults
    let schedule = schedule.unwrap_or_else(|| "0 */6 * * *".to_string());

    // Validate schedule is 5-field format
    if !validate_cron_schedule_5field(&schedule) {
        return Err(format!(
            "Invalid cron schedule '{}': must have exactly 5 whitespace-separated fields (minute hour day month weekday)",
            schedule
        ));
    }

    // Read prompt from template file, with fallback
    let prompt = prompt.or_else(|| {
        let path = format!("{}/profiles/default/templates/knowledge-pipeline.md", data_dir);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let trimmed = content.trim().to_string();
                if !trimmed.is_empty() {
                    tracing::info!("[knowledge-pipeline] Loaded template from {}", path);
                    Some(trimmed)
                } else {
                    tracing::warn!("[knowledge-pipeline] Template file {} is empty, using fallback", path);
                    None
                }
            }
            Err(e) => {
                tracing::warn!("[knowledge-pipeline] Could not read template {}: {}, using fallback", path, e);
                None
            }
        }
    }).unwrap_or_else(|| {
        // Fallback prompt — keep the agent functional even without the template file
        "# Knowledge Pipeline\n\nYou have only 10 iterations. Follow exactly in order.\n\n## Step 1 (iteration 1)\nquery_database({\"operation\": \"query\", \"sql\": \"SELECT id, name FROM channels WHERE closed = false;\"})\n\n## Step 2 (iteration 2)\nquery_database({\"operation\": \"query\", \"sql\": \"SELECT channel_id, COUNT(*)::int as cnt FROM summaries GROUP BY channel_id;\"})\n\n## Step 3 (iteration 3)\nquery_database({\"operation\": \"query\", \"sql\": \"SELECT id, profile FROM threads WHERE status='completed' AND created_at > NOW() - INTERVAL '7 days';\"})\n\n## Step 4 (iteration 4)\nactions command=relevance_indexer — call actions with command 'relevance_indexer', no other inputs needed.\n\n## Step 5 (iteration 5)\nactions command=hindsight_populator — call actions with command 'hindsight_populator', no other inputs needed. If it fails, continue to Step 6.\n\n## Step 6 (iterations 6-10)\nProduce a brief summary of the 3 query results + the 2 actions called.\n\n## CRITICAL RULES\n- Do NOT use: search_thread_messages, search_channel_prompts, search_messages, filesystem_list, search_wiki, manage_subtasks\n- Only 10 iterations total. Budget them tightly.\n- If a query fails, retry ONCE. If still fails, skip and continue.\n- After all steps, output the final summary. That is your last action.".to_string()
    });

    let existing = sqlx::query_scalar::<_, String>(
        r#"SELECT id FROM cron_jobs WHERE name = 'knowledge-pipeline' LIMIT 1"#,
    )
    .fetch_optional(pool)
    .await;

    // If we can't query or already exists, skip creation (idempotent)
    if existing.as_ref().ok().and_then(|v| v.as_ref()).is_some() || existing.is_err() {
        return Ok(());
    }

    // 1. Create the cron job — no channel_id or profile set; the scheduler
    //    resolves the default cron channel at runtime, matching dashboard behavior.
    let id = format!(
        "knowledge-pipeline-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    let skills_json = serde_json::json!(["knowledge-pipeline"]).to_string();

    sql_forge!(
        r#"
        INSERT INTO cron_jobs (id, name, display_name, schedule, prompt, skills, mode, template, planning_mode, profile, enabled, active)
        VALUES (:id, 'knowledge-pipeline', 'Knowledge Pipeline', :schedule, :prompt, :skills, 'agentic', '', 'prompt_only', 'pipeline', true, true)
        "#,
        ( :id = &id, :schedule = &schedule, :prompt = &prompt, :skills = &skills_json )
    )
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to create cron job: {}", e))?;

    tracing::info!("[knowledge-pipeline] Created cron job with id={}", id);
    Ok(())
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
            None,
            None,
            None,
            None,
        );
        assert_eq!(cfg.unwrap().profile_name, "task-profile");
    }

    #[test]
    fn test_profile_from_channel_when_task_none() {
        let cfg = resolve_thread_config(None, "channel-profile", None, None, None, None);
        assert_eq!(cfg.unwrap().profile_name, "channel-profile");
    }

    #[test]
    fn test_profile_from_channel_when_task_empty() {
        let cfg = resolve_thread_config(Some(""), "channel-profile", None, None, None, None);
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
            None,
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
            None,
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
            None,
            Some("deepseek-v4-flash"),
            None,
            Some("claude-3"),
        );
        assert_eq!(cfg.unwrap().model, "deepseek-v4-flash");
    }

    #[test]
    fn test_model_falls_back_to_profile() {
        let cfg = resolve_thread_config(None, "default", None, None, None, Some("claude-3"));
        assert_eq!(cfg.unwrap().model, "claude-3");
    }

    #[test]
    fn test_model_channel_overrides_profile() {
        let cfg = resolve_thread_config(
            None,
            "default",
            None,
            Some("deepseek-v4-flash"),
            None,
            Some("claude-3"),
        );
        assert_eq!(cfg.unwrap().model, "deepseek-v4-flash");
    }

    #[test]
    fn test_model_skip_empty_channel() {
        let cfg = resolve_thread_config(None, "default", None, Some(""), None, Some("claude-3"));
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
        // by the agent's validation (tested via the resolve function contract)
        let cfg = resolve_thread_config(None, "default", None, None, None, None);
        // resolve_thread_config always returns Some as long as profile is non-empty
        // (it falls back to env vars). The *agent* checks thread.provider directly.
        let c = cfg.unwrap();
        // The strings should come from env var fallbacks (not None/empty)
        assert!(
            !c.provider.is_empty(),
            "provider must not be empty even after full fallback"
        );
        assert!(
            !c.model.is_empty(),
            "model must not be empty even after full fallback"
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
}
