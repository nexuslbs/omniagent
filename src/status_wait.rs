//! First-class waiting on kanban task / thread STATUS changes.
//!
//! Incident 2026-09-06/07 (threads 1136/1146): the operator asked the agent to
//! "listen to the S11 task and act when it goes done/blocked", but no tool
//! listened to kanban/thread status changes - builtin_wait-task and
//! builtin_poll-task only track background TOOL tasks (docker/ssh exec
//! processes), never kanban task or thread state. The agent therefore burned
//! 3x1h blind wait-task calls on a background watcher that had silently died,
//! while the task had actually already gone done.
//!
//! This module provides the missing capability: a BOUNDED wait that observes
//! the real status column of a kanban task or thread row and returns as soon
//! as the row enters one of the caller's target statuses (polling the primary
//! key every ~1s, so the wake-up is prompt - well under the <1-2 min
//! requirement - for every mutation source: the HTTP API, the external kanban
//! MCP subprocess, and in-process agent transitions all write the same DB
//! row). No event bus, no dispatch changes, no wall-clock burning in the
//! agent loop: one call, bounded by `timeout_secs`.

use crate::error::AppResult;
use sqlx::PgPool;
use std::time::{Duration, Instant};

/// What kind of row the wait observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEntity {
    /// A kanban task row (`kanban_tasks.status`).
    KanbanTask,
    /// A conversation thread row (`threads.status`).
    Thread,
}

impl WaitEntity {
    pub fn as_str(&self) -> &'static str {
        match self {
            WaitEntity::KanbanTask => "kanban_task",
            WaitEntity::Thread => "thread",
        }
    }
}

/// Outcome of a `wait_for_status` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitOutcome {
    pub entity: WaitEntity,
    /// The entity id that was waited on (kanban task id or thread id as text).
    pub id: String,
    /// True when the entity's status was observed in `until` (a match).
    pub reached: bool,
    /// The current status of the entity (None when the row does not exist).
    pub status: Option<String>,
    /// Seconds spent waiting.
    pub elapsed_secs: u64,
    /// Human-readable detail: why the wait ended.
    pub detail: String,
}

/// Parse a comma-separated status list (as passed to the `until` parameter of
/// the wait-for-status tool / HTTP long-poll endpoints) into a clean,
/// lower-cased, de-duplicated vector.
pub fn parse_until(raw: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let p = part.trim().to_lowercase();
        if p.is_empty() || seen.contains(&p) {
            continue;
        }
        seen.push(p);
    }
    seen
}

/// Fetch the current status of a kanban task or thread row.
async fn current_status(pool: &PgPool, entity: WaitEntity, id: &str) -> AppResult<Option<String>> {
    match entity {
        WaitEntity::KanbanTask => {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM kanban_tasks WHERE id = $1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await?;
            Ok(status)
        }
        WaitEntity::Thread => {
            // Thread ids are BIGINT; tolerate a non-numeric id by treating the
            // row as missing rather than erroring.
            let Ok(tid) = id.parse::<i64>() else {
                return Ok(None);
            };
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM threads WHERE id = $1")
                    .bind(tid)
                    .fetch_optional(pool)
                    .await?;
            Ok(status)
        }
    }
}

/// True when `thread_id` is an ACTIVE thread (`pending` or `processing`) of
/// kanban task `task_id` - i.e. that thread is what the task is currently
/// running. Mirrors the active-thread predicate used by the dispatcher and the
/// kanban API (`threads.task_id = <task> AND status IN ('pending',
/// 'processing')`), evaluated for ONE thread id so a task with several queued
/// threads can never hide the caller's own id.
pub async fn thread_runs_task(pool: &PgPool, task_id: &str, thread_id: i64) -> AppResult<bool> {
    let runs: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM threads \
         WHERE task_id = $1 AND id = $2 AND status IN ('pending', 'processing'))",
    )
    .bind(task_id)
    .bind(thread_id)
    .fetch_one(pool)
    .await?;
    Ok(runs)
}

