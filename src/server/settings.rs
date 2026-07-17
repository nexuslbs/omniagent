//! Settings API: read/write settings organized by category.
//!
//! Settings values are stored in `settings.yml` (same directory as `plugins.yml`),
//! with support for `$env:VAR` and `$secret:NAME` notation to indirectly
//! reference environment variables or DB-stored secrets.
//!
//! Four bootstrap settings are always read-only and come directly from process
//! environment variables: `host`, `port`, `database_url`, `omni_dir`.
//!
//! - `GET /settings`: returns all settings with metadata, values resolved
//! - `PUT /settings`: updates one or more values and writes to settings.yml

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use super::AppState;

use crate::plugins_yaml;

/// A single option for a select-type setting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingOption {
    pub id: String,
    pub name: String,
}

/// Metadata describing how a setting should be rendered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingMeta {
    /// Rendering type: "text", "number", "boolean", "secret", "select", "textarea"
    #[serde(rename = "type")]
    pub field_type: String,
    /// Human-readable description
    pub description: String,
    /// Options for select type
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<SettingOption>>,
    /// Whether the setting is read-only
    #[serde(default)]
    pub readonly: bool,
    /// Default value if not set in settings.yml and not set via env
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// A single setting entry with its current value and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingEntry {
    pub name: String,
    pub value: String,
    pub metadata: SettingMeta,
}

/// A category grouping related settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingCategory {
    pub name: String,
    pub label: String,
    pub settings: Vec<SettingEntry>,
}

/// Response from GET /settings
#[derive(Debug, Serialize)]
pub struct SettingsResponse {
    pub categories: Vec<SettingCategory>,
}

/// Request body for PUT /settings
#[derive(Debug, Deserialize)]
pub struct UpdateSettingsRequest {
    pub updates: Vec<SettingUpdate>,
}

#[derive(Debug, Deserialize)]
pub struct SettingUpdate {
    pub name: String,
    pub value: String,
}

/// Build the router for /settings endpoints using the shared AppState.
#[allow(dead_code)]
pub fn settings_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(get_settings_handler))
        .route("/", put(update_settings_handler))
}

/// Path to the settings.yml file relative to data_dir.
fn settings_path(data_dir: &str) -> String {
    format!("{}/settings.yml", data_dir)
}

/// Load settings.yml as a flat key-value map.
/// Returns an empty map if the file doesn't exist or can't be parsed.
pub(crate) fn load_settings_file(data_dir: &str) -> HashMap<String, String> {
    let path = settings_path(data_dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    // Parse YAML as a mapping of string → string
    let raw: serde_yaml::Value = match serde_yaml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };

    let mut map = HashMap::new();
    if let serde_yaml::Value::Mapping(mapping) = raw {
        for (key, value) in mapping {
            let k = match key.as_str() {
                Some(s) => s.to_lowercase(),
                None => continue,
            };
            let v = match value.as_str() {
                Some(s) => s.to_string(),
                None => {
                    // Non-string values: serialize back to YAML string
                    serde_yaml::to_string(&value)
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                }
            };
            map.insert(k, v);
        }
    }
    map
}

/// Write a key-value map to settings.yml.
fn write_settings_file(data_dir: &str, vars: &HashMap<String, String>) -> Result<(), String> {
    let path = settings_path(data_dir);

    // Build a YAML mapping preserving insertion order (sorted by key)
    let mut sorted_keys: Vec<&String> = vars.keys().collect();
    sorted_keys.sort();

    let mut content = String::from("# Settings for OmniAgent\n");
    content.push_str("# Values support $env:VAR and $secret:NAME refs.\n\n");

    for key in sorted_keys {
        if let Some(value) = vars.get(key) {
            // If value contains special chars or starts with $, quote it
            let formatted = if value.starts_with('$')
                || value.contains(':')
                || value.contains('#')
                || value.is_empty()
            {
                format!("'{}'\n", value.replace('\'', "''"))
            } else if value.contains(' ') || value.contains('\n') {
                format!("'{}'\n", value.replace('\'', "''"))
            } else {
                format!("{}\n", value)
            };
            content.push_str(&format!("{}: {}\n", key, formatted.trim_end()));
        }
    }

    std::fs::write(&path, content).map_err(|e| format!("Failed to write settings.yml: {}", e))
}

