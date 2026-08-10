//! Durable working-memory notes for the agent (WS-1 notes toolset).
//!
//! Thread dir = `{omni_dir}/data/threads/{thread_id}/`. The `note_*` tools
//! write plain files inside that directory. Note names are validated to be
//! plain filenames (no path separators, no `..`, no absolute paths), so a
//! resolved path can never escape the thread dir.
//!
//! WS-4a: `context-*.json` dump files are read ONCE per thread — a second
//! read of the same dump returns a synthetic "[duplicate read ...]" marker
//! instead of the content (rule 12 anti-loop).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

/// Maximum characters returned by `note_read` (design: ~8KB).
pub const MAX_NOTE_CHARS: usize = 8192;

/// Per-thread record of which `context-*.json` dumps have already been read.
static READ_DUMPS: Mutex<Option<HashMap<i64, HashSet<String>>>> = Mutex::new(None);

/// Resolve the omni dir: plugin config value, else `OMNI_DIR` env, else
/// the conventional default `/opt/omni`.
pub fn omni_dir_from(cfg_omni_dir: &str) -> String {
    if !cfg_omni_dir.is_empty() {
        cfg_omni_dir.to_string()
    } else {
        std::env::var("OMNI_DIR").unwrap_or_else(|_| "/opt/omni".to_string())
    }
}

/// Thread dir = `{omni_dir}/data/threads/{thread_id}/`.
pub fn thread_dir(omni_dir: &str, thread_id: i64) -> PathBuf {
    Path::new(omni_dir)
        .join("data")
        .join("threads")
        .join(thread_id.to_string())
}

/// Validate a note file name: a single plain filename component.
/// Rejects empty names, `..`, absolute paths, and any path separators.
pub fn safe_note_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("note name must not be empty".to_string());
    }
    if trimmed.len() > 200 {
        return Err("note name too long".to_string());
    }
    let p = Path::new(trimmed);
    if p.components().count() != 1 {
        return Err(format!(
            "note name must be a plain filename (no path separators): '{name}'"
        ));
    }
    match p.components().next() {
        Some(Component::Normal(_)) => {}
        _ => {
            return Err(format!(
                "note name must not contain '..' or absolute paths: '{name}'"
            ))
        }
    }
    Ok(trimmed.to_string())
}

fn dump_was_read(thread_id: i64, name: &str) -> bool {
    let guard = READ_DUMPS.lock().unwrap_or_else(|p| p.into_inner());
    match guard.as_ref() {
        Some(map) => map
            .get(&thread_id)
            .map(|set| set.contains(name))
            .unwrap_or(false),
        None => false,
    }
}

fn mark_dump_read(thread_id: i64, name: &str) {
    let mut guard = READ_DUMPS.lock().unwrap_or_else(|p| p.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    map.entry(thread_id)
        .or_default()
        .insert(name.to_string());
}

fn truncate_to(mut content: String, cap: usize) -> String {
    let total = content.chars().count();
    if total > cap {
        content = content.chars().take(cap).collect();
        content.push_str(&format!(
            "\n[note truncated: showing chars 0-{cap} of {total} total chars]"
        ));
    }
    content
}

/// Read `{dir}/{name}`. `context-*.json` dumps are read-once per thread
/// (WS-4a). Output is capped at `MAX_NOTE_CHARS`.
pub fn note_read(dir: &Path, name: &str, thread_id: i64) -> (String, bool) {
    let name = match safe_note_name(name) {
        Ok(n) => n,
        Err(e) => return (e, true),
    };
    let is_dump = name.starts_with("context-") && name.ends_with(".json");
    if is_dump && dump_was_read(thread_id, &name) {
        return (
            format!(
                "[duplicate read of {name} (thread {thread_id}) — forbidden by rule 12: \
                 never re-read a context dump; trust the injected '=== Context Compacted ===' \
                 summary and your notes instead]"
            ),
            false,
        );
    }
    let path = dir.join(&name);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return (format!("error reading note '{name}': {e}"), true),
    };
    if is_dump {
        mark_dump_read(thread_id, &name);
    }
    (truncate_to(content, MAX_NOTE_CHARS), false)
}

