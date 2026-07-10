use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Delimiter used between entries in the memory store.
pub const ENTRY_DELIMITER: &str = "\n§\n";

// ── Template loader ─────────────────────────────────────────────

/// Load a template file from `profiles/<name>/templates/<name>.md`.
pub fn load_template(data_dir: &str, profile_name: &str, template_name: &str) -> Option<String> {
    if template_name.is_empty() {
        return None;
    }
    let path: PathBuf = [data_dir, "profiles", profile_name, "templates", template_name]
        .iter()
        .collect();
    let path = if path.extension().is_some() {
        path
    } else {
        let mut with_ext = path;
        with_ext.set_extension("md");
        with_ext
    };
    if !path.exists() {
        return None;
    }
    match fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim().to_string();
            if trimmed.is_empty() { None } else { Some(trimmed) }
        }
        Err(_) => None,
    }
}

// ── Number formatting ──────────────────────────────────────────

fn format_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result
}

// ── Memory Store ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStore {
    memories_dir: PathBuf,
    profile_path: Option<String>,
    snapshot: HashMap<String, String>,
}

impl MemoryStore {
    pub fn new(base_path: &str) -> Self {
        Self {
            memories_dir: PathBuf::from(base_path).join("memories"),
            profile_path: Some(base_path.to_string()),
            snapshot: HashMap::new(),
        }
    }

    pub fn load_from_disk(&mut self) {
        let _ = fs::create_dir_all(&self.memories_dir);

        let memory_entries = self.read_file(&self.memories_dir.join("MEMORY.md"));
        let user_entries = self.read_file(&self.memories_dir.join("USER.md"));

        let memory_path = self.memories_dir.join("MEMORY.md");
        let (memory_hash, hash_valid) = if memory_path.exists() {
            match fs::read_to_string(&memory_path) {
                Ok(raw) => {
                    let hash = format!("{:x}", Sha256::digest(raw.as_bytes()));
                    let expected = Self::read_expected_hash(&self.memories_dir.join("MEMORY.md.sha256"));
                    let valid = expected.as_ref().map_or(true, |e| e == &hash);
                    (Some(hash), valid)
                }
                Err(_) => (None, true),
            }
        } else {
            (None, true)
        };

        self.snapshot.insert(
            "memory_entries".to_string(),
            if memory_entries.is_empty() {
                "(no long-term memory records)".to_string()
            } else {
                let count = memory_entries.lines().filter(|l| l.starts_with("- ")).count();
                let chars = memory_entries.len();
                format!("{} records, {} chars", format_thousands(count), format_thousands(chars))
            },
        );

        self.snapshot.insert(
            "user_entries".to_string(),
            if user_entries.is_empty() {
                "(no user profile)".to_string()
            } else {
                let chars = user_entries.len();
                format!("{} chars", format_thousands(chars))
            },
        );

        if let Some(hash) = &memory_hash {
            self.snapshot.insert("memory_hash".to_string(), hash.clone());
        }
        self.snapshot.insert("hash_valid".to_string(), hash_valid.to_string());

        self.snapshot.insert("memory_raw".to_string(), memory_entries);
        self.snapshot.insert("user_raw".to_string(), user_entries);
    }

    pub fn get_snapshot(&self, key: &str) -> Option<&String> {
        self.snapshot.get(key)
    }

    pub fn get_memory_raw(&self) -> &str {
        self.snapshot.get("memory_raw").map(|s| s.as_str()).unwrap_or("")
    }

    pub fn get_user_raw(&self) -> &str {
        self.snapshot.get("user_raw").map(|s| s.as_str()).unwrap_or("")
    }

    pub fn memories_dir(&self) -> &Path {
        &self.memories_dir
    }

    fn read_file(&self, path: &Path) -> String {
        if path.exists() {
            fs::read_to_string(path).unwrap_or_default()
        } else {
            String::new()
        }
    }

    fn read_expected_hash(path: &Path) -> Option<String> {
        if path.exists() {
            fs::read_to_string(path).ok().map(|s| s.trim().to_string())
        } else {
            None
        }
    }
}
