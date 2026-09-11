//! Toolsets: named, reusable tool allow-lists (`{data_dir}/config/toolsets.yml`).
//!
//! A toolset is a set of tool names exactly as they are exposed to the agent
//! (`plugin__tool`). The toolset used to execute a THREAD is the FIRST DEFINED
//! one on this chain (highest priority first):
//!
//! ```text
//! workflow_role > workflow > task > channel > profile
//! ```
//!
//! * no level defines a toolset  -> ALL tools are allowed (`None`);
//! * the highest-priority DEFINED level wins outright - this is NOT an
//!   intersection with the lower levels;
//! * an EMPTY list is a valid toolset and means "no tool at all".
//!
//! The resolved toolset id is persisted on the thread (`threads.toolset`) at
//! thread-creation time, exactly like `threads.template`, so the executor
//! never re-resolves the chain. A toolset id that is NOT defined in
//! `config/toolsets.yml` is a hard error: the thread ends `failed` with a
//! message naming the toolset id and the level that defined it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Canonical file name of the toolsets document inside `{data_dir}/config/`.
pub const TOOLSETS_FILE: &str = "toolsets.yml";

/// The `toolsets.yml` document: a map of toolset id -> exposed tool names.
///
/// ```yaml
/// toolsets:
///   toolset_1_empty: []
///   toolset_2: [my_plugin_1__tool_1, my_plugin_2__tool_1]
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsetsFile {
    /// Toolset id -> tool names. An empty list is valid (no tool allowed).
    pub toolsets: BTreeMap<String, Vec<String>>,
}

/// Canonical path to `{data_dir}/config/toolsets.yml`.
pub fn toolsets_path(data_dir: impl AsRef<Path>) -> PathBuf {
    crate::config_path::config_path(data_dir, TOOLSETS_FILE)
}

impl ToolsetsFile {
    /// Parse and validate a `toolsets.yml` document.
    pub fn from_yaml(yaml: &str) -> Result<Self, ToolsetsConfigError> {
        let file: ToolsetsFile = serde_yaml::from_str(yaml)?;
        file.validate()?;
        Ok(file)
    }

    /// Load, parse and validate `toolsets.yml` from disk.
    pub fn load(path: &Path) -> Result<Self, ToolsetsConfigError> {
        if !path.exists() {
            return Err(ToolsetsConfigError::NotFound {
                path: path.to_path_buf(),
            });
        }
        Self::from_yaml(&std::fs::read_to_string(path)?)
    }

    /// Load `{data_dir}/config/toolsets.yml`, treating a missing file as an
    /// EMPTY document (a fresh install defines no toolset: every thread then
    /// resolves to "all tools allowed").
    pub fn load_or_empty(data_dir: &str) -> Result<Self, ToolsetsConfigError> {
        match Self::load(&toolsets_path(data_dir)) {
            Ok(file) => Ok(file),
            Err(ToolsetsConfigError::NotFound { .. }) => Ok(Self::default()),
            Err(err) => Err(err),
        }
    }

    /// Structural validation: toolset ids must be non-blank and so must every
    /// tool name (a blank name would silently never match a real tool).
    /// Duplicate tool names inside one toolset are de-duplicated on write.
    pub fn validate(&self) -> Result<(), ToolsetsConfigError> {
        for (key, tools) in &self.toolsets {
            if key.trim().is_empty() {
                return Err(ToolsetsConfigError::Invalid {
                    key: key.clone(),
                    message: "toolset id must not be blank".to_string(),
                });
            }
            if key.trim() != key {
                return Err(ToolsetsConfigError::Invalid {
                    key: key.clone(),
                    message: "toolset id must not have leading/trailing whitespace".to_string(),
                });
            }
            if let Some(bad) = tools.iter().find(|tool| tool.trim().is_empty()) {
                return Err(ToolsetsConfigError::Invalid {
                    key: key.clone(),
                    message: format!("tool name {bad:?} must not be blank"),
                });
            }
        }
        Ok(())
    }

