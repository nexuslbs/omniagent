use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

use crate::profiles_yaml::ProfileDef;

/// A profile defines the model, provider, data paths, and allowed tools
/// for a given context (channel or direct prompt).
///
/// Profiles are DECLARED by `{data_dir}/config/profiles.yml` - a
/// `profiles/<name>/` directory with no matching YAML entry is ignored; a
/// YAML entry without a directory is a valid existing profile. The legacy
/// `profiles/<name>/config.json` stays on disk (backward compat) but is no
/// longer read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// Default model for this profile. None → resolves to the provider's
    /// default model / global default.
    pub model: Option<String>,
    /// Default provider for this profile. None → falls through to the global
    /// default_provider at resolution time.
    pub provider: Option<String>,
    /// Profile-level plan override (bool). Tier: channel.plan → profile.plan
    /// → None (plugin decides at runtime).
    pub plan: Option<bool>,
    /// Profile-level thread template. Tier: channel.template →
    /// profile.template → "dev-development" (running steps).
    pub template: Option<String>,
    /// Base API URL override for this profile
    pub base_url: Option<String>,
    /// API key override for this profile
    pub api_key: Option<String>,
    /// Max tokens for this profile
    pub max_tokens: Option<u32>,
    /// Temperature for this profile
    pub temperature: Option<f32>,
    /// List of allowed MCP tool names for this profile (from profiles.yml)
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

/// Legacy schema for `profiles/<name>/config.json` - KEPT for backward
/// compat only (the file stays on disk, untouched, but is NOT read for
/// resolution anymore).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ProfileConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
}

impl Profile {
    /// Create a default profile with the given name (in-memory fallback used
    /// when no profiles.yml declares the profile). Provider/model are
    /// neutral (`None`): resolution falls through to the global
    /// `default_provider` / the provider's default model - never to a
    /// hardcoded vendor name.
    pub fn default(name: &str) -> Self {
        Self {
            name: name.to_string(),
            model: None,
            provider: None,
            plan: None,
            template: None,
            base_url: None,
            api_key: None,
            max_tokens: None,
            temperature: None,
            allowed_tools: Vec::new(), // Tools come from profiles.yml / dashboard UI
            auto_retrieval_enabled: true,
            retrieval_aggressiveness: 2,
            grounding_required: false,
            prompt_budget: None, // uses PROMPT_BUDGET_DEFAULT (15,000)
        }
    }

    /// Build a `Profile` from a `profiles.yml` definition. YAML-absent
    /// fields stay `None` (so an entry that omits `provider`/`model` falls
    /// through to the global `default_provider` - never to the in-memory
    /// built-ins). Non-YAML runtime defaults (retrieval/grounding/budget)
    /// take the `Profile::default` values.
    pub fn from_def(name: &str, def: &ProfileDef) -> Self {
        let mut p = Profile::default(name);
        p.provider = def.provider.clone();
        p.model = def.model.clone();
        p.plan = def.plan;
        p.template = def.template.clone();
        p.base_url = def.base_url.clone();
        p.api_key = def.api_key.clone();
        p.max_tokens = def.max_tokens;
        p.temperature = def.temperature;
        p.allowed_tools = def.allowed_tools.clone().unwrap_or_default();
        p
    }

