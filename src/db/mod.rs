pub mod channels;
pub mod kanban;
pub mod memory;
pub mod messages;
pub mod migrations;
pub mod schedule;
pub mod schema;
pub mod stats;
pub mod summaries;
pub mod threads;
pub mod types;

use crate::error::AppResult;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

pub async fn connect(database_url: &str) -> AppResult<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// App-wide advisory lock key. Keys are scoped PER-DATABASE in Postgres, so a
/// single constant is safe: omnistable and omnidev (separate postgres, separate
/// omniagent DB) never contend, while a second container/process pointed at the
/// SAME database is exactly the case that must be rejected.
pub const ADVISORY_LOCK_KEY: i64 = 72700123;

/// Acquire the single-instance advisory lock on a DEDICATED connection.
///
/// Returns `Ok(true)` if the lock was acquired (this process is now the sole
/// owner of the database), `Ok(false)` if another live instance already holds
/// it, and `Err` on a connection-level failure (in which case the caller should
/// fail loud rather than proceed without the guard).
///
/// The lock is session-scoped: it is released automatically when the returned
/// connection closes (crash, container stop, graceful exit), so a restart never
/// leaves a stale lock behind.
pub async fn try_acquire_advisory_lock(
    database_url: &str,
) -> AppResult<(bool, sqlx::PgConnection)> {
    // A dedicated connection (NOT the pool) so the session-level lock is held
    // for the process lifetime. Pool checkouts are recycled and would drop the
    // session lock the moment the checkout is returned.
    use sqlx::Connection;
    let mut conn = sqlx::PgConnection::connect(database_url).await?;
    // pg_try_advisory_lock(bigint) returns true iff acquired.
    let row: (bool,) = sqlx::query_as("SELECT pg_try_advisory_lock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .fetch_one(&mut conn)
        .await?;
    Ok((row.0, conn))
}
