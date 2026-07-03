use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// A profile defines the model, provider, data paths, and allowed tools
/// for a given context (channel or direct prompt).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// Default model for this profile (e.g. "deepseek-v4-flash")
    pub model: Option<String>,
    /// Default provider for this profile (e.g. "opencode-go")
    pub provider: Option<String>,
    /// Base API URL override for this profile
    pub base_url: Option<String>,
    /// API key override for this profile
    pub api_key: Option<String>,
    /// Max tokens for this profile
    pub max_tokens: Option<u32>,
    /// Temperature for this profile
    pub temperature: Option<f32>,
    /// List of allowed MCP tool names for this profile
    pub allowed_tools: Vec<String>,
    /// Whether automatic retrieval is enabled for this profile
    pub auto_retrieval_enabled: bool,
    /// Retrieval aggressiveness: 0=off, 1=conservative, 2=balanced, 3=aggressive
    pub retrieval_aggressiveness: u8,
    /// Whether grounding is required for answers
    pub grounding_required: bool,
    /// Context budget for the ContextBuilder (in characters).
    /// If None, falls back to PROMPT_BUDGET_DEFAULT (15,000).
    pub prompt_budget: Option<usize>,
}

/// Default context budget for profiles that don't specify one.
pub const PROMPT_BUDGET_DEFAULT: usize = 15_000;

/// The list of core native tools — used for multi-select in the dashboard.
/// External tools (MCP plugins installed via plugin_registry) are loaded
/// dynamically at runtime and are not listed here.
pub const CORE_TOOLS: &[&str] = &[
    "cron_create-job",
    "cron_list-cron-jobs",
    "cron_delete-cron-job",
    "cron_update-cron-job",
    "kanban_create-task",
    "kanban_list-kanban-tasks",
    "kanban_update-task",
    "kanban_delete-task",
    "kanban_add-dependency",
    "kanban_remove-dependency",
    "plugin_manager",
    "fetch",
    "filesystem_read",
    "filesystem_write",
    "filesystem_list",
    "filesystem_search",
    "filesystem_info",
    "search_messages",
    "search_wiki",
    "git_create-repo",
    "git_clone-repo",
    "git_commit-push",
    "git_status",
    "query_database",
    "docker_compose",
    "skills_create-skill",
    "metrics_get-metrics",
    "subtasks_add-subtask",
    "subtasks_list-subtasks",
    "subtasks_update-subtask",
    "subtasks_delete-subtask",
    "subtasks_get-subtask-counts",
    "hindsight_recall",
    "hindsight_retain",
    "hindsight_reflect",
    "actions_kanban-dispatcher",
    "actions_relevance-indexer",
    "actions_hindsight-populator",
    "actions_setup-knowledge-pipeline",
    "test-python-tool_wait",
    "test-python-tool_echo",
    "test-python-tool_test-error",
    "test-python-tool_save-datetime",
    "test-rust-tool_wait",
    "test-rust-tool_echo",
    "test-rust-tool_test-error",
    "test-rust-tool_save-datetime",
    "test-js-tool_wait",
    "test-js-tool_echo",
    "test-js-tool_test-error",
    "test-js-tool_save-datetime",
];

/// Schema for profiles/<name>/config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
}

impl Profile {
    /// Create a default profile with the given name.
    pub fn default(name: &str) -> Self {
        Self {
            name: name.to_string(),
            model: Some("deepseek-v4-flash".to_string()),
            provider: Some("deepseek".to_string()),
            base_url: None,
            api_key: None,
            max_tokens: None,
            temperature: None,
            allowed_tools: CORE_TOOLS.iter().map(|s| s.to_string()).collect(),
            auto_retrieval_enabled: true,
            retrieval_aggressiveness: 2,
            grounding_required: false,
            prompt_budget: None, // uses PROMPT_BUDGET_DEFAULT (15,000)
        }
    }

