//! System prompt assembly — identity, tool guidance, memory, user profile.
//!
//! Three tiers joined with `\n\n`:
//! * **stable** — identity, tool guidance, active profile hint
//! * **context** — system message from caller (optional)
//! * **volatile** — MEMORY.md snapshot, USER.md snapshot, platform info, timestamp
//!
//! The system prompt is built **once per session** and cached. Only the volatile
//! tier changes between sessions (different memory content, timestamp). The stable
//! tier is constant for the lifetime of the agent process, keeping upstream
//! prefix caches warm across turns within a single tool-calling loop.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const ENTRY_DELIMITER: &str = "\n§\n";

// ── Character limits ────────────────────────────────────────────

/// Max characters for the MEMORY block in the system prompt.
const MEMORY_CHAR_LIMIT: usize = 4_500;
/// Max characters for the USER profile block in the system prompt.
const USER_CHAR_LIMIT: usize = 2_000;

// ── Stable identity / guidance texts ────────────────────────────

const DEFAULT_AGENT_IDENTITY: &str = "You are OmniAgent, an intelligent AI assistant \
that helps users with research, file management, web searches, and data analysis. \
You are helpful, knowledgeable, and direct. You assist users with a wide range of \
tasks including answering questions, writing and editing files, analyzing information, \
and executing actions via your tools. You communicate clearly, admit uncertainty \
when appropriate, and prioritize being genuinely useful.";

const TOOL_GUIDANCE: &str = "You have access to a set of tools to accomplish tasks. \
Use them proactively — do not describe what you would do without actually doing it. \
Each tool has a specific purpose described in its function definition. \
Read the tool descriptions carefully before choosing which tool to use. \
Write final results using the filesystem_write tool and include a summary of \
what was accomplished.";

const PROFILE_HINT: &str = "Active OmniAgent profile: default. \
Your profile configuration determines which model, provider, and tools are available. \
Profile data (memories, user profile) lives under the profile's data directory.";

/// Build the platform-specific hint based on channel metadata.
fn platform_hint(platform: &str) -> Option<String> {
    match platform.to_lowercase().as_str() {
        "telegram" => Some(
            "You are on a text messaging communication platform, Telegram. \
             Standard markdown is automatically converted to Telegram format. \
             Supported: **bold**, *italic*, ~~strikethrough~~, ||spoiler||, \
             `inline code`, ```code blocks```, and [links](url). \
             You can send media files natively by including MEDIA:/absolute/path/to/file \
             in your response."
                .to_string(),
        ),
        _ => None,
    }
}

// ── Memory Store ────────────────────────────────────────────────

/// Simple bounded memory store backed by MEMORY.md and USER.md files.
///
/// Maintains a frozen snapshot at load time for system prompt injection.
/// The snapshot is never mutated mid-session, keeping the prompt prefix cache stable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStore {
    /// Path to the profile's memories directory.
    memories_dir: PathBuf,
    /// Frozen snapshot for system prompt — set once at `load_from_disk()`.
    snapshot: HashMap<String, String>,
}

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

impl MemoryStore {
    /// Create a new MemoryStore for the given profile base path.
    ///
    /// The `memories_dir` is `<base_path>/memories/`.
    /// Does NOT load data — call `load_from_disk()` before using.
    pub fn new(base_path: &str) -> Self {
        Self {
            memories_dir: PathBuf::from(base_path).join("memories"),
            snapshot: HashMap::new(),
        }
    }

    /// Load entries from MEMORY.md and USER.md, capturing the system prompt snapshot.
    pub fn load_from_disk(&mut self) {
        let _ = fs::create_dir_all(&self.memories_dir);

        let memory_entries = self.read_file(&self.memories_dir.join("MEMORY.md"));
        let user_entries = self.read_file(&self.memories_dir.join("USER.md"));

        self.snapshot.insert("memory".to_string(), self.render_block("memory", &memory_entries, MEMORY_CHAR_LIMIT));
        self.snapshot.insert("user".to_string(), self.render_block("user", &user_entries, USER_CHAR_LIMIT));
    }

    /// Return the frozen snapshot block for the given target ("memory" or "user").
    /// Returns None if the snapshot has no content.
    pub fn format_for_system_prompt(&self, target: &str) -> Option<&str> {
        let block = self.snapshot.get(target)?;
        if block.is_empty() { None } else { Some(block.as_str()) }
    }

    // ── Internal helpers ──

