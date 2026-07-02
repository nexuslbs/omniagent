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
use sql_forge::sql_forge;
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

/// Minimal row for resolving a $secret: reference from the secrets table.
#[derive(Debug, sqlx::FromRow)]
struct SecretValueRow {
    current_value: String,
}

/// Resolve $env:VAR_NAME prefix in a config value string.
/// Returns the env var value if found, or the original string if not (graceful fallback).
///
/// For `$secret:` prefix, the value is passed through unresolved
/// (use the async `resolve_config_ref_value` for full resolution with a DB pool).
///
/// This function is for YAML plugin config values ONLY. It does NOT handle
/// `${VAR}` syntax — that legacy format is only supported in `plugin.json`
/// and `mcp-config.json` env blocks via `resolve_env_var`.
pub fn resolve_config_value(value: &str) -> String {
    if let Some(var_name) = value.strip_prefix("$env:") {
        return std::env::var(var_name).unwrap_or_else(|_| {
            tracing::warn!("Config references $env:{} but env var is not set", var_name);
            value.to_string()
        });
    }
    // $secret: passes through — can't resolve without DB pool
    if value.starts_with("$secret:") {
        return value.to_string();
    }
    // No ${VAR} resolution — YAML config uses $env: and $secret: only
    value.to_string()
}

/// Resolve ${VAR} references in a string against the process environment.
/// Unresolvable references are replaced with empty string.
///
/// This is for legacy `plugin.json`/`mcp-config.json` env block values only.
/// YAML plugin config values should use `$env:` or `$secret:` instead.
pub fn resolve_legacy_env_vars(value: &str) -> String {
    resolve_legacy_vars(value)
}

/// Resolve `${VAR}` references (legacy format) in a value string.
fn resolve_env_var(value: &str) -> String {
    resolve_legacy_vars(value)
}

