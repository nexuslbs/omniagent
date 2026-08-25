//! Plugin Manager — abstraction layer over plugin lifecycle and MCP tool registry.
//!
//! Defines a trait that decouples Axum handlers, the executor, and the scheduler
//! from the concrete MCP registry / client registry implementation.
//!
//! Phase 1: `LegacyPluginManager` wraps the existing global statics.
//! Phase 2: `ActorPluginManager` replaces the McpRegistry RwLock with a tokio actor.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::mcp::{McpRegistry, McpTool};

/// Plugin manager trait — single authority for all plugin lifecycle operations.
///
/// Every call site in the server handlers, agent executor, and scheduler goes
/// through this trait instead of directly touching global statics or RwLocks.
#[async_trait]
pub trait PluginManager: Send + Sync + 'static {
    /// Snapshot the full tool registry (for the executor/scheduler).
    /// Returns a cloned McpRegistry — zero contention on subsequent operations.
    async fn snapshot_registry(&self) -> McpRegistry;

    /// Register tools into the registry (after MCP server init).
    async fn register_tools(&self, tools: Vec<McpTool>);

    /// Remove all tools belonging to a given server.
    /// Returns the names of removed tools.
    async fn remove_server_tools(&self, server_name: &str) -> Vec<String>;

    /// Get all tool names (for building prompt context).
    async fn all_tool_names(&self) -> Vec<String>;

    /// Remove an MCP client from the registry (e.g. on disable).
    fn remove_client(&self, name: &str);

    /// Initialize a single external MCP server by name and return its tools.
    /// Registers the client in the external clients registry.
    async fn initialize_single_server(
        &self,
        data_dir: &str,
        server_name: &str,
    ) -> Result<Vec<McpTool>, String>;
}

// ═════════════════════════════════════════════════════════════════════════════
// Phase 1: Legacy wrapper around existing global statics
// ═════════════════════════════════════════════════════════════════════════════

/// Wraps the current global statics and the `Arc<RwLock<McpRegistry>>` behind the trait interface.
///
/// No behavior changes — every method delegates to the same statics
/// that the direct call sites used. This is a pure abstraction extraction.
#[derive(Clone)]
pub struct LegacyPluginManager {
    registry: Arc<tokio::sync::RwLock<McpRegistry>>,
    clients: Arc<crate::mcp::external::client::ExternalMcpClients>,
    // DB pool for resolving $secret:NAME refs in MCP plugin configs.
    // Option so unit tests can construct the manager without a DB.
    pool: Option<sqlx::PgPool>,
}

impl LegacyPluginManager {
    pub fn new(
        registry: Arc<tokio::sync::RwLock<McpRegistry>>,
        clients: Arc<crate::mcp::external::client::ExternalMcpClients>,
        pool: Option<sqlx::PgPool>,
    ) -> Self {
        Self {
            registry,
            clients,
            pool,
        }
    }

    /// Get the inner registry (for call sites that need direct lock access).
    pub fn inner_registry(&self) -> &Arc<tokio::sync::RwLock<McpRegistry>> {
        &self.registry
    }
}

#[async_trait]
impl PluginManager for LegacyPluginManager {
    async fn snapshot_registry(&self) -> McpRegistry {
        self.registry.read().await.clone()
    }

    async fn register_tools(&self, tools: Vec<McpTool>) {
        self.registry.write().await.register_all(tools);
    }

    async fn remove_server_tools(&self, server_name: &str) -> Vec<String> {
        self.registry.write().await.remove_by_server(server_name)
    }

    async fn all_tool_names(&self) -> Vec<String> {
        self.registry
            .read()
            .await
            .all()
            .iter()
            .map(|t| t.full_name.clone())
            .collect()
    }

    fn remove_client(&self, name: &str) {
        self.clients.remove(name);
    }

