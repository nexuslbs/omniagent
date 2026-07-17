use crate::error::{AppResult, ErrorContext};
use crate::mcp::{AppContext, McpRegistry};
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock;

// ── Global mutable config ──────────────────────────────────────────────────

/// Global mutable config shared across the application.
/// Initialized once at startup, updated when settings change via the API.
pub static GLOBAL_CONFIG: OnceLock<Arc<RwLock<AgentConfig>>> = OnceLock::new();

/// Initialize the global config from a loaded AgentConfig.
/// Returns the Arc so callers can hold their own reference.
/// Panics if called more than once (safety guarantee for startup).
pub fn init_global(config: AgentConfig) -> Arc<RwLock<AgentConfig>> {
    let arc = Arc::new(RwLock::new(config));
    GLOBAL_CONFIG
        .set(arc.clone())
        .unwrap_or_else(|_| panic!("GLOBAL_CONFIG already initialized"));
    arc
}

/// Reload the global config from environment variables.
/// Call this after settings are updated (e.g. from PUT /settings).
/// Does nothing if the global hasn't been initialized yet.
pub fn reload_global() {
    if let Some(global) = GLOBAL_CONFIG.get() {
        match AgentConfig::from_env() {
            Ok(new_config) => {
                tracing::info!("Reloaded global config from environment");
                if let Ok(mut guard) = global.write() {
                    *guard = new_config;
                }
            }
            Err(e) => {
                tracing::error!("Failed to reload config from environment: {:?}", e);
            }
        }
    }
}

/// Reload the global config from settings.yml, resolving $env:/$secret: refs.
/// Called after PUT /settings writes to settings.yml so the change takes
/// effect immediately without a container restart.
/// Does nothing if the global hasn't been initialized yet.
pub async fn reload_global_from_settings(data_dir: &str, pool: &PgPool) {
    if let Some(global) = GLOBAL_CONFIG.get() {
        match AgentConfig::from_settings_yaml(data_dir, pool).await {
            Ok(new_config) => {
                tracing::info!("Reloaded global config from settings.yml");
                if let Ok(mut guard) = global.write() {
                    *guard = new_config;
                }
            }
            Err(e) => {
                tracing::error!("Failed to reload config from settings.yml: {:?}", e);
            }
        }
    }
}

/// Get a reference to the global config, if initialized.
pub fn get_global() -> Option<&'static Arc<RwLock<AgentConfig>>> {
    GLOBAL_CONFIG.get()
}

// ── AgentConfig ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub llm_api_key: String,
    pub default_provider: String,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Max iterations for threads with no planning mode (complexity-based).
    pub max_iterations_no_plan: u32,
    /// Max iterations for threads with planning enabled.
    pub max_iterations_plan: u32,
    /// Max tokens for the per-thread end-of-execution summary LLM call.
    pub thread_summary_tokens: u32,
    /// Max retries for unfinished subtasks before marking the thread as failed.
    pub max_unfinished_subtask_retries: u32,
    /// Days before old messages and summaries are deleted.
    pub delete_after_days: u32,
    /// MCP tool name for generating the LLM prompt (system prompt + context assembly).
    /// The tool is called by the executor before each LLM invocation to build
    /// the complete prompt from profile, memory, skills, thread context, etc.
    /// Default: "prompt_generate": change this if the prompt plugin is registered
    /// under a different name.
    pub prompt_tool_name: String,
    /// MCP tool name for compacting conversation history.
    /// Default: "prompt_compact-messages".
    pub compact_messages_tool_name: String,

    // Context management / token explosion prevention
    /// Soft char budget for the prompt. When exceeded, condense every STATE_BLOCK_UPDATE_INTERVAL turns.
    pub prompt_char_budget_soft: usize,
    /// Hard char budget for the prompt. When exceeded, condense before ANY LLM call to bring below soft.
    pub prompt_char_budget_hard: usize,
    /// Max chars for old messages after condensation (metadata block stays).
    pub old_message_char_budget: usize,
    /// How often (in iterations) to condense when soft budget is exceeded.
    pub state_block_update_interval: u32,
    /// How many full assistant→tool cycles to keep verbatim during condensation.
    pub condense_keep_turns: usize,
    /// Token budget: soft threshold for condensation (uses configured tokenizer tool if available, else char budget).
    pub prompt_token_budget_soft: usize,
    /// Token budget: hard threshold, condense before any LLM call.
    pub prompt_token_budget_hard: usize,
    /// MCP tool name for token counting. Empty = fall back to char budgets.
    pub tokenizer_encoding_tool: String,

    /// When to insert prompts as messages (msg_type: "prompt") into the messages table.
    /// - "off": never insert
    /// - "first": insert the first LLM call's prompt only (default)
    /// - "first+compact": first prompt + prompts after context compaction
    /// - "all": insert every prompt before every LLM call
    pub prompt_log_level: String,

    /// Threshold in seconds for background mode — tools that complete within
    /// this time return normally. Tools that exceed this return a "processing"
    /// result with a task ID and continue executing in the background.
    /// Default: 30 seconds.
    pub tool_bg_secs: u64,

    // Infrastructure config (merged from former config::Config)
    pub database_url: String,
    pub database_readonly_url: String,
    pub host: String,
    pub port: u16,

    /// Max retries for spawning platform messages (external channels).
    pub platform_max_spawn_retries: u32,
    /// Max inline file KB for attachments.
    pub max_inline_file_kb: u32,
    /// Default profile name (used at login / session start).
    pub default_profile: String,
    /// Workspace directory path.
    pub workspace_dir: String,
    /// Path to MCP servers config file.
    pub mcp_servers_config: String,
}

