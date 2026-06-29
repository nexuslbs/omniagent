//! Plugin state management via YAML files (platforms.yml, tools.yml, providers.yml).
//!
//! These files live at `<data_dir>/<name>.yml` and manage the enabled/disabled
//! status and configuration for each plugin. The plugin manifests themselves
//! are loaded from `plugin.json` files on disk via the `plugin::installer` module.
//!
//! Reads happen on every access (no caching — files are tiny, parsing is ~50µs).
//! Writes are atomic: write to `.tmp` → fsync → rename.

use crate::error::{Error, AppResult, ErrorContext};
use crate::err_msg;
use crate::plugin::{ConfigSchemaField, PluginManifest, PluginType};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// YAML types
// ---------------------------------------------------------------------------

/// A single plugin entry in a YAML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginYamlEntry {
    pub enabled: bool,
    /// If true, this plugin ships with omniagent (its handler is compiled in or bundled).
    /// Built-in tools are disabled by default when not present in the YAML file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<bool>,
    #[serde(default = "default_config")]
    pub config: serde_json::Value,
}

fn default_config() -> serde_json::Value {
    serde_json::json!({})
}

/// Wrapper for YAML files with a named section (e.g. `platforms:`, `tools:`, `providers:`).
#[derive(Debug, Serialize, Deserialize)]
pub struct PluginYamlFile {
    pub platforms: Option<BTreeMap<String, PluginYamlEntry>>,
    pub tools: Option<BTreeMap<String, PluginYamlEntry>>,
    pub providers: Option<BTreeMap<String, PluginYamlEntry>>,
}

// ---------------------------------------------------------------------------
// PluginYamlType enum
// ---------------------------------------------------------------------------

/// Which YAML file a plugin belongs to.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginYamlType {
    Platform,
    Tool,
    Provider,
}

impl PluginYamlType {
    /// The section name in the YAML file (e.g. "platforms", "tools", "providers").
    pub fn file_name(&self) -> &str {
        match self {
            PluginYamlType::Platform => "platforms",
            PluginYamlType::Tool => "tools",
            PluginYamlType::Provider => "providers",
        }
    }

    /// The YAML filename (e.g. "platforms.yml", "tools.yml", "providers.yml").
    pub fn yaml_file(&self) -> &str {
        match self {
            PluginYamlType::Platform => "platforms.yml",
            PluginYamlType::Tool => "tools.yml",
            PluginYamlType::Provider => "providers.yml",
        }
    }

    /// Map from a manifest `PluginType` (Platform, Mcp, Provider) to the YAML file type.
    pub fn from_plugin_type(pt: &PluginType) -> Self {
        match pt {
            PluginType::Platform => PluginYamlType::Platform,
            PluginType::Mcp => PluginYamlType::Tool,
            PluginType::Provider => PluginYamlType::Provider,
        }
    }

    /// Map from a string representation (used by installer/manifest JSON).
    pub fn from_type_str(s: &str) -> Self {
        match s {
            "platform" => PluginYamlType::Platform,
            "mcp" => PluginYamlType::Tool,
            "provider" => PluginYamlType::Provider,
            _ => PluginYamlType::Tool, // fallback
        }
    }

    /// Return the string representation for the API response (compatible with frontend).
    pub fn to_type_str(&self) -> &str {
        match self {
            PluginYamlType::Platform => "platform",
            PluginYamlType::Tool => "mcp",
            PluginYamlType::Provider => "provider",
        }
    }
}

// ---------------------------------------------------------------------------
// API response type (backward-compatible with dashboard)
// ---------------------------------------------------------------------------

/// Plugin detail as returned by the HTTP API — matches the format the frontend expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default)]
    pub resolved_env: HashMap<String, String>,
    /// Empty string for YAML-based plugins (no created_at in file).
    pub created_at: String,
    /// Empty string for YAML-based plugins.
    pub updated_at: String,
    /// True if the plugin is a Rust crate that hasn't been compiled yet
    #[serde(default)]
    pub needs_build: bool,
}

// ---------------------------------------------------------------------------
// File path helpers
// ---------------------------------------------------------------------------

fn file_path(data_dir: &str, pt: &PluginYamlType) -> PathBuf {
    PathBuf::from(data_dir).join(pt.yaml_file())
}

// ---------------------------------------------------------------------------
// Low-level YAML I/O
// ---------------------------------------------------------------------------

