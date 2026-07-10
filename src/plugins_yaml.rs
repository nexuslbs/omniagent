//! Plugin state management via YAML files (platforms.yml, tools.yml, providers.yml).
//!
//! These files live at `<data_dir>/<name>.yml` and manage the enabled/disabled
//! status and configuration for each plugin. The plugin manifests themselves
//! are loaded from `plugin.json` files on disk via the `plugin::installer` module.
//!
//! Reads happen on every access (no caching — files are tiny, parsing is ~50µs).
//! Writes are atomic: write to `.tmp` → fsync → rename.

use crate::err_msg;
use crate::error::{AppResult, Error, ErrorContext};
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
    /// Source identifier: "built-in", "bundled", or "remote".
    /// Authoritative — determines which binary/source to use.
    /// No more builtin:bool or remote:{...} guessing.
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default = "default_config")]
    pub config: serde_json::Value,
}

fn default_source() -> String {
    "built-in".to_string()
}

/// Describes a git remote source for a plugin installed from a git repository.
///
/// The plugin type (mcp, platform, provider) is inferred from the YAML file section
/// (tools.yml, platforms.yml, providers.yml) — not stored in this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRemote {
    /// The git clone URL (https://... or git@...).
    pub url: String,
    /// Optional git ref: branch name, tag, or commit SHA. Defaults to the repository's HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Optional subdirectory path within the repo where plugin.json lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
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
            PluginYamlType::Platform => "platforms", // section name still "platforms" in unified plugins.yml
            PluginYamlType::Tool => "tools", // section name still "tools" in unified plugins.yml
            PluginYamlType::Provider => "providers", // section name still "providers" in unified plugins.yml
        }
    }

    /// The directory name under plugins/ directory (e.g. "platforms", "tools", "providers").
    pub fn type_dir_name(&self) -> &str {
        match self {
            PluginYamlType::Platform => "platforms",
            PluginYamlType::Tool => "tools", // Was "mcp", now consistently "tools"
            PluginYamlType::Provider => "providers",
        }
    }

    /// The YAML filename (e.g. "platforms.yml", "tools.yml", "providers.yml").
    pub fn yaml_file(&self) -> &str {
        "plugins.yml"
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
            "mcp" => PluginYamlType::Tool, // Map legacy "mcp" to "tool"
            "tool" => PluginYamlType::Tool,
            "provider" => PluginYamlType::Provider,
            _ => PluginYamlType::Tool, // fallback
        }
    }

    /// Return the string representation for the API response (compatible with frontend).
    pub fn to_type_str(&self) -> &str {
        match self {
            PluginYamlType::Platform => "platform",
            PluginYamlType::Tool => "tools", // Was "mcp", now consistently "tools" for API
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
    /// Status is one of "enabled" or "disabled" only.
    /// "duplicated" is NOT a status — use `is_duplicated` flag instead.
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
    /// If populated, the plugin was installed from a git remote (shows clone URL + ref).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<PluginRemote>,
    /// True if the plugin has a remote but has not been cloned yet.
    #[serde(default)]
    pub needs_download: bool,
    /// True when this source is NOT the primary (YAML-configured) one.
    /// Non-primary sources of a duplicate plugin name get this flag.
    /// The original status (enabled/disabled) is preserved unchanged.
    #[serde(default)]
    pub is_duplicated: bool,
    /// True if the plugin has source code (Cargo.toml or plugin.json entrypoint).
    /// False = pre-built binary only, no source to install/reinstall from.
    #[serde(default)]
    pub has_source_code: bool,
    /// True if this is a script-language MCP (plugin.json entrypoint with non-Rust command).
    /// Script MCPs don't need compilation — they run via the configured command directly.
    #[serde(default)]
    pub is_script: bool,
    /// Human-readable explanation when status is "error".
    /// Empty string when status is not "error".
    #[serde(default)]
    pub status_message: String,
    /// Programming language: "Rust", "Python", "Node.js", or "unknown".
    #[serde(default)]
    pub language: String,
}

// ---------------------------------------------------------------------------
// File path helpers
// ---------------------------------------------------------------------------

fn file_path(data_dir: &str, _pt: &PluginYamlType) -> PathBuf {
    // Unified plugins.yml replaces the old per-type files
    PathBuf::from(data_dir).join("plugins.yml")
}

// ---------------------------------------------------------------------------
// Low-level YAML I/O
// ---------------------------------------------------------------------------

/// Load the entire PluginYamlFile (all three sections) from plugins.yml.
pub fn load_all_sections(data_dir: &str) -> AppResult<PluginYamlFile> {
    let path = PathBuf::from(data_dir).join("plugins.yml");
    if !path.exists() {
        return Ok(PluginYamlFile {
            platforms: None,
            tools: None,
            providers: None,
        });
    }
    let content = fs::read_to_string(&path).ctx(format!("Failed to read {}", path.display()))?;
    let file: PluginYamlFile =
        serde_yaml::from_str(&content).ctx(format!("Failed to parse {}", path.display()))?;
    Ok(file)
}

/// Save the entire PluginYamlFile (all three sections) to plugins.yml.
pub fn save_all_sections(data_dir: &str, file: &PluginYamlFile) -> AppResult<()> {
    let path = PathBuf::from(data_dir).join("plugins.yml");
    let tmp_path = path.with_extension("yml.tmp");
    let yaml = serde_yaml::to_string(file).ctx("Failed to serialize plugin YAML")?;
    {
        let mut f =
            fs::File::create(&tmp_path).ctx(format!("Failed to create {}", tmp_path.display()))?;
        f.write_all(yaml.as_bytes())
            .ctx(format!("Failed to write {}", tmp_path.display()))?;
        f.sync_all()
            .ctx(format!("Failed to sync {}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, &path).ctx(format!(
        "Failed to rename {} to {}",
        tmp_path.display(),
        path.display()
    ))?;
    Ok(())
}

/// Load the raw entries map from plugins.yml for a specific section.
pub fn load_raw(
    data_dir: &str,
    pt: &PluginYamlType,
) -> AppResult<BTreeMap<String, PluginYamlEntry>> {
    let file = load_all_sections(data_dir)?;
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
/// Preserves all three sections (platforms, tools, providers) in the unified plugins.yml.
pub fn save_file(
    data_dir: &str,
    pt: &PluginYamlType,
    entries: BTreeMap<String, PluginYamlEntry>,
) -> AppResult<()> {
    let mut file = load_all_sections(data_dir).unwrap_or(PluginYamlFile {
        platforms: None,
        tools: None,
        providers: None,
    });
    match pt.file_name() {
        "platforms" => file.platforms = Some(entries),
        "tools" => file.tools = Some(entries),
        "providers" => file.providers = Some(entries),
        _ => err_msg!("Unknown plugin YAML type: {}", pt.file_name()),
    }
    save_all_sections(data_dir, &file)
}

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
/// Preserves the existing `source` field; for new entries, defaults to `"built-in"`.
pub fn set_entry(
    data_dir: &str,
    pt: &PluginYamlType,
    name: &str,
    enabled: bool,
    config: serde_json::Value,
) -> AppResult<PluginYamlEntry> {
    let mut entries = load_raw(data_dir, pt)?;
    let source = entries
        .get(name)
        .map(|e| e.source.clone())
        .unwrap_or_else(|| "built-in".to_string());
    let entry = PluginYamlEntry {
        enabled,
        source,
        config,
    };
    entries.insert(name.to_string(), entry.clone());
    save_file(data_dir, pt, entries)?;
    Ok(entry)
}

/// Helper: determine if a plugin is built-in by checking if its source lives
/// under the plugin source directory at /app/plugins/ (workspace source dir).
pub fn is_plugin_builtin(_data_dir: &str, name: &str, plugin_type: &PluginYamlType) -> bool {
    let type_dir = plugin_type.type_dir_name();
    let source_dir = format!("/app/plugins/{}/{}", type_dir, name);
    std::path::Path::new(&source_dir)
        .join("Cargo.toml")
        .exists()
        || std::path::Path::new(&source_dir)
            .join("plugin.json")
            .exists()
}

/// Set a plugin entry with an explicit source override.
/// The `source` is one of "built-in", "bundled", or "remote".
pub fn set_entry_with_source(
    data_dir: &str,
    pt: &PluginYamlType,
    name: &str,
    enabled: bool,
    source: &str,
    config: serde_json::Value,
) -> AppResult<PluginYamlEntry> {
    let mut entries = load_raw(data_dir, pt)?;
    let entry = PluginYamlEntry {
        enabled,
        source: source.to_string(),
        config,
    };
    entries.insert(name.to_string(), entry.clone());
    save_file(data_dir, pt, entries)?;
    Ok(entry)
}

/// Get a plugin entry by searching all three YAML types, returning the entry and its type.
pub fn get_entry_with_type(
    data_dir: &str,
    name: &str,
) -> AppResult<Option<(PluginYamlType, PluginYamlEntry)>> {
    for pt in &[
        PluginYamlType::Platform,
        PluginYamlType::Tool,
        PluginYamlType::Provider,
    ] {
        if let Ok(Some(entry)) = get_entry(data_dir, pt, name) {
            return Ok(Some((pt.clone(), entry)));
        }
    }
    Ok(None)
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

// ---------------------------------------------------------------------------
// Remote plugin store (.remote/plugins.yml) — persists remote plugin info
// independently of the main YAML, so switching sources doesn't lose it.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemotePluginStore {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<std::collections::BTreeMap<String, PluginRemote>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<std::collections::BTreeMap<String, PluginRemote>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<std::collections::BTreeMap<String, PluginRemote>>,
}

fn remote_plugins_path(data_dir: &str) -> String {
    format!("{}/remote.yml", data_dir)
}

/// Load the remote plugin store from `.remote/plugins.yml`.
pub fn load_remote_plugins(data_dir: &str) -> RemotePluginStore {
    let path = remote_plugins_path(data_dir);
    let p = std::path::Path::new(&path);
    if p.exists() {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|c| serde_yaml::from_str(&c).ok())
            .unwrap_or_default()
    } else {
        RemotePluginStore::default()
    }
}

/// Save a remote plugin entry to `.remote/plugins.yml`.
pub fn save_remote_plugin(
    data_dir: &str,
    pt: &PluginYamlType,
    name: &str,
    remote: &PluginRemote,
) -> AppResult<()> {
    let mut store = load_remote_plugins(data_dir);
    let entries = match pt {
        PluginYamlType::Tool => store.tools.get_or_insert_with(Default::default),
        PluginYamlType::Platform => store.platforms.get_or_insert_with(Default::default),
        PluginYamlType::Provider => store.providers.get_or_insert_with(Default::default),
    };
    entries.insert(name.to_string(), remote.clone());
    let path = remote_plugins_path(data_dir);
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let content = serde_yaml::to_string(&store)
        .map_err(|e| Error::Message(format!("Failed to serialize remote plugins: {}", e)))?;
    std::fs::write(&path, content)
        .map_err(|e| Error::Message(format!("Failed to write remote plugins: {}", e)))?;
    Ok(())
}

/// Remove a remote plugin entry from `.remote/plugins.yml`.
pub fn remove_remote_plugin(data_dir: &str, pt: &PluginYamlType, name: &str) -> AppResult<()> {
    let mut store = load_remote_plugins(data_dir);
    let entries = match pt {
        PluginYamlType::Tool => &mut store.tools,
        PluginYamlType::Platform => &mut store.platforms,
        PluginYamlType::Provider => &mut store.providers,
    };
    if let Some(ref mut map) = entries {
        map.remove(name);
    }
    let path = remote_plugins_path(data_dir);
    let content = serde_yaml::to_string(&store)
        .map_err(|e| Error::Message(format!("Failed to serialize remote plugins: {}", e)))?;
    std::fs::write(&path, content)
        .map_err(|e| Error::Message(format!("Failed to write remote plugins: {}", e)))?;
    Ok(())
}

/// Get a remote plugin entry from `.remote/plugins.yml`.
pub fn get_remote_plugin(data_dir: &str, pt: &PluginYamlType, name: &str) -> Option<PluginRemote> {
    let store = load_remote_plugins(data_dir);
    let entries = match pt {
        PluginYamlType::Tool => store.tools.as_ref()?,
        PluginYamlType::Platform => store.platforms.as_ref()?,
        PluginYamlType::Provider => store.providers.as_ref()?,
    };
    entries.get(name).cloned()
}

/// Check if a remote plugin entry exists in remote.yml.
pub fn has_remote_entry(data_dir: &str, pt: &PluginYamlType, name: &str) -> bool {
    get_remote_plugin(data_dir, pt, name).is_some()
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
            String::new()
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
/// This function is kept for backward compatibility but should not be used
/// by any new code. All env resolution uses `$env:` syntax instead.
pub fn resolve_legacy_env_vars(value: &str) -> String {
    resolve_legacy_vars(value)
}

/// Resolve `$env:VAR` references in a manifest env value for `build_plugin_detail`.
/// YAML plugin config values should use `resolve_config_value` instead.
fn resolve_env_var(value: &str) -> String {
    if let Some(var_name) = value.strip_prefix("$env:") {
        return std::env::var(var_name).unwrap_or_else(|_| {
            tracing::warn!("Config env ref $env:{} not set", var_name);
            String::new()
        });
    }
    // No ${VAR} support — that syntax is not used anywhere
    value.to_string()
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
            String::new()
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
pub async fn resolve_config_refs(env: &mut HashMap<String, String>, pool: &sqlx::PgPool) {
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
            // e.g., "/opt/omni/plugins/mcp/docker-compose/plugin.json" → directory name
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
    is_duplicated: bool,
) -> PluginDetail {
    let enabled = yaml_entry
        .map(|e| e.enabled)
        // All plugins default to disabled when not present in YAML.
        // They must be explicitly added to YAML to be enabled,
        // regardless of source type (bundled, built-in, remote).
        .unwrap_or(false);
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
        PluginType::Mcp => "tool", // Was "mcp", now consistently "tool" for API
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

    // Compute needs_build: compilable crate with no compiled binary
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
            let cargo_package_name =
                std::fs::read_to_string(&cargo_toml)
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
            // - Package name from Cargo.toml (workspace members with mcp-server- prefix)
            // - get_bin_path() for builtins (binary next to omniagent or in workspace target/release)
            let mut candidates = vec![format!("{}/target/release/{}", dir, dir_name)];
            if let Some(ref pkg) = cargo_package_name {
                candidates.push(format!("{}/target/release/{}", dir, pkg));

                // Also check workspace root target (for builtin:true plugins whose
                // binaries live at /app/target/release/<package> from workspace builds)
                let workspace_root = std::path::Path::new("/app");
                if workspace_root.join("Cargo.toml").exists() {
                    candidates.push(format!(
                        "{}/target/release/{}",
                        workspace_root.to_string_lossy(),
                        pkg
                    ));
                }

                // Check get_bin_path() — resolves binary next to the omniagent executable
                // or at /app/target/release/<name>
                if let Some(bin_path) = crate::mcp::external::config::get_bin_path(pkg) {
                    candidates.push(bin_path);
                }
            }

            !candidates.iter().any(|p| std::path::Path::new(p).exists())
        })
        .unwrap_or(false);

    // Compute has_source_code: Cargo.toml, package.json, pyproject.toml, or source
    // files (.py, .js, .ts) in the plugin directory indicate source code is present.
    // Bare binary names like "mcp-server-cron" or API-mode providers have no source.
    // Remote plugins without explicit entrypoint may still have source files.
    let has_source_code = if manifest.api_mode.is_some() {
        false
    } else {
        plugin_dir
        .map(|dir| {
            let dir_path = std::path::Path::new(dir);
            if dir_path.join("Cargo.toml").exists() {
                return true;
            }
            if dir_path.join("package.json").exists() {
                return true;
            }
            if dir_path.join("pyproject.toml").exists() {
                return true;
            }
            // Check if manifest has a script entrypoint (not a bare binary name)
            if let Some(ep) = manifest.entrypoint.as_ref() {
                if !ep.command.is_empty() {
                    // Check if entrypoint references a file/script in the plugin directory
                    // (e.g., "./target/release/mcp-server-foo" or "./plugin.py").
                    // If the first token contains a path separator, it's a script/path.
                    let first_word = ep.command.split_whitespace().next().unwrap_or("");
                    if first_word.contains('/') || first_word.contains('\\') {
                        return true;
                    }
                    // Bare binary names like "mcp-server-actions" or "python3" are NOT
                    // source code — they're either pre-compiled binaries or script runners.
                    // Known runners are checked by looking at the first word's characteristics:
                    // - Known script runners always have extensions or are well-known names
                    // - Plugin binaries follow the "mcp-server-*" or similar conventions
                    // We return false here — bare binary name = no source code.
                    return false;
                }
            }
            // Check for source files by extension (covers remote plugins without
            // explicit entrypoint in plugin.json — e.g. test-js-tool, test-python-tool)
            if has_source_file_by_extension(dir_path) {
                return true;
            }
            false
        })
        .unwrap_or(false)
    };

    // Compute is_script: has entrypoint with script runner but no Cargo.toml
    // Note: with the match-based check above, is_script can never be true
    // because we return false for all bare entrypoint commands. Plugins with
    // script entrypoints must have path-based commands (e.g., "./plugin.py").
    let is_script = false;

    // Detect programming language
    // API-mode providers (api_mode set) have no language — they're HTTP API based.
    let language = if manifest.api_mode.is_some() {
        String::new()
    } else {
        plugin_dir
        .map(|dir| {
            let dir_path = std::path::Path::new(dir);
            if dir_path.join("Cargo.toml").exists() {
                return "Rust".to_string();
            }
            if dir_path.join("package.json").exists() {
                return "Node.js".to_string();
            }
            if let Some(ep) = manifest.entrypoint.as_ref() {
                let cmd = ep.command.to_lowercase();
                if cmd.contains(".py") || cmd.contains("python") {
                    return "Python".to_string();
                }
                if cmd.contains(".js") || cmd.contains("node ") || cmd.contains("node.") {
                    return "Node.js".to_string();
                }
            }
            "unknown".to_string()
        })
        .unwrap_or_else(|| {
            // No plugin_dir — try manifest entrypoint
            if let Some(ep) = manifest.entrypoint.as_ref() {
                let cmd = ep.command.to_lowercase();
                if cmd.contains(".py") || cmd.contains("python") {
                    return "Python".to_string();
                }
                if cmd.contains(".js") || cmd.contains("node ") || cmd.contains("node.") {
                    return "Node.js".to_string();
                }
                if cmd.contains("mcp-server-") {
                    return "Rust".to_string();
                }
            }
            String::new()
        })
    };

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
        remote: if source == "remote" {
            // For remote plugins, look up the remote info from remote.yml
            let yaml_type = PluginYamlType::from_plugin_type(&manifest.plugin_type);
            get_remote_plugin(data_dir, &yaml_type, &manifest.name)
        } else {
            None
        },
        needs_download: false,
        is_duplicated,
        has_source_code,
        is_script,
        status_message: String::new(),
        language,
    }
}

/// Check if a plugin directory contains source code files by extension.
/// Returns true if any file with a recognized source extension exists
/// (excluding typical build output, .git, and node_modules directories).
fn has_source_file_by_extension(dir_path: &std::path::Path) -> bool {
    let source_extensions = [
        ".py", ".js", ".ts", ".jsx", ".tsx", ".rb", ".go", ".sh",
        ".java", ".kt", ".swift", ".c", ".cpp", ".h", ".hpp",
    ];
    walkdir::WalkDir::new(dir_path)
        .max_depth(3)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .any(|e| {
            let path = e.path();
            // Skip .git, target/, node_modules/, .remote/ directories
            let parent = path.parent().unwrap_or(path);
            let parent_name = parent.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if parent_name == ".git" || parent_name == "target"
                || parent_name == "node_modules" || parent_name == ".remote"
            {
                return false;
            }
            if let Some(ext) = path.extension() {
                let ext_str = format!(".{}", ext.to_string_lossy().to_lowercase());
                source_extensions.contains(&ext_str.as_str())
            } else {
                false
            }
        })
}

// ---------------------------------------------------------------------------
// Discovery + enrichment (combines disk manifests with YAML state)
// ---------------------------------------------------------------------------

/// A single plugin's discovered sources, grouped by key (directory name).
#[derive(Debug, Clone)]
struct PluginSourceGroup {
    key: String,
    sources: Vec<(PluginManifest, String, String)>,
    yaml_type: Option<PluginYamlType>,
    yaml_entry: Option<PluginYamlEntry>,
}

/// Pick the primary source for a plugin based on YAML configuration.
/// Returns the index into `sources` that should be the primary (active) entry,
/// or `None` if no source should be designated as primary (all are equal).
fn pick_primary_source(group: &PluginSourceGroup) -> Option<usize> {
    let yaml_entry = &group.yaml_entry;
    let sources = &group.sources;

    // Helper: find the first source matching one of the given types
    let find_source = |types: &[&str]| -> Option<usize> {
        for t in types {
            for (i, (_, source, _)) in sources.iter().enumerate() {
                if source == t {
                    return Some(i);
                }
            }
        }
        None
    };

    // YAML's `source` field is authoritative.
    // Prefer the source that matches the YAML entry's source value.
    if let Some(entry) = yaml_entry {
        if let Some(idx) = find_source(&[&entry.source]) {
            return Some(idx);
        }
    }

    // If the exact source doesn't exist on disk, try other available sources
    // in priority order: built-in > bundled > remote
    if yaml_entry.is_some() {
        if let Some(idx) = find_source(&["built-in", "bundled", "remote"]) {
            return Some(idx);
        }
    }

    // No YAML entry: do NOT designate any source as primary.
    // All sources are equal — none is "duplicated" over the others.
    // The frontend shows the disabled-card styling and buttons on all of them.
    None
}

/// List all plugins, combining disk discovery with YAML overrides.
/// Groups multiple sources (bundled, built-in, remote) by name.
/// YAML determines which source is primary; others show as "duplicated".
pub fn list_plugins(data_dir: &str) -> AppResult<Vec<PluginDetail>> {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());
    let discovered = crate::plugin::installer::discover_plugins(data_dir, &workspace_dir);

    // Pre-load all YAML entries for efficient lookup
    let platform_entries = load_raw(data_dir, &PluginYamlType::Platform)?;
    let tool_entries = load_raw(data_dir, &PluginYamlType::Tool)?;
    let provider_entries = load_raw(data_dir, &PluginYamlType::Provider)?;

    // Group discovered plugins by key (directory name)
    let mut groups: std::collections::BTreeMap<String, PluginSourceGroup> =
        std::collections::BTreeMap::new();

    for (manifest, source, base_path) in &discovered {
        let raw_key = crate::plugin::installer::extract_plugin_key_from_path(base_path);
        if raw_key.is_empty() {
            continue;
        }
        let yaml_type = PluginYamlType::from_plugin_type(&manifest.plugin_type);

        // For remote sources, resolve key via remote.yml (e.g. "cron-echo" → "cron")
        let key = if *source == "remote" {
            let remote_plugins = load_remote_plugins(data_dir);
            let entries = match &yaml_type {
                PluginYamlType::Tool => remote_plugins.tools.as_ref(),
                PluginYamlType::Platform => remote_plugins.platforms.as_ref(),
                PluginYamlType::Provider => remote_plugins.providers.as_ref(),
            };
            if let Some(entries) = entries {
                entries
                    .iter()
                    .find_map(|(repo_name, info)| {
                        let subpath = info.path.as_deref().unwrap_or("");
                        if (!subpath.is_empty()
                            && base_path.ends_with(&format!("/{}/plugin.json", subpath)))
                            || base_path.contains(&format!("/.remote/{}/", repo_name))
                        {
                            Some(repo_name.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or(raw_key.clone())
            } else {
                raw_key.clone()
            }
        } else {
            raw_key.clone()
        };

        let yaml_entry = match &yaml_type {
            PluginYamlType::Platform => platform_entries.get(&key),
            PluginYamlType::Tool => tool_entries.get(&key),
            PluginYamlType::Provider => provider_entries.get(&key),
        };

        // Remote sources show if YAML entry has source: remote,
        // OR if remote.yml has an entry for this plugin.
        if source == "remote" {
            let has_source = yaml_entry
                .map(|e| e.source.as_str() == "remote")
                .unwrap_or(false);
            let has_store = get_remote_plugin(data_dir, &yaml_type, &key).is_some();
            if !has_source && !has_store {
                continue;
            }
        }

        let entry = groups
            .entry(key.clone())
            .or_insert_with(|| PluginSourceGroup {
                key: key.clone(),
                sources: Vec::new(),
                yaml_type: None,
                yaml_entry: None,
            });
        entry
            .sources
            .push((manifest.clone(), source.clone(), base_path.clone()));
        // Only set YAML info on first insertion (all sources share the same YAML)
        if entry.yaml_type.is_none() {
            entry.yaml_type = Some(yaml_type);
            entry.yaml_entry = yaml_entry.cloned();
        }
    }

    // After standard discovery + YAML remote path, check YAML entries for remote.path subdirectories.
    // A remote plugin at .remote/<name>/ with remote.path: "tools/<name>" has its
    // plugin.json at .remote/<name>/tools/<name>/plugin.json — not at the root level
    // that remote.yml-driven discovery covers. Use the YAML remote.path to construct
    // the exact path and discover these deterministicly.
    for (yaml_type, yaml_entries) in &[
        (PluginYamlType::Platform, &platform_entries),
        (PluginYamlType::Tool, &tool_entries),
        (PluginYamlType::Provider, &provider_entries),
    ] {
        for (name, entry) in *yaml_entries {
            if let Some(ref remote) = get_remote_plugin(data_dir, yaml_type, name) {
                if let Some(ref remote_path) = remote.path {
                    let type_dir = yaml_type.type_dir_name();
                    let manifest_path = format!(
                        "{}/plugins/{}/.remote/{}/{}/plugin.json",
                        data_dir, type_dir, name, remote_path
                    );
                    if !std::path::Path::new(&manifest_path).exists() {
                        continue;
                    }
                    // Check if a remote source is already in this group
                    if groups.contains_key(name) {
                        let has_remote = groups[name].sources.iter().any(|(_, s, _)| s == "remote");
                        if has_remote {
                            continue;
                        }
                    }
                    if let Ok(manifest) = crate::plugin::load_manifest(&manifest_path) {
                        let base_path = manifest_path.to_string();
                        let key = name.clone();
                        // Add to existing group or create new one
                        if let Some(group) = groups.get_mut(name) {
                            group
                                .sources
                                .push((manifest, "remote".to_string(), base_path));
                        } else {
                            let sources = vec![(manifest, "remote".to_string(), base_path)];
                            groups.insert(
                                key,
                                PluginSourceGroup {
                                    key: name.clone(),
                                    sources,
                                    yaml_type: Some(yaml_type.clone()),
                                    yaml_entry: Some(entry.clone()),
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    let mut results: Vec<PluginDetail> = Vec::new();

    for (key, group) in &groups {
        let primary_idx = pick_primary_source(group);

        let _yaml_type = group.yaml_type.as_ref().unwrap_or(&PluginYamlType::Tool);
        let yaml_entry_ref = group.yaml_entry.as_ref();

        for (i, (manifest, source, base_path)) in group.sources.iter().enumerate() {
            let is_primary = primary_idx.map(|idx| i == idx);
            // When primary_idx is None (no YAML entry, none enabled), no source is a "duplicate"

            // Skip sources that match a disabled YAML entry — user has explicitly
            // removed them via the Remove action (wrote enabled: false to plugins.yml).
            // Suppress all sources regardless of source type mismatch — the plugin
            // name is the authority, and enabled: false means "don't show this plugin".
            if let Some(entry) = yaml_entry_ref {
                if !entry.enabled {
                    continue;
                }
            }

            // Plugin directory is the parent of the base_path
            let plugin_dir = std::path::Path::new(base_path)
                .parent()
                .and_then(|p| p.to_str());

            // For the primary source, pass the real YAML entry; for duplicates, pass None
            // to trigger default (disabled for built-in, and forced "duplicated" status)
            let detail_yaml_entry = if is_primary.unwrap_or(true) { yaml_entry_ref } else { None };

            let detail = build_plugin_detail(
                manifest,
                source,
                detail_yaml_entry,
                Some(key),
                plugin_dir,
                data_dir,
                is_primary.map(|p| !p).unwrap_or(group.sources.len() > 1),
            );

            // is_duplicated is now set via the build_plugin_detail parameter,
            // NOT by overriding the status. The original status (enabled/disabled)
            // is preserved for all sources — the frontend uses is_duplicated
            // to display the duplicate label separately.

            results.push(detail);
        }
    } // end of groups loop

    // ── "Not found" entries: YAML entries with no discovered source on disk ──
    // Check all YAML entries and add synthetic entries for those without matching groups.
    for (yaml_type, entries) in &[
        (PluginYamlType::Platform, &platform_entries),
        (PluginYamlType::Tool, &tool_entries),
        (PluginYamlType::Provider, &provider_entries),
    ] {
        for (key, yaml_entry) in *entries {
            if !groups.contains_key(key) {
                let is_remote = yaml_entry.source == "remote";
                let manifest = PluginManifest {
                    name: key.clone(),
                    version: "0.1.0".to_string(),
                    plugin_type: match yaml_type {
                        PluginYamlType::Platform => PluginType::Platform,
                        PluginYamlType::Tool => PluginType::Mcp,
                        PluginYamlType::Provider => PluginType::Provider,
                    },
                    description: Some(if is_remote {
                        "Remote plugin — not downloaded yet".to_string()
                    } else {
                        "Plugin source not found on disk".to_string()
                    }),
                    entrypoint: None,
                    capabilities: None,
                    config_schema: Vec::new(),
                    env: std::collections::HashMap::new(),
                    default_base_url: None,
                    api_mode: None,
                    api_modes: None,
                };
                let source = if is_remote { "remote" } else { "bundled" };
                let mut detail = build_plugin_detail(
                    &manifest,
                    source,
                    Some(yaml_entry),
                    Some(key),
                    None, // no plugin_dir
                    data_dir,
                    false,
                );
                detail.status = "not_found".to_string();
                detail.needs_download = is_remote;
                detail.has_source_code = false;
                detail.needs_build = false;
                // For remote plugins not yet cloned, set needs_download on the source
                if is_remote {
                    detail.source = Some("remote".to_string());
                }
                results.push(detail);
            }
        }
    }

    // Sort by name for deterministic ordering
    results.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(results)
}

/// Get a single plugin by name, combining disk discovery with YAML state.
/// Returns the PRIMARY source for that plugin name based on YAML configuration,
/// unlike list_plugins which returns all sources with is_duplicated flags.
pub fn get_plugin(data_dir: &str, name: &str) -> AppResult<Option<PluginDetail>> {
    let workspace_dir =
        std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());
    let discovered = crate::plugin::installer::discover_plugins(data_dir, &workspace_dir);

    // Pre-load YAML entries
    let platform_entries = load_raw(data_dir, &PluginYamlType::Platform)?;
    let tool_entries = load_raw(data_dir, &PluginYamlType::Tool)?;
    let provider_entries = load_raw(data_dir, &PluginYamlType::Provider)?;

    // Group by key (same logic as list_plugins)
    let mut groups: std::collections::BTreeMap<String, PluginSourceGroup> =
        std::collections::BTreeMap::new();

    for (manifest, source, base_path) in &discovered {
        let raw_key = crate::plugin::installer::extract_plugin_key_from_path(base_path);
        if raw_key.is_empty() {
            continue;
        }
        let yaml_type = PluginYamlType::from_plugin_type(&manifest.plugin_type);

        // For remote sources, the key extracted from the path may be a subdirectory
        // (e.g. "cron-echo") while the remote.yml key is the repo name ("cron").
        // Resolve to the correct key so the group merges properly.
        let key = if *source == "remote" {
            let remote_plugins = load_remote_plugins(data_dir);
            let entries = match &yaml_type {
                PluginYamlType::Tool => remote_plugins.tools.as_ref(),
                PluginYamlType::Platform => remote_plugins.platforms.as_ref(),
                PluginYamlType::Provider => remote_plugins.providers.as_ref(),
            };
            if let Some(entries) = entries {
                entries
                    .iter()
                    .find_map(|(repo_name, info)| {
                        let subpath = info.path.as_deref().unwrap_or("");
                        if (!subpath.is_empty()
                            && base_path.ends_with(&format!("/{}/plugin.json", subpath)))
                            || base_path.contains(&format!("/.remote/{}/", repo_name))
                        {
                            Some(repo_name.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or(raw_key.clone())
            } else {
                raw_key.clone()
            }
        } else {
            raw_key.clone()
        };

        let yaml_entry = match &yaml_type {
            PluginYamlType::Platform => platform_entries.get(&key),
            PluginYamlType::Tool => tool_entries.get(&key),
            PluginYamlType::Provider => provider_entries.get(&key),
        };

        // Remote sources show if YAML entry has source: remote,
        // OR if remote.yml has an entry for this plugin.
        if source == "remote" {
            let has_source = yaml_entry
                .map(|e| e.source.as_str() == "remote")
                .unwrap_or(false);
            let has_store = get_remote_plugin(data_dir, &yaml_type, &key).is_some();
            if !has_source && !has_store {
                continue;
            }
        }

        let entry = groups
            .entry(key.clone())
            .or_insert_with(|| PluginSourceGroup {
                key: key.clone(),
                sources: Vec::new(),
                yaml_type: None,
                yaml_entry: None,
            });
        entry
            .sources
            .push((manifest.clone(), source.clone(), base_path.clone()));
        if entry.yaml_type.is_none() {
            entry.yaml_type = Some(yaml_type);
            entry.yaml_entry = yaml_entry.cloned();
        }
    }

    // Find ALL groups matching the requested name and merge their sources.
    // A plugin name may span multiple groups when remote sources have subpath keys
    // (e.g., "cron" group + "cron-echo" group both match name "cron").
    let mut merged_group: Option<PluginSourceGroup> = None;
    for (key, group) in &groups {
        let matches = key == name || group.sources.iter().any(|(m, _, _)| m.name == name);
        if !matches {
            continue;
        }
        if let Some(ref mut mg) = merged_group {
            for (m, s, bp) in &group.sources {
                mg.sources.push((m.clone(), s.clone(), bp.clone()));
            }
            // Prefer the group that has a YAML entry
            if mg.yaml_entry.is_none() && group.yaml_entry.is_some() {
                mg.yaml_type = group.yaml_type.clone();
                mg.yaml_entry = group.yaml_entry.clone();
            }
        } else {
            merged_group = Some(group.clone());
        }
    }

    if let Some(group) = merged_group {
        let primary_idx = pick_primary_source(&group).unwrap_or(0);
        let (manifest, source, base_path) = &group.sources[primary_idx];
        let _yaml_type = group.yaml_type.as_ref().unwrap_or(&PluginYamlType::Tool);
        let yaml_entry = group.yaml_entry.as_ref();
        let plugin_dir = std::path::Path::new(base_path)
            .parent()
            .and_then(|p| p.to_str());

        return Ok(Some(build_plugin_detail(
            manifest,
            source,
            yaml_entry,
            Some(&group.key),
            plugin_dir,
            data_dir,
            false, // get_plugin always returns the primary (is_duplicated=false)
        )));
    }

    // Not found via discovery — check YAML entries directly (YAML-only entry with no disk source)
    if let Some(detail) = build_not_found_from_yaml(
        data_dir,
        name,
        &platform_entries,
        &tool_entries,
        &provider_entries,
    ) {
        return Ok(Some(detail));
    }

    Ok(None)
}

/// Build a synthetic "not found" PluginDetail from YAML entries if the plugin name exists in any YAML file.
fn build_not_found_from_yaml(
    data_dir: &str,
    name: &str,
    platform_entries: &BTreeMap<String, PluginYamlEntry>,
    tool_entries: &BTreeMap<String, PluginYamlEntry>,
    provider_entries: &BTreeMap<String, PluginYamlEntry>,
) -> Option<PluginDetail> {
    for (yaml_type, entries) in &[
        (PluginYamlType::Platform, platform_entries),
        (PluginYamlType::Tool, tool_entries),
        (PluginYamlType::Provider, provider_entries),
    ] {
        if let Some(yaml_entry) = entries.get(name) {
            let is_remote = yaml_entry.source == "remote";
            let manifest = PluginManifest {
                name: name.to_string(),
                version: "0.1.0".to_string(),
                plugin_type: match yaml_type {
                    PluginYamlType::Platform => PluginType::Platform,
                    PluginYamlType::Tool => PluginType::Mcp,
                    PluginYamlType::Provider => PluginType::Provider,
                },
                description: Some(if is_remote {
                    "Remote plugin — not downloaded yet".to_string()
                } else {
                    "Plugin source not found on disk".to_string()
                }),
                entrypoint: None,
                capabilities: None,
                config_schema: Vec::new(),
                env: std::collections::HashMap::new(),
                default_base_url: None,
                api_mode: None,
                api_modes: None,
            };
            let source = if is_remote { "remote" } else { "bundled" };
            let mut detail = build_plugin_detail(
                &manifest,
                source,
                Some(yaml_entry),
                Some(name),
                None,
                data_dir,
                false,
            );
            detail.status = "not_found".to_string();
            detail.needs_download = is_remote;
            detail.has_source_code = false;
            detail.needs_build = false;
            if is_remote {
                detail.source = Some("remote".to_string());
            }
            return Some(detail);
        }
    }
    None
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
    for (manifest, source, base_path) in &discovered {
        let key = extract_plugin_key(manifest, source, base_path);
        if key == name || manifest.name == name {
            let type_str = match manifest.plugin_type {
                PluginType::Platform => "platform",
                PluginType::Mcp => "mcp",
                PluginType::Provider => "provider",
            };
            return Ok(Some(type_str.to_string()));
        }
    }

    // Fallback: check YAML entries for remote.path — the plugin may exist at
    // .remote/<name>/{path}/plugin.json which may not be in the root-level scan.
    for (yaml_type, entries) in &[
        (
            PluginYamlType::Platform,
            load_raw(data_dir, &PluginYamlType::Platform)?,
        ),
        (
            PluginYamlType::Tool,
            load_raw(data_dir, &PluginYamlType::Tool)?,
        ),
        (
            PluginYamlType::Provider,
            load_raw(data_dir, &PluginYamlType::Provider)?,
        ),
    ] {
        if let Some(_entry) = entries.get(name) {
            if let Some(ref remote) = get_remote_plugin(data_dir, yaml_type, name) {
                if let Some(ref remote_path) = remote.path {
                    let type_dir = yaml_type.type_dir_name();
                    let manifest_path = format!(
                        "{}/plugins/{}/.remote/{}/{}/plugin.json",
                        data_dir, type_dir, name, remote_path
                    );
                    if std::path::Path::new(&manifest_path).exists() {
                        let type_str = match yaml_type {
                            PluginYamlType::Platform => "platform",
                            PluginYamlType::Tool => "mcp",
                            PluginYamlType::Provider => "provider",
                        };
                        return Ok(Some(type_str.to_string()));
                    }
                }
            }
        }
    }

    // Second fallback: check remote.yml directly (no YAML entry needed)
    // for plugins that have been cloned but not yet registered in plugins.yml.
    for yaml_type in &[
        PluginYamlType::Tool,
        PluginYamlType::Platform,
        PluginYamlType::Provider,
    ] {
        if let Some(remote) = get_remote_plugin(data_dir, yaml_type, name) {
            if let Some(ref remote_path) = remote.path {
                let type_dir = yaml_type.type_dir_name();
                let manifest_path = format!(
                    "{}/plugins/{}/.remote/{}/{}/plugin.json",
                    data_dir, type_dir, name, remote_path
                );
                if std::path::Path::new(&manifest_path).exists() {
                    let type_str = match yaml_type {
                        PluginYamlType::Platform => "platform",
                        PluginYamlType::Tool => "mcp",
                        PluginYamlType::Provider => "provider",
                    };
                    return Ok(Some(type_str.to_string()));
                }
            }
            // Also check for plugin.json at .remote/<name>/ root (1 level)
            let type_dir = yaml_type.type_dir_name();
            let manifest_path = format!(
                "{}/plugins/{}/.remote/{}/plugin.json",
                data_dir, type_dir, name
            );
            if std::path::Path::new(&manifest_path).exists() {
                let type_str = match yaml_type {
                    PluginYamlType::Platform => "platform",
                    PluginYamlType::Tool => "mcp",
                    PluginYamlType::Provider => "provider",
                };
                return Ok(Some(type_str.to_string()));
            }
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
    let resp = req.send().await.ctx(format!("Failed to fetch {}", url))?;
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
        let api_key = detail
            .config
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
/// Load the raw YAML config value for a specific plugin from platforms/tools/providers YAML.
/// Returns the `config` object (a JSON Value) if found.
pub fn load_plugin_yaml_config(
    plugin_name: &str,
    data_dir: &str,
    yaml_type: &PluginYamlType,
) -> Option<serde_json::Value> {
    let yaml_path = PathBuf::from(data_dir).join(yaml_type.yaml_file());

    // Load the YAML file and find this plugin's config
    (|| -> Option<serde_json::Value> {
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
    })()
}

/// Merge YAML plugin config values into the env map with PREFIXED keys
/// (e.g. "access_token" from YAML becomes "MYTOOL_ACCESS_TOKEN" in env).
///
/// This is used by MCP tool servers and provider plugins that need env vars
/// with prefixed names. For platform plugins, use `load_plugin_yaml_config`
/// directly — the `config` field (original keys) is sent as configure params.
pub fn merge_yaml_config_into_env(
    env: &mut HashMap<String, String>,
    plugin_name: &str,
    data_dir: &str,
    yaml_type: &PluginYamlType,
) {
    let prefix = plugin_name.to_uppercase().replace('-', "_");

    let config = load_plugin_yaml_config(plugin_name, data_dir, yaml_type);
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
        assert_eq!(
            env.get("MATTERMOST_SERVER_URL").unwrap(),
            "https://mm.example.com"
        );
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
        assert_eq!(
            env.get("MY_TOOL_API_URL").unwrap(),
            "https://api.example.com/v1"
        );
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
        assert_eq!(
            env.get("DEEPSEEK_DEFAULT_MODEL").unwrap(),
            "deepseek-v4-flash"
        );
        assert_eq!(
            env.get("DEEPSEEK_API_BASE").unwrap(),
            "https://api.deepseek.com"
        );
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
