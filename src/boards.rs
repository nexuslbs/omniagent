//! Board configuration: parsing + validation of `boards.yml` (kanban boards).
//!
//! Boards are FILE-DEFINED like workflows: the YAML file at
//! `<OMNI_DIR>/config/boards.yml` is the single source of truth - there are
//! NO `boards` DB tables. The file maps board names to default execution
//! options (channel, profile, workflow, plan, template, priority) that act as
//! the task's fallback when the kanban task itself does not set an option.
//!
//! Feature flag: boards are ACTIVE only while `boards.yml` exists. When the
//! file is absent (omnistable today) every kanban behavior is unchanged:
//! tasks without a board are normal, the dispatcher works as before, and no
//! board validation is applied. When the file is present, a task with
//! `board IS NULL` or a board not listed in the file is an INVALID-BOARD task:
//! it is never dispatched, and any thread-creation attempt for it fails the
//! thread with a clear error message.
//!
//! File structure:
//!
//! ```yaml
//! boards:
//!   main:
//!     channel: mattermost-stable-channel   # channel name or id
//!     profile: omni
//!     workflow: omniagent-dev
//!     plan: true
//!     template: ...                        # optional
//!     priority: 3                          # optional
//! ```
//!
//! Unknown keys inside a board are tolerated (forward compat): serde ignores
//! them.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A board's default execution options - the same option set a kanban task
/// can carry (kanban_tasks: channel_id, profile, workflow_id, plan,
/// template, priority). Each field is optional; resolution falls through
/// to the next level (Channel / Global Settings) when a field is absent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoardConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
}

/// Parsed `boards.yml`: `boards:` dict of board name → options.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoardsFile {
    pub boards: BTreeMap<String, BoardConfig>,
}

#[derive(Debug)]
pub enum BoardsConfigError {
    NotFound {
        path: PathBuf,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Yaml {
        message: String,
    },
}

impl std::fmt::Display for BoardsConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoardsConfigError::NotFound { path } => {
                write!(f, "boards.yml not found at {}", path.display())
            }
            BoardsConfigError::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            BoardsConfigError::Yaml { message } => write!(f, "invalid boards.yml: {message}"),
        }
    }
}

impl std::error::Error for BoardsConfigError {}

impl BoardsFile {
    /// Parse a `boards.yml` document. An empty/whitespace document counts as
    /// an empty board set (so file CRUD can write an empty doc safely).
    pub fn from_yaml(yaml: &str) -> Result<Self, BoardsConfigError> {
        if yaml.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_yaml::from_str(yaml).map_err(|e| BoardsConfigError::Yaml {
            message: e.to_string(),
        })
    }

    pub fn load(path: &Path) -> Result<Self, BoardsConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BoardsConfigError::NotFound {
                    path: path.to_path_buf(),
                });
            }
            Err(e) => {
                return Err(BoardsConfigError::Io {
                    path: path.to_path_buf(),
                    source: e,
                });
            }
        };
        Self::from_yaml(&text)
    }

    pub fn to_yaml(&self) -> Result<String, BoardsConfigError> {
        serde_yaml::to_string(self).map_err(|e| BoardsConfigError::Yaml {
            message: e.to_string(),
        })
    }

    /// Atomic save (temp + rename), mirroring `WorkflowsFile::save`.
    pub fn save(&self, path: &Path) -> Result<(), BoardsConfigError> {
        let yaml = self.to_yaml()?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Err(BoardsConfigError::Io {
                path: path.to_path_buf(),
                source: e,
            });
        }
        let tmp = dir.join(format!(".boards.yml.tmp.{}", std::process::id()));
        if let Err(e) = std::fs::write(&tmp, yaml) {
            return Err(BoardsConfigError::Io {
                path: tmp.clone(),
                source: e,
            });
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(BoardsConfigError::Io {
                path: path.to_path_buf(),
                source: e,
            });
        }
        Ok(())
    }

    pub fn upsert(&mut self, key: &str, board: BoardConfig) {
        self.boards.insert(key.to_string(), board);
    }

    pub fn remove(&mut self, key: &str) -> Option<BoardConfig> {
        self.boards.remove(key)
    }

    pub fn get(&self, key: &str) -> Option<&BoardConfig> {
        self.boards.get(key)
    }
}

/// Canonical path to the deployment's `boards.yml` (under the data dir).
pub fn boards_path(data_dir: impl AsRef<Path>) -> PathBuf {
    crate::config_path::config_path(data_dir, "boards.yml")
}