/// Shared context bundle used by channel_handler and process_thread.
/// Combines the infrastructure dependencies that are passed to both functions.
#[derive(Clone)]
pub struct AgentContext {
    pub pool: PgPool,
    pub llm: Arc<crate::llm::LLMClient>,
    pub config: Arc<RwLock<AgentConfig>>,
    pub mcp: Arc<tokio::sync::RwLock<McpRegistry>>,
    pub ctx: AppContext,
}

impl AgentContext {
    /// Take a snapshot of the current config for use during a single thread/task.
    /// This ensures consistent field values throughout one processing cycle
    /// even if the global config is updated concurrently.
    pub fn config_snapshot(&self) -> AgentConfig {
        self.config.read().unwrap().clone()
    }
}

impl AgentConfig {
    /// Load agent configuration at startup.
    ///
    /// Bootstrap settings (DATABASE_URL, HOST, PORT, OMNI_DIR) come from
    /// process environment variables. All other settings are read from
    /// settings.yml (if available) or use hardcoded defaults.
    /// After startup, use reload_global_from_settings() for hot-reload.
    pub fn from_env() -> AppResult<Self> {
        // Bootstrap: read OMNI_DIR from env to find settings.yml
        let data_dir = std::env::var("OMNI_DIR").unwrap_or_else(|_| "/opt/omni".to_string());
        let settings = crate::server::settings::load_settings_file(&data_dir);

        // Helper: get a resolved value or default (sync — no $secret: resolution at startup)
        let get = |key: &str, default: &str| -> String {
            settings.get(key).cloned().unwrap_or_else(|| default.to_string())
        };

        Ok(Self {
            llm_api_key: String::new(),
            default_provider: get("default_provider", "openai"),
            max_tokens: get("max_tokens", "4096").parse().unwrap_or(4096),
            temperature: get("temperature", "0.7").parse().unwrap_or(0.7),
            max_iterations_no_plan: get("max_iterations_no_plan", "30").parse().unwrap_or(30),
            max_iterations_plan: get("max_iterations_plan", "120").parse().unwrap_or(120),
            thread_summary_tokens: get("thread_summary_tokens", "2048").parse().unwrap_or(2048),
            max_unfinished_subtask_retries: get("max_unfinished_subtask_retries", "3").parse().unwrap_or(3),
            delete_after_days: get("delete_after_days", "30").parse().unwrap_or(30),
            prompt_tool_name: get("prompt_generate_tool", "prompt_generate"),
            compact_messages_tool_name: get("prompt_compact_messages_tool", "prompt_compact-messages"),

            // Context management thresholds
            prompt_char_budget_soft: get("prompt_char_budget_soft", "350000").parse().unwrap_or(350000),
            prompt_char_budget_hard: get("prompt_char_budget_hard", "500000").parse().unwrap_or(500000),
            old_message_char_budget: get("old_message_char_budget", "100000").parse().unwrap_or(100000),
            state_block_update_interval: get("state_block_update_interval", "5").parse().unwrap_or(5),
            condense_keep_turns: get("condense_keep_turns", "4").parse().unwrap_or(4).max(1),

            // Token-based budgets
            prompt_token_budget_soft: get("prompt_token_budget_soft", "200000").parse().unwrap_or(200000),
            prompt_token_budget_hard: get("prompt_token_budget_hard", "350000").parse().unwrap_or(350000),
            tokenizer_encoding_tool: get("tokenizer_encoding_tool", ""),

            prompt_log_level: get("prompt_log_level", "first"),

            tool_bg_secs: get("tool_bg_secs", "30").parse().unwrap_or(30),

            // Bootstrap: infrastructure from env
            database_url: std::env::var("DATABASE_URL").ctx("DATABASE_URL must be set")?,
            database_readonly_url: std::env::var("DATABASE_READONLY_URL").unwrap_or_else(|_| {
                std::env::var("DATABASE_URL")
                    .unwrap_or_else(|_| "postgres://localhost:5432/omniagent".to_string())
            }),
            host: std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: std::env::var("PORT")
                .unwrap_or_else(|_| "8080".to_string())
                .parse()
                .ctx("PORT must be a valid number")?,
            platform_max_spawn_retries: get("platform_max_spawn_retries", "3").parse().unwrap_or(3),
            max_inline_file_kb: get("max_inline_file_kb", "100").parse().unwrap_or(100),
            default_profile: get("default_profile", "omni"),
            workspace_dir: get("workspace_dir", "/opt/workspace"),
            mcp_servers_config: get("mcp_servers_config", ""),
        })
    }

