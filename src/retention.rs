//! Data retention: periodic SOFT delete and HARD delete.
//!
//! Both operations run through EXACTLY the same code path from two triggers:
//!
//!  * the in-process daily scheduler (one run per operation per day), and
//!  * the imperative HTTP API
//!    (`POST /api/retention/soft-delete`, `POST /api/retention/hard-delete`).
//!
//! Disabled convention (operator amendment 2): a `days` value of `None`
//! (empty/unset) or `Some(0)` DISABLES that operation: it deletes nothing and
//! reports `status: "disabled"` (never an error). Only `> 0` enables it.
//!
//! No-defaults convention (operator amendment 3): neither
//! `delete_after_days_soft` nor `delete_after_days_hard` has a default value.
//! Both are empty/unset out of the box, so both operations are disabled until
//! an operator explicitly sets a value `> 0`.
//!
//! Both operations are bounded (batched DELETE ... LIMIT) and serialized by a
//! process-wide lock, so a manual trigger arriving while the daily run (or
//! another trigger) is in flight waits instead of deleting the same rows twice
//! concurrently.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Instant;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::Mutex;

use crate::error::AppResult;

/// Automatic interval: each operation runs ONCE PER DAY.
pub const DAILY_INTERVAL_SECS: u64 = 86_400;

/// Rows per DELETE statement: a run never holds a huge single statement/lock.
const BATCH_SIZE: i64 = 1_000;
/// Hard cap on batches per table per run, so a run always terminates.
const MAX_BATCHES: u32 = 10_000;

/// Soft-delete eligibility: old messages EXCEPT the first (`min(thread_sequence)`)
/// and the last (`max(thread_sequence)`) message of every thread.
const SOFT_MESSAGE_ELIGIBLE: &str = "SELECT m.id FROM messages m \
     JOIN (SELECT thread_id, MIN(thread_sequence) AS min_seq, MAX(thread_sequence) AS max_seq \
             FROM messages GROUP BY thread_id) e ON e.thread_id = m.thread_id \
     WHERE m.created_at < $1 AND m.thread_sequence <> e.min_seq AND m.thread_sequence <> e.max_seq";

/// Hard-delete eligibility: every message older than the cutoff.
const HARD_MESSAGE_ELIGIBLE: &str = "SELECT id FROM messages WHERE created_at < $1";

// ── Report / status ────────────────────────────────────────────────────────

/// Result summary of one retention run (also returned by the trigger API).
#[derive(Debug, Clone, Serialize)]
pub struct RetentionReport {
    /// "soft_delete" | "hard_delete"
    pub operation: String,
    /// "disabled" | "completed"
    pub status: String,
    /// false when the operation is disabled (0/empty), true otherwise.
    pub enabled: bool,
    /// Configured retention horizon (None = empty/unset = disabled).
    pub days: Option<u32>,
    /// Rows deleted per table (only tables actually touched are listed).
    pub rows_deleted: BTreeMap<String, u64>,
    pub total_deleted: u64,
    pub duration_ms: u64,
}

impl RetentionReport {
    fn new(operation: &str, days: Option<u32>) -> Self {
        Self {
            operation: operation.to_string(),
            status: "disabled".to_string(),
            enabled: !is_disabled(days),
            days,
            rows_deleted: BTreeMap::new(),
            total_deleted: 0,
            duration_ms: 0,
        }
    }

    fn finish(mut self, started: Instant) -> Self {
        self.total_deleted = self.rows_deleted.values().sum();
        self.duration_ms = started.elapsed().as_millis() as u64;
        self
    }
}

/// Schedule + last-run view of one retention operation.
#[derive(Debug, Clone, Serialize)]
pub struct RetentionSchedule {
    pub interval_secs: u64,
    pub runs_per_day: u32,
    pub days: Option<u32>,
    pub enabled: bool,
    pub last_run: Option<RetentionReport>,
}

/// Status of both retention operations (served by `GET /api/retention/status`).
#[derive(Debug, Clone, Serialize)]
pub struct RetentionStatus {
    pub soft_delete: RetentionSchedule,
    pub hard_delete: RetentionSchedule,
}