    fn read_file(&self, path: &Path) -> Vec<String> {
        if !path.exists() {
            return vec![];
        }
        match fs::read_to_string(path) {
            Ok(content) => {
                let raw = content.trim().to_string();
                if raw.is_empty() {
                    return vec![];
                }
                raw.split(ENTRY_DELIMITER)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            Err(e) => {
                tracing::warn!("Failed to read memory file {:?}: {}", path, e);
                vec![]
            }
        }
    }

    fn render_block(&self, target: &str, entries: &[String], limit: usize) -> String {
        if entries.is_empty() {
            return String::new();
        }
        let content = entries.join(ENTRY_DELIMITER);
        let current = content.len();
        let pct = if limit > 0 {
            std::cmp::min(100, (current as f64 / limit as f64 * 100.0) as usize)
        } else {
            0
        };

        let header = if target == "user" {
            format!(
                "USER PROFILE (who the user is) [{}% — {}/{} chars]",
                pct, format_thousands(current), format_thousands(limit)
            )
        } else {
            format!(
                "MEMORY (your personal notes) [{}% — {}/{} chars]",
                pct, format_thousands(current), format_thousands(limit)
            )
        };

        let separator = "═".repeat(46);
        format!("{}\n{}\n{}\n{}", separator, header, separator, content)
    }
}

// ── System Prompt Builder ───────────────────────────────────────

/// Build the three-tier system prompt dict.
///
/// Returns a struct with `stable`, `context`, and `volatile` fields.
pub fn build_system_prompt_parts(
    memory_store: &MemoryStore,
    platform: &str,
    system_message: Option<&str>,
    profile_name: &str,
) -> PromptParts {
    use std::env;

    // ── Stable tier ────────────────────────────────────────────
    let mut stable_parts: Vec<String> = Vec::new();

    stable_parts.push(DEFAULT_AGENT_IDENTITY.to_string());
    stable_parts.push(TOOL_GUIDANCE.to_string());

    // Profile hint
    if profile_name == "default" {
        stable_parts.push(PROFILE_HINT.to_string());
    } else {
        stable_parts.push(format!(
            "Active OmniAgent profile: {}. This session reads and writes \
             profile data under the profile's directory.",
            profile_name
        ));
    }

    // Platform hint
    if let Some(hint) = platform_hint(platform) {
        stable_parts.push(hint);
    }

    // ── Context tier ───────────────────────────────────────────
    let mut context_parts: Vec<String> = Vec::new();

    if let Some(msg) = system_message {
        context_parts.push(msg.to_string());
    }

    // ── Volatile tier ──────────────────────────────────────────
    let mut volatile_parts: Vec<String> = Vec::new();

    if let Some(mem_block) = memory_store.format_for_system_prompt("memory") {
        volatile_parts.push(mem_block.to_string());
    }
    if let Some(user_block) = memory_store.format_for_system_prompt("user") {
        volatile_parts.push(user_block.to_string());
    }

    // Timestamp line
    use chrono::Utc;
    let now = Utc::now();
    let timestamp_line = format!(
        "Conversation started: {}",
        now.format("%A, %B %d, %Y")
    );
    // Try to add host info if available
    if let Ok(hostname) = env::var("HOSTNAME").or_else(|_| env::var("HOST")) {
        volatile_parts.push(format!("Host: {}", hostname));
    }
    if let Ok(cwd) = env::current_dir() {
        volatile_parts.push(format!("Working directory: {}", cwd.display()));
    }
    volatile_parts.push(timestamp_line);

    PromptParts {
        stable: stable_parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        context: context_parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        volatile: volatile_parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

/// Build the full system prompt string from all tiers.
pub fn build_system_prompt(
    memory_store: &MemoryStore,
    platform: &str,
    system_message: Option<&str>,
    profile_name: &str,
) -> String {
    let parts = build_system_prompt_parts(memory_store, platform, system_message, profile_name);
    let segments: Vec<&str> = [&parts.stable, &parts.context, &parts.volatile]
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str())
        .collect();
    segments.join("\n\n")
}

/// Result of `build_system_prompt_parts`.
pub struct PromptParts {
    pub stable: String,
    pub context: String,
    pub volatile: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let memories = dir.path().join("memories");
        fs::create_dir_all(&memories).unwrap();
        dir
    }

    #[test]
    fn test_empty_memory_store() {
        let dir = setup_test_dir();
        let mut store = MemoryStore::new(dir.path().to_str().unwrap());
        store.load_from_disk();
        assert!(store.format_for_system_prompt("memory").is_none());
        assert!(store.format_for_system_prompt("user").is_none());
    }

    #[test]
    fn test_memory_store_with_content() {
        let dir = setup_test_dir();
        let memories = dir.path().join("memories");
        fs::write(
            memories.join("MEMORY.md"),
            "User prefers concise responses\n§\nProject uses Rust with tokio",
        )
        .unwrap();

        let mut store = MemoryStore::new(dir.path().to_str().unwrap());
        store.load_from_disk();

        let block = store.format_for_system_prompt("memory").unwrap();
        assert!(block.contains("MEMORY"));
        assert!(block.contains("User prefers concise responses"));
        assert!(block.contains("Project uses Rust with tokio"));
    }

    #[test]
    fn test_build_system_prompt() {
        let dir = setup_test_dir();
        let memories = dir.path().join("memories");
        fs::write(memories.join("MEMORY.md"), "Test memory").unwrap();
        fs::write(memories.join("USER.md"), "Test user").unwrap();

        let mut store = MemoryStore::new(dir.path().to_str().unwrap());
        store.load_from_disk();

        let prompt = build_system_prompt(&store, "telegram", Some("Custom system message"), "default");
        assert!(prompt.contains("OmniAgent"));
        assert!(prompt.contains("Tool guidance"));
        assert!(prompt.contains("Test memory"));
        assert!(prompt.contains("Test user"));
        assert!(prompt.contains("Telegram"));
    }
}