/// Human-readable noun for an entity, used in guard messages.
fn entity_noun(entity: WaitEntity) -> &'static str {
    match entity {
        WaitEntity::KanbanTask => "task",
        WaitEntity::Thread => "thread",
    }
}

/// CORE GUARD against a self-referential wait (incident telegram thread
/// 4122/4126): an executor thread ran `wait_for_status(task_id=<its own task>,
/// timeout_s=420, until=done,blocked)`. A thread waiting for the completion of
/// the very task it is running can only be answered when that thread ends, so
/// the call blocks for the whole timeout while the agent polls pointlessly.
///
/// Pure decision core (no DB, so it is unit-testable): returns the refusal
/// message when the caller waits on the entity its own thread is running, else
/// `None`. `is_self` is resolved by [`self_wait_error`].
///
/// The guard only refuses when the wait would actually BLOCK: when the target
/// already sits in one of the `until` statuses (or the row does not exist) the
/// normal path resolves instantly, so that behaviour is preserved.
pub fn self_wait_refusal(
    entity: WaitEntity,
    id: &str,
    is_self: bool,
    current_status: Option<&str>,
    until: &[String],
    caller_thread_id: i64,
) -> Option<String> {
    if !is_self {
        return None;
    }
    let Some(cur) = current_status else {
        // Missing row: the normal path returns "not found" immediately.
        return None;
    };
    if until.iter().any(|u| u == cur) {
        // Already in a target status: the normal path returns "matched"
        // immediately (no hang, no polling), so it stays available.
        return None;
    }
    let noun = entity_noun(entity);
    Some(format!(
        "wait_for_status: cannot wait on {noun} '{id}' - its currently-running thread (#{caller_thread_id}) is the calling thread; a {noun} cannot wait for its own completion. Use wait_task / poll_task to wait for a background TOOL task (for example an ssh_run dispatched as a background task), not wait_for_status on your own {noun}."
    ))
}

/// DB-facing self-wait guard, called by the `wait_for_status` tool BEFORE it
/// enters the poll loop. Returns the refusal message when the calling thread is
/// the thread currently running the target entity, else `None` (normal
/// behaviour). `caller_thread_id` is `None` outside a thread execution (e.g.
/// the HTTP long-poll endpoints), where no self-wait is possible.
pub async fn self_wait_error(
    pool: &PgPool,
    entity: WaitEntity,
    id: &str,
    until: &[String],
    caller_thread_id: Option<i64>,
) -> AppResult<Option<String>> {
    let Some(caller) = caller_thread_id else {
        return Ok(None);
    };
    let is_self = match entity {
        WaitEntity::KanbanTask => thread_runs_task(pool, id, caller).await?,
        WaitEntity::Thread => id.trim().parse::<i64>().ok() == Some(caller),
    };
    if !is_self {
        return Ok(None);
    }
    let status = current_status(pool, entity, id).await?;
    Ok(self_wait_refusal(
        entity,
        id,
        true,
        status.as_deref(),
        until,
        caller,
    ))
}