    async fn initialize_single_server(
        &self,
        data_dir: &str,
        server_name: &str,
    ) -> Result<Vec<McpTool>, String> {
        crate::mcp::external::client::initialize_single_server_tools(
            data_dir,
            self.pool.as_ref(),
            server_name,
            &self.clients,
        )
        .await
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// Phase 2: Actor-based plugin manager
// ═════════════════════════════════════════════════════════════════════════════

/// Commands that the actor processes one at a time.
enum PluginCommand {
    RegisterTools {
        tools: Vec<McpTool>,
        resp: oneshot::Sender<()>,
    },
    RemoveServerTools {
        server_name: String,
        resp: oneshot::Sender<Vec<String>>,
    },
    SnapshotRegistry {
        resp: oneshot::Sender<McpRegistry>,
    },
    AllToolNames {
        resp: oneshot::Sender<Vec<String>>,
    },
    InitializeSingleServer {
        data_dir: String,
        server_name: String,
        clients: std::sync::Arc<crate::mcp::external::client::ExternalMcpClients>,
        // DB pool for resolving $secret:NAME refs in MCP plugin configs.
        pool: Option<sqlx::PgPool>,
        resp: oneshot::Sender<Result<Vec<McpTool>, String>>,
    },
}

/// Actor-based plugin manager.
///
/// Owns the `McpRegistry` directly (no RwLock) and processes all registry
/// mutations and snapshots through an `mpsc` channel. Zero lock contention
/// between readers (snapshots) and writers (register/remove).
///
/// Client management (`remove_client`, etc.) delegates to ExternalMcpClients.
#[derive(Clone)]
pub struct ActorPluginManager {
    tx: mpsc::UnboundedSender<PluginCommand>,
    clients: Arc<crate::mcp::external::client::ExternalMcpClients>,
    // DB pool for resolving $secret:NAME refs in MCP plugin configs.
    // Option so unit tests can construct the manager without a DB.
    pool: Option<sqlx::PgPool>,
}

impl ActorPluginManager {
    /// Create a new actor and spawn its task.
    ///
    /// The actor owns an `McpRegistry` initialized with `initial_registry`.
    /// The returned handle can be cloned (each clone shares the same sender).
    pub fn new(
        initial_registry: McpRegistry,
        clients: Arc<crate::mcp::external::client::ExternalMcpClients>,
        pool: Option<sqlx::PgPool>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<PluginCommand>();
        tokio::spawn(actor_loop(initial_registry, rx));
        Self { tx, clients, pool }
    }
}

/// The actor's event loop — runs on a dedicated tokio task.
async fn actor_loop(mut registry: McpRegistry, mut rx: mpsc::UnboundedReceiver<PluginCommand>) {
    tracing::info!("[plugin-manager] Actor started");
    while let Some(cmd) = rx.recv().await {
        match cmd {
            PluginCommand::RegisterTools { tools, resp } => {
                registry.register_all(tools);
                let _ = resp.send(());
            }
            PluginCommand::RemoveServerTools { server_name, resp } => {
                let removed = registry.remove_by_server(&server_name);
                let _ = resp.send(removed);
            }
            PluginCommand::SnapshotRegistry { resp } => {
                let snapshot = registry.clone();
                let _ = resp.send(snapshot);
            }
            PluginCommand::AllToolNames { resp } => {
                let names = registry.all().iter().map(|t| t.full_name.clone()).collect();
                let _ = resp.send(names);
            }
            PluginCommand::InitializeSingleServer {
                data_dir,
                server_name,
                clients,
                pool,
                resp,
            } => {
                // Spawn a subtask so the actor isn't blocked on MCP I/O
                let result = tokio::spawn(async move {
                    crate::mcp::external::client::initialize_single_server_tools(
                        &data_dir,
                        pool.as_ref(),
                        &server_name,
                        &clients,
                    )
                    .await
                })
                .await;
                match result {
                    Ok(Ok(tools)) => {
                        let _ = resp.send(Ok(tools));
                    }
                    Ok(Err(e)) => {
                        let _ = resp.send(Err(e));
                    }
                    Err(e) => {
                        let _ = resp.send(Err(format!("Actor task panicked: {}", e)));
                    }
                }
            }
        }
    }
    tracing::warn!("[plugin-manager] Actor stopped (channel closed)");
}

#[async_trait]
impl PluginManager for ActorPluginManager {
    async fn snapshot_registry(&self) -> McpRegistry {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(PluginCommand::SnapshotRegistry { resp: tx });
        rx.await.unwrap_or_default()
    }

    async fn register_tools(&self, tools: Vec<McpTool>) {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(PluginCommand::RegisterTools { tools, resp: tx });
        let _ = rx.await;
    }

    async fn remove_server_tools(&self, server_name: &str) -> Vec<String> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(PluginCommand::RemoveServerTools {
            server_name: server_name.to_string(),
            resp: tx,
        });
        rx.await.unwrap_or_default()
    }

    async fn all_tool_names(&self) -> Vec<String> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(PluginCommand::AllToolNames { resp: tx });
        rx.await.unwrap_or_default()
    }