/// Load the raw entries map from a YAML file, or return an empty map if file doesn't exist.
pub fn load_raw(data_dir: &str, pt: &PluginYamlType) -> AppResult<BTreeMap<String, PluginYamlEntry>> {
    let path = file_path(data_dir, pt);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let content =
        fs::read_to_string(&path).ctx(format!("Failed to read {}", path.display()))?;
    let file: PluginYamlFile = serde_yaml::from_str(&content)
        .ctx(format!("Failed to parse {}", path.display()))?;

    let section_name = pt.file_name();
    let entries = match section_name {
        "platforms" => file.platforms.unwrap_or_default(),
        "tools" => file.tools.unwrap_or_default(),
        "providers" => file.providers.unwrap_or_default(),
        _ => BTreeMap::new(),
    };
    Ok(entries)
}

/// Save entries to a YAML file (atomic write: .tmp → fsync → rename).
fn save_file(
    data_dir: &str,
    pt: &PluginYamlType,
    entries: BTreeMap<String, PluginYamlEntry>,
) -> AppResult<()> {
    let path = file_path(data_dir, pt);
    let tmp_path = path.with_extension("yml.tmp");

    // Build the file with the correct section name
    let file = match pt.file_name() {
        "platforms" => PluginYamlFile {
            platforms: Some(entries),
            tools: None,
            providers: None,
        },
        "tools" => PluginYamlFile {
            platforms: None,
            tools: Some(entries),
            providers: None,
        },
        "providers" => PluginYamlFile {
            platforms: None,
            tools: None,
            providers: Some(entries),
        },
        _ => err_msg!("Unknown plugin YAML type: {}", pt.file_name()),
    };

    let yaml = serde_yaml::to_string(&file).ctx("Failed to serialize plugin YAML")?;

    {
        let mut f = fs::File::create(&tmp_path)
            .ctx(format!("Failed to create {}", tmp_path.display()))?;
        f.write_all(yaml.as_bytes())
            .ctx(format!("Failed to write {}", tmp_path.display()))?;
        f.sync_all()
            .ctx(format!("Failed to fsync {}", tmp_path.display()))?;
    }

    fs::rename(&tmp_path, &path).ctx(format!(
        "Failed to rename {} -> {}",
        tmp_path.display(),
        path.display()
    ))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Get a single entry from a YAML file by plugin name.
pub fn get_entry(
    data_dir: &str,
    pt: &PluginYamlType,
    name: &str,
) -> AppResult<Option<PluginYamlEntry>> {
    let entries = load_raw(data_dir, pt)?;
    Ok(entries.get(name).cloned())
}

/// Set a plugin entry (enabled + config) in a YAML file. Creates the entry if it doesn't exist.
pub fn set_entry(
    data_dir: &str,
    pt: &PluginYamlType,
    name: &str,
    enabled: bool,
    config: serde_json::Value,
) -> AppResult<PluginYamlEntry> {
    let mut entries = load_raw(data_dir, pt)?;
    // Preserve existing builtin flag if the entry already exists
    let existing_builtin = entries.get(name).and_then(|e| e.builtin);
    let entry = PluginYamlEntry {
        enabled,
        builtin: existing_builtin.or(Some(false)),
        config,
    };
    entries.insert(name.to_string(), entry.clone());
    save_file(data_dir, pt, entries)?;
    Ok(entry)
}

/// Set only the enabled/disabled status of a plugin.
pub fn set_enabled(
    data_dir: &str,
    pt: &PluginYamlType,
    name: &str,
    enabled: bool,
) -> AppResult<PluginYamlEntry> {
    let mut entries = load_raw(data_dir, pt)?;
    let entry = entries
        .get_mut(name)
        .ok_or_else(|| Error::Message(format!("Plugin '{}' not found in YAML", name)))?;
    entry.enabled = enabled;
    let result = entry.clone();
    save_file(data_dir, pt, entries)?;
    Ok(result)
}

/// Update only the config of a plugin.
pub fn update_config(
    data_dir: &str,
    pt: &PluginYamlType,
    name: &str,
    config: serde_json::Value,
) -> AppResult<PluginYamlEntry> {
    let mut entries = load_raw(data_dir, pt)?;
    let entry = entries
        .get_mut(name)
        .ok_or_else(|| Error::Message(format!("Plugin '{}' not found in YAML", name)))?;
    entry.config = config;
    let result = entry.clone();
    save_file(data_dir, pt, entries)?;
    Ok(result)
}

/// Remove a plugin entry from a YAML file.
pub fn remove_entry(data_dir: &str, pt: &PluginYamlType, name: &str) -> AppResult<bool> {
    let mut entries = load_raw(data_dir, pt)?;
    let existed = entries.remove(name).is_some();
    if existed {
        save_file(data_dir, pt, entries)?;
    }
    Ok(existed)
}

// ---------------------------------------------------------------------------
// Enriched plugin building
// ---------------------------------------------------------------------------

/// Resolve $env:VAR_NAME prefix in a config value string.
/// Returns the env var value if found, or the original string if not (graceful fallback).
pub fn resolve_config_value(value: &str) -> String {
    if let Some(var_name) = value.strip_prefix("$env:") {
        return std::env::var(var_name).unwrap_or_else(|_| {
            tracing::warn!("Config references $env:{} but env var is not set", var_name);
            value.to_string()
        });
    }
    value.to_string()
}

/// Resolve ${VAR} references in a string against the process environment.
/// Unresolvable references are replaced with empty string.
fn resolve_env_var(value: &str) -> String {
    let mut result = value.to_string();
    loop {
        let before = result.clone();
        while let Some(start) = result.find("${") {
            if let Some(end) = result[start..].find('}') {
                let var_name = &result[start + 2..start + end];
                match std::env::var(var_name) {
                    Ok(val) => {
                        result.replace_range(start..start + end + 1, &val);
                    }
                    Err(_) => {
                        result.replace_range(start..start + end + 1, "");
                    }
                }
            } else {
                break;
            }
        }
        if result == before {
            break;
        }
    }
    result
}

/// Build a `PluginDetail` from a disk manifest and YAML state.
/// Extract the YAML key for a plugin from its discovery path.
///
/// For sourced plugins (installed/bundled), the key is the parent directory name
/// of the plugin.json file. For mcp_config sources, the manifest name
/// itself is the key.
fn extract_plugin_key(manifest: &PluginManifest, source: &str, base_path: &str) -> String {
    match source {
        "mcp_config" => manifest.name.clone(),
        _ => {
            // Extract key from base_path parent directory name
            // e.g., "/opt/data/plugins/mcp/docker-compose/plugin.json" → directory name
            if let Some(parent) = std::path::Path::new(base_path).parent() {
                if let Some(dir_name) = parent.file_name().and_then(|n| n.to_str()) {
                    return dir_name.replace('-', "_");
                }
            }
            manifest.name.clone()
        }
    }
}

fn build_plugin_detail(
    manifest: &PluginManifest,
    source: &str,
    yaml_entry: Option<&PluginYamlEntry>,
    key: Option<&str>,
    plugin_dir: Option<&str>,
) -> PluginDetail {
    let enabled = yaml_entry
        .map(|e| e.enabled)
        // Plugins are enabled by default when not present in YAML
        .unwrap_or(true);
    let config = yaml_entry
        .map(|e| e.config.clone())
        .unwrap_or(serde_json::json!({}));

    // Parse config_schema from manifest
    let mut config_schema: Vec<ConfigSchemaField> = manifest.config_schema.clone();

    // Populate allowed_values from dynamic enum cache for fields with refresh_url
    for field in config_schema.iter_mut() {
        if let Some(ref url) = field.refresh_url {
            let cache = crate::plugin::DYNAMIC_ENUM_CACHE
                .lock()
                .expect("dynamic enum cache lock poisoned");
            if let Some(entry) = cache.get(url) {
                if entry.fetched_at.elapsed() < crate::plugin::DYNAMIC_ENUM_TTL {
                    field.allowed_values = Some(entry.values.clone());
                }
            }
        }
    }

    // Resolve env vars from manifest's env block, then merge config on top
    let manifest_env = &manifest.env;
    let mut resolved = HashMap::new();
    for (key, val) in manifest_env {
        let resolved_val = resolve_env_var(val);
        resolved.insert(key.clone(), resolved_val);
    }
    // Config values override env vars
    if let Some(config_obj) = config.as_object() {
        for (key, val) in config_obj {
            let raw = val.as_str().map(|s| s.to_string()).unwrap_or_default();
            let resolved_val = resolve_config_value(&raw);
            resolved.insert(key.clone(), resolved_val);
        }
    }

    // For provider plugins, resolve api_key from process env as fallback
    if manifest.plugin_type == PluginType::Provider {
        let name_upper = manifest.name.to_uppercase().replace('-', "_");
        let provider_api_key_var = format!("{}_API_KEY", name_upper);
        for field in &config_schema {
            if field.key == "api_key" && !resolved.contains_key("api_key") {
                let env_val = crate::llm::resolve_llm_api_key(Some(
                    &std::env::var(&provider_api_key_var).unwrap_or_default(),
                ));
                if !env_val.is_empty() {
                    resolved.insert("api_key".to_string(), env_val);
                }
                break;
            }
        }
    }

    let plugin_type_str = match manifest.plugin_type {
        PluginType::Platform => "platform",
        PluginType::Mcp => "mcp",
        PluginType::Provider => "provider",
    };

    // Determine the display key: use the explicit key if provided, fall back to manifest name
    let display_key = key.unwrap_or(&manifest.name).to_string();

    // Inject the manifest's name as the label for subtitle display
    let mut manifest_value = serde_json::to_value(manifest).unwrap_or_default();
    if let Some(obj) = manifest_value.as_object_mut() {
        if !obj.contains_key("label") {
            obj.insert("label".to_string(), serde_json::json!(manifest.name));
        }
    }

    // Compute needs_build: Rust crate with no compiled binary
    let needs_build = plugin_dir
        .map(|dir| {
            let cargo_toml = std::path::Path::new(dir).join("Cargo.toml");
            if !cargo_toml.exists() {
                return false;
            }
            // Check if binary exists at target/release/<package_name>
            // The package name matches the directory name (with underscores)
            let dir_name = std::path::Path::new(dir)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let binary_path = std::path::Path::new(dir)
                .join("target")
                .join("release")
                .join(dir_name);
            // Also try with underscores (Rust convention)
            let binary_with_underscores = std::path::Path::new(dir)
                .join("target")
                .join("release")
                .join(dir_name.replace('-', "_"));
            !binary_path.exists() && !binary_with_underscores.exists()
        })
        .unwrap_or(false);

    PluginDetail {
        id: 0,
        name: display_key,
        plugin_type: plugin_type_str.to_string(),
        version: manifest.version.clone(),
        source: Some(source.to_string()),
        status: if enabled {
            "enabled".to_string()
        } else {
            "disabled".to_string()
        },
        manifest: manifest_value,
        config,
        config_schema,
        resolved_env: resolved,
        created_at: String::new(),
        updated_at: String::new(),
        needs_build,
    }
}

// ---------------------------------------------------------------------------
// Discovery + enrichment (combines disk manifests with YAML state)
// ---------------------------------------------------------------------------

/// List all plugins, combining disk discovery with YAML overrides.
pub fn list_plugins(data_dir: &str) -> AppResult<Vec<PluginDetail>> {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());
    let discovered = crate::plugin::installer::discover_plugins(data_dir, &workspace_dir);

    // Pre-load all YAML entries for efficient lookup
    let platform_entries = load_raw(data_dir, &PluginYamlType::Platform)?;
    let tool_entries = load_raw(data_dir, &PluginYamlType::Tool)?;
    let provider_entries = load_raw(data_dir, &PluginYamlType::Provider)?;

    let mut results: Vec<PluginDetail> = Vec::new();

    for (manifest, source, base_path) in &discovered {
        let yaml_type = PluginYamlType::from_plugin_type(&manifest.plugin_type);
        let yaml_entry = match yaml_type {
            PluginYamlType::Platform => platform_entries.get(&manifest.name),
            PluginYamlType::Tool => tool_entries.get(&manifest.name),
            PluginYamlType::Provider => provider_entries.get(&manifest.name),
        };

        let key = extract_plugin_key(manifest, source, base_path);
        // Plugin directory is the parent of the plugin.json path
        let plugin_dir = std::path::Path::new(base_path).parent().and_then(|p| p.to_str());
        results.push(build_plugin_detail(
            manifest,
            source,
            yaml_entry,
            Some(&key),
            plugin_dir,
        ));
    }

    // Sort by name for deterministic ordering
    results.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(results)
}

