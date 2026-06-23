//! LLM provider abstraction — supports multiple backends with reasoning and caching.
//!
//! Providers are configured via environment variables:
//! - `LLM_PROVIDER` — "opencode-go" (default), "openai", "anthropic"
//! - `LLM_API_KEY` — API key
//! - `LLM_BASE_URL` — Base URL for the API (default for each provider)
//! - `LLM_MODEL` — Model name (e.g. "deepseek-v4-flash")
//! - `LLM_MAX_TOKENS` — Max tokens (default: 8192)
//! - `LLM_TEMPERATURE` — Temperature (default: 0.7)
//!
//! OpenCode Go serves two API surfaces depending on the model:
//! - `chat_completions` — OpenAI-compatible `/v1/chat/completions` (GLM, Kimi, DeepSeek)
//! - `anthropic_messages` — Anthropic-compatible `/v1/messages` (MiniMax, Qwen 3.7)
//!   API mode is auto-detected from the model name.

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;

pub mod provider_config;

// ---------------------------------------------------------------------------
// Provider identification — String-based, extensible via plugin_registry
// ---------------------------------------------------------------------------

/// A provider identifier — stores the plugin name.
///
/// Custom provider names work out of the box; no enum variants needed.
/// Resolution against the plugin_registry happens at config-time via
/// `resolve_provider_config()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub fn new(name: &str) -> Self {
        Self(name.to_string())
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Provider metadata — loaded from plugin manifests at startup
// ---------------------------------------------------------------------------

/// Provider defaults loaded from plugin manifests (plugins/providers/*/plugin.json).
#[derive(Debug, Clone)]
pub struct ProviderMetadata {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub default_base_url: String,
    pub api_mode: String,
}

/// Scan filesystem directories for provider plugin manifests and return a map.
fn scan_provider_manifests(dirs: &[&str]) -> HashMap<String, ProviderMetadata> {
    let mut map = HashMap::new();
    for dir in dirs {
        let base = Path::new(dir);
        if !base.exists() {
            continue;
        }
        let entries = match std::fs::read_dir(base) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let plugin_dir = entry.path();
            if !plugin_dir.is_dir() {
                continue;
            }
            let manifest_path = plugin_dir.join("plugin.json");
            if !manifest_path.exists() {
                continue;
            }
            let content = match std::fs::read_to_string(&manifest_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let manifest: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let plugin_type = manifest.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if plugin_type != "provider" {
                continue;
            }
            let name = manifest.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let default_base_url = manifest
                .get("default_base_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let api_mode = manifest
                .get("api_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("chat_completions")
                .to_string();
            map.insert(name.clone(), ProviderMetadata {
                name,
                default_base_url,
                api_mode,
            });
        }
    }
    map
}

/// Static cache of provider metadata loaded from plugin manifests.
/// Scans development sources first (plugins/providers/), then installed
/// plugins (data/plugins/installed/). Installed plugins override bundled ones.
pub static PROVIDER_METADATA: Lazy<HashMap<String, ProviderMetadata>> = Lazy::new(|| {
    let workspace_dir = std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());
    let data_dir = std::env::var("OMNI_DATA_DIR").unwrap_or_else(|_| "/opt/data".to_string());

    let bundled = format!("{}/plugins/providers", workspace_dir);
    let installed = format!("{}/plugins/installed", data_dir);

    // Bundled first, then installed overrides
    let mut map = scan_provider_manifests(&[&bundled, &installed]);

    // If no providers found, add minimal builtin defaults as last resort
    if map.is_empty() {
        map.insert("opencode-go".to_string(), ProviderMetadata {
            name: "opencode-go".to_string(),
            default_base_url: "https://opencode.ai/zen/go/v1".to_string(),
            api_mode: "dynamic".to_string(),
        });
        map.insert("deepseek".to_string(), ProviderMetadata {
            name: "deepseek".to_string(),
            default_base_url: "https://api.deepseek.com/v1".to_string(),
            api_mode: "chat_completions".to_string(),
        });
    }

    map
});

#[allow(dead_code)]
/// Resolve the default base URL for a provider from the plugin metadata.
pub fn resolve_default_base_url(provider_name: &str) -> String {
    PROVIDER_METADATA
        .get(provider_name)
        .map(|m| m.default_base_url.clone())
        .unwrap_or_default()
}

/// Resolve the API mode for a provider from the plugin metadata.
pub fn resolve_provider_api_mode(provider_name: &str) -> String {
    PROVIDER_METADATA
        .get(provider_name)
        .map(|m| m.api_mode.clone())
        .unwrap_or_else(|| "chat_completions".to_string())
}

// ---------------------------------------------------------------------------
// API mode — determines which endpoint format to use
// ---------------------------------------------------------------------------

/// API surface mode — some providers serve different endpoints per model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ApiMode {
    /// OpenAI-compatible `/chat/completions` (OpenAI SDK format).
    ChatCompletions,
    /// Anthropic Messages API `/messages` (Anthropic SDK format).
    AnthropicMessages,
}

