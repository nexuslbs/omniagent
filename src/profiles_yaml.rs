//! `profiles.yml` — SINGLE source of truth for profile definitions.
//!
//! Profile definitions (previously `profiles/<name>/config.json`) live in
//! `{data_dir}/config/profiles.yml`, mirroring how `config/channels.yml`
//! defines and resolves channels. The map KEY is the profile NAME — the
//! stable identifier used everywhere (`threads.profile`, channels.yml
//! `profile:`, kanban boards/tasks `profile:`, the dashboard profile
//! selects, `/profile` platform commands).
//!
//! ```yaml
//! profiles:
//!   omni:
//!     provider: opencode-go        # optional: falls through to global
//!     model: deepseek-v4-flash     #   default_provider when omitted
//!     plan: false                  # optional profile-level plan override
//!     template: dev-development    # optional profile-level thread template
//!     allowed_tools: []            # optional allowed MCP tool names
//! ```
//!
//! FIELD NAMING (bare, matches channels.yml):
//! - `provider` / `model` — bare names (NOT the legacy `default_provider` /
//!   `default_model` or the DB column names).
//! - `plan` (bool) — profile-level plan override (tier between channel and
//!   global in the plan fallback chain).
//! - `template` (str) — profile-level thread template (tier between channel
//!   and the `dev-development` default).
//! - `allowed_tools` (list) — allowed MCP tool names for the profile.
//! - `base_url` / `api_key` / `max_tokens` / `temperature` — provider-level
//!   overrides kept from the legacy `Profile`/`ProfileConfig` schema.
//! - NO `name` field inside an entry — the map key IS the name (same
//!   yml-key pattern as channels.yml).
//!
//! The legacy `profiles/<name>/config.json` files are NOT removed and NOT
//! migrated — they stay on disk for backward compat until a future release
//! removes them, but they are NO LONGER read for resolution.
//!
//! Profiles become *declared* by presence in the YAML: a `profiles/<name>/`
//! directory with no matching entry in `config/profiles.yml` is ignored
//! (not listed, not resolvable); conversely a YAML entry is considered an
//! existing profile even with no `profiles/<name>/` directory (the dir only
//! carries profile *files*: templates, skills, wiki, MEMORY.md).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::config_path::config_path;
use crate::error::{AppResult, Error};

/// Name of the profile definition file inside `{data_dir}/config/`.
pub const PROFILES_FILE: &str = "profiles.yml";

/// Global data dir — set once at startup (main.rs) so the yml store is
/// reachable from every profile query.
static DATA_DIR: OnceLock<String> = OnceLock::new();

/// Serialize concurrent read-modify-write cycles on profiles.yml (mutations
/// reload the file, mutate a copy, then save atomically). One writer keeps
/// concurrent auto-creations from clobbering each other.
static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// Set the global data dir (idempotent; first call wins).
pub fn set_data_dir(dir: &str) {
    let _ = DATA_DIR.set(dir.to_string());
}

/// The globally configured data dir (if set).
pub fn data_dir() -> Option<&'static str> {
    DATA_DIR.get().map(|s| s.as_str())
}

// ── File structs ────────────────────────────────────────────────────────────

/// Top-level document: `{ profiles: { <name>: {...} } }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilesFile {
    #[serde(default)]
    pub profiles: HashMap<String, ProfileDef>,
}

/// One profile definition. Field names are BARE (`provider`/`model`/
/// `plan`/`template`/`allowed_tools`) — the legacy `config.json` schema
/// (also bare) is preserved for the provider-level fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileDef {
    /// Default provider for this profile (e.g. "opencode-go"). Omitted →
    /// falls through to the global `default_provider` at resolution time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Default model for this profile (e.g. "deepseek-v4-flash").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Profile-level plan override (bool). Tier: channel.plan → profile.plan
    /// → None (plugin decides at runtime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,
    /// Profile-level thread template. Tier: channel.template →
    /// profile.template → "dev-development" (running steps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Allowed MCP tool names for this profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// Base API URL override for this profile's provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// API key override for this profile's provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Max tokens for this profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Temperature for this profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

// ── Path / IO ───────────────────────────────────────────────────────────────

/// `{data_dir}/config/profiles.yml`.
pub fn profiles_path(data_dir: impl AsRef<std::path::Path>) -> PathBuf {
    config_path(data_dir, PROFILES_FILE)
}

