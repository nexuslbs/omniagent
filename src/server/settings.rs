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

/// Load settings.yml as a flat key-value map, supporting both flat format
/// (`KEY: value`) and nested section format (`section:\n  key: value`).
/// Returns an empty map if the file doesn't exist or can't be parsed.
pub(crate) fn load_settings_file(data_dir: &str) -> HashMap<String, String> {
    let path = settings_path(data_dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let raw: serde_yaml::Value = match serde_yaml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };

    let mut map = HashMap::new();
    flatten_yaml_mapping(&raw, &mut map, "");
    map
}

/// Recursively flatten nested YAML mappings into a flat key-value map.
/// Handles both flat (`KEY: value`) and nested (`section:\n  key: value`) formats.
fn flatten_yaml_mapping(
    value: &serde_yaml::Value,
    map: &mut HashMap<String, String>,
    _prefix: &str,
) {
    if let serde_yaml::Value::Mapping(mapping) = value {
        for (key, val) in mapping {
            let k = match key.as_str() {
                Some(s) => s.to_lowercase(),
                None => continue,
            };
            match val {
                serde_yaml::Value::String(s) => {
                    map.insert(k, s.clone());
                }
                serde_yaml::Value::Bool(b) => {
                    map.insert(k, b.to_string());
                }
                serde_yaml::Value::Number(n) => {
                    map.insert(k, n.to_string());
                }
                serde_yaml::Value::Mapping(_) => {
                    // Nested section: recurse into it
                    flatten_yaml_mapping(val, map, "");
                }
                serde_yaml::Value::Null => {
                    map.insert(k, String::new());
                }
                _ => {
                    // Fallback: serialize to string
                    if let Ok(s) = serde_yaml::to_string(val) {
                        map.insert(k, s.trim().to_string());
                    }
                }
            }
        }
    }
}

