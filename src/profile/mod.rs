use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
}

impl Profile {
    /// Create a default profile with the given name.
    pub fn default(name: &str) -> Self {
        Self {
            name: name.to_string(),
            model: Some("deepseek-v4-flash".to_string()),
            provider: Some("opencode-go".to_string()),
            base_url: None,
            api_key: None,
            max_tokens: None,
            temperature: None,
            allowed_tools: vec![
                "filesystem_read".to_string(),
                "filesystem_write".to_string(),
                "filesystem_list".to_string(),
                "filesystem_search".to_string(),
                "filesystem_info".to_string(),
                "fetch".to_string(),
                "search_messages".to_string(),
                "search_wiki".to_string(),
                "promote_to_memory".to_string(),
                "list_memories".to_string(),
                "review_memories".to_string(),
                "get_metrics".to_string(),
                // External MCP tools — git
                "git_create_github_repo".to_string(),
                "git_clone_repo".to_string(),
                "git_commit_and_push".to_string(),
                "git_status".to_string(),
                // External MCP tools — docker
                "docker_ps".to_string(),
                "docker_compose".to_string(),
                "docker_exec".to_string(),
                "docker_build".to_string(),
                "docker_info".to_string(),
                "docker_run".to_string(),
            ],
            auto_retrieval_enabled: true,
            retrieval_aggressiveness: 2,
            grounding_required: false,
        }
    }

    /// Resolve the effective model, checking channel override first, then profile.
    #[expect(dead_code)]
    pub fn resolve_model(&self, channel_model: Option<&str>) -> Option<String> {
        channel_model
            .map(|s| s.to_string())
            .or_else(|| self.model.clone())
    }

    /// Resolve the effective provider.
    #[expect(dead_code)]
    pub fn resolve_provider(&self, channel_provider: Option<&str>) -> Option<String> {
        channel_provider
            .map(|s| s.to_string())
            .or_else(|| self.provider.clone())
    }
}

/// The profile configuration loaded from the data directory.
/// Maps profile names to their configurations.
#[derive(Debug, Clone)]
pub struct ProfileRegistry {
    pub profiles: HashMap<String, Profile>,
    #[expect(dead_code)]
    pub default_profile: String,
    #[expect(dead_code)]
    pub data_dir: String,
}

impl ProfileRegistry {
    /// Create a new registry, scanning the data directory for profiles.
    pub fn new(data_dir: &str) -> Self {
        let mut registry = Self {
            profiles: HashMap::new(),
            default_profile: "default".to_string(),
            data_dir: data_dir.to_string(),
        };
        registry.ensure_default();
        registry
    }

    /// Ensure the default profile exists.
    fn ensure_default(&mut self) {
        if !self.profiles.contains_key("default") {
            self.profiles.insert(
                "default".to_string(),
                Profile::default("default"),
            );
        }
    }

    /// Get a profile by name, falling back to default.
    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles
            .get(name)
            .or_else(|| self.profiles.get("default"))
    }

    /// Get the default profile.
    #[expect(dead_code)]
    pub fn default(&self) -> &Profile {
        self.profiles
            .get("default")
            .expect("Default profile must exist")
    }
}
