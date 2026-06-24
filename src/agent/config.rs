use anyhow::{Context, Result};
use crate::llm::resolve_llm_api_key;
use crate::mcp::{AppContext, McpRegistry};
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub llm_api_key: String,
    pub llm_model: String,
    pub llm_provider: String,
    pub llm_base_url: String,
    pub max_tokens: u32,
    pub temperature: f32,
    #[expect(dead_code)]
    pub summarize_after_days: u32,
    /// Max iterations for threads with no planning mode (complexity-based).
    pub max_iterations_no_plan: u32,
    /// Max iterations for threads with simple planning (auto_plan).
    pub max_iterations_simple_plan: u32,
    /// Max iterations for threads with complex planning + subtasks (auto_subtasks).
    pub max_iterations_complex_plan: u32,
    /// Number of threads per half-window for summary generation.
    /// A summary is generated every 2*summary_window completed threads.
    pub summary_window: u32,
    /// Max tokens for the channel-level summary generation LLM call.
    pub channel_summary_tokens: u32,
    /// Max tokens for the per-thread end-of-execution summary LLM call.
    pub thread_summary_tokens: u32,
    /// Days before old messages and summaries are deleted.
    pub delete_after_days: u32,
    /// Max output tokens for the planning LLM call.
    pub prompt_plan_max_tokens: u32,

    // Context management / token explosion prevention
    /// Soft char budget for the prompt. When exceeded, condense every STATE_BLOCK_UPDATE_INTERVAL turns.
    pub prompt_char_budget_soft: usize,
    /// Hard char budget for the prompt. When exceeded, condense before ANY LLM call to bring below soft.
    #[expect(dead_code)]
    pub prompt_char_budget_hard: usize,
    /// Max chars for old messages after condensation (metadata block stays).
    pub old_message_char_budget: usize,
    /// How often (in iterations) to condense when soft budget is exceeded.
    pub state_block_update_interval: u32,
    /// How many full assistant→tool cycles to keep verbatim during condensation.
    pub condense_keep_turns: usize,
    /// Token budget — soft threshold for condensation (uses tiktoken for accurate counting).
    pub prompt_token_budget_soft: usize,
    /// Token budget — hard threshold, condense before any LLM call (uses tiktoken).
    pub prompt_token_budget_hard: usize,
    /// tiktoken encoding/model name ("gpt-4", "cl100k_base", "o200k_base").
    pub tokenizer_encoding: String,
    /// Multiplier to account for provider tokenizer mismatch with tiktoken.
    pub prompt_token_safety_factor: f64,

    // Infrastructure config (merged from former config::Config)
    pub database_url: String,
    pub database_readonly_url: String,
    pub qdrant_url: String,
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
    pub config: AgentConfig,
    pub mcp: McpRegistry,
    pub ctx: AppContext,
}