/// Get a single plugin by name, combining disk discovery with YAML state.
pub fn get_plugin(data_dir: &str, name: &str) -> AppResult<Option<PluginDetail>> {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());
    let discovered = crate::plugin::installer::discover_plugins(data_dir, &workspace_dir);

    // Pre-load YAML entries
    let platform_entries = load_raw(data_dir, &PluginYamlType::Platform)?;
    let tool_entries = load_raw(data_dir, &PluginYamlType::Tool)?;
    let provider_entries = load_raw(data_dir, &PluginYamlType::Provider)?;

    for (manifest, source, base_path) in &discovered {
        if manifest.name == name {
            let yaml_type = PluginYamlType::from_plugin_type(&manifest.plugin_type);
            let yaml_entry = match yaml_type {
                PluginYamlType::Platform => platform_entries.get(&manifest.name),
                PluginYamlType::Tool => tool_entries.get(&manifest.name),
                PluginYamlType::Provider => provider_entries.get(&manifest.name),
            };
            let key = extract_plugin_key(manifest, source, base_path);
            let plugin_dir = std::path::Path::new(base_path).parent().and_then(|p| p.to_str());
            return Ok(Some(build_plugin_detail(
                manifest,
                source,
                yaml_entry,
                Some(&key),
                plugin_dir,
            )));
        }
    }

    Ok(None)
}

