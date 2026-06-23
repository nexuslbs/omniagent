//! Relevance indexer — analyzes wiki files and generates a compact
//! `relevant-index.md` listing the most important pages for prompt context.
//!
//! Designed as a direct-mode cron job (like kanban_dispatcher):
//! - Scans all `.md` files under `<data_dir>/profiles/<profile>/wiki/`
//! - Computes checksums to skip unchanged files
//! - Scores each file by recency (mtime) + reference count (from messages table)
//! - Writes `relevant-index.md` (max ~1000 chars)

use anyhow::Result;
use sha2::{Digest, Sha256};
use sql_forge::sql_forge;
use sqlx::PgPool;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::{info, warn};

/// Cache file stored alongside wiki files.
const CACHE_FILE: &str = ".relevance-cache.json";
/// Output file — loaded into prompt context.
const OUTPUT_FILE: &str = "relevant-index.md";
/// Max characters for the output file.
const MAX_OUTPUT_CHARS: usize = 1000;
/// Max wiki files to consider (prevents huge scans).
const MAX_FILES: usize = 200;
/// How many entries max in the output.
const MAX_ENTRIES: usize = 30;

// ── Cache entry ──

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    /// SHA256 checksum of the file content (for detecting changes).
    checksum: String,
    /// Combined score (0.0 – 100.0).
    score: f64,
    /// Reference count from messages table.
    ref_count: u64,
    /// Last modification time (as Unix timestamp seconds).
    mtime_secs: u64,
    /// When this entry was last computed.
    computed_at: String,
}

// ── Wiki file info ──

struct WikiFile {
    /// Relative path like "Architecture.md" or "Reference/Docker.md"
    rel_path: String,
    /// Absolute path on disk.
    abs_path: PathBuf,
    /// Mtime as Unix timestamp.
    mtime_secs: u64,
    /// Content checksum.
    checksum: String,
    /// Score (populated later).
    score: f64,
    /// Reference count (populated later).
    ref_count: u64,
}

// ── Public entry point ──

/// Run the relevance indexer for all profiles found in the data directory.
/// Called by the scheduler as a direct-mode task.
pub async fn run_relevance_indexer(pool: &PgPool, data_dir: &str) -> Result<()> {
    let profiles_dir: PathBuf = [data_dir, "profiles"].iter().collect();
    if !profiles_dir.exists() {
        info!("[relevance-indexer] No profiles directory at {:?}, skipping", profiles_dir);
        return Ok(());
    }

    let entries = match fs::read_dir(&profiles_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!("[relevance-indexer] Failed to read profiles dir: {:?}", e);
            return Ok(());
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        info!("[relevance-indexer] Processing profile '{}'", name);
        if let Err(e) = index_profile(pool, data_dir, &name).await {
            warn!("[relevance-indexer] Profile '{}' failed: {:?}", name, e);
        }
    }

    info!("[relevance-indexer] Finished");
    Ok(())
}

/// Index one profile's wiki.
async fn index_profile(pool: &PgPool, data_dir: &str, profile_name: &str) -> Result<()> {
    let wiki_dir: PathBuf = [data_dir, "profiles", profile_name, "wiki"].iter().collect();
    if !wiki_dir.exists() {
        return Ok(()); // No wiki directory — nothing to do.
    }

    // Load cache
    let cache_path = wiki_dir.join(CACHE_FILE);
    let mut cache: HashMap<String, CacheEntry> = load_cache(&cache_path);

    // Scan wiki files
    let mut files: Vec<WikiFile> = Vec::new();
    let mut ctx = CollectWikiFilesCtx {
        root: &wiki_dir,
        files: &mut files,
        cache: &mut cache,
        cache_path: &cache_path,
    };
    collect_wiki_files(&mut ctx, &wiki_dir)?;

    if files.is_empty() {
        info!("[relevance-indexer] No wiki files found for profile '{}'", profile_name);
        return Ok(());
    }

    // Collect reference counts from messages table (one query for all files)
    let ref_counts = collect_reference_counts(pool, &files, profile_name).await;

    // Compute final scores
    let now_secs = now_unix_secs();
    for file in &mut files {
        file.ref_count = ref_counts.get(&file.rel_path).copied().unwrap_or(0);
        file.score = compute_score(file, now_secs);
    }

    // Sort by score descending
    files.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    // Update cache with new scores
    for file in &files {
        cache.insert(file.rel_path.clone(), CacheEntry {
            checksum: file.checksum.clone(),
            score: file.score,
            ref_count: file.ref_count,
            mtime_secs: file.mtime_secs,
            computed_at: chrono::Utc::now().to_rfc3339(),
        });
    }
    save_cache(&cache_path, &cache)?;

    // Generate relevant-index.md
    let output_path = wiki_dir.join(OUTPUT_FILE);
    let content = generate_output(&files);
    fs::write(&output_path, &content)?;

    info!(
        "[relevance-indexer] Wrote {} ({} entries, {} chars) for profile '{}'",
        OUTPUT_FILE,
        files.len().min(MAX_ENTRIES),
        content.len(),
        profile_name
    );

    Ok(())
}

