//! Board configuration: parsing + validation of `boards.yml` (kanban boards).
//!
//! Boards are FILE-DEFINED like workflows: the YAML file at
//! `<OMNI_DIR>/config/boards.yml` is the single source of truth - there are
//! NO `boards` DB tables. The file maps board names to default execution
//! options (channel, profile, workflow, plan, template, priority) that act as
//! the task's fallback when the kanban task itself does not set an option.
//!
//! Boards are ALWAYS ENABLED: there is no "boards feature off" state and no
//! code path may treat the feature as disabled. A task's `board` is ALWAYS
//! resolved and validated against the EFFECTIVE board set:
//!
//! * `boards.yml` present and valid -> the file's board set is authoritative;
//! * `boards.yml` missing -> the built-in DEFAULT board set
//!   ([`default_boards`]: at least the [`DEFAULT_BOARD_NAME`] board) is used,
//!   so kanban keeps working end to end (task creation, dispatch board
//!   resolution, dashboard board selector) and the API surfaces a LOUD,
//!   STRUCTURED warning ([`BoardsConfigWarning`], code
//!   `boards_config_missing`) instead of a silent empty board list;
//! * `boards.yml` present but unreadable / invalid YAML / defining ZERO
//!   boards -> an explicit configuration ERROR ([`BoardsConfigError`]), never
//!   a silent disable and never an empty success.
//!
//! A task with `board IS NULL` (or empty) or a board not present in the
//! effective board set is an INVALID-BOARD task: it is never dispatched, and
//! any thread-creation attempt for it fails the thread with a clear error
//! message.
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
//!     toolset: my-toolset                  # optional (config/toolsets.yml)
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
/// template, priority) plus the BOARD tier of the toolset chain. Each field
/// is optional; resolution falls through to the next level (Channel / Global
/// Settings) when a field is absent.
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
    /// Toolset id (`config/toolsets.yml`) contributed by this BOARD. It is the
    /// fourth tier of the first-match chain
    /// `workflow_role > workflow > task > board > channel > profile`: it wins
    /// over the channel/profile tiers and loses to the task/workflow tiers.
    /// Unset/empty contributes nothing (all tools allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolset: Option<String>,
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
    /// The file exists but defines ZERO boards. Boards are always enabled, so
    /// an empty board set is an explicit configuration error - never a silent
    /// "boards disabled" / empty board list.
    Empty {
        path: PathBuf,
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
            BoardsConfigError::Empty { path } => write!(
                f,
                "boards.yml at {} defines no boards (boards are always enabled - \
                 add at least one board, e.g. '{DEFAULT_BOARD_NAME}')",
                path.display()
            ),
        }
    }
}

impl std::error::Error for BoardsConfigError {}

/// Name of the built-in DEFAULT board used when `boards.yml` is missing.
/// Boards are always enabled, so this board always exists.
pub const DEFAULT_BOARD_NAME: &str = "main";

/// The operator-configured default board name (`settings.yml` `default_board`),
/// falling back to [`DEFAULT_BOARD_NAME`] when the key is absent (audit HV-A4).
/// A deployment that seeds a differently named default board sets this key
/// instead of shipping a code change.
pub fn default_board_name() -> String {
    crate::runtime_settings::get_str("default_board", DEFAULT_BOARD_NAME)
}

/// The built-in DEFAULT board set: one board ([`default_board_name`]) with no
/// execution options of its own, so every option falls through to the
/// Channel / Global Settings levels. Used when `boards.yml` is absent - kanban
/// therefore keeps working end to end without any config file.
pub fn default_boards() -> BoardsFile {
    let mut file = BoardsFile::default();
    file.upsert(&default_board_name(), BoardConfig::default());
    file
}

/// Where the EFFECTIVE board set came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardsSource {
    /// `config/boards.yml` was present and valid.
    File,
    /// `config/boards.yml` is missing: the built-in default board set applies.
    Default,
}

impl BoardsSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            BoardsSource::File => "file",
            BoardsSource::Default => "default",
        }
    }
}

/// Loud, STRUCTURED configuration warning. Serialized verbatim into the
/// `GET /boards` payload as `config_warning`, so a missing/partial config is
/// visible to operators instead of degrading into a silent empty board list.
#[derive(Debug, Clone, Serialize)]
pub struct BoardsConfigWarning {
    /// Stable machine-readable code (currently `boards_config_missing`).
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
    /// Path of the missing/unusable configuration file.
    pub path: String,
    /// The board names actually in use (the fallback set) - always non-empty.
    pub fallback_boards: Vec<String>,
}

/// The EFFECTIVE board configuration: the parsed file OR the built-in default
/// set, plus an optional loud warning for the missing-file case.
#[derive(Debug, Clone)]
pub struct BoardsConfig {
    pub file: BoardsFile,
    pub source: BoardsSource,
    pub warning: Option<BoardsConfigWarning>,
}

impl BoardsConfig {
    pub fn board(&self, name: &str) -> Option<&BoardConfig> {
        self.file.boards.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.file.boards.contains_key(name)
    }

