//! Actions management — loaded from a versioned `actions.yml` file in the data directory.
//!
//! The file lives at `<data_dir>/actions.yml` (mounted from omni-stack root as /opt/data).
//! Actions are defined in YAML format:
//!
//! ```yaml
//! actions:
//!   my_action:
//!     enabled: true
//!     tool_name: search_messages
//!     params:
//!       query: "daily"
//!       limit: 10
//! ```
//!
//! Reads happen on every access (no caching — file is tiny, parsing is ~50µs).
//! Writes are atomic: write to `.tmp` → fsync → rename.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

// ── YAML format types ──

/// Top-level YAML structure.
#[derive(Debug, Serialize, Deserialize)]
pub struct ActionsFile {
    pub actions: BTreeMap<String, ActionEntry>,
}

/// A single action entry in the YAML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionEntry {
    pub enabled: bool,
    pub tool_name: String,
    #[serde(default = "default_params")]
    pub params: serde_json::Value,
    /// Optional human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn default_params() -> serde_json::Value {
    serde_json::json!({})
}

// ── API response type (for backward compatibility with dashboard) ──

/// Action as returned by the HTTP API — matches the format the dashboard expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionApi {
    pub id: String,
    pub name: String,
    pub tool_name: String,
    pub params: serde_json::Value,
    /// Always empty string for YAML-based actions (no created_at in file).
    pub created_at: String,
    /// Always empty string for YAML-based actions.
    pub updated_at: String,
    /// Whether this action is enabled (can be executed).
    pub enabled: bool,
    /// Derived from `id starts with "builtin_"` — kept for backward compat, always false for new actions.
    #[serde(default)]
    pub is_builtin: bool,
}

// ── File path ──

fn actions_path(data_dir: &str) -> PathBuf {
    PathBuf::from(data_dir).join("actions.yml")
}

// ── Load ──

