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
}