/// The interval between automatic runs, overridable via
/// `OMNIAGENT_RETENTION_INTERVAL_SECS` (test/dev only; default = one run per day).
pub fn daily_interval_secs() -> u64 {
    std::env::var("OMNIAGENT_RETENTION_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DAILY_INTERVAL_SECS)
}

/// `0` or empty/unset disables the operation.
pub fn is_disabled(days: Option<u32>) -> bool {
    matches!(days, None | Some(0))
}

fn disabled_report(operation: &str, days: Option<u32>, started: Instant) -> RetentionReport {
    tracing::info!(
        "Retention {operation}: DISABLED ({} is empty or 0) - no rows deleted",
        setting_name(operation)
    );
    RetentionReport::new(operation, days).finish(started)
}

fn setting_name(operation: &str) -> &'static str {
    if operation == "soft_delete" {
        "delete_after_days_soft"
    } else {
        "delete_after_days_hard"
    }
}

/// Last-run bookkeeping for the status endpoint.
fn last_runs() -> &'static std::sync::RwLock<BTreeMap<String, RetentionReport>> {
    static LAST: OnceLock<std::sync::RwLock<BTreeMap<String, RetentionReport>>> = OnceLock::new();
    LAST.get_or_init(|| std::sync::RwLock::new(BTreeMap::new()))
}

fn record(report: &RetentionReport) {
    match last_runs().write() {
        Ok(mut guard) => {
            guard.insert(report.operation.clone(), report.clone());
        }
        Err(e) => tracing::warn!("Retention status lock poisoned: {e}"),
    }
}

fn last_run(operation: &str) -> Option<RetentionReport> {
    last_runs()
        .read()
        .ok()
        .and_then(|guard| guard.get(operation).cloned())
}

/// Schedule + last run for both operations (config values are read live).
pub fn status(soft_days: Option<u32>, hard_days: Option<u32>) -> RetentionStatus {
    let interval = daily_interval_secs();
    let entry = |op: &str, days: Option<u32>| RetentionSchedule {
        interval_secs: interval,
        runs_per_day: if interval >= DAILY_INTERVAL_SECS {
            1
        } else {
            (DAILY_INTERVAL_SECS / interval) as u32
        },
        days,
        enabled: !is_disabled(days),
        last_run: last_run(op),
    };
    RetentionStatus {
        soft_delete: entry("soft_delete", soft_days),
        hard_delete: entry("hard_delete", hard_days),
    }
}

/// Serializes soft/hard runs: a trigger arriving while another run is in flight
/// waits (no concurrent duplicate delete of the same rows).
fn run_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ── Public entry points (shared by scheduler and API) ──────────────────────

/// Run the SOFT delete once. Same code path for the daily run and the API.
pub async fn run_soft_delete(pool: &PgPool, days: Option<u32>) -> AppResult<RetentionReport> {
    let report = soft_delete(pool, days).await?;
    record(&report);
    Ok(report)
}

/// Run the HARD delete once. Same code path for the daily run and the API.
pub async fn run_hard_delete(pool: &PgPool, days: Option<u32>) -> AppResult<RetentionReport> {
    let report = hard_delete(pool, days).await?;
    record(&report);
    Ok(report)
}

/// SOFT delete: removes `messages` older than `days` days, EXCEPT the FIRST and
/// the LAST message of every thread. No-op (status "disabled") for 0/empty.
pub async fn soft_delete(pool: &PgPool, days: Option<u32>) -> AppResult<RetentionReport> {
    let started = Instant::now();
    if is_disabled(days) {
        return Ok(disabled_report("soft_delete", days, started));
    }
    let days = days.unwrap_or(0);
    let _guard = run_lock().lock().await;
    let cutoff = cutoff(days);

    let mut tx = pool.begin().await?;
    disable_message_append_only_trigger(&mut tx).await?;
    // FK safety: NULL any column referencing a message row about to be deleted.
    null_message_references(&mut tx, cutoff, SOFT_MESSAGE_ELIGIBLE).await?;
    let deleted = batch_delete(
        &mut tx,
        &format!("DELETE FROM messages WHERE id IN ({SOFT_MESSAGE_ELIGIBLE} LIMIT $2)"),
        cutoff,
    )
    .await?;
    enable_message_append_only_trigger(&mut tx).await?;
    tx.commit().await?;

    let mut rows = BTreeMap::new();
    rows.insert("messages".to_string(), deleted);
    let report = {
        let mut r = RetentionReport::new("soft_delete", Some(days));
        r.status = "completed".to_string();
        r.rows_deleted = rows;
        r.finish(started)
    };
    tracing::info!(
        "Retention soft-delete: completed (older than {} days): {} rows, {} ms",
        days,
        report.total_deleted,
        report.duration_ms
    );
    Ok(report)
}

