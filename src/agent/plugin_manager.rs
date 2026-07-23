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
}

impl LegacyPluginManager {
    pub fn new(
        registry: Arc<tokio::sync::RwLock<McpRegistry>>,
        clients: Arc<crate::mcp::external::client::ExternalMcpClients>,
    ) -> Self {
        Self { registry, clients }
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
            .map(|t| t.name.clone())
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
}

impl ActorPluginManager {
    /// Create a new actor and spawn its task.
    ///
    /// The actor owns an `McpRegistry` initialized with `initial_registry`.
    /// The returned handle can be cloned (each clone shares the same sender).
    pub fn new(
        initial_registry: McpRegistry,
        clients: Arc<crate::mcp::external::client::ExternalMcpClients>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<PluginCommand>();
        tokio::spawn(actor_loop(initial_registry, rx));
        Self { tx, clients }
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
                let names = registry.all().iter().map(|t| t.name.clone()).collect();
                let _ = resp.send(names);
            }
            PluginCommand::InitializeSingleServer {
                data_dir,
                server_name,
                clients,
                resp,
            } => {
                // Spawn a subtask so the actor isn't blocked on MCP I/O
                let result = tokio::spawn(async move {
                    crate::mcp::external::client::initialize_single_server_tools(
                        &data_dir,
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
            resp: tx,
        });
        rx.await.unwrap_or(Err("Actor channel closed".to_string()))
    }
}