/// Determine the API mode for an OpenCode Go model.
///
/// Mirrors Hermes' `opencode_model_api_mode()` logic:
/// - MiniMax models → `anthropic_messages`
/// - Qwen 3.7 Max → `anthropic_messages`
/// - Everything else (GLM, Kimi, DeepSeek, etc.) → `chat_completions`
pub fn opencode_model_api_mode(model_id: &str) -> ApiMode {
    let normalized = model_id.trim().to_lowercase();
    if normalized.starts_with("minimax-") || normalized.starts_with("qwen3.7-max") {
        ApiMode::AnthropicMessages
    } else {
        ApiMode::ChatCompletions
    }
}

impl ApiMode {
    /// Resolve the API mode for a given provider + model combination.
    /// Provider defaults come from the plugin manifest (PROVIDER_METADATA).
    /// The "dynamic" mode auto-detects based on model name (used by opencode-go).
    pub fn resolve(provider_name: &str, model_id: &str) -> Self {
        let mode = resolve_provider_api_mode(provider_name);
        match mode.as_str() {
            "dynamic" => opencode_model_api_mode(model_id),
            "anthropic_messages" => ApiMode::AnthropicMessages,
            _ => ApiMode::ChatCompletions,
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct LLMConfig {
    pub provider: ProviderId,
    pub api_mode: ApiMode,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    #[expect(dead_code)]
    pub max_tokens: u32,
    #[expect(dead_code)]
    pub temperature: f32,
}

impl LLMConfig {
    /// Build config from environment variables.
    ///
    /// # Panics
    ///
    /// Panics if `LLM_PROVIDER` contains an unrecognised value.
    pub fn from_env() -> Self {
        let provider_name = std::env::var("LLM_PROVIDER")
            .unwrap_or_else(|_| "opencode-go".to_string());

        let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string());

        let provider = ProviderId::new(&provider_name);
        let api_mode = ApiMode::resolve(&provider_name, &model);

        let base_url = std::env::var("LLM_BASE_URL").unwrap_or_else(|_| match provider_name.as_str() {
            "opencode-go" => "https://opencode.ai/zen/go/v1".to_string(),
            "openai" => "https://api.openai.com/v1".to_string(),
            "anthropic" => "https://api.anthropic.com/v1".to_string(),
            "deepseek" => "https://api.deepseek.com/v1".to_string(),
            _ => String::new(),
        });

        let api_key = match provider_name.as_str() {
            "deepseek" => {
                // Backward compat: DEEPSEEK_API_KEY is the legacy env var
                // for the deepseek provider, registered via provider plugin.
                std::env::var("DEEPSEEK_API_KEY")
                    .or_else(|_| std::env::var("LLM_API_KEY"))
                    .unwrap_or_default()
            }
            _ => std::env::var("LLM_API_KEY").unwrap_or_default(),
        };

        Self {
            provider,
            api_mode,
            api_key,
            base_url,
            model,
            max_tokens: std::env::var("LLM_MAX_TOKENS")
                .unwrap_or_else(|_| "8192".to_string())
                .parse()
                .unwrap_or(8192),
            temperature: std::env::var("LLM_TEMPERATURE")
                .unwrap_or_else(|_| "0.7".to_string())
                .parse()
                .unwrap_or(0.7),
        }
    }
}

// ---------------------------------------------------------------------------
// Chat / Completion types
// ---------------------------------------------------------------------------

/// A single message in a chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// Tool call ID for tool result messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls in assistant messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallData>>,
    /// Name field for tool result messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: &str) -> Self {
        Self {
            role: "system".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
            name: None,
        }
    }

    pub fn user(content: &str) -> Self {
        Self {
            role: "user".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
            name: None,
        }
    }

    pub fn assistant(content: &str) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
            name: None,
        }
    }

    pub fn tool_result(tool_call_id: &str, name: &str, content: &str) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.to_string(),
            tool_call_id: Some(tool_call_id.to_string()),
            tool_calls: None,
            name: Some(name.to_string()),
        }
    }
}