    fn remove_client(&self, name: &str) {
        self.clients.remove(name);
    }

    async fn initialize_single_server(
        &self,
        data_dir: &str,
        server_name: &str,
    ) -> Result<Vec<McpTool>, String> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(PluginCommand::InitializeSingleServer {
            data_dir: data_dir.to_string(),
            server_name: server_name.to_string(),
            clients: self.clients.clone(),
            pool: self.pool.clone(),
            resp: tx,
        });
        rx.await.unwrap_or(Err("Actor channel closed".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::external::client::ExternalMcpClients;
    use crate::mcp::{AppContext, McpToolHandler, McpToolResult};
    use serde_json::{json, Value};

    fn make_test_handler() -> McpToolHandler {
        Arc::new(|_args: Value, _ctx: AppContext| {
            Box::pin(async {
                Ok(McpToolResult {
                    call_id: String::new(),
                    content: "ok".to_string(),
                    is_error: false,
                })
            })
        })
    }

    fn make_test_tool(name: &str) -> McpTool {
        McpTool {
            name: name.to_string(),
            full_name: name.to_string(),
            description: "test".to_string(),
            input_schema: json!({"type": "object"}),
            server_name: None,
            timeout_secs: Some(30),
            handler: make_test_handler(),
        }
    }

    fn make_test_manager() -> ActorPluginManager {
        ActorPluginManager::new(
            McpRegistry::new(),
            Arc::new(ExternalMcpClients::new()),
            None, // no DB pool in unit tests
        )
    }

    #[tokio::test]
    async fn test_snapshot_registry_empty() {
        let mgr = make_test_manager();
        let registry = mgr.snapshot_registry().await;
        assert!(registry.all().is_empty());
    }

    #[tokio::test]
    async fn test_all_tool_names_empty() {
        let mgr = make_test_manager();
        let names = mgr.all_tool_names().await;
        assert!(names.is_empty());
    }

    #[tokio::test]
    async fn test_register_tools_then_snapshot() {
        let mgr = make_test_manager();
        mgr.register_tools(vec![make_test_tool("tool_a"), make_test_tool("tool_b")])
            .await;

        let registry = mgr.snapshot_registry().await;
        let names: Vec<&str> = registry.all().iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"tool_a"));
        assert!(names.contains(&"tool_b"));
        assert_eq!(registry.all().len(), 2);
    }

    #[tokio::test]
    async fn test_all_tool_names_after_register() {
        let mgr = make_test_manager();
        mgr.register_tools(vec![make_test_tool("tool_a"), make_test_tool("tool_b")])
            .await;

        let names = mgr.all_tool_names().await;
        assert!(names.contains(&"tool_a".to_string()));
        assert!(names.contains(&"tool_b".to_string()));
        assert_eq!(names.len(), 2);
    }

    #[tokio::test]
    async fn test_remove_server_tools() {
        let mut tool_a = make_test_tool("tool_a");
        tool_a.server_name = Some("server1".to_string());
        tool_a.full_name = "server1_tool-a".to_string();

        let mut tool_b = make_test_tool("tool_b");
        tool_b.server_name = Some("server1".to_string());
        tool_b.full_name = "server1_tool-b".to_string();

        let mut tool_c = make_test_tool("tool_c");
        tool_c.server_name = Some("server2".to_string());
        tool_c.full_name = "server2_tool-c".to_string();

        let mgr = make_test_manager();
        mgr.register_tools(vec![tool_a, tool_b, tool_c]).await;

        let removed = mgr.remove_server_tools("server1").await;
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&"server1_tool-a".to_string()));
        assert!(removed.contains(&"server1_tool-b".to_string()));

        let names = mgr.all_tool_names().await;
        assert_eq!(names.len(), 1);
        // all_tool_names returns FULL names (always-full-name rule):
        // tool_c's full_name is "server2_tool-c".
        assert_eq!(names[0], "server2_tool-c");
    }

    #[tokio::test]
    async fn test_actor_can_be_cloned() {
        let mgr = make_test_manager();
        let mgr2 = mgr.clone();
        mgr2.register_tools(vec![make_test_tool("cloned_tool")])
            .await;
        let names = mgr.all_tool_names().await;
        assert!(names.contains(&"cloned_tool".to_string()));
    }
}
