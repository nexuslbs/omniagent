//! Shared wiki root (`<omni_dir>/wiki/`).
//!
//! The wiki is an INSTANCE-LEVEL, SHARED corpus that lives at the ROOT of the
//! omni dir (`<omni_dir>/wiki/`), NOT under a profile
//! (`<omni_dir>/profiles/<profile>/wiki/`). Every component that reads, writes
//! or indexes the wiki must resolve the root through the helpers here so the
//! path is defined in exactly one place.
//!
//! `migrate_profile_wikis` performs the one-time, idempotent move of a legacy
//! per-profile wiki to the shared root.

use std::path::{Path, PathBuf};

/// Root of the shared wiki: `<data_dir>/wiki/`.
pub fn wiki_root(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("wiki")
}

/// Shared promoted-memory directory: `<data_dir>/wiki/Memory/Promoted/`.
pub fn promoted_dir(data_dir: &str) -> PathBuf {
    wiki_root(data_dir).join("Memory").join("Promoted")
}

/// Outcome of a profile-wiki migration pass (for logging / reporting).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WikiMigration {
    /// Number of legacy `profiles/<profile>/wiki/` trees found and processed.
    pub profiles: usize,
    /// Files relocated to the shared root.
    pub moved: usize,
    /// Duplicate files (byte-identical) collapsed into the shared root copy.
    pub deduplicated: usize,
    /// Files kept alongside a different shared-root file (`<stem>.from-<profile><ext>`).
    pub conflicts: usize,
}

impl WikiMigration {
    /// True when no legacy profile wiki existed (fresh install, or the
    /// migration already ran): the call is a safe no-op.
    pub fn is_noop(&self) -> bool {
        self.profiles == 0
    }
}

/// One-time, idempotent migration of the legacy per-profile wiki
/// (`<data_dir>/profiles/<profile>/wiki/`) to the shared root
/// (`<data_dir>/wiki/`).
///
/// SCOPE: only DECLARED profiles (`config/profiles.yml`, plus the default
/// profile) are migrated; a `profiles/<name>/` directory with no yml entry is
/// ignored by the profile registry, so its legacy wiki tree is left untouched.
/// When `config/profiles.yml` is absent/empty, every profile dir is migrated.
///
/// MERGE RULE (when several profiles have a wiki): the wiki is SHARED, so every
/// legacy profile wiki is merged into the single shared root. Files are MOVED
/// preserving their relative path. On a destination collision:
///   * byte-identical   -> the incoming copy is dropped (deduplicated);
///   * different bytes  -> the shared-root file WINS and the incoming file is
///     preserved next to it as `<stem>.from-<profile><ext>` so no content is
///     lost and no divergence remains.
/// Empty legacy directories are removed afterwards, so nothing is left in the
/// old location. Fresh installs (no `profiles/<profile>/wiki/`) are a no-op.
///
/// Called once at server startup (see `main.rs`); it is safe to call on every
/// start because it does nothing once the legacy trees are gone.
pub fn migrate_profile_wikis(data_dir: &str) -> WikiMigration {
    let mut report = WikiMigration::default();
    let profiles_root = Path::new(data_dir).join("profiles");
    let entries = match std::fs::read_dir(&profiles_root) {
        Ok(e) => e,
        Err(_) => return report, // no profiles/ dir: fresh install
    };
    let root = wiki_root(data_dir);
    let declared = declared_profiles(data_dir);

    for entry in entries.flatten() {
        let profile_dir = entry.path();
        if !profile_dir.is_dir() {
            continue;
        }
        let legacy = profile_dir.join("wiki");
        if !legacy.is_dir() {
            continue;
        }
        let profile = entry.file_name().to_string_lossy().to_string();
        if let Some(names) = &declared {
            if !names.iter().any(|n| n == &profile) {
                // Undeclared profile dir: ignored by the registry, wiki left alone.
                continue;
            }
        }
        report.profiles += 1;
        if let Err(e) = merge_tree(&legacy, &root, &profile, &mut report) {
            tracing::warn!(
                "[wiki-migration] profile '{}': failed to merge {}: {}",
                profile,
                legacy.display(),
                e
            );
        }
        // Only drop the legacy tree once every file has been relocated.
        if count_files(&legacy) == 0 {
            if let Err(e) = std::fs::remove_dir_all(&legacy) {
                tracing::warn!(
                    "[wiki-migration] could not remove legacy dir {}: {}",
                    legacy.display(),
                    e
                );
            }
        }
    }

    if report.profiles > 0 {
        tracing::info!(
            "[wiki-migration] merged {} legacy profile wiki(s) into {}: {} moved, {} deduplicated, {} conflict(s) preserved",
            report.profiles,
            root.display(),
            report.moved,
            report.deduplicated,
            report.conflicts
        );
    }
    report
}

