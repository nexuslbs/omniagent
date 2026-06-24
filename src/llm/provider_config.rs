//! Provider configuration resolution from the plugin_registry.
//!
//! Provides the `resolve_provider_config()` function that queries
//! the plugin_registry table for provider plugin details and falls
//! back to environment variable defaults.

use anyhow::Result;
use sqlx::PgPool;

use crate::plugin;

#[expect(dead_code)]
/// Resolved provider configuration from the plugin_registry.
pub struct ProviderConfig {
    pub name: String,
    pub default_base_url: String,
    pub api_mode: String, // "chat_completions" or "anthropic_messages"
    pub default_model: String,
}

/// Query the plugin_registry for provider details.
///
/// Falls back to env-var defaults if the provider is not found in the DB.
#[expect(dead_code)]
pub async fn resolve_provider_config(
    pool: &PgPool,
    provider_name: &str,
) -> Result<Option<ProviderConfig>> {
    match plugin::get_plugin_by_name(pool, provider_name).await {
        Ok(Some(row)) if row.plugin_type == "provider" => {
            let manifest: plugin::PluginManifest =
                serde_json::from_value(row.manifest.clone())?;
            let base_url = manifest.default_base_url.unwrap_or_else(|| {
                crate::llm::resolve_default_base_url(provider_name)
            });
            let api_mode = manifest
                .api_mode
                .clone()
                .unwrap_or_else(|| "chat_completions".to_string());
            Ok(Some(ProviderConfig {
                name: provider_name.to_string(),
                default_base_url: base_url,
                api_mode,
                default_model: String::new(),
            }))
        }
        _ => Ok(None),
    }
}
