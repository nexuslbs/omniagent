//! `tasks.yml` — single source of truth for cron schedules and hooks.
//!
//! Definitions (previously rows in the `cron_jobs` and `hooks` tables) live in
//! `{data_dir}/config/tasks.yml` with two top-level keys:
//!
//! ```yaml
//! schedules:
//!   knowledge_pipeline:
//!     enabled: true
//!     channel: cron            # channel NAME (resolved to channel_id at load)
//!     profile: omni
//!     planning_mode: true      # on/off/None → legacy planning_mode/plan
//!     cron: 0 6 * * *
//!     prompt: Some cron prompt
//! hooks:
//!   after_thread:
//!     enabled: true
//!     channel: my-channel
//!     event: thread_started
//!     scope: channel
//!     target: my-scopped-channel
//!     count: 20
//!     prompt: Some hook prompt here
//! ```
//!
//! The map KEY is the task id (used in `threads.schedule_task_id`, URLs and the
//! `hook_counters` table). YAML field names match the legacy table column
//! names as-is; `cron` is the cron expression (legacy column `schedule`) and
//! `action` maps to the legacy `action_id` (presence implies `mode=action`).
//! Runtime state (last run / next run / created / updated / running) is NOT
//! stored: runs are observable via the threads each task creates
//! (`threads.schedule_task_id` for schedules, `threads.hook_caused` + metadata
//! for hooks). Hook counters are the only runtime state and live in the
//! `hook_counters` table.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::config_path::config_path;
use crate::error::{AppResult, Error};

/// Name of the tasks definition file inside `{data_dir}/config/`.
pub const TASKS_FILE: &str = "tasks.yml";

// ── File structs ────────────────────────────────────────────────────────────

/// Top-level document: `{ schedules: {...}, hooks: {...} }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TasksFile {
    #[serde(default)]
    pub schedules: HashMap<String, ScheduleDef>,
    #[serde(default)]
    pub hooks: HashMap<String, HookDef>,
}

/// One cron schedule definition (legacy `cron_jobs` row).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleDef {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Channel NAME (resolved to channel_id at load; unknown → None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// `true`/`false`/`"on"`/`"off"`/None → legacy planning_mode + plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_mode: Option<PlanningMode>,
    /// The cron expression, 5-field Linux format (legacy column `schedule`).
    pub cron: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Action id — presence implies mode=action (legacy action_id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Comma-separated or JSON-array string of skill names (legacy `skills`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub silent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// One hook definition (legacy `hooks` row).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HookDef {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Channel NAME (resolved to channel_id at load; unknown → None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_mode: Option<PlanningMode>,
    /// Event name: thread_started | thread_finished | new_message.
    pub event: String,
    /// Scope: global | channel | profile (default global).
    #[serde(default = "default_scope")]
    pub scope: String,
    /// Named channel/profile filter (missing = all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Trigger threshold (default 1).
    #[serde(default = "default_count")]
    pub count: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Action id — presence implies mode=action unless `mode` is explicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Explicit mode: agentic | action (optional; derived otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// `planning_mode` accepts a bool (`planning_mode: true`) or a string
/// (`planning_mode: on`) — both map to the legacy planning_mode/plan pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PlanningMode {
    Bool(bool),
    Str(String),
}

fn default_true() -> bool {
    true
}

fn default_scope() -> String {
    "global".to_string()
}

fn default_count() -> i32 {
    1
}

impl Default for ScheduleDef {
    fn default() -> Self {
        Self {
            enabled: true,
            channel: None,
            profile: None,
            planning_mode: None,
            cron: String::new(),
            prompt: None,
            action: None,
            template: None,
            skills: None,
            silent: None,
            display_name: None,
        }
    }
}

impl Default for HookDef {
    fn default() -> Self {
        Self {
            enabled: true,
            channel: None,
            profile: None,
            planning_mode: None,
            event: String::new(),
            scope: "global".to_string(),
            target: None,
            count: 1,
            prompt: None,
            action: None,
            mode: None,
            template: None,
            display_name: None,
        }
    }
}

impl PlanningMode {
    /// Map back to the legacy `(planning_mode TEXT, plan BOOL)` pair.
    pub fn to_legacy(&self) -> (Option<String>, Option<bool>) {
        match self {
            PlanningMode::Bool(true) => (Some("on".to_string()), Some(true)),
            PlanningMode::Bool(false) => (Some("off".to_string()), Some(false)),
            PlanningMode::Str(s) => match s.to_lowercase().as_str() {
                "on" | "true" | "1" | "always" | "auto_plan" | "plan_with_subtasks" => {
                    (Some("on".to_string()), Some(true))
                }
                "off" | "false" | "0" | "never" | "none" => (Some("off".to_string()), Some(false)),
                other => (Some(other.to_string()), None),
            },
        }
    }

    /// Build from the legacy request pair (plan bool wins over planning_mode
    /// string; empty string → None).
    pub fn from_legacy(planning_mode: Option<&str>, plan: Option<bool>) -> Option<Self> {
        if let Some(p) = plan {
            return Some(PlanningMode::Bool(p));
        }
        match planning_mode.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => match s.to_lowercase().as_str() {
                "on" | "true" | "1" | "always" | "auto_plan" | "plan_with_subtasks" => {
                    Some(PlanningMode::Bool(true))
                }
                "off" | "false" | "0" | "never" | "none" => Some(PlanningMode::Bool(false)),
                other => Some(PlanningMode::Str(other.to_string())),
            },
            None => None,
        }
    }
}

impl ScheduleDef {
    /// The legacy `mode` for this schedule: action if `action` is set.
    pub fn mode(&self) -> String {
        if self
            .action
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            "agentic".to_string()
        } else {
            "action".to_string()
        }
    }

    /// Normalized legacy plan flag (planning_mode → plan).
    pub fn plan(&self) -> Option<bool> {
        self.planning_mode.as_ref().and_then(|p| p.to_legacy().1)
    }

    /// Normalized legacy planning_mode string.
    pub fn planning_mode_str(&self) -> Option<String> {
        self.planning_mode.as_ref().and_then(|p| p.to_legacy().0)
    }
}