/// Get enabled provider names for the settings API.
pub fn get_enabled_providers(data_dir: &str) -> AppResult<Vec<(String, String)>> {
    let entries = load_raw(data_dir, &PluginYamlType::Provider)?;

    let workspace_dir =
        std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());
    let discovered = crate::plugin::installer::discover_plugins(data_dir, &workspace_dir);

    let mut providers: Vec<(String, String)> = Vec::new();

    for (name, entry) in &entries {
        if !entry.enabled {
            continue;
        }
        // Get description from manifest if available
        let description = discovered
            .iter()
            .find(|(m, _, _)| m.name == *name)
            .and_then(|(m, _, _)| m.description.clone())
            .unwrap_or_else(|| name.clone());
        providers.push((name.clone(), description));
    }

    // Also include plugins discovered from disk that don't have YAML entries yet (default: enabled)
    for (manifest, _source, _base_path) in &discovered {
        if manifest.plugin_type != PluginType::Provider {
            continue;
        }
        if !entries.contains_key(&manifest.name) {
            providers.push((
                manifest.name.clone(),
                manifest
                    .description
                    .clone()
                    .unwrap_or_else(|| manifest.name.clone()),
            ));
        }
    }

    providers.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(providers)
}

/// Check if a provider exists and is enabled.
pub fn provider_exists_and_enabled(data_dir: &str, name: &str) -> AppResult<bool> {
    let entries = load_raw(data_dir, &PluginYamlType::Provider)?;
    if let Some(entry) = entries.get(name) {
        return Ok(entry.enabled);
    }
    // Check if plugin exists on disk at all (even if no YAML entry yet)
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());
    let discovered = crate::plugin::installer::discover_plugins(data_dir, &workspace_dir);
    Ok(discovered
        .iter()
        .any(|(m, _, _)| m.name == name && m.plugin_type == PluginType::Provider))
}

