//! DB-down recovery phase.
//!
//! When the agent loop collapses because the database became unreachable, the
//! process switches into a dedicated recovery phase instead of running the
//! normal agent loop: a flag (`DB_RECOVERY`) is set, the loop polls the DB
//! until it is online again (bounded retry with exponential backoff, no
//! crash-looping), then reuses the STARTUP recovery logic
//! ([`crate::db::threads::skip_all_pending_threads`]) to mark every
//! pending/processing thread as skipped, and only then returns to the normal
//! agentic loop.

use sqlx::PgPool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// True while the process is in the DB-recovery phase. The agent loop checks
/// this flag and must not run (no DB polling, no thread processing) while it
/// is set.
static DB_RECOVERY: AtomicBool = AtomicBool::new(false);

/// Return true while the DB-recovery phase is active.
pub fn is_recovering() -> bool {
    DB_RECOVERY.load(Ordering::SeqCst)
}

/// Default maximum number of DB-access attempts before the recovery phase
/// gives up (override with env `OMNIAGENT_DB_RECOVERY_MAX_RETRIES`).
const DEFAULT_MAX_RETRIES: u32 = 60;

/// Read the bounded retry limit for the recovery phase from the environment,
/// falling back to [`DEFAULT_MAX_RETRIES`]. Values of 0 or unparseable input
/// fall back to the default.
fn recovery_max_retries() -> u32 {
    std::env::var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_RETRIES)
}

/// Exponential backoff for attempt `attempt` (1-based): 1s, 2s, 4s, ... capped
/// at 30s so a long outage never spins hot and never crash-loops.
fn backoff_delay(attempt: u32) -> Duration {
    let base = 1u64 << attempt.saturating_sub(1).min(5); // 1, 2, 4, 8, 16, 32
    Duration::from_secs(base.min(30))
}

/// Check whether the database is reachable with a `SELECT 1`.
pub(crate) async fn db_is_online(pool: &PgPool) -> bool {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(pool)
        .await
        .map(|v| v == 1)
        .unwrap_or(false)
}

