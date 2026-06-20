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
use crate::models::MessageNew;

/// Central list of all known direct-mode task types.
/// Add new variants here, then add a match arm in `handle_direct_task`.
/// Keep in sync with DIRECT_TASK_TYPES in dashboard server (server/routes/schedule.ts).
pub const DIRECT_TASK_TYPES: &[(&str, &str)] = &[
    ("kanban_dispatcher", "Kanban Dispatcher"),
    ("relevance_indexer", "Relevance Indexer"),
];

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
    direct_task_type: Option<String>,
}

/// Spawn the cron scheduler loop as a background task.
pub fn spawn(pool: PgPool, data_dir: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("[cron-scheduler] Starting cron scheduler loop");

        loop {
            if let Err(e) = tick(&pool, &data_dir).await {
                error!("[cron-scheduler] Tick failed: {:?}", e);
            }
            sleep(Duration::from_secs(30)).await;
        }
    })
}

/// One tick: find due jobs, claim one atomically, fire it, release.
async fn tick(pool: &PgPool, data_dir: &str) -> Result<()> {
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

        // ── Direct mode: run predefined task without agent ──
        let job_mode = job.mode.as_deref().unwrap_or("agentic");
        if job_mode == "direct" {
            if let Some(task_type) = job.direct_task_type.as_deref() {
                match task_type {
                    "kanban_dispatcher" => {
                        info!(
                            "[cron-scheduler] Running kanban_dispatcher for job '{}'",
                            display_name
                        );
                        run_kanban_dispatcher(pool).await?;
                    }
                    "relevance_indexer" => {
                        info!(
                            "[cron-scheduler] Running relevance_indexer for job '{}'",
                            display_name
                        );
                        crate::relevance::run_relevance_indexer(pool, data_dir).await?;
                    }
                    other => {
                        let known: Vec<&str> = DIRECT_TASK_TYPES.iter().map(|(v, _)| *v).collect();
                        warn!(
                            "[cron-scheduler] Unknown direct_task_type '{}' for job '{}'. Known types: {}",
                            other, display_name, known.join(", ")
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
        let provider = channel.current_provider.clone()
            .or_else(|| prof.provider.clone())
            .or_else(|| Some(std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "opencode-go".to_string())));
        let model = channel.current_model.clone()
            .or_else(|| prof.model.clone())
            .or_else(|| Some(std::env::var("LLM_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string())));

        // ── Create a thread with cause='cron' ──
        let thread = match queries::create_thread(
            pool,
            "cron",
            channel.id,
            &profile_name,
            provider.as_deref(),
            model.as_deref(),
            None,
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
        SELECT id, name, display_name, schedule, prompt, channel_id, profile, mode, direct_task_type
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
    queries::create_channel(pool, "cron-session", "cron", "cron", "cron", "cron-session").await
}

/// Run the kanban_dispatcher: move all 'todo' tasks to 'ready' by creating
/// threads and messages for each, respecting dependencies and ordering by
/// priority DESC, position ASC.
async fn run_kanban_dispatcher(pool: &PgPool) -> Result<()> {
    #[derive(Debug, FromRow)]
    struct TodoTaskRow {
        id: String,
        title: String,
        body: Option<String>,
        channel_id: Option<i64>,
        profile: Option<String>,
    }

    let tasks: Vec<TodoTaskRow> = sql_forge!(
        TodoTaskRow,
        r#"
        SELECT id, title, body, channel_id, profile
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

        // Resolve profile
        let profile_name = if let Some(ref p) = t.profile {
            p.clone()
        } else {
            match queries::find_channel_by_id(pool, task_channel_id).await {
                Ok(Some(ch)) => ch.current_profile.clone(),
                _ => "default".to_string(),
            }
        };

        // Create thread
        let thread = match queries::create_thread(
            pool,
            "kanban",
            task_channel_id,
            &profile_name,
            None,
            None,
            Some(&t.id),
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
