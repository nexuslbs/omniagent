//! Global provider registry for external provider subprocesses.
//!
//! Provides a thread-safe static registry of external provider clients,
//! accessible by provider name from both the reload handler and the LLM client.

use crate::provider::external::client::ExternalProviderClient;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Global provider registry: maps provider name to its subprocess client.
/// Initialized empty — providers are populated on enable via `reload_plugins()`.
pub static PROVIDER_REGISTRY: Lazy<RwLock<ProviderRegistry>> =
    Lazy::new(|| RwLock::new(ProviderRegistry::new()));

/// Thread-safe registry of external provider subprocess clients.
pub struct ProviderRegistry {
    clients: HashMap<String, Arc<ExternalProviderClient>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// Create and register a new provider client.
    /// The client is NOT started yet — call `start()` on the returned Arc.
    pub fn register(&mut self, name: &str, command: &str, args: &[String]) {
        let client = Arc::new(ExternalProviderClient::new(name, command, args));
        self.clients.insert(name.to_string(), client);
    }

    /// Register a pre-built, already-started provider client.
    /// Unlike `register()`, this does NOT create a new client — the caller
    /// is responsible for having called `start()` on the arc before inserting.
    /// This prevents races where another task grabs the Arc before start completes.
    pub fn register_arc(&mut self, name: &str, client: Arc<ExternalProviderClient>) {
        self.clients.insert(name.to_string(), client);
    }

    /// Start all registered providers (called at agent startup).
    pub async fn start_all(&self) {
        for (_name, client) in &self.clients {
            if let Err(e) = client.start().await {
                tracing::error!("Failed to start provider '{}': {:?}", _name, e);
            }
        }
    }

    /// Check if a provider is registered as an external subprocess.
    pub fn has_provider(&self, name: &str) -> bool {
        self.clients.contains_key(name)
    }

    /// Get a cloned Arc to an external provider client (drops registry lock immediately).
    pub fn get_cloned(&self, name: &str) -> Option<Arc<ExternalProviderClient>> {
        self.clients.get(name).cloned()
    }

    /// Remove and stop a provider subprocess.
    pub fn remove(&mut self, name: &str) {
        self.clients.remove(name);
    }
}