// ── File scanning ──

/// Shared context for collect_wiki_files across recursive calls.
struct CollectWikiFilesCtx<'a> {
    root: &'a Path,
    files: &'a mut Vec<WikiFile>,
    cache: &'a mut HashMap<String, CacheEntry>,
    cache_path: &'a Path,
}

fn collect_wiki_files(ctx: &mut CollectWikiFilesCtx<'_>, dir: &Path) -> Result<()> {
    if ctx.files.len() >= MAX_FILES {
        return Ok(());
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().and_then(|s| s.to_str()) == Some(CACHE_FILE)
            || path.file_name().and_then(|s| s.to_str()) == Some(OUTPUT_FILE)
        {
            continue; // Skip our own files
        }

        if path.is_dir() {
            collect_wiki_files(ctx, &path)?;
            continue;
        }

        if !path.extension().and_then(|e| e.to_str()) .map(|e| e.eq_ignore_ascii_case("md")) .unwrap_or(false) {
            continue;
        }

        let rel_path = path.strip_prefix(ctx.root)
            .ok()
            .and_then(|p| p.to_str())
            .map(|s| s.to_string());

        let rel_path = match rel_path {
            Some(p) => p,
            None => continue,
        };

        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let mtime_secs = match metadata.modified() {
            Ok(t) => t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs(),
            Err(_) => 0,
        };

        // Read content and compute checksum
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let checksum = compute_sha256(&content);

        // Check cache — if checksum matches, skip re-computation and use cached score
        if let Some(cached) = ctx.cache.get(&rel_path) {
            if cached.checksum == checksum && cached.mtime_secs == mtime_secs {
                // File unchanged — push with cached score
                ctx.files.push(WikiFile {
                    rel_path,
                    abs_path: path,
                    mtime_secs,
                    checksum,
                    score: cached.score,
                    ref_count: cached.ref_count,
                });
                continue;
            }
        }

        // New or changed file — will compute score later
        ctx.files.push(WikiFile {
            rel_path,
            abs_path: path,
            mtime_secs,
            checksum,
            score: 0.0,
            ref_count: 0,
        });
    }

    Ok(())
}

// ── Reference counting ──

/// Query the messages table for how many times each wiki file path appears.
async fn collect_reference_counts(
    pool: &PgPool,
    files: &[WikiFile],
    _profile_name: &str,
) -> HashMap<String, u64> {
    let mut counts = HashMap::new();

    // Build LIKE patterns for each wiki file
    // We run one query per file — for a small set (<=200) this is fine.
    // A single query with many OR LIKE would be more efficient but more complex.
    for file in files {
        // Normalize path separators for LIKE matching
        let query_path = file.rel_path.replace('\\', "/");
        let pattern = format!("%{}%", query_path);

        #[derive(Debug, sqlx::FromRow)]
        struct CountRow {
            count: Option<i64>,
        }

        let result: std::result::Result<CountRow, sqlx::Error> = sql_forge!(
            CountRow,
            "SELECT COUNT(*)::bigint AS count FROM messages WHERE content ILIKE :pattern",
            ( :pattern = &pattern )
        )
        .fetch_one(pool)
        .await;

        match result {
            Ok(row) => {
                counts.insert(file.rel_path.clone(), row.count.unwrap_or(0) as u64);
            }
            Err(e) => {
                warn!("[relevance-indexer] Failed to count references for '{}': {:?}", file.rel_path, e);
                counts.insert(file.rel_path.clone(), 0);
            }
        }
    }

    counts
}

// ── Scoring ──

fn compute_score(file: &WikiFile, now_secs: u64) -> f64 {
    // Recency score: 0-50 points based on how recently modified
    let age_secs = now_secs.saturating_sub(file.mtime_secs);
    let recency_score = if age_secs < 3600 {          // < 1 hour
        50.0
    } else if age_secs < 86400 {                      // < 1 day
        40.0
    } else if age_secs < 604800 {                     // < 1 week
        30.0
    } else if age_secs < 2_592_000 {                  // < 1 month
        20.0
    } else if age_secs < 7_776_000 {                  // < 3 months
        10.0
    } else {
        5.0
    };

    // Reference score: 0-50 points based on how often referenced
    // Cap at 50 references for max score
    let ref_score = (file.ref_count.min(50) as f64 / 50.0) * 50.0;

    recency_score + ref_score
}

// ── Output generation ──