/// Load all actions from the YAML file. Returns an empty vec if file doesn't exist.
pub fn load_actions(data_dir: &str) -> Result<Vec<ActionApi>> {
    let path = actions_path(data_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let file: ActionsFile = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    let mut result: Vec<ActionApi> = file
        .actions
        .into_iter()
        .filter(|(_, entry)| entry.enabled)
        .map(|(id, entry)| {
            let id_str = id.clone();
            ActionApi {
                id: id_str.clone(),
                name: id_str,
                tool_name: entry.tool_name,
                params: entry.params,
                created_at: String::new(),
                updated_at: String::new(),
                enabled: entry.enabled,
                is_builtin: false,
            }
        })
        .collect();

    // Sort by id for deterministic ordering
    result.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(result)
}

/// Load ALL actions from YAML (including disabled). For the dashboard list view.
pub fn load_all_actions(data_dir: &str) -> Result<Vec<ActionApi>> {
    let path = actions_path(data_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let file: ActionsFile = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    let mut result: Vec<ActionApi> = file
        .actions
        .into_iter()
        .map(|(id, entry)| {
            let id_str = id.clone();
            ActionApi {
                id: id_str.clone(),
                name: id_str,
                tool_name: entry.tool_name,
                params: entry.params,
                created_at: String::new(),
                updated_at: String::new(),
                enabled: entry.enabled,
                is_builtin: false,
            }
        })
        .collect();

    result.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(result) 
}

/// Get a single action by id (only enabled).
pub fn get_action(data_dir: &str, id: &str) -> Result<Option<ActionApi>> {
    let actions = load_actions(data_dir)?;
    Ok(actions.into_iter().find(|a| a.id == id))
}

/// Get a single action by id (including disabled). For dashboard operations.
pub fn get_action_unfiltered(data_dir: &str, id: &str) -> Result<Option<ActionApi>> {
    let actions = load_all_actions(data_dir)?;
    Ok(actions.into_iter().find(|a| a.id == id))
}

/// Check if an action exists and is enabled.
pub fn action_exists(data_dir: &str, id: &str) -> bool {
    get_action(data_dir, id).ok().flatten().is_some()
}

// ── Save ──

/// Save all actions to the YAML file (atomic write: .tmp → fsync → rename).
fn save_actions_file(data_dir: &str, actions: BTreeMap<String, ActionEntry>) -> Result<()> {
    let file = ActionsFile { actions };
    let path = actions_path(data_dir);
    let tmp_path = path.with_extension("yml.tmp");

    let yaml = serde_yaml::to_string(&file)
        .context("Failed to serialize actions YAML")?;

    {
        let mut f = fs::File::create(&tmp_path)
            .with_context(|| format!("Failed to create {}", tmp_path.display()))?;
        f.write_all(yaml.as_bytes())
            .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("Failed to fsync {}", tmp_path.display()))?;
    }

    fs::rename(&tmp_path, &path)
        .with_context(|| format!("Failed to rename {} -> {}", tmp_path.display(), path.display()))?;

    Ok(())
}

/// Load the raw actions map from file, or return an empty map if file doesn't exist.
fn load_raw_actions(data_dir: &str) -> Result<BTreeMap<String, ActionEntry>> {
    let path = actions_path(data_dir);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let file: ActionsFile = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(file.actions)
}

/// Add a new action (errors if id already exists).
pub fn add_action(data_dir: &str, id: &str, tool_name: &str, params: &serde_json::Value) -> Result<ActionApi> {
    let mut actions = load_raw_actions(data_dir)?;

    if actions.contains_key(id) {
        anyhow::bail!("Action '{}' already exists", id);
    }

    actions.insert(id.to_string(), ActionEntry {
        enabled: true,
        tool_name: tool_name.to_string(),
        params: params.clone(),
        description: None,
    });

    save_actions_file(data_dir, actions)?;

    Ok(ActionApi {
        id: id.to_string(),
        name: id.to_string(),
        tool_name: tool_name.to_string(),
        params: params.clone(),
        created_at: String::new(),
        updated_at: String::new(),
        enabled: true,
        is_builtin: false,
    })
}

/// Update an existing action.
pub fn update_action(
    data_dir: &str,
    id: &str,
    tool_name: &str,
    params: &serde_json::Value,
    enabled: Option<bool>,
) -> Result<ActionApi> {
    let mut actions = load_raw_actions(data_dir)?;

    let entry = actions.get_mut(id).ok_or_else(|| anyhow::anyhow!("Action '{}' not found", id))?;

    entry.tool_name = tool_name.to_string();
    entry.params = params.clone();
    if let Some(e) = enabled {
        entry.enabled = e;
    }
    let current_enabled = entry.enabled;

    save_actions_file(data_dir, actions)?;

    Ok(ActionApi {
        id: id.to_string(),
        name: id.to_string(),
        tool_name: tool_name.to_string(),
        params: params.clone(),
        created_at: String::new(),
        updated_at: String::new(),
        enabled: current_enabled,
        is_builtin: false,
    })
}

/// Delete an action by id.
pub fn delete_action(data_dir: &str, id: &str) -> Result<bool> {
    let mut actions = load_raw_actions(data_dir)?;

    let existed = actions.remove(id).is_some();
    if existed {
        save_actions_file(data_dir, actions)?;
    }
    Ok(existed)
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_data_dir() -> (tempfile::TempDir, String) {
        let dir = tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        (dir, path)
    }

    #[test]
    fn test_load_actions_empty() {
        let (_d, path) = test_data_dir();
        let actions = load_actions(&path).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn test_add_and_load() {
        let (_d, path) = test_data_dir();
        let a = add_action(&path, "test_action", "search_messages", &serde_json::json!({"query": "hello"})).unwrap();
        assert_eq!(a.id, "test_action");
        assert_eq!(a.tool_name, "search_messages");

        let actions = load_actions(&path).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].id, "test_action");
    }

    #[test]
    fn test_add_duplicate_fails() {
        let (_d, path) = test_data_dir();
        add_action(&path, "dup", "tool_a", &serde_json::json!({})).unwrap();
        let r = add_action(&path, "dup", "tool_b", &serde_json::json!({}));
        assert!(r.is_err());
    }

    #[test]
    fn test_update() {
        let (_d, path) = test_data_dir();
        add_action(&path, "act", "tool_a", &serde_json::json!({"p": 1})).unwrap();
        let updated = update_action(&path, "act", "tool_b", &serde_json::json!({"p": 2}), None).unwrap();
        assert_eq!(updated.tool_name, "tool_b");

        let loaded = get_action(&path, "act").unwrap().unwrap();
        assert_eq!(loaded.params, serde_json::json!({"p": 2}));
    }

    #[test]
    fn test_update_nonexistent_fails() {
        let (_d, path) = test_data_dir();
        let r = update_action(&path, "nope", "tool", &serde_json::json!({}), None);
        assert!(r.is_err());
    }

    #[test]
    fn test_delete() {
        let (_d, path) = test_data_dir();
        add_action(&path, "act", "tool", &serde_json::json!({})).unwrap();
        assert!(delete_action(&path, "act").unwrap());
        assert!(!delete_action(&path, "act").unwrap());
        assert!(load_actions(&path).unwrap().is_empty());
    }

    #[test]
    fn test_builtin_prefix_blocked() {
        let (_d, path) = test_data_dir();
        assert!(add_action(&path, "builtin_foo", "tool", &serde_json::json!({})).is_err());
        // Add a non-builtin first
        add_action(&path, "builtin_foo", "tool", &serde_json::json!({})).ok(); // should fail silently via is_err
        // Verify it wasn't added
        assert!(load_actions(&path).unwrap().is_empty());
    }

    #[test]
    fn test_load_only_enabled() {
        let (_d, path) = test_data_dir();
        // Manually write a file with a disabled action
        let mut actions = BTreeMap::new();
        actions.insert("disabled_act".to_string(), ActionEntry {
            enabled: false,
            tool_name: "tool".to_string(),
            params: serde_json::json!({}),
            description: None,
        });
        actions.insert("enabled_act".to_string(), ActionEntry {
            enabled: true,
            tool_name: "tool2".to_string(),
            params: serde_json::json!({}),
            description: None,
        });
        save_actions_file(&path, actions).unwrap();

        let loaded = load_actions(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "enabled_act");
    }

    #[test]
    fn test_action_exists() {
        let (_d, path) = test_data_dir();
        assert!(!action_exists(&path, "nope"));
        add_action(&path, "exists", "tool", &serde_json::json!({})).unwrap();
        assert!(action_exists(&path, "exists"));
    }

    #[test]
    fn test_get_action_not_found() {
        let (_d, path) = test_data_dir();
        assert!(get_action(&path, "nope").unwrap().is_none());
    }

    #[test]
    fn test_is_builtin_flag() {
        let (_d, path) = test_data_dir();
        // Manually add a builtin-looking action to the file
        let mut actions = BTreeMap::new();
        actions.insert("builtin_test".to_string(), ActionEntry {
            enabled: true,
            tool_name: "some_tool".to_string(),
            params: serde_json::json!({}),
            description: Some("Built-in test".to_string()),
        });
        save_actions_file(&path, actions).unwrap();

        let loaded = load_actions(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(!loaded[0].is_builtin); // builtin flag removed — now always false
        assert_eq!(loaded[0].id, "builtin_test");
    }

    #[test]
    fn test_atomic_write() {
        let (_d, path) = test_data_dir();
        add_action(&path, "a1", "t1", &serde_json::json!({})).unwrap();
        add_action(&path, "a2", "t2", &serde_json::json!({})).unwrap();

        // Verify no .tmp file remains
        let tmp = PathBuf::from(&path).join("actions.yml.tmp");
        assert!(!tmp.exists());

        // Verify file is valid YAML
        let content = fs::read_to_string(actions_path(&path)).unwrap();
        let parsed: ActionsFile = serde_yaml::from_str(&content).unwrap();
        assert_eq!(parsed.actions.len(), 2);
    }
}