/// Convert a lower_snake_case setting name to UPPER_SNAKE_CASE for env var lookup.
fn setting_name_to_env(name: &str) -> String {
    name.to_uppercase()
}

/// The canonical list of all settings with their metadata (no values).
/// Values are resolved at request time from settings.yml + env vars.
fn get_all_setting_definitions() -> Vec<(String, SettingMeta)> {
    vec![
        // ── General ──
        (
            "max_tokens".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Maximum tokens per LLM response".into(),
                options: None,
                readonly: false,
                default: Some("4096".into()),
            },
        ),
        (
            "temperature".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "LLM sampling temperature (0.0 – 2.0)".into(),
                options: None,
                readonly: false,
                default: Some("0.7".into()),
            },
        ),
        (
            "max_iterations_no_plan".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Max tool-call iterations for threads with no planning".into(),
                options: None,
                readonly: false,
                default: Some("30".into()),
            },
        ),
        (
            "max_iterations_plan".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Max tool-call iterations for threads with planning enabled".into(),
                options: None,
                readonly: false,
                default: Some("120".into()),
            },
        ),
        (
            "tool_short_timeout_secs".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Short timeout in seconds for MCP tool calls before switching to background mode".into(),
                options: None,
                readonly: false,
                default: Some("5".into()),
            },
        ),
        (
            "tool_long_timeout_secs".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Long timeout in seconds for background MCP tool execution".into(),
                options: None,
                readonly: false,
                default: Some("300".into()),
            },
        ),
        (
            "max_unfinished_subtask_retries".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Max retries before marking a thread as failed when subtasks remain unfinished or plan JSON is invalid".into(),
                options: None,
                readonly: false,
                default: Some("3".into()),
            },
        ),
        (
            "prompt_generate_tool".into(),
            SettingMeta {
                field_type: "select".into(),
                description: "Name of the MCP tool to call for generating prompts".into(),
                options: None,
                readonly: false,
                default: Some("prompt_generate".into()),
            },
        ),
        (
            "prompt_compact_messages_tool".into(),
            SettingMeta {
                field_type: "select".into(),
                description: "Name of the MCP tool to call for compacting conversation history".into(),
                options: None,
                readonly: false,
                default: Some("prompt_compact-messages".into()),
            },
        ),
        // ── Memory & Retention ──
        (
            "memory_max_chars".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Max characters for MEMORY.md in the system prompt".into(),
                options: None,
                readonly: false,
                default: Some("5000".into()),
            },
        ),
        (
            "soul_max_chars".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Max characters for SOUL.md in the system prompt".into(),
                options: None,
                readonly: false,
                default: Some("1000".into()),
            },
        ),
        (
            "delete_after_days".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Days before old messages and summaries are deleted".into(),
                options: None,
                readonly: false,
                default: Some("30".into()),
            },
        ),
        (
            "thread_summary_tokens".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Maximum tokens for per-thread end-of-execution summary".into(),
                options: None,
                readonly: false,
                default: Some("2048".into()),
            },
        ),
        // ── Connections ──
        (
            "max_pool_connections".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Maximum per-channel connections per MCP server. Each channel gets its own pool; this caps how many simultaneous tool calls a single server can handle per channel. Increase for multi-tool workloads, decrease to save memory. Minimum 1.".into(),
                options: None,
                readonly: false,
                default: Some("5".into()),
            },
        ),
        (
            "max_inline_file_kb".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Maximum file size (KB) for inline file content in inbound messages. Files larger than this are listed as metadata only. The agent can still read them via MCP tools.".into(),
                options: None,
                readonly: false,
                default: Some("100".into()),
            },
        ),
        // ── General (LLM Provider) ──
        (
            "llm_provider".into(),
            SettingMeta {
                field_type: "select".into(),
                description: "Default LLM provider backend for channels without an explicit provider".into(),
                options: Some(vec![
                    SettingOption { id: "opencode-go".into(), name: "OpenCode Go".into() },
                    SettingOption { id: "openai".into(), name: "OpenAI".into() },
                    SettingOption { id: "anthropic".into(), name: "Anthropic".into() },
                    SettingOption { id: "deepseek".into(), name: "DeepSeek".into() },
                ]),
                readonly: false,
                default: Some("opencode-go".into()),
            },
        ),
        // API keys are now managed per-provider via the Providers page,
        // not settings.yml.
        // ── System (read-only bootstrap from env vars) ──
        (
            "host".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "HTTP server bind address (read-only, from env HOST)".into(),
                options: None,
                readonly: true,
                default: Some("0.0.0.0".into()),
            },
        ),
        (
            "port".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "HTTP server port (read-only, from env PORT)".into(),
                options: None,
                readonly: true,
                default: Some("8080".into()),
            },
        ),
        (
            "database_url".into(),
            SettingMeta {
                field_type: "secret".into(),
                description: "PostgreSQL connection string (read-only, from env DATABASE_URL)".into(),
                options: None,
                readonly: true,
                default: Some("".into()),
            },
        ),
        (
            "omni_dir".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Data directory for profiles and wiki (read-only, from env OMNI_DIR)".into(),
                options: None,
                readonly: true,
                default: Some("".into()),
            },
        ),
        // ── Context Management ──
        (
            "prompt_char_budget_soft".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Soft char budget for prompts. When exceeded, context is condensed every N turns.".into(),
                options: None,
                readonly: false,
                default: Some("350000".into()),
            },
        ),
        (
            "prompt_char_budget_hard".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Hard char budget for prompts. When exceeded, context is condensed before the next LLM call.".into(),
                options: None,
                readonly: false,
                default: Some("500000".into()),
            },
        ),
        (
            "old_message_char_budget".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Max chars for old messages after condensation. The metadata block stays in full.".into(),
                options: None,
                readonly: false,
                default: Some("100000".into()),
            },
        ),
        (
            "state_block_update_interval".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "How often (in iterations) to refresh the state block when soft budget is exceeded.".into(),
                options: None,
                readonly: false,
                default: Some("5".into()),
            },
        ),
        (
            "condense_keep_turns".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Number of full assistant→tool cycles to keep verbatim during condensation.".into(),
                options: None,
                readonly: false,
                default: Some("4".into()),
            },
        ),
        // ── Token Budgets ──
        (
            "prompt_token_budget_soft".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Soft token budget for prompts. Triggers condensation when exceeded (uses tiktoken).".into(),
                options: None,
                readonly: false,
                default: Some("200000".into()),
            },
        ),
        (
            "prompt_token_budget_hard".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Hard token budget for prompts. Condenses before any LLM call when exceeded (uses tiktoken).".into(),
                options: None,
                readonly: false,
                default: Some("350000".into()),
            },
        ),
        (
            "tokenizer_encoding".into(),
            SettingMeta {
                field_type: "select".into(),
                description: "Tiktoken encoding for token counting. Corresponds to the model provider's tokenizer.".into(),
                options: Some(vec![
                    SettingOption { id: "gpt-4".into(), name: "GPT-4 (cl100k_base)".into() },
                    SettingOption { id: "cl100k_base".into(), name: "cl100k_base".into() },
                    SettingOption { id: "o200k_base".into(), name: "o200k_base (GPT-4o)".into() },
                ]),
                readonly: false,
                default: Some("gpt-4".into()),
            },
        ),
        (
            "prompt_token_safety_factor".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Multiplier to account for provider tokenizer mismatch with tiktoken.".into(),
                options: None,
                readonly: false,
                default: Some("15.0".into()),
            },
        ),
        // ── Tool Execution ──
        (
            "watchdog_default".into(),
            SettingMeta {
                field_type: "textarea".into(),
                description: "JSON config for the global tool watchdog (applied to tools without per-tool config). Format: { \\\"thresholds\\\": [{ \\\"at_percent\\\": 0.5, \\\"action\\\": { \\\"Notify\\\": { \\\"message\\\": \\\"...\\\" } } }, { \\\"at_percent\\\": 0.8, \\\"action\\\": \\\"Cancel\\\" }] }".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        // ── Prompts ──
        (
            "prompt_log_level".into(),
            SettingMeta {
                field_type: "select".into(),
                description: "When to insert prompts as messages (msg_type: \"prompt\") into the messages table: Off (never), First (first LLM call only), First+Compact (first + after context compaction), or All (every LLM call)".into(),
                options: Some(vec![
                    SettingOption { id: "off".into(), name: "Off: never insert prompts".into() },
                    SettingOption { id: "first".into(), name: "First: insert the first prompt only".into() },
                    SettingOption { id: "first+compact".into(), name: "First+Compact: first prompt + prompts after context compaction".into() },
                    SettingOption { id: "all".into(), name: "All: insert every prompt before every LLM call".into() },
                ]),
                readonly: false,
                default: Some("first".into()),
            },
        ),
        // ── Vectorization (Messages) ──
        (
            "vectorize_messages".into(),
            SettingMeta {
                field_type: "boolean".into(),
                description: "Enable vectorization of messages for semantic search.".into(),
                options: None,
                readonly: false,
                default: Some("false".into()),
            },
        ),
        (
            "messages_vectorization_method".into(),
            SettingMeta {
                field_type: "select".into(),
                description: "Vectorization method for messages: local (built-in), openai, or custom API.".into(),
                options: Some(vec![
                    SettingOption { id: "local".into(), name: "Local".into() },
                    SettingOption { id: "openai".into(), name: "OpenAI".into() },
                    SettingOption { id: "custom".into(), name: "Custom API".into() },
                ]),
                readonly: false,
                default: Some("local".into()),
            },
        ),
        (
            "messages_vectorization_api_url".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "API URL for message vectorization (required when method is openai or custom).".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        (
            "messages_vectorization_interval".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Interval in seconds between message vectorization runs.".into(),
                options: None,
                readonly: false,
                default: Some("3600".into()),
            },
        ),
        (
            "messages_vectorization_protocol".into(),
            SettingMeta {
                field_type: "select".into(),
                description: "API protocol for message vectorization.".into(),
                options: Some(vec![
                    SettingOption { id: "openai".into(), name: "OpenAI-compatible".into() },
                    SettingOption { id: "custom".into(), name: "Custom protocol".into() },
                ]),
                readonly: false,
                default: Some("openai".into()),
            },
        ),
        (
            "messages_vectorization_api_key".into(),
            SettingMeta {
                field_type: "secret".into(),
                description: "API key for message vectorization endpoint. Use $env:VAR or $secret:NAME to reference external values.".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        (
            "messages_vectorization_api_model".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Model name for message vectorization (e.g. text-embedding-ada-002).".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        // ── Vectorization (Wiki) ──
        (
            "vectorize_wiki".into(),
            SettingMeta {
                field_type: "boolean".into(),
                description: "Enable vectorization of wiki pages for semantic search.".into(),
                options: None,
                readonly: false,
                default: Some("false".into()),
            },
        ),
        (
            "wiki_vectorization_method".into(),
            SettingMeta {
                field_type: "select".into(),
                description: "Vectorization method for wiki pages: local (built-in), openai, or custom API.".into(),
                options: Some(vec![
                    SettingOption { id: "local".into(), name: "Local".into() },
                    SettingOption { id: "openai".into(), name: "OpenAI".into() },
                    SettingOption { id: "custom".into(), name: "Custom API".into() },
                ]),
                readonly: false,
                default: Some("local".into()),
            },
        ),
        (
            "wiki_vectorization_api_url".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "API URL for wiki vectorization (required when method is openai or custom).".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        (
            "wiki_vectorization_interval".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Interval in seconds between wiki vectorization runs.".into(),
                options: None,
                readonly: false,
                default: Some("3600".into()),
            },
        ),
        (
            "wiki_vectorization_protocol".into(),
            SettingMeta {
                field_type: "select".into(),
                description: "API protocol for wiki vectorization.".into(),
                options: Some(vec![
                    SettingOption { id: "openai".into(), name: "OpenAI-compatible".into() },
                    SettingOption { id: "custom".into(), name: "Custom protocol".into() },
                ]),
                readonly: false,
                default: Some("openai".into()),
            },
        ),
        (
            "wiki_vectorization_api_key".into(),
            SettingMeta {
                field_type: "secret".into(),
                description: "API key for wiki vectorization endpoint. Use $env:VAR or $secret:NAME to reference external values.".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        (
            "wiki_vectorization_api_model".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Model name for wiki vectorization (e.g. text-embedding-ada-002).".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        // ── Group 2 settings ──
        (
            "planning_complexity_simple_max_chars".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Max chars for 'simple' complexity classification".into(),
                options: None,
                readonly: false,
                default: Some("60".into()),
            },
        ),
        (
            "planning_complexity_standard_max_chars".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Max chars for 'standard' complexity classification".into(),
                options: None,
                readonly: false,
                default: Some("200".into()),
            },
        ),
        (
            "planning_complexity_keywords".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Comma-separated keywords that trigger 'complex' classification".into(),
                options: None,
                readonly: false,
                default: Some("implement,refactor,redesign,architecture,create,build,design,develop,deploy".into()),
            },
        ),
        (
            "platform_max_spawn_retries".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Max retries for spawning platform messages (external channels)".into(),
                options: None,
                readonly: false,
                default: Some("3".into()),
            },
        ),
        (
            "default_profile".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Default profile name used at login / session start".into(),
                options: None,
                readonly: false,
                default: Some("default".into()),
            },
        ),
        (
            "workspace_dir".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Workspace directory path for project files".into(),
                options: None,
                readonly: false,
                default: Some("/opt/workspace".into()),
            },
        ),
        (
            "mcp_servers_config".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Path to MCP servers config file".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
    ]
}

/// Organize setting definitions into categories.
fn categorize_settings(defs: Vec<(String, String, SettingMeta)>) -> Vec<SettingCategory> {
    let mut categories: Vec<SettingCategory> = vec![
        SettingCategory {
            name: "general".into(),
            label: "General".into(),
            settings: vec![],
        },
        SettingCategory {
            name: "planning".into(),
            label: "Planning".into(),
            settings: vec![],
        },
        SettingCategory {
            name: "memory".into(),
            label: "Memory & Retention".into(),
            settings: vec![],
        },
        SettingCategory {
            name: "system".into(),
            label: "System".into(),
            settings: vec![],
        },
    ];

    for (name, value, meta) in defs {
        let cat_name = match name.as_str() {
            "max_tokens" | "temperature" => "general",
            "max_iterations_no_plan"
            | "max_iterations_plan"
            | "tool_short_timeout_secs"
            | "tool_long_timeout_secs"
            | "max_unfinished_subtask_retries" => "general",
            "delete_after_days"
            | "thread_summary_tokens"
            | "memory_max_chars"
            | "soul_max_chars" => "memory",
            "max_pool_connections" | "max_inline_file_kb" | "prompt_generate_tool"
            | "prompt_compact_messages_tool" | "llm_provider" | "watchdog_default"
            | "prompt_log_level"
            | "prompt_char_budget_soft" | "prompt_char_budget_hard"
            | "old_message_char_budget" | "state_block_update_interval"
            | "condense_keep_turns" | "prompt_token_budget_soft"
            | "prompt_token_budget_hard" | "tokenizer_encoding"
            | "prompt_token_safety_factor"
            | "vectorize_messages" | "messages_vectorization_method" | "messages_vectorization_api_url" | "messages_vectorization_interval" | "messages_vectorization_protocol" | "messages_vectorization_api_key" | "messages_vectorization_api_model"
            | "vectorize_wiki" | "wiki_vectorization_method" | "wiki_vectorization_api_url" | "wiki_vectorization_interval" | "wiki_vectorization_protocol" | "wiki_vectorization_api_key" | "wiki_vectorization_api_model" => "general",
            "planning_complexity_simple_max_chars"
            | "planning_complexity_standard_max_chars"
            | "planning_complexity_keywords" => "planning",
            "platform_max_spawn_retries"
            | "default_profile"
            | "workspace_dir"
            | "mcp_servers_config" => "system",
            _ => "system",
        };

        if let Some(cat) = categories.iter_mut().find(|c| c.name == cat_name) {
            cat.settings.push(SettingEntry {
                name,
                value,
                metadata: meta,
            });
        }
    }

    categories.retain(|c| !c.settings.is_empty());
    categories
}

/// The 4 bootstrap settings that are always read-only from process env vars.
/// These are never stored in settings.yml — they come directly from env.
/// The env var name is the UPPER_SNAKE_CASE version of the setting name.
const BOOTSTRAP_SETTINGS: &[&str] = &["host", "port", "database_url", "omni_dir"];

/// Resolve a single setting value with $env:/$secret: support.
pub(crate) async fn resolve_setting_value(
    raw_value: &str,
    pool: &sqlx::PgPool,
) -> String {
    if raw_value.starts_with("$env:") || raw_value.starts_with("$secret:") {
        plugins_yaml::resolve_config_ref_value(raw_value, pool).await
    } else {
        raw_value.to_string()
    }
}

/// Resolve a collection of setting values in place.
pub(crate) async fn resolve_setting_values(map: &mut HashMap<String, String>, pool: &sqlx::PgPool) {
    let keys: Vec<String> = map.keys().cloned().collect();
    for key in keys {
        if let Some(value) = map.get(&key).cloned() {
            let resolved = resolve_setting_value(&value, pool).await;
            map.insert(key, resolved);
        }
    }
}

/// Enrich llm_provider setting options with dynamically loaded provider plugins.
/// Reads enabled providers from providers.yml.
fn enrich_provider_options(meta: &mut SettingMeta, data_dir: &str) {
    let providers = match plugins_yaml::get_enabled_providers(data_dir) {
        Ok(rows) if !rows.is_empty() => rows,
        _ => return, // Fall back to hardcoded options
    };

    meta.options = Some(
        providers
            .into_iter()
            .map(|(id, name)| SettingOption { id, name })
            .collect(),
    );
}

// ── Handlers ──

/// GET /settings: return all settings organized by category.
///
/// Values are resolved from settings.yml with $env:/$secret: support.
/// Bootstrap settings (host, port, database_url, omni_dir) always come
/// from process environment variables with UPPER_CASE names.
pub async fn get_settings_handler(
    State(state): State<Arc<AppState>>,
) -> Json<SettingsResponse> {
    // Load raw values from settings.yml
    let data_dir = state.data_dir.clone();
    let mut settings_values = tokio::task::spawn_blocking(move || load_settings_file(&data_dir))
        .await
        .unwrap_or_default();

    // Resolve $env:/$secret: references in settings.yml values
    resolve_setting_values(&mut settings_values, &state.pool).await;

    // Build the list of (name, resolved_value, meta) from definitions
    let mut defs: Vec<(String, String, SettingMeta)> = get_all_setting_definitions()
        .into_iter()
        .map(|(name, meta)| {
            let value = if BOOTSTRAP_SETTINGS.contains(&name.as_str()) {
                let env_name = setting_name_to_env(&name);
                std::env::var(&env_name)
                    .unwrap_or_else(|_| meta.default.clone().unwrap_or_default())
            } else {
                // Check settings.yml first, then default
                settings_values
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| meta.default.clone().unwrap_or_default())
            };
            (name, value, meta)
        })
        .collect();

    // Enrich llm_provider options with dynamically loaded provider plugins
    if let Some((_, _, ref mut meta)) = defs.iter_mut().find(|(name, _, _)| name == "llm_provider")
    {
        enrich_provider_options(meta, &state.data_dir);
    }

    // Enrich prompt_generate_tool and prompt_compact_messages_tool with available MCP tools
    let registry = state.tool_registry.read().await;
    let mcp_tools: Vec<&crate::mcp::McpTool> = registry.all();
    for tool_key in ["prompt_generate_tool", "prompt_compact_messages_tool"] {
        if let Some((_, _, ref mut meta)) = defs.iter_mut().find(|(name, _, _)| name.as_str() == tool_key)
        {
            let mut options: Vec<SettingOption> = mcp_tools
                .iter()
                .map(|t| {
                    let id = t.full_name.clone();
                    SettingOption { id: id.clone(), name: id }
                })
                .collect();
            // Sort alphabetically
            options.sort_by(|a, b| a.name.cmp(&b.name));
            meta.options = Some(options);
        }
    }

    Json(SettingsResponse {
        categories: categorize_settings(defs),
    })
}