impl AgentConfig {
    /// Load agent configuration from environment variables.
    ///
    /// # Env vars
    /// - `LLM_API_KEY` — API key for the LLM provider
    /// - `LLM_MODEL` — Model name (default: "gpt-4")
    /// - `LLM_PROVIDER` — Provider name (default: "openai")
    /// - `LLM_BASE_URL` — Base URL for the API (optional per-provider default)
    /// - `MAX_TOKENS` — Max tokens per response (default: 4096)
    /// - `TEMPERATURE` — Sampling temperature (default: 0.7)
    /// - `SUMMARIZE_AFTER_DAYS` — Days before auto-summarization (default: 7)
    /// - `MAX_ITERATIONS` — Max agent turns per thread before skipping (default: 60)
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            llm_api_key: {
                let provider = std::env::var("LLM_PROVIDER").unwrap_or_default();
                let provider_key = if provider.is_empty() {
                    String::new()
                } else {
                    format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"))
                };
                resolve_llm_api_key(Some(&std::env::var(&provider_key).unwrap_or_default()))
            },
            llm_model: std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4".to_string()),
            llm_provider: std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "openai".to_string()),
            llm_base_url: std::env::var("LLM_BASE_URL").unwrap_or_default(),
            max_tokens: std::env::var("MAX_TOKENS")
                .unwrap_or_else(|_| "4096".to_string())
                .parse()
                .unwrap_or(4096),
            temperature: std::env::var("TEMPERATURE")
                .unwrap_or_else(|_| "0.7".to_string())
                .parse()
                .unwrap_or(0.7),
            summarize_after_days: std::env::var("SUMMARIZE_AFTER_DAYS")
                .unwrap_or_else(|_| "7".to_string())
                .parse()
                .unwrap_or(7),
            max_iterations_no_plan: std::env::var("MAX_ITERATIONS_NO_PLAN")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .unwrap_or(30),
            max_iterations_simple_plan: std::env::var("MAX_ITERATIONS_SIMPLE_PLAN")
                .unwrap_or_else(|_| "120".to_string())
                .parse()
                .unwrap_or(120),
            max_iterations_complex_plan: std::env::var("MAX_ITERATIONS_COMPLEX_PLAN")
                .unwrap_or_else(|_| "600".to_string())
                .parse()
                .unwrap_or(600),
            summary_window: std::env::var("SUMMARY_WINDOW")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .unwrap_or(10),
            channel_summary_tokens: std::env::var("CHANNEL_SUMMARY_TOKENS")
                .unwrap_or_else(|_| "4096".to_string())
                .parse()
                .unwrap_or(4096),
            thread_summary_tokens: std::env::var("THREAD_SUMMARY_TOKENS")
                .unwrap_or_else(|_| "2048".to_string())
                .parse()
                .unwrap_or(2048),
            delete_after_days: std::env::var("DELETE_AFTER_DAYS")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .unwrap_or(30),
            prompt_plan_max_tokens: std::env::var("PROMPT_PLAN_MAX_TOKENS")
                .unwrap_or_else(|_| "2048".to_string())
                .parse()
                .unwrap_or(2048),

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
                .unwrap_or_else(|_| "2".to_string())
                .parse()
                .unwrap_or(2),
            
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

            // Infrastructure config (merged from former config::Config)
            database_url: std::env::var("DATABASE_URL")
                .context("DATABASE_URL must be set")?,
            database_readonly_url: std::env::var("DATABASE_READONLY_URL")
                .unwrap_or_else(|_| std::env::var("DATABASE_URL")
                    .unwrap_or_else(|_| "postgres://localhost:5432/omniagent".to_string())),
            qdrant_url: std::env::var("QDRANT_URL")
                .unwrap_or_else(|_| "http://localhost:6333".to_string()),
            host: std::env::var("HOST")
                .unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: std::env::var("PORT")
                .unwrap_or_else(|_| "8080".to_string())
                .parse()
                .context("PORT must be a valid number")?,
            vectorize_messages: std::env::var("VECTORIZE_MESSAGES")
                .unwrap_or_else(|_| "false".to_string())
                .parse::<bool>()
                .unwrap_or(false),
            vectorize_wiki: std::env::var("VECTORIZE_WIKI")
                .unwrap_or_else(|_| "false".to_string())
                .parse::<bool>()
                .unwrap_or(false),
            messages_vectorization_method:
                std::env::var("MESSAGES_VECTORIZATION_METHOD").unwrap_or_else(|_| "local".to_string()),
            messages_vectorization_api_url: std::env::var("MESSAGES_VECTORIZATION_API_URL").ok(),
            messages_vectorization_interval_secs: std::env::var("MESSAGES_VECTORIZATION_INTERVAL")
                .unwrap_or_else(|_| "3600".to_string())
                .parse()
                .context("MESSAGES_VECTORIZATION_INTERVAL must be a valid number")?,
            messages_vectorization_protocol: std::env::var("MESSAGES_VECTORIZATION_PROTOCOL")
                .unwrap_or_else(|_| "openai".to_string()),
            messages_vectorization_api_key: std::env::var("MESSAGES_VECTORIZATION_API_KEY").ok(),
            messages_vectorization_api_model:
                std::env::var("MESSAGES_VECTORIZATION_API_MODEL").ok(),
            wiki_vectorization_method:
                std::env::var("WIKI_VECTORIZATION_METHOD").unwrap_or_else(|_| "local".to_string()),
            wiki_vectorization_api_url: std::env::var("WIKI_VECTORIZATION_API_URL").ok(),
            wiki_vectorization_interval_secs: std::env::var("WIKI_VECTORIZATION_INTERVAL")
                .unwrap_or_else(|_| "3600".to_string())
                .parse()
                .context("WIKI_VECTORIZATION_INTERVAL must be a valid number")?,
            wiki_vectorization_protocol:
                std::env::var("WIKI_VECTORIZATION_PROTOCOL").unwrap_or_else(|_| "openai".to_string()),
            wiki_vectorization_api_key: std::env::var("WIKI_VECTORIZATION_API_KEY").ok(),
            wiki_vectorization_api_model: std::env::var("WIKI_VECTORIZATION_API_MODEL").ok(),
        })
    }
}