/// Wait until `entity` `id` reaches one of the `until` statuses, the row
/// disappears, or `timeout_secs` elapses.
///
/// Checks the row's status every `poll_interval` (default callers use ~1s),
/// so the returned future resolves promptly after a real status transition
/// from any writer. Returns a structured outcome; a timeout is NOT an error -
/// the caller should re-check the real state and decide (re-wait in a bounded
/// chunk, or act on what it finds).
pub async fn wait_for_status(
    pool: &PgPool,
    entity: WaitEntity,
    id: &str,
    until: &[String],
    timeout_secs: u64,
    poll_interval: Duration,
) -> AppResult<WaitOutcome> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_secs);
    let wanted = until.join(",");

    loop {
        let status = current_status(pool, entity, id).await?;
        let elapsed = started.elapsed().as_secs();
        match status {
            None => {
                return Ok(WaitOutcome {
                    entity,
                    id: id.to_string(),
                    reached: false,
                    status: None,
                    elapsed_secs: elapsed,
                    detail: format!(
                        "{} '{}' not found; waited {}s for [{}]",
                        entity.as_str(),
                        id,
                        elapsed,
                        wanted
                    ),
                });
            }
            Some(cur) if until.iter().any(|u| u == &cur) => {
                return Ok(WaitOutcome {
                    entity,
                    id: id.to_string(),
                    reached: true,
                    status: Some(cur.clone()),
                    elapsed_secs: elapsed,
                    detail: format!(
                        "{} '{}' reached status '{}' (wanted [{}]) after {}s",
                        entity.as_str(),
                        id,
                        cur,
                        wanted,
                        elapsed
                    ),
                });
            }
            Some(_) => {}
        }
        if Instant::now() >= deadline {
            // Build the detail BEFORE moving `status` into the outcome.
            let detail = format!(
                "{} '{}' still in status {:?} after {}s timeout (wanted [{}])",
                entity.as_str(),
                id,
                status,
                elapsed,
                wanted
            );
            return Ok(WaitOutcome {
                entity,
                id: id.to_string(),
                reached: false,
                status,
                elapsed_secs: elapsed,
                detail,
            });
        }
        tokio::time::sleep(poll_interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_until_splits_commas_and_lowercases() {
        assert_eq!(parse_until("done,blocked"), vec!["done", "blocked"]);
        assert_eq!(parse_until(" Done ,  BLOCKED "), vec!["done", "blocked"]);
    }

    #[test]
    fn parse_until_dedupes_and_skips_empty() {
        assert_eq!(parse_until("done,done,,done"), vec!["done"]);
        assert_eq!(parse_until(" , , "), Vec::<String>::new());
        assert_eq!(parse_until(""), Vec::<String>::new());
    }

    #[test]
    fn wait_entity_as_str() {
        assert_eq!(WaitEntity::KanbanTask.as_str(), "kanban_task");
        assert_eq!(WaitEntity::Thread.as_str(), "thread");
    }

    #[test]
    fn self_wait_refusal_fires_for_the_calling_thread() {
        let until = parse_until("done,blocked");
        let msg = self_wait_refusal(
            WaitEntity::KanbanTask,
            "task_orchestrator_remote_test_blocked_on_the",
            true,
            Some("running"),
            &until,
            4122,
        )
        .expect("a thread waiting on the task it runs must be refused");
        assert!(msg.contains("task_orchestrator_remote_test_blocked_on_the"));
        assert!(msg.contains("#4122"));
        assert!(msg.contains("cannot wait"));
        assert!(
            msg.contains("wait_task"),
            "hints the right primitive: {msg}"
        );
        assert!(
            msg.contains("poll_task"),
            "hints the right primitive: {msg}"
        );
        assert!(!msg.contains('\u{2014}'), "no emdash in user-visible text");
    }

    #[test]
    fn self_wait_refusal_ignores_other_threads() {
        let until = parse_until("done");
        assert!(self_wait_refusal(
            WaitEntity::KanbanTask,
            "task_other",
            false,
            Some("running"),
            &until,
            7
        )
        .is_none());
        assert!(self_wait_refusal(
            WaitEntity::Thread,
            "8",
            false,
            Some("processing"),
            &until,
            7
        )
        .is_none());
    }

    #[test]
    fn self_wait_refusal_keeps_an_instant_match_available() {
        // The target already sits in `until`: the normal path returns
        // "matched" immediately, so no refusal (and no hang either way).
        let until = parse_until("running,done");
        assert!(self_wait_refusal(
            WaitEntity::KanbanTask,
            "task_self",
            true,
            Some("running"),
            &until,
            7
        )
        .is_none());
        // Missing row: the normal path returns "not found" immediately.
        assert!(self_wait_refusal(
            WaitEntity::KanbanTask,
            "task_missing",
            true,
            None,
            &until,
            7
        )
        .is_none());
    }

    #[test]
    fn self_wait_refusal_covers_the_thread_target_form() {
        let until = parse_until("completed,failed");
        let msg = self_wait_refusal(
            WaitEntity::Thread,
            "4122",
            true,
            Some("processing"),
            &until,
            4122,
        )
        .expect("a thread waiting on itself must be refused");
        assert!(msg.contains("thread '4122'"));
        assert!(msg.contains("#4122"));
    }

    /// Insert a kanban task in status 'running' served by ONE processing
    /// thread; returns (task_id, thread_id). Only touches rows it creates.
    async fn seed_running_task(pool: &PgPool, tag: &str) -> (String, i64) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let task_id = format!("task-self-wait-{}-{}-{}", tag, std::process::id(), n);
        let channel = format!("test-channel-self-wait-{}", std::process::id());
        sqlx::query(
            "INSERT INTO kanban_tasks (id, title, status, board, channel_id, profile, thread_status, created_at, updated_at)
             VALUES ($1, 'wait_for_status self-wait guard test', 'running', 'main', $2, 'test-profile', 'scheduled', NOW(), NOW())",
        )
        .bind(&task_id)
        .bind(&channel)
        .execute(pool)
        .await
        .expect("insert task");
        let thread_id: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile, task_id, workflow_step)
             VALUES ('processing', 'user', $1, 'test-profile', $2, 'running') RETURNING id",
        )
        .bind(&channel)
        .bind(&task_id)
        .fetch_one(pool)
        .await
        .expect("insert thread");
        (task_id, thread_id)
    }

    async fn cleanup_task(pool: &PgPool, task_id: &str) {
        let _ = sqlx::query("DELETE FROM threads WHERE task_id = $1")
            .bind(task_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM kanban_tasks WHERE id = $1")
            .bind(task_id)
            .execute(pool)
            .await;
    }

    /// Requirement 1: the guard fires for the task's own running thread and
    /// returns without any polling (skipped when DATABASE_URL is absent).
    #[tokio::test]
    async fn self_wait_guard_fires_immediately_for_the_tasks_own_thread() {
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = PgPool::connect(&db_url).await.expect("connect dev db");
        let (task_id, thread_id) = seed_running_task(&pool, "fires").await;

        let started = Instant::now();
        let refusal = self_wait_error(
            &pool,
            WaitEntity::KanbanTask,
            &task_id,
            &parse_until("done,blocked"),
            Some(thread_id),
        )
        .await
        .expect("guard query");
        let elapsed = started.elapsed();
        let msg = refusal.expect("self-wait must be refused");
        assert!(msg.contains(&task_id), "names the task: {msg}");
        assert!(
            msg.contains(&format!("#{thread_id}")),
            "names the calling thread: {msg}"
        );
        assert!(msg.contains("wait_task"), "hints the primitive: {msg}");
        assert!(
            elapsed < Duration::from_secs(2),
            "guard must return immediately, no polling: {elapsed:?}"
        );

        cleanup_task(&pool, &task_id).await;
    }

    /// Requirement 2: a DIFFERENT caller keeps the normal wait semantics
    /// (e.g. an ssh_run dispatched as a background task) - the guard stays
    /// silent and the wait still times out normally.
    #[tokio::test]
    async fn self_wait_guard_leaves_other_waiters_on_the_normal_path() {
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = PgPool::connect(&db_url).await.expect("connect dev db");
        let (task_id, _thread_id) = seed_running_task(&pool, "other").await;

        let refusal = self_wait_error(
            &pool,
            WaitEntity::KanbanTask,
            &task_id,
            &parse_until("done"),
            Some(9_999_999),
        )
        .await
        .expect("guard query");
        assert!(refusal.is_none(), "other waiters are unaffected");

        let outcome = wait_for_status(
            &pool,
            WaitEntity::KanbanTask,
            &task_id,
            &parse_until("done"),
            1,
            Duration::from_millis(200),
        )
        .await
        .expect("wait ok");
        assert!(!outcome.reached);
        assert_eq!(outcome.status.as_deref(), Some("running"));

        cleanup_task(&pool, &task_id).await;
    }
}