impl HookDef {
    /// Effective mode: explicit `mode` wins; else action when `action` is set.
    pub fn mode(&self) -> String {
        if let Some(m) = self
            .mode
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            return m.to_string();
        }
        if self
            .action
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            "agentic".to_string()
        } else {
            "action".to_string()
        }
    }

    pub fn plan(&self) -> Option<bool> {
        self.planning_mode.as_ref().and_then(|p| p.to_legacy().1)
    }

    pub fn planning_mode_str(&self) -> Option<String> {
        self.planning_mode.as_ref().and_then(|p| p.to_legacy().0)
    }
}

// ── Path / IO ───────────────────────────────────────────────────────────────

/// `{data_dir}/config/tasks.yml`.
pub fn tasks_path(data_dir: impl AsRef<std::path::Path>) -> PathBuf {
    config_path(data_dir, TASKS_FILE)
}

/// Load tasks.yml. Missing file → empty `TasksFile` (all definitions come
/// from the yml, so an absent file means "no schedules, no hooks").
/// A malformed file is an error (surfaced to API callers).
pub fn load_tasks(data_dir: &str) -> AppResult<TasksFile> {
    let path = tasks_path(data_dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(TasksFile::default()),
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
        return Ok(TasksFile::default());
    }
    serde_yaml::from_str(&content)
        .map_err(|e| Error::Message(format!("Failed to parse {}: {}", path.display(), e)))
}

/// Load tasks.yml, logging + ignoring parse errors (used by background loops:
/// a broken file must not take down the scheduler / hooks engine).
pub fn load_tasks_or_empty(data_dir: &str) -> TasksFile {
    match load_tasks(data_dir) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("[tasks.yml] load failed, treating as empty: {}", e);
            TasksFile::default()
        }
    }
}

