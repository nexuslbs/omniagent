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

/// One-time, idempotent migration of the legacy retention setting key.
///
/// The old `delete_after_days` key is RENAMED to `soft_delete_after_days`,
/// but its VALUE is intentionally NOT carried over: neither
/// `soft_delete_after_days` nor `hard_delete_after_days` has a default (empty
/// or 0 = disabled), so the legacy numeric value is dropped and the renamed
/// setting starts empty (= disabled) until an operator sets a value > 0.
///
/// Only the legacy line is removed; every other line (comments, section
/// layout, other settings) is preserved verbatim. No new key is inserted.
/// Best-effort: a missing/unreadable settings file (or a write failure) is
/// logged and ignored, never fatal.
pub fn migrate_legacy_settings(data_dir: impl AsRef<Path>) {
    const LEGACY_KEY: &str = "delete_after_days";
    let path = config_path(data_dir, "settings.yml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return, // no settings.yml (fresh install): nothing to migrate
    };

    // Match `delete_after_days:` as a KEY (indented, whole key, colon after).
    let is_legacy = |line: &str| {
        let trimmed = line.trim_start();
        if line.len() == trimmed.len() {
            return false; // top-level: not a section setting
        }
        match trimmed.strip_prefix(LEGACY_KEY) {
            Some(rest) => rest.trim_start().starts_with(':'),
            None => false,
        }
    };

    if !content.lines().any(is_legacy) {
        return; // already migrated (or never had it): idempotent no-op
    }

    let mut migrated = String::with_capacity(content.len());
    for line in content.lines() {
        if is_legacy(line) {
            continue;
        }
        migrated.push_str(line);
        migrated.push('\n');
    }

    if let Err(e) = std::fs::write(&path, migrated) {
        tracing::warn!(
            "Failed to migrate legacy '{}' key in {}: {:?} (value intentionally not carried over)",
            LEGACY_KEY,
            path.display(),
            e
        );
    } else {
        tracing::info!(
            "Migrated settings.yml: removed legacy '{}' key (no value carried over; soft/hard delete both start empty = disabled)",
            LEGACY_KEY
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_legacy_settings_removes_key_without_carrying_value() {
        let dir =
            std::env::temp_dir().join(format!("omniagent-config-path-test-{}", std::process::id()));
        let cfg_dir = dir.join("config");
        std::fs::create_dir_all(&cfg_dir).expect("create temp config dir");
        let file = cfg_dir.join("settings.yml");
        std::fs::write(
            &file,
            "general:\n  condense_keep_turns: 4\n  delete_after_days: 15\n  temperature: 0.7\n",
        )
        .expect("write settings.yml");

        migrate_legacy_settings(&dir);
        // Idempotent: a second call changes nothing.
        migrate_legacy_settings(&dir);

        let after = std::fs::read_to_string(&file).expect("read settings.yml");
        assert!(
            !after.contains("delete_after_days"),
            "legacy key removed: {after}"
        );
        assert!(
            !after.contains("soft_delete_after_days"),
            "no value carried over: {after}"
        );
        assert!(after.contains("condense_keep_turns: 4"));
        assert!(after.contains("temperature: 0.7"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
