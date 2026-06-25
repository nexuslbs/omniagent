//! OmniAgent — library crate shared by the main binary and external MCP servers.
// Items not used within the lib may be used by the main binary or MCP server binaries.
#![allow(dead_code)]
pub mod actions;
pub mod agent;
pub mod commands;
pub mod complexity;
pub mod context_builder;
pub mod db;
pub mod llm;
pub mod mcp;
pub mod platform;
pub mod plugin;
pub mod plugins_yaml;
pub mod profile;
pub mod prompt_builder;
pub mod relevance;
pub mod hindsight_populator;
pub mod scheduler;
pub mod server;
pub mod subtask;
pub mod vectorizer;