/// Resolve `${VAR}` references (legacy format) in a value string.
fn resolve_legacy_vars(value: &str) -> String {
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

// ---------------------------------------------------------------------------
// Shared async config reference resolvers (for $env:, $secret:, ${VAR})
// ---------------------------------------------------------------------------

/// Resolve `$env:VAR` and `$secret:NAME` references in a single value.
///
/// - `$env:VAR` — reads from process environment via `std::env::var`
/// - `$secret:NAME` — reads from the `secrets` table in the DB
///
/// For `$secret:`, returns the original string if DB lookup fails.
/// Does NOT handle `${VAR}` — that legacy syntax is only resolved in
/// `plugin.json`/`mcp-config.json` env blocks via the sync resolve path.
pub async fn resolve_config_ref_value(value: &str, pool: &sqlx::PgPool) -> String {
    if let Some(var_name) = value.strip_prefix("$env:") {
        return std::env::var(var_name).unwrap_or_else(|_| {
            tracing::warn!("Config ref $env:{} env var not set", var_name);
            value.to_string()
        });
    }
    if let Some(secret_name) = value.strip_prefix("$secret:") {
        match sql_forge!(
            SecretValueRow,
            r#"SELECT current_value FROM secrets WHERE name = :secret_name"#,
            ( :secret_name = secret_name )
        )
        .fetch_optional(pool)
        .await
        {
            Ok(Some(row)) => return row.current_value,
            Ok(None) => {
                tracing::warn!(
                    "Config ref $secret:{} not found in secrets table",
                    secret_name
                );
            }
            Err(e) => {
                tracing::error!("DB error resolving $secret:{}: {:?}", secret_name, e);
            }
        }
    }
    value.to_string()
}

/// Resolve `$env:VAR` and `$secret:NAME` references in all values of a config map.
pub async fn resolve_config_refs(
    env: &mut HashMap<String, String>,
    pool: &sqlx::PgPool,
) {
    let keys: Vec<String> = env.keys().cloned().collect();
    for key in keys {
        if let Some(value) = env.remove(&key) {
            let resolved = resolve_config_ref_value(&value, pool).await;
            env.insert(key, resolved);
        }
    }
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
                    return dir_name.to_string();
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
    data_dir: &str,
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

    // Merge YAML config values into env with derived env keys
    // (e.g. connection_mode: websocket → PLUGINNAME_CONNECTION_MODE)
    // so the API response reflects what the subprocess will actually receive.
    let yaml_type = PluginYamlType::from_plugin_type(&manifest.plugin_type);
    merge_yaml_config_into_env(&mut resolved, &manifest.name, data_dir, &yaml_type);

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
            let dir_name = std::path::Path::new(dir)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            // Read package name from Cargo.toml for proper binary resolution
            let cargo_package_name = std::fs::read_to_string(&cargo_toml)
                .ok()
                .and_then(|content| {
                    content.lines().find_map(|line| {
                        let trimmed = line.trim();
                        if let Some(name) = trimmed.strip_prefix("name = \"") {
                            name.strip_suffix('\"').map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                });

            // All possible binary paths to check:
            // - Directory name convention (standalone plugin)
            // - Underscore variant (Rust convention)
            // - Package name from Cargo.toml (workspace members with mcp-server- prefix)
            let mut candidates = vec![
                format!("{}/target/release/{}", dir, dir_name),
                format!("{}/target/release/{}", dir, dir_name.replace('-', "_")),
            ];
            if let Some(ref pkg) = cargo_package_name {
                candidates.push(format!("{}/target/release/{}", dir, pkg));
                if pkg.contains('-') {
                    candidates.push(format!("{}/target/release/{}", dir, pkg.replace('-', "_")));
                }
            }

            !candidates.iter().any(|p| std::path::Path::new(p).exists())
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
            data_dir,
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
                data_dir,
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

        // Resolve API key from the provider's resolved plugin config
        let api_key = detail.config
            .get("api_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

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
// Plugin config → env helper
// ---------------------------------------------------------------------------

/// Merge YAML config values from a plugin's YAML file into its env map.
///
/// For each config value set in the YAML file for this plugin, derive the
/// corresponding environment variable name by uppercasing the config key
/// and prefixing with the plugin name (e.g. `connection_mode` for a plugin
/// named `mattermost` → `MATTERMOST_CONNECTION_MODE`) and override the env
/// map entry.
///
/// This allows plugin config fields (set via the dashboard or YAML) to take
/// effect without requiring those values to be defined in .env.
///
/// Works with all plugin types: platforms.yml, tools.yml, providers.yml.
pub fn merge_yaml_config_into_env(
    env: &mut HashMap<String, String>,
    plugin_name: &str,
    data_dir: &str,
    yaml_type: &PluginYamlType,
) {
    let prefix = plugin_name.to_uppercase().replace('-', "_");
    let yaml_path = PathBuf::from(data_dir).join(yaml_type.yaml_file());

    // Load the YAML file and find this plugin's config
    let config = (|| -> Option<serde_json::Value> {
        use std::collections::BTreeMap;
        let content = std::fs::read_to_string(yaml_path).ok()?;
        #[derive(Deserialize)]
        struct Root {
            #[serde(default)]
            platforms: Option<BTreeMap<String, Entry>>,
            #[serde(default)]
            tools: Option<BTreeMap<String, Entry>>,
            #[serde(default)]
            providers: Option<BTreeMap<String, Entry>>,
        }
        #[derive(Deserialize)]
        struct Entry {
            #[serde(default)]
            config: Option<serde_json::Value>,
        }
        let root: Root = serde_yaml::from_str(&content).ok()?;
        let section = match yaml_type.file_name() {
            "platforms" => root.platforms,
            "tools" => root.tools,
            "providers" => root.providers,
            _ => return None,
        };
        section?.get(plugin_name)?.config.clone()
    })();

    if let Some(ref entry_config) = config {
        if let Some(obj) = entry_config.as_object() {
            for (key, val) in obj {
                let env_key = format!("{}_{}", prefix, key.to_uppercase().replace('-', "_"));
                let raw = val.as_str().map(|s| s.to_string()).unwrap_or_default();
                let str_val = resolve_config_value(&raw);
                if !str_val.is_empty() {
                    env.insert(env_key, str_val);
                }
            }
        }
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

    #[test]
    fn test_merge_yaml_config_into_env_platform() {
        let (_d, path) = test_data_dir();
        write_test_file(
            &path,
            &PluginYamlType::Platform,
            r#"
platforms:
  mattermost:
    enabled: true
    config:
      connection_mode: websocket
      server_url: "https://mm.example.com"
"#,
        );
        let mut env = HashMap::new();
        env.insert("MATTERMOST_TOKEN".to_string(), "abc".to_string());
        merge_yaml_config_into_env(&mut env, "mattermost", &path, &PluginYamlType::Platform);
        assert_eq!(env.get("MATTERMOST_CONNECTION_MODE").unwrap(), "websocket");
        assert_eq!(env.get("MATTERMOST_SERVER_URL").unwrap(), "https://mm.example.com");
        assert_eq!(env.get("MATTERMOST_TOKEN").unwrap(), "abc"); // original key preserved
    }

    #[test]
    fn test_merge_yaml_config_into_env_tool() {
        let (_d, path) = test_data_dir();
        write_test_file(
            &path,
            &PluginYamlType::Tool,
            r#"
tools:
  my-tool:
    enabled: true
    config:
      api_url: "https://api.example.com/v1"
      timeout: "30"
"#,
        );
        let mut env = HashMap::new();
        merge_yaml_config_into_env(&mut env, "my-tool", &path, &PluginYamlType::Tool);
        assert_eq!(env.get("MY_TOOL_API_URL").unwrap(), "https://api.example.com/v1");
        assert_eq!(env.get("MY_TOOL_TIMEOUT").unwrap(), "30");
    }

    #[test]
    fn test_merge_yaml_config_into_env_provider() {
        let (_d, path) = test_data_dir();
        write_test_file(
            &path,
            &PluginYamlType::Provider,
            r#"
providers:
  deepseek:
    enabled: true
    config:
      default_model: deepseek-v4-flash
      api_base: "https://api.deepseek.com"
"#,
        );
        let mut env = HashMap::new();
        merge_yaml_config_into_env(&mut env, "deepseek", &path, &PluginYamlType::Provider);
        assert_eq!(env.get("DEEPSEEK_DEFAULT_MODEL").unwrap(), "deepseek-v4-flash");
        assert_eq!(env.get("DEEPSEEK_API_BASE").unwrap(), "https://api.deepseek.com");
    }

    #[test]
    fn test_merge_yaml_config_into_env_no_yaml_file() {
        let (_d, path) = test_data_dir();
        // No YAML file exists yet — should not error
        let mut env = HashMap::new();
        merge_yaml_config_into_env(&mut env, "ghost", &path, &PluginYamlType::Tool);
        assert!(env.is_empty());
    }

    #[test]
    fn test_merge_yaml_config_into_env_plugin_not_in_file() {
        let (_d, path) = test_data_dir();
        write_test_file(
            &path,
            &PluginYamlType::Platform,
            r#"
platforms:
  telegram:
    enabled: true
    config: {}
"#,
        );
        let mut env = HashMap::new();
        // Plugin "slack" not in the file — should not error, no env vars added
        merge_yaml_config_into_env(&mut env, "slack", &path, &PluginYamlType::Platform);
        assert!(env.is_empty());
    }

    #[test]
    fn test_merge_yaml_config_into_env_key_with_hyphens() {
        let (_d, path) = test_data_dir();
        write_test_file(
            &path,
            &PluginYamlType::Tool,
            r#"
tools:
  mcp-server-foo:
    enabled: true
    config:
      my-setting: "bar"
"#,
        );
        let mut env = HashMap::new();
        merge_yaml_config_into_env(&mut env, "mcp-server-foo", &path, &PluginYamlType::Tool);
        // Hyphens in plugin name → underscores in prefix
        assert_eq!(env.get("MCP_SERVER_FOO_MY_SETTING").unwrap(), "bar");
    }
}
