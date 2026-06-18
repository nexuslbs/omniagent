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
const MEMORY_CHAR_LIMIT: usize = 2_500;
/// Max characters for the USER profile block in the system prompt.
const USER_CHAR_LIMIT: usize = 1_000;

// ── Stable identity / guidance texts ────────────────────────────

const DEFAULT_AGENT_IDENTITY: &str = "You are OmniAgent — precise, efficient, autonomous. \
Your tools: filesystem, HTTP fetch, search. Use minimum roundtrips.";

const TOOL_GUIDANCE: &str = "TOOL USE RULES (fail the task if you violate these):\n\
1. PLAN before acting — decide ALL data needed in one shot.\n\
2. BATCH every fetch into ONE turn. Need 4 GitHub repos + 4 crates + 4 shields? \
Fetch all 12 in a SINGLE tool-calling round.\n\
3. NEVER fetch the same URL twice. If you already fetched a URL, USE its result. \
Do not re-fetch with different query params, do not try alternative APIs for the same data. \
The data you have is sufficient.\n\
4. TRUST YOUR RESULTS — once you have data, move forward. Don't second-guess.\n\
5. COMPLETE in 3-5 tool-calling rounds max. More than 10 means you failed at batching.\n\
6. READ the input file, DO the work, WRITE output, VERIFY, DONE. No detours.\n\
7. Skip Critical-Instructions.md and Anti-Patterns.md — they are not needed for \
normal research tasks.";

const SKILLS_GUIDANCE: &str = "";

/// Grounding policy — instructions for evidence-based answers.
const GROUNDING_POLICY: &str = "GROUNDING POLICY:\n\
1. Prefer retrieved evidence over prior assumptions — when evidence is \
available in your context, cite it explicitly.\n\
2. If uncertain about a factual or project-specific claim, state your \
uncertainty clearly. Do not fabricate details.\n\
3. For factual/project-specific claims, provide grounding references \
whenever possible (message IDs, wiki file paths, tool call IDs).\n\
4. If you lack sufficient evidence to answer, either ask a clarifying \
question or trigger a search/retrieval tool before responding.";

const WIKI_GUIDANCE: &str = "Your wiki at <data_dir>/profiles/<profile>/wiki/ stores long-term knowledge.";

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
        if i > 0 && (s.len() - i).is_multiple_of(3) {
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

        self.snapshot.insert(
            "memory".to_string(),
            self.render_block("memory", &memory_entries, MEMORY_CHAR_LIMIT),
        );
        self.snapshot.insert(
            "user".to_string(),
            self.render_block("user", &user_entries, USER_CHAR_LIMIT),
        );
    }

    /// Return the frozen snapshot block for the given target ("memory" or "user").
    /// Returns None if the snapshot has no content.
    pub fn format_for_system_prompt(&self, target: &str) -> Option<&str> {
        let block = self.snapshot.get(target)?;
        if block.is_empty() {
            None
        } else {
            Some(block.as_str())
        }
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
                pct,
                format_thousands(current),
                format_thousands(limit)
            )
        } else {
            format!(
                "MEMORY (your personal notes) [{}% — {}/{} chars]",
                pct,
                format_thousands(current),
                format_thousands(limit)
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
    stable_parts.push(SKILLS_GUIDANCE.to_string());
    stable_parts.push(GROUNDING_POLICY.to_string());

    // Wiki guidance with the actual data directory
    let wiki_hint = WIKI_GUIDANCE.replace("<profile>", profile_name);
    let data_dir = env::var("OMNI_DATA_DIR").unwrap_or_else(|_| "/opt/data".to_string());
    stable_parts.push(wiki_hint.replace("<data_dir>", &data_dir));

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
    let timestamp_line = format!("Conversation started: {}", now.format("%A, %B %d, %Y"));
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

        let prompt =
            build_system_prompt(&store, "telegram", Some("Custom system message"), "default");
        assert!(prompt.contains("OmniAgent"));
        assert!(prompt.contains("GROUNDING POLICY"));
        assert!(prompt.contains("Test memory"));
        assert!(prompt.contains("Test user"));
        assert!(prompt.contains("Telegram"));
    }
}
