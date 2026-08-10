//! Durable context dumps (WS-2): JSON-lines digests of destroyed tool
//! results, written to `context-<iter>.json` in the thread dir so the agent
//! can recover what it learned after pruning/compaction destroyed the raw
//! tool output.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Mutex;

/// Max size of a single context dump file; oldest entries are dropped beyond
/// this (design: 200KB).
pub const DUMP_MAX_BYTES: u64 = 200 * 1024;
/// Keep at most this many context-*.json files per thread dir.
pub const DUMP_MAX_FILES: usize = 3;
/// Chars captured from each end of a destroyed tool result.
const DIGEST_HEAD_CHARS: usize = 400;

/// (file path, tool+args hash) pairs already appended in this process.
static APPENDED: Mutex<Option<HashSet<(String, u64)>>> = Mutex::new(None);

fn hash_of(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Append one JSON-lines digest entry `{"tool","args","chars","head","tail"}`
/// to `context-<iter>.json` in `dir`. Dedupes by (file, tool+args hash).
/// Returns `true` when a new entry was actually appended.
pub fn append_dump(dir: &Path, iter: u32, tool: &str, args: &str, content: &str) -> bool {
    if tool.is_empty() && content.is_empty() {
        return false;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let file = dir.join(format!("context-{iter}.json"));
    let file_key = file.to_string_lossy().to_string();
    let dedupe_key = (file_key.clone(), hash_of(&format!("{tool}\u{0}{args}")));
    {
        let mut guard = APPENDED.lock().unwrap_or_else(|p| p.into_inner());
        let set = guard.get_or_insert_with(HashSet::new);
        if set.contains(&dedupe_key) {
            return false;
        }
    }

    let chars = content.chars().count();
    let head: String = content.chars().take(DIGEST_HEAD_CHARS).collect();
    let tail: String = content
        .chars()
        .rev()
        .take(DIGEST_HEAD_CHARS)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    // When the caller has no args (e.g. the pruner), key the digest on the
    // content hash so distinct results still get distinct entries.
    let args_s = if args.trim().is_empty() {
        format!("[content:{}]", hash_of(content))
    } else {
        args.to_string()
    };
    let line = serde_json::json!({
        "tool": tool,
        "args": args_s,
        "chars": chars,
        "head": head,
        "tail": tail,
    })
    .to_string();

    let mut out = String::with_capacity(line.len() + 2);
    out.push_str(&line);
    out.push('\n');
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
    {
        Ok(mut f) => {
            use std::io::Write;
            if f.write_all(out.as_bytes()).is_err() || f.flush().is_err() {
                return false;
            }
        }
        Err(_) => return false,
    }
    {
        let mut guard = APPENDED.lock().unwrap_or_else(|p| p.into_inner());
        guard.get_or_insert_with(HashSet::new).insert(dedupe_key);
    }
    enforce_caps(dir, iter);
    true
}

/// Drop oldest entries once a dump file exceeds `DUMP_MAX_BYTES`, and keep
/// at most `DUMP_MAX_FILES` context-*.json files (delete older ones).
pub fn enforce_caps(dir: &Path, iter: u32) {
    // Per-file cap: keep the newest lines that fit in DUMP_MAX_BYTES.
    let file = dir.join(format!("context-{iter}.json"));
    if let Ok(meta) = std::fs::metadata(&file) {
        if meta.len() > DUMP_MAX_BYTES {
            if let Ok(content) = std::fs::read_to_string(&file) {
                let lines: Vec<&str> = content.lines().collect();
                let mut kept: Vec<&str> = Vec::new();
                let mut bytes = 0usize;
                for l in lines.iter().rev() {
                    let lbytes = l.len() + 1;
                    if bytes + lbytes > DUMP_MAX_BYTES as usize {
                        break;
                    }
                    bytes += lbytes;
                    kept.push(l);
                }
                kept.reverse();
                let _ = std::fs::write(&file, kept.join("\n") + "\n");
            }
        }
    }
    // Keep at most DUMP_MAX_FILES context files.
    let mut files: Vec<(String, u32)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = name
                .strip_prefix("context-")
                .and_then(|r| r.strip_suffix(".json"))
            {
                if let Ok(n) = rest.parse::<u32>() {
                    files.push((name, n));
                }
            }
        }
    }
    if files.len() > DUMP_MAX_FILES {
        files.sort_by_key(|(_, n)| *n);
        for (name, _) in files.iter().take(files.len() - DUMP_MAX_FILES) {
            let _ = std::fs::remove_file(dir.join(name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dump-tests-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn appends_jsonl_and_dedupes() {
        let dir = tmp_dir("dedupe");
        assert!(append_dump(&dir, 5, "filesystem_read", r#"{"path":"/a"}"#, "alpha"));
        // same tool+args → dedupe
        assert!(!append_dump(&dir, 5, "filesystem_read", r#"{"path":"/a"}"#, "alpha"));
        // different args → new entry
        assert!(append_dump(&dir, 5, "filesystem_read", r#"{"path":"/b"}"#, "beta"));
        let content = std::fs::read_to_string(dir.join("context-5.json")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let entry: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry["tool"], "filesystem_read");
        assert_eq!(entry["chars"], 5);
        assert_eq!(entry["head"], "alpha");
        assert_eq!(entry["tail"], "alpha");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_file_cap_drops_oldest() {
        let dir = tmp_dir("cap");
        // ~2500 chars per entry; 150 entries would be ~375KB without the cap
        let big = "y".repeat(2400);
        for i in 0..150 {
            assert!(append_dump(&dir, 9, "t-cap", &format!("arg-{i}"), &big));
        }
        let file = dir.join("context-9.json");
        let size = std::fs::metadata(&file).unwrap().len();
        assert!(
            size <= DUMP_MAX_BYTES + 4000,
            "dump file too big: {size} bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_only_last_three_dump_files() {
        let dir = tmp_dir("files");
        for i in 1..=5 {
            assert!(append_dump(&dir, i, "t-files", "a", "content"));
        }
        enforce_caps(&dir, 5);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("context-"))
            .collect();
        names.sort();
        assert_eq!(names, vec!["context-3.json", "context-4.json", "context-5.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
