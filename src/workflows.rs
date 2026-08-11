//! Workflow configuration: parsing + validation of `workflows.yml` (Phase 0).
//!
//! Workflows are FILE-DEFINED (decisions N4/R9): the YAML file at `<OMNI_DIR>/workflows.yml`
//! is the single source of truth — there are NO `workflows` / `workflow_roles` DB tables.
//! This module implements Phase 0 only: parse + validate. The Workflows dashboard CRUD
//! that writes the file is Phase 5; the execution engine that consumes the parsed config
//! is Phase 1+.
//!
//! File structure (wiki `WorkflowImplementation.md` §4):
//!
//! ```yaml
//! workflows:
//!   weekly-report:              # key == id == name (no separate name field)
//!     profile: report-profile   # optional workflow-level defaults
//!     provider: anthropic
//!     model: claude-sonnet-4-5
//!     plan_mode: auto_plan
//!     retries: 2
//!     roles:
//!       executor:               # REQUIRED role; template OPTIONAL
//!         template: |
//!           You are the executor...
//!       tester:                 # optional role; template REQUIRED when present
//!         template: |
//!           You are the tester...
//!       reviewer:               # optional role; template REQUIRED when present
//!         template: |
//!           You are the reviewer...
//! ```
//!
//! Validation rules (Phase 0):
//! - workflow keys must be non-empty (`key == id == name`);
//! - role keys must be exactly one of `executor` | `tester` | `reviewer`;
//! - the `executor` role is required;
//! - `tester` / `reviewer` templates are required when the role is present
//!   (the `executor` template is optional).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Role keys. Roles are role/display names only — they must NEVER be used as
/// thread step keys (see [`STEP_KEYS`]).
pub const EXECUTOR_ROLE: &str = "executor";
pub const TESTER_ROLE: &str = "tester";
pub const REVIEWER_ROLE: &str = "reviewer";
pub const ROLE_KEYS: [&str; 3] = [EXECUTOR_ROLE, TESTER_ROLE, REVIEWER_ROLE];

/// Thread step keys (`threads.workflow_step`): 'running' | 'testing' | 'review'.
/// Step keys only — NEVER role names (N5).
pub const STEP_KEYS: [&str; 3] = ["running", "testing", "review"];

/// Map a thread step key (`running`/`testing`/`review`) to its workflow role
/// key. Step keys and role keys are intentionally distinct (N5); returns
/// `None` for anything that is not a workflow step.
pub fn role_for_step(step: &str) -> Option<&'static str> {
    match step {
        "running" => Some(EXECUTOR_ROLE),
        "testing" => Some(TESTER_ROLE),
        "review" => Some(REVIEWER_ROLE),
        _ => None,
    }
}

/// Workflow-level (or per-role override) execution defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct WorkflowDefaults {
    pub profile: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub plan_mode: Option<String>,
    pub retries: Option<u32>,
}

/// A single role inside a workflow. `template` is the system prompt the role
/// runs with; any other fields override the workflow-level defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct WorkflowRole {
    pub template: Option<String>,
    #[serde(flatten)]
    pub overrides: WorkflowDefaults,
}

/// One workflow definition. The map key is the workflow id/name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct Workflow {
    #[serde(flatten)]
    pub defaults: WorkflowDefaults,

    /// If true, execution counters (`workflow_state.executions`) are cleared
    /// when the task moves to review. Default: false.
    pub clear_executions_on_review: bool,
    pub roles: BTreeMap<String, WorkflowRole>,
}

/// Parsed `workflows.yml` (root document).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct WorkflowsFile {
    pub workflows: BTreeMap<String, Workflow>,
}

impl WorkflowsFile {
    /// Parse and validate a `workflows.yml` document.
    pub fn from_yaml(yaml: &str) -> Result<Self, WorkflowConfigError> {
        let file: WorkflowsFile = serde_yaml::from_str(yaml)?;
        file.validate()?;
        Ok(file)
    }

    /// Load, parse and validate `workflows.yml` from disk.
    pub fn load(path: &Path) -> Result<Self, WorkflowConfigError> {
        if !path.exists() {
            return Err(WorkflowConfigError::NotFound {
                path: path.to_path_buf(),
            });
        }
        Self::from_yaml(&std::fs::read_to_string(path)?)
    }