/// Write a key-value map to settings.yml using the nested section format.
fn write_settings_file(data_dir: &str, vars: &HashMap<String, String>) -> Result<(), String> {
    let path = settings_path(data_dir);

    // ── Section groupings for the nested YAML output ──
    let section_order = ["general", "execution", "prompt", "memory"];
    let sections: std::collections::HashMap<&str, Vec<&str>> = [
        ("general", vec![
            "condense_keep_turns", "delete_after_days", "default_provider",
            "llm_provider", "max_inline_file_kb", "max_pool_connections",
            "max_tokens", "max_unfinished_subtask_retries",
            "old_message_char_budget", "soul_max_chars",
            "state_block_update_interval", "temperature",
            "thread_summary_tokens", "tokenizer_encoding", "watchdog_default",
        ]),
        ("execution", vec![
            "max_iterations_no_plan", "max_iterations_plan",
            "tool_bg_secs", "tool_long_timeout_secs", "tool_short_timeout_secs",
        ]),
        ("prompt", vec![
            "prompt_char_budget_hard", "prompt_char_budget_soft",
            "prompt_compact_messages_tool", "prompt_generate_tool",
            "prompt_log_level", "prompt_token_budget_hard",
            "prompt_token_budget_soft", "prompt_token_safety_factor",
        ]),
        ("memory", vec![
            "memory_max_chars",
            "messages_vectorization_api_key", "messages_vectorization_api_model",
            "messages_vectorization_api_url", "messages_vectorization_interval",
            "messages_vectorization_method", "messages_vectorization_protocol",
            "vectorize_messages", "vectorize_wiki",
            "wiki_vectorization_api_key", "wiki_vectorization_api_model",
            "wiki_vectorization_api_url", "wiki_vectorization_interval",
            "wiki_vectorization_method", "wiki_vectorization_protocol",
        ]),
    ].into_iter().collect();

    /// Format a single YAML value with proper quoting.
    fn format_value(value: &str) -> String {
        if value.starts_with('$')
            || value.contains(':')
            || value.contains('#')
            || value.is_empty()
            || value.contains(' ')
            || value.contains('\n')
        {
            format!("'{}'", value.replace('\'', "''"))
        } else {
            value.to_string()
        }
    }

    let mut content = String::from("# Settings for OmniAgent\n");
    content.push_str("# Values support $env:VAR and $secret:NAME refs.\n\n");

    let mut written = std::collections::HashSet::new();

    for section_name in &section_order {
        let mut sec_values: Vec<(String, String)> = Vec::new();
        if let Some(keys) = sections.get(section_name) {
            for key in keys {
                if let Some(value) = vars.get(*key) {
                    sec_values.push((key.to_string(), format_value(value)));
                    written.insert(key.to_string());
                }
            }
        }
        if sec_values.is_empty() {
            continue;
        }
        content.push_str(&format!("{}:\n", section_name));
        for (key, value) in &sec_values {
            content.push_str(&format!("  {}: {}\n", key, value));
        }
        content.push('\n');
    }

    // Any remaining keys not assigned to a section go under a general catch-all
    let mut remaining: Vec<&String> = vars.keys()
        .filter(|k| !written.contains(k.as_str()))
        .collect();
    if !remaining.is_empty() {
        remaining.sort();
        content.push_str("unsorted:\n");
        for key in remaining {
            if let Some(value) = vars.get(key) {
                content.push_str(&format!("  {}: {}\n", key, format_value(value)));
            }
        }
        content.push('\n');
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
            "tool_bg_secs".into(),
            SettingMeta {
                field_type: "number".into(),
                description: "Threshold in seconds before switching tool calls to background mode (returns a processing status with task ID)".into(),
                options: None,
                readonly: false,
                default: Some("30".into()),
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
            "default_provider".into(),
            SettingMeta {
                field_type: "select".into(),
                description: "Default LLM provider backend for channels without an explicit provider".into(),
                options: Some(vec![
                    SettingOption { id: "opencode-go".into(), name: "opencode-go".into() },
                    SettingOption { id: "openai".into(), name: "openai".into() },
                    SettingOption { id: "anthropic".into(), name: "anthropic".into() },
                    SettingOption { id: "deepseek".into(), name: "deepseek".into() },
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
                default: Some("/opt/omni".into()),
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
        // ── Group 2 settings ──
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
                description: "Default profile name used when no profile is specified for a channel".into(),
                options: None,
                readonly: false,
                default: Some("omni".into()),
            },
        ),
        // ── Reaction emoji ──
        (
            "completed_emoji".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Emoji sent as a reaction when a thread completes successfully (default: :white_check_mark:)".into(),
                options: None,
                readonly: false,
                default: Some(":white_check_mark:".into()),
            },
        ),
        (
            "failed_emoji".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Emoji sent as a reaction when a thread fails (default: :x:)".into(),
                options: None,
                readonly: false,
                default: Some(":x:".into()),
            },
        ),
        (
            "interrupted_emoji".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Emoji sent as a reaction when a thread is interrupted (default: :broken_heart:)".into(),
                options: None,
                readonly: false,
                default: Some(":broken_heart:".into()),
            },
        ),
        (
            "default_emoji".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Fallback emoji for unknown thread final statuses (default: :o:)".into(),
                options: None,
                readonly: false,
                default: Some(":o:".into()),
            },
        ),
        // ── Kanban status transitions ──
        (
            "kanban_completed_status".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Kanban status set when a task thread completes (default: review)".into(),
                options: None,
                readonly: false,
                default: Some("review".into()),
            },
        ),
        (
            "kanban_failed_status".into(),
            SettingMeta {
                field_type: "text".into(),
                description: "Kanban status set when a task thread fails (default: blocked)".into(),
                options: None,
                readonly: false,
                default: Some("blocked".into()),
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
            name: "prompt".into(),
            label: "Prompt".into(),
            settings: vec![],
        },
        SettingCategory {
            name: "execution".into(),
            label: "Execution".into(),
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
            // prompt category
            "max_inline_file_kb"
            | "prompt_generate_tool"
            | "prompt_compact_messages_tool"
            | "prompt_log_level" => "prompt",
            // execution category
            "max_iterations_no_plan"
            | "max_iterations_plan"
            | "max_tokens"
            | "max_unfinished_subtask_retries"
            | "temperature"
            | "tool_bg_secs"
            | "completed_emoji"
            | "failed_emoji"
            | "interrupted_emoji"
            | "default_emoji"
            | "kanban_completed_status"
            | "kanban_failed_status" => "execution",
            // memory category
            "delete_after_days"
            | "thread_summary_tokens"
            | "memory_max_chars"
            | "soul_max_chars" => "memory",
            // system : bootstrap from env
            "host" | "port" | "database_url" | "omni_dir" => "system",
            // everything else → general
            _ => "general",
        };

        if let Some(cat) = categories.iter_mut().find(|c| c.name == cat_name) {
            cat.settings.push(SettingEntry {
                name,
                value,
                metadata: meta,
            });
        }
    }

    for cat in categories.iter_mut() {
        cat.settings.sort_by(|a, b| a.name.cmp(&b.name));
    }

    categories.retain(|c| !c.settings.is_empty());
    categories
}

/// The 4 bootstrap settings that are always read-only from process env vars.
/// These are never stored in settings.yml : they come directly from env.
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

/// Enrich default_provider setting options with dynamically loaded provider plugins.
/// Reads enabled providers from providers.yml.
fn enrich_provider_options(meta: &mut SettingMeta, data_dir: &str) {
    let providers = match plugins_yaml::get_enabled_providers(data_dir) {
        Ok(rows) if !rows.is_empty() => rows,
        _ => return, // Fall back to hardcoded options
    };

    meta.options = Some(
        providers
            .into_iter()
            .map(|(id, _)| SettingOption { id: id.clone(), name: id })
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

    // Enrich default_provider options with dynamically loaded provider plugins
    if let Some((_, _, ref mut meta)) = defs.iter_mut().find(|(name, _, _)| name == "default_provider")
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
        "default_provider",
        "max_pool_connections",
        "max_inline_file_kb",
        "tool_bg_secs",
        "prompt_log_level",
        "platform_max_spawn_retries",
        "default_profile",
        "completed_emoji",
        "failed_emoji",
        "interrupted_emoji",
        "default_emoji",
        "kanban_completed_status",
        "kanban_failed_status",
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
