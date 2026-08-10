//! WS-2: durable JSON-lines digests of destroyed tool results, written to
//! `context-<iter>.json` in the thread dir so the agent can recover what it
//! learned after pruning/compaction destroyed the raw tool output.

/// Max size of a single context dump file; oldest entries are dropped beyond
/// this (design: 200KB).
pub const DUMP_MAX_BYTES: u64 = 200 * 1024;
/// Keep at most this many context-*.json files per thread dir.
pub const DUMP_MAX_FILES: usize = 3;
/// Chars captured from each end of a destroyed tool result.
const DIGEST_HEAD_CHARS: usize = 400;

/// (file path, tool+args hash) pairs already appended in this process.
static APPENDED: std::sync::Mutex<Option<std::collections::HashSet<(String, u64)>>> =
    std::sync::Mutex::new(None);

/// Append one JSON-lines digest entry `{"tool","args","chars","head","tail"}`
/// to `context-<iter>.json` in `dir`. Dedupes by (file, tool+args hash)
/// within the process. Returns `true` when a new entry was appended.
pub fn append(dir: &std::path::Path, iter: u32, tool: &str, args: &str, content: &str) -> bool {
    if tool.is_empty() && content.is_empty() {
        return false;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let file = dir.join(format!("context-{iter}.json"));
    let dedupe_key = (
        file.to_string_lossy().to_string(),
        crate::agent::helpers::hash_tool_args(&format!("{tool}\u{0}{args}")),
    );
    {
        let guard = APPENDED.lock().unwrap_or_else(|p| p.into_inner());
        if guard
            .as_ref()
            .map(|set| set.contains(&dedupe_key))
            .unwrap_or(false)
        {
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
        format!(
            "[content:{}]",
            crate::agent::helpers::hash_tool_args(content)
        )
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
        guard
            .get_or_insert_with(std::collections::HashSet::new)
            .insert(dedupe_key);
    }
    enforce_caps(dir, iter);
    true
}

/// Drop oldest entries once a dump file exceeds `DUMP_MAX_BYTES`, and keep
/// at most `DUMP_MAX_FILES` context-*.json files (delete older ones).
pub fn enforce_caps(dir: &std::path::Path, iter: u32) {
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
        let d = std::env::temp_dir().join(format!("ctxdump-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn appends_jsonl_and_dedupes() {
        let dir = tmp_dir("dedupe");
        assert!(append(
            &dir,
            3,
            "filesystem_read",
            r#"{"path":"/a"}"#,
            "alpha"
        ));
        assert!(!append(
            &dir,
            3,
            "filesystem_read",
            r#"{"path":"/a"}"#,
            "alpha"
        ));
        assert!(append(
            &dir,
            3,
            "filesystem_read",
            r#"{"path":"/b"}"#,
            "beta"
        ));
        let content = std::fs::read_to_string(dir.join("context-3.json")).unwrap();
        assert_eq!(content.lines().count(), 2);
        let entry: serde_json::Value =
            serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(entry["tool"], "filesystem_read");
        assert_eq!(entry["chars"], 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_file_cap_and_keep_three() {
        let dir = tmp_dir("cap");
        let big = "z".repeat(2400);
        for i in 0..150 {
            assert!(append(&dir, 9, "t-cap", &format!("arg-{i}"), &big));
        }
        let size = std::fs::metadata(dir.join("context-9.json")).unwrap().len();
        assert!(size <= DUMP_MAX_BYTES + 4000, "dump too big: {size}");
        for i in 1..=5 {
            assert!(append(&dir, i, "t-files", "a", "content"));
        }
        enforce_caps(&dir, 5);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("context-"))
            .collect();
        names.sort();
        // context-9.json was written by the cap part of this test, so the
        // three kept files are 4, 5, 9 (oldest dropped first).
        assert_eq!(
            names,
            vec!["context-4.json", "context-5.json", "context-9.json"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