/// Atomically persist tasks.yml (tmp file + rename). Creates the config dir
/// idempotently. Validation is the caller's responsibility.
pub fn save_tasks(data_dir: &str, tasks: &TasksFile) -> AppResult<()> {
    crate::config_path::ensure_config_dir(data_dir);
    let path = tasks_path(data_dir);
    let yaml = serde_yaml::to_string(tasks)
        .map_err(|e| Error::Message(format!("Failed to serialize tasks.yml: {}", e)))?;
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

// ── Validation ──────────────────────────────────────────────────────────────

/// Validate a schedule definition (cron expression 5-field format).
pub fn validate_schedule(key: &str, def: &ScheduleDef) -> Result<(), String> {
    if def.cron.trim().is_empty() {
        return Err(format!(
            "schedule '{}': 'cron' is required (5-field Linux format, e.g. '0 9 * * 1-5')",
            key
        ));
    }
    if !crate::scheduler::validate_cron_schedule_5field(&def.cron) {
        return Err(format!(
            "schedule '{}': invalid cron expression '{}': expected 5 fields (min hour dom month dow)",
            key, def.cron
        ));
    }
    Ok(())
}

/// Validate a hook definition (event/scope/mode values + count >= 1), reusing
/// the legacy CHECK-constraint semantics from the hooks table.
pub fn validate_hook(key: &str, def: &HookDef) -> Result<(), String> {
    if !crate::hooks::VALID_EVENTS.contains(&def.event.as_str()) {
        return Err(format!(
            "hook '{}': invalid event '{}': must be one of {:?}",
            key,
            def.event,
            crate::hooks::VALID_EVENTS
        ));
    }
    if !crate::hooks::VALID_SCOPES.contains(&def.scope.as_str()) {
        return Err(format!(
            "hook '{}': invalid scope '{}': must be one of {:?}",
            key,
            def.scope,
            crate::hooks::VALID_SCOPES
        ));
    }
    let mode = def.mode();
    if !crate::hooks::VALID_MODES.contains(&mode.as_str()) {
        return Err(format!(
            "hook '{}': invalid mode '{}': must be one of {:?}",
            key,
            mode,
            crate::hooks::VALID_MODES
        ));
    }
    if def.count < 1 {
        return Err(format!("hook '{}': count must be >= 1", key));
    }
    if mode == "action"
        && def
            .action
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
    {
        return Err(format!(
            "hook '{}': mode=action requires an 'action' id",
            key
        ));
    }
    Ok(())
}

// ── Channel resolution ──────────────────────────────────────────────────────

/// Resolve a channel NAME to its id — since channels now live in
/// channels.yml, the id IS the name (the yml key). Missing/unknown/empty
/// name → `None` (default-channel semantics) — never crashes.
pub async fn resolve_channel_id(_pool: &PgPool, name: Option<&str>) -> Option<String> {
    let name = name.map(str::trim).filter(|n| !n.is_empty())?;
    crate::channels_yaml::exists(name).then(|| name.to_string())
}

/// Resolve a channel id (== name) to its NAME (identity — channels.yml keys
/// are referenced by key string, like schedule_task_id/workflow_id).
/// Unknown/zero id → `None`.
pub async fn channel_name_for_id(_pool: &PgPool, id: Option<String>) -> Option<String> {
    let id = id?;
    if id.is_empty() {
        return None;
    }
    crate::channels_yaml::exists(&id).then_some(id)
}

// ── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_yaml() -> &'static str {
        r#"
schedules:
  knowledge_pipeline:
    enabled: true
    channel: cron
    profile: omni
    planning_mode: true
    cron: 0 6 * * *
    prompt: Run the knowledge pipeline
  nightly:
    cron: 1 6 * * *
    action: some-action
    silent: true
hooks:
  after_thread:
    enabled: true
    channel: my-channel
    profile: omni
    planning_mode: true
    event: thread_started
    scope: channel
    target: my-scopped-channel
    count: 20
    prompt: Some hook prompt here
  message_hook:
    event: new_message
    scope: profile
    count: 100
    action: some-action
"#
    }

    #[test]
    fn parse_full_file() {
        let tasks: TasksFile = serde_yaml::from_str(sample_yaml()).expect("parse");
        assert_eq!(tasks.schedules.len(), 2);
        assert_eq!(tasks.hooks.len(), 2);

        let s = &tasks.schedules["knowledge_pipeline"];
        assert!(s.enabled);
        assert_eq!(s.channel.as_deref(), Some("cron"));
        assert_eq!(s.cron, "0 6 * * *");
        assert_eq!(s.plan(), Some(true));
        assert_eq!(s.planning_mode_str().as_deref(), Some("on"));
        assert_eq!(s.mode(), "agentic");

        let n = &tasks.schedules["nightly"];
        assert_eq!(n.action.as_deref(), Some("some-action"));
        assert_eq!(n.mode(), "action");
        assert_eq!(n.silent, Some(true));
        assert!(n.enabled, "enabled defaults to true");

        let h = &tasks.hooks["after_thread"];
        assert_eq!(h.event, "thread_started");
        assert_eq!(h.scope, "channel");
        assert_eq!(h.target.as_deref(), Some("my-scopped-channel"));
        assert_eq!(h.count, 20);
        assert_eq!(h.mode(), "agentic", "action absent → agentic");

        let m = &tasks.hooks["message_hook"];
        assert_eq!(m.mode(), "action", "action presence implies action mode");
        assert_eq!(m.scope, "profile");
        assert_eq!(m.count, 100);
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("tasksyml-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tasks = load_tasks(dir.to_str().unwrap()).expect("missing file → empty");
        assert!(tasks.schedules.is_empty());
        assert!(tasks.hooks.is_empty());
    }

    #[test]
    fn parse_error_is_reported() {
        let dir = std::env::temp_dir().join(format!("tasksyml-err-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("config"));
        std::fs::write(dir.join("config").join("tasks.yml"), "schedules: [unclosed").unwrap();
        let err = load_tasks(dir.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
        // load_tasks_or_empty swallows it
        let tasks = load_tasks_or_empty(dir.to_str().unwrap());
        assert!(tasks.schedules.is_empty());
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = std::env::temp_dir().join(format!("tasksyml-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut tasks = TasksFile::default();
        tasks.schedules.insert(
            "test".to_string(),
            ScheduleDef {
                cron: "*/5 * * * *".to_string(),
                ..Default::default()
            },
        );
        save_tasks(dir.to_str().unwrap(), &tasks).expect("save");
        let loaded = load_tasks(dir.to_str().unwrap()).expect("reload");
        assert_eq!(loaded.schedules.len(), 1);
        assert_eq!(loaded.schedules["test"].cron, "*/5 * * * *");
        assert!(loaded.schedules["test"].enabled, "enabled defaults true");
        // hooks roundtrip too
        let mut tasks2 = loaded;
        tasks2.hooks.insert(
            "h1".to_string(),
            HookDef {
                event: "thread_finished".to_string(),
                count: 3,
                scope: "global".to_string(),
                ..Default::default()
            },
        );
        save_tasks(dir.to_str().unwrap(), &tasks2).expect("save2");
        let reloaded = load_tasks(dir.to_str().unwrap()).expect("reload2");
        assert_eq!(reloaded.hooks["h1"].count, 3);
        assert_eq!(reloaded.hooks["h1"].mode(), "agentic");
    }

    #[test]
    fn planning_mode_normalization() {
        assert_eq!(
            PlanningMode::Bool(true).to_legacy(),
            (Some("on".to_string()), Some(true))
        );
        assert_eq!(
            PlanningMode::Bool(false).to_legacy(),
            (Some("off".to_string()), Some(false))
        );
        assert_eq!(
            PlanningMode::Str("on".to_string()).to_legacy(),
            (Some("on".to_string()), Some(true))
        );
        assert_eq!(
            PlanningMode::Str("weird".to_string()).to_legacy(),
            (Some("weird".to_string()), None)
        );
        // from legacy: plan wins
        assert_eq!(
            PlanningMode::from_legacy(Some("on"), Some(false)),
            Some(PlanningMode::Bool(false))
        );
        assert_eq!(PlanningMode::from_legacy(None, None), None);
        assert_eq!(
            PlanningMode::from_legacy(Some("auto_plan"), None),
            Some(PlanningMode::Bool(true))
        );
    }

    #[test]
    fn validate_schedule_rules() {
        let ok = ScheduleDef {
            cron: "0 9 * * 1-5".to_string(),
            ..Default::default()
        };
        assert!(validate_schedule("s1", &ok).is_ok());
        let bad = ScheduleDef {
            cron: "0 0 9 * * *".to_string(),
            ..Default::default()
        };
        assert!(validate_schedule("s1", &bad).is_err());
        let empty = ScheduleDef::default();
        assert!(validate_schedule("s1", &empty).is_err());
    }

    #[test]
    fn validate_hook_rules() {
        let ok = HookDef {
            event: "thread_started".to_string(),
            count: 5,
            ..Default::default()
        };
        assert!(validate_hook("h1", &ok).is_ok());
        let bad_event = HookDef {
            event: "bogus".to_string(),
            ..Default::default()
        };
        assert!(validate_hook("h1", &bad_event).is_err());
        let bad_scope = HookDef {
            event: "thread_started".to_string(),
            scope: "bogus".to_string(),
            ..Default::default()
        };
        assert!(validate_hook("h1", &bad_scope).is_err());
        let bad_count = HookDef {
            event: "thread_started".to_string(),
            count: 0,
            ..Default::default()
        };
        assert!(validate_hook("h1", &bad_count).is_err());
        let action_without_id = HookDef {
            event: "thread_started".to_string(),
            mode: Some("action".to_string()),
            ..Default::default()
        };
        assert!(validate_hook("h1", &action_without_id).is_err());
        let action_ok = HookDef {
            event: "thread_started".to_string(),
            action: Some("a1".to_string()),
            ..Default::default()
        };
        assert!(validate_hook("h1", &action_ok).is_ok());
    }

    #[test]
    fn mode_derivation() {
        let s = ScheduleDef {
            action: Some("x".to_string()),
            ..Default::default()
        };
        assert_eq!(s.mode(), "action");
        let s2 = ScheduleDef::default();
        assert_eq!(s2.mode(), "agentic");
        let h = HookDef {
            mode: Some("action".to_string()),
            ..Default::default()
        };
        assert_eq!(h.mode(), "action", "explicit mode wins");
    }
}
