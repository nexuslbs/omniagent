pub mod channels;
pub mod kanban;
pub mod memory;
pub mod messages;
pub mod migrations;
pub mod schedule;
pub mod schema;
pub mod stats;
pub mod subscriptions;
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