/// Get a provider's plugin type from disk discovery.
pub fn get_disk_plugin_type(data_dir: &str, name: &str) -> AppResult<Option<String>> {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());
    let discovered = crate::plugin::installer::discover_plugins(data_dir, &workspace_dir);
    for (manifest, _, _) in &discovered {
        if manifest.name == name {
            let type_str = match manifest.plugin_type {
                PluginType::Platform => "platform",
                PluginType::Mcp => "mcp",
                PluginType::Provider => "provider",
            };
            return Ok(Some(type_str.to_string()));
        }
    }
    Ok(None)
}

/// Get a plugin's manifest (PluginManifest) from disk discovery.
pub fn get_disk_manifest(data_dir: &str, name: &str) -> AppResult<Option<PluginManifest>> {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());
    let discovered = crate::plugin::installer::discover_plugins(data_dir, &workspace_dir);
    Ok(discovered
        .into_iter()
        .find(|(m, _, _)| m.name == name)
        .map(|(m, _, _)| m))
}

// ---------------------------------------------------------------------------
// Refresh dynamic enum models (moved from plugin/mod.rs, adapted for YAML)
// ---------------------------------------------------------------------------

/// Fetch model IDs from an OpenAI-compatible `/v1/models` endpoint and return them.
/// If an `api_key` is provided, it's sent as a Bearer token in the Authorization header.
pub async fn fetch_enum_values(url: &str, api_key: Option<&str>) -> AppResult<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ctx("Failed to build HTTP client")?;
    let mut req = client.get(url);
    if let Some(key) = api_key {
        req = req.header("Authorization", format!("Bearer {}", key));
    }
    let resp = req
        .send()
        .await
        .ctx(format!("Failed to fetch {}", url))?;
    let json: serde_json::Value = resp
        .json()
        .await
        .ctx(format!("Failed to parse response from {}", url))?;

    let mut values = Vec::new();
    if let Some(data) = json["data"].as_array() {
        for item in data {
            if let Some(id) = item["id"].as_str() {
                values.push(id.to_string());
            }
        }
    }

    if values.is_empty() {
        err_msg!(
            "No model IDs found in response from {} (expected {{data: [{{id: ...}}]}})",
            url
        );
    }

    Ok(values)
}

