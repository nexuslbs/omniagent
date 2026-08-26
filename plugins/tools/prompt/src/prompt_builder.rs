//! System prompt assembly: identity, tool guidance, memory.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::memory_store::MemoryStore;

// ── Plugin config ──────────────────────────────────────────────────────
//
// Config values are provided by the omniagent via the configure message
// at startup. Plugins never read env vars for config. Users can use
// $env: notation in plugins.yaml if they want values from env vars.

/// Plugin-configurable limits for prompt builder.
#[derive(Debug, Clone)]
pub struct PromptBuilderConfig {
    pub memory_max_chars: usize,
}

impl Default for PromptBuilderConfig {
    fn default() -> Self {
        Self {
            memory_max_chars: 5_000,
        }
    }
}

// ── Stable identity / guidance texts ────────────────────────────────────

fn build_dynamic_identity(tool_names: &[String]) -> String {
    // Generic tool listing: names come from the plugin registry, ALREADY
    // fully qualified (builtin_* / {server}_{tool}). This function must NOT
    // know any specific tool or plugin name - the registry provides the
    // names, and every name is the full name. No short-name knowledge here.
    let tool_list = if tool_names.is_empty() {
        "no tools available".to_string()
    } else {
        tool_names.join(", ")
    };

    format!("You are OmniAgent: precise, efficient, autonomous. Your tools: {tool_list}. Use minimum roundtrips. If a tool fails, move on: don't retry more than twice. HONESTY RULE: if you cannot complete the task, your final summary MUST clearly state that you gave up and why, and what remains undone - NEVER claim the task was completed unless every requested step was actually done and verified. NEVER end a turn with only thinking and no action: a response with no tool call is treated as the end of the task, so every turn MUST end with either tool calls or a final answer. If you have finished thinking, immediately emit your next tool call or your final answer - never stop after reasoning alone.")
}

