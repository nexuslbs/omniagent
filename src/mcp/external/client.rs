//! MCP client implementations for stdio and HTTP transports.
//!
//! Each external MCP server is represented by an `McpServerClient` that
//! manages the connection lifecycle: initialize → tools/list → tools/call → shutdown.
//!
//! The `StdioMcpClient` spawns a subprocess and communicates via stdin/stdout.
//! The `HttpMcpClient` connects to an HTTP server endpoint.

use crate::mcp::external::config::McpServerConfig;
use crate::mcp::external::protocol::*;
use crate::mcp::{McpTool, McpToolResult};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Circuit breaker state
// ---------------------------------------------------------------------------

/// Circuit breaker states for external MCP servers.
#[derive(Debug, Clone, PartialEq)]
pub enum CircuitState {
    /// Normal operation — requests are allowed.
    Closed,
    /// Too many failures — requests are blocked.
    Open,
    /// Healing period — one test request is allowed.
    #[expect(dead_code)]
    HalfOpen,
}

/// Per-server circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    state: Arc<Mutex<CircuitStateInner>>,
}

#[derive(Debug)]
struct CircuitStateInner {
    state: CircuitState,
    consecutive_failures: u32,
    max_retries: u32,
}

impl CircuitBreaker {
    pub fn new(max_retries: u32) -> Self {
        Self {
            state: Arc::new(Mutex::new(CircuitStateInner {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                max_retries,
            })),
        }
    }

    /// Check if a request is allowed. Returns true if the circuit is closed
    /// or half-open (allowing a test request).
    pub fn is_allowed(&self) -> bool {
        let inner = self.state.lock().unwrap();
        matches!(inner.state, CircuitState::Closed | CircuitState::HalfOpen)
    }

    /// Record a successful request — resets failure count.
    pub fn record_success(&self) {
        let mut inner = self.state.lock().unwrap();
        inner.consecutive_failures = 0;
        inner.state = CircuitState::Closed;
    }

    /// Record a failed request. Opens the circuit if max retries exceeded.
    pub fn record_failure(&self) {
        let mut inner = self.state.lock().unwrap();
        inner.consecutive_failures += 1;
        if inner.consecutive_failures >= inner.max_retries {
            inner.state = CircuitState::Open;
            tracing::warn!(
                "Circuit breaker opened after {} consecutive failures",
                inner.consecutive_failures
            );
        }
    }

    /// Get the current state (for diagnostics).
    #[allow(dead_code)]
    pub fn state(&self) -> CircuitState {
        self.state.lock().unwrap().state.clone()
    }
}

// ---------------------------------------------------------------------------
// Server health status
// ---------------------------------------------------------------------------

/// Health status of an external MCP server.
#[derive(Debug, Clone)]
#[expect(dead_code)]
pub struct ServerHealth {
    pub connected: bool,
    pub tool_count: usize,
    pub circuit_state: CircuitState,
    pub last_error: Option<String>,
}

// ---------------------------------------------------------------------------
// MCP Server Client trait
// ---------------------------------------------------------------------------

