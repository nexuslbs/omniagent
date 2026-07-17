//! System prompt assembly: identity, tool guidance, memory, user profile.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::memory_store::MemoryStore;

// ── Character limits ────────────────────────────────────────────

fn memory_max_chars() -> usize {
    std::env::var("MEMORY_MAX_CHARS").ok().and_then(|v| v.parse().ok()).unwrap_or(5_000)
}
fn soul_max_chars() -> usize {
    std::env::var("SOUL_MAX_CHARS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000)
}

// ── Stable identity / guidance texts ────────────────────────────

fn build_dynamic_identity(tool_names: &[String]) -> String {
    let _has_filesystem = tool_names.iter().any(|n| n.starts_with("filesystem"));
    let has_fetch = tool_names.iter().any(|n| n == "fetch");
    let has_search = tool_names.iter().any(|n| n.starts_with("search_"));
    let has_query = tool_names.iter().any(|n| n.starts_with("query_"));
    let has_kanban = tool_names.iter().any(|n| n.starts_with("kanban"));
    let has_cron = tool_names.iter().any(|n| n.starts_with("cron"));
    let has_git = tool_names.iter().any(|n| {
        n.starts_with("commit") || n.starts_with("create_github") || n.starts_with("clone_repo") || n == "status"
    });
    let has_subtasks = tool_names.iter().any(|n| n.starts_with("manage_subtask"));
    let has_skills = tool_names.iter().any(|n| n.starts_with("create_skill") || n.starts_with("list_skills"));
    let has_plugin = tool_names.iter().any(|n| n == "plugin_manager" || n == "list_plugins");

    let mut parts: Vec<&str> = vec!["filesystem (read/write/list)"];
    if has_fetch { parts.push("fetch (HTTP)"); }
    if has_search { parts.push("search (messages/wiki)"); }
    if has_query { parts.push("query_database (SQL)"); }
    if has_kanban { parts.push("kanban"); }
    if has_cron { parts.push("cron"); }
    if has_git { parts.push("git"); }
    if has_subtasks { parts.push("manage_subtasks"); }
    if has_skills { parts.push("skills"); }
    if has_plugin { parts.push("plugin_manager"); }

    let is_categorized = |name: &str| -> bool {
        name.starts_with("filesystem") || name == "fetch" || name.starts_with("search_")
            || name.starts_with("query_") || name.starts_with("kanban") || name.starts_with("cron")
            || name.starts_with("commit") || name.starts_with("create_github")
            || name.starts_with("clone_repo") || name == "status"
            || name.starts_with("manage_subtask") || name.starts_with("create_skill")
            || name.starts_with("list_skills") || name == "plugin_manager" || name == "list_plugins"
            || name == "list_tool_details" || name == "compose"
            || name.starts_with("hindsight_") || name.starts_with("docker_")
            || name == "promote_to_memory" || name == "list_memories"
            || name == "review_memories" || name == "manage_memory"
            || name == "get_metrics" || name.starts_with("setup_") || name.starts_with("kanban_")
    };
    let extra: Vec<&str> = tool_names.iter().map(|s| s.as_str()).filter(|n| !is_categorized(n)).collect();
    if !extra.is_empty() {
        for e in &extra { parts.push(e); }
    }

    let tool_list = if parts.is_empty() { tool_names.join(", ") } else { parts.join(", ") };

    format!("You are OmniAgent: precise, efficient, autonomous. Your tools: {tool_list}. Use minimum roundtrips. If a tool fails, move on: don't retry more than twice.")
}

const TOOL_GUIDANCE: &str = "TOOL USE RULES (fail the task if you violate these):\n\
1. CALL TOOLS DIRECTLY: Do NOT search the filesystem, read plugin configs, \
read mcp-config.json files, inspect server.py files, or look at docker-compose files \
to discover what tools exist or how to call them. The function-calling API already \
shows you every available tool with its name, description, and parameters. \
If you need information about available tools, use the list_tool_details tool. \
Reading config files to find tools is always wrong and wastes turns.\n\
2. SEARCH BEFORE QUERY: Use search (search_messages, search_wiki) before \
query_database for text/vector searches. Only use query_database for structured \
aggregations (counts, sums, averages, groupings).\n\
3. WRITE SINGLE-FIELD FILES: When using filesystem_write_tool, write complete \
single-field content files. Do NOT write partial files and append later. Do NOT \
write placeholder content expecting to \"fill in\" values afterward.\n\
4. RENAME INSTEAD OF RECREATE: When a file/directory already exists and you \
need to change its name, rename it (filesystem_move). Do NOT delete and recreate.\n\
5. NO POLLING: Do NOT repeatedly check the same condition. If you're waiting \
for something, use the appropriate tool once and wait for the result.\n\
6. TOGGLE INSTEAD OF CONDITIONAL: For boolean/config values, use the toggle \
endpoint. Do NOT read the current value, compute the negation, and write it back.\n\
7. COMPLETE WORK: Before presenting results, finish ALL steps. Do not interrupt \
your work to show intermediate progress unless asked.\n\
8. CONFIRM DESTRUCTIVE ACTIONS: Before delete/overwrite/stop operations, \
present what you will do and wait for confirmation.\n\
9. SKIP ON FAILURE: If an operation fails (network error, not found, bad request), \
try once more with a different approach, then move on. Do NOT retry the same \
failing call more than once. There is no hidden state that changes between retries.";

fn build_active_profile_hint(profile_name: &str) -> String {
    format!("Active Hermes profile: {profile_name}.")
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

// ── Memory / profile readings ───────────────────────────────────

fn read_memory_section(memory_store: &MemoryStore) -> String {
    let raw = memory_store.get_memory_raw();
    if raw.is_empty() { return String::new(); }
    format!("## MEMORY (your personal notes)\n{}", raw)
}

fn read_user_profile_section(memory_store: &MemoryStore) -> String {
    let raw = memory_store.get_user_raw();
    if raw.is_empty() { return String::new(); }
    let truncated = truncate_content(raw, soul_max_chars());
    let header = if raw.len() > soul_max_chars() {
        format!("## USER PROFILE (who the user is) [TRUNCATED: showing first {} of {} chars]", soul_max_chars(), raw.len())
    } else {
        format!("## USER PROFILE (who the user is) [{}%: {}/{} chars]", 100, raw.len(), raw.len())
    };
    format!("{header}\n{truncated}")
}

fn truncate_content(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars { return content.to_string(); }
    let truncate_at = content.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(content.len());
    format!("{}...\n\n[... truncated from {} to ~{} chars]", &content[..truncate_at], content.len(), max_chars)
}

/// Truncate content to `max_chars` bytes (safe UTF-8 boundary).
pub fn truncate_content_pub(content: &str, max_chars: usize) -> String {
    truncate_content(content, max_chars)
}

// ── Prompt building ─────────────────────────────────────────────

/// Build the full system prompt string from all tiers.
pub fn build_system_prompt(
    memory_store: &MemoryStore,
    platform: &str,
    system_message: Option<&str>,
    profile_name: &str,
    tool_names: &[String],
) -> String {
    let parts = build_system_prompt_parts(memory_store, platform, system_message, profile_name, tool_names);
    parts.join("\n\n")
}

/// Build the three-tier system prompt as separate parts.
pub fn build_system_prompt_parts(
    memory_store: &MemoryStore,
    platform: &str,
    system_message: Option<&str>,
    profile_name: &str,
    tool_names: &[String],
) -> Vec<String> {
    let mut parts = Vec::new();

    // Tier 1: Stable
    let identity = build_dynamic_identity(tool_names);
    parts.push(identity);
    parts.push(TOOL_GUIDANCE.to_string());
    let profile_hint = build_active_profile_hint(profile_name);
    parts.push(profile_hint);

    // Tier 2: Context / optional system message
    if let Some(msg) = system_message {
        if !msg.is_empty() {
            parts.push(msg.to_string());
        }
    }

    // Tier 3: Volatile
    if let Some(hint) = build_platform_hint(platform) {
        parts.push(hint.to_string());
    }

    let memory_section = read_memory_section(memory_store);
    if !memory_section.is_empty() {
        parts.push(memory_section);
    }

    let user_section = read_user_profile_section(memory_store);
    if !user_section.is_empty() {
        parts.push(user_section);
    }

    parts
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
        let user_raw = memory_store.get_user_raw();
        let mut parts = Vec::new();
        if !memory_raw.is_empty() {
            parts.push(format!("MEMORY: {} chars", memory_raw.len()));
        }
        if !user_raw.is_empty() {
            parts.push(format!("USER PROFILE: {} chars", user_raw.len()));
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

// ── Subtask types ──────────────────────────────────────────────

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
    if subtasks.is_empty() { return None; }
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

// ── Return type for build_system_prompt_parts ───────────────────

#[derive(Debug, Clone)]
pub struct PromptParts {
    pub parts: Vec<String>,
}

impl PromptParts {
    pub fn join(&self) -> String {
        self.parts.join("\n\n")
    }
}
