//! `channels.yml` — SINGLE source of truth for channel definitions AND
//! runtime state.
//!
//! Channel definitions (previously rows in the `channels` table) live in
//! `{data_dir}/config/channels.yml`. The map KEY is the channel NAME — the
//! stable identifier used everywhere: API id (`GET /channels/{name}`),
//! `threads.channel_id`, `messages.channel_id`, `kanban_tasks.channel_id`,
//! `summaries.channel_id` and tasks.yml `channel:` references. This mirrors
//! the established yml-key pattern: `threads.schedule_task_id` holds the
//! tasks.yml schedule key, `threads.workflow_id` holds the workflows.yml
//! workflow key — yml keys are referenced by key string, never by a
//! DB-generated id.
//!
//! ```yaml
//! channels:
//!   kanban:
//!     resource_identifier: kanban
//!     cause: system
//!     profile: omni
//!   mattermost-af66ardb:
//!     platform: mattermost
//!     resource_identifier: af66ardbd3dffe7gwxnokzi19c
//!     cause: user
//!     profile: omni
//! ```
//!
//! FIELD NAMING (bare, matches `threads.profile/provider/model` and
//! tasks.yml `profile:`):
//! - `profile` / `model` / `provider` — NOT the legacy DB column names
//!   `current_profile` / `current_model` / `current_provider`. The API keeps
//!   exposing `current_*` for dashboard compatibility; the loader maps.
//! - `plan` (bool) — the single channel-level plan override.
//! - NO `metadata`, NO `external_id` (derived from `resource_identifier`),
//!   NO `created_at`/`updated_at` (no runtime clock in a
//!   static file).
//!
//! Runtime-mutable fields (`profile`/`model`/`provider`/`closed`/`readonly`/
//! `plan`/`template`) live in the SAME yml and are REWRITTEN atomically
//! (tmp file + rename) when they change — the yml is the only store, there
//! is no runtime table. The `channels` database table and all its foreign
//! keys are dropped by the migration; `channel_id` columns are TEXT holding
//! channel NAMES.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::config_path::config_path;
use crate::error::{AppResult, Error};

/// Name of the channels definition file inside `{data_dir}/config/`.
pub const CHANNELS_FILE: &str = "channels.yml";

/// Global data dir — set once at startup (main.rs) so the yml store is
/// reachable from every channel query (the yml replaces the DB table, but
/// callers still pass the (now unused) pool).
static DATA_DIR: OnceLock<String> = OnceLock::new();

/// Serialize concurrent read-modify-write cycles on channels.yml (mutations
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

/// Top-level document: `{ channels: { <name>: {...} } }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelsFile {
    #[serde(default)]
    pub channels: HashMap<String, ChannelDef>,
}

/// One channel definition. Field names are BARE (`profile`/`model`/
/// `provider`/`plan`) — the legacy `current_*`/`metadata` names are dead and
/// NOT accepted here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelDef {
    /// Platform name ("mattermost", "telegram", ...). OPTIONAL: a channel
    /// without a platform is type 'cli' (kanban/cron system channels).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Identifier of the resource within the platform (chat_id, session id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_identifier: Option<String>,
    /// cause: user | cron | system | setup.
    #[serde(default = "default_cause")]
    pub cause: String,
    /// Bare profile name (legacy channels.current_profile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Bare model name (legacy channels.current_model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Bare provider name (legacy channels.current_provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readonly: Option<bool>,
    /// Single channel-level plan override (bool).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

fn default_cause() -> String {
    "system".to_string()
}

// ── Path / IO ───────────────────────────────────────────────────────────────

/// `{data_dir}/config/channels.yml`.
pub fn channels_path(data_dir: impl AsRef<std::path::Path>) -> PathBuf {
    config_path(data_dir, CHANNELS_FILE)
}

/// Load channels.yml from a data dir. Missing file → empty `ChannelsFile`
/// (channels are created on demand). A malformed file is an error (surfaced
/// to API callers).
pub fn load_channels_from(data_dir: &str) -> AppResult<ChannelsFile> {
    let path = channels_path(data_dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ChannelsFile::default()),
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
        return Ok(ChannelsFile::default());
    }
    serde_yaml::from_str(&content)
        .map_err(|e| Error::Message(format!("Failed to parse {}: {}", path.display(), e)))
}