/// A client for an external MCP server.
pub trait McpServerClient: Send + Sync {
    /// Initialize the connection and discover available tools.
    fn initialize(&mut self) -> Result<Vec<McpExternalTool>>;
    /// Call a tool on the server.
    fn call_tool(&self, name: &str, arguments: &Value) -> Result<McpToolResult>;
    /// Shutdown the connection.
    fn shutdown(&mut self) -> Result<()>;
    /// Get the server's display name.
    fn name(&self) -> &str;
    /// Check server health.
    #[expect(dead_code)]
    fn health(&self) -> ServerHealth;
    /// Convert external tools to McpTool instances with a circuit-breaking wrapper.
    fn to_mcp_tools(&mut self) -> Vec<McpTool> {
        let tools = match self.initialize() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("Failed to initialize external MCP server '{}': {:?}", self.name(), e);
                return vec![];
            }
        };

        let server_name = self.name().to_string();
        let circuit = Arc::new(CircuitBreaker::new(3));
        let mut result = Vec::with_capacity(tools.len());

        for t in tools {
            let name = format!("{}_{}", server_name, t.name);
            let schema = convert_input_schema(&t.input_schema);
            let description = format!("[external:{}] {}", server_name, t.description);
            let circuit = circuit.clone();
            let sn = server_name.clone();
            let tn = t.name.clone();

            result.push(McpTool {
                name,
                description,
                input_schema: schema,
                handler: Arc::new(move |args: Value, _ctx: crate::mcp::AppContext| {
                    if !circuit.is_allowed() {
                        return Ok(McpToolResult {
                            call_id: String::new(),
                            content: format!(
                                "Circuit breaker is OPEN for external MCP server '{}'. \
                                 Tool calls are temporarily blocked due to repeated failures. \
                                 Try again later or check server status.",
                                sn
                            ),
                            is_error: true,
                        });
                    }

                    let inner_result = call_tool_direct(&sn, &tn, &args);
                    match inner_result {
                        Ok(res) => {
                            circuit.record_success();
                            Ok(res)
                        }
                        Err(e) => {
                            circuit.record_failure();
                            Ok(McpToolResult {
                                call_id: String::new(),
                                content: format!(
                                    "External MCP server '{}' tool '{}' failed: {}",
                                    sn, tn, e
                                ),
                                is_error: true,
                            })
                        }
                    }
                }),
            });
        }

        result
    }
}

/// Direct tool call helper that dispatches to the right transport.
fn call_tool_direct(server_name: &str, tool_name: &str, args: &Value) -> Result<McpToolResult> {
    // This is a bridge function. In a full implementation, we'd look up
    // the client instance by server_name. For now, we use a global registry.
    let mut clients = CLIENT_REGISTRY.lock().map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
    let client = clients.get_mut(server_name)
        .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not found in registry", server_name))?;
    client.call_tool(tool_name, args)
}

// Global registry of active MCP server clients.
use once_cell::sync::Lazy;
use std::sync::Mutex as StdMutex;
static CLIENT_REGISTRY: Lazy<StdMutex<HashMap<String, Box<dyn McpServerClient>>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));

/// Register an MCP client in the global registry.
pub fn register_client(name: &str, client: Box<dyn McpServerClient>) {
    if let Ok(mut registry) = CLIENT_REGISTRY.lock() {
        registry.insert(name.to_string(), client);
    }
}

/// Convert MCP inputSchema to the JSON Schema format the LLM expects.
fn convert_input_schema(schema: &Value) -> Value {
    // MCP inputSchema is already JSON Schema-compatible.
    // We just ensure the required fields exist.
    if schema.is_object() {
        let mut s = schema.clone();
        if s.get("type").is_none() {
            s["type"] = Value::String("object".to_string());
        }
        s
    } else {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }
}

// ---------------------------------------------------------------------------
// Stdio MCP Client
// ---------------------------------------------------------------------------

/// An MCP client that communicates with a subprocess via stdin/stdout.
pub struct StdioMcpClient {
    config: McpServerConfig,
    /// Interior mutability for subprocess handles.
    process: StdMutex<Option<Child>>,
    next_id: AtomicU64,
    tools: StdMutex<Vec<McpExternalTool>>,
    #[allow(dead_code)]
    circuit: CircuitBreaker,
    connected: StdMutex<bool>,
    #[allow(dead_code)]
    last_error: StdMutex<Option<String>>,
}

impl StdioMcpClient {
    pub fn new(config: McpServerConfig) -> Self {
        Self {
            circuit: CircuitBreaker::new(config.max_retries),
            config,
            process: StdMutex::new(None),
            next_id: AtomicU64::new(1),
            tools: StdMutex::new(Vec::new()),
            connected: StdMutex::new(false),
            last_error: StdMutex::new(None),
        }
    }

    /// Spawn the subprocess (under lock).
    fn spawn_locked(&self) -> Result<std::sync::MutexGuard<'_, Option<Child>>> {
        let mut guard = self.process.lock().map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
        if guard.is_some() {
            return Ok(guard);
        }