/// Refresh dynamic enum values for a specific plugin by name.
///
/// Fetches from the plugin's `refresh_url` (if any), updates the in-memory cache,
/// and returns the enriched plugin detail with fresh `allowed_values`.
/// Returns `None` if the plugin has no `refresh_url` fields.
pub async fn refresh_plugin_models(data_dir: &str, name: &str) -> AppResult<Option<PluginDetail>> {
    let detail = get_plugin(data_dir, name)?
        .ok_or_else(|| Error::Message(format!("Plugin '{}' not found", name)))?;

    // Re-parse config_schema from the manifest to get mutable fields
    let manifest_value = &detail.manifest;
    let mut config_schema: Vec<ConfigSchemaField> = manifest_value["config_schema"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<ConfigSchemaField>(v.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut had_refresh = false;

    for field in config_schema.iter_mut() {
        let refresh_url = match &field.refresh_url {
            Some(url) if !url.is_empty() => url.clone(),
            _ => continue,
        };
        had_refresh = true;

        // Resolve API key: try {NAME}_API_KEY, then LLM_API_KEY
        let provider_key =
            std::env::var(format!("{}_API_KEY", name.to_uppercase().replace('-', "_")))
                .unwrap_or_default();
        let api_key = {
            let key = crate::llm::resolve_llm_api_key(Some(&provider_key));
            if key.is_empty() {
                None
            } else {
                Some(key)
            }
        };

        match fetch_enum_values(&refresh_url, api_key.as_deref()).await {
            Ok(values) => {
                field.allowed_values = Some(values.clone());
                let mut cache = crate::plugin::DYNAMIC_ENUM_CACHE
                    .lock()
                    .expect("dynamic enum cache lock poisoned");
                cache.insert(
                    refresh_url,
                    crate::plugin::DynamicEnumEntry {
                        values,
                        fetched_at: std::time::Instant::now(),
                    },
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to refresh dynamic enum from {}: {:#}",
                    refresh_url,
                    e
                );
            }
        }
    }

    if had_refresh {
        let mut result = detail;
        result.config_schema = config_schema;
        Ok(Some(result))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_data_dir() -> (tempfile::TempDir, String) {
        let dir = tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        (dir, path)
    }

    fn write_test_file(data_dir: &str, pt: &PluginYamlType, content: &str) {
        let path = file_path(data_dir, pt);
        std::fs::write(&path, content).unwrap();
    }

    #[test]
    fn test_load_raw_empty() {
        let (_d, path) = test_data_dir();
        let entries = load_raw(&path, &PluginYamlType::Platform).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_load_raw_platforms() {
        let (_d, path) = test_data_dir();
        write_test_file(
            &path,
            &PluginYamlType::Platform,
            r#"
platforms:
  telegram:
    enabled: true
    config:
      bot_token: "12345"
  mattermost:
    enabled: false
    config: {}
"#,
        );
        let entries = load_raw(&path, &PluginYamlType::Platform).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries["telegram"].enabled);
        assert_eq!(
            entries["telegram"].config["bot_token"],
            serde_json::json!("12345")
        );
        assert!(!entries["mattermost"].enabled);
    }

    #[test]
    fn test_load_raw_providers() {
        let (_d, path) = test_data_dir();
        write_test_file(
            &path,
            &PluginYamlType::Provider,
            r#"
providers:
  anthropic:
    enabled: false
    config:
      default_model: claude-sonnet-4
  openai:
    enabled: true
    config: {}
"#,
        );
        let entries = load_raw(&path, &PluginYamlType::Provider).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(!entries["anthropic"].enabled);
        assert!(entries["openai"].enabled);
    }

    #[test]
    fn test_set_and_get_entry() {
        let (_d, path) = test_data_dir();
        let pt = PluginYamlType::Tool;

        // Set a new entry
        let entry = set_entry(
            &path,
            &pt,
            "my-tool",
            true,
            serde_json::json!({"key": "value"}),
        )
        .unwrap();
        assert!(entry.enabled);
        assert_eq!(entry.config["key"], serde_json::json!("value"));

        // Retrieve it
        let loaded = get_entry(&path, &pt, "my-tool").unwrap().unwrap();
        assert!(loaded.enabled);
        assert_eq!(loaded.config["key"], serde_json::json!("value"));
    }

    #[test]
    fn test_set_enabled() {
        let (_d, path) = test_data_dir();
        let pt = PluginYamlType::Platform;

        // Create entry first
        set_entry(&path, &pt, "test", false, serde_json::json!({})).unwrap();

        // Enable it
        set_enabled(&path, &pt, "test", true).unwrap();
        let entry = get_entry(&path, &pt, "test").unwrap().unwrap();
        assert!(entry.enabled);

        // Disable it
        set_enabled(&path, &pt, "test", false).unwrap();
        let entry = get_entry(&path, &pt, "test").unwrap().unwrap();
        assert!(!entry.enabled);
    }

    #[test]
    fn test_update_config() {
        let (_d, path) = test_data_dir();
        let pt = PluginYamlType::Tool;

        set_entry(&path, &pt, "tool", true, serde_json::json!({"a": 1})).unwrap();
        update_config(&path, &pt, "tool", serde_json::json!({"b": 2})).unwrap();

        let entry = get_entry(&path, &pt, "tool").unwrap().unwrap();
        assert!(!entry.config.as_object().unwrap().contains_key("a"));
        assert_eq!(entry.config["b"], serde_json::json!(2));
    }

    #[test]
    fn test_remove_entry() {
        let (_d, path) = test_data_dir();
        let pt = PluginYamlType::Platform;

        set_entry(&path, &pt, "p1", true, serde_json::json!({})).unwrap();
        assert!(remove_entry(&path, &pt, "p1").unwrap());
        assert!(!remove_entry(&path, &pt, "p1").unwrap());
        assert!(load_raw(&path, &pt).unwrap().is_empty());
    }

    #[test]
    fn test_atomic_write() {
        let (_d, path) = test_data_dir();
        let pt = PluginYamlType::Provider;

        set_entry(&path, &pt, "p1", true, serde_json::json!({})).unwrap();
        set_entry(&path, &pt, "p2", false, serde_json::json!({})).unwrap();

        // Verify no .tmp file remains
        let tmp = PathBuf::from(&path)
            .join(pt.yaml_file())
            .with_extension("yml.tmp");
        assert!(!tmp.exists());

        // Verify file is valid YAML
        let content = std::fs::read_to_string(file_path(&path, &pt)).unwrap();
        let parsed: PluginYamlFile = serde_yaml::from_str(&content).unwrap();
        assert!(parsed.providers.is_some());
        assert_eq!(parsed.providers.unwrap().len(), 2);
    }

    #[test]
    fn test_map_plugin_types() {
        assert_eq!(
            PluginYamlType::from_plugin_type(&PluginType::Platform),
            PluginYamlType::Platform
        );
        assert_eq!(
            PluginYamlType::from_plugin_type(&PluginType::Mcp),
            PluginYamlType::Tool
        );
        assert_eq!(
            PluginYamlType::from_plugin_type(&PluginType::Provider),
            PluginYamlType::Provider
        );
    }

    #[test]
    fn test_file_paths() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        assert_eq!(
            file_path(path, &PluginYamlType::Platform),
            PathBuf::from(path).join("platforms.yml")
        );
        assert_eq!(
            file_path(path, &PluginYamlType::Tool),
            PathBuf::from(path).join("tools.yml")
        );
        assert_eq!(
            file_path(path, &PluginYamlType::Provider),
            PathBuf::from(path).join("providers.yml")
        );
    }
}
