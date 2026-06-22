//! Plugin management system — types, DB queries, manifest loading, and disk sync.
//!
//! This module provides the backend infrastructure for managing plugins:
//! - Plugin manifest types (from plugin.json on disk)
//! - Database CRUD operations (using sql_forge! macro)
//! - Disk syncing (scanning plugin directories and upserting into DB)
//! - Backward-compatible scanning of legacy config files

pub mod installer;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::{FromRow, PgPool};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Plugin manifest as loaded from plugin.json on disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(rename = "type")]
    pub plugin_type: PluginType,
    pub description: Option<String>,
    #[serde(default)]
    pub entrypoint: Option<PluginEntrypoint>,
    pub capabilities: Option<PluginCapabilities>,
    #[serde(default)]
    pub config_schema: Vec<ConfigSchemaField>,
    /// Environment variables for the plugin subprocess.
    /// Supports ${VAR} syntax for runtime resolution from the host env.
    /// Only used for platform plugins (ignored for MCP tools).
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Default base URL for provider plugins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_url: Option<String>,
    /// API mode for provider plugins ("chat_completions", "anthropic_messages", "dynamic").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    Platform,
    Mcp,
    Provider,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntrypoint {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_transport")]
    pub transport: String, // "stdio" or "http"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>, // for HTTP transport
}

/// Capabilities a platform plugin can advertise
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCapabilities {
    #[serde(default)]
    pub inbound: bool,
    #[serde(default)]
    pub outbound: bool,
}

/// Field definition for plugin config forms
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSchemaField {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub secret: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_values: Option<Vec<String>>, // for enum fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>, // "uri", "email", "password"
    /// URL to fetch dynamic enum values from (expects OpenAI `/v1/models` format: `{data: [{id:...}]}`).
    /// When set, `allowed_values` is populated from this endpoint instead of the hardcoded list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Secret,
    Boolean,
    Integer,
    Enum,
    MultiSelect,
}

// ---------------------------------------------------------------------------
// DB Row type
// ---------------------------------------------------------------------------

/// A row from the plugin_registry table.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct PluginRegistryRow {
    pub id: i64,
    pub name: String,
    pub plugin_type: String,
    pub version: String,
    pub source: Option<String>,
    pub status: String,
    pub manifest: serde_json::Value,
    pub config: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Enriched plugin response for the API (includes parsed schema from manifest).
#[derive(Debug, Clone, Serialize)]
pub struct PluginDetail {
    pub id: i64,
    pub name: String,
    pub plugin_type: String,
    pub version: String,
    pub source: Option<String>,
    pub status: String,
    pub manifest: serde_json::Value,
    pub config: serde_json::Value,
    pub config_schema: Vec<ConfigSchemaField>,
    /// Resolved environment variables from the manifest's env block.
    /// ${VAR} references are resolved against the process environment.
    /// Merged with DB config: DB config values take precedence over env.
    #[serde(default)]
    pub resolved_env: HashMap<String, String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Default helpers
// ---------------------------------------------------------------------------

pub fn default_version() -> String {
    "0.1.0".to_string()
}

pub fn default_transport() -> String {
    "stdio".to_string()
}

// ---------------------------------------------------------------------------
// Manifest loading
// ---------------------------------------------------------------------------

/// Read and validate a plugin.json file from disk.
pub fn load_manifest(path: &str) -> Result<PluginManifest> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read plugin manifest: {}", path))?;
    let manifest: PluginManifest = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse plugin manifest: {}", path))?;
    if manifest.name.is_empty() {
        anyhow::bail!("Plugin manifest has empty 'name' field: {}", path);
    }
    Ok(manifest)
}

// ---------------------------------------------------------------------------
// DB query functions (all use sql_forge! macro)
// ---------------------------------------------------------------------------

/// List all plugins in the registry.
pub async fn list_plugins(pool: &PgPool) -> Result<Vec<PluginRegistryRow>> {
    let rows: Vec<PluginRegistryRow> = sql_forge!(
        PluginRegistryRow,
        r#"
        SELECT id, name, plugin_type, version, source, status, manifest, config,
               created_at, updated_at
        FROM plugin_registry
        ORDER BY name ASC
        "#
    )
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to list plugins: {}", e))?;
    Ok(rows)
}