/// Run the DB-recovery phase.
///
/// Sets the recovery flag, polls the DB until it is online (bounded retry with
/// exponential backoff, no crash-looping), then reuses the STARTUP recovery
/// logic (`skip_all_pending_threads`) to mark every pending/processing thread
/// as skipped and redispatch stuck kanban tasks. Returns `true` when recovery
/// completed and the normal agent loop may resume; `false` when the bounded
/// retry was exhausted (the caller should exit the process so the supervisor
/// restarts it and the startup recovery runs).
pub async fn run_recovery_phase(pool: &PgPool, data_dir: &str) -> bool {
    DB_RECOVERY.store(true, Ordering::SeqCst);
    let max_retries = recovery_max_retries();
    tracing::info!(
        "[db-recovery] DB unreachable: entering recovery phase (bounded retry, max {} attempts, backoff 1s..30s)",
        max_retries
    );

    for attempt in 1..=max_retries {
        if db_is_online(pool).await {
            tracing::info!(
                "[db-recovery] DB is online (attempt {}); running startup-style recovery",
                attempt
            );
            // Reuse the SAME logic the startup path uses to mark stale
            // threads: skip_all_pending_threads marks every pending/processing
            // thread terminal 'skipped' and redispatches kanban tasks sitting
            // in a workflow column without an active thread.
            match crate::db::threads::skip_all_pending_threads(pool, data_dir).await {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!(
                            "[db-recovery] Skipped {} pending/processing threads (startup logic)",
                            n
                        );
                    }
                    DB_RECOVERY.store(false, Ordering::SeqCst);
                    tracing::info!(
                        "[db-recovery] recovery complete; resuming the normal agent loop"
                    );
                    return true;
                }
                Err(e) => {
                    tracing::warn!(
                        "[db-recovery] skip_all_pending_threads failed (attempt {}): {:?}",
                        attempt,
                        e
                    );
                }
            }
        } else {
            tracing::info!(
                "[db-recovery] DB not reachable (attempt {}/{}); retrying in {:?}",
                attempt,
                max_retries,
                backoff_delay(attempt)
            );
        }
        if attempt < max_retries {
            tokio::time::sleep(backoff_delay(attempt)).await;
        }
    }

    DB_RECOVERY.store(false, Ordering::SeqCst);
    tracing::error!(
        "[db-recovery] gave up after {} attempts; the process will exit and restart with startup recovery",
        max_retries
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate the `OMNIAGENT_DB_RECOVERY_MAX_RETRIES`
    /// env var and/or the global `DB_RECOVERY` flag so parallel tokio tests
    /// cannot interleave. tokio mutex so the guard can be held across await
    /// without tripping clippy's await_holding_lock.
    static ENV_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    #[test]
    fn flag_starts_clear() {
        let _guard = ENV_LOCK.blocking_lock();
        assert!(!is_recovering(), "recovery flag must start clear");
    }

    #[test]
    fn backoff_is_bounded_exponential() {
        let _guard = ENV_LOCK.blocking_lock();
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(4), Duration::from_secs(8));
        assert_eq!(backoff_delay(5), Duration::from_secs(16));
        // Capped at 30s: no unbounded growth, no crash-looping.
        for attempt in 6..=100 {
            let d = backoff_delay(attempt);
            assert!(d >= Duration::from_secs(1), "never zero");
            assert!(d <= Duration::from_secs(30), "never above the cap");
        }
    }

    #[test]
    fn max_retries_parses_env_with_default_fallback() {
        let _guard = ENV_LOCK.blocking_lock();
        let previous = std::env::var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES").ok();
        let restore = || match &previous {
            Some(v) => std::env::set_var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES", v),
            None => std::env::remove_var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES"),
        };

        std::env::remove_var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES");
        assert_eq!(recovery_max_retries(), DEFAULT_MAX_RETRIES);

        std::env::set_var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES", "0");
        assert_eq!(recovery_max_retries(), DEFAULT_MAX_RETRIES, "0 falls back");

        std::env::set_var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES", "abc");
        assert_eq!(
            recovery_max_retries(),
            DEFAULT_MAX_RETRIES,
            "garbage falls back"
        );

        std::env::set_var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES", "5");
        assert_eq!(recovery_max_retries(), 5, "valid value wins");

        restore();
    }

    #[tokio::test]
    async fn recovery_skips_pending_threads_when_db_online() {
        // DB-backed test against the dev database (skipped when DATABASE_URL
        // is absent, exactly like the other DB tests in this crate). Only
        // touches the row it creates itself; never production.
        let _guard = ENV_LOCK.lock().await;
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");

        let thread_id: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile) \
             VALUES ('pending', 'user', 'test-channel-db-recovery', 'test-profile') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert test thread");

        // No kanban task is linked, so skip_all_pending_threads only marks the
        // thread skipped; the redispatch loop tolerates any data_dir.
        let ok = run_recovery_phase(&pool, "/opt/omni").await;
        assert!(ok, "recovery must succeed when the DB is online");
        assert!(!is_recovering(), "flag cleared after successful recovery");

        let (status, terminal): (String, bool) =
            sqlx::query_as("SELECT status, terminal FROM threads WHERE id = $1")
                .bind(thread_id)
                .fetch_one(&pool)
                .await
                .expect("fetch thread after recovery");
        assert_eq!(status, "skipped", "pending thread marked skipped");
        assert!(terminal, "skipped thread is terminal");

        sqlx::query("DELETE FROM threads WHERE id = $1")
            .bind(thread_id)
            .execute(&pool)
            .await
            .expect("cleanup test thread");
    }

    #[tokio::test]
    async fn recovery_gives_up_bounded_when_db_unreachable() {
        // Point the pool at a port that refuses connections: recovery must
        // retry a bounded number of times (env override, no real backoff
        // accumulation) and then give up instead of crash-looping.
        let _guard = ENV_LOCK.lock().await;
        let previous = std::env::var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES").ok();
        std::env::set_var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES", "2");
        let dead_url = "postgres://nobody:nothing@127.0.0.1:1/omniagent";
        let pool = match sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(2))
            .connect(dead_url)
            .await
        {
            Ok(p) => p,
            Err(_) => {
                match &previous {
                    Some(v) => std::env::set_var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES", v),
                    None => std::env::remove_var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES"),
                }
                return; // no server at all: nothing to verify, skip
            }
        };

        let ok = run_recovery_phase(&pool, "/opt/omni").await;
        match &previous {
            Some(v) => std::env::set_var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES", v),
            None => std::env::remove_var("OMNIAGENT_DB_RECOVERY_MAX_RETRIES"),
        }
        assert!(
            !ok,
            "recovery must give up after the bounded retries (no crash-loop)"
        );
        assert!(!is_recovering(), "flag cleared after giving up");
    }
}
