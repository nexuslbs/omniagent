use crate::agent::plugin_manager::PluginManager;
use crate::error::{AppResult, ErrorContext};
use crate::mcp::AppContext;
use parking_lot::RwLock;
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::OnceLock;

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
                let mut guard = global.write();
                *guard = new_config;
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
                let mut guard = global.write();
                *guard = new_config;
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
    /// Output budget used when a response is truncated (finish_reason=length).
    /// Normal calls keep the small `max_tokens`; the retry escalates to this.
    pub max_tokens_on_truncation: u32,
    pub temperature: f32,
    /// Max iterations for threads with no planning mode (complexity-based).
    pub max_iterations_no_plan: u32,
    /// Max iterations for threads with planning enabled.
    pub max_iterations_plan: u32,
    /// Max tokens for the per-thread end-of-execution summary LLM call.
    pub thread_summary_tokens: u32,
    /// Max retries for unfinished subtasks before marking the thread as failed.
    pub max_unfinished_subtask_retries: u32,
    /// Max consecutive LLM provider errors before the thread is marked failed.
    /// The provider just returns the error; omniagent owns the retry policy.
    /// Default: 3.
    pub provider_max_retries: u32,
    /// Days before old messages, summaries, threads and kanban history are deleted.
    /// 0 disables the cleanup entirely.
    pub delete_after_days: u32,
    /// Interval (seconds) between in-process kanban dispatcher runs (0 disables; default 15).
    pub kanban_dispatcher_interval_secs: u64,
    /// MCP tool name for generating the LLM prompt (system prompt + context assembly).
    /// The tool is called by the executor before each LLM invocation to build
    /// the complete prompt from profile, memory, skills, thread context, etc.
    /// Default: "prompt_generate": change this if the prompt plugin is registered
    /// under a different name.
    pub prompt_tool_name: String,
    /// MCP tool name for compacting conversation history.
    /// Default: "prompt_compact-messages".
    pub compact_messages_tool_name: String,

    /// Hard context budget (chars): engine-level pruning runs ONLY when the
    /// total context size exceeds this threshold. Sourced from the SAME
    /// settings key as the prompt plugin (`prompt_char_budget_hard`) so the
    /// two layers always agree — no hardcoded limits in code.
    pub char_budget_hard: usize,
    /// Soft context budget (chars): pruning compacts until the total size
    /// drops below this (`prompt_char_budget_soft`).
    pub char_budget_soft: usize,
    /// How many of the most recent read-type tool results are kept in full
    /// by pruning (settings `read_keep_last`).
    pub read_keep_last: usize,
    /// Excerpt size (chars) kept for older read-type results (settings
    /// `read_excerpt_chars`).
    pub read_excerpt_chars: usize,
    /// Cap for the thread's durable `auto-notes.md` (settings
    /// `auto_note_max_chars`).
    pub auto_note_max_chars: usize,
    /// Per-entry cap for engine auto-notes (settings `auto_note_entry_chars`).
    pub auto_note_entry_chars: usize,

    // When to insert prompts as messages (msg_type: "prompt") into the messages table.
    /// - "off": never insert
    /// - "first": insert the first LLM call's prompt only (default)
    /// - "first+compact": first prompt + prompts after context compaction
    /// - "all": insert every prompt before every LLM call
    pub prompt_log_level: String,

    /// Threshold in seconds for background mode : tools that complete within
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

    // ── Vectorization (message/wiki background embedding workers) ──
    /// Whether the background message vectorizer runs (populates embedding_vec).
    pub vectorize_messages: bool,
    /// Embedding method for messages: "local" (HashVectorizer) or "api".
    pub messages_vectorization_method: String,
    pub messages_vectorization_api_url: Option<String>,
    pub messages_vectorization_protocol: String,
    pub messages_vectorization_api_key: Option<String>,
    pub messages_vectorization_api_model: Option<String>,
    /// Poll interval (seconds) between message vectorizer batch runs.
    pub messages_vectorization_interval_secs: u64,
    /// Whether the background wiki vectorizer runs (Qdrant).
    pub vectorize_wiki: bool,
    pub wiki_vectorization_method: String,
    pub wiki_vectorization_api_url: Option<String>,
    pub wiki_vectorization_protocol: String,
    pub wiki_vectorization_api_key: Option<String>,
    pub wiki_vectorization_api_model: Option<String>,
    pub wiki_vectorization_interval_secs: u64,
}

