//! Settings API — read/write environment variables organized by category.
//!
//! - `GET /settings` — returns all settings with metadata
//! - `PUT /settings` — updates one or more values and writes to .env file

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
                description: "Max tool-call iterations for threads with no planning (complexity-based)".into(),
                options: None,
                readonly: false,
                default: Some("30".into()),
            },
        ),
        (
            "MAX_ITERATIONS_SIMPLE_PLAN".into(),
            get_env_or_default("MAX_ITERATIONS_SIMPLE_PLAN", "120"),
            SettingMeta {
                field_type: "number".into(),
                description: "Max tool-call iterations for threads with simple planning (auto_plan)".into(),
                options: None,
                readonly: false,
                default: Some("120".into()),
            },
        ),
        (
            "MAX_ITERATIONS_COMPLEX_PLAN".into(),
            get_env_or_default("MAX_ITERATIONS_COMPLEX_PLAN", "600"),
            SettingMeta {
                field_type: "number".into(),
                description: "Max tool-call iterations for threads with complex planning + subtasks (auto_subtasks)".into(),
                options: None,
                readonly: false,
                default: Some("600".into()),
            },
        ),
        (
            "LLM_MAX_TOKENS".into(),
            get_env_or_default("LLM_MAX_TOKENS", "8192"),
            SettingMeta {
                field_type: "number".into(),
                description: "Maximum tokens for the LLM client".into(),
                options: None,
                readonly: false,
                default: Some("8192".into()),
            },
        ),
        // ── Planning ──
        (
            "PLANNING_MODE".into(),
            get_env_or_default("PLANNING_MODE", "auto_plan"),
            SettingMeta {
                field_type: "select".into(),
                description: "How tasks are planned: Prompt Only (no plan), Auto-Plan (plan context only), or Auto-Plan + Subtasks (with step tracking and enforcement)".into(),
                options: Some(vec![
                    SettingOption { id: "prompt_only".into(), name: "Prompt Only — send as is, no planning".into() },
                    SettingOption { id: "auto_plan".into(), name: "Auto-Plan — create plan for context (no subtasks)".into() },
                    SettingOption { id: "auto_subtasks".into(), name: "Auto-Plan + Subtasks — enforce completion via subtasks".into() },
                ]),
                readonly: false,
                default: Some("auto_plan".into()),
            },
        ),
        (
            "PROMPT_PLAN_MAX_TOKENS".into(),
            get_env_or_default("PROMPT_PLAN_MAX_TOKENS", "2048"),
            SettingMeta {
                field_type: "number".into(),
                description: "Maximum tokens for the planning LLM call".into(),
                options: None,
                readonly: false,
                default: Some("2048".into()),
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
            "PLANNING_COMPLEXITY_SIMPLE_MAX_CHARS".into(),
            get_env_or_default("PLANNING_COMPLEXITY_SIMPLE_MAX_CHARS", "60"),
            SettingMeta {
                field_type: "number".into(),
                description: "Max character count for simple prompts (greetings, short commands) — these get no plan".into(),
                options: None,
                readonly: false,
                default: Some("60".into()),
            },
        ),
        (
            "PLANNING_COMPLEXITY_STANDARD_MAX_CHARS".into(),
            get_env_or_default("PLANNING_COMPLEXITY_STANDARD_MAX_CHARS", "200"),
            SettingMeta {
                field_type: "number".into(),
                description: "Max character count for standard prompts — prompts above this get full planning with subtasks".into(),
                options: None,
                readonly: false,
                default: Some("200".into()),
            },
        ),
        (
            "PLANNING_COMPLEXITY_KEYWORDS".into(),
            get_env_or_default(
                "PLANNING_COMPLEXITY_KEYWORDS",
                "implement,refactor,redesign,architecture,create,build,design,develop,migrate,restructure,overhaul,rewrite,configure,set up,deploy,integrate,add feature,fix bug,resolve issue,multi-step,complex",
            ),
            SettingMeta {
                field_type: "textarea".into(),
                description: "Comma-separated keywords that trigger complex planning with subtasks".into(),
                options: None,
                readonly: false,
                default: Some("implement,refactor,redesign,architecture,create,build,design,develop,migrate,restructure,overhaul,rewrite,configure,set up,deploy,integrate,add feature,fix bug,resolve issue,multi-step,complex".into()),
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
            "USER_MAX_CHARS".into(),
            get_env_or_default("USER_MAX_CHARS", "1000"),
            SettingMeta {
                field_type: "number".into(),
                description: "Max characters for USER.md in the system prompt".into(),
                options: None,
                readonly: false,
                default: Some("1000".into()),
            },
        ),
        (
            "SUMMARIZE_AFTER_DAYS".into(),
            get_env_or_default("SUMMARIZE_AFTER_DAYS", "7"),
            SettingMeta {
                field_type: "number".into(),
                description: "Days of inactivity before auto-summarizing threads".into(),
                options: None,
                readonly: false,
                default: Some("7".into()),
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
            "SUMMARY_WINDOW".into(),
            get_env_or_default("SUMMARY_WINDOW", "10"),
            SettingMeta {
                field_type: "number".into(),
                description: "Threads per summary generation window".into(),
                options: None,
                readonly: false,
                default: Some("10".into()),
            },
        ),
        (
            "CHANNEL_SUMMARY_TOKENS".into(),
            get_env_or_default("CHANNEL_SUMMARY_TOKENS", "4096"),
            SettingMeta {
                field_type: "number".into(),
                description: "Maximum tokens for channel-level summary generation".into(),
                options: None,
                readonly: false,
                default: Some("4096".into()),
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
            "QDRANT_URL".into(),
            get_env_or_default("QDRANT_URL", "http://qdrant:6333"),
            SettingMeta {
                field_type: "text".into(),
                description: "Qdrant vector database URL".into(),
                options: None,
                readonly: true,
                default: Some("http://qdrant:6333".into()),
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
        (
            "WORKSPACE_DIR".into(),
            get_env_or_default("WORKSPACE_DIR", "/opt/workspace"),
            SettingMeta {
                field_type: "text".into(),
                description: "Workspace directory for projects".into(),
                options: None,
                readonly: true,
                default: Some("/opt/workspace".into()),
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
                    SettingOption { id: "off".into(), name: "Off — never insert prompts".into() },
                    SettingOption { id: "first".into(), name: "First — insert the first prompt only".into() },
                    SettingOption { id: "first+compact".into(), name: "First+Compact — first prompt + prompts after context compaction".into() },
                    SettingOption { id: "all".into(), name: "All — insert every prompt before every LLM call".into() },
                ]),
                readonly: false,
                default: Some("first".into()),
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
            "MAX_TOKENS" | "TEMPERATURE" | "LLM_MAX_TOKENS" => "general",
            "MAX_ITERATIONS_NO_PLAN"
            | "MAX_ITERATIONS_SIMPLE_PLAN"
            | "MAX_ITERATIONS_COMPLEX_PLAN" => "general",
            "PLANNING_MODE"
            | "PROMPT_PLAN_MAX_TOKENS"
            | "MAX_UNFINISHED_SUBTASK_RETRIES"
            | "PLANNING_COMPLEXITY_SIMPLE_MAX_CHARS"
            | "PLANNING_COMPLEXITY_STANDARD_MAX_CHARS"
            | "PLANNING_COMPLEXITY_KEYWORDS" => "planning",
            "SUMMARIZE_AFTER_DAYS"
            | "DELETE_AFTER_DAYS"
            | "SUMMARY_WINDOW"
            | "CHANNEL_SUMMARY_TOKENS"
            | "THREAD_SUMMARY_TOKENS"
            | "MEMORY_MAX_CHARS"
            | "USER_MAX_CHARS" => "memory",
            "LLM_PROVIDER" | "PROMPT_LOG_LEVEL" => "general",
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

/// GET /settings — return all settings organized by category.
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

/// PUT /settings — update one or more settings and write to .env.
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
        "MAX_ITERATIONS_SIMPLE_PLAN",
        "MAX_ITERATIONS_COMPLEX_PLAN",
        "LLM_MAX_TOKENS",
        "PROMPT_PLAN_MAX_TOKENS",
        "PLANNING_MODE",
        "MAX_UNFINISHED_SUBTASK_RETRIES",
        "SUMMARIZE_AFTER_DAYS",
        "DELETE_AFTER_DAYS",
        "SUMMARY_WINDOW",
        "CHANNEL_SUMMARY_TOKENS",
        "THREAD_SUMMARY_TOKENS",
        "MEMORY_MAX_CHARS",
        "USER_MAX_CHARS",
        "LLM_PROVIDER",
        "MAX_POOL_CONNECTIONS",
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