    /// Load a profile config from `<data_dir>/profiles/<name>/config.json`.
    /// Returns None if the file doesn't exist or can't be read.
    pub fn load_config(data_dir: &str, name: &str) -> Option<ProfileConfig> {
        let path: PathBuf = [data_dir, "profiles", name, "config.json"].iter().collect();
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Apply a ProfileConfig on top of the default — fields from config override defaults.
    pub fn with_config(mut self, config: ProfileConfig) -> Self {
        if let Some(p) = config.provider {
            self.provider = Some(p);
        }
        if let Some(m) = config.model {
            self.model = Some(m);
        }
        if let Some(tools) = config.allowed_tools {
            self.allowed_tools = tools;
        }
        self
    }

    /// Resolve the effective model, checking channel override first, then profile.
    #[allow(dead_code)]
    pub fn resolve_model(&self, channel_model: Option<&str>) -> Option<String> {
        channel_model
            .map(|s| s.to_string())
            .or_else(|| self.model.clone())
    }

    /// Resolve the effective provider.
    #[allow(dead_code)]
    pub fn resolve_provider(&self, channel_provider: Option<&str>) -> Option<String> {
        channel_provider
            .map(|s| s.to_string())
            .or_else(|| self.provider.clone())
    }
}

/// Read the default profile name from the DEFAULT_PROFILE env var, falling back to "default".
pub fn default_profile_name() -> String {
    std::env::var("DEFAULT_PROFILE").unwrap_or_else(|_| "default".to_string())
}

/// The profile configuration loaded from the data directory.
/// Maps profile names to their configurations.
#[derive(Debug, Clone)]
pub struct ProfileRegistry {
    pub profiles: HashMap<String, Profile>,
    #[allow(dead_code)]
    pub default_profile: String,
    pub data_dir: String,
}

impl ProfileRegistry {
    /// Create a new registry, scanning the data directory for profiles.
    pub fn new(data_dir: &str) -> Self {
        let default = default_profile_name();
        let mut registry = Self {
            profiles: HashMap::new(),
            default_profile: default.clone(),
            data_dir: data_dir.to_string(),
        };
        registry.scan_filesystem();
        registry.ensure_default();
        registry
    }

    /// Scan the filesystem for profile directories and load config.json.
    fn scan_filesystem(&mut self) {
        let profiles_dir: PathBuf = [&self.data_dir, "profiles"].iter().collect();
        if !profiles_dir.exists() {
            return;
        }
        let entries = match fs::read_dir(&profiles_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let profile = if let Some(config) = Profile::load_config(&self.data_dir, &name) {
                Profile::default(&name).with_config(config)
            } else {
                Profile::default(&name)
            };
            self.profiles.insert(name, profile);
        }
    }

    /// Ensure the default profile exists.
    fn ensure_default(&mut self) {
        if !self.profiles.contains_key(&self.default_profile) {
            self.profiles.insert(
                self.default_profile.clone(),
                Profile::default(&self.default_profile),
            );
        }
    }

    /// Get a profile by name, falling back to default.
    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles
            .get(name)
            .or_else(|| self.profiles.get(&self.default_profile))
    }

    /// Get the default profile.
    #[allow(dead_code)]
    pub fn default(&self) -> &Profile {
        self.profiles
            .get(&self.default_profile)
            .expect("Default profile must exist")
    }

    /// List all profile names (filesystem directories).
    pub fn list_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.profiles.keys().cloned().collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_profile_has_core_tools() {
        let p = Profile::default("test");
        assert!(p.allowed_tools.contains(&"create_cron_job".to_string()));
        assert!(p.allowed_tools.contains(&"plugin_manager".to_string()));
        assert_eq!(p.allowed_tools.len(), CORE_TOOLS.len());
    }

    #[test]
    fn test_profile_config_override() {
        let profile = Profile::default("test").with_config(ProfileConfig {
            provider: Some("anthropic".to_string()),
            model: Some("claude-3".to_string()),
            allowed_tools: Some(vec!["filesystem_read".to_string()]),
        });
        assert_eq!(profile.provider, Some("anthropic".to_string()));
        assert_eq!(profile.model, Some("claude-3".to_string()));
        assert_eq!(profile.allowed_tools, vec!["filesystem_read".to_string()]);
    }

    #[test]
    fn test_registry_empty_data_dir() {
        let registry = ProfileRegistry::new("/tmp/nonexistent");
        let default_name = crate::profile::default_profile_name();
        assert!(registry.get(&default_name).is_some());
        assert!(registry.list_names().contains(&default_name));
    }
}
