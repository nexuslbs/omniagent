//! Cron scheduler — polls `cron_jobs` table and fires due jobs by inserting
//! messages into a dedicated cron channel.
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
use sqlx::PgPool;
use std::str::FromStr;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

use crate::db::types as queries;
use crate::models::{MessageNew, MessageStatus};

/// Spawn the cron scheduler loop as a background task.
pub fn spawn(pool: PgPool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("[cron-scheduler] Starting cron scheduler loop");

        loop {
            if let Err(e) = tick(&pool).await {
                error!("[cron-scheduler] Tick failed: {:?}", e);
            }
            sleep(Duration::from_secs(30)).await;
        }
    })
}

/// One tick: find due jobs, claim one atomically, fire it, release.
async fn tick(pool: &PgPool) -> Result<()> {
    let jobs = fetch_due_jobs(pool).await?;

    for job in jobs {
        let now = Utc::now();
        let display_name = if job.display_name.is_empty() {
            &job.name
        } else {
            &job.display_name
        };

        // ── Atomic claim: only one tick can claim this job ──
        let claimed = sql_forge!(
            r#"
            UPDATE cron_jobs
            SET running = true
            WHERE id = :id
              AND NOT running
            "#,
            ( :id = &job.id )
        )
        .execute(pool)
        .await?;

        if claimed.rows_affected() == 0 {
            // Another tick already claimed this job — skip
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

        // ── Ensure cron channel exists ──
        let channel = ensure_cron_channel(pool).await?;

        // ── Insert a pending message for this job ──
        let subtype = job.name.clone();
        let msg = MessageNew {
            channel_id: channel.id,
            role: "system".to_string(),
            content: job.prompt.clone(),
            status: MessageStatus::Pending,
            thread_id: None,  // will be set to message id by init_thread_root
            thread_sequence: 0,
            external_id: Some(format!("cron:{}:{}", job.id, now.timestamp())),
            metadata: serde_json::json!({
                "cron_job_id": job.id,
                "cron_job_name": job.name,
                "cron_display_name": display_name,
                "scheduled_at": job.schedule,
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "cron".to_string(),
            msg_subtype: Some(subtype),
            iteration_count: 0,
            profile: "default".to_string(),
            provider: None,
            model: None,
            processing_time_ms: None,
            token_usage: None,
        };

        let msg_result = queries::init_thread_root(pool, &msg).await;

        // ── Release claim and update timestamps ──
        let new_next = calculate_next_run(&job.schedule, &now);
        release_job(pool, &job.id, &now, &new_next).await?;

        match msg_result {
            Ok(created) => {
                info!(
                    "[cron-scheduler] Inserted message {} for job '{}'",
                    created.id, display_name
                );
            }
            Err(e) => {
                error!(
                    "[cron-scheduler] Failed to insert message for job '{}': {:?}",
                    display_name, e
                );
            }
        }
    }

    Ok(())
}

/// Fetch enabled jobs whose next_run_at is due (null or ≤ now).
async fn fetch_due_jobs(pool: &PgPool) -> Result<Vec<CronJobRow>> {
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        r#"
        SELECT id, name, display_name, schedule, prompt
        FROM cron_jobs
        WHERE enabled = true
          AND (next_run_at IS NULL OR next_run_at <= NOW())
        ORDER BY created_at ASC
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, name, display_name, schedule, prompt)| CronJobRow {
            id,
            name,
            display_name,
            schedule,
            prompt,
        })
        .collect())
}

/// Release the running flag and update timestamps.
async fn release_job(
    pool: &PgPool,
    job_id: &str,
    last_run: &DateTime<Utc>,
    next_run: &DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE cron_jobs
        SET running = false,
            last_run_at = $1::timestamptz,
            next_run_at = $2::timestamptz
        WHERE id = $3
        "#,
    )
    .bind(last_run.to_rfc3339())
    .bind(next_run.to_rfc3339())
    .bind(job_id)
    .execute(pool)
    .await?;

    Ok(())
}

struct CronJobRow {
    id: String,
    name: String,
    display_name: String,
    schedule: String,
    prompt: String,
}

/// Ensure a cron channel exists (upsert on conflict).
async fn ensure_cron_channel(pool: &PgPool) -> Result<crate::models::Channel> {
    queries::create_channel(pool, "cron-default", "cron", "cron-default", "cron").await
}

/// Parse a cron expression and compute the next run after `now`.
fn calculate_next_run(expression: &str, now: &DateTime<Utc>) -> DateTime<Utc> {
    match Schedule::from_str(expression) {
        Ok(schedule) => {
            if let Some(next) = schedule.upcoming(Utc).next() {
                next
            } else {
                warn!(
                    "[cron-scheduler] Expression '{}' produced no upcoming times, defaulting to +1h",
                    expression
                );
                *now + chrono::Duration::hours(1)
            }
        }
        Err(e) => {
            warn!(
                "[cron-scheduler] Failed to parse cron expression '{}': {}. Defaulting to +1h.",
                expression, e
            );
            *now + chrono::Duration::hours(1)
        }
    }
}
