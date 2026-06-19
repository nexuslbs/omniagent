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
2. BATCH every fetch into ONE turn. Need 4 GitHub repos + 4 READMEs + 4 GitHub APIs? \
Fetch all 12 in a SINGLE tool-calling round.\n\
3. NEVER fetch the same URL twice. If you already fetched a URL, USE its result. \
Do not re-fetch with different query params, do not try alternative APIs for the same data. \
The data you have is sufficient.\n\
4. TRUST YOUR RESULTS — once you have data, move forward. Don't second-guess.\n\
5. COMPLETE in 2-4 tool-calling rounds max for research. More than 6 means you failed at batching.\n\
6. READ the input file, DO the work, WRITE output, VERIFY, DONE. No detours.\n\
7. BEFORE fetching external data, ALWAYS use search_messages (to check \
past conversation history) and search_wiki (to check the project knowledge base). \
Existing knowledge may already cover the topic.\n\
8. Skip Critical-Instructions.md and Anti-Patterns.md — they are not needed for \
normal research tasks.\n\
9. OUTPUT QUALITY: When writing research-output.md, include clear headers, \
comparison tables where appropriate, and cite sources. Verify the file was written \
by reading it back with filesystem_read.\n\
\n\
FILESYSTEM ACCESS:\n\
- Read/write/search/list operations are allowed under TWO directories:\n\
  * data_dir=<data_dir> — agent config, profiles, wiki, memories\n\
  * /opt/workspace/ — project development (create, edit, manage project files)\n\
- Research output file path: <data_dir>/research-output.md (MUST write to this path).\n\
- To read the research input, use filesystem_read(path=\"<data_dir>/research-input.md\").\n\
- For project files, write to paths under /opt/workspace/.\n\
- Do NOT try to access paths under /app/ — they are outside the allowed directories.";

const RESEARCH_WORKFLOW: &str = "\
RESEARCH WORKFLOW (follow this exact sequence for research tasks):\n\
1. Read research-input.md to understand the requirements and output path.\n\
2. search_messages for relevant past research and existing knowledge.\n\
3. search_wiki for knowledge base articles on the topic.\n\
4. Fetch ALL external data in ONE batch — combine all HTTP fetches into a single\n\
   tool-calling round. Do NOT fetch one URL at a time.\n\
5. Write research-output.md with structured headers, comparison tables, and cited sources.\n\
6. Verify the output was written correctly by reading it back.";

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

const WIKI_GUIDANCE: &str = "Your wiki at <data_dir>/profiles/<profile>/wiki/ stores long-term knowledge. \
Use search_wiki to find relevant wiki pages. \
Use search_messages to find past conversations and research results. \
Both are available as MCP tools — check them before fetching external data.";

const DB_SCHEMA: &str = "DATABASE SCHEMA (PostgreSQL):\\n\\\
channels: id, name, platform, external_id, cause, metadata (JSONB), closed, created_at, updated_at\\n\\\
threads: id, channel_id, status, cause, profile, provider, model, \
input_tokens, cached_tokens, output_tokens, duration_ms, created_at, started_at, ended_at\\n\\\
messages: id, thread_id, role, content, thread_sequence, msg_type, msg_subtype, \
external_id, metadata (JSONB), embedding, summary_text, is_summary, \
processing_time_ms (tool latency), token_usage (JSONB), created_at\\n\\\
summaries: id, channel_id, next_thread_id, content (cross-thread summary text), created_at\\n\\\
\\n\\\
Key relationships: threads.channel_id → channels.id ; messages.thread_id → threads.id ; \
summaries.channel_id → channels.id\\n\\\
\\n\\\
The query_database MCP tool gives you read-only SQL access (SELECT only). Query these tables directly via \
sql-forge or SQL. Use search_messages for full-text search across messages. \
Use summaries to understand what happened in prior threads.";