/// Feature gate: boards are active iff `{data_dir}/config/boards.yml` exists.
pub fn boards_enabled(data_dir: impl AsRef<Path>) -> bool {
    boards_path(data_dir).is_file()
}

/// Resolve the board config for a task's `board` value.
///
/// - Boards disabled (no boards.yml): `Ok(None)` - the board field is inert.
/// - Boards enabled + board is NULL: `Err("task has no board")`.
/// - Boards enabled + board not in the file:
///   `Err("task board 'X' not found in boards.yml")`.
/// - Boards enabled + board found: `Ok(Some(cfg))`.
pub fn task_board(
    data_dir: impl AsRef<Path>,
    board: Option<&str>,
) -> Result<Option<BoardConfig>, String> {
    if !boards_enabled(data_dir.as_ref()) {
        return Ok(None);
    }
    match board {
        None => Err("task has no board".to_string()),
        Some(name) => {
            let path = boards_path(data_dir.as_ref());
            let file =
                BoardsFile::load(&path).map_err(|e| format!("failed to load boards.yml: {e}"))?;
            file.boards
                .get(name)
                .cloned()
                .map(Some)
                .ok_or_else(|| format!("task board '{name}' not found in boards.yml"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_boards_yml() {
        let yaml = r#"
boards:
  main:
    channel: kanban
    profile: omni
    workflow: omniagent-dev
    plan: true
"#;
        let file = BoardsFile::from_yaml(yaml).expect("parse");
        let b = file.boards.get("main").expect("board main");
        assert_eq!(b.channel.as_deref(), Some("kanban"));
        assert_eq!(b.profile.as_deref(), Some("omni"));
        assert_eq!(b.workflow.as_deref(), Some("omniagent-dev"));
        assert_eq!(b.plan, Some(true));
    }

    #[test]
    fn unknown_keys_tolerated() {
        let yaml = "boards:\n  main:\n    channel: kanban\n    future_key: 42\n";
        let file = BoardsFile::from_yaml(yaml).expect("tolerate unknown keys");
        assert_eq!(
            file.boards.get("main").and_then(|b| b.channel.as_deref()),
            Some("kanban")
        );
    }

    #[test]
    fn invalid_yaml_rejected() {
        let err = BoardsFile::from_yaml("boards: [not, a, dict").unwrap_err();
        assert!(matches!(err, BoardsConfigError::Yaml { .. }));
    }

    #[test]
    fn absent_file_is_not_found() {
        let err = BoardsFile::load(Path::new("/nonexistent/boards.yml")).unwrap_err();
        assert!(matches!(err, BoardsConfigError::NotFound { .. }));
    }

    #[test]
    fn empty_document_is_empty_board_set() {
        let file = BoardsFile::from_yaml("").expect("empty doc parses");
        assert!(file.boards.is_empty());
        let file = BoardsFile::from_yaml("   \n  \n").expect("whitespace doc parses");
        assert!(file.boards.is_empty());
    }

    #[test]
    fn save_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config").join("boards.yml");
        let mut file = BoardsFile::default();
        file.upsert(
            "main",
            BoardConfig {
                channel: Some("kanban".into()),
                ..Default::default()
            },
        );
        file.save(&path).expect("save");
        let loaded = BoardsFile::load(&path).expect("load");
        assert_eq!(
            loaded.boards.get("main").and_then(|b| b.channel.as_deref()),
            Some("kanban")
        );
    }

    #[test]
    fn task_board_disabled_when_file_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No boards.yml in the temp dir -> feature disabled, any board ok.
        assert!(task_board(dir.path(), None).is_ok());
        assert!(task_board(dir.path(), Some("anything")).is_ok());
    }

    #[test]
    fn task_board_invalid_when_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = boards_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "boards:\n  main:\n    channel: kanban\n").unwrap();
        // Enabled + NULL board -> error.
        let err = task_board(dir.path(), None).unwrap_err();
        assert_eq!(err, "task has no board");
        // Enabled + unknown board -> error.
        let err = task_board(dir.path(), Some("nope")).unwrap_err();
        assert!(err.contains("not found in boards.yml"));
        // Enabled + valid board -> Some(cfg).
        let cfg = task_board(dir.path(), Some("main")).expect("valid board");
        assert_eq!(cfg.unwrap().channel.as_deref(), Some("kanban"));
    }
}
