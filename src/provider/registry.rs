//! Provider registry — manages external provider plugin subprocesses.
//!
//! Provider plugins that have an `entrypoint` in their plugin.json are started
//! as child processes and communicate via JSON-lines over stdio.
//! HTTP-based providers are handled directly by `LLMClient`.

use crate::error::AppResult;
use crate::provider::external::client::ExternalProviderClient;
use crate::provider::external::CompleteParams;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Global registry of external provider subprocesses.
pub static PROVIDER_REGISTRY: Lazy<RwLock<ProviderRegistry>> =
    Lazy::new(|| RwLock::new(ProviderRegistry::new()));

/// Manages external provider plugin subprocesses.
pub struct ProviderRegistry {
    clients: HashMap<String, Arc<ExternalProviderClient>>,
}

impl ProviderRegistry {
    fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// Register and start an external provider subprocess.
    pub fn register(&mut self, name: &str, command: &str, args: &[String]) {
        let client = Arc::new(ExternalProviderClient::new(name, command, args));
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

/// Initiate a completion via an external provider subprocess, if one exists
/// for the given provider name.
pub async fn try_external_completion(
    provider_name: &str,
    model: &str,
    messages: Vec<serde_json::Value>,
    max_tokens: u32,
    temperature: f32,
) -> Option<AppResult<String>> {
    let client = {
        let registry = PROVIDER_REGISTRY.read().ok()?;
        registry.get_cloned(provider_name)?
    };
    // Drop registry lock — client is an Arc clone, no borrow on the registry

    let params = CompleteParams {
        model: model.to_string(),
        messages,
        max_tokens,
        temperature,
        stream: false,
        tools: None,
    };

    let result = client.complete(&params).await;
    Some(result.map(|r| r.content))
}