        let cmd = self.config.command.as_ref()
            .ok_or_else(|| anyhow::anyhow!("stdio MCP server '{}' has no command configured", self.config.name))?;

        tracing::info!(
            "Spawning external MCP server '{}': {} {}",
            self.config.name,
            cmd,
            self.config.args.join(" ")
        );

        let mut command = Command::new(cmd);
        command
            .args(&self.config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        for (key, value) in &self.config.env {
            let resolved = crate::mcp::external::config::resolve_env_vars(value);
            command.env(key, resolved);
        }

        let child = command
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server '{}'", self.config.name))?;

        *self.connected.lock().unwrap() = true;
        *guard = Some(child);
        Ok(guard)
    }

    /// Send a request and read response (runs under process lock).
    fn send_request_locked(
        child: &mut Child,
        request: &str,
        server_name: &str,
    ) -> Result<String> {
        let stdin = child.stdin.as_mut()
            .ok_or_else(|| anyhow::anyhow!("Failed to open stdin for MCP server"))?;
        stdin.write_all(request.as_bytes())
            .with_context(|| format!("Failed to write to MCP server '{}' stdin", server_name))?;
        stdin.write_all(b"\n")
            .context("Failed to write newline to MCP server stdin")?;
        stdin.flush()?;

        let stdout = child.stdout.as_mut()
            .ok_or_else(|| anyhow::anyhow!("Failed to open stdout for MCP server"))?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line)
            .with_context(|| format!("Failed to read response from MCP server '{}'", server_name))?;

        Ok(line.trim().to_string())
    }
}

impl McpServerClient for StdioMcpClient {
    fn initialize(&mut self) -> Result<Vec<McpExternalTool>> {
        {
            let tools = self.tools.lock().unwrap();
            if !tools.is_empty() {
                return Ok(tools.clone());
            }
        }

        // Step 1: Initialize
        let mut guard = self.spawn_locked()?;
        let child = guard.as_mut().unwrap();
        let server_name = &self.config.name;

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_initialize_request(id);
        let response = Self::send_request_locked(child, &req, server_name)?;
        let init_result = match parse_response(&response)? {
            JsonRpcResponse::Success { result, .. } => result,
            JsonRpcResponse::Error { error, .. } => {
                return Err(anyhow::anyhow!("MCP initialize error ({}): {}", error.code, error.message));
            }
        };

        if let Some(server_info) = init_result.get("serverInfo") {
            tracing::info!(
                "MCP server '{}' connected: {} v{}",
                server_name,
                server_info.get("name").and_then(|v| v.as_str()).unwrap_or("unknown"),
                server_info.get("version").and_then(|v| v.as_str()).unwrap_or("0"),
            );
        }

        // Step 2: Send initialized notification (no response expected)
        let notif = build_initialized_notification();
        let stdin = child.stdin.as_mut()
            .ok_or_else(|| anyhow::anyhow!("Failed to open stdin for MCP server"))?;
        stdin.write_all(notif.as_bytes())
            .with_context(|| format!("Failed to write notification to MCP server '{}' stdin", server_name))?;
        stdin.write_all(b"\n")
            .context("Failed to write newline to MCP server stdin")?;
        stdin.flush()?;

        // Step 3: List tools
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_list_tools_request(id);
        let response = Self::send_request_locked(child, &req, server_name)?;
        let list_result = match parse_response(&response)? {
            JsonRpcResponse::Success { result, .. } => result,
            JsonRpcResponse::Error { error, .. } => {
                return Err(anyhow::anyhow!("MCP tools/list error ({}): {}", error.code, error.message));
            }
        };

        let tools: ListToolsResult = serde_json::from_value(list_result)
            .context("Failed to parse tools/list result")?;

        tracing::info!(
            "MCP server '{}' exposes {} tool(s)",
            server_name,
            tools.tools.len()
        );

        *self.tools.lock().unwrap() = tools.tools.clone();
        Ok(tools.tools)
    }

