//! Database migrations: delegated to the standalone `db-migrations` crate.
//!
//! This module is a thin re-export of the `db_migrations::run` function.
//! The actual migration logic lives in `db-migrations/src/lib.rs` so it can
//! be compiled and run independently of the full omniagent binary.
//!
//! To run migrations standalone:
//!   DATABASE_URL=postgres://... cargo run --package db-migrations

pub use db_migrations::run;