/// Shared context bundle used by channel_handler and process_thread.
/// Combines the infrastructure dependencies that are passed to both functions.
#[derive(Clone)]
pub struct AgentContext {
    pub pool: PgPool,
    pub llm: Arc<crate::llm::LLMClient>,
    pub config: Arc<RwLock<AgentConfig>>,
    pub ctx: AppContext,
    pub plugin_manager: Arc<dyn PluginManager>,
}

impl AgentContext {
    /// Take a snapshot of the current config for use during a single thread/task.
    /// This ensures consistent field values throughout one processing cycle
    /// even if the global config is updated concurrently.
    pub fn config_snapshot(&self) -> AgentConfig {
        self.config.read().clone()
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

        // Helper: get a resolved value or default (sync : no $secret: resolution at startup)
        let get = |key: &str, default: &str| -> String {
            settings
                .get(key)
                .cloned()
                .unwrap_or_else(|| default.to_string())
        };

        // Empty strings in settings.yml are treated as None for optional fields.
        let opt_str = |v: &str| -> Option<String> {
            if v.trim().is_empty() {
                None
            } else {
                Some(v.to_string())
            }
        };

        Ok(Self {
            llm_api_key: String::new(),
            default_provider: get("default_provider", "openai"),
            max_tokens: get("max_tokens", "32768").parse().unwrap_or(32768),
            max_tokens_on_truncation: get("max_tokens_on_truncation", "16384")
                .parse()
                .unwrap_or(16384),
            temperature: get("temperature", "0.7").parse().unwrap_or(0.7),
            max_iterations_no_plan: get("max_iterations_no_plan", "30").parse().unwrap_or(30),
            max_iterations_plan: get("max_iterations_plan", "120").parse().unwrap_or(120),
            thread_summary_tokens: get("thread_summary_tokens", "2048").parse().unwrap_or(2048),
            max_unfinished_subtask_retries: get("max_unfinished_subtask_retries", "1")
                .parse()
                .unwrap_or(3),
            provider_max_retries: get("provider_max_retries", "3").parse().unwrap_or(3),
            delete_after_days: get("delete_after_days", "30").parse().unwrap_or(30),
            kanban_dispatcher_interval_secs: get("kanban_dispatcher_interval", "15")
                .parse()
                .unwrap_or(15),
            prompt_tool_name: get("prompt_generate_tool", "prompt_generate"),
            compact_messages_tool_name: get(
                "prompt_compact_messages_tool",
                "prompt_compact-messages",
            ),
            char_budget_hard: get("prompt_char_budget_hard", "200000")
                .parse()
                .unwrap_or(200000),
            char_budget_soft: get("prompt_char_budget_soft", "100000")
                .parse()
                .unwrap_or(100000),
            read_keep_last: get("read_keep_last", "3").parse().unwrap_or(3),
            read_excerpt_chars: get("read_excerpt_chars", "2000").parse().unwrap_or(2000),
            auto_note_max_chars: get("auto_note_max_chars", "24000").parse().unwrap_or(24000),
            auto_note_entry_chars: get("auto_note_entry_chars", "3000").parse().unwrap_or(3000),

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

            // Vectorization — defaults match settings.yml so the worker is
            // active out of the box (vectorize_messages: true by default).
            vectorize_messages: get("vectorize_messages", "true").parse().unwrap_or(true),
            messages_vectorization_method: get("messages_vectorization_method", "local"),
            messages_vectorization_api_url: opt_str(&get("messages_vectorization_api_url", "")),
            messages_vectorization_protocol: get("messages_vectorization_protocol", "openai"),
            messages_vectorization_api_key: opt_str(&get("messages_vectorization_api_key", "")),
            messages_vectorization_api_model: opt_str(&get("messages_vectorization_api_model", "")),
            messages_vectorization_interval_secs: get("messages_vectorization_interval", "3600")
                .parse()
                .unwrap_or(3600),
            vectorize_wiki: get("vectorize_wiki", "false").parse().unwrap_or(false),
            wiki_vectorization_method: get("wiki_vectorization_method", "local"),
            wiki_vectorization_api_url: opt_str(&get("wiki_vectorization_api_url", "")),
            wiki_vectorization_protocol: get("wiki_vectorization_protocol", "openai"),
            wiki_vectorization_api_key: opt_str(&get("wiki_vectorization_api_key", "")),
            wiki_vectorization_api_model: opt_str(&get("wiki_vectorization_api_model", "")),
            wiki_vectorization_interval_secs: get("wiki_vectorization_interval", "3600")
                .parse()
                .unwrap_or(3600),
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

        // Empty strings in settings.yml are treated as None for optional fields.
        let opt_str = |v: &str| -> Option<String> {
            if v.trim().is_empty() {
                None
            } else {
                Some(v.to_string())
            }
        };

        Ok(Self {
            llm_api_key: String::new(),
            default_provider: get("default_provider", "openai"),
            max_tokens: get("max_tokens", "32768").parse().unwrap_or(32768),
            max_tokens_on_truncation: get("max_tokens_on_truncation", "16384")
                .parse()
                .unwrap_or(16384),
            temperature: get("temperature", "0.7").parse().unwrap_or(0.7),
            max_iterations_no_plan: get("max_iterations_no_plan", "30").parse().unwrap_or(30),
            max_iterations_plan: get("max_iterations_plan", "120").parse().unwrap_or(120),
            thread_summary_tokens: get("thread_summary_tokens", "2048").parse().unwrap_or(2048),
            max_unfinished_subtask_retries: get("max_unfinished_subtask_retries", "1")
                .parse()
                .unwrap_or(3),
            provider_max_retries: get("provider_max_retries", "3").parse().unwrap_or(3),
            delete_after_days: get("delete_after_days", "30").parse().unwrap_or(30),
            kanban_dispatcher_interval_secs: get("kanban_dispatcher_interval", "15")
                .parse()
                .unwrap_or(15),
            prompt_tool_name: get("prompt_generate_tool", "prompt_generate"),
            compact_messages_tool_name: get(
                "prompt_compact_messages_tool",
                "prompt_compact-messages",
            ),
            char_budget_hard: get("prompt_char_budget_hard", "200000")
                .parse()
                .unwrap_or(200000),
            char_budget_soft: get("prompt_char_budget_soft", "100000")
                .parse()
                .unwrap_or(100000),
            read_keep_last: get("read_keep_last", "3").parse().unwrap_or(3),
            read_excerpt_chars: get("read_excerpt_chars", "2000").parse().unwrap_or(2000),
            auto_note_max_chars: get("auto_note_max_chars", "24000").parse().unwrap_or(24000),
            auto_note_entry_chars: get("auto_note_entry_chars", "3000").parse().unwrap_or(3000),

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
            max_inline_file_kb: get("max_inline_file_kb", "100").parse().unwrap_or(100),
            default_profile: get("default_profile", "omni"),

            // Vectorization (same defaults as from_env; values come from settings.yml)
            vectorize_messages: get("vectorize_messages", "true").parse().unwrap_or(true),
            messages_vectorization_method: get("messages_vectorization_method", "local"),
            messages_vectorization_api_url: opt_str(&get("messages_vectorization_api_url", "")),
            messages_vectorization_protocol: get("messages_vectorization_protocol", "openai"),
            messages_vectorization_api_key: opt_str(&get("messages_vectorization_api_key", "")),
            messages_vectorization_api_model: opt_str(&get("messages_vectorization_api_model", "")),
            messages_vectorization_interval_secs: get("messages_vectorization_interval", "3600")
                .parse()
                .unwrap_or(3600),
            vectorize_wiki: get("vectorize_wiki", "false").parse().unwrap_or(false),
            wiki_vectorization_method: get("wiki_vectorization_method", "local"),
            wiki_vectorization_api_url: opt_str(&get("wiki_vectorization_api_url", "")),
            wiki_vectorization_protocol: get("wiki_vectorization_protocol", "openai"),
            wiki_vectorization_api_key: opt_str(&get("wiki_vectorization_api_key", "")),
            wiki_vectorization_api_model: opt_str(&get("wiki_vectorization_api_model", "")),
            wiki_vectorization_interval_secs: get("wiki_vectorization_interval", "3600")
                .parse()
                .unwrap_or(3600),
        })
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── AgentConfig construction ────────────────────────────────────────────

    #[test]
    fn test_agent_config_default_like_construction() {
        // Test that AgentConfig can be constructed with typical defaults.
        let cfg = AgentConfig {
            llm_api_key: "".to_string(),
            default_provider: "openai".to_string(),
            max_tokens: 32768,
            max_tokens_on_truncation: 16384,
            temperature: 0.7,
            max_iterations_no_plan: 30,
            max_iterations_plan: 120,
            thread_summary_tokens: 2048,
            max_unfinished_subtask_retries: 1,
            provider_max_retries: 3,
            delete_after_days: 30,
            kanban_dispatcher_interval_secs: 15,
            prompt_tool_name: "prompt_generate".to_string(),
            compact_messages_tool_name: "prompt_compact-messages".to_string(),
            char_budget_hard: 200000,
            char_budget_soft: 100000,
            read_keep_last: 3,
            read_excerpt_chars: 2000,
            auto_note_max_chars: 24000,
            auto_note_entry_chars: 3000,
            prompt_log_level: "first".to_string(),
            tool_bg_secs: 30,
            database_url: "postgres://localhost:***@host:5432/db".to_string(),
            database_readonly_url: "postgres://user:***@host:5432/db_ro".to_string(),
            host: "127.0.0.1".to_string(),
            port: 3000,
            platform_max_spawn_retries: 10,
            max_inline_file_kb: 500,
            default_profile: "custom".to_string(),
            vectorize_messages: true,
            messages_vectorization_method: "local".to_string(),
            messages_vectorization_api_url: None,
            messages_vectorization_protocol: "openai".to_string(),
            messages_vectorization_api_key: None,
            messages_vectorization_api_model: None,
            messages_vectorization_interval_secs: 3600,
            vectorize_wiki: false,
            wiki_vectorization_method: "local".to_string(),
            wiki_vectorization_api_url: None,
            wiki_vectorization_protocol: "openai".to_string(),
            wiki_vectorization_api_key: None,
            wiki_vectorization_api_model: None,
            wiki_vectorization_interval_secs: 3600,
        };
        assert_eq!(cfg.default_provider, "openai");
        assert_eq!(cfg.max_tokens, 32768);
        assert_eq!(cfg.max_tokens_on_truncation, 16384);
        assert_eq!(cfg.temperature, 0.7);
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 3000);
        assert_eq!(cfg.default_profile, "custom");
        assert_eq!(cfg.database_url, "postgres://localhost:***@host:5432/db");
        assert_eq!(
            cfg.database_readonly_url,
            "postgres://user:***@host:5432/db_ro"
        );
        assert_eq!(cfg.max_inline_file_kb, 500);
        assert_eq!(cfg.platform_max_spawn_retries, 10);
    }

    #[test]
    fn test_agent_config_complete_field_count() {
        // AgentConfig has 37 fields. This test verifies all are present,
        // by constructing a minimal config and checking all fields are accessible.
        let cfg = AgentConfig {
            llm_api_key: String::new(),
            default_provider: String::new(),
            max_tokens: 0,
            max_tokens_on_truncation: 0,
            temperature: 0.0,
            max_iterations_no_plan: 0,
            max_iterations_plan: 0,
            thread_summary_tokens: 0,
            max_unfinished_subtask_retries: 0,
            provider_max_retries: 0,
            delete_after_days: 0,
            kanban_dispatcher_interval_secs: 0,
            prompt_tool_name: String::new(),
            compact_messages_tool_name: String::new(),
            char_budget_hard: 0,
            char_budget_soft: 0,
            read_keep_last: 0,
            read_excerpt_chars: 0,
            auto_note_max_chars: 0,
            auto_note_entry_chars: 0,
            prompt_log_level: String::new(),
            tool_bg_secs: 0,
            database_url: "postgres://localhost:5432/omniagent".to_string(),
            database_readonly_url: String::new(),
            host: String::new(),
            port: 0,
            platform_max_spawn_retries: 0,
            max_inline_file_kb: 0,
            default_profile: String::new(),
            vectorize_messages: false,
            messages_vectorization_method: String::new(),
            messages_vectorization_api_url: None,
            messages_vectorization_protocol: String::new(),
            messages_vectorization_api_key: None,
            messages_vectorization_api_model: None,
            messages_vectorization_interval_secs: 0,
            vectorize_wiki: false,
            wiki_vectorization_method: String::new(),
            wiki_vectorization_api_url: None,
            wiki_vectorization_protocol: String::new(),
            wiki_vectorization_api_key: None,
            wiki_vectorization_api_model: None,
            wiki_vectorization_interval_secs: 0,
        };
        // Verify a few key fields are accessible
        assert_eq!(cfg.database_url, "postgres://localhost:5432/omniagent");
        assert_eq!(cfg.max_tokens, 0);
        assert_eq!(cfg.temperature, 0.0);
    }

    // ── config_snapshot ─────────────────────────────────────────────────────
    // config_snapshot requires an AgentContext which needs PgPool, LLMClient,
    // AppContext, and PluginManager — all infrastructure-heavy. We skip that
    // test here since it would require real DB connections.

    // ── from_env helper closure ─────────────────────────────────────────────
    // The 'get' closure used inside from_env() is testable in isolation.

    #[test]
    fn test_from_env_get_closure_behavior() {
        // Simulate the get closure logic used in from_env():
        // let get = |key: &str, default: &str| -> String {
        //     settings.get(key).cloned().unwrap_or_else(|| default.to_string())
        // };
        use std::collections::HashMap;

        let mut settings = HashMap::new();
        settings.insert("default_provider".to_string(), "custom".to_string());

        let get = |key: &str, default: &str| -> String {
            settings
                .get(key)
                .cloned()
                .unwrap_or_else(|| default.to_string())
        };

        assert_eq!(get("default_provider", "openai"), "custom");
        assert_eq!(get("nonexistent", "fallback"), "fallback");
        assert_eq!(get("max_tokens", "32768"), "32768");
    }

    #[test]
    fn test_get_closure_numeric_parsing() {
        // Test the numeric parsing pattern used in from_env().
        let parse_or_default = |val: &str, default: u32| -> u32 { val.parse().unwrap_or(default) };

        assert_eq!(parse_or_default("32768", 2048), 32768);
        assert_eq!(parse_or_default("abc", 2048), 2048);
        assert_eq!(parse_or_default("0", 100), 0);
    }

    #[test]
    fn test_get_closure_float_parsing() {
        let parse_or_default = |val: &str, default: f32| -> f32 { val.parse().unwrap_or(default) };

        assert_eq!(parse_or_default("0.7", 0.5), 0.7);
        assert_eq!(parse_or_default("invalid", 0.5), 0.5);
        assert_eq!(parse_or_default("1.0", 0.0), 1.0);
    }
}
