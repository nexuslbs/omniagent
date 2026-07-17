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
    pub llm_provider: String,
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
    #[allow(dead_code)]
    pub prompt_char_budget_hard: usize,
    /// Max chars for old messages after condensation (metadata block stays).
    pub old_message_char_budget: usize,
    /// How often (in iterations) to condense when soft budget is exceeded.
    pub state_block_update_interval: u32,
    /// How many full assistant→tool cycles to keep verbatim during condensation.
    pub condense_keep_turns: usize,
    /// Token budget: soft threshold for condensation (uses tiktoken for accurate counting).
    pub prompt_token_budget_soft: usize,
    /// Token budget: hard threshold, condense before any LLM call (uses tiktoken).
    pub prompt_token_budget_hard: usize,
    /// tiktoken encoding/model name ("gpt-4", "cl100k_base", "o200k_base").
    pub tokenizer_encoding: String,
    /// Multiplier to account for provider tokenizer mismatch with tiktoken.
    pub prompt_token_safety_factor: f64,

    /// When to insert prompts as messages (msg_type: "prompt") into the messages table.
    /// - "off": never insert
    /// - "first": insert the first LLM call's prompt only (default)
    /// - "first+compact": first prompt + prompts after context compaction
    /// - "all": insert every prompt before every LLM call
    pub prompt_log_level: String,

    /// Global watchdog configuration for tools that don't have their own.
    /// Applied to all tool calls that don't have a per-tool watchdog defined.
    /// If None, no watchdog runs for tools without their own configuration.
    pub global_watchdog: Option<crate::mcp::WatchdogConfig>,

    /// Short timeout (seconds) for the "fast path" — tools that complete within
    /// this time return normally. Tools that exceed this return a "processing"
    /// result and continue in the background.
    /// Default: 5 seconds.
    pub tool_short_timeout_secs: u64,
    /// Long timeout (seconds) for background task execution — the maximum time
    /// a background tool task is allowed to run before being force-cancelled.
    /// Default: 300 seconds (5 minutes).
    pub tool_long_timeout_secs: u64,

    // Infrastructure config (merged from former config::Config)
    pub database_url: String,
    pub database_readonly_url: String,
    pub host: String,
    pub port: u16,

    // Vectorization settings
    pub vectorize_messages: bool,
    pub vectorize_wiki: bool,
    pub messages_vectorization_method: String,
    pub messages_vectorization_api_url: Option<String>,
    pub messages_vectorization_interval_secs: u64,
    pub messages_vectorization_protocol: String,
    pub messages_vectorization_api_key: Option<String>,
    pub messages_vectorization_api_model: Option<String>,
    pub wiki_vectorization_method: String,
    pub wiki_vectorization_api_url: Option<String>,
    pub wiki_vectorization_interval_secs: u64,
    pub wiki_vectorization_protocol: String,
    pub wiki_vectorization_api_key: Option<String>,
    pub wiki_vectorization_api_model: Option<String>,
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
    /// Load agent configuration from environment variables.
    ///
    /// # Env vars
    /// - `LLM_PROVIDER`: Provider name (default: "openai")
    /// - `MAX_TOKENS`: Max tokens per response (default: 4096)
    /// - `TEMPERATURE`: Sampling temperature (default: 0.7)
    /// - `MAX_ITERATIONS`: Max agent turns per thread before skipping (default: 60)
    ///
    /// The API key comes from the provider's plugin config (providers.yml with $env:
    /// references), not from hardcoded env var names.
    pub fn from_env() -> AppResult<Self> {
        Ok(Self {
            llm_api_key: String::new(),
            llm_provider: std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "openai".to_string()),
            max_tokens: std::env::var("MAX_TOKENS")
                .unwrap_or_else(|_| "4096".to_string())
                .parse()
                .unwrap_or(4096),
            temperature: std::env::var("TEMPERATURE")
                .unwrap_or_else(|_| "0.7".to_string())
                .parse()
                .unwrap_or(0.7),
            max_iterations_no_plan: std::env::var("MAX_ITERATIONS_NO_PLAN")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .unwrap_or(30),
            max_iterations_plan: std::env::var("MAX_ITERATIONS_PLAN")
                .unwrap_or_else(|_| "120".to_string())
                .parse()
                .unwrap_or(120),
            thread_summary_tokens: std::env::var("THREAD_SUMMARY_TOKENS")
                .unwrap_or_else(|_| "2048".to_string())
                .parse()
                .unwrap_or(2048),
            max_unfinished_subtask_retries: std::env::var("MAX_UNFINISHED_SUBTASK_RETRIES")
                .unwrap_or_else(|_| "3".to_string())
                .parse()
                .unwrap_or(3),
            delete_after_days: std::env::var("DELETE_AFTER_DAYS")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .unwrap_or(30),
            prompt_tool_name: std::env::var("PROMPT_GENERATE_TOOL")
                .unwrap_or_else(|_| "prompt_generate".to_string()),
            compact_messages_tool_name: std::env::var("PROMPT_COMPACT_MESSAGES_TOOL")
                .unwrap_or_else(|_| "prompt_compact-messages".to_string()),

            // Context management thresholds
            prompt_char_budget_soft: std::env::var("PROMPT_CHAR_BUDGET_SOFT")
                .unwrap_or_else(|_| "350000".to_string())
                .parse()
                .unwrap_or(350000),
            prompt_char_budget_hard: std::env::var("PROMPT_CHAR_BUDGET_HARD")
                .unwrap_or_else(|_| "500000".to_string())
                .parse()
                .unwrap_or(500000),
            old_message_char_budget: std::env::var("OLD_MESSAGE_CHAR_BUDGET")
                .unwrap_or_else(|_| "100000".to_string())
                .parse()
                .unwrap_or(100000),
            state_block_update_interval: std::env::var("STATE_BLOCK_UPDATE_INTERVAL")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .unwrap_or(5),
            condense_keep_turns: std::env::var("CONDENSE_KEEP_TURNS")
                .unwrap_or_else(|_| "4".to_string())
                .parse()
                .unwrap_or(4)
                .max(1),

            // Token-based budgets (use tiktoken for accurate counting)
            prompt_token_budget_soft: std::env::var("PROMPT_TOKEN_BUDGET_SOFT")
                .unwrap_or_else(|_| "200000".to_string())
                .parse()
                .unwrap_or(200000),
            prompt_token_budget_hard: std::env::var("PROMPT_TOKEN_BUDGET_HARD")
                .unwrap_or_else(|_| "350000".to_string())
                .parse()
                .unwrap_or(350000),
            tokenizer_encoding: std::env::var("TOKENIZER_ENCODING")
                .unwrap_or_else(|_| "gpt-4".to_string()),
            prompt_token_safety_factor: std::env::var("PROMPT_TOKEN_SAFETY_FACTOR")
                .unwrap_or_else(|_| "15.0".to_string())
                .parse()
                .unwrap_or(15.0),

            prompt_log_level: std::env::var("PROMPT_LOG_LEVEL")
                .unwrap_or_else(|_| "first".to_string()),

            global_watchdog: std::env::var("WATCHDOG_DEFAULT").ok().and_then(|v| {
                serde_json::from_str::<crate::mcp::WatchdogConfig>(&v).ok()
            }),

            tool_short_timeout_secs: std::env::var("TOOL_SHORT_TIMEOUT_SECS")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .unwrap_or(5),
            tool_long_timeout_secs: std::env::var("TOOL_LONG_TIMEOUT_SECS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300),

            // Infrastructure config (merged from former config::Config)
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
            vectorize_messages: std::env::var("VECTORIZE_MESSAGES")
                .unwrap_or_else(|_| "false".to_string())
                .parse::<bool>()
                .unwrap_or(false),
            vectorize_wiki: std::env::var("VECTORIZE_WIKI")
                .unwrap_or_else(|_| "false".to_string())
                .parse::<bool>()
                .unwrap_or(false),
            messages_vectorization_method: std::env::var("MESSAGES_VECTORIZATION_METHOD")
                .unwrap_or_else(|_| "local".to_string()),
            messages_vectorization_api_url: std::env::var("MESSAGES_VECTORIZATION_API_URL").ok(),
            messages_vectorization_interval_secs: std::env::var("MESSAGES_VECTORIZATION_INTERVAL")
                .unwrap_or_else(|_| "3600".to_string())
                .parse()
                .ctx("MESSAGES_VECTORIZATION_INTERVAL must be a valid number")?,
            messages_vectorization_protocol: std::env::var("MESSAGES_VECTORIZATION_PROTOCOL")
                .unwrap_or_else(|_| "openai".to_string()),
            messages_vectorization_api_key: std::env::var("MESSAGES_VECTORIZATION_API_KEY").ok(),
            messages_vectorization_api_model: std::env::var("MESSAGES_VECTORIZATION_API_MODEL")
                .ok(),
            wiki_vectorization_method: std::env::var("WIKI_VECTORIZATION_METHOD")
                .unwrap_or_else(|_| "local".to_string()),
            wiki_vectorization_api_url: std::env::var("WIKI_VECTORIZATION_API_URL").ok(),
            wiki_vectorization_interval_secs: std::env::var("WIKI_VECTORIZATION_INTERVAL")
                .unwrap_or_else(|_| "3600".to_string())
                .parse()
                .ctx("WIKI_VECTORIZATION_INTERVAL must be a valid number")?,
            wiki_vectorization_protocol: std::env::var("WIKI_VECTORIZATION_PROTOCOL")
                .unwrap_or_else(|_| "openai".to_string()),
            wiki_vectorization_api_key: std::env::var("WIKI_VECTORIZATION_API_KEY").ok(),
            wiki_vectorization_api_model: std::env::var("WIKI_VECTORIZATION_API_MODEL").ok(),
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
            llm_provider: get("llm_provider", "openai"),
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
            tokenizer_encoding: get("tokenizer_encoding", "gpt-4"),
            prompt_token_safety_factor: get("prompt_token_safety_factor", "15.0")
                .parse()
                .unwrap_or(15.0),

            prompt_log_level: get("prompt_log_level", "first"),

            global_watchdog: settings.get("watchdog_default").and_then(|v| {
                serde_json::from_str::<crate::mcp::WatchdogConfig>(v).ok()
            }),

            tool_short_timeout_secs: get("tool_short_timeout_secs", "5").parse().unwrap_or(5),
            tool_long_timeout_secs: get("tool_long_timeout_secs", "300").parse().unwrap_or(300),

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
            vectorize_messages: get("vectorize_messages", "false")
                .parse::<bool>()
                .unwrap_or(false),
            vectorize_wiki: get("vectorize_wiki", "false")
                .parse::<bool>()
                .unwrap_or(false),
            messages_vectorization_method: get("messages_vectorization_method", "local"),
            messages_vectorization_api_url: settings
                .get("messages_vectorization_api_url")
                .cloned()
                .filter(|v| !v.is_empty()),
            messages_vectorization_interval_secs: get("messages_vectorization_interval", "3600")
                .parse()
                .unwrap_or(3600),
            messages_vectorization_protocol: get("messages_vectorization_protocol", "openai"),
            messages_vectorization_api_key: settings
                .get("messages_vectorization_api_key")
                .cloned()
                .filter(|v| !v.is_empty()),
            messages_vectorization_api_model: settings
                .get("messages_vectorization_api_model")
                .cloned()
                .filter(|v| !v.is_empty()),
            wiki_vectorization_method: get("wiki_vectorization_method", "local"),
            wiki_vectorization_api_url: settings
                .get("wiki_vectorization_api_url")
                .cloned()
                .filter(|v| !v.is_empty()),
            wiki_vectorization_interval_secs: get("wiki_vectorization_interval", "3600")
                .parse()
                .unwrap_or(3600),
            wiki_vectorization_protocol: get("wiki_vectorization_protocol", "openai"),
            wiki_vectorization_api_key: settings
                .get("wiki_vectorization_api_key")
                .cloned()
                .filter(|v| !v.is_empty()),
            wiki_vectorization_api_model: settings
                .get("wiki_vectorization_api_model")
                .cloned()
                .filter(|v| !v.is_empty()),
        })
    }
}