const TOOL_GUIDANCE: &str = "TOOL USE RULES (fail the task if you violate these):\n\
1. CALL TOOLS DIRECTLY: Do NOT search the filesystem, read plugin config files, \
or inspect server configuration to discover what tools exist or how to call them. \
Available tools are listed with their name, description, and parameters in the \
function-calling API. Reading config files to find tools is always wrong and wastes turns.\n\
2. SEARCH BEFORE QUERY: Use search tools before querying databases for text or \
vector searches. Only use direct data queries for structured aggregations \
(counts, sums, averages, groupings).\n\
3. WRITE COMPLETE FILES: When writing a file, write the entire content in a single \
operation. Do NOT write placeholder content expecting to fill in values afterward. \
EXCEPTION - LARGE OUTPUTS: if the file content is too large to fit in a single \
response (approaching your output token limit), split it across multiple \
filesystem_write calls: first with append=false, then append=true for each \
subsequent chunk. Never abandon a large write - chunk it. Never let an output \
length limit cause task failure.\n\
4. RENAME INSTEAD OF RECREATE: When a file or directory already exists and you \
need to change its name, use the rename tool. Do NOT delete and recreate.\n\
5. NO POLLING: Do NOT repeatedly check the same condition. If you're waiting \
for something, make a single request and wait for the result.\n\
6. SET DIRECTLY: For configuration values, set the new value directly. Do NOT \
read the current value, flip it, and write it back.\n\
7. COMPLETE WORK: Before presenting results, finish ALL steps. Do not interrupt \
your work to show intermediate progress unless asked.\n\
8. CONFIRM DESTRUCTIVE ACTIONS: Before delete, overwrite, or stop operations, \
present what you will do and wait for confirmation.\n\
9. SKIP ON FAILURE: If an operation fails (network error, not found, bad request), \
try once more with a different approach, then move on. Do NOT retry the same \
failing call more than once. There is no hidden state that changes between retries.\n\
10. TAKE NOTES: maintain a durable working memory with the note_* tools \
(notes_note-write/notes_note-append/notes_note-read/notes_note-list/notes_note-rm) after every non-trivial \
discovery (paths, line numbers, commands, root causes, decisions). Notes \
survive compaction and thread death - the retry thread starts with them.\n\
11. VERIFY-ONCE: read a file ONCE with `filesystem_read` (offset/limit paging - ONE
call per page) and write the facts you need into your working notes; never re-read the
same file or line range. NEVER use `docker_compose exec ... sed -n` / `grep -n` to read
file contents: docker_compose is for RUNNING commands/builds, not reading files.
Re-reading overlapping line ranges of the same file is the #1 budget killer (threads have
died at 120/120 after 100+ sed windows with zero commits). Consult your notes, not the
disk, when you need content again.\n\
12. NEVER RE-READ CONTEXT DUMPS: a context-*.json dump is read ONCE per \
thread - a second read returns a '[duplicate read ...]' marker, not content. \
Trust the injected '=== Context Compacted ===' summary and your notes instead; \
re-reading dumps is a forbidden anti-loop that wastes iterations.\n\
13. SUBTASKS: after planning a multi-step task, create one subtask per plan step \
with the subtasks tool (subtasks_manage-subtasks, action=\"add\"); as you finish \
each step mark its subtask completed (action=\"update\", subtask_id=N, \
status=\"completed\"); cancel any subtask that is no longer needed \
(status=\"cancelled\"); before your final answer, complete or cancel ALL subtasks \
so none remain pending.";

fn build_active_profile_hint(profile_name: &str) -> String {
    format!("Active profile: {profile_name}.")
}

fn build_platform_hint(platform: &str) -> Option<&'static str> {
    match platform {
        "telegram" => Some("You are on a text messaging communication platform, Telegram. \
Standard markdown is automatically converted to Telegram format. Supported: **bold**, \
*italic*, ~~strikethrough~~, ||spoiler||, `inline code`, ```code blocks```, [links](url), \
and ## headers. Telegram has NO table syntax: prefer bullet lists or labeled key: value \
pairs over pipe tables (any tables you do emit are auto-rewritten into row-group bullets, \
which you can produce directly for cleaner output). You can send media files natively: \
to deliver a file to the user, include MEDIA:/absolute/path/to/file in your response. \
Images (.png, .jpg, .webp) appear as photos, audio (.ogg) sends as voice bubbles, and \
videos (.mp4) play inline. You can also include image URLs in markdown format ![alt](url) \
and they will be sent as native photos."),
        "mattermost" => Some("You are on a Mattermost messaging platform. Standard markdown formatting is supported: **bold**, *italic*, `code`, ```code blocks```, [links](url), headings, lists, tables, blockquotes. Mattermost supports most GFM (GitHub Flavored Markdown)."),
        _ => None,
    }
}

// ── Memory readings ─────────────────────────────────────────────────────

fn read_memory_section(memory_store: &MemoryStore, memory_max_chars: usize) -> String {
    let raw = memory_store.get_memory_raw();
    if raw.is_empty() {
        return String::new();
    }
    let truncated = truncate_content(raw, memory_max_chars);
    let header = if raw.chars().count() > memory_max_chars {
        format!(
            "## MEMORY (your personal notes) [TRUNCATED: showing first {} of {} chars]",
            memory_max_chars,
            raw.chars().count()
        )
    } else {
        format!(
            "## MEMORY (your personal notes) [{} chars]",
            raw.chars().count()
        )
    };
    format!("{header}\n{truncated}")
}

fn truncate_content(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }
    let truncate_at = content
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(content.len());
    format!(
        "{}...\n\n[... truncated from {} to ~{} chars]",
        &content[..truncate_at],
        content.chars().count(),
        max_chars
    )
}

/// Truncate content to `max_chars` Unicode scalar values (safe UTF-8 boundary).
pub fn truncate_content_pub(content: &str, max_chars: usize) -> String {
    truncate_content(content, max_chars)
}

// ── Prompt building ─────────────────────────────────────────────────────

/// Build the full system prompt string from all tiers.
pub fn build_system_prompt(
    memory_store: &MemoryStore,
    platform: &str,
    system_message: Option<&str>,
    profile_name: &str,
    tool_names: &[String],
    config: &PromptBuilderConfig,
) -> String {
    let parts = build_system_prompt_parts(
        memory_store,
        platform,
        system_message,
        profile_name,
        tool_names,
        config,
    );
    parts.join("\n\n")
}

