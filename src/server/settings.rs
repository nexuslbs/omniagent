//! Settings API - read/write environment variables organized by category.
//!
//! - `GET /settings` - returns all settings with metadata
//! - `PUT /settings` - updates one or more values and writes to .env file

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
    /// Default value if not set in .env
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

/// Load current .env file as a HashMap.
fn load_env_file(env_path: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let content = match std::fs::read_to_string(env_path) {
        Ok(c) => c,
        Err(_) => return map,
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            map.insert(key, value);
        }
    }
    map
}

/// Write HashMap back to .env file.
fn write_env_file(env_path: &str, vars: &HashMap<String, String>) -> Result<(), String> {
    let mut content = String::new();
    let mut keys: Vec<&String> = vars.keys().collect();
    keys.sort();

    for key in keys {
        if let Some(value) = vars.get(key) {
            content.push_str(&format!("{}={}\n", key, value));
        }
    }

    std::fs::write(env_path, content).map_err(|e| format!("Failed to write .env: {}", e))
}

/// The canonical list of all settings with their metadata.
fn get_all_setting_definitions() -> Vec<(String, String, SettingMeta)> {
    vec![
        // ── General ──
        (
            "MAX_TOKENS".into(),
            get_env_or_default("MAX_TOKENS", "4096"),
            SettingMeta {
                field_type: "number".into(),
                description: "Maximum tokens per LLM response".into(),
                options: None,
                readonly: false,
                default: Some("4096".into()),
            },
        ),
        (
            "TEMPERATURE".into(),
            get_env_or_default("TEMPERATURE", "0.7"),
            SettingMeta {
                field_type: "number".into(),
                description: "LLM sampling temperature (0.0 – 2.0)".into(),
                options: None,
                readonly: false,
                default: Some("0.7".into()),
            },
        ),
        (
            "MAX_ITERATIONS_NO_PLAN".into(),
            get_env_or_default("MAX_ITERATIONS_NO_PLAN", "30"),
            SettingMeta {
                field_type: "number".into(),
                description: "Max tool-call iterations for threads with no planning".into(),
                options: None,
                readonly: false,
                default: Some("30".into()),
            },
        ),
        (
            "MAX_ITERATIONS_PLAN".into(),
            get_env_or_default("MAX_ITERATIONS_PLAN", "120"),
            SettingMeta {
                field_type: "number".into(),
                description: "Max tool-call iterations for threads with planning enabled".into(),
                options: None,
                readonly: false,
                default: Some("120".into()),
            },
        ),
        (
            "MAX_UNFINISHED_SUBTASK_RETRIES".into(),
            get_env_or_default("MAX_UNFINISHED_SUBTASK_RETRIES", "3"),
            SettingMeta {
                field_type: "number".into(),
                description: "Max retries before marking a thread as failed when subtasks remain unfinished or plan JSON is invalid".into(),
                options: None,
                readonly: false,
                default: Some("3".into()),
            },
        ),
        (
            "PROMPT_GENERATE_TOOL".into(),
            get_env_or_default("PROMPT_GENERATE_TOOL", "prompt_generate"),
            SettingMeta {
                field_type: "select".into(),
                description: "Name of the MCP tool to call for generating prompts".into(),
                options: None,
                readonly: false,
                default: Some("prompt_generate".into()),
            },
        ),
        (
            "PROMPT_COMPACT_MESSAGES_TOOL".into(),
            get_env_or_default("PROMPT_COMPACT_MESSAGES_TOOL", "prompt_compact-messages"),
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
            "MEMORY_MAX_CHARS".into(),
            get_env_or_default("MEMORY_MAX_CHARS", "5000"),
            SettingMeta {
                field_type: "number".into(),
                description: "Max characters for MEMORY.md in the system prompt".into(),
                options: None,
                readonly: false,
                default: Some("5000".into()),
            },
        ),
        (
            "SOUL_MAX_CHARS".into(),
            get_env_or_default("SOUL_MAX_CHARS", "1000"),
            SettingMeta {
                field_type: "number".into(),
                description: "Max characters for SOUL.md in the system prompt".into(),
                options: None,
                readonly: false,
                default: Some("1000".into()),
            },
        ),
        (
            "DELETE_AFTER_DAYS".into(),
            get_env_or_default("DELETE_AFTER_DAYS", "30"),
            SettingMeta {
                field_type: "number".into(),
                description: "Days before old messages and summaries are deleted".into(),
                options: None,
                readonly: false,
                default: Some("30".into()),
            },
        ),
        (
            "THREAD_SUMMARY_TOKENS".into(),
            get_env_or_default("THREAD_SUMMARY_TOKENS", "2048"),
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
            "MAX_POOL_CONNECTIONS".into(),
            get_env_or_default("MAX_POOL_CONNECTIONS", "5"),
            SettingMeta {
                field_type: "number".into(),
                description: "Maximum per-channel connections per MCP server. Each channel gets its own pool; this caps how many simultaneous tool calls a single server can handle per channel. Increase for multi-tool workloads, decrease to save memory. Minimum 1.".into(),
                options: None,
                readonly: false,
                default: Some("5".into()),
            },
        ),
        (
            "MAX_INLINE_FILE_KB".into(),
            get_env_or_default("MAX_INLINE_FILE_KB", "100"),
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
            "LLM_PROVIDER".into(),
            get_env_or_default("LLM_PROVIDER", "opencode-go"),
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
        // ── System (read-only) ──
        (
            "HOST".into(),
            get_env_or_default("HOST", "0.0.0.0"),
            SettingMeta {
                field_type: "text".into(),
                description: "HTTP server bind address".into(),
                options: None,
                readonly: true,
                default: Some("0.0.0.0".into()),
            },
        ),
        (
            "PORT".into(),
            get_env_or_default("PORT", "8080"),
            SettingMeta {
                field_type: "number".into(),
                description: "HTTP server port".into(),
                options: None,
                readonly: true,
                default: Some("8080".into()),
            },
        ),
        (
            "OMNI_DIR".into(),
            get_env_or_default("OMNI_DIR", ""),
            SettingMeta {
                field_type: "text".into(),
                description: "Data directory for profiles and wiki (must be set via env)".into(),
                options: None,
                readonly: true,
                default: Some("".into()),
            },
        ),
        // ── Context Management ──
        (
            "PROMPT_CHAR_BUDGET_SOFT".into(),
            get_env_or_default("PROMPT_CHAR_BUDGET_SOFT", "350000"),
            SettingMeta {
                field_type: "number".into(),
                description: "Soft char budget for prompts. When exceeded, context is condensed every N turns.".into(),
                options: None,
                readonly: false,
                default: Some("350000".into()),
            },
        ),
        (
            "PROMPT_CHAR_BUDGET_HARD".into(),
            get_env_or_default("PROMPT_CHAR_BUDGET_HARD", "500000"),
            SettingMeta {
                field_type: "number".into(),
                description: "Hard char budget for prompts. When exceeded, context is condensed before the next LLM call.".into(),
                options: None,
                readonly: false,
                default: Some("500000".into()),
            },
        ),
        (
            "OLD_MESSAGE_CHAR_BUDGET".into(),
            get_env_or_default("OLD_MESSAGE_CHAR_BUDGET", "100000"),
            SettingMeta {
                field_type: "number".into(),
                description: "Max chars for old messages after condensation. The metadata block stays in full.".into(),
                options: None,
                readonly: false,
                default: Some("100000".into()),
            },
        ),
        (
            "STATE_BLOCK_UPDATE_INTERVAL".into(),
            get_env_or_default("STATE_BLOCK_UPDATE_INTERVAL", "5"),
            SettingMeta {
                field_type: "number".into(),
                description: "How often (in iterations) to refresh the state block when soft budget is exceeded.".into(),
                options: None,
                readonly: false,
                default: Some("5".into()),
            },
        ),
        (
            "CONDENSE_KEEP_TURNS".into(),
            get_env_or_default("CONDENSE_KEEP_TURNS", "4"),
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
            "PROMPT_TOKEN_BUDGET_SOFT".into(),
            get_env_or_default("PROMPT_TOKEN_BUDGET_SOFT", "200000"),
            SettingMeta {
                field_type: "number".into(),
                description: "Soft token budget for prompts. Triggers condensation when exceeded (uses tiktoken).".into(),
                options: None,
                readonly: false,
                default: Some("200000".into()),
            },
        ),
        (
            "PROMPT_TOKEN_BUDGET_HARD".into(),
            get_env_or_default("PROMPT_TOKEN_BUDGET_HARD", "350000"),
            SettingMeta {
                field_type: "number".into(),
                description: "Hard token budget for prompts. Condenses before any LLM call when exceeded (uses tiktoken).".into(),
                options: None,
                readonly: false,
                default: Some("350000".into()),
            },
        ),
        (
            "TOKENIZER_ENCODING".into(),
            get_env_or_default("TOKENIZER_ENCODING", "gpt-4"),
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
            "PROMPT_TOKEN_SAFETY_FACTOR".into(),
            get_env_or_default("PROMPT_TOKEN_SAFETY_FACTOR", "15.0"),
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
            "WATCHDOG_DEFAULT".into(),
            get_env_or_default("WATCHDOG_DEFAULT", ""),
            SettingMeta {
                field_type: "textarea".into(),
                description: "JSON config for the global tool watchdog (applied to tools without per-tool config). Format: { \"thresholds\": [{ \"at_percent\": 0.5, \"action\": { \"Notify\": { \"message\": \"...\" } } }, { \"at_percent\": 0.8, \"action\": \"Cancel\" }] }".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        // ── Prompts ──
        (
            "PROMPT_LOG_LEVEL".into(),
            get_env_or_default("PROMPT_LOG_LEVEL", "first"),
            SettingMeta {
                field_type: "select".into(),
                description: "When to insert prompts as messages (msg_type: \"prompt\") into the messages table: Off (never), First (first LLM call only), First+Compact (first + after context compaction), or All (every LLM call)".into(),
                options: Some(vec![
                    SettingOption { id: "off".into(), name: "Off - never insert prompts".into() },
                    SettingOption { id: "first".into(), name: "First - insert the first prompt only".into() },
                    SettingOption { id: "first+compact".into(), name: "First+Compact - first prompt + prompts after context compaction".into() },
                    SettingOption { id: "all".into(), name: "All - insert every prompt before every LLM call".into() },
                ]),
                readonly: false,
                default: Some("first".into()),
            },
        ),
        // ── Vectorization (Messages) ──
        (
            "VECTORIZE_MESSAGES".into(),
            get_env_or_default("VECTORIZE_MESSAGES", "false"),
            SettingMeta {
                field_type: "boolean".into(),
                description: "Enable vectorization of messages for semantic search.".into(),
                options: None,
                readonly: false,
                default: Some("false".into()),
            },
        ),
        (
            "MESSAGES_VECTORIZATION_METHOD".into(),
            get_env_or_default("MESSAGES_VECTORIZATION_METHOD", "local"),
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
            "MESSAGES_VECTORIZATION_API_URL".into(),
            get_env_or_default("MESSAGES_VECTORIZATION_API_URL", ""),
            SettingMeta {
                field_type: "text".into(),
                description: "API URL for message vectorization (required when method is openai or custom).".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        (
            "MESSAGES_VECTORIZATION_INTERVAL".into(),
            get_env_or_default("MESSAGES_VECTORIZATION_INTERVAL", "3600"),
            SettingMeta {
                field_type: "number".into(),
                description: "Interval in seconds between message vectorization runs.".into(),
                options: None,
                readonly: false,
                default: Some("3600".into()),
            },
        ),
        (
            "MESSAGES_VECTORIZATION_PROTOCOL".into(),
            get_env_or_default("MESSAGES_VECTORIZATION_PROTOCOL", "openai"),
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
            "MESSAGES_VECTORIZATION_API_KEY".into(),
            get_env_or_default("MESSAGES_VECTORIZATION_API_KEY", ""),
            SettingMeta {
                field_type: "secret".into(),
                description: "API key for message vectorization endpoint.".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        (
            "MESSAGES_VECTORIZATION_API_MODEL".into(),
            get_env_or_default("MESSAGES_VECTORIZATION_API_MODEL", ""),
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
            "VECTORIZE_WIKI".into(),
            get_env_or_default("VECTORIZE_WIKI", "false"),
            SettingMeta {
                field_type: "boolean".into(),
                description: "Enable vectorization of wiki pages for semantic search.".into(),
                options: None,
                readonly: false,
                default: Some("false".into()),
            },
        ),
        (
            "WIKI_VECTORIZATION_METHOD".into(),
            get_env_or_default("WIKI_VECTORIZATION_METHOD", "local"),
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
            "WIKI_VECTORIZATION_API_URL".into(),
            get_env_or_default("WIKI_VECTORIZATION_API_URL", ""),
            SettingMeta {
                field_type: "text".into(),
                description: "API URL for wiki vectorization (required when method is openai or custom).".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        (
            "WIKI_VECTORIZATION_INTERVAL".into(),
            get_env_or_default("WIKI_VECTORIZATION_INTERVAL", "3600"),
            SettingMeta {
                field_type: "number".into(),
                description: "Interval in seconds between wiki vectorization runs.".into(),
                options: None,
                readonly: false,
                default: Some("3600".into()),
            },
        ),
        (
            "WIKI_VECTORIZATION_PROTOCOL".into(),
            get_env_or_default("WIKI_VECTORIZATION_PROTOCOL", "openai"),
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
            "WIKI_VECTORIZATION_API_KEY".into(),
            get_env_or_default("WIKI_VECTORIZATION_API_KEY", ""),
            SettingMeta {
                field_type: "secret".into(),
                description: "API key for wiki vectorization endpoint.".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
        (
            "WIKI_VECTORIZATION_API_MODEL".into(),
            get_env_or_default("WIKI_VECTORIZATION_API_MODEL", ""),
            SettingMeta {
                field_type: "text".into(),
                description: "Model name for wiki vectorization (e.g. text-embedding-ada-002).".into(),
                options: None,
                readonly: false,
                default: Some("".into()),
            },
        ),
    ]
}

/// Get env var value or default, checking both in-process env and .env file.
fn get_env_or_default(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
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
            "MAX_TOKENS" | "TEMPERATURE" => "general",
            "MAX_ITERATIONS_NO_PLAN"
            | "MAX_ITERATIONS_PLAN" => "general",
            "DELETE_AFTER_DAYS"
            | "THREAD_SUMMARY_TOKENS"
            | "MEMORY_MAX_CHARS"
            | "SOUL_MAX_CHARS" => "memory",
            "LLM_PROVIDER" | "PROMPT_LOG_LEVEL" | "MAX_INLINE_FILE_KB" | "MAX_UNFINISHED_SUBTASK_RETRIES" | "PROMPT_GENERATE_TOOL" | "PROMPT_COMPACT_MESSAGES_TOOL" | "WATCHDOG_DEFAULT"
            | "PROMPT_CHAR_BUDGET_SOFT" | "PROMPT_CHAR_BUDGET_HARD" | "OLD_MESSAGE_CHAR_BUDGET" | "STATE_BLOCK_UPDATE_INTERVAL" | "CONDENSE_KEEP_TURNS"
            | "PROMPT_TOKEN_BUDGET_SOFT" | "PROMPT_TOKEN_BUDGET_HARD" | "TOKENIZER_ENCODING" | "PROMPT_TOKEN_SAFETY_FACTOR"
            | "VECTORIZE_MESSAGES" | "MESSAGES_VECTORIZATION_METHOD" | "MESSAGES_VECTORIZATION_API_URL" | "MESSAGES_VECTORIZATION_INTERVAL" | "MESSAGES_VECTORIZATION_PROTOCOL" | "MESSAGES_VECTORIZATION_API_KEY" | "MESSAGES_VECTORIZATION_API_MODEL"
            | "VECTORIZE_WIKI" | "WIKI_VECTORIZATION_METHOD" | "WIKI_VECTORIZATION_API_URL" | "WIKI_VECTORIZATION_INTERVAL" | "WIKI_VECTORIZATION_PROTOCOL" | "WIKI_VECTORIZATION_API_KEY" | "WIKI_VECTORIZATION_API_MODEL" => "general",
            "MAX_POOL_CONNECTIONS" => "general",
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

// ── Handlers ──

/// GET /settings - return all settings organized by category.
pub async fn get_settings_handler(State(state): State<Arc<AppState>>) -> Json<SettingsResponse> {
    // Reload .env file to get current values, then merge with defaults
    let env_path = state.env_path.clone();
    let env_vars = tokio::task::spawn_blocking(move || load_env_file(&env_path))
        .await
        .unwrap_or_default();

    let mut defs: Vec<(String, String, SettingMeta)> = get_all_setting_definitions()
        .into_iter()
        .map(|(name, _default_value, meta)| {
            // Check .env first, then process env, then default
            let value = env_vars
                .get(&name)
                .cloned()
                .or_else(|| std::env::var(&name).ok())
                .unwrap_or_else(|| meta.default.clone().unwrap_or_default());
            (name, value, meta)
        })
        .collect();

    // Enrich LLM_PROVIDER options with dynamically loaded provider plugins
    if let Some((_, _, ref mut meta)) = defs.iter_mut().find(|(name, _, _)| name == "LLM_PROVIDER")
    {
        enrich_provider_options(meta, &state.data_dir);
    }

    // Enrich PROMPT_GENERATE_TOOL and PROMPT_COMPACT_MESSAGES_TOOL with available MCP tools
    let registry = state.tool_registry.read().await;
    let mcp_tools: Vec<&crate::mcp::McpTool> = registry.all();
    for tool_key in ["PROMPT_GENERATE_TOOL", "PROMPT_COMPACT_MESSAGES_TOOL"] {
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

/// Enrich LLM_PROVIDER setting options with dynamically loaded provider plugins.
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

/// PUT /settings - update one or more settings and write to .env.
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

    let env_path_read = state.env_path.clone();
    let mut env_vars = tokio::task::spawn_blocking(move || load_env_file(&env_path_read))
        .await
        .unwrap_or_default();

    // Known writable setting names for validation
    let writable_keys: std::collections::HashSet<&str> = [
        "MAX_TOKENS",
        "TEMPERATURE",
        "MAX_ITERATIONS_NO_PLAN",
        "MAX_ITERATIONS_PLAN",
        "MAX_UNFINISHED_SUBTASK_RETRIES",
        "PROMPT_GENERATE_TOOL",
        "PROMPT_COMPACT_MESSAGES_TOOL",
        "DELETE_AFTER_DAYS",
        "THREAD_SUMMARY_TOKENS",
        "MEMORY_MAX_CHARS",
        "SOUL_MAX_CHARS",
        "LLM_PROVIDER",
        "MAX_POOL_CONNECTIONS",
        "MAX_INLINE_FILE_KB",
        "WATCHDOG_DEFAULT",
        "PROMPT_CHAR_BUDGET_SOFT",
        "PROMPT_CHAR_BUDGET_HARD",
        "OLD_MESSAGE_CHAR_BUDGET",
        "STATE_BLOCK_UPDATE_INTERVAL",
        "CONDENSE_KEEP_TURNS",
        "PROMPT_TOKEN_BUDGET_SOFT",
        "PROMPT_TOKEN_BUDGET_HARD",
        "TOKENIZER_ENCODING",
        "PROMPT_TOKEN_SAFETY_FACTOR",
        "VECTORIZE_MESSAGES",
        "MESSAGES_VECTORIZATION_METHOD",
        "MESSAGES_VECTORIZATION_API_URL",
        "MESSAGES_VECTORIZATION_INTERVAL",
        "MESSAGES_VECTORIZATION_PROTOCOL",
        "MESSAGES_VECTORIZATION_API_KEY",
        "MESSAGES_VECTORIZATION_API_MODEL",
        "VECTORIZE_WIKI",
        "WIKI_VECTORIZATION_METHOD",
        "WIKI_VECTORIZATION_API_URL",
        "WIKI_VECTORIZATION_INTERVAL",
        "WIKI_VECTORIZATION_PROTOCOL",
        "WIKI_VECTORIZATION_API_KEY",
        "WIKI_VECTORIZATION_API_MODEL",
    ]
    .into_iter()
    .collect();

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
        env_vars.insert(update.name.clone(), update.value.clone());
        // Also set in the process environment so currently-running
        // MemoryStore / future loads pick up the change immediately
        // without requiring a container restart.
        std::env::set_var(&update.name, &update.value);
        applied.push(update.name.clone());
    }

    let env_path_write = state.env_path.clone();
    match tokio::task::spawn_blocking(move || write_env_file(&env_path_write, &env_vars))
        .await
        .unwrap_or(Err("spawn_blocking failed".to_string()))
    {
        Ok(()) => {
            tracing::info!("Settings updated: {:?}", applied);
            // Reload the global config so the change takes effect immediately
            // without requiring a container restart.
            crate::agent::config::reload_global();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "ok",
                    "updated": applied,
                })),
            )
        }
        Err(e) => {
            tracing::error!("Failed to write .env: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e })),
            )
        }
    }
}