    /// Serialize this document back to YAML text.
    pub fn to_yaml(&self) -> Result<String, ToolsetsConfigError> {
        serde_yaml::to_string(self).map_err(ToolsetsConfigError::Yaml)
    }

    /// Atomically persist this document to `path` (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<(), ToolsetsConfigError> {
        self.validate()?;
        let yaml = self.to_yaml()?;
        let dir = path.parent().ok_or_else(|| {
            ToolsetsConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "toolsets path has no parent directory",
            ))
        })?;
        std::fs::create_dir_all(dir).map_err(ToolsetsConfigError::Io)?;
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| TOOLSETS_FILE.to_string());
        let tmp_path = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));
        std::fs::write(&tmp_path, yaml.as_bytes()).map_err(ToolsetsConfigError::Io)?;
        std::fs::rename(&tmp_path, path).map_err(ToolsetsConfigError::Io)?;
        Ok(())
    }

    /// All defined toolset ids, sorted (BTreeMap order).
    pub fn ids(&self) -> Vec<String> {
        self.toolsets.keys().cloned().collect()
    }

    /// The tools of one toolset; `None` when the id is not defined.
    pub fn get(&self, id: &str) -> Option<&Vec<String>> {
        self.toolsets.get(id)
    }

    /// Whether `id` is defined in this document.
    pub fn contains(&self, id: &str) -> bool {
        self.toolsets.contains_key(id)
    }

    /// Insert or replace a toolset (de-duplicating its tool names), then
    /// validate the whole document.
    pub fn upsert(&mut self, id: &str, tools: Vec<String>) -> Result<(), ToolsetsConfigError> {
        let mut seen: Vec<String> = Vec::new();
        for tool in tools {
            if !seen.iter().any(|existing| existing == &tool) {
                seen.push(tool);
            }
        }
        self.toolsets.insert(id.to_string(), seen);
        self.validate()
    }

    /// Remove a toolset by id; returns the removed tool list, if any.
    pub fn remove(&mut self, id: &str) -> Option<Vec<String>> {
        self.toolsets.remove(id)
    }
}

/// Errors produced while loading/parsing/validating `toolsets.yml`.
#[derive(Debug)]
pub enum ToolsetsConfigError {
    /// YAML syntax / type error.
    Yaml(serde_yaml::Error),
    /// Structural validation failure for a specific toolset.
    Invalid { key: String, message: String },
    /// The toolsets.yml file does not exist.
    NotFound { path: PathBuf },
    /// Failed to read/write the file.
    Io(std::io::Error),
}

impl std::fmt::Display for ToolsetsConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolsetsConfigError::Yaml(err) => write!(f, "invalid toolsets.yml: {err}"),
            ToolsetsConfigError::Invalid { key, message } => {
                write!(f, "toolset '{key}': {message}")
            }
            ToolsetsConfigError::NotFound { path } => {
                write!(f, "toolsets.yml not found at {}", path.display())
            }
            ToolsetsConfigError::Io(err) => write!(f, "failed to read toolsets.yml: {err}"),
        }
    }
}

impl std::error::Error for ToolsetsConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ToolsetsConfigError::Yaml(err) => Some(err),
            ToolsetsConfigError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<serde_yaml::Error> for ToolsetsConfigError {
    fn from(err: serde_yaml::Error) -> Self {
        ToolsetsConfigError::Yaml(err)
    }
}

impl From<std::io::Error> for ToolsetsConfigError {
    fn from(err: std::io::Error) -> Self {
        ToolsetsConfigError::Io(err)
    }
}

// ── Resolution ──────────────────────────────────────────────────────────────

/// One level of the toolset resolution chain, highest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolsetLevel {
    /// The workflow ROLE running this thread step (executor/tester/reviewer).
    WorkflowRole,
    /// The workflow itself.
    Workflow,
    /// The kanban task / schedule entry / hook task that caused the thread.
    Task,
    /// The channel the thread runs in.
    Channel,
    /// The profile the thread runs as.
    Profile,
}