    /// Board names of the effective set (never empty).
    pub fn names(&self) -> Vec<String> {
        self.file.boards.keys().cloned().collect()
    }
}

impl BoardsFile {
    /// Parse a `boards.yml` document. An empty/whitespace document counts as
    /// an empty board set (so file CRUD can write an empty doc safely and the
    /// CALLER decides whether an empty set is acceptable).
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

/// Load the EFFECTIVE board configuration. Boards are ALWAYS enabled, so this
/// never yields an empty board set:
///
/// * file present, valid and defining at least one board -> `File`, no
///   warning;
/// * file missing -> the built-in DEFAULT board set + a loud structured
///   warning (`boards_config_missing`) - never a silent empty board list;
/// * file present but unusable (unreadable, invalid YAML, or ZERO boards) ->
///   `Err(BoardsConfigError)`: an explicit error, never a silent disable.
pub fn load_boards_config(data_dir: impl AsRef<Path>) -> Result<BoardsConfig, BoardsConfigError> {
    let path = boards_path(data_dir.as_ref());
    match BoardsFile::load(&path) {
        Ok(file) => {
            if file.boards.is_empty() {
                return Err(BoardsConfigError::Empty { path });
            }
            Ok(BoardsConfig {
                file,
                source: BoardsSource::File,
                warning: None,
            })
        }
        Err(BoardsConfigError::NotFound { path }) => {
            let file = default_boards();
            let warning = BoardsConfigWarning {
                code: "boards_config_missing".to_string(),
                message: format!(
                    "boards.yml is missing at {}; boards are ALWAYS enabled, so the built-in \
                     default board set ({DEFAULT_BOARD_NAME}) is in use. Create the file to \
                     configure boards.",
                    path.display()
                ),
                path: path.display().to_string(),
                fallback_boards: file.boards.keys().cloned().collect(),
            };
            Ok(BoardsConfig {
                file,
                source: BoardsSource::Default,
                warning: Some(warning),
            })
        }
        Err(err) => Err(err),
    }
}

/// Resolve a task's `board` against an already-loaded effective config (used
/// by the auto-dispatcher, which loads the configuration ONCE per scan).
///
/// * board NULL/empty -> `Err("task has no board")`;
/// * board not in the effective set ->
///   `Err("task board 'X' not found in boards.yml")`;
/// * board found -> `Ok(Some(cfg))`.
pub fn task_board_in(
    config: &BoardsConfig,
    board: Option<&str>,
) -> Result<Option<BoardConfig>, String> {
    match board.map(str::trim).filter(|b| !b.is_empty()) {
        None => Err("task has no board".to_string()),
        Some(name) => config
            .board(name)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("task board '{name}' not found in boards.yml")),
    }
}