    /// Load a single workflow definition by id from `<data_dir>/workflows.yml`.
    ///
    /// A missing file or an unknown workflow id yields `Ok(None)` (treated as
    /// "no workflow"); parse/validation errors are returned — never silently
    /// swallowed, so callers can decide whether to degrade or fail.
    pub fn load_workflow(data_dir: &str, id: &str) -> Result<Option<Workflow>, WorkflowConfigError> {
        if id.trim().is_empty() {
            return Ok(None);
        }
        let path = Path::new(data_dir).join("workflows.yml");
        match Self::load(&path) {
            Ok(file) => Ok(file.workflows.get(id).cloned()),
            Err(WorkflowConfigError::NotFound { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Validate structural constraints. See module docs for the rules.
    pub fn validate(&self) -> Result<(), WorkflowConfigError> {
        for (key, workflow) in &self.workflows {
            if key.trim().is_empty() {
                return Err(WorkflowConfigError::Invalid {
                    key: key.clone(),
                    message: "workflow key must be non-empty (key == id == name)".into(),
                });
            }
            for role_key in workflow.roles.keys() {
                if !ROLE_KEYS.contains(&role_key.as_str()) {
                    return Err(WorkflowConfigError::Invalid {
                        key: key.clone(),
                        message: format!(
                            "unknown role '{role_key}' (expected {})",
                            ROLE_KEYS.join(" | ")
                        ),
                    });
                }
            }
            if !workflow.roles.contains_key(EXECUTOR_ROLE) {
                return Err(WorkflowConfigError::Invalid {
                    key: key.clone(),
                    message: format!("role '{EXECUTOR_ROLE}' is required"),
                });
            }
            for role in [TESTER_ROLE, REVIEWER_ROLE] {
                if let Some(role_def) = workflow.roles.get(role) {
                    let template = role_def.template.as_deref().unwrap_or("").trim();
                    if template.is_empty() {
                        return Err(WorkflowConfigError::Invalid {
                            key: key.clone(),
                            message: format!("role '{role}' requires a non-empty template"),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Serialize this document back to YAML text.
    pub fn to_yaml(&self) -> Result<String, WorkflowConfigError> {
        serde_yaml::to_string(self).map_err(WorkflowConfigError::Yaml)
    }

    /// Atomically persist this document to `path` (write a temp file, then rename).
    pub fn save(&self, path: &Path) -> Result<(), WorkflowConfigError> {
        let yaml = self.to_yaml()?;
        let dir = path.parent().ok_or_else(|| {
            WorkflowConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workflows path has no parent directory",
            ))
        })?;
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "workflows.yml".to_string());
        let tmp_path = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));
        std::fs::write(&tmp_path, yaml.as_bytes()).map_err(WorkflowConfigError::Io)?;
        std::fs::rename(&tmp_path, path).map_err(WorkflowConfigError::Io)?;
        Ok(())
    }

    /// Insert or replace a workflow, then validate the whole document.
    pub fn upsert(&mut self, key: &str, workflow: Workflow) -> Result<(), WorkflowConfigError> {
        self.workflows.insert(key.to_string(), workflow);
        self.validate()
    }

    /// Remove a workflow by key; returns the removed definition, if any.
    pub fn remove(&mut self, key: &str) -> Option<Workflow> {
        self.workflows.remove(key)
    }

    /// Resolve every workflow's effective per-role settings
    /// (workflow_role > workflow_field).
    pub fn resolve_all(&self) -> Vec<(String, Workflow, BTreeMap<String, ResolvedWorkflowRole>)> {
        self.workflows
            .iter()
            .map(|(key, workflow)| {
                let roles = workflow
                    .roles
                    .keys()
                    .filter_map(|role_key| {
                        workflow
                            .resolve_role(role_key)
                            .map(|resolved| (role_key.clone(), resolved))
                    })
                    .collect();
                (key.clone(), workflow.clone(), roles)
            })
            .collect()
    }
}

/// Effective per-role settings after precedence resolution.
///
/// Precedence is: workflow_role > workflow_field > task_field >
/// channel_field > global_setting. This struct captures the top two tiers
/// (role-level overrides and workflow-level defaults); the remaining tiers
/// are resolved at runtime by the executor when a task is dispatched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ResolvedWorkflowRole {
    pub template: Option<String>,
    pub profile: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub plan_mode: Option<String>,
    pub retries: Option<u32>,
}

impl Workflow {
    /// Resolve the effective settings for one role.
    ///
    /// Role-level overrides take precedence over workflow-level defaults
    /// (workflow_role > workflow_field). Returns `None` when the role is not
    /// defined on the workflow.
    pub fn resolve_role(&self, role_key: &str) -> Option<ResolvedWorkflowRole> {
        let role = self.roles.get(role_key)?;
        Some(ResolvedWorkflowRole {
            template: role.template.clone(),
            profile: role
                .overrides
                .profile
                .clone()
                .or_else(|| self.defaults.profile.clone()),
            provider: role
                .overrides
                .provider
                .clone()
                .or_else(|| self.defaults.provider.clone()),
            model: role
                .overrides
                .model
                .clone()
                .or_else(|| self.defaults.model.clone()),
            plan_mode: role
                .overrides
                .plan_mode
                .clone()
                .or_else(|| self.defaults.plan_mode.clone()),
            retries: role.overrides.retries.or(self.defaults.retries),
        })
    }
}

/// Errors produced while loading/parsing/validating `workflows.yml`.
#[derive(Debug)]
pub enum WorkflowConfigError {
    /// YAML syntax / type error.
    Yaml(serde_yaml::Error),
    /// Structural validation failure for a specific workflow.
    Invalid { key: String, message: String },
    /// The workflows.yml file does not exist.
    NotFound { path: PathBuf },
    /// Failed to read the file from disk.
    Io(std::io::Error),
}

impl std::fmt::Display for WorkflowConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowConfigError::Yaml(err) => write!(f, "invalid workflows.yml: {err}"),
            WorkflowConfigError::Invalid { key, message } => {
                write!(f, "workflow '{key}': {message}")
            }
            WorkflowConfigError::NotFound { path } => {
                write!(f, "workflows.yml not found at {}", path.display())
            }
            WorkflowConfigError::Io(err) => write!(f, "failed to read workflows.yml: {err}"),
        }
    }
}

impl std::error::Error for WorkflowConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WorkflowConfigError::Yaml(err) => Some(err),
            WorkflowConfigError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<serde_yaml::Error> for WorkflowConfigError {
    fn from(err: serde_yaml::Error) -> Self {
        WorkflowConfigError::Yaml(err)
    }
}

impl From<std::io::Error> for WorkflowConfigError {
    fn from(err: std::io::Error) -> Self {
        WorkflowConfigError::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_defaults() -> WorkflowDefaults {
        WorkflowDefaults {
            profile: None,
            provider: None,
            model: None,
            plan_mode: None,
            retries: None,
        }
    }

    #[test]
    fn test_save_and_reload_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workflows.yml");
        let file = WorkflowsFile::from_yaml(VALID_YAML).expect("valid yaml");
        file.save(&path).expect("save");
        assert!(path.exists(), "file should exist after save");
        let loaded = WorkflowsFile::load(&path).expect("reload");
        assert_eq!(loaded, file);
    }

    #[test]
    fn test_save_is_atomic_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workflows.yml");
        let file = WorkflowsFile::from_yaml(VALID_YAML).expect("valid yaml");
        file.save(&path).expect("save");
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(entries, vec!["workflows.yml".to_string()]);
    }

    #[test]
    fn test_upsert_rejects_missing_executor_role() {
        let mut file = WorkflowsFile::default();
        let wf = Workflow {
            defaults: empty_defaults(),
            clear_executions_on_review: false,
            roles: BTreeMap::new(),
        };
        let err = file.upsert("no-executor", wf).expect_err("must fail");
        assert!(
            err.to_string().contains("executor"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_upsert_rejects_tester_without_template() {
        let mut file = WorkflowsFile::default();
        let mut wf = Workflow {
            defaults: empty_defaults(),
            clear_executions_on_review: false,
            roles: BTreeMap::new(),
        };
        wf.roles.insert(
            TESTER_ROLE.to_string(),
            WorkflowRole {
                template: None,
                overrides: empty_defaults(),
            },
        );
        let err = file.upsert("no-template", wf).expect_err("must fail");
        assert!(
            err.to_string().contains("template"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_remove_key() {
        let mut file = WorkflowsFile::from_yaml(VALID_YAML).expect("valid yaml");
        assert!(file.remove("weekly-report").is_some());
        assert!(file.remove("weekly-report").is_none());
        assert!(file.workflows.is_empty());
    }

    #[test]
    fn test_resolve_role_precedence() {
        let mut wf = Workflow {
            defaults: empty_defaults(),
            clear_executions_on_review: false,
            roles: BTreeMap::new(),
        };
        wf.defaults.profile = Some("wf-profile".to_string());
        wf.defaults.provider = Some("wf-provider".to_string());
        wf.defaults.retries = Some(3);
        wf.roles.insert(
            EXECUTOR_ROLE.to_string(),
            WorkflowRole {
                template: Some("executor-template".to_string()),
                overrides: WorkflowDefaults {
                    profile: Some("role-profile".to_string()),
                    ..empty_defaults()
                },
            },
        );
        let resolved = wf
            .resolve_role(EXECUTOR_ROLE)
            .expect("executor role present");
        // role-level override wins over the workflow-level default
        assert_eq!(resolved.profile.as_deref(), Some("role-profile"));
        // workflow-level defaults fill in when the role has no override
        assert_eq!(resolved.provider.as_deref(), Some("wf-provider"));
        assert_eq!(resolved.retries, Some(3));
        assert_eq!(resolved.template.as_deref(), Some("executor-template"));
        // absent roles resolve to None
        assert!(wf.resolve_role(TESTER_ROLE).is_none());
    }

    #[test]
    fn test_resolve_all_includes_resolution() {
        let file = WorkflowsFile::from_yaml(VALID_YAML).expect("valid yaml");
        let resolved = file.resolve_all();
        assert_eq!(resolved.len(), 1);
        let (key, _, roles) = &resolved[0];
        assert_eq!(key, "weekly-report");
        assert_eq!(roles.len(), 3);
        let executor = &roles[EXECUTOR_ROLE];
        assert!(executor.template.is_some(), "executor template present");
        // workflow-level defaults apply to roles without an override
        assert_eq!(executor.profile.as_deref(), Some("research-profile"));
        assert_eq!(executor.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn role_for_step_maps_step_keys_to_role_keys() {
        assert_eq!(role_for_step("running"), Some(EXECUTOR_ROLE));
        assert_eq!(role_for_step("testing"), Some(TESTER_ROLE));
        assert_eq!(role_for_step("review"), Some(REVIEWER_ROLE));
        // Step keys only — role names and anything else are not valid steps.
        assert_eq!(role_for_step("executor"), None);
        assert_eq!(role_for_step("blocked"), None);
        assert_eq!(role_for_step(""), None);
        assert_eq!(role_for_step("bogus"), None);
    }

    #[test]
    fn load_workflow_missing_file_and_unknown_id_yield_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No workflows.yml on disk: Ok(None), not an error.
        assert!(WorkflowsFile::load_workflow(dir.path().to_str().unwrap(), "wf").unwrap().is_none());
        // Empty id: Ok(None) without touching the filesystem.
        assert!(WorkflowsFile::load_workflow(dir.path().to_str().unwrap(), "").unwrap().is_none());
    }

    #[test]
    fn load_workflow_propagates_parse_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workflows.yml");
        std::fs::write(&path, "workflows: [not-a-map").expect("write broken yaml");
        let err = WorkflowsFile::load_workflow(dir.path().to_str().unwrap(), "wf")
            .expect_err("broken yaml must not be swallowed");
        assert!(
            err.to_string().contains("invalid workflows.yml"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_workflow_returns_workflow_for_known_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workflows.yml");
        WorkflowsFile::from_yaml(VALID_YAML)
            .expect("valid yaml")
            .save(&path)
            .expect("save");
        let wf = WorkflowsFile::load_workflow(dir.path().to_str().unwrap(), "weekly-report")
            .expect("load workflow")
            .expect("workflow exists");
        assert_eq!(wf.defaults.provider.as_deref(), Some("anthropic"));
        // Unknown id -> Ok(None).
        assert!(WorkflowsFile::load_workflow(dir.path().to_str().unwrap(), "nope")
            .unwrap()
            .is_none());
    }

    const VALID_YAML: &str = r#"
workflows:
  weekly-report:
    profile: research-profile
    provider: anthropic
    model: claude-sonnet-4-5
    plan_mode: auto_plan
    retries: 2
    roles:
      executor:
        template: |
          You are the executor for this workflow.
      tester:
        template: |
          You are the tester for this workflow.
      reviewer:
        template: |
          You are the reviewer for this workflow.
"#;

    #[test]
    fn parses_and_validates_happy_path() {
        let file = WorkflowsFile::from_yaml(VALID_YAML).expect("valid yaml should parse");
        assert_eq!(file.workflows.len(), 1);

        let wf = &file.workflows["weekly-report"];
        assert_eq!(wf.defaults.profile.as_deref(), Some("research-profile"));
        assert_eq!(wf.defaults.provider.as_deref(), Some("anthropic"));
        assert_eq!(wf.defaults.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(wf.defaults.plan_mode.as_deref(), Some("auto_plan"));
        assert_eq!(wf.defaults.retries, Some(2));

        assert_eq!(wf.roles.len(), 3);
        let executor = &wf.roles[EXECUTOR_ROLE];
        assert!(executor
            .template
            .as_deref()
            .unwrap_or("")
            .contains("executor"));
        // Per-role overrides default to None when not specified.
        assert_eq!(executor.overrides, WorkflowDefaults::default());
        assert!(wf.roles[TESTER_ROLE]
            .template
            .as_deref()
            .unwrap_or("")
            .contains("tester"));
        assert!(wf.roles[REVIEWER_ROLE]
            .template
            .as_deref()
            .unwrap_or("")
            .contains("reviewer"));
    }

    #[test]
    fn missing_tester_template_is_rejected() {
        let yaml = r#"
workflows:
  bugfix:
    roles:
      executor:
        template: |
          You are the executor.
      tester:
        profile: anthropic   # override present, but NO template
"#;
        let err = WorkflowsFile::from_yaml(yaml).expect_err("missing tester template must fail");
        let msg = err.to_string();
        assert!(msg.contains("bugfix"), "unexpected message: {msg}");
        assert!(msg.contains("tester"), "unexpected message: {msg}");
        assert!(msg.contains("template"), "unexpected message: {msg}");
    }

    #[test]
    fn missing_reviewer_template_is_rejected() {
        let yaml = r#"
workflows:
  bugfix:
    roles:
      executor:
        template: |
          You are the executor.
      reviewer:
        template: "   "
"#;
        let err = WorkflowsFile::from_yaml(yaml).expect_err("blank reviewer template must fail");
        assert!(err.to_string().contains("reviewer"));
        assert!(err.to_string().contains("template"));
    }

    #[test]
    fn missing_executor_role_is_rejected() {
        let yaml = r#"
workflows:
  bugfix:
    roles:
      tester:
        template: |
          You are the tester.
"#;
        let err = WorkflowsFile::from_yaml(yaml).expect_err("missing executor must fail");
        let msg = err.to_string();
        assert!(msg.contains("bugfix"));
        assert!(msg.contains("executor"));
    }

    #[test]
    fn unknown_role_key_is_rejected() {
        let yaml = r#"
workflows:
  bugfix:
    roles:
      executor:
        template: |
          You are the executor.
      typist:
        template: |
          You are the typist.
"#;
        let err = WorkflowsFile::from_yaml(yaml).expect_err("unknown role key must fail");
        let msg = err.to_string();
        assert!(msg.contains("typist"));
        assert!(msg.contains("unknown role"));
    }

    #[test]
    fn empty_workflows_map_is_valid() {
        let file = WorkflowsFile::from_yaml("workflows: {}\n").expect("empty map is valid");
        assert!(file.workflows.is_empty());
    }

    #[test]
    fn executor_template_is_optional() {
        let yaml = r#"
workflows:
  bugfix:
    roles:
      executor:
        profile: research-profile
"#;
        let file = WorkflowsFile::from_yaml(yaml).expect("executor without template is valid");
        let wf = &file.workflows["bugfix"];
        assert!(wf.roles[EXECUTOR_ROLE].template.is_none());
        assert_eq!(
            wf.roles[EXECUTOR_ROLE].overrides.profile.as_deref(),
            Some("research-profile")
        );
    }

    #[test]
    fn empty_document_is_valid() {
        let file = WorkflowsFile::from_yaml("").expect("empty document is valid");
        assert!(file.workflows.is_empty());
    }

    #[test]
    fn load_missing_file_returns_not_found() {
        let missing = std::env::temp_dir().join("does-not-exist-workflows.yml");
        let err = WorkflowsFile::load(&missing).expect_err("missing file must fail");
        assert!(matches!(err, WorkflowConfigError::NotFound { .. }));
    }

    #[test]
    fn serializes_back_without_losing_fields() {
        let file = WorkflowsFile::from_yaml(VALID_YAML).unwrap();
        let round_trip = serde_yaml::to_string(&file).expect("serialize");
        let reparsed = WorkflowsFile::from_yaml(&round_trip).expect("reparse");
        assert_eq!(reparsed, file);
    }

    #[test]
    fn clear_executions_on_review_defaults_false() {
        let yaml = "workflows:\n  default:\n    roles:\n      executor:\n        template: t\n";
        let file = WorkflowsFile::from_yaml(yaml).expect("parse");
        assert!(!file.workflows["default"].clear_executions_on_review);
    }

    #[test]
    fn clear_executions_on_review_round_trips() {
        let yaml = "workflows:\n  default:\n    clear_executions_on_review: true\n    roles:\n      executor:\n        template: t\n";
        let file = WorkflowsFile::from_yaml(yaml).expect("parse");
        assert!(file.workflows["default"].clear_executions_on_review);
        let out = serde_yaml::to_string(&file).expect("serialize");
        assert!(out.contains("clear_executions_on_review"));
        let reparsed = WorkflowsFile::from_yaml(&out).expect("reparse");
        assert_eq!(reparsed, file);
    }
}