impl ToolsetLevel {
    /// Stable machine name (used in thread metadata and API payloads).
    pub fn as_str(self) -> &'static str {
        match self {
            ToolsetLevel::WorkflowRole => "workflow_role",
            ToolsetLevel::Workflow => "workflow",
            ToolsetLevel::Task => "task",
            ToolsetLevel::Channel => "channel",
            ToolsetLevel::Profile => "profile",
        }
    }

    /// Human phrase for error messages, e.g. `workflow role 'executor'`.
    pub fn describe(self, owner: &str) -> String {
        let owner = owner.trim();
        match self {
            ToolsetLevel::WorkflowRole => format!("workflow role '{owner}'"),
            ToolsetLevel::Workflow => format!("workflow '{owner}'"),
            ToolsetLevel::Task => format!("task '{owner}'"),
            ToolsetLevel::Channel => format!("channel '{owner}'"),
            ToolsetLevel::Profile => format!("profile '{owner}'"),
        }
    }
}

/// A resolved toolset id plus the level/owner that defined it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolsetRef {
    pub id: String,
    pub level: ToolsetLevel,
    pub owner: String,
}

impl ToolsetRef {
    /// `toolset 'foo' defined by workflow role 'executor'`.
    pub fn describe(&self) -> String {
        format!(
            "toolset '{}' defined by {}",
            self.id,
            self.level.describe(&self.owner)
        )
    }
}

/// The candidate toolset ids gathered from every level.
///
/// Each field is `Some((owner, Some(id)))` when that level DEFINES a toolset
/// (owner = the workflow id / role key / task id / channel name / profile
/// name), and `None` when the level defines nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolsetLevels {
    pub workflow_role: Option<(String, Option<String>)>,
    pub workflow: Option<(String, Option<String>)>,
    pub task: Option<(String, Option<String>)>,
    pub channel: Option<(String, Option<String>)>,
    pub profile: Option<(String, Option<String>)>,
}

impl ToolsetLevels {
    /// Every level in priority order.
    #[allow(clippy::type_complexity)]
    fn ordered(&self) -> [(ToolsetLevel, &Option<(String, Option<String>)>); 5] {
        [
            (ToolsetLevel::WorkflowRole, &self.workflow_role),
            (ToolsetLevel::Workflow, &self.workflow),
            (ToolsetLevel::Task, &self.task),
            (ToolsetLevel::Channel, &self.channel),
            (ToolsetLevel::Profile, &self.profile),
        ]
    }
}

/// First-match resolution over the five levels.
///
/// Returns the highest-priority DEFINED toolset. A blank id counts as
/// undefined (defensive: an empty YAML scalar must never mean "no tools").
pub fn resolve(levels: &ToolsetLevels) -> Option<ToolsetRef> {
    for (level, candidate) in levels.ordered() {
        if let Some((owner, Some(id))) = candidate {
            let id = id.trim();
            if !id.is_empty() {
                return Some(ToolsetRef {
                    id: id.to_string(),
                    level,
                    owner: owner.clone(),
                });
            }
        }
    }
    None
}

// ── Sources ─────────────────────────────────────────────────────────────────

/// Gather the toolset ids the standard sources define for a thread.
///
/// * `profile` / `channel_id` / `workflow_id` are loaded from the config
///   files (`profiles.yml`, `channels.yml`, `workflows.yml`);
/// * `task` is the value the caller resolved from its own store (kanban task
///   column, `tasks.yml` schedule entry, hook definition) together with the
///   owner label used in error messages;
/// * `workflow_step` selects the workflow ROLE (executor/tester/reviewer).
///
/// A missing or unreadable config file degrades to "this level defines
/// nothing" - broken OPTIONAL config never takes tools away from a thread.
#[derive(Debug, Clone, Default)]
pub struct ThreadToolsetSources<'a> {
    pub data_dir: &'a str,
    pub profile: Option<&'a str>,
    pub channel_id: Option<&'a str>,
    /// `(owner label, toolset id)` for the task / schedule / hook level.
    pub task: Option<(String, Option<String>)>,
    pub workflow_id: Option<&'a str>,
    pub workflow_step: Option<&'a str>,
}