/// Load profiles.yml from a data dir. Missing file → empty `ProfilesFile`
/// (a fresh install has no declared profiles; the default profile is
/// auto-declared by the registry). A malformed file is an error (surfaced
/// to API callers).
pub fn load_profiles_from(data_dir: &str) -> AppResult<ProfilesFile> {
    let path = profiles_path(data_dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ProfilesFile::default()),
        Err(e) => {
            return Err(Error::Message(format!(
                "Failed to read {}: {}",
                path.display(),
                e
            )))
        }
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(ProfilesFile::default());
    }
    serde_yaml::from_str(&content)
        .map_err(|e| Error::Message(format!("Failed to parse {}: {}", path.display(), e)))
}

/// Load profiles.yml using the global data dir (set at startup).
pub fn load_profiles() -> AppResult<ProfilesFile> {
    let dir = data_dir().ok_or_else(|| {
        Error::Message("profiles_yaml::set_data_dir() was not called".to_string())
    })?;
    load_profiles_from(dir)
}

/// Load profiles.yml, logging + ignoring parse errors (used by background
/// loops: a broken file must not take down the scheduler / supervisor).
pub fn load_profiles_or_empty() -> ProfilesFile {
    match load_profiles() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("[profiles.yml] load failed, treating as empty: {}", e);
            ProfilesFile::default()
        }
    }
}

/// Atomically persist profiles.yml (tmp file + rename). Creates the config
/// dir idempotently. Validation is the caller's responsibility.
pub fn save_profiles_file(data_dir: &str, profiles: &ProfilesFile) -> AppResult<()> {
    crate::config_path::ensure_config_dir(data_dir);
    let path = profiles_path(data_dir);
    let yaml = serde_yaml::to_string(profiles)
        .map_err(|e| Error::Message(format!("Failed to serialize profiles.yml: {}", e)))?;
    let tmp = path.with_extension("yml.tmp");
    std::fs::write(&tmp, yaml)
        .map_err(|e| Error::Message(format!("Failed to write {}: {}", tmp.display(), e)))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        Error::Message(format!(
            "Failed to rename {} → {}: {}",
            tmp.display(),
            path.display(),
            e
        ))
    })?;
    Ok(())
}

/// Read-modify-write a single profile entry under the global save lock.
/// `mutate` receives the existing `ProfileDef` (or `None` when absent) and
/// returns the new definition; the entry is upserted under `name` and the
/// file rewritten atomically. Serializes concurrent mutations.
pub fn update_profile<F>(name: &str, mutate: F) -> AppResult<ProfileDef>
where
    F: FnOnce(Option<&ProfileDef>) -> AppResult<ProfileDef>,
{
    let dir = data_dir().ok_or_else(|| {
        Error::Message("profiles_yaml::set_data_dir() was not called".to_string())
    })?;
    let _guard = SAVE_LOCK
        .lock()
        .map_err(|e| Error::Message(format!("profiles.yml save lock poisoned: {}", e)))?;
    let mut file = load_profiles_from(dir)?;
    let existing = file.profiles.get(name).cloned();
    let new_def = mutate(existing.as_ref())?;
    file.profiles.insert(name.to_string(), new_def.clone());
    save_profiles_file(dir, &file)?;
    Ok(new_def)
}

/// Merge an external `ProfilesFile` (from the import API) into the on-disk
/// store under the global save lock. Existing entries with the same name are
/// OVERWRITTEN (upsert semantics — same as the channels import precedent,
/// where every imported channel is PATCHed/upserted into channels.yml).
/// Each imported definition is validated before persist. Returns the names
/// that were newly added and the names that were updated.
pub fn merge_profiles_file(imported: &ProfilesFile) -> AppResult<(Vec<String>, Vec<String>)> {
    let dir = data_dir().ok_or_else(|| {
        Error::Message("profiles_yaml::set_data_dir() was not called".to_string())
    })?;
    let _guard = SAVE_LOCK
        .lock()
        .map_err(|e| Error::Message(format!("profiles.yml save lock poisoned: {}", e)))?;
    let mut file = load_profiles_from(dir)?;
    let mut added = Vec::new();
    let mut updated = Vec::new();
    for (name, def) in &imported.profiles {
        validate_profile(name, def)?;
        if file.profiles.contains_key(name) {
            updated.push(name.clone());
        } else {
            added.push(name.clone());
        }
        file.profiles.insert(name.clone(), def.clone());
    }
    if !added.is_empty() || !updated.is_empty() {
        save_profiles_file(dir, &file)?;
    }
    Ok((added, updated))
}

