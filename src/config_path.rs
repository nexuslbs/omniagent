//! Canonical path resolution for OMNI_DIR config files.
//!
//! All root-level yml config files (`actions.yml`, `plugins.yml`, `remote.yml`,
//! `settings.yml`, `workflows.yml`) live in `{data_dir}/config/`. Every consumer
//! resolves its path through [`config_path`] so the layout has a single source of
//! truth. Docker-compose files intentionally stay at the `data_dir` root.

use std::path::{Path, PathBuf};

/// Canonical path to a named config file: `{data_dir}/config/{name}`.
pub fn config_path(data_dir: impl AsRef<Path>, name: &str) -> PathBuf {
    Path::new(data_dir.as_ref()).join("config").join(name)
}

/// Best-effort, idempotent creation of the `{data_dir}/config/` directory.
/// Non-fatal: callers should not fail startup if the dir cannot be created.
pub fn ensure_config_dir(data_dir: impl AsRef<Path>) {
    let dir = Path::new(data_dir.as_ref()).join("config");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("Failed to create config dir {}: {:?}", dir.display(), e);
    }
}

/// One-time, idempotent migration of the legacy retention setting keys.
///
/// The two retention settings have been renamed twice:
///
///  * `soft_delete_after_days` -> `delete_after_days_soft` and
///    `hard_delete_after_days` -> `delete_after_days_hard`: the previous key is
///    RENAMED in place and its VALUE IS CARRIED OVER, so an existing deployment
///    keeps its configured retention horizon under the new name;
///  * `delete_after_days` (the oldest name of the soft-delete horizon) is
///    DROPPED and its value is intentionally NOT carried over: neither
///    `delete_after_days_soft` nor `delete_after_days_hard` has a default (empty
///    or 0 = disabled), so the renamed setting starts empty (= disabled) until
///    an operator sets a value > 0.
///
/// Only a real setting KEY line (indented, whole key, colon after) is touched;
/// every other line (comments, section layout, other settings) is preserved
/// verbatim. No new key is inserted. Best-effort: a missing/unreadable settings
/// file (or a write failure) is logged and ignored, never fatal.
pub fn migrate_legacy_settings(data_dir: impl AsRef<Path>) {
    /// Previous key names of the two retention settings (value kept).
    const RENAMED_KEYS: [(&str, &str); 2] = [
        ("soft_delete_after_days", "delete_after_days_soft"),
        ("hard_delete_after_days", "delete_after_days_hard"),
    ];
    /// Oldest key: removed, value intentionally NOT carried over (see above).
    const REMOVED_KEY: &str = "delete_after_days";

    let path = config_path(data_dir, "settings.yml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return, // no settings.yml (fresh install): nothing to migrate
    };

    let mut changed = false;
    let mut migrated = String::with_capacity(content.len());
    for line in content.lines() {
        if let Some((indent, key, rest)) = setting_key(line) {
            // The even older soft-delete key is dropped (value not carried over).
            if key == REMOVED_KEY {
                changed = true;
                continue;
            }
            // Previous names are renamed in place, value carried over.
            if let Some((_, current)) = RENAMED_KEYS.iter().find(|(legacy, _)| *legacy == key) {
                migrated.push_str(indent);
                migrated.push_str(current);
                migrated.push(':');
                migrated.push_str(rest);
                migrated.push('\n');
                changed = true;
                continue;
            }
        }
        migrated.push_str(line);
        migrated.push('\n');
    }

    if !changed {
        return; // already migrated (or never had a legacy key): idempotent no-op
    }

    if let Err(e) = std::fs::write(&path, migrated) {
        tracing::warn!(
            "Failed to migrate legacy retention keys in {}: {:?}",
            path.display(),
            e
        );
    } else {
        tracing::info!(
            "Migrated settings.yml: renamed legacy retention keys (values kept) and dropped '{}' (value not carried over; soft/hard delete both start empty = disabled)",
            REMOVED_KEY
        );
    }
}

/// Split an indented `key: rest` settings line into `(indent, key, rest)`.
/// Returns `None` for top-level lines (no indent), which are never settings.
fn setting_key(line: &str) -> Option<(&str, &str, &str)> {
    let trimmed = line.trim_start();
    if line.len() == trimmed.len() {
        return None; // top-level: not a section setting
    }
    let (key, rest) = trimmed.split_once(':')?;
    Some((&line[..line.len() - trimmed.len()], key.trim_end(), rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("omniagent-config-path-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn write_settings(dir: &Path, content: &str) -> PathBuf {
        let cfg_dir = dir.join("config");
        std::fs::create_dir_all(&cfg_dir).expect("create temp config dir");
        let file = cfg_dir.join("settings.yml");
        std::fs::write(&file, content).expect("write settings.yml");
        file
    }

    #[test]
    fn migrate_legacy_settings_removes_oldest_key_without_carrying_value() {
        let dir = temp_dir("oldest");
        let file = write_settings(
            &dir,
            "general:\n  condense_keep_turns: 4\n  delete_after_days: 15\n  temperature: 0.7\n",
        );

        migrate_legacy_settings(&dir);
        // Idempotent: a second call changes nothing.
        migrate_legacy_settings(&dir);

        let after = std::fs::read_to_string(&file).expect("read settings.yml");
        assert!(
            !after.lines().any(|l| l.contains("delete_after_days:")),
            "oldest key removed (value not carried over): {after}"
        );
        assert!(after.contains("condense_keep_turns: 4"));
        assert!(after.contains("temperature: 0.7"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_legacy_settings_renames_previous_keys_and_keeps_values() {
        let dir = temp_dir("previous");
        let file = write_settings(
            &dir,
            "general:\n  condense_keep_turns: 4\n  soft_delete_after_days: 30\n  hard_delete_after_days: 90\n  temperature: 0.7\n",
        );

        migrate_legacy_settings(&dir);
        // Idempotent: a second call changes nothing.
        migrate_legacy_settings(&dir);

        let after = std::fs::read_to_string(&file).expect("read settings.yml");
        assert!(after.contains("delete_after_days_soft: 30"), "{after}");
        assert!(after.contains("delete_after_days_hard: 90"), "{after}");
        assert!(!after.contains("soft_delete_after_days"), "{after}");
        assert!(!after.contains("hard_delete_after_days"), "{after}");
        assert!(after.contains("condense_keep_turns: 4"));
        assert!(after.contains("temperature: 0.7"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
