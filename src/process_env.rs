//! Process-environment isolation for spawned children.
//!
//! omniagent must NEVER pass its ambient environment (which includes the
//! /opt/omni/.env vars loaded by the server, e.g. COMPOSE_PROJECT_NAME for the
//! production compose project) to spawned plugin/tool child processes. Every
//! child is spawned with an EMPTY environment plus ONLY explicitly passed vars:
//!
//! - the plugin's configured `env:` map (with `$env:` / `$secret:` refs
//!   resolved by the core because the config explicitly declared them), and
//! - an explicit minimal `PATH` so the child can resolve its own grandchildren:
//!   Rust's `Command::new` resolves bare program names via `execvp` using the
//!   PARENT's environ `PATH`, so an env-cleared child would otherwise fail with
//!   ENOENT on its own spawns.
//!
//! No other variable is ever passed implicitly. There is NO whitelist.

/// Minimal explicit `PATH` passed to every spawned child (binary resolution
/// only; not ambient, never inherited from the server's environment).
pub const MINIMAL_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
