//! OmniAgent: library crate shared by the main binary and external MCP servers.
// Items not used within the lib may be used by the main binary or MCP server binaries.
#![expect(
    dead_code,
    reason = "lib items may be consumed by main bin or MCP server binaries"
)]
pub mod agent;
pub mod boards;
pub mod channels_yaml;
pub mod commands;
pub mod config_path;
pub mod db;
pub mod error;
pub mod hooks;
pub mod kanban_action;
pub mod kanban_dispatch;
pub mod llm;
pub mod mcp;
pub mod models_yaml;
pub mod platform;
pub mod plugin;
pub mod plugins_yaml;
pub mod process_env;
pub mod profile;
pub mod profiles_yaml;
pub mod provider;
pub mod resolution;
pub mod scheduler;
pub mod server;
pub mod subtask;
pub mod tasks_yaml;
pub mod vectorizer;
pub mod workflows;