    /// Load agent configuration from settings.yml file.
    /// Resolves $env:/$secret: references. Bootstrap settings (host, port,
    /// database_url) still come from process environment variables.
    /// Fields not present in settings.yml use their from_env() defaults.
    pub async fn from_settings_yaml(data_dir: &str, pool: &PgPool) -> AppResult<Self> {
        let mut settings = crate::server::settings::load_settings_file(data_dir);
        crate::server::settings::resolve_setting_values(&mut settings, pool).await;

        // Helper: get a resolved value or default
        let get = |key: &str, default: &str| -> String {
            settings
                .get(key)
                .cloned()
                .unwrap_or_else(|| default.to_string())
        };

        Ok(Self {
            llm_api_key: String::new(),
            default_provider: get("default_provider", "openai"),
            max_tokens: get("max_tokens", "4096").parse().unwrap_or(4096),
            temperature: get("temperature", "0.7").parse().unwrap_or(0.7),
            max_iterations_no_plan: get("max_iterations_no_plan", "30").parse().unwrap_or(30),
            max_iterations_plan: get("max_iterations_plan", "120").parse().unwrap_or(120),
            thread_summary_tokens: get("thread_summary_tokens", "2048").parse().unwrap_or(2048),
            max_unfinished_subtask_retries: get("max_unfinished_subtask_retries", "3")
                .parse()
                .unwrap_or(3),
            delete_after_days: get("delete_after_days", "30").parse().unwrap_or(30),
            prompt_tool_name: get("prompt_generate_tool", "prompt_generate"),
            compact_messages_tool_name: get("prompt_compact_messages_tool", "prompt_compact-messages"),

            // Context management thresholds
            prompt_char_budget_soft: get("prompt_char_budget_soft", "350000").parse().unwrap_or(350000),
            prompt_char_budget_hard: get("prompt_char_budget_hard", "500000").parse().unwrap_or(500000),
            old_message_char_budget: get("old_message_char_budget", "100000").parse().unwrap_or(100000),
            state_block_update_interval: get("state_block_update_interval", "5").parse().unwrap_or(5),
            condense_keep_turns: get("condense_keep_turns", "4").parse().unwrap_or(4).max(1),

            // Token-based budgets
            prompt_token_budget_soft: get("prompt_token_budget_soft", "200000")
                .parse()
                .unwrap_or(200000),
            prompt_token_budget_hard: get("prompt_token_budget_hard", "350000")
                .parse()
                .unwrap_or(350000),
            tokenizer_encoding_tool: get("tokenizer_encoding_tool", ""),

            prompt_log_level: get("prompt_log_level", "first"),

            tool_bg_secs: get("tool_bg_secs", "30").parse().unwrap_or(30),

            // Bootstrap settings always from process env
            database_url: std::env::var("DATABASE_URL").ctx("DATABASE_URL must be set")?,
            database_readonly_url: std::env::var("DATABASE_READONLY_URL").unwrap_or_else(|_| {
                std::env::var("DATABASE_URL")
                    .unwrap_or_else(|_| "postgres://localhost:5432/omniagent".to_string())
            }),
            host: std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: std::env::var("PORT")
                .unwrap_or_else(|_| "8080".to_string())
                .parse()
                .ctx("PORT must be a valid number")?,
            platform_max_spawn_retries: get("platform_max_spawn_retries", "3").parse().unwrap_or(3),
            max_inline_file_kb: get("max_inline_file_kb", "100")
                .parse()
                .unwrap_or(100),
            default_profile: get("default_profile", "omni"),
            workspace_dir: get("workspace_dir", "/opt/workspace"),
            mcp_servers_config: get("mcp_servers_config", ""),
        })
    }
}