    fn call_tool(&self, name: &str, arguments: &Value) -> Result<McpToolResult> {
        let mut guard = self.process.lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
        let child = guard.as_mut()
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not initialized", self.config.name))?;
        let server_name = &self.config.name;

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_call_tool_request(id, name, arguments);
        let response = Self::send_request_locked(child, &req, server_name)?;

        let result_value = match parse_response(&response)? {
            JsonRpcResponse::Success { result, .. } => result,
            JsonRpcResponse::Error { error, .. } => {
                return Err(anyhow::anyhow!("MCP tool call error ({}): {}", error.code, error.message));
            }
        };

        let result: CallToolResult = serde_json::from_value(result_value)
            .context("Failed to parse tools/call result")?;

        let text = extract_tool_result_text(&result);
        Ok(McpToolResult {
            call_id: String::new(),
            content: text,
            is_error: result.is_error,
        })
    }

    fn shutdown(&mut self) -> Result<()> {
        if let Ok(mut guard) = self.process.lock() {
            if let Some(mut child) = guard.take() {
                child.kill().ok();
                child.wait().ok();
            }
        }
        *self.connected.lock().unwrap() = false;
        Ok(())
    }

    fn name(&self) -> &str {
        &self.config.name
    }

    fn health(&self) -> ServerHealth {
        ServerHealth {
            connected: *self.connected.lock().unwrap(),
            tool_count: self.tools.lock().unwrap().len(),
            circuit_state: self.circuit.state(),
            last_error: self.last_error.lock().unwrap().clone(),
        }
    }
}

impl Drop for StdioMcpClient {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// HTTP MCP Client
// ---------------------------------------------------------------------------

/// An MCP client that connects to an HTTP server.
/// Uses a simple request-response pattern via POST.
pub struct HttpMcpClient {
    config: McpServerConfig,
    client: reqwest::blocking::Client,
    next_id: AtomicU64,
    tools: Vec<McpExternalTool>,
    #[allow(dead_code)]
    circuit: CircuitBreaker,
    connected: bool,
    #[allow(dead_code)]
    last_error: Option<String>,
}

impl HttpMcpClient {
    pub fn new(config: McpServerConfig) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .unwrap_or_default();

        Self {
            circuit: CircuitBreaker::new(config.max_retries),
            config,
            client,
            next_id: AtomicU64::new(1),
            tools: Vec::new(),
            connected: false,
            last_error: None,
        }
    }

    fn base_url(&self) -> &str {
        self.config.url.as_deref().unwrap_or("http://localhost:3000/mcp")
    }

    fn post(&self, body: &str) -> Result<String> {
        let url = self.base_url();
        let response = self.client
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .with_context(|| format!("HTTP request to MCP server '{}' at {} failed", self.config.name, url))?;

        let text = response.text()
            .with_context(|| format!("Failed to read HTTP response from MCP server '{}'", self.config.name))?;

        Ok(text)
    }
}