/// Resolve the toolset for a thread from the standard sources.
pub fn resolve_for_thread(sources: &ThreadToolsetSources<'_>) -> Option<ToolsetRef> {
    let levels = ToolsetLevels {
        workflow_role: sources
            .workflow_id
            .and_then(|wf_id| workflow_role_level(sources.data_dir, wf_id, sources.workflow_step)),
        workflow: sources
            .workflow_id
            .and_then(|wf_id| workflow_level(sources.data_dir, wf_id)),
        task: sources.task.clone(),
        channel: sources
            .channel_id
            .and_then(|cid| channel_level(sources.data_dir, cid)),
        profile: sources
            .profile
            .and_then(|name| profile_level(sources.data_dir, name)),
    };
    resolve(&levels)
}

/// `<workflow role of step>.toolset` for the workflow; `None` when the step is
/// not a workflow role or the role defines no toolset.
pub fn workflow_role_level(
    data_dir: &str,
    workflow_id: &str,
    workflow_step: Option<&str>,
) -> Option<(String, Option<String>)> {
    let role_key = crate::workflows::role_for_step(workflow_step?)?;
    let workflow = crate::workflows::WorkflowsFile::load_workflow(data_dir, workflow_id).ok()??;
    let role = workflow.roles.get(role_key)?;
    Some((role_key.to_string(), role.toolset.clone()))
}

/// `Workflow.toolset` for the workflow.
pub fn workflow_level(data_dir: &str, workflow_id: &str) -> Option<(String, Option<String>)> {
    let workflow = crate::workflows::WorkflowsFile::load_workflow(data_dir, workflow_id).ok()??;
    Some((workflow_id.to_string(), workflow.toolset.clone()))
}

/// Channel-level toolset from `channels.yml`.
pub fn channel_level(data_dir: &str, channel_id: &str) -> Option<(String, Option<String>)> {
    let file = crate::channels_yaml::load_channels_from(data_dir).ok()?;
    let def = file.channels.get(channel_id)?;
    Some((channel_id.to_string(), def.toolset.clone()))
}

/// Profile-level toolset from `profiles.yml`.
pub fn profile_level(data_dir: &str, profile: &str) -> Option<(String, Option<String>)> {
    let registry = crate::profile::ProfileRegistry::new(data_dir);
    let p = registry.get(profile)?;
    Some((profile.to_string(), p.toolset.clone()))
}

// ── Execution-time lookup ───────────────────────────────────────────────────

/// Load the toolset id -> tool list map. A missing file is an empty map.
pub fn load_map(data_dir: &str) -> BTreeMap<String, Vec<String>> {
    ToolsetsFile::load_or_empty(data_dir)
        .map(|f| f.toolsets)
        .unwrap_or_default()
}

/// The tools allowed by a toolset id.
///
/// * `Ok(tools)` - the id is defined; only these tools are allowed (an empty
///   vector means no tool at all).
/// * `Err(message)` - the id is NOT defined in `config/toolsets.yml`.
pub fn tools_for(data_dir: &str, toolset_id: &str) -> Result<Vec<String>, String> {
    load_map(data_dir)
        .get(toolset_id.trim())
        .cloned()
        .ok_or_else(|| not_defined_message(data_dir, toolset_id.trim(), None))
}

/// Effective tool allow-list of a thread.
///
/// * `Ok(None)` - the thread defines no toolset: ALL tools are allowed.
/// * `Ok(Some(tools))` - only these tools (possibly none).
/// * `Err(message)` - the thread's toolset id is not defined; the caller MUST
///   end the thread as `failed` with this message.
pub fn thread_tools(
    data_dir: &str,
    toolset_id: Option<&str>,
) -> Result<Option<Vec<String>>, String> {
    match toolset_id.map(str::trim).filter(|id| !id.is_empty()) {
        None => Ok(None),
        Some(id) => tools_for(data_dir, id).map(Some),
    }
}