/// Load channels.yml using the global data dir (set at startup).
pub fn load_channels() -> AppResult<ChannelsFile> {
    let dir = data_dir().ok_or_else(|| {
        Error::Message("channels_yaml::set_data_dir() was not called".to_string())
    })?;
    load_channels_from(dir)
}

/// Load channels.yml, logging + ignoring parse errors (used by background
/// loops: a broken file must not take down the scheduler / supervisor).
pub fn load_channels_or_empty() -> ChannelsFile {
    match load_channels() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("[channels.yml] load failed, treating as empty: {}", e);
            ChannelsFile::default()
        }
    }
}

/// Atomically persist channels.yml (tmp file + rename). Creates the config
/// dir idempotently. Validation is the caller's responsibility.
pub fn save_channels_file(data_dir: &str, channels: &ChannelsFile) -> AppResult<()> {
    crate::config_path::ensure_config_dir(data_dir);
    let path = channels_path(data_dir);
    let yaml = serde_yaml::to_string(channels)
        .map_err(|e| Error::Message(format!("Failed to serialize channels.yml: {}", e)))?;
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

/// Read-modify-write a single channel entry under the global save lock.
/// `mutate` receives the existing `ChannelDef` (or `None` when absent) and
/// returns the new definition; the entry is upserted under `name` and the
/// file rewritten atomically. Serializes concurrent mutations.
pub fn update_channel<F>(name: &str, mutate: F) -> AppResult<ChannelDef>
where
    F: FnOnce(Option<&ChannelDef>) -> AppResult<ChannelDef>,
{
    let dir = data_dir().ok_or_else(|| {
        Error::Message("channels_yaml::set_data_dir() was not called".to_string())
    })?;
    let _guard = SAVE_LOCK
        .lock()
        .map_err(|e| Error::Message(format!("channels.yml save lock poisoned: {}", e)))?;
    let mut file = load_channels_from(dir)?;
    let existing = file.channels.get(name).cloned();
    let new_def = mutate(existing.as_ref())?;
    file.channels.insert(name.to_string(), new_def.clone());
    save_channels_file(dir, &file)?;
    Ok(new_def)
}

// ── Lookups ─────────────────────────────────────────────────────────────────

/// Get a channel definition by its name (yml key). Missing → None.
pub fn get_by_name(name: &str) -> AppResult<Option<ChannelDef>> {
    let file = load_channels()?;
    Ok(file.channels.get(name).cloned())
}

/// Get a channel definition by (platform, resource_identifier). Missing → None.
pub fn get_by_platform_and_resource(
    platform: &str,
    resource_identifier: &str,
) -> AppResult<Option<(String, ChannelDef)>> {
    let file = load_channels()?;
    for (name, def) in &file.channels {
        if def
            .platform
            .as_deref()
            .map(|p| p == platform)
            .unwrap_or(false)
            && def
                .resource_identifier
                .as_deref()
                .map(|r| r == resource_identifier)
                .unwrap_or(false)
        {
            return Ok(Some((name.clone(), def.clone())));
        }
    }
    Ok(None)
}

/// All channels sorted by name.
pub fn find_all() -> AppResult<Vec<(String, ChannelDef)>> {
    let file = load_channels()?;
    let mut v: Vec<(String, ChannelDef)> = file.channels.into_iter().collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(v)
}

/// True when a channel name exists in the yml.
pub fn exists(name: &str) -> bool {
    get_by_name(name).map(|c| c.is_some()).unwrap_or(false)
}

/// Resolve the effective channel NAME for a producer (scheduler, hooks,
/// kanban dispatch, CLI): an explicit channel name (must exist in
/// channels.yml) wins; otherwise the named default-channel setting
/// (`default_*_channel` in settings.yml) is used. When neither resolves to
/// a known channel, returns `None` — the caller creates the thread with an
/// empty channel and fails it with "no channel defined" (the record is
/// kept for audit).
pub fn resolve_default_channel(explicit: Option<&str>, setting_name: &str) -> Option<String> {
    if let Some(name) = explicit.map(str::trim).filter(|n| !n.is_empty()) {
        // The explicit name wins even when it is NOT a known channel: the
        // caller then fails the thread with "channel not found" (fail-with-
        // record) — never silently substitute the default setting.
        return Some(name.to_string());
    }
    let dir = data_dir()?;
    let value = crate::server::settings::load_settings_file(dir)
        .get(setting_name)
        .cloned()
        .unwrap_or_default();
    let name = value.trim().to_string();
    if name.is_empty() || !exists(&name) {
        return None;
    }
    Some(name)
}

// ── Validation ──────────────────────────────────────────────────────────────

/// Validate a channel definition (duplicate keys are impossible — the map
/// key IS the name; cause must be one of the known values; platform-less
/// channels are allowed = type 'cli').
pub fn validate_channel(name: &str, def: &ChannelDef) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("channel name (yml key) must not be empty".to_string());
    }
    if !matches!(def.cause.as_str(), "user" | "cron" | "system" | "setup") {
        return Err(format!(
            "channel '{}': invalid cause '{}': must be one of user|cron|system|setup",
            name, def.cause
        ));
    }
    Ok(())
}

// ── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_yaml() -> &'static str {
        r#"
channels:
  kanban:
    resource_identifier: kanban
    cause: system
    profile: omni
  cron:
    resource_identifier: cron
    cause: system
    profile: omni
  mattermost-af66ardb:
    platform: mattermost
    resource_identifier: af66ardbd3dffe7gwxnokzi19c
    cause: user
    profile: omni
    plan: true
"#
    }

    #[test]
    fn parse_full_file() {
        let file: ChannelsFile = serde_yaml::from_str(sample_yaml()).expect("parse");
        assert_eq!(file.channels.len(), 3);
        let k = &file.channels["kanban"];
        assert_eq!(k.platform, None, "platform-less system channel");
        assert_eq!(k.cause, "system");
        assert_eq!(k.profile.as_deref(), Some("omni"));
        let m = &file.channels["mattermost-af66ardb"];
        assert_eq!(m.platform.as_deref(), Some("mattermost"));
        assert_eq!(m.plan, Some(true), "single plan bool");
        assert!(
            m.model.is_none() && m.provider.is_none(),
            "bare optional fields"
        );
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("channelsyml-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = load_channels_from(dir.to_str().unwrap()).expect("missing file → empty");
        assert!(file.channels.is_empty());
    }

    #[test]
    fn parse_error_is_reported() {
        let dir = std::env::temp_dir().join(format!("channelsyml-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(
            dir.join("config").join("channels.yml"),
            "channels: [unclosed",
        )
        .unwrap();
        let err = load_channels_from(dir.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = std::env::temp_dir().join(format!("channelsyml-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut file = ChannelsFile::default();
        file.channels.insert(
            "test-channel".to_string(),
            ChannelDef {
                platform: Some("mattermost".to_string()),
                resource_identifier: Some("rid-123".to_string()),
                cause: "user".to_string(),
                profile: Some("omni".to_string()),
                plan: Some(false),
                ..Default::default()
            },
        );
        save_channels_file(dir.to_str().unwrap(), &file).expect("save");
        let loaded = load_channels_from(dir.to_str().unwrap()).expect("reload");
        assert_eq!(loaded.channels.len(), 1);
        let c = &loaded.channels["test-channel"];
        assert_eq!(c.platform.as_deref(), Some("mattermost"));
        assert_eq!(c.cause, "user");
        assert_eq!(c.plan, Some(false));
    }

    #[test]
    fn update_channel_upserts_and_persists() {
        let dir = std::env::temp_dir().join(format!("channelsyml-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        set_data_dir(dir.to_str().unwrap()); // first call wins in the process; unique per test dir
                                             // Insert a new channel
        update_channel("cli-new", |existing| {
            assert!(existing.is_none());
            Ok(ChannelDef {
                cause: "user".to_string(),
                profile: Some("omni".to_string()),
                ..Default::default()
            })
        })
        .expect("upsert");
        // Mutate the same channel (existing must be seen)
        update_channel("cli-new", |existing| {
            let mut d = existing.cloned().unwrap_or_default();
            d.closed = Some(true);
            Ok(d)
        })
        .expect("mutate");
        let loaded = load_channels().expect("reload");
        assert!(loaded.channels["cli-new"].closed.unwrap_or(false));
        assert_eq!(loaded.channels["cli-new"].cause, "user");
    }

    #[test]
    fn validation_rules() {
        assert!(validate_channel(
            "ok",
            &ChannelDef {
                cause: "system".to_string(),
                ..Default::default()
            }
        )
        .is_ok());
        let bad_cause = ChannelDef {
            cause: "bogus".to_string(),
            ..Default::default()
        };
        assert!(validate_channel("x", &bad_cause).is_err());
        assert!(validate_channel("", &ChannelDef::default()).is_err());
    }

    // ── default channel resolution (explicit -> default setting -> '') ──

    /// Ensure a global data dir exists for the resolution tests. The
    /// OnceLock global may already be set by an earlier test in this
    /// process; fixtures are upserted into whatever dir is current (the
    /// channels.yml writer is serialized by the save lock, so concurrent
    /// fixture writes merge safely).
    fn ensure_global_data_dir() -> String {
        if let Some(d) = data_dir() {
            return d.to_string();
        }
        let d = std::env::temp_dir().join(format!("channelsyml-rdc-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        set_data_dir(d.to_str().unwrap());
        d.to_str().unwrap().to_string()
    }

    fn seed_platformless_channel(name: &str) {
        update_channel(name, |_| {
            Ok(ChannelDef {
                cause: "system".to_string(),
                ..Default::default()
            })
        })
        .expect("seed channel");
    }

    fn write_settings_yml(content: &str) {
        let dir = data_dir().expect("global data dir");
        std::fs::create_dir_all(std::path::Path::new(dir).join("config")).unwrap();
        std::fs::write(
            std::path::Path::new(dir)
                .join("config")
                .join("settings.yml"),
            content,
        )
        .unwrap();
    }

    #[test]
    fn resolve_default_channel_resolution_chain() {
        let _ = ensure_global_data_dir();
        seed_platformless_channel("cron");
        seed_platformless_channel("kanban");

        // No default configured -> None (empty channel -> fail-with-record).
        write_settings_yml("general:\n  x: 1\n");
        assert_eq!(
            resolve_default_channel(None, "default_schedule_channel"),
            None
        );

        // Explicit channel always wins (unknown names too — the caller then
        // fails with "channel not found" instead of substituting a default).
        assert_eq!(
            resolve_default_channel(Some("kanban"), "default_schedule_channel").as_deref(),
            Some("kanban")
        );
        assert_eq!(
            resolve_default_channel(Some("no-such-channel"), "default_schedule_channel").as_deref(),
            Some("no-such-channel")
        );

        // Default settings resolve to known channels.yml names.
        write_settings_yml(
            "channels:\n  default_schedule_channel: cron\n  default_kanban_channel: kanban\n",
        );
        assert_eq!(
            resolve_default_channel(Some("   "), "default_schedule_channel").as_deref(),
            Some("cron"),
            "whitespace explicit falls through to the default setting"
        );
        assert_eq!(
            resolve_default_channel(None, "default_schedule_channel").as_deref(),
            Some("cron")
        );
        assert_eq!(
            resolve_default_channel(None, "default_kanban_channel").as_deref(),
            Some("kanban")
        );

        // A default naming a missing channel -> None (no silent substitution;
        // the thread is created empty and fails with "no channel defined").
        write_settings_yml("channels:\n  default_schedule_channel: missing-channel\n");
        assert_eq!(
            resolve_default_channel(None, "default_schedule_channel"),
            None
        );
        assert_eq!(
            resolve_default_channel(Some("cron"), "default_schedule_channel").as_deref(),
            Some("cron"),
            "explicit still wins over a broken default setting"
        );
    }
}