const DOCKER_EXECUTION_GUIDANCE: &str = "DOCKER CODE EXECUTION: \
You can execute arbitrary code, run builds, install packages, and perform \
computations using Docker. The `compose` tool supports: ps, up, down, logs, \
build, exec, stop, restart, pull. \
\
TOOLBOX PATTERN: If the task needs tools not available in the agent container, \
create a docker-compose.yml with a 'toolbox' service in the workspace directory, \
build it, then use `compose exec toolbox <cmd>` to run anything. \
This keeps side-effects isolated, reproducible, and portable. \
\
EXISTING PROJECTS: If the workspace project already has a docker-compose.yml, \
use `compose exec <service> <cmd>` to run commands inside its containers. \
Prefer this over installing tools in the agent container — it keeps the agent \
image lean and projects self-contained.";

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
    stable_parts.push(RESEARCH_WORKFLOW.to_string());
    stable_parts.push(SKILLS_GUIDANCE.to_string());
    stable_parts.push(GROUNDING_POLICY.to_string());
    stable_parts.push(DOCKER_EXECUTION_GUIDANCE.to_string());
    stable_parts.push(DB_SCHEMA.to_string());

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

/// Build a lightweight planning prompt for the PROMPT_GRAPH phase.
///
/// This is a focused prompt that asks the LLM to produce a plan / context
/// specification before the actual execution. The plan is then injected
/// as context for the execution phase.
///
/// The user message is included as a reference so the LLM can scope its
/// plan appropriately, but it does NOT execute any tools here.
pub fn build_planning_prompt(
    memory_store: &MemoryStore,
    platform: &str,
    _profile_name: &str,
    user_message: &str,
    plan_iteration: u32,
    _max_iterations: u32,
    previous_plan: Option<&str>,
) -> String {
    // Base system identity — everything except tool guidance since
    // planning doesn't execute tools.
    let identity = DEFAULT_AGENT_IDENTITY;

    // Memory + user profile
    let mut volatile_parts: Vec<String> = Vec::new();
    if let Some(mem_block) = memory_store.format_for_system_prompt("memory") {
        volatile_parts.push(mem_block.to_string());
    }
    if let Some(user_block) = memory_store.format_for_system_prompt("user") {
        volatile_parts.push(user_block.to_string());
    }

    // Platform hint
    if let Some(hint) = platform_hint(platform) {
        volatile_parts.push(hint);
    }

    // Timestamp
    use chrono::Utc;
    let now = Utc::now();
    volatile_parts.push(format!("Conversation started: {}", now.format("%A, %B %d, %Y")));

    // Build the planning instruction
    let is_refinement = plan_iteration > 0 && previous_plan.is_some();
    let task_instruction = if is_refinement {
        format!(
            r#"You previously produced a plan for the user's request. \
Review it below and improve it. Fix any gaps or errors. \
If the plan is already complete and correct, respond with exactly:

PLAN_ACCEPTED

Otherwise, produce an improved plan.

Previous plan:
{prev}"#,
            prev = previous_plan.unwrap_or("")
        )
    } else {
        r#"You are in the PLANNING phase. Your job is to produce a detailed plan
for how to fulfill the user's request.

The plan should specify:
1. What tools or capabilities you will need
2. What data or resources you need to retrieve
3. The step-by-step approach
4. Any assumptions or preconditions

Produce a single, direct execution path. Do NOT include fallback approaches,
alternatives, or contingency plans — if the chosen path fails at execution
time, the execution phase will adapt naturally.

Format your plan as structured markdown with sections.
Do NOT execute any tools or produce code — only plan.

The user's request is provided below as a reference."#.to_string()
    };

    let volatile = volatile_parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    format!(
        r#"{identity}

{volatile}

## Planning Task

{task_instruction}

## User Request (reference)

{user_message}"#,
        identity = identity,
        volatile = volatile,
        task_instruction = task_instruction,
        user_message = user_message,
    )
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
