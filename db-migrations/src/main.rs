//! Standalone database migrator for OmniAgent.
//!
//! Applies all schema migrations to a Postgres database.
//! Usage:
//!   DATABASE_URL=postgres://... cargo run --package db-migrations
//!   cargo run --package db-migrations -- --database-url postgres://...
//!
//! This binary has minimal dependencies and can be compiled independently
//! from the full omniagent binary, making it safe to deploy schema changes
//! that might otherwise be blocked by compile-time sqlx validation.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        eprintln!("ERROR: DATABASE_URL environment variable must be set");
        eprintln!();
        eprintln!("Usage: DATABASE_URL=postgres://user:pass@host/dbname cargo run --package db-migrations");
        std::process::exit(1);
    });

    tracing::info!("Connecting to database...");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&database_url)
        .await?;
    tracing::info!("Connected. Running migrations...");

    db_migrations::run(&pool).await?;

    tracing::info!("All migrations completed successfully.");
    Ok(())
}