// ── Lookups ─────────────────────────────────────────────────────────────────

/// Get a profile definition by name (yml key). Missing → None.
pub fn get_by_name(name: &str) -> AppResult<Option<ProfileDef>> {
    let file = load_profiles()?;
    Ok(file.profiles.get(name).cloned())
}

/// All profiles sorted by name.
pub fn find_all() -> AppResult<Vec<(String, ProfileDef)>> {
    let file = load_profiles()?;
    let mut v: Vec<(String, ProfileDef)> = file.profiles.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(v)
}

/// True when a profile name is declared in the yml.
pub fn exists(name: &str) -> bool {
    get_by_name(name).map(|p| p.is_some()).unwrap_or(false)
}

// ── Validation ──────────────────────────────────────────────────────────────

/// Validate a profile definition (duplicate keys are impossible — the map
/// key IS the name). A profile entry may be empty (all optional fields) —
/// the registry supplies the in-memory defaults.
pub fn validate_profile(name: &str, _def: &ProfileDef) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("profile name (yml key) must not be empty".to_string());
    }
    if let Some(tools) = &_def.allowed_tools {
        if tools.iter().any(|t| t.trim().is_empty()) {
            return Err(format!(
                "profile '{}': allowed_tools contains an empty tool name",
                name
            ));
        }
    }
    Ok(())
}

/// Read-modify-write a single profile entry in an EXPLICIT data dir under the
/// global save lock. Data-dir-parameterized core of [`update_profile`] so the
/// profile registry can write to a registry-owned data dir (tests, resolvers)
/// without relying on the process-global dir.
pub fn update_profile_in<F>(data_dir: &str, name: &str, mutate: F) -> AppResult<ProfileDef>
where
    F: FnOnce(Option<&ProfileDef>) -> AppResult<ProfileDef>,
{
    let _guard = SAVE_LOCK
        .lock()
        .map_err(|e| Error::Message(format!("profiles.yml save lock poisoned: {}", e)))?;
    let mut file = load_profiles_from(data_dir)?;
    let existing = file.profiles.get(name).cloned();
    let new_def = mutate(existing.as_ref())?;
    file.profiles.insert(name.to_string(), new_def.clone());
    save_profiles_file(data_dir, &file)?;
    Ok(new_def)
}

/// Merge an external `ProfilesFile` into the on-disk store of an EXPLICIT
/// data dir under the global save lock. Data-dir-parameterized core of
/// [`merge_profiles_file`] (used by the import API, which holds the data dir
/// in server state).
pub fn merge_profiles_file_in(
    data_dir: &str,
    imported: &ProfilesFile,
) -> AppResult<(Vec<String>, Vec<String>)> {
    let _guard = SAVE_LOCK
        .lock()
        .map_err(|e| Error::Message(format!("profiles.yml save lock poisoned: {}", e)))?;
    let mut file = load_profiles_from(data_dir)?;
    let mut added = Vec::new();
    let mut updated = Vec::new();
    for (name, def) in &imported.profiles {
        validate_profile(name, def)?;
        if file.profiles.contains_key(name) {
            updated.push(name.clone());
        } else {
            added.push(name.clone());
        }
        file.profiles.insert(name.clone(), def.clone());
    }
    if !added.is_empty() || !updated.is_empty() {
        save_profiles_file(data_dir, &file)?;
    }
    Ok((added, updated))
}

// ── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_yaml() -> &'static str {
        r#"
profiles:
  omni:
    allowed_tools: []
  research:
    provider: opencode-go
    model: deepseek-v4-flash
    plan: true
    template: researcher
    allowed_tools:
      - filesystem_read
      - search_messages