/// Request payload for LLM completion.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    pub temperature: f32,
    pub stream: bool,
    /// Optional tool definitions (OpenAI function calling format).
    pub tools: Option<Vec<serde_json::Value>>,
}

/// Token usage statistics returned by the provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default)]
    #[serde(alias = "prompt_cache_hit_tokens")]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallData {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

/// Response from an LLM completion call.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: String,
    /// Reasoning/thinking content, if provided by the model.
    pub reasoning: Option<String>,
    /// Tool calls requested by the model, if any.
    pub tool_calls: Vec<ToolCallData>,
    pub usage: Option<Usage>,
    /// Wall-clock time of the LLM call in milliseconds.
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// OpenAI-compatible response shapes (for opencode-go and OpenAI)
// ---------------------------------------------------------------------------

/// Generic OpenAI-compatible chat completion response that handles both
/// streaming-chunk and non-streaming formats.
#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    #[allow(dead_code)]
    id: Option<String>,
    /// `"chat.completion"` (non-streaming) or `"chat.completion.chunk"` (streaming).
    object: Option<String>,
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    /// Present in non-streaming responses.
    #[serde(default)]
    message: Option<OpenAiMessage>,
    /// Present in streaming chunks.
    #[serde(default)]
    delta: Option<OpenAiDelta>,
    #[allow(dead_code)]
    #[serde(default)]
    finish_reason: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    index: u32,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessage {
    #[serde(default)]
    content: Option<String>,
    /// Extension field used by opencode-go / DeepSeek for reasoning text.
    #[serde(default)]
    reasoning_content: Option<String>,
    /// Tool calls requested by the model (OpenAI function calling format).
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallData>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiDelta {
    #[serde(default)]
    content: Option<String>,
    /// Extension field used by opencode-go / DeepSeek for reasoning text.
    #[serde(default)]
    reasoning_content: Option<String>,
}

// ---------------------------------------------------------------------------
// Anthropic response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
    content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    #[expect(dead_code)]
    signature: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

// ---------------------------------------------------------------------------
// LLM Client
// ---------------------------------------------------------------------------

/// An HTTP client that talks to a configurable LLM provider.
pub struct LLMClient {
    pub config: LLMConfig,
    client: reqwest::Client,
}

impl LLMClient {
    /// Create a new client from the given configuration.
    pub fn new(config: LLMConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300)) // 5 minutes
            .build()
            .expect("Failed to build reqwest Client");
        Self { config, client }
    }

    /// Send a completion request and return the response.
    ///
    /// Dispatches to the appropriate provider-specific implementation based on
    /// `self.config.provider` and `self.config.api_mode`.
    pub async fn completion(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let start = std::time::Instant::now();
        let mut resp = match self.config.api_mode {
            ApiMode::ChatCompletions => self.completion_openai(request).await,
            ApiMode::AnthropicMessages => self.completion_anthropic(request).await,
        }?;
        resp.duration_ms = start.elapsed().as_millis() as u64;
        Ok(resp)
    }

    // -----------------------------------------------------------------------
    // OpenAI-compatible (covers opencode-go + vanilla OpenAI)
    // -----------------------------------------------------------------------

    async fn completion_openai(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        // Build the JSON body — the opencode-go provider gets an extra
        // `include_reasoning: true` flag.
        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": request.messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": request.stream,
        });

        if matches!(self.config.provider.0.as_str(), "opencode-go" | "deepseek") {
            body["include_reasoning"] = serde_json::Value::Bool(true);
        }

        // Include tools if provided
        if let Some(ref tools) = request.tools {
            if !tools.is_empty() {
                body["tools"] = serde_json::Value::Array(tools.clone());
            }
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send OpenAI-compatible completion request")?;

        let status = resp.status();
        let resp_text = resp.text().await.context("Failed to read response body")?;

        if !status.is_success() {
            anyhow::bail!("OpenAI-compatible API returned {status}: {resp_text}",);
        }

        let parsed: OpenAiResponse = serde_json::from_str(&resp_text)
            .with_context(|| format!("Failed to parse OpenAI response: {resp_text}"))?;

        Self::extract_from_openai_response(parsed)
    }

    fn extract_from_openai_response(response: OpenAiResponse) -> Result<CompletionResponse> {
        match response.object.as_deref() {
            Some("chat.completion.chunk") => Self::extract_openai_streaming(response),
            _ => Self::extract_openai_nonstreaming(response), // includes `chat.completion` and unknown
        }
    }

    fn extract_openai_nonstreaming(response: OpenAiResponse) -> Result<CompletionResponse> {
        let choice = response
            .choices
            .into_iter()
            .next()
            .context("OpenAI response has no choices")?;

        let content = choice
            .message
            .as_ref()
            .and_then(|m| m.content.clone())
            .or_else(|| choice.delta.as_ref().and_then(|d| d.content.clone()))
            .unwrap_or_default();

        let reasoning = choice
            .message
            .as_ref()
            .and_then(|m| m.reasoning_content.clone())
            .or_else(|| {
                choice
                    .delta
                    .as_ref()
                    .and_then(|d| d.reasoning_content.clone())
            });

        let tool_calls = choice
            .message
            .as_ref()
            .and_then(|m| m.tool_calls.clone())
            .unwrap_or_default();

        Ok(CompletionResponse {
            content,
            reasoning,
            tool_calls,
            usage: response.usage,
            duration_ms: 0,
        })
    }

    fn extract_openai_streaming(response: OpenAiResponse) -> Result<CompletionResponse> {
        // For streaming chunks, concatenate all deltas.
        let mut content = String::new();
        let mut reasoning: Option<String> = None;

        for choice in &response.choices {
            if let Some(ref delta) = choice.delta {
                if let Some(ref c) = delta.content {
                    content.push_str(c);
                }
                if let Some(ref r) = delta.reasoning_content {
                    reasoning.get_or_insert_with(String::new).push_str(r);
                }
            }
        }

        Ok(CompletionResponse {
            content,
            reasoning,
            tool_calls: vec![],
            usage: response.usage,
            duration_ms: 0,
        })
    }

    // -----------------------------------------------------------------------
    // Anthropic Messages API
    // -----------------------------------------------------------------------

    async fn completion_anthropic(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let url = format!("{}/messages", self.config.base_url.trim_end_matches('/'));

        // Convert our ChatMessages to Anthropic's format.
        // Anthropic uses "system" as a top-level parameter, not a message role.
        let mut system: Option<String> = None;
        let mut messages: Vec<serde_json::Value> = Vec::new();

        for msg in &request.messages {
            if msg.role == "system" {
                system = Some(msg.content.clone());
            } else {
                messages.push(serde_json::json!({
                    "role": msg.role,
                    "content": msg.content,
                }));
            }
        }

        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
        });

        if let Some(s) = system {
            body["system"] = serde_json::Value::String(s);
        }

        // Enable thinking if we want to capture reasoning (only for Anthropic provider)
        if self.config.provider.0 == "anthropic" {
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": request.max_tokens.min(32000),
            });
        }

        // Build request — auth header differs by provider
        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json");

        match self.config.provider.0.as_str() {
            "anthropic" => {
                req = req
                    .header("x-api-key", &self.config.api_key)
                    .header("anthropic-version", "2023-06-01");
            }
            // OpenCode Go / OpenAI in Anthropic mode use Bearer token
            _ => {
                req = req.header("Authorization", format!("Bearer {}", self.config.api_key));
            }
        }

        let resp = req
            .json(&body)
            .send()
            .await
            .context("Failed to send Anthropic completion request")?;

        let status = resp.status();
        let resp_text = resp
            .text()
            .await
            .context("Failed to read Anthropic response body")?;

        if !status.is_success() {
            anyhow::bail!("Anthropic API returned {status}: {resp_text}");
        }

        let parsed: AnthropicResponse = serde_json::from_str(&resp_text)
            .with_context(|| format!("Failed to parse Anthropic response: {resp_text}"))?;

        // Extract text and thinking from content blocks
        let mut content = String::new();
        let mut reasoning: Option<String> = None;

        for block in parsed.content {
            match block.block_type.as_str() {
                "text" => {
                    if let Some(text) = block.text {
                        content.push_str(&text);
                    }
                }
                "thinking" => {
                    if let Some(think) = block.thinking {
                        reasoning = Some(reasoning.unwrap_or_default() + &think);
                    }
                }
                _ => {}
            }
        }

        // Map Anthropic usage to our common Usage struct
        let usage = parsed.usage.map(|u| Usage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            cached_tokens: u.cache_read_input_tokens.or(u.cache_creation_input_tokens),
            reasoning_tokens: None, // Anthropic doesn't separate reasoning tokens in usage
        });

        Ok(CompletionResponse {
            content,
            reasoning,
            tool_calls: vec![],
            usage,
            duration_ms: 0,
        })
    }
}
