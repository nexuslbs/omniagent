//! Standalone migration runner: applies db-migrations to the live DATABASE_URL.
//! Used by the omnidev dev workflow to migrate an empty postgres volume before
//! compiling with SQLX_OFFLINE=false (sqlx validates queries against the live DB).

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("DATABASE_URL")?;
    let pool = sqlx::PgPool::connect(&url).await?;
    db_migrations::run(&pool).await?;
    println!("[db-migrations] apply OK");
    Ok(())
}