"#
    }

    #[test]
    fn parse_full_file() {
        let file: ProfilesFile = serde_yaml::from_str(sample_yaml()).expect("parse");
        assert_eq!(file.profiles.len(), 2);
        let o = &file.profiles["omni"];
        assert_eq!(o.provider, None, "bare optional fields");
        assert_eq!(o.model, None);
        assert_eq!(o.allowed_tools.as_deref(), Some(&[] as &[String]));
        let r = &file.profiles["research"];
        assert_eq!(r.provider.as_deref(), Some("opencode-go"));
        assert_eq!(r.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(r.plan, Some(true));
        assert_eq!(r.template.as_deref(), Some("researcher"));
        assert_eq!(
            r.allowed_tools.as_deref().unwrap().len(),
            2,
            "allowed_tools list"
        );
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("profilesyml-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = load_profiles_from(dir.to_str().unwrap()).expect("missing file → empty");
        assert!(file.profiles.is_empty());
    }

    #[test]
    fn parse_error_is_reported() {
        let dir = std::env::temp_dir().join(format!("profilesyml-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(
            dir.join("config").join("profiles.yml"),
            "profiles: [unclosed",
        )
        .unwrap();
        let err = load_profiles_from(dir.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = std::env::temp_dir().join(format!("profilesyml-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut file = ProfilesFile::default();
        file.profiles.insert(
            "omni".to_string(),
            ProfileDef {
                provider: Some("opencode-go".to_string()),
                model: Some("deepseek-v4-flash".to_string()),
                plan: Some(false),
                template: Some("dev-development".to_string()),
                allowed_tools: Some(vec!["filesystem_read".to_string()]),
                ..Default::default()
            },
        );
        save_profiles_file(dir.to_str().unwrap(), &file).expect("save");
        let loaded = load_profiles_from(dir.to_str().unwrap()).expect("reload");
        assert_eq!(loaded.profiles.len(), 1);
        let p = &loaded.profiles["omni"];
        assert_eq!(p.provider.as_deref(), Some("opencode-go"));
        assert_eq!(p.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(p.plan, Some(false));
        assert_eq!(p.template.as_deref(), Some("dev-development"));
        assert_eq!(p.allowed_tools.as_deref().unwrap().len(), 1);
    }

    #[test]
    fn update_profile_upserts_and_persists() {
        let dir = std::env::temp_dir().join(format!("profilesyml-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        set_data_dir(dir.to_str().unwrap()); // first call wins in the process; unique per test dir
                                             // Insert a new profile
        update_profile("cli-new", |existing| {
            assert!(existing.is_none());
            Ok(ProfileDef {
                provider: Some("noop".to_string()),
                ..Default::default()
            })
        })
        .expect("upsert");
        // Mutate the same profile (existing must be seen)
        update_profile("cli-new", |existing| {
            let mut d = existing.cloned().unwrap_or_default();
            d.plan = Some(true);
            Ok(d)
        })
        .expect("mutate");
        let loaded = load_profiles().expect("reload");
        assert!(loaded.profiles["cli-new"].plan.unwrap_or(false));
    }

    #[test]
    fn merge_profiles_file_overwrites_and_adds() {
        let dir = std::env::temp_dir().join(format!("profilesyml-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        set_data_dir(dir.to_str().unwrap());
        // Existing profile on disk
        update_profile("existing", |_| {
            Ok(ProfileDef {
                provider: Some("old".to_string()),
                ..Default::default()
            })
        })
        .expect("seed");
        // Import: overwrites `existing`, adds `new`
        let mut imported = ProfilesFile::default();
        imported.profiles.insert(
            "existing".to_string(),
            ProfileDef {
                provider: Some("new-provider".to_string()),
                ..Default::default()
            },
        );
        imported.profiles.insert(
            "new".to_string(),
            ProfileDef {
                model: Some("deepseek-v4-flash".to_string()),
                ..Default::default()
            },
        );
        let (added, updated) = merge_profiles_file(&imported).expect("merge");
        assert_eq!(added, vec!["new".to_string()]);
        assert_eq!(updated, vec!["existing".to_string()]);
        let loaded = load_profiles().expect("reload");
        assert_eq!(
            loaded.profiles["existing"].provider.as_deref(),
            Some("new-provider")
        );
        assert!(loaded.profiles.contains_key("new"));
    }

    #[test]
    fn validation_rules() {
        assert!(validate_profile("ok", &ProfileDef::default()).is_ok());
        assert!(validate_profile("", &ProfileDef::default()).is_err());
        assert!(validate_profile(
            "bad",
            &ProfileDef {
                allowed_tools: Some(vec!["".to_string()]),
                ..Default::default()
            }
        )
        .is_err());
    }
}
