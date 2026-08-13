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