/// Build the three-tier system prompt as separate parts.
pub fn build_system_prompt_parts(
    memory_store: &MemoryStore,
    platform: &str,
    system_message: Option<&str>,
    profile_name: &str,
    tool_names: &[String],
    config: &PromptBuilderConfig,
) -> Vec<String> {
    build_system_prompt_sections(
        memory_store,
        platform,
        system_message,
        profile_name,
        tool_names,
        config,
    )
    .into_iter()
    .map(|(_, _, text)| text)
    .collect()
}

/// Ordered, named system-prompt sections - `(name, order, text)` triples.
///
/// Task 9 contract: when the plugin config sets `emit_sections`, these are
/// returned to the omniagent core as `sections: [{name, order, text}]`, which
/// assembles the system prompt sorted by ascending `order` with per-thread
/// scope shadowing. The EMISSION ORDER (push order) is identical to
/// `build_system_prompt_parts` so the legacy joined rendering stays
/// byte-identical; the `order` values follow the convention (identity -100,
/// deployment/persona 0, tool guidance 100-199, channel/platform 200+,
/// memory last).
pub fn build_system_prompt_sections(
    memory_store: &MemoryStore,
    platform: &str,
    system_message: Option<&str>,
    profile_name: &str,
    tool_names: &[String],
    config: &PromptBuilderConfig,
) -> Vec<(String, i64, String)> {
    let mut sections: Vec<(String, i64, String)> = Vec::new();

    // Tier 1: Stable (identity -100, tool guidance 100, profile/persona 0)
    sections.push((
        "identity".to_string(),
        -100,
        build_dynamic_identity(tool_names),
    ));
    sections.push(("tool_guidance".to_string(), 100, TOOL_GUIDANCE.to_string()));
    sections.push((
        "profile".to_string(),
        0,
        build_active_profile_hint(profile_name),
    ));

    // Tier 2: Context / optional system message (deployment persona)
    if let Some(msg) = system_message {
        if !msg.is_empty() {
            sections.push(("deployment".to_string(), 10, msg.to_string()));
        }
    }

    // Tier 3: Volatile (platform 200, memory last)
    if let Some(hint) = build_platform_hint(platform) {
        sections.push(("platform".to_string(), 200, hint.to_string()));
    }

    let memory_section = read_memory_section(memory_store, config.memory_max_chars);
    if !memory_section.is_empty() {
        sections.push(("memory".to_string(), 300, memory_section));
    }

    sections
}

#[derive(Debug, Clone)]
pub struct PlanningPromptParams<'a> {
    pub platform: &'a str,
    pub profile_name: &'a str,
    pub user_message: &'a str,
    pub plan_iteration: u32,
    pub max_iterations: u32,
    pub previous_plan: Option<&'a str>,
    pub use_json_plan: bool,
}

/// Build a planning prompt for task decomposition.
pub fn build_planning_prompt(
    memory_store: &MemoryStore,
    p: PlanningPromptParams<'_>,
    tool_names: &[String],
) -> String {
    let tool_list = if tool_names.is_empty() {
        String::new()
    } else {
        format!("Your available tools: {}.", tool_names.join(", "))
    };

    let context = if p.plan_iteration == 0 {
        format!(
            "## Plan{iter_note}\n\
Before responding, create a high-level plan with numbered steps. \
{tool_list}\n\
Be specific about which tool to use and what parameters to pass. \
Aim for the minimum number of steps to complete the task. \
Wrap your plan in a <plan> block. After delivering the final answer, \
evaluate: if the task was completed, call the completion tool.",
            iter_note = if p.max_iterations > 1 {
                format!(" (iteration {}/{})", p.plan_iteration + 1, p.max_iterations)
            } else {
                String::new()
            }
        )
    } else {
        format!(
            "## Revised Plan (iteration {}/{})\n\
Your previous plan did not fully complete the task. \
Review what was done vs what remains. Identify the specific \
blockage and create a revised plan. Each step must include \
which tool to use and what parameters.\n\n\
Previous plan:\n{}",
            p.plan_iteration + 1,
            p.max_iterations,
            p.previous_plan.unwrap_or("(none)")
        )
    };

    let memory_info = {
        let memory_raw = memory_store.get_memory_raw();
        let mut parts = Vec::new();
        if !memory_raw.is_empty() {
            parts.push(format!("MEMORY: {} chars", memory_raw.len()));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("\nAvailable context:\n{}", parts.join("\n"))
        }
    };

    let user_msg = if p.user_message.is_empty() {
        String::new()
    } else {
        format!("\n\nUser request:\n{}", p.user_message)
    };

    format!("{context}{memory_info}{user_msg}")
}

