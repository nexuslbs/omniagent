//! External MCP server integration — connect to stdio or HTTP/SSE MCP servers.
//!
//! Implements the Model Context Protocol (MCP) for connecting to external
//! tool servers. Supports:
//! - **stdio transport**: spawn subprocess, JSON-RPC over stdin/stdout
//! - **HTTP transport**: connect to HTTP MCP servers with SSE notifications
//! - **Circuit breaker**: automatic disable after N consecutive failures
//! - **Dynamic registry**: merge external tools with built-in tools
//!
//! External servers are configured via `MCP_SERVERS_CONFIG` env var pointing
//! to a JSON or YAML config file, or programmatically via `load_servers()`.

pub mod client;
pub mod config;
pub mod protocol;