/// HARD delete: removes ALL rows older than `days` days in `kanban_history`,
/// `messages`, `secret_versions` (versions only, never `secrets`),
/// `thread_subtasks` and `threads`. No-op (status "disabled") for 0/empty.
pub async fn hard_delete(pool: &PgPool, days: Option<u32>) -> AppResult<RetentionReport> {
    let started = Instant::now();
    if is_disabled(days) {
        return Ok(disabled_report("hard_delete", days, started));
    }
    let days = days.unwrap_or(0);
    let _guard = run_lock().lock().await;
    let cutoff = cutoff(days);

    let mut tx = pool.begin().await?;

    // 1. kanban_history: no FK to kanban_tasks, prunable independently.
    let kanban_history = batch_delete(
        &mut tx,
        "DELETE FROM kanban_history WHERE id IN \
         (SELECT id FROM kanban_history WHERE created_at < $1 LIMIT $2)",
        cutoff,
    )
    .await?;

    // 2. messages: (a) every message older than the cutoff and (b) the
    //    messages of threads that are about to be deleted (FK
    //    messages.thread_id -> threads.id), so deleting a thread can never hit
    //    an FK error.
    disable_message_append_only_trigger(&mut tx).await?;
    null_message_references(&mut tx, cutoff, HARD_MESSAGE_ELIGIBLE).await?;
    let old_messages = batch_delete(
        &mut tx,
        &format!("DELETE FROM messages WHERE id IN ({HARD_MESSAGE_ELIGIBLE} LIMIT $2)"),
        cutoff,
    )
    .await?;
    let thread_messages = batch_delete(
        &mut tx,
        "DELETE FROM messages WHERE id IN \
         (SELECT m.id FROM messages m JOIN threads t ON t.id = m.thread_id \
          WHERE t.created_at < $1 LIMIT $2)",
        cutoff,
    )
    .await?;
    enable_message_append_only_trigger(&mut tx).await?;

    // 3. thread_subtasks: old rows + subtasks of the threads being deleted
    //    (FK thread_subtasks.thread_id -> threads.id: subtasks die before the
    //    thread).
    let thread_subtasks = batch_delete(
        &mut tx,
        "DELETE FROM thread_subtasks WHERE id IN \
         (SELECT s.id FROM thread_subtasks s \
          WHERE s.created_at < $1 OR s.thread_id IN (SELECT id FROM threads WHERE created_at < $1) \
          LIMIT $2)",
        cutoff,
    )
    .await?;

    // 4. secret_versions ONLY (FK secret_versions.secret_id -> secrets.id):
    //    the `secrets` rows themselves are NEVER deleted.
    let secret_versions = batch_delete(
        &mut tx,
        "DELETE FROM secret_versions WHERE id IN \
         (SELECT id FROM secret_versions WHERE created_at < $1 LIMIT $2)",
        cutoff,
    )
    .await?;

    // 5. threads self-FK: surviving threads whose parent is being deleted get
    //    parent_id = NULL (children before parents, so it can never error).
    sqlx::query(
        "UPDATE threads SET parent_id = NULL \
         WHERE parent_id IN (SELECT id FROM threads WHERE created_at < $1)",
    )
    .bind(cutoff)
    .execute(tx.as_mut())
    .await?;

    // 6. threads: all threads older than the cutoff.
    let threads = batch_delete(
        &mut tx,
        "DELETE FROM threads WHERE id IN \
         (SELECT id FROM threads WHERE created_at < $1 LIMIT $2)",
        cutoff,
    )
    .await?;

    tx.commit().await?;

    let mut rows = BTreeMap::new();
    rows.insert("kanban_history".to_string(), kanban_history);
    rows.insert("messages".to_string(), old_messages + thread_messages);
    rows.insert("thread_subtasks".to_string(), thread_subtasks);
    rows.insert("secret_versions".to_string(), secret_versions);
    rows.insert("threads".to_string(), threads);
    let report = {
        let mut r = RetentionReport::new("hard_delete", Some(days));
        r.status = "completed".to_string();
        r.rows_deleted = rows;
        r.finish(started)
    };
    tracing::info!(
        "Retention hard-delete: completed (older than {} days): {:?} ({} rows, {} ms)",
        days,
        report.rows_deleted,
        report.total_deleted,
        report.duration_ms
    );
    Ok(report)
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn cutoff(days: u32) -> DateTime<Utc> {
    Utc::now() - Duration::days(days as i64)
}

/// Delete in bounded batches (`LIMIT $2`). Returns the total rows deleted.
async fn batch_delete(
    tx: &mut Transaction<'_, Postgres>,
    sql: &str,
    cutoff: DateTime<Utc>,
) -> AppResult<u64> {
    let mut total = 0u64;
    for _ in 0..MAX_BATCHES {
        // Data (identifiers) is trusted schema metadata; values are bound.
        let affected = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(cutoff)
            .bind(BATCH_SIZE)
            .execute(tx.as_mut())
            .await?
            .rows_affected();
        total += affected;
        if affected < BATCH_SIZE as u64 {
            break;
        }
    }
    Ok(total)
}

async fn disable_message_append_only_trigger(tx: &mut Transaction<'_, Postgres>) -> AppResult<()> {
    // The append-only trigger blocks DELETE on messages; the age-based purge is
    // the sanctioned path, so it is disabled for the duration of this
    // transaction only (DDL is transactional: any failure rolls the re-enable
    // back together with the deletes).
    sqlx::query("ALTER TABLE messages DISABLE TRIGGER trg_messages_append_only")
        .execute(tx.as_mut())
        .await?;
    Ok(())
}

async fn enable_message_append_only_trigger(tx: &mut Transaction<'_, Postgres>) -> AppResult<()> {
    sqlx::query("ALTER TABLE messages ENABLE TRIGGER trg_messages_append_only")
        .execute(tx.as_mut())
        .await?;
    Ok(())
}

/// FK safety net: for every FK column in the schema that references
/// `messages(id)`, set the referencing value to NULL for the messages that are
/// about to be deleted (so no FK error can occur). Verified schema has no such
/// FK today, so this is normally a no-op - it keeps the purge correct if one is
/// ever introduced.
async fn null_message_references(
    tx: &mut Transaction<'_, Postgres>,
    cutoff: DateTime<Utc>,
    eligible: &str,
) -> AppResult<u64> {
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT tc.table_name, kcu.column_name \
           FROM information_schema.table_constraints tc \
           JOIN information_schema.key_column_usage kcu \
             ON kcu.constraint_name = tc.constraint_name \
            AND kcu.table_schema = tc.table_schema \
           JOIN information_schema.constraint_column_usage ccu \
             ON ccu.constraint_name = tc.constraint_name \
            AND ccu.table_schema = tc.table_schema \
          WHERE tc.constraint_type = 'FOREIGN KEY' \
            AND tc.table_schema = current_schema() \
            AND ccu.table_name = 'messages' \
            AND ccu.column_name = 'id'",
    )
    .fetch_all(tx.as_mut())
    .await?;

    let mut total = 0u64;
    for (table, column) in columns {
        let sql = format!(
            "UPDATE \"{table}\" SET \"{column}\" = NULL WHERE \"{column}\" IN ({eligible})"
        );
        // Identifiers come from information_schema; cutoff is a bound value.
        total += sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
            .bind(cutoff)
            .execute(tx.as_mut())
            .await?
            .rows_affected();
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_or_empty_is_disabled() {
        assert!(is_disabled(None));
        assert!(is_disabled(Some(0)));
        assert!(!is_disabled(Some(1)));
        assert!(!is_disabled(Some(365)));
    }

    #[test]
    fn disabled_operation_reports_disabled_with_zero_rows() {
        let report = RetentionReport::new("soft_delete", None);
        assert_eq!(report.status, "disabled");
        assert!(!report.enabled);
        assert_eq!(report.total_deleted, 0);
        assert!(report.rows_deleted.is_empty());
    }

    #[test]
    fn schedule_exposes_one_run_per_day_and_no_default() {
        let st = status(None, None);
        assert_eq!(st.soft_delete.interval_secs, DAILY_INTERVAL_SECS);
        assert_eq!(st.hard_delete.interval_secs, DAILY_INTERVAL_SECS);
        assert!(!st.soft_delete.enabled);
        assert!(!st.hard_delete.enabled);
        assert_eq!(st.soft_delete.days, None);
        assert_eq!(st.hard_delete.days, None);
    }
}