/// Validate a resolved toolset reference against `config/toolsets.yml`.
pub fn check_ref(data_dir: &str, resolved: &ToolsetRef) -> Result<Vec<String>, String> {
    load_map(data_dir)
        .get(resolved.id.trim())
        .cloned()
        .ok_or_else(|| not_defined_message(data_dir, resolved.id.trim(), Some(resolved.describe())))
}

/// The explicit, operator-facing error for an undefined toolset id.
fn not_defined_message(data_dir: &str, id: &str, describe: Option<String>) -> String {
    let path = toolsets_path(data_dir);
    match describe {
        Some(describe) => format!(
            "{describe} is not defined in config/toolsets.yml ({}): \
             the thread cannot run, add the toolset or clear the field",
            path.display()
        ),
        None => format!(
            "toolset '{id}' is not defined in config/toolsets.yml ({}): \
             the thread cannot run, add the toolset or clear the field",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn level(owner: &str, id: Option<&str>) -> Option<(String, Option<String>)> {
        Some((owner.to_string(), id.map(|s| s.to_string())))
    }

    fn write_toolsets(dir: &Path, yaml: &str) {
        std::fs::create_dir_all(dir.join("config")).expect("config dir");
        std::fs::write(dir.join("config").join(TOOLSETS_FILE), yaml).expect("write toolsets.yml");
    }

    const SAMPLE: &str = "\
toolsets:
  empty_set: []
  two_tools: [p1__t1, p2__t1]
";

    #[test]
    fn parse_and_round_trip() {
        let file = ToolsetsFile::from_yaml(SAMPLE).expect("parse");
        assert_eq!(file.ids(), vec!["empty_set", "two_tools"]);
        assert_eq!(file.get("empty_set"), Some(&Vec::<String>::new()));
        assert_eq!(file.get("two_tools").unwrap().len(), 2);
        let yaml = file.to_yaml().expect("serialize");
        let again = ToolsetsFile::from_yaml(&yaml).expect("reparse");
        assert_eq!(again, file);
    }

    #[test]
    fn validate_rejects_blank_id_and_blank_tool() {
        let blank_id = ToolsetsFile::from_yaml("toolsets:\n  \"\": []\n");
        assert!(blank_id.is_err());
        let blank_tool = ToolsetsFile::from_yaml("toolsets:\n  a: [\"  \"]\n");
        assert!(blank_tool.is_err());
    }

    #[test]
    fn upsert_dedupes_and_remove() {
        let mut file = ToolsetsFile::default();
        file.upsert("a", ids(&["x", "x", "y"])).expect("upsert");
        assert_eq!(file.get("a"), Some(&ids(&["x", "y"])));
        assert_eq!(file.remove("a"), Some(ids(&["x", "y"])));
        assert!(!file.contains("a"));
        assert!(file.remove("a").is_none());
    }

    #[test]
    fn resolve_matrix_priority_order() {
        // Nothing defined anywhere -> None (all tools allowed).
        assert_eq!(resolve(&ToolsetLevels::default()), None);

        // Each level alone wins when it is the only one defined.
        let base = ToolsetLevels::default();
        let expect = |levels: &ToolsetLevels, id: &str, lvl: ToolsetLevel, owner: &str| {
            let r = resolve(levels).expect("resolved");
            assert_eq!(r.id, id);
            assert_eq!(r.level, lvl);
            assert_eq!(r.owner, owner);
        };

        let mut l = base.clone();
        l.profile = level("omni", Some("p_set"));
        expect(&l, "p_set", ToolsetLevel::Profile, "omni");

        let mut l = base.clone();
        l.channel = level("telegram", Some("c_set"));
        l.profile = level("omni", Some("p_set"));
        expect(&l, "c_set", ToolsetLevel::Channel, "telegram");

        let mut l = base.clone();
        l.task = level("task_1", Some("t_set"));
        l.channel = level("telegram", Some("c_set"));
        l.profile = level("omni", Some("p_set"));
        expect(&l, "t_set", ToolsetLevel::Task, "task_1");

        let mut l = base.clone();
        l.workflow = level("wf", Some("w_set"));
        l.task = level("task_1", Some("t_set"));
        l.channel = level("telegram", Some("c_set"));
        l.profile = level("omni", Some("p_set"));
        expect(&l, "w_set", ToolsetLevel::Workflow, "wf");

        let mut l = base.clone();
        l.workflow_role = level("executor", Some("r_set"));
        l.workflow = level("wf", Some("w_set"));
        l.task = level("task_1", Some("t_set"));
        l.channel = level("telegram", Some("c_set"));
        l.profile = level("omni", Some("p_set"));
        expect(&l, "r_set", ToolsetLevel::WorkflowRole, "executor");

        // A level that is PRESENT but UNDEFINED (None id) never wins.
        let mut l = base.clone();
        l.workflow_role = level("executor", None);
        l.profile = level("omni", Some("p_set"));
        expect(&l, "p_set", ToolsetLevel::Profile, "omni");

        // Blank ids are treated as undefined.
        let mut l = base.clone();
        l.workflow = level("wf", Some("  "));
        l.profile = level("omni", Some("p_set"));
        expect(&l, "p_set", ToolsetLevel::Profile, "omni");
    }

    #[test]
    fn resolve_keeps_empty_list_toolset() {
        let l = ToolsetLevels {
            workflow_role: level("executor", Some("empty_set")),
            ..Default::default()
        };
        let r = resolve(&l).expect("resolved");
        assert_eq!(r.id, "empty_set");
        assert_eq!(r.level, ToolsetLevel::WorkflowRole);
    }

    #[test]
    fn thread_tools_none_allowed_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_string_lossy().to_string();
        write_toolsets(dir.path(), SAMPLE);
        assert_eq!(thread_tools(&data_dir, None), Ok(None));
        assert_eq!(thread_tools(&data_dir, Some("")), Ok(None));
    }

    #[test]
    fn thread_tools_empty_list_is_no_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_string_lossy().to_string();
        write_toolsets(dir.path(), SAMPLE);
        assert_eq!(thread_tools(&data_dir, Some("empty_set")), Ok(Some(vec![])));
        assert_eq!(
            thread_tools(&data_dir, Some("two_tools")),
            Ok(Some(ids(&["p1__t1", "p2__t1"])))
        );
    }

    #[test]
    fn thread_tools_undefined_id_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_string_lossy().to_string();
        write_toolsets(dir.path(), SAMPLE);
        let err = thread_tools(&data_dir, Some("nope")).expect_err("must fail");
        assert!(err.contains("toolset 'nope'"), "message: {err}");
        assert!(err.contains(TOOLSETS_FILE), "message: {err}");
    }

    #[test]
    fn missing_file_means_every_id_is_undefined() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_string_lossy().to_string();
        assert_eq!(thread_tools(&data_dir, None), Ok(None));
        assert!(thread_tools(&data_dir, Some("anything")).is_err());
    }

    #[test]
    fn check_ref_names_the_defining_level() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_string_lossy().to_string();
        write_toolsets(dir.path(), SAMPLE);
        let r = ToolsetRef {
            id: "foo".to_string(),
            level: ToolsetLevel::WorkflowRole,
            owner: "executor".to_string(),
        };
        let err = check_ref(&data_dir, &r).expect_err("must fail");
        assert!(
            err.starts_with("toolset 'foo' defined by workflow role 'executor' is not defined"),
            "message: {err}"
        );
        let ok = ToolsetRef {
            id: "two_tools".to_string(),
            level: ToolsetLevel::Profile,
            owner: "omni".to_string(),
        };
        assert_eq!(check_ref(&data_dir, &ok), Ok(ids(&["p1__t1", "p2__t1"])));
    }

    #[test]
    fn save_is_atomic_and_reloadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = toolsets_path(dir.path().to_string_lossy().to_string());
        let mut file = ToolsetsFile::default();
        file.upsert("a", ids(&["x"])).expect("upsert");
        file.save(&path).expect("save");
        let loaded = ToolsetsFile::load(&path).expect("load");
        assert_eq!(loaded, file);
    }
}