/// Get a single plugin by name.
pub async fn get_plugin_by_name(pool: &PgPool, name: &str) -> Result<Option<PluginRegistryRow>> {
    let row: Option<PluginRegistryRow> = sql_forge!(
        PluginRegistryRow,
        r#"
        SELECT id, name, plugin_type, version, source, status, manifest, config,
               created_at, updated_at
        FROM plugin_registry
        WHERE name = :name
        "#,
        ( :name = name )
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to get plugin '{}': {}", name, e))?;
    Ok(row)
}

/// Upsert a plugin (INSERT ON CONFLICT UPDATE).
pub async fn upsert_plugin(
    pool: &PgPool,
    name: &str,
    plugin_type: &str,
    version: &str,
    source: Option<&str>,
    manifest: &serde_json::Value,
    config: &serde_json::Value,
) -> Result<PluginRegistryRow> {
    let row: PluginRegistryRow = sql_forge!(
        PluginRegistryRow,
        r#"
        INSERT INTO plugin_registry (name, plugin_type, version, source, status, manifest, config)
        VALUES (:name, :plugin_type, :version, :source, 'enabled', :manifest::jsonb, :config::jsonb)
        ON CONFLICT (name) DO UPDATE SET
            plugin_type = EXCLUDED.plugin_type,
            version = EXCLUDED.version,
            source = EXCLUDED.source,
            manifest = EXCLUDED.manifest,
            updated_at = NOW()
        RETURNING id, name, plugin_type, version, source, status, manifest, config,
                  created_at, updated_at
        "#,
        ( :name = name,
          :plugin_type = plugin_type,
          :version = version,
          :source = source.unwrap_or(""),
          :manifest = manifest,
          :config = config )
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to upsert plugin '{}': {}", name, e))?;
    Ok(row)
}

/// Update only the config field of a plugin.
pub async fn update_plugin_config(
    pool: &PgPool,
    name: &str,
    config: &serde_json::Value,
) -> Result<PluginRegistryRow> {
    let row: PluginRegistryRow = sql_forge!(
        PluginRegistryRow,
        r#"
        UPDATE plugin_registry
        SET config = :config::jsonb, updated_at = NOW()
        WHERE name = :name
        RETURNING id, name, plugin_type, version, source, status, manifest, config,
                  created_at, updated_at
        "#,
        ( :config = config, :name = name )
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to update config for plugin '{}': {}", name, e))?;
    Ok(row)
}

/// Update only the status field of a plugin.
pub async fn update_plugin_status(
    pool: &PgPool,
    name: &str,
    status: &str,
) -> Result<PluginRegistryRow> {
    let row: PluginRegistryRow = sql_forge!(
        PluginRegistryRow,
        r#"
        UPDATE plugin_registry
        SET status = :status, updated_at = NOW()
        WHERE name = :name
        RETURNING id, name, plugin_type, version, source, status, manifest, config,
                  created_at, updated_at
        "#,
        ( :status = status, :name = name )
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to update status for plugin '{}': {}", name, e))?;
    Ok(row)
}

/// Delete a plugin from the registry by name.
pub async fn delete_plugin(pool: &PgPool, name: &str) -> Result<bool> {
    let result = sql_forge!(
        "DELETE FROM plugin_registry WHERE name = :name",
        ( :name = name )
    )
    .execute(pool)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to delete plugin '{}': {}", name, e))?;
    Ok(result.rows_affected() > 0)
}

/// Convert a PluginRegistryRow into a PluginDetail (enriched with parsed schema + resolved env).
pub fn enrich_plugin(row: &PluginRegistryRow) -> PluginDetail {
    let mut config_schema = row.manifest["config_schema"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<ConfigSchemaField>(v.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Populate allowed_values from dynamic enum cache for fields with refresh_url
    for field in config_schema.iter_mut() {
        if let Some(ref url) = field.refresh_url {
            let cache = DYNAMIC_ENUM_CACHE.lock().unwrap();
            if let Some(entry) = cache.get(url) {
                if entry.fetched_at.elapsed() < DYNAMIC_ENUM_TTL {
                    field.allowed_values = Some(entry.values.clone());
                }
            }
        }
    }

    // Resolve env vars from manifest's env block, then merge DB config on top
    let manifest_env = row.manifest["env"].as_object().cloned().unwrap_or_default();
    let mut resolved = HashMap::new();
    for (key, val) in &manifest_env {
        let raw = val.as_str().unwrap_or("");
        let resolved_val = resolve_env_var(raw);
        resolved.insert(key.clone(), resolved_val);
    }
    // DB config overrides env vars
    if let Some(db_config) = row.config.as_object() {
        for (key, val) in db_config {
            resolved.insert(
                key.clone(),
                val.as_str().map(|s| s.to_string()).unwrap_or_default(),
            );
        }
    }

    PluginDetail {
        id: row.id,
        name: row.name.clone(),
        plugin_type: row.plugin_type.clone(),
        version: row.version.clone(),
        source: row.source.clone(),
        status: row.status.clone(),
        manifest: row.manifest.clone(),
        config: row.config.clone(),
        config_schema,
        resolved_env: resolved,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// Resolve ${VAR} references in a string against the process environment.
/// Leaves unresolvable references as-is (e.g. ${MISSING_VAR} stays literal).
/// If the variable is not set, the reference is left unchanged and the loop
/// terminates to prevent infinite re-resolution of unresolvable references.
fn resolve_env_var(value: &str) -> String {
    let mut result = value.to_string();
    // Match ${VAR_NAME} patterns and replace with env value if found
    while let Some(start) = result.find("${") {
        if let Some(end) = result[start..].find('}') {
            let var_name = &result[start + 2..start + end];
            match std::env::var(var_name) {
                Ok(val) => {
                    result.replace_range(start..start + end + 1, &val);
                }
                Err(_) => {
                    // Var not set — leave literal ${VAR_NAME} and stop resolving
                    break;
                }
            }
        } else {
            break;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Dynamic enum refresh (refresh_url support)
// ---------------------------------------------------------------------------

struct DynamicEnumEntry {
    values: Vec<String>,
    fetched_at: std::time::Instant,
}

static DYNAMIC_ENUM_CACHE: Lazy<Mutex<HashMap<String, DynamicEnumEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

const DYNAMIC_ENUM_TTL: std::time::Duration = std::time::Duration::from_secs(300); // 5 minutes

/// Fetch model IDs from an OpenAI-compatible `/v1/models` endpoint and return them.
async fn fetch_enum_values(url: &str) -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("Failed to build HTTP client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to fetch {}", url))?;
    let json: serde_json::Value = resp
        .json()
        .await
        .with_context(|| format!("Failed to parse response from {}", url))?;

    let mut values = Vec::new();
    if let Some(data) = json["data"].as_array() {
        for item in data {
            if let Some(id) = item["id"].as_str() {
                values.push(id.to_string());
            }
        }
    }

    if values.is_empty() {
        anyhow::bail!("No model IDs found in response from {} (expected {{data: [{{id: ...}}]}})", url);
    }

    Ok(values)
}

/// Refresh dynamic enum values for a specific plugin by name.
///
/// Fetches from the plugin's `refresh_url` (if any), updates the in-memory cache,
/// and returns the enriched plugin detail with fresh `allowed_values`.
/// Returns `None` if the plugin has no `refresh_url` fields.
pub async fn refresh_plugin_models(pool: &PgPool, name: &str) -> Result<Option<PluginDetail>> {
    let row = get_plugin_by_name(pool, name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Plugin '{}' not found", name))?;

    let mut detail = enrich_plugin(&row);
    let mut had_refresh = false;

    for field in detail.config_schema.iter_mut() {
        let refresh_url = match &field.refresh_url {
            Some(url) if !url.is_empty() => url.clone(),
            _ => continue,
        };
        had_refresh = true;

        match fetch_enum_values(&refresh_url).await {
            Ok(values) => {
                field.allowed_values = Some(values.clone());
                let mut cache = DYNAMIC_ENUM_CACHE.lock().unwrap();
                cache.insert(
                    refresh_url,
                    DynamicEnumEntry {
                        values,
                        fetched_at: std::time::Instant::now(),
                    },
                );
            }
            Err(e) => {
                tracing::warn!("Failed to refresh dynamic enum from {}: {:#}", refresh_url, e);
                // Keep existing allowed_values if fetch fails
            }
        }
    }

    Ok(if had_refresh { Some(detail) } else { None })
}

// ---------------------------------------------------------------------------
// Disk sync
// ---------------------------------------------------------------------------

/// Scan plugin directories and upsert into DB.
///
/// Scans:
/// - `<data_dir>/plugins/installed/<name>/plugin.json` — user-installed
/// - `<workspace_dir>/plugins/<type>/<name>/plugin.json` — bundled/repo plugins
///
/// For backward compatibility, ALSO scans:
/// - `<data_dir>/config/platforms.json` — existing platform config
/// - `<data_dir>/config/mcp-servers.json` — existing MCP config
///
/// Removes DB entries for plugins that no longer exist on disk
/// (unless source='bundled' and the plugin still exists in the repo dir).
pub async fn sync_plugins_from_disk(pool: &PgPool, data_dir: &str) -> Result<()> {
    tracing::info!("Syncing plugins from disk (data_dir: {})", data_dir);

    let workspace_dir =
        std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());

    // Discover all plugins from disk
    let discovered = installer::discover_plugins(data_dir, &workspace_dir);

    // Track names we've seen for cleanup
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (manifest, source_type, _base_path) in &discovered {
        let manifest_json = serde_json::to_value(manifest)
            .unwrap_or_else(|_| serde_json::json!({}));
        let plugin_type_str = match manifest.plugin_type {
            PluginType::Platform => "platform",
            PluginType::Mcp => "mcp",
            PluginType::Provider => "provider",
        };

        match upsert_plugin(
            pool,
            &manifest.name,
            plugin_type_str,
            &manifest.version,
            Some(source_type),
            &manifest_json,
            &serde_json::json!({}),
        )
        .await
        {
            Ok(row) => {
                tracing::info!(
                    "Synced plugin '{}' (type={}, source={})",
                    row.name,
                    row.plugin_type,
                    source_type
                );
                seen_names.insert(manifest.name.clone());
            }
            Err(e) => {
                tracing::warn!("Failed to upsert plugin '{}': {:?}", manifest.name, e);
            }
        }
    }

    // Remove DB entries for plugins that no longer exist on disk
    let all_db_plugins = list_plugins(pool).await?;
    for db_plugin in &all_db_plugins {
        if seen_names.contains(&db_plugin.name) {
            continue;
        }
        // Skip bundled plugins that still exist in the repo dir
        if db_plugin.source.as_deref() == Some("bundled") {
            let bundled_path = format!(
                "{}/plugins/{}/{}",
                workspace_dir, db_plugin.plugin_type, db_plugin.name
            );
            if Path::new(&bundled_path).exists() {
                continue;
            }
        }
        tracing::info!(
            "Removing stale plugin '{}' from registry (no longer on disk)",
            db_plugin.name
        );
        if let Err(e) = delete_plugin(pool, &db_plugin.name).await {
            tracing::warn!("Failed to remove stale plugin '{}': {:?}", db_plugin.name, e);
        }
    }

    tracing::info!("Plugin sync complete ({} plugins discovered)", discovered.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_manifest_valid() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("plugin.json");
        let manifest_content = r#"{
            "name": "test-plugin",
            "version": "1.0.0",
            "type": "mcp",
            "description": "A test plugin",
            "entrypoint": {
                "command": "python3",
                "args": ["server.py"],
                "transport": "stdio"
            },
            "config_schema": [
                {
                    "key": "api_key",
                    "label": "API Key",
                    "type": "secret",
                    "required": true,
                    "description": "Your API key"
                }
            ]
        }"#;
        std::fs::write(&manifest_path, manifest_content).unwrap();

        let manifest = load_manifest(manifest_path.to_str().unwrap()).unwrap();
        assert_eq!(manifest.name, "test-plugin");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.plugin_type, PluginType::Mcp);
        assert_eq!(manifest.entrypoint.as_ref().unwrap().command, "python3");
        assert_eq!(manifest.config_schema.len(), 1);
        assert_eq!(manifest.config_schema[0].key, "api_key");
    }

    #[test]
    fn test_load_manifest_invalid_path() {
        let result = load_manifest("/nonexistent/plugin.json");
        assert!(result.is_err());
    }

    #[test]
    fn test_load_manifest_empty_name() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("plugin.json");
        let manifest_content = r#"{
            "name": "",
            "type": "mcp",
            "entrypoint": {
                "command": "test"
            }
        }"#;
        std::fs::write(&manifest_path, manifest_content).unwrap();
        let result = load_manifest(manifest_path.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty 'name'"));
    }

    #[test]
    fn test_manifest_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("plugin.json");
        let manifest_content = r#"{
            "name": "minimal",
            "type": "platform",
            "entrypoint": {
                "command": "./run.sh"
            }
        }"#;
        std::fs::write(&manifest_path, manifest_content).unwrap();
        let manifest = load_manifest(manifest_path.to_str().unwrap()).unwrap();
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.entrypoint.as_ref().unwrap().transport, "stdio");
        assert!(manifest.entrypoint.as_ref().unwrap().url.is_none());
        assert!(manifest.config_schema.is_empty());
        assert!(manifest.capabilities.is_none());
    }

    #[test]
    fn test_parse_plugin_type() {
        assert_eq!(serde_json::from_str::<PluginType>("\"platform\"").unwrap(), PluginType::Platform);
        assert_eq!(serde_json::from_str::<PluginType>("\"mcp\"").unwrap(), PluginType::Mcp);
    }

    #[test]
    fn test_parse_field_type() {
        assert_eq!(serde_json::from_str::<FieldType>("\"string\"").unwrap(), FieldType::String);
        assert_eq!(serde_json::from_str::<FieldType>("\"secret\"").unwrap(), FieldType::Secret);
        assert_eq!(serde_json::from_str::<FieldType>("\"boolean\"").unwrap(), FieldType::Boolean);
        assert_eq!(serde_json::from_str::<FieldType>("\"integer\"").unwrap(), FieldType::Integer);
        assert_eq!(serde_json::from_str::<FieldType>("\"enum\"").unwrap(), FieldType::Enum);
        assert_eq!(serde_json::from_str::<FieldType>("\"multi_select\"").unwrap(), FieldType::MultiSelect);
    }

    #[test]
    fn test_config_schema_field_validation() {
        let json = r#"{
            "key": "api_key",
            "label": "API Key",
            "type": "secret",
            "required": true,
            "secret": true,
            "description": "Your API key",
            "min": 8,
            "max": 128,
            "format": "password"
        }"#;
        let field: ConfigSchemaField = serde_json::from_str(json).unwrap();
        assert_eq!(field.field_type, FieldType::Secret);
        assert!(field.required);
        assert!(field.secret);
        assert_eq!(field.min, Some(8));
        assert_eq!(field.max, Some(128));
        assert_eq!(field.format, Some("password".to_string()));
    }

    #[test]
    fn test_enrich_plugin_with_schema() {
        let row = PluginRegistryRow {
            id: 1,
            name: "test".to_string(),
            plugin_type: "mcp".to_string(),
            version: "1.0.0".to_string(),
            source: Some("bundled".to_string()),
            status: "enabled".to_string(),
            manifest: serde_json::json!({
                "config_schema": [
                    {"key": "key1", "label": "Key 1", "type": "string"}
                ]
            }),
            config: serde_json::json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let detail = enrich_plugin(&row);
        assert_eq!(detail.config_schema.len(), 1);
        assert_eq!(detail.config_schema[0].key, "key1");
    }
}
