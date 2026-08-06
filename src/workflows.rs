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
}
