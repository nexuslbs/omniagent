//! OmniAgent: library crate shared by the main binary and external MCP servers.
// Items not used within the lib may be used by the main binary or MCP server binaries.
#![expect(
    dead_code,
    reason = "lib items may be consumed by main bin or MCP server binaries"
)]
pub mod agent;
pub mod commands;
pub mod db;
pub mod error;
pub mod llm;
pub mod mcp;
pub mod platform;
pub mod plugin;
pub mod plugins_yaml;
pub mod profile;
pub mod provider;
pub mod safety;
pub mod scheduler;
pub mod server;
pub mod subtask;