/// Names declared in `config/profiles.yml` (plus the default profile).
/// `None` when the file is absent/empty/unreadable - callers then migrate
/// every profile dir.
fn declared_profiles(data_dir: &str) -> Option<Vec<String>> {
    let pf = crate::profiles_yaml::load_profiles_from(data_dir).ok()?;
    if pf.profiles.is_empty() {
        return None;
    }
    let mut names: Vec<String> = pf.profiles.keys().cloned().collect();
    let default = crate::profile::default_profile_name();
    if !names.iter().any(|n| n == &default) {
        names.push(default);
    }
    Some(names)
}

/// Recursively move every file of `src` into `dst_root`, preserving relative
/// paths and applying the collision rule documented on `migrate_profile_wikis`.
fn merge_tree(
    src: &Path,
    dst_root: &Path,
    profile: &str,
    report: &mut WikiMigration,
) -> std::io::Result<()> {
    let mut stack: Vec<PathBuf> = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path.strip_prefix(src).unwrap_or(&path);
            let dest = dst_root.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if dest.exists() {
                let identical = std::fs::read(&path).ok() == std::fs::read(&dest).ok();
                if identical {
                    std::fs::remove_file(&path)?;
                    report.deduplicated += 1;
                } else {
                    let alt_dest = collision_dest(&dest, profile);
                    std::fs::rename(&path, &alt_dest)?;
                    report.conflicts += 1;
                    tracing::warn!(
                        "[wiki-migration] collision: kept shared-root {} and preserved incoming as {}",
                        dest.display(),
                        alt_dest.display()
                    );
                }
            } else {
                std::fs::rename(&path, &dest)?;
                report.moved += 1;
            }
        }
    }
    Ok(())
}

/// Build the collision filename `<stem>.from-<profile><ext>` next to `dest`.
fn collision_dest(dest: &Path, profile: &str) -> PathBuf {
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "page".to_string());
    let name = match dest.extension() {
        Some(ext) => format!("{}.from-{}.{}", stem, profile, ext.to_string_lossy()),
        None => format!("{}.from-{}", stem, profile),
    };
    dest.with_file_name(name)
}

/// Count regular files under `dir` (recursively). Missing dir -> 0.
fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                n += 1;
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let uniq = format!(
            "wiki_mig_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let root = std::env::temp_dir().join(uniq);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
    }

    #[test]
    fn roots_are_shared_not_profile_scoped() {
        assert_eq!(wiki_root("/opt/omni"), PathBuf::from("/opt/omni/wiki"));
        assert_eq!(
            promoted_dir("/opt/omni"),
            PathBuf::from("/opt/omni/wiki/Memory/Promoted")
        );
    }

    #[test]
    fn migrates_single_profile_and_is_idempotent() {
        let root = tmpdir("single");
        write(&root, "profiles/omni/wiki/index.md", "hello");
        write(&root, "profiles/omni/wiki/Reference/A.md", "a");

        let r = migrate_profile_wikis(root.to_str().unwrap());
        assert_eq!(r.profiles, 1);
        assert_eq!(r.moved, 2);
        assert!(!root.join("profiles/omni/wiki").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/index.md")).unwrap(),
            "hello"
        );

        let r2 = migrate_profile_wikis(root.to_str().unwrap());
        assert!(r2.is_noop(), "second run must be a no-op");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn merges_profiles_dedups_identical_and_preserves_conflicts() {
        let root = tmpdir("merge");
        write(&root, "profiles/a/wiki/shared.md", "same");
        write(&root, "profiles/a/wiki/only-a.md", "a");
        write(&root, "profiles/b/wiki/shared.md", "same"); // identical -> dedup
        write(&root, "profiles/b/wiki/only-b.md", "b");
        // Different bytes under the same relative path -> preserved.
        write(&root, "profiles/a/wiki/page.md", "from-a");
        write(&root, "profiles/b/wiki/page.md", "from-b");

        let before = count_files(&root.join("wiki"));
        assert_eq!(before, 0);
        let r = migrate_profile_wikis(root.to_str().unwrap());
        assert_eq!(r.profiles, 2);
        assert_eq!(r.deduplicated, 1);
        assert_eq!(r.conflicts, 1);
        assert_eq!(r.moved, 4);
        assert_eq!(
            std::fs::read_to_string(root.join("wiki/shared.md")).unwrap(),
            "same"
        );
        // Exactly one page.md from the profiles plus the preserved collision.
        let collisions: Vec<_> = std::fs::read_dir(root.join("wiki"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".from-"))
            .collect();
        assert_eq!(collisions.len(), 1);
        assert!(!root.join("profiles/a/wiki").exists());
        assert!(!root.join("profiles/b/wiki").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