    /// Load a profile config from `<data_dir>/profiles/<name>/config.json`
    /// (LEGACY - no longer consulted for resolution; kept for backward
    /// compat and tests).
    #[allow(dead_code)]
    pub fn load_config(data_dir: &str, name: &str) -> Option<ProfileConfig> {
        let path: std::path::PathBuf = [data_dir, "profiles", name, "config.json"].iter().collect();
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Apply a legacy ProfileConfig on top of the default (LEGACY - only
    /// used by tests; the YAML store is the resolution source now).
    #[allow(dead_code)]
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

/// Read the default profile name from the global config, falling back to "omni".
pub fn default_profile_name() -> String {
    crate::agent::config::get_global()
        .map(|g| g.read().default_profile.clone())
        .unwrap_or_else(|| "omni".to_string())
}

/// The profile configuration loaded from the data directory.
/// Maps profile names to their configurations.
///
/// A profile is DECLARED iff it has an entry in `config/profiles.yml`. The
/// filesystem `profiles/<name>/` directories are NOT scanned as a source of
/// truth: a dir without a YAML entry is ignored; a YAML entry without a dir
/// is a valid existing profile.
#[derive(Debug, Clone)]
pub struct ProfileRegistry {
    pub profiles: HashMap<String, Profile>,
    #[allow(dead_code)]
    pub default_profile: String,
    pub data_dir: String,
}

impl ProfileRegistry {
    /// Create a new registry, sourcing profiles from `config/profiles.yml`.
    pub fn new(data_dir: &str) -> Self {
        let default = default_profile_name();
        let mut registry = Self {
            profiles: HashMap::new(),
            default_profile: default.clone(),
            data_dir: data_dir.to_string(),
        };
        registry.scan_yaml();
        registry.ensure_default();
        registry
    }

    /// Scan `{data_dir}/config/profiles.yml` for declared profiles. A
    /// malformed file is logged and treated as empty (background readers
    /// must not crash; API callers see the error through the yml loaders).
    fn scan_yaml(&mut self) {
        let file = match crate::profiles_yaml::load_profiles_from(&self.data_dir) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("[profile] profiles.yml load failed, treating as empty: {e}");
                return;
            }
        };
        for (name, def) in file.profiles {
            self.profiles
                .insert(name.clone(), Profile::from_def(&name, &def));
        }
    }

    /// Ensure the default profile is DECLARED.
    ///
    /// When the default profile has no entry in `config/profiles.yml`, the
    /// in-memory default is inserted AND the entry is upserted into
    /// `config/profiles.yml` (atomic write under the save lock) so the
    /// default profile is declared on disk - this is the startup
    /// auto-create. It NEVER creates or touches anything under
    /// `profiles/` - no `create_dir_all`, no `config.json` write. An
    /// existing legacy `profiles/<default>/config.json` is left alone.
    fn ensure_default(&mut self) {
        if self.profiles.contains_key(&self.default_profile) {
            return;
        }
        self.profiles.insert(
            self.default_profile.clone(),
            Profile::from_def(&self.default_profile, &ProfileDef::default()),
        );
        if let Err(e) =
            crate::profiles_yaml::update_profile_in(&self.data_dir, &self.default_profile, |_| {
                Ok(ProfileDef::default())
            })
        {
            tracing::warn!(
                "profile: failed to declare default profile '{}' in profiles.yml: {e}",
                self.default_profile
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

    /// List all declared profile names (profiles.yml entries), sorted.
    pub fn list_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.profiles.keys().cloned().collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "profile-mod-{}-{}-{}",
            tag,
            std::process::id(),
            std::thread::current().name().unwrap_or("t")
        ));
        let _ = fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        dir
    }

    #[test]
    fn test_default_profile_starts_empty() {
        let p = Profile::default("test");
        assert!(
            p.allowed_tools.is_empty(),
            "Default profile should have no tools - they come from profiles.yml"
        );
        assert_eq!(p.plan, None);
        assert_eq!(p.template, None);
        assert_eq!(p.provider, None, "no vendor-name provider default in core");
        assert_eq!(p.model, None, "no vendor-name model default in core");
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
    fn test_from_def_absent_fields_stay_none() {
        // A YAML entry that omits provider/model must NOT inherit the
        // in-memory built-ins (falls through to global default_provider).
        let p = Profile::from_def("omni", &ProfileDef::default());
        assert_eq!(p.provider, None, "absent provider stays None");
        assert_eq!(p.model, None, "absent model stays None");
        assert_eq!(p.plan, None);
        assert_eq!(p.template, None);
        assert!(p.allowed_tools.is_empty());
    }

    #[test]
    fn test_from_def_applies_fields() {
        let p = Profile::from_def(
            "research",
            &ProfileDef {
                provider: Some("opencode-go".to_string()),
                model: Some("deepseek-v4-flash".to_string()),
                plan: Some(true),
                template: Some("researcher".to_string()),
                allowed_tools: Some(vec!["search_messages".to_string()]),
                ..Default::default()
            },
        );
        assert_eq!(p.provider.as_deref(), Some("opencode-go"));
        assert_eq!(p.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(p.plan, Some(true));
        assert_eq!(p.template.as_deref(), Some("researcher"));
        assert_eq!(p.allowed_tools, vec!["search_messages".to_string()]);
    }

    #[test]
    fn test_registry_empty_data_dir_declares_default_without_profiles_dir() {
        // Startup auto-create: no profiles.yml, no profiles/ dir → the
        // default profile is declared IN profiles.yml and NOTHING is created
        // under profiles/ (no dir, no config.json).
        let dir = std::env::temp_dir().join(format!("profile-reg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir); // NO config/ dir at all
        let registry = ProfileRegistry::new(dir.to_str().unwrap());
        let default_name = registry.default_profile.clone();
        assert!(registry.get(&default_name).is_some());
        assert!(registry.list_names().contains(&default_name));
        // profiles.yml now declares the default profile...
        let yml_path = dir.join("config").join("profiles.yml");
        assert!(yml_path.exists(), "profiles.yml should be auto-created");
        let file = crate::profiles_yaml::load_profiles_from(dir.to_str().unwrap()).unwrap();
        assert!(file.profiles.contains_key(&default_name));
        // ...but NOTHING under profiles/ exists.
        assert!(
            !dir.join("profiles").exists(),
            "startup auto-create must NOT touch the profiles/ directory"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_yaml_entry_without_dir_is_existing_profile() {
        // A profiles.yml entry with NO profiles/<name>/ directory → the
        // profile exists and resolves (acceptance criterion).
        let dir = temp_dir("yaml-only");
        std::fs::write(
            dir.join("config").join("profiles.yml"),
            "profiles:\n  omni:\n    provider: opencode-go\n    model: deepseek-v4-flash\n    plan: true\n    template: dev-development\n    allowed_tools:\n      - filesystem_read\n",
        )
        .unwrap();
        let registry = ProfileRegistry::new(dir.to_str().unwrap());
        let p = registry.get("omni").expect("yaml-only profile resolves");
        assert_eq!(p.provider.as_deref(), Some("opencode-go"));
        assert_eq!(p.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(p.plan, Some(true));
        assert_eq!(p.template.as_deref(), Some("dev-development"));
        assert_eq!(p.allowed_tools, vec!["filesystem_read".to_string()]);
        assert!(
            !dir.join("profiles").exists(),
            "no directory needed for a yaml-declared profile"
        );
    }

    #[test]
    fn test_dir_without_yaml_entry_is_ignored() {
        // A profiles/<name>/ directory with NO yml entry → ignored: not
        // listed, not resolvable (falls back to the default profile).
        let dir = temp_dir("dir-only");
        std::fs::create_dir_all(dir.join("profiles").join("ghost")).unwrap();
        std::fs::write(
            dir.join("profiles").join("ghost").join("config.json"),
            r#"{"provider": "openai", "model": "gpt-4"}"#,
        )
        .unwrap();
        let registry = ProfileRegistry::new(dir.to_str().unwrap());
        assert!(
            !registry.list_names().contains(&"ghost".to_string()),
            "dir without yml entry must be ignored"
        );
        assert_eq!(
            registry.get("ghost").unwrap().name,
            registry.default_profile
        );
        // The legacy config.json stays on disk untouched.
        assert!(
            dir.join("profiles")
                .join("ghost")
                .join("config.json")
                .exists(),
            "legacy config.json files are NOT removed"
        );
    }

    #[test]
    fn test_existing_config_json_left_untouched() {
        // Backward compat: an existing profiles/<default>/config.json is
        // neither read for resolution nor modified.
        let dir = temp_dir("legacy");
        std::fs::create_dir_all(dir.join("profiles").join("omni")).unwrap();
        let cfg =
            r#"{"provider": "openai", "model": "gpt-4", "allowed_tools": ["filesystem_read"]}"#;
        std::fs::write(dir.join("profiles").join("omni").join("config.json"), cfg).unwrap();
        // profiles.yml declares omni WITHOUT provider/model → config.json is
        // NOT consulted (provider stays None, falls through to global).
        std::fs::write(
            dir.join("config").join("profiles.yml"),
            "profiles:\n  omni:\n    allowed_tools: []\n",
        )
        .unwrap();
        let registry = ProfileRegistry::new(dir.to_str().unwrap());
        let p = registry.get("omni").expect("resolves from yml");
        assert_eq!(p.provider, None, "config.json must NOT be read");
        assert_eq!(p.model, None, "config.json must NOT be read");
        assert_eq!(
            fs::read_to_string(dir.join("profiles").join("omni").join("config.json")).unwrap(),
            cfg,
            "config.json content must be unchanged"
        );
    }

    #[test]
    fn test_registry_get_falls_back_to_default() {
        let dir = temp_dir("fallback");
        let registry = ProfileRegistry::new(dir.to_str().unwrap());
        let default_name = registry.default_profile.clone();
        assert!(registry.get("no-such-profile").is_some());
        assert_eq!(registry.get("no-such-profile").unwrap().name, default_name);
    }
}