/// Resolve the board config for a task's `board` value. Boards are ALWAYS
/// enabled, so validation ALWAYS runs - there is no "boards disabled" path and
/// no inert `Ok(None)` board field:
///
/// * board NULL/empty -> `Err("task has no board")`;
/// * board not in the effective board set ->
///   `Err("task board 'X' not found in boards.yml")`;
/// * board found -> `Ok(Some(cfg))`;
/// * the configuration is unreadable/invalid/empty -> `Err(...)` (fail loud,
///   never a silent "no board").
///
/// With `boards.yml` absent the built-in DEFAULT board set applies, so `main`
/// resolves and every other name is rejected exactly as if the file listed
/// only that board.
pub fn task_board(
    data_dir: impl AsRef<Path>,
    board: Option<&str>,
) -> Result<Option<BoardConfig>, String> {
    let config = load_boards_config(data_dir.as_ref())
        .map_err(|e| format!("failed to load boards.yml: {e}"))?;
    task_board_in(&config, board)
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
    fn board_toolset_round_trips() {
        let yaml = "boards:\n  omnidev:\n    channel: omnidev\n    toolset: my-toolset\n";
        let file = BoardsFile::from_yaml(yaml).expect("parse");
        assert_eq!(
            file.boards
                .get("omnidev")
                .and_then(|b| b.toolset.as_deref()),
            Some("my-toolset")
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config").join("boards.yml");
        file.save(&path).expect("save");
        let loaded = BoardsFile::load(&path).expect("load");
        assert_eq!(
            loaded
                .boards
                .get("omnidev")
                .and_then(|b| b.toolset.as_deref()),
            Some("my-toolset")
        );
        // A board with no toolset omits the key entirely (no noise).
        let mut bare = BoardsFile::default();
        bare.upsert("main", BoardConfig::default());
        let text = bare.to_yaml().expect("serialize");
        assert!(!text.contains("toolset"), "no toolset key expected: {text}");
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

    // -----------------------------------------------------------------------
    // Always-on boards: DEFAULT set + loud warning + fail-loud empty/invalid
    // -----------------------------------------------------------------------

    fn write_boards(dir: &Path, yaml: &str) -> PathBuf {
        let path = boards_path(dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, yaml).unwrap();
        path
    }

    #[test]
    fn default_set_has_the_default_board() {
        let file = default_boards();
        assert!(!file.boards.is_empty(), "default set is never empty");
        assert!(file.boards.contains_key(DEFAULT_BOARD_NAME));
    }

    #[test]
    fn config_missing_falls_back_to_default_set_with_loud_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = load_boards_config(dir.path()).expect("missing file -> default set");
        assert_eq!(cfg.source, BoardsSource::Default);
        assert!(cfg.contains(DEFAULT_BOARD_NAME));
        let w = cfg.warning.expect("missing config must warn loudly");
        assert_eq!(w.code, "boards_config_missing");
        assert_eq!(w.fallback_boards, vec![DEFAULT_BOARD_NAME.to_string()]);
        assert!(w.message.contains("boards.yml is missing"), "{}", w.message);
        assert!(w.message.contains(DEFAULT_BOARD_NAME));
    }

    #[test]
    fn config_present_has_no_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_boards(dir.path(), "boards:\n  main:\n    channel: kanban\n");
        let cfg = load_boards_config(dir.path()).expect("valid config");
        assert_eq!(cfg.source, BoardsSource::File);
        assert!(cfg.warning.is_none());
        assert_eq!(cfg.names(), vec!["main".to_string()]);
    }

    #[test]
    fn config_present_but_empty_is_an_explicit_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_boards(dir.path(), "");
        let err = load_boards_config(dir.path()).unwrap_err();
        assert!(matches!(err, BoardsConfigError::Empty { .. }), "got {err}");
        assert!(err.to_string().contains("defines no boards"), "got {err}");
        // A `boards: {}` document is the same explicit error.
        write_boards(dir.path(), "boards: {}\n");
        let err = load_boards_config(dir.path()).unwrap_err();
        assert!(matches!(err, BoardsConfigError::Empty { .. }), "got {err}");
    }

    #[test]
    fn config_present_but_invalid_is_an_explicit_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_boards(dir.path(), "boards: [not, a, dict");
        let err = load_boards_config(dir.path()).unwrap_err();
        assert!(matches!(err, BoardsConfigError::Yaml { .. }), "got {err}");
    }

    #[test]
    fn task_board_is_never_inert_when_file_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No boards.yml: the DEFAULT board set applies, so validation STILL runs.
        assert_eq!(
            task_board(dir.path(), Some(DEFAULT_BOARD_NAME))
                .expect("default board resolves without boards.yml")
                .unwrap()
                .channel,
            None
        );
        let err = task_board(dir.path(), None).unwrap_err();
        assert_eq!(err, "task has no board");
        let err = task_board(dir.path(), Some("")).unwrap_err();
        assert_eq!(err, "task has no board");
        let err = task_board(dir.path(), Some("anything")).unwrap_err();
        assert!(err.contains("not found in boards.yml"), "got: {err}");
    }

    #[test]
    fn task_board_is_identical_with_and_without_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        // File absent -> default set. File present listing ONLY `main` -> the
        // effective set is the same, so the outcomes must be identical.
        let absent = task_board(dir.path(), Some(DEFAULT_BOARD_NAME)).unwrap();
        let absent_unknown = task_board(dir.path(), Some("nope")).unwrap_err();
        write_boards(dir.path(), "boards:\n  main:\n    channel: kanban\n");
        let present = task_board(dir.path(), Some(DEFAULT_BOARD_NAME)).unwrap();
        let present_unknown = task_board(dir.path(), Some("nope")).unwrap_err();
        assert_eq!(absent.is_some(), present.is_some());
        assert_eq!(absent_unknown, present_unknown);
    }

    #[test]
    fn task_board_fails_loud_on_empty_or_invalid_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_boards(dir.path(), "");
        let err = task_board(dir.path(), Some(DEFAULT_BOARD_NAME)).unwrap_err();
        assert!(err.starts_with("failed to load boards.yml:"), "got: {err}");
        assert!(err.contains("defines no boards"), "got: {err}");
        write_boards(dir.path(), "boards: [not, a, dict");
        let err = task_board(dir.path(), Some(DEFAULT_BOARD_NAME)).unwrap_err();
        assert!(err.starts_with("failed to load boards.yml:"), "got: {err}");
        assert!(err.contains("invalid boards.yml"), "got: {err}");
    }

    #[test]
    fn task_board_invalid_when_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_boards(dir.path(), "boards:\n  main:\n    channel: kanban\n");
        // NULL/empty board -> error.
        let err = task_board(dir.path(), None).unwrap_err();
        assert_eq!(err, "task has no board");
        let err = task_board(dir.path(), Some("  ")).unwrap_err();
        assert_eq!(err, "task has no board");
        // Unknown board -> error.
        let err = task_board(dir.path(), Some("nope")).unwrap_err();
        assert!(err.contains("not found in boards.yml"));
        // Valid board -> Some(cfg).
        let cfg = task_board(dir.path(), Some("main")).expect("valid board");
        assert_eq!(cfg.unwrap().channel.as_deref(), Some("kanban"));
    }
}