// ── Subtask types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SubtaskStatus {
    Pending,
    Completed,
    Cancelled,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSubtask {
    pub description: String,
    pub status: SubtaskStatus,
}

pub fn format_subtask_section(subtasks: &[ThreadSubtask], thread_id: i64) -> Option<String> {
    if subtasks.is_empty() {
        return None;
    }
    let mut lines = vec![format!("## Subtasks (Thread #{thread_id})")];
    for (i, s) in subtasks.iter().enumerate() {
        let icon = match s.status {
            SubtaskStatus::Completed => "✅",
            SubtaskStatus::Cancelled => "❌",
            SubtaskStatus::Error => "⚠️",
            SubtaskStatus::Pending => "⬜",
        };
        lines.push(format!("{}. {} {}", i + 1, icon, s.description));
    }
    lines.push(String::new());
    Some(lines.join("\n"))
}

// ── Return type for build_system_prompt_parts ───────────────────────────

#[derive(Debug, Clone)]
pub struct PromptParts {
    pub parts: Vec<String>,
}

impl PromptParts {
    pub fn join(&self) -> String {
        self.parts.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_and_parts_are_byte_identical() {
        let store = MemoryStore::new(".");
        let sections = build_system_prompt_sections(
            &store,
            "mattermost",
            Some("system override"),
            "omni",
            &["fetch".to_string(), "filesystem_read".to_string()],
            &PromptBuilderConfig::default(),
        );
        let parts = build_system_prompt_parts(
            &store,
            "mattermost",
            Some("system override"),
            "omni",
            &["fetch".to_string(), "filesystem_read".to_string()],
            &PromptBuilderConfig::default(),
        );
        // Same push order → same joined text (legacy byte-identical).
        assert_eq!(
            sections
                .iter()
                .map(|(_, _, t)| t.clone())
                .collect::<Vec<_>>(),
            parts
        );
    }

    #[test]
    fn sections_carry_names_and_orders() {
        let store = MemoryStore::new(".");
        let sections = build_system_prompt_sections(
            &store,
            "telegram",
            None,
            "omni",
            &[],
            &PromptBuilderConfig::default(),
        );
        let names: Vec<&str> = sections.iter().map(|(n, _, _)| n.as_str()).collect();
        // identity, tool_guidance, profile, platform (no system_message, no memory)
        assert_eq!(
            names,
            vec!["identity", "tool_guidance", "profile", "platform"]
        );
        let identity = &sections[0];
        assert_eq!(identity.0, "identity");
        assert_eq!(identity.1, -100);
        assert!(identity.2.contains("You are OmniAgent"));
        assert_eq!(sections[1].1, 100, "tool guidance 100-199");
        assert_eq!(sections[2].1, 0, "profile/persona 0");
        assert_eq!(sections[3].1, 200, "platform 200+");
    }

    #[test]
    fn sections_include_deployment_and_memory_when_present() {
        let store = MemoryStore::new(".");
        let sections = build_system_prompt_sections(
            &store,
            "cli",
            Some("custom deployment msg"),
            "omni",
            &[],
            &PromptBuilderConfig::default(),
        );
        let names: Vec<&str> = sections.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["identity", "tool_guidance", "profile", "deployment"]
        );
        assert_eq!(sections[3].0, "deployment");
        assert_eq!(sections[3].1, 10);
        assert_eq!(sections[3].2, "custom deployment msg");
    }
}
