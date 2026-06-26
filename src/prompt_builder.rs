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

// ── Template loader ─────────────────────────────────────────────

/// Load a template file from `profiles/<name>/templates/<name>.md`.
/// Returns None if the file doesn't exist or is empty.
pub fn load_template(data_dir: &str, profile_name: &str, template_name: &str) -> Option<String> {
    if template_name.is_empty() {
        return None;
    }
    let path: PathBuf = [data_dir, "profiles", profile_name, "templates", template_name]
        .iter()
        .collect();
    // Try with .md extension if not already present
    let path = if path.extension().is_some() {
        path
    } else {
        let mut with_ext = path;
        with_ext.set_extension("md");
        with_ext
    };
    if !path.exists() {
        tracing::warn!("Template file not found: {:?}", path);
        return None;
    }
    match fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Err(e) => {
            tracing::warn!("Failed to read template {:?}: {}", path, e);
            None
        }
    }
}

// ── Character limits ────────────────────────────────────────────

/// Read MEMORY_MAX_CHARS from env, default 5_000.
fn memory_max_chars() -> usize {
    std::env::var("MEMORY_MAX_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000)
}

/// Read USER_MAX_CHARS from env, default 1_000.
fn user_max_chars() -> usize {
    std::env::var("USER_MAX_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000)
}

// ── Stable identity / guidance texts ────────────────────────────

const DEFAULT_AGENT_IDENTITY: &str = "You are OmniAgent — precise, efficient, autonomous. \
Your tools: filesystem (read/write/list), compose (docker build/up/exec/logs), \
fetch (HTTP), search (messages/wiki), query_database (SQL), kanban, cron, git. \
Use minimum roundtrips. If a tool fails, move on — don't retry more than twice.";

const TOOL_GUIDANCE: &str = "TOOL USE RULES (fail the task if you violate these):\n\
1. PLAN before acting — decide ALL data needed in one shot.\n\
2. BATCH every fetch into ONE turn. Need 4 repos + 4 READMEs? \
Fetch all 8 in a SINGLE tool-calling round.\n\
3. NEVER fetch the same URL twice. If you already fetched a URL, USE its result. \
Do not re-fetch with different query params, do not try alternative APIs for the same data. \
The data you have is sufficient.\n\
4. TRUST YOUR RESULTS — once you have data, move forward. Don't second-guess.\n\
5. If a tool call returns an error, **move on immediately**. \
Do not retry the same tool more than twice total. \
A tool that fails consistently will not start working on the 5th try.\n\
6. ACTION over exploration — when given a specific task to execute \
(e.g. \"build a blog\", \"run docker compose\"), DO IT directly. \
Do not search past messages for context you don't need. \
Do not list the workspace repeatedly. Start writing code and building. \
**For build tasks: call docker_compose(build) then docker_compose(up -d) \
in the same turn — do NOT read every file first.** \
A docker build failure tells you exactly what's wrong; fix and rebuild.\\n\\\
7. FINAL MESSAGE = SUMMARY: After all tool calls complete, your final text \
response must be a concise summary of what was accomplished. Cover key results, \
decisions, and any follow-up actions needed. This replaces the need for a \
separate summarization step.";
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

// Compact summary (not raw DDL) — ~150 chars vs 500+ for the full schema.
const DB_SCHEMA: &str = "DATABASE SCHEMA SUMMARY:\n\
Tables: channels (config per conversation space), threads (per-topic runs), \
messages (per-turn content), summaries (cross-thread), cron_jobs (schedules), \
kanban_tasks (board items), thread_subtasks (step tracking), actions (tools).\n\
Key FK: messages.thread_id→threads.id, threads.channel_id→channels.id, \
threads.task_id→kanban_tasks.id, cron_jobs.channel_id→channels.id.\n\
Use query_database (SELECT-only SQL) for structured data, search_messages for \
full-text across messages, search_wiki for project knowledge.";

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
    /// Profile base path (<data_dir>/profiles/<name>), used to locate wiki/relevant-index.md.
    profile_path: Option<String>,
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
            profile_path: Some(base_path.to_string()),
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
            self.render_block("memory", &memory_entries, memory_max_chars()),
        );
        self.snapshot.insert(
            "user".to_string(),
            self.render_block("user", &user_entries, user_max_chars()),
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

    /// Load relevant-index.md from the profile's wiki directory and return as a block.
    /// Returns None if the file doesn't exist or is empty.
    pub fn load_relevant_index(&self) -> Option<String> {
        let profile_path = match self.profile_path {
            Some(ref p) => p.clone(),
            None => return None,
        };
        let wiki_index_path = PathBuf::from(&profile_path).join("wiki").join("relevant-index.md");
        if !wiki_index_path.exists() {
            return None;
        }
        match fs::read_to_string(&wiki_index_path) {
            Ok(content) => {
                let trimmed = content.trim().to_string();
                if trimmed.is_empty() || trimmed.contains("(No wiki pages found)") {
                    return None;
                }
                Some(format!(
                    "RELEVANT WIKI PAGES (most important wiki files for context):\n{}",
                    trimmed
                ))
            }
            Err(e) => {
                tracing::warn!("Failed to read relevant-index.md {:?}: {}", wiki_index_path, e);
                None
            }
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
        let truncated = if current > limit {
            let truncate_at = content
                .char_indices()
                .nth(limit)
                .map(|(i, _)| i)
                .unwrap_or(content.len());
            content[..truncate_at].to_string()
        } else {
            content.clone()
        };
        let effective = truncated.len();
        let pct = if limit > 0 {
            std::cmp::min(100, (current as f64 / limit as f64 * 100.0) as usize)
        } else {
            0
        };

        let header = if target == "user" {
            format!(
                "USER PROFILE (who the user is) [{}% — {}/{} chars]",
                pct,
                format_thousands(effective),
                format_thousands(limit)
            )
        } else {
            format!(
                "MEMORY (your personal notes) [{}% — {}/{} chars]",
                pct,
                format_thousands(effective),
                format_thousands(limit)
            )
        };

        let separator = "═".repeat(46);
        format!("{}\n{}\n{}\n{}", separator, header, separator, truncated)
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
    let mut stable_parts: Vec<String> = vec![
        DEFAULT_AGENT_IDENTITY.to_string(),
        TOOL_GUIDANCE.to_string(),
        GROUNDING_POLICY.to_string(),
        DB_SCHEMA.to_string(),
    ];

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
    //
    // MEMORY.md and USER.md are loaded inside delimiter markers to signal
    // that they are locked instructions, not optional context. This
    // leverages the primacy effect — content with strong delimiter markers
    // receives more attention from the model.
    let mut locked_parts: Vec<String> = Vec::new();
    if let Some(mem_block) = memory_store.format_for_system_prompt("memory") {
        locked_parts.push(mem_block.to_string());
    }
    if let Some(user_block) = memory_store.format_for_system_prompt("user") {
        locked_parts.push(user_block.to_string());
    }

    let mut volatile_parts: Vec<String> = Vec::new();

    if !locked_parts.is_empty() {
        let locked_content = locked_parts.join("\n\n");
        volatile_parts.push(format!(
            "═══ LOCKED INSTRUCTIONS (FOLLOW EXACTLY) ═══\n{}",
            locked_content
        ));
    }

    // ── Reference context (below locked instructions, for reference only) ──
    let mut ref_parts: Vec<String> = Vec::new();

    // Relevant wiki index (compact listing of most important wiki pages)
    if let Some(relevant_block) = memory_store.load_relevant_index() {
        ref_parts.push(relevant_block);
    }

    // Timestamp line
    use chrono::Utc;
    let now = Utc::now();
    let timestamp_line = format!("Conversation started: {}", now.format("%A, %B %d, %Y"));
    // Try to add host info if available
    if let Ok(hostname) = env::var("HOSTNAME").or_else(|_| env::var("HOST")) {
        ref_parts.push(format!("Host: {}", hostname));
    }
    if let Ok(cwd) = env::current_dir() {
        ref_parts.push(format!("Working directory: {}", cwd.display()));
    }
    ref_parts.push(timestamp_line);

    if !ref_parts.is_empty() {
        if ref_parts.len() == 1 {
            volatile_parts.push(ref_parts.remove(0));
        } else {
            volatile_parts.push(ref_parts.join("\n\n"));
        }
    }

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

/// Parameters for [`build_planning_prompt`].
pub struct PlanningPromptParams<'a> {
    pub platform: &'a str,
    #[allow(dead_code)]
    pub profile_name: &'a str,
    pub user_message: &'a str,
    pub plan_iteration: u32,
    #[allow(dead_code)]
    pub max_iterations: u32,
    pub previous_plan: Option<&'a str>,
    pub use_json_plan: bool,
}

/// Build a lightweight planning prompt for the PROMPT_PLAN phase.
///
/// This is a focused prompt that asks the LLM to produce a plan / context
/// specification before the actual execution. The plan is then injected
/// as context for the execution phase.
///
/// The user message is included as a reference so the LLM can scope its
/// plan appropriately, but it does NOT execute any tools here.
pub fn build_planning_prompt(
    memory_store: &MemoryStore,
    p: PlanningPromptParams<'_>,
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
    if let Some(hint) = platform_hint(p.platform) {
        volatile_parts.push(hint);
    }

    // Timestamp
    use chrono::Utc;
    let now = Utc::now();
    volatile_parts.push(format!("Conversation started: {}", now.format("%A, %B %d, %Y")));

    // Build the planning instruction
    let is_refinement = p.plan_iteration > 0 && p.previous_plan.is_some();
    let task_instruction = if is_refinement {
        format!(
            r#"You previously produced a plan for the user's request. \
Review it below and improve it. Fix any gaps or errors. \
If the plan is already complete and correct, respond with exactly:

PLAN_ACCEPTED

Otherwise, produce an improved plan.

Previous plan:
{prev}"#,
            prev = p.previous_plan.unwrap_or("")
        )
    } else if p.use_json_plan {
        r#"You are in the PLANNING phase. Your job is to produce a detailed execution plan
for the user's request.

Reply with a JSON object (and ONLY valid JSON — no surrounding markdown, no backticks) with the following structure:

{
  "description": "Brief summary of your overall approach (1-2 sentences)",
  "steps": [
    "Step 1: what to do first",
    "Step 2: what to do next",
    "Step 3: what to do after"
  ]
}

The plan should specify:
1. What tools or capabilities you will need
2. What data or resources you need to retrieve
3. The step-by-step approach
4. Any assumptions or preconditions

Each step should be a clear, actionable description. Keep steps concise (under 200 chars each).
Aim for 3-6 steps. Do NOT include fallback approaches, alternatives, or contingency plans
— if the chosen path fails at execution time, the execution phase will adapt naturally.

IMPORTANT: Each step in "steps" will be automatically converted into a tracked subtask.
During execution, you MUST call `manage_subtasks(action="update", subtask_id=N, status="completed")`
for each step as you finish it, or `manage_subtasks(action="update", subtask_id=N, status="cancelled")`
if a step becomes irrelevant. Use `manage_subtasks(action="list")` to see current state.
If you reach the end of execution with any subtask still pending, the thread will be marked as FAILED.

Do NOT execute any tools or produce code — only plan.

The user's request is provided below as a reference."#.to_string()
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
        user_message = p.user_message,
    )
}

// ── Subtask Formatting ─────────────────────────────────────────

/// Status of a subtask within a thread's execution plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SubtaskStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
    Error,
}

/// A single subtask in a thread's execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSubtask {
    /// Display name of this subtask.
    pub name: String,
    /// Current status.
    pub status: SubtaskStatus,
    /// Zero-based index in the ordered list of subtasks.
    pub step_index: usize,
    /// Total number of steps in the plan.
    pub total_steps: usize,
}

/// Format a list of subtasks as a markdown section for the system prompt.
///
/// Returns `None` when the list is empty — no section is added.
/// When subtasks exist, formats them as:
///
/// ```text
/// ## Current Task Progress
/// Thread: 12345
/// 🔴 Subtask Name  (step 2 of 5)
///
///   1. ✅ Name
///   2. 🔄 Name  ← current
///   3. ⏳ Name
///   4. ⏳ Name
///   5. ⏳ Name
/// ```
///
/// Status emoji mapping:
/// - `completed` → ✅
/// - `in_progress` → 🔄
/// - `pending` → ⏳
/// - `cancelled` → ❌
pub fn format_subtask_section(subtasks: &[ThreadSubtask], thread_id: i64) -> Option<String> {
    if subtasks.is_empty() {
        return None;
    }

    // Find the current (in_progress) subtask
    let current_idx = subtasks.iter().position(|s| s.status == SubtaskStatus::InProgress);
    let current_name = current_idx.and_then(|idx| {
        let s = &subtasks[idx];
        // Use total_steps from the current subtask for display
        Some(format!("{}  (step {} of {})", s.name, idx + 1, s.total_steps))
    });

    // Build the step list
    let mut steps = String::new();
    for (i, subtask) in subtasks.iter().enumerate() {
        let emoji = match subtask.status {
            SubtaskStatus::Completed => "✅",
            SubtaskStatus::InProgress => "🔄",
            SubtaskStatus::Pending => "⏳",
            SubtaskStatus::Cancelled => "❌",
            SubtaskStatus::Error => "💥",
        };
        let current_marker = if subtask.status == SubtaskStatus::InProgress {
            "  ← current"
        } else {
            ""
        };
        steps.push_str(&format!("  {}. {} {}{}\n", i + 1, emoji, subtask.name, current_marker));
    }

    // Build the subtask management instruction block
    let management_instruction = if subtasks.iter().any(|s| s.status == SubtaskStatus::Pending) {
        "\n\n## Subtask Tracking Rules\n\
         You MUST call `manage_subtasks(action=\"update\", subtask_id=N, status=\"completed\")` \
         each time you finish a subtask.\n\
         If a subtask becomes irrelevant, call `manage_subtasks(action=\"update\", subtask_id=N, status=\"cancelled\")`.\n\
         If a subtask encounters an unrecoverable error, call `manage_subtasks(action=\"update\", subtask_id=N, status=\"error\")` \
         and continue to the next subtask — do NOT abort the entire thread.\n\
         Use `manage_subtasks(action=\"list\")` to refresh the current state at any point.\n\
         Before delivering your final answer, ALL subtasks must be either `completed`, `cancelled`, or `error` — \
         never leave any subtask in `pending` status."
            .to_string()
    } else {
        String::new()
    };

    // Assemble the section
    let section = if let Some(ref cur) = current_name {
        format!(
            "## Current Task Progress\n\
             Thread: {}\n\
             🔴 {} \n\
             \n\
             {}{}",
            thread_id,
            cur,
            steps.trim_end(),
            management_instruction,
        )
    } else {
        format!(
            "## Current Task Progress\n\
             Thread: {}\n\
             \n\
             {}{}",
            thread_id,
            steps.trim_end(),
            management_instruction,
        )
    };

    Some(section)
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
