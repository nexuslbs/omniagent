//! Cron scheduler — polls `cron_jobs` table and fires due jobs by creating
//! threads with cause='cron' and a cause message, then setting them pending
//! for the executor to pick up.
//!
//! The scheduler runs as a background tokio task, polling every 30 seconds.
//! Concurrency is enforced atomically at the DB level:
//! - Job is claimed with `UPDATE ... WHERE NOT running`
//! - If 0 rows affected, another tick already claimed it → skip
//! - After firing, `running` is cleared and timestamps updated

use anyhow::Result;
use chrono::{DateTime, Utc};
use cron::Schedule;
use sql_forge::sql_forge;
use sqlx::FromRow;
use sqlx::PgPool;
use std::str::FromStr;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

use crate::db::types as queries;
use crate::mcp::{AppContext, McpRegistry, McpToolCall};
use crate::models::MessageNew;

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
    instruction_file: Option<String>,
    planning_mode: String,
}

/// Spawn the cron scheduler loop as a background task.
pub fn spawn(pool: PgPool, data_dir: String, mcp_registry: McpRegistry, app_context: AppContext) -> tokio::task::JoinHandle<()> {
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
async fn tick(pool: &PgPool, data_dir: &str, mcp_registry: &McpRegistry, app_context: &AppContext) -> Result<()> {
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
            pool: &PgPool,
            data_dir: &str,
            mcp_registry: &McpRegistry,
            app_context: &AppContext,
            job: &CronJobDueRow,
            display_name: &str,
            action_id: &str,
            thread_id: i64,
            now_ts: i64,
        ) -> anyhow::Result<()> {
            let (action_name, exec_result) = resolve_and_execute_action(
                pool, data_dir, mcp_registry, app_context, job, display_name, action_id,
            ).await;

            match exec_result {
                Ok(()) => {
                    // ── Success: system terminal + seq-0 ──
                    let cron_name = job.name.clone().unwrap_or_default();
                    let metadata = serde_json::json!({
                        "cron_job_id": job.id,
                        "cron_job_name": job.name,
                        "cron_display_name": display_name,
                        "scheduled_at": job.schedule,
                    });
                    queries::set_thread_system(pool, thread_id).await?;
                    let msg = MessageNew {
                        thread_id,
                        role: "system".to_string(),
                        content: action_name,
                        thread_sequence: 0,
                        external_id: Some(format!("cron:{}:{}", job.id, now_ts)),
                        metadata,
                        embedding: None,
                        summary_text: None,
                        is_summary: false,
                        msg_type: "cron".to_string(),
                        msg_subtype: Some(cron_name),
                        processing_time_ms: None,
                        token_usage: None,
                    };
                    queries::create_message(pool, &msg).await?;
                }
                Err(err_msg) => {
                    report_action_failure(pool, job, display_name, &action_name, err_msg, thread_id, now_ts).await?;
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
                    let (action_name, exec_result) = resolve_and_execute_action(
                        pool, data_dir, mcp_registry, app_context,
                        &job, display_name, action_id,
                    ).await;

                    if let Err(err_msg) = exec_result {
                        // Action failed — now create thread + report error
                        let cron_channel = ensure_cron_channel(pool).await?;
                        let thread = queries::create_thread(
                            pool,
                            "cron",
                            cron_channel.id,
                            "cron",
                            None,
                            None,
                            None,
                            Some(&job.id),
                            &job.planning_mode,
                        )
                        .await?;
                        report_action_failure(pool, &job, display_name, &action_name, err_msg, thread.id, now.timestamp()).await?;
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
                        None,
                        None,
                        None,
                        Some(&job.id),
                        &job.planning_mode,
                    )
                    .await?;

                    if let Err(e) = run_action_and_report(
                        pool, data_dir, mcp_registry, app_context,
                        &job, display_name, action_id, thread.id, now.timestamp(),
                    ).await {
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
        let prof = profile_registry.get(&profile_name).cloned().unwrap_or_else(|| {
            crate::profile::Profile::default("default")
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

        // ── Create a thread with cause='cron' ──
        let planning_mode = queries::resolve_thread_planning_mode(
            channel.metadata.get("planning_mode").and_then(|v| v.as_str()).unwrap_or(""),
            &job.planning_mode,
            "cron",
            &std::env::var("PLANNING_MODE").unwrap_or_else(|_| "auto_subtasks".to_string()),
        );
        let thread = match queries::create_thread(
            pool,
            "cron",
            channel.id,
            &profile_name,
            provider.as_deref(),
            model.as_deref(),
            None,
            Some(&job.id),
            &planning_mode,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                error!(
                    "[cron-scheduler] Failed to create thread for job '{}': {:?}",
                    display_name, e
                );
                let new_next = calculate_next_run(&job.schedule, &now);
                release_job(pool, &job.id, &now, &new_next).await?;
                continue;
            }
        };

        // ── Add a cause message with the prompt content ──
        let subtype = job.name.clone().unwrap_or_default();
        let cause_msg = MessageNew {
            thread_id: thread.id,
            role: "cause".to_string(),
            content: job.prompt.clone().unwrap_or_default(),
            thread_sequence: 0,
            external_id: Some(format!("cron:{}:{}", job.id, now.timestamp())),
            metadata: serde_json::json!({
                "cron_job_id": job.id,
                "cron_job_name": job.name,
                "cron_display_name": display_name,
                "scheduled_at": job.schedule,
                "channel_id": channel.id,
                "profile": profile_name,
                "instruction_file": job.instruction_file.as_deref().unwrap_or(""),
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "cron".to_string(),
            msg_subtype: Some(subtype),
            processing_time_ms: None,
            token_usage: None,
        };

        match queries::create_cause_and_set_pending(pool, &cause_msg).await {
            Ok(created) => {
                info!(
                    "[cron-scheduler] Created cause message {} and set thread {} pending for job '{}'",
                    created.id, thread.id, display_name
                );
            }
            Err(e) => {
                error!(
                    "[cron-scheduler] Failed to create cause message for job '{}': {:?}",
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
async fn fetch_due_jobs(pool: &PgPool) -> Result<Vec<CronJobDueRow>> {
    let rows: Vec<CronJobDueRow> = sql_forge!(
        CronJobDueRow,
        r#"
        SELECT id, name, display_name, schedule, prompt, channel_id, profile, mode, action_id, silent, instruction_file, planning_mode
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
) -> Result<()> {
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
async fn ensure_cron_channel(pool: &PgPool) -> Result<crate::models::Channel> {
    // First try to find existing cron channel
    if let Ok(Some(ch)) = queries::get_channel_by_platform_and_resource(pool, "cron", "cron-session").await {
        return Ok(ch);
    }
    queries::create_channel(pool, "cron-session", "cron", "cron-default", "cron", "cron-session").await
        .map_err(|e| anyhow::anyhow!("Failed to create cron channel: {:#}", e))
}

/// Resolve and execute an action, returning (action_name, result).
async fn resolve_and_execute_action(
    pool: &PgPool,
    data_dir: &str,
    mcp_registry: &McpRegistry,
    app_context: &AppContext,
    job: &CronJobDueRow,
    display_name: &str,
    action_id: &str,
) -> (String, Result<(), String>) {
    match action_id {
        "builtin_kanban_dispatcher" => {
            let name = "Kanban Dispatcher".to_string();
            let r = run_kanban_dispatcher(pool, data_dir)
                .await
                .map_err(|e| format!("{:#}", e));
            (name, r)
        }
        "builtin_relevance_indexer" => {
            let name = "Relevance Indexer".to_string();
            let r = crate::relevance::run_relevance_indexer(pool, data_dir)
                .await
                .map_err(|e| format!("{:#}", e));
            (name, r)
        }
        "builtin_hindsight_populator" => {
            let name = "Hindsight Populator".to_string();
            let r = crate::hindsight_populator::run_hindsight_populator(pool, data_dir)
                .await
                .map(|summary| {
                    tracing::info!("[hindsight-populator] {}", summary);
                })
                .map_err(|e| format!("{:#}", e));
            (name, r)
        }
        other => {
            match queries::get_action(pool, other).await {
                Ok(Some(action)) => {
                    let call = McpToolCall {
                        id: String::new(),
                        name: action.tool_name.clone(),
                        arguments: action.params.clone(),
                    };
                    let r = match mcp_registry.execute(&call, app_context.clone()).await {
                        Ok(result) => {
                            if result.is_error { Err(result.content) } else { Ok(()) }
                        }
                        Err(e) => Err(format!("{:#}", e)),
                    };
                    (action.name, r)
                }
                Ok(None) => (other.to_string(), Err(format!("Action '{}' not found", other))),
                Err(e) => (other.to_string(), Err(format!("DB lookup failed: {:#}", e))),
            }
        }
    }
}

/// Report an action failure by setting thread to failed and writing seq-0/seq-1 messages.
async fn report_action_failure(
    pool: &PgPool,
    job: &CronJobDueRow,
    display_name: &str,
    action_name: &str,
    err_msg: String,
    thread_id: i64,
    now_ts: i64,
) -> anyhow::Result<()> {
    let cron_name = job.name.clone().unwrap_or_default();
    let err_subtype = if let Some(code) = extract_error_code(&err_msg) {
        Some(code)
    } else {
        None
    };
    let metadata = serde_json::json!({
        "cron_job_id": job.id,
        "cron_job_name": job.name,
        "cron_display_name": display_name,
        "scheduled_at": job.schedule,
    });

    sql_forge!(
        "UPDATE threads SET status = 'failed', terminal = true WHERE id = :id",
        ( :id = thread_id )
    )
    .execute(pool)
    .await?;

    // seq-0: action name
    let msg0 = MessageNew {
        thread_id,
        role: "system".to_string(),
        content: action_name.to_string(),
        thread_sequence: 0,
        external_id: Some(format!("cron:{}:{:?}", job.id, now_ts)),
        metadata: metadata.clone(),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "cron".to_string(),
        msg_subtype: Some(cron_name.clone()),
        processing_time_ms: None,
        token_usage: None,
    };
    queries::create_message(pool, &msg0).await?;

    // seq-1: error details
    let msg1 = MessageNew {
        thread_id,
        role: "system".to_string(),
        content: err_msg,
        thread_sequence: 1,
        external_id: Some(format!("cron:{}:{:?}:error", job.id, now_ts)),
        metadata,
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "error".to_string(),
        msg_subtype: err_subtype,
        processing_time_ms: None,
        token_usage: None,
    };
    queries::create_message(pool, &msg1).await?;

    Ok(())
}

/// Run the kanban_dispatcher: move all 'todo' tasks to 'ready' by creating
/// threads and messages for each, respecting dependencies and ordering by
/// priority DESC, position ASC.
pub async fn run_kanban_dispatcher(pool: &PgPool, data_dir: &str) -> Result<()> {
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
                    warn!("[kanban-dispatcher] No default cron channel found, skipping task '{}'", t.id);
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
        let profile_name = t
            .profile
            .clone()
            .unwrap_or(channel.current_profile.clone());
        if profile_name.is_empty() {
            warn!(
                "[kanban-dispatcher] No profile resolved for task '{}' (channel {})",
                t.id, task_channel_id
            );
            continue;
        }

        // Resolve provider+model: channel → profile → env vars
        let profile_registry = crate::profile::ProfileRegistry::new(data_dir);
        let prof = profile_registry.get(&profile_name).cloned().unwrap_or_else(|| {
            crate::profile::Profile::default("default")
        });

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
                    "[kanban-dispatcher] Could not resolve config for task '{}' — empty profile",
                    t.id
                );
                continue;
            }
        };

        // Create thread with resolved profile, provider, and model
        let planning_mode = queries::resolve_thread_planning_mode(
            channel.metadata.get("planning_mode").and_then(|v| v.as_str()).unwrap_or(""),
            "",
            "kanban",
            &std::env::var("PLANNING_MODE").unwrap_or_else(|_| "auto_subtasks".to_string()),
        );
        let thread = match queries::create_thread(
            pool,
            "kanban",
            task_channel_id,
            &profile_name,
            Some(&provider),
            Some(&model),
            Some(&t.id),
            None,
            &planning_mode,
        )
        .await
        {
            Ok(th) => th,
            Err(e) => {
                warn!(
                    "[kanban-dispatcher] Failed to create thread for task '{}': {:?}",
                    t.id, e
                );
                continue;
            }
        };

        // Create cause message
        let content = t.body.as_deref().unwrap_or(&t.title);
        let content = if content.is_empty() {
            format!("Execute kanban task: {}", t.title)
        } else {
            content.to_string()
        };

        let cause_msg = crate::models::MessageNew {
            thread_id: thread.id,
            role: "cause".to_string(),
            content,
            thread_sequence: 0,
            external_id: Some(format!("kanban:{}", t.id)),
            metadata: serde_json::json!({
                "kanban_task_id": t.id,
                "kanban_task_title": t.title,
                "template": t.template.as_deref().unwrap_or(""),
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "kanban".to_string(),
            msg_subtype: Some(t.id.clone()),
            processing_time_ms: None,
            token_usage: None,
        };

        match queries::create_cause_and_set_pending(pool, &cause_msg).await {
            Ok(created) => {
                info!(
                    "[kanban-dispatcher] Created cause message {} / thread {} for task '{}'",
                    created.id, thread.id, t.id
                );
            }
            Err(e) => {
                warn!(
                    "[kanban-dispatcher] Failed to create cause for task '{}': {:?}",
                    t.id, e
                );
                continue;
            }
        }

        // Advance to ready
        sql_forge!(
            "UPDATE kanban_tasks SET status = 'ready', updated_at = NOW() WHERE id = :id",
            ( :id = &t.id )
        )
        .execute(pool)
        .await?;

        info!(
            "[kanban-dispatcher] Task '{}' advanced to ready (thread {})",
            t.id, thread.id
        );
        dispatched += 1;
    }

    info!(
        "[kanban-dispatcher] Dispatched {} tasks ({} skipped due to deps, {} total todo)",
        dispatched, skipped_deps, tasks.len()
    );

    Ok(())
}

/// Parse a cron expression and compute the next run after `now`.
fn calculate_next_run(expression: &str, now: &DateTime<Utc>) -> DateTime<Utc> {
    match Schedule::from_str(expression) {
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
///    model:        channel.current_model → profile.model → LLM_MODEL env
///
/// Returns `None` when the resolved profile name is empty.
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
            std::env::var("LLM_PROVIDER")
                .unwrap_or_else(|_| "opencode-go".to_string())
        });

    let model = channel_model
        .filter(|s| !s.is_empty())
        .or_else(|| profile_model.filter(|s| !s.is_empty()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            std::env::var("LLM_MODEL")
                .unwrap_or_else(|_| "deepseek-v4-flash".to_string())
        });

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

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Profile resolution ───

    #[test]
    fn test_profile_from_task() {
        let cfg = resolve_thread_config(
            Some("task-profile"),
            "channel-profile",
            None, None, None, None,
        );
        assert_eq!(cfg.unwrap().profile_name, "task-profile");
    }

    #[test]
    fn test_profile_from_channel_when_task_none() {
        let cfg = resolve_thread_config(
            None,
            "channel-profile",
            None, None, None, None,
        );
        assert_eq!(cfg.unwrap().profile_name, "channel-profile");
    }

    #[test]
    fn test_profile_from_channel_when_task_empty() {
        let cfg = resolve_thread_config(
            Some(""),
            "channel-profile",
            None, None, None, None,
        );
        assert_eq!(cfg.unwrap().profile_name, "channel-profile");
    }

    #[test]
    fn test_profile_empty_returns_none() {
        let cfg = resolve_thread_config(
            None,
            "",
            None, None, None, None,
        );
        assert!(cfg.is_none());
    }

    #[test]
    fn test_profile_empty_channel_with_empty_task_returns_none() {
        let cfg = resolve_thread_config(
            Some(""),
            "",
            None, None, None, None,
        );
        assert!(cfg.is_none());
    }

    // ─── Provider resolution ───

    #[test]
    fn test_provider_from_channel() {
        let cfg = resolve_thread_config(
            None, "default",
            Some("deepseek"), None,
            Some("anthropic"), None,
        );
        assert_eq!(cfg.unwrap().provider, "deepseek");
    }

    #[test]
    fn test_provider_falls_back_to_profile() {
        let cfg = resolve_thread_config(
            None, "default",
            None, None,
            Some("anthropic"), None,
        );
        assert_eq!(cfg.unwrap().provider, "anthropic");
    }

    #[test]
    fn test_provider_skip_empty_channel() {
        let cfg = resolve_thread_config(
            None, "default",
            Some(""), None,
            Some("anthropic"), None,
        );
        assert_eq!(cfg.unwrap().provider, "anthropic");
    }

    #[test]
    fn test_provider_channel_overrides_profile() {
        let cfg = resolve_thread_config(
            None, "default",
            Some("deepseek"), None,
            Some("anthropic"), None,
        );
        assert_eq!(cfg.unwrap().provider, "deepseek");
    }

    // ─── Model resolution ───

    #[test]
    fn test_model_from_channel() {
        let cfg = resolve_thread_config(
            None, "default",
            None, Some("deepseek-v4-flash"),
            None, Some("claude-3"),
        );
        assert_eq!(cfg.unwrap().model, "deepseek-v4-flash");
    }

    #[test]
    fn test_model_falls_back_to_profile() {
        let cfg = resolve_thread_config(
            None, "default",
            None, None,
            None, Some("claude-3"),
        );
        assert_eq!(cfg.unwrap().model, "claude-3");
    }

    #[test]
    fn test_model_channel_overrides_profile() {
        let cfg = resolve_thread_config(
            None, "default",
            None, Some("deepseek-v4-flash"),
            None, Some("claude-3"),
        );
        assert_eq!(cfg.unwrap().model, "deepseek-v4-flash");
    }

    #[test]
    fn test_model_skip_empty_channel() {
        let cfg = resolve_thread_config(
            None, "default",
            None, Some(""),
            None, Some("claude-3"),
        );
        assert_eq!(cfg.unwrap().model, "claude-3");
    }

    // ─── Combined scenarios ───

    #[test]
    fn test_full_resolution_chain() {
        let cfg = resolve_thread_config(
            Some("my-profile"),
            "channel-profile",
            Some("deepseek"), Some("deepseek-v4-flash"),
            None, None,
        );
        let c = cfg.unwrap();
        assert_eq!(c.profile_name, "my-profile");
        assert_eq!(c.provider, "deepseek");
        assert_eq!(c.model, "deepseek-v4-flash");
    }

    #[test]
    fn test_full_fallback_all_from_channel() {
        let cfg = resolve_thread_config(
            None, "chan-profile",
            Some("deepseek"), Some("deepseek-v4-flash"),
            None, None,
        );
        let c = cfg.unwrap();
        assert_eq!(c.profile_name, "chan-profile");
        assert_eq!(c.provider, "deepseek");
        assert_eq!(c.model, "deepseek-v4-flash");
    }

    #[test]
    fn test_full_fallback_all_from_profile() {
        let cfg = resolve_thread_config(
            None, "prof-profile",
            None, None,
            Some("anthropic"), Some("claude-3"),
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
            None, "default",
            Some("deepseek"), Some("deepseek-v4-flash"),
            Some("anthropic"), Some("claude-3"),
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
        let cfg = resolve_thread_config(
            None, "default",
            None, None,
            None, None,
        );
        // resolve_thread_config always returns Some as long as profile is non-empty
        // (it falls back to env vars). The *agent* checks thread.provider directly.
        let c = cfg.unwrap();
        // The strings should come from env var fallbacks (not None/empty)
        assert!(!c.provider.is_empty(), "provider must not be empty even after full fallback");
        assert!(!c.model.is_empty(), "model must not be empty even after full fallback");
    }
}