/// Append a line to `{dir}/{name}` (creating the thread dir if needed).
pub fn note_append(dir: &Path, name: &str, content: &str) -> (String, bool) {
    let name = match safe_note_name(name) {
        Ok(n) => n,
        Err(e) => return (e, true),
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        return (format!("error creating thread dir: {e}"), true);
    }
    let path = dir.join(&name);
    let mut out = String::new();
    if !content.trim().is_empty() {
        out.push_str(content.trim_end());
        out.push('\n');
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => match f.write_all(out.as_bytes()) {
            Ok(_) => (format!("appended to {name}"), false),
            Err(e) => (format!("error appending to '{name}': {e}"), true),
        },
        Err(e) => (format!("error opening note '{name}': {e}"), true),
    }
}

/// Overwrite `{dir}/{name}`.
pub fn note_write(dir: &Path, name: &str, content: &str) -> (String, bool) {
    let name = match safe_note_name(name) {
        Ok(n) => n,
        Err(e) => return (e, true),
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        return (format!("error creating thread dir: {e}"), true);
    }
    let path = dir.join(&name);
    match std::fs::write(&path, content) {
        Ok(_) => (format!("wrote {name}"), false),
        Err(e) => (format!("error writing note '{name}': {e}"), true),
    }
}

/// List file names in the thread dir (sorted).
pub fn note_list(dir: &Path) -> (String, bool) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => return (format!("error listing notes: {e}"), true),
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    if names.is_empty() {
        return ("(no notes yet)".to_string(), false);
    }
    (names.join("\n"), false)
}

/// Remove `{dir}/{name}`.
pub fn note_rm(dir: &Path, name: &str) -> (String, bool) {
    let name = match safe_note_name(name) {
        Ok(n) => n,
        Err(e) => return (e, true),
    };
    let path = dir.join(&name);
    match std::fs::remove_file(&path) {
        Ok(_) => (format!("removed {name}"), false),
        Err(e) => (format!("error removing note '{name}': {e}"), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("note-tests-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip_append_read_write_list_rm() {
        let dir = tmp_dir("roundtrip");
        // append then read
        assert!(!note_append(&dir, "notes.md", "fact one").1);
        assert!(!note_append(&dir, "notes.md", "fact two").1);
        let (content, err) = note_read(&dir, "notes.md", 1);
        assert!(!err);
        assert!(content.contains("fact one") && content.contains("fact two"));
        // overwrite
        assert!(!note_write(&dir, "notes.md", "fresh").1);
        let (content, _) = note_read(&dir, "notes.md", 1);
        assert_eq!(content, "fresh");
        // list
        let (listing, _) = note_list(&dir);
        assert!(listing.contains("notes.md"));
        // rm
        assert!(!note_rm(&dir, "notes.md").1);
        let (content, err) = note_read(&dir, "notes.md", 1);
        assert!(err); // gone
        assert!(content.contains("error reading"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sandbox_rejects_escapes() {
        let dir = tmp_dir("sandbox");
        for bad in ["../evil", "/etc/passwd", "a/b", "..", ".", "sub/note"] {
            let (msg, is_err) = note_write(&dir, bad, "x");
            assert!(is_err, "name '{bad}' should be rejected, got: {msg}");
            assert!(msg.contains("plain filename") || msg.contains("must not"));
        }
        // nothing was written outside
        let evil = dir.join("..").join("evil");
        assert!(!evil.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_caps_at_8k() {
        let dir = tmp_dir("cap");
        let big = "x".repeat(MAX_NOTE_CHARS + 5000);
        assert!(!note_write(&dir, "big.txt", &big).1);
        let (content, err) = note_read(&dir, "big.txt", 1);
        assert!(!err);
        assert!(content.contains("[note truncated"));
        assert!(content.chars().count() <= MAX_NOTE_CHARS + 120);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_dump_read_once() {
        let dir = tmp_dir("readonce");
        assert!(!note_write(&dir, "context-7.json", "dump contents").1);
        // first read returns content
        let (c1, e1) = note_read(&dir, "context-7.json", 42);
        assert!(!e1);
        assert_eq!(c1, "dump contents");
        // second read is blocked (rule 12)
        let (c2, e2) = note_read(&dir, "context-7.json", 42);
        assert!(!e2);
        assert!(c2.contains("duplicate read of context-7.json"));
        assert!(c2.contains("rule 12"));
        // different thread can still read it
        let (c3, e3) = note_read(&dir, "context-7.json", 43);
        assert!(!e3);
        assert_eq!(c3, "dump contents");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