fn generate_output(files: &[WikiFile]) -> String {
    let mut output = String::from("# Relevant Wiki Pages\n\n");
    let mut remaining = MAX_OUTPUT_CHARS.saturating_sub(output.len());

    for file in files.iter().take(MAX_ENTRIES) {
        let line = if file.score > 0.0 {
            format!("- [{}]({}) — score: {:.0}\n", file.rel_path, file.rel_path, file.score)
        } else {
            format!("- [{}]({})\n", file.rel_path, file.rel_path)
        };

        if line.len() > remaining {
            break;
        }

        output.push_str(&line);
        remaining = remaining.saturating_sub(line.len());
    }

    if output.trim().lines().count() <= 1 {
        output.push_str("(No wiki pages found)\n");
    }

    output
}

// ── Cache helpers ──

fn load_cache(path: &Path) -> HashMap<String, CacheEntry> {
    if !path.exists() {
        return HashMap::new();
    }
    match fs::read_to_string(path) {
        Ok(content) => {
            serde_json::from_str(&content).unwrap_or_else(|e| {
                warn!("[relevance-indexer] Failed to parse cache, starting fresh: {:?}", e);
                HashMap::new()
            })
        }
        Err(e) => {
            warn!("[relevance-indexer] Failed to read cache: {:?}", e);
            HashMap::new()
        }
    }
}

fn save_cache(path: &Path, cache: &HashMap<String, CacheEntry>) -> Result<()> {
    let content = serde_json::to_string_pretty(cache)?;
    fs::write(path, content)?;
    Ok(())
}

// ── Utility ──

fn compute_sha256(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_sha256() {
        let hash = compute_sha256("hello world");
        assert_eq!(hash.len(), 64); // SHA256 hex
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_output_empty() {
        let out = generate_output(&[]);
        assert!(out.contains("No wiki pages found"));
        assert!(out.len() <= MAX_OUTPUT_CHARS);
    }

    #[test]
    fn test_generate_output_few_entries() {
        let files = vec![
            WikiFile {
                rel_path: "Architecture.md".into(),
                abs_path: PathBuf::new(),
                mtime_secs: 1000,
                checksum: "abc".into(),
                score: 90.0,
                ref_count: 10,
            },
            WikiFile {
                rel_path: "Reference/Docker.md".into(),
                abs_path: PathBuf::new(),
                mtime_secs: 1000,
                checksum: "def".into(),
                score: 50.0,
                ref_count: 3,
            },
        ];
        let out = generate_output(&files);
        assert!(out.contains("Architecture.md"));
        assert!(out.contains("Reference/Docker.md"));
        assert!(out.contains("score: 90"));
        assert!(out.contains("score: 50"));
        assert!(out.len() <= MAX_OUTPUT_CHARS);
    }

    #[test]
    fn test_generate_output_truncated() {
        let files: Vec<WikiFile> = (0..50)
            .map(|i| WikiFile {
                rel_path: format!("Page{}.md", i),
                abs_path: PathBuf::new(),
                mtime_secs: 1000,
                checksum: format!("c{}", i),
                score: (50 - i) as f64,
                ref_count: 0,
            })
            .collect();
        let out = generate_output(&files);
        assert!(out.len() <= MAX_OUTPUT_CHARS);
        // Should have more than 1 line (the header)
        assert!(out.trim().lines().count() > 1);
    }

    #[test]
    fn test_compute_score_new_file_no_refs() {
        let now = 1_000_000;
        let file = WikiFile {
            rel_path: "test.md".into(),
            abs_path: PathBuf::new(),
            mtime_secs: now - 1800, // 30 min ago
            checksum: "abc".into(),
            score: 0.0,
            ref_count: 0,
        };
        // Recency: < 1 hour → 50. Ref: 0 → 0. Total: 50
        assert!((compute_score(&file, now) - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_score_old_with_refs() {
        let now = 10_000_000;
        let file = WikiFile {
            rel_path: "test.md".into(),
            abs_path: PathBuf::new(),
            mtime_secs: 1_000_000, // very old
            checksum: "abc".into(),
            score: 0.0,
            ref_count: 25, // half of cap
        };
        // Recency: > 3 months → 5. Ref: 25/50*50 = 25. Total: 30
        assert!((compute_score(&file, now) - 30.0).abs() < 0.01);
    }

    #[test]
    fn test_cache_roundtrip() {
        let dir = std::env::temp_dir().join("relevance-test-cache");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let cache_path = dir.join("test-cache.json");
        let mut cache: HashMap<String, CacheEntry> = HashMap::new();
        cache.insert("Architecture.md".into(), CacheEntry {
            checksum: "abc".into(),
            score: 90.0,
            ref_count: 10,
            mtime_secs: 1000,
            computed_at: "now".into(),
        });

        save_cache(&cache_path, &cache).unwrap();
        let loaded = load_cache(&cache_path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["Architecture.md"].score, 90.0);

        let _ = fs::remove_dir_all(&dir);
    }
}