/// PUT /settings: update one or more settings and write to settings.yml.
///
/// Bootstrap settings (host, port, database_url, omni_dir) are read-only
/// and cannot be updated.
pub async fn update_settings_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UpdateSettingsRequest>,
) -> impl IntoResponse {
    if body.updates.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "No updates provided" })),
        );
    }

    // Known writable setting names (everything except bootstrap env vars)
    let writable_keys: std::collections::HashSet<&str> = [
        "max_tokens",
        "temperature",
        "max_iterations_no_plan",
        "max_iterations_plan",
        "max_unfinished_subtask_retries",
        "prompt_generate_tool",
        "prompt_compact_messages_tool",
        "delete_after_days",
        "thread_summary_tokens",
        "memory_max_chars",
        "soul_max_chars",
        "llm_provider",
        "max_pool_connections",
        "max_inline_file_kb",
        "tool_short_timeout_secs",
        "tool_long_timeout_secs",
        "watchdog_default",
        "prompt_char_budget_soft",
        "prompt_char_budget_hard",
        "old_message_char_budget",
        "state_block_update_interval",
        "condense_keep_turns",
        "prompt_token_budget_soft",
        "prompt_token_budget_hard",
        "tokenizer_encoding",
        "prompt_token_safety_factor",
        "vectorize_messages",
        "messages_vectorization_method",
        "messages_vectorization_api_url",
        "messages_vectorization_interval",
        "messages_vectorization_protocol",
        "messages_vectorization_api_key",
        "messages_vectorization_api_model",
        "vectorize_wiki",
        "wiki_vectorization_method",
        "wiki_vectorization_api_url",
        "wiki_vectorization_interval",
        "wiki_vectorization_protocol",
        "wiki_vectorization_api_key",
        "wiki_vectorization_api_model",
        "prompt_log_level",
        "planning_complexity_simple_max_chars",
        "planning_complexity_standard_max_chars",
        "planning_complexity_keywords",
        "platform_max_spawn_retries",
        "default_profile",
        "workspace_dir",
        "mcp_servers_config",
    ]
    .into_iter()
    .collect();

    // Load current settings.yml values
    let data_dir = state.data_dir.clone();
    let mut file_vars = tokio::task::spawn_blocking(move || load_settings_file(&data_dir))
        .await
        .unwrap_or_default();

    let mut applied: Vec<String> = Vec::new();

    for update in &body.updates {
        if !writable_keys.contains(update.name.as_str()) {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": format!("Setting '{}' is read-only", update.name),
                    "field": update.name,
                })),
            );
        }
        // Store the raw value (may contain $env: or $secret: refs)
        file_vars.insert(update.name.clone(), update.value.clone());
        applied.push(update.name.clone());
    }

    let data_dir_write = state.data_dir.clone();
    match tokio::task::spawn_blocking(move || write_settings_file(&data_dir_write, &file_vars))
        .await
        .unwrap_or(Err("spawn_blocking failed".to_string()))
    {
        Ok(()) => {
            tracing::info!("Settings updated: {:?}", applied);
            // Reload the global config from settings.yml so the change
            // takes effect immediately without requiring a container restart.
            crate::agent::config::reload_global_from_settings(
                &state.data_dir,
                &state.pool,
            )
            .await;
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "ok",
                    "updated": applied,
                })),
            )
        }
        Err(e) => {
            tracing::error!("Failed to write settings.yml: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e })),
            )
        }
    }
}
