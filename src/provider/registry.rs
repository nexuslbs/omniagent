//! Global provider registry for external provider subprocesses.
//!
//! Provides a thread-safe static registry of external provider clients,
//! accessible by provider name from both the reload handler and the LLM client.

use crate::provider::external::client::ExternalProviderClient;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Global provider registry: maps provider name to its subprocess client.
/// Initialized empty - providers are populated on enable via `reload_plugins()`.
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
    /// The client is NOT started yet - call `start()` on the returned Arc.
    /// `current_dir` is the plugin install dir: relative entrypoint args
    /// resolve against it (the subprocess CWD), not the omniagent process CWD.
    pub fn register(
        &mut self,
        name: &str,
        command: &str,
        args: &[String],
        current_dir: Option<String>,
    ) {
        let client = Arc::new(ExternalProviderClient::new(
            name,
            command,
            args,
            current_dir,
        ));
        self.clients.insert(name.to_string(), client);
    }

    /// Register a pre-built, already-started provider client.
    /// Unlike `register()`, this does NOT create a new client - the caller
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

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_is_empty() {
        let registry = ProviderRegistry::new();
        assert!(!registry.has_provider("nonexistent"));
        assert!(registry.get_cloned("nonexistent").is_none());
    }

    #[test]
    fn test_register_and_has_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register("test-provider", "echo", &["hello".to_string()], None);
        assert!(registry.has_provider("test-provider"));
    }

    #[test]
    fn test_register_and_get_cloned() {
        let mut registry = ProviderRegistry::new();
        registry.register("test-provider", "echo", &["hello".to_string()], None);
        let client = registry.get_cloned("test-provider");
        assert!(client.is_some());
    }

    #[test]
    fn test_get_cloned_missing_returns_none() {
        let registry = ProviderRegistry::new();
        assert!(registry.get_cloned("nonexistent").is_none());
    }

    #[test]
    fn test_remove_removes_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register("test-provider", "echo", &["hello".to_string()], None);
        assert!(registry.has_provider("test-provider"));
        registry.remove("test-provider");
        assert!(!registry.has_provider("test-provider"));
    }

    #[test]
    fn test_double_register_replaces() {
        let mut registry = ProviderRegistry::new();
        registry.register("p1", "echo", &["a".to_string()], None);
        registry.register("p1", "cat", &["b".to_string()], None);
        assert!(registry.has_provider("p1"));
        assert!(registry.get_cloned("p1").is_some());
    }

    #[test]
    fn test_register_arc() {
        let mut registry = ProviderRegistry::new();
        let client = Arc::new(ExternalProviderClient::new("p1", "echo", &[], None));
        registry.register_arc("p1", client);
        assert!(registry.has_provider("p1"));
        assert!(registry.get_cloned("p1").is_some());
    }

    #[test]
    fn test_has_provider_false_for_missing() {
        let registry = ProviderRegistry::new();
        assert!(!registry.has_provider("missing"));
    }

    #[test]
    fn test_default_is_empty() {
        let registry = ProviderRegistry::default();
        assert!(registry.get_cloned("anything").is_none());
    }
}