impl McpServerClient for HttpMcpClient {
    fn initialize(&mut self) -> Result<Vec<McpExternalTool>> {
        if !self.tools.is_empty() {
            return Ok(self.tools.clone());
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_initialize_request(id);
        let response = self.post(&req)?;

        let result_value = match parse_response(response.trim())? {
            JsonRpcResponse::Success { result, .. } => result,
            JsonRpcResponse::Error { error, .. } => {
                return Err(anyhow::anyhow!("MCP initialize error ({}): {}", error.code, error.message));
            }
        };

        if let Some(server_info) = result_value.get("serverInfo") {
            tracing::info!(
                "HTTP MCP server '{}' connected: {} v{}",
                self.config.name,
                server_info.get("name").and_then(|v| v.as_str()).unwrap_or("unknown"),
                server_info.get("version").and_then(|v| v.as_str()).unwrap_or("0"),
            );
        }

        // List tools
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_list_tools_request(id);
        let response = self.post(&req)?;

        let list_value = match parse_response(response.trim())? {
            JsonRpcResponse::Success { result, .. } => result,
            JsonRpcResponse::Error { error, .. } => {
                return Err(anyhow::anyhow!("MCP tools/list error ({}): {}", error.code, error.message));
            }
        };

        let tools: ListToolsResult = serde_json::from_value(list_value)
            .context("Failed to parse tools/list result")?;

        tracing::info!(
            "HTTP MCP server '{}' exposes {} tool(s)",
            self.config.name,
            tools.tools.len()
        );

        self.connected = true;
        self.tools = tools.tools.clone();
        Ok(tools.tools)
    }

    fn call_tool(&self, name: &str, arguments: &Value) -> Result<McpToolResult> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_call_tool_request(id, name, arguments);
        let response = self.post(&req)?;

        let result_value = match parse_response(response.trim())? {
            JsonRpcResponse::Success { result, .. } => result,
            JsonRpcResponse::Error { error, .. } => {
                return Err(anyhow::anyhow!("MCP tool call error ({}): {}", error.code, error.message));
            }
        };

        let result: CallToolResult = serde_json::from_value(result_value)
            .context("Failed to parse tools/call result")?;

        let text = extract_tool_result_text(&result);
        Ok(McpToolResult {
            call_id: String::new(),
            content: text,
            is_error: result.is_error,
        })
    }

    fn shutdown(&mut self) -> Result<()> {
        self.connected = false;
        Ok(())
    }

    fn name(&self) -> &str {
        &self.config.name
    }

    fn health(&self) -> ServerHealth {
        ServerHealth {
            connected: self.connected,
            tool_count: self.tools.len(),
            circuit_state: self.circuit.state(),
            last_error: self.last_error.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Factory: create the right client type from config
// ---------------------------------------------------------------------------

/// Create an MCP client from a server configuration.
pub fn create_client(config: McpServerConfig) -> Box<dyn McpServerClient> {
    match config.transport {
        crate::mcp::external::config::McpTransport::Stdio => {
            Box::new(StdioMcpClient::new(config))
        }
        crate::mcp::external::config::McpTransport::Http => {
            Box::new(HttpMcpClient::new(config))
        }
    }
}

/// Initialize all external MCP servers and register their tools.
/// Returns a list of McpTool instances merged from all servers.
pub fn initialize_external_tools(data_dir: &str) -> Vec<McpTool> {
    let configs = crate::mcp::external::config::load_servers_config(data_dir);
    let mut all_tools = Vec::new();

    for cfg in configs {
        let server_name = cfg.name.clone();
        let mut client = create_client(cfg);

        match client.initialize() {
            Ok(tools) => {
                tracing::info!(
                    "External MCP server '{}' initialized with {} tool(s)",
                    server_name,
                    tools.len()
                );

                // Convert external tools to McpTool format
                let mcp_tools = client.to_mcp_tools();
                let count = mcp_tools.len();
                all_tools.extend(mcp_tools);

                // Register in global client registry for call dispatch
                register_client(&server_name, client);

                tracing::info!("Registered {} external tool(s) from '{}'", count, server_name);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to initialize external MCP server '{}': {:?}",
                    server_name,
                    e
                );
            }
        }
    }

    all_tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circuit_breaker_initial_state() {
        let cb = CircuitBreaker::new(3);
        assert!(cb.is_allowed());
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_circuit_breaker_opens_after_failures() {
        let cb = CircuitBreaker::new(3);
        cb.record_failure();
        assert!(cb.is_allowed());
        cb.record_failure();
        assert!(cb.is_allowed());
        cb.record_failure();
        assert!(!cb.is_allowed());
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[test]
    fn test_circuit_breaker_resets_on_success() {
        let cb = CircuitBreaker::new(3);
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        assert!(cb.is_allowed());
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[test]
    fn test_convert_input_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            }
        });
        let converted = convert_input_schema(&schema);
        assert_eq!(converted["type"], "object");
    }

    #[test]
    fn test_convert_input_schema_missing_type() {
        let schema = serde_json::json!({
            "properties": {"x": {"type": "number"}}
        });
        let converted = convert_input_schema(&schema);
        assert_eq!(converted["type"], "object");
    }
}
