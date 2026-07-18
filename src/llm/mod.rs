//! LLM provider abstraction: supports multiple backends with reasoning and caching.
//!
//! Providers are configured via plugin config (providers.yml with $env: references).
//! The only hardcoded env var names are the infrastructure defaults set by the
//! deployment repo: `OMNI_DIR`, `WORKSPACE_DIR`, and `LLM_PROVIDER`.
//!
//! The API key comes from the provider's plugin config (providers.yml with $env:
//! references). The startup fallback is empty: no hardcoded env var names.
//!
//! OpenCode Go serves two API surfaces depending on the model:
//! - `chat_completions`: OpenAI-compatible `/v1/chat/completions` (GLM, Kimi, DeepSeek)
//! - `anthropic_messages`: Anthropic-compatible `/v1/messages` (MiniMax, Qwen 3.7)
//!   API mode is auto-detected from the model name.

use crate::err_msg;
use crate::error::{AppResult, Error, ErrorContext};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::warn;

// ---------------------------------------------------------------------------
// Provider identification: String-based, extensible via plugin_registry
// ---------------------------------------------------------------------------

/// A provider identifier: stores the plugin name.
///
/// Custom provider names work out of the box; no enum variants needed.
/// Resolution against the plugin_registry happens at config-time via
/// `super::plugin` functions.
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
// Provider metadata: loaded from plugin manifests at startup
// ---------------------------------------------------------------------------

/// Provider defaults loaded from plugin manifests (plugins/providers/*/plugin.json).
#[derive(Debug, Clone)]
pub struct ProviderMetadata {
    #[allow(dead_code)]
    pub name: String,
    pub default_base_url: String,
    pub api_mode: String,
    /// Per-model overrides: API mode → list of model prefixes.
    /// The first matching prefix wins when resolving for a specific model.
    pub api_modes: HashMap<String, Vec<String>>,
    pub default_model: String,
}

/// Extract default_model from a provider plugin manifest's config_schema.
/// Looks for a field with key="default_model" and reads its "default" value.
fn extract_default_model(manifest: &serde_json::Value) -> String {
    manifest
        .get("config_schema")
        .and_then(|schema| schema.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|field| field.get("key").and_then(|k| k.as_str()) == Some("default_model"))
        })
        .and_then(|field| field.get("default").and_then(|d| d.as_str()))
        .unwrap_or("")
        .to_string()
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
            let name = manifest
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
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
            let api_modes: HashMap<String, Vec<String>> = manifest
                .get("api_modes")
                .and_then(|v| {
                    v.as_object().map(|obj| {
                        obj.iter()
                            .filter_map(|(key, val)| {
                                val.as_array().map(|arr| {
                                    (
                                        key.clone(),
                                        arr.iter()
                                            .filter_map(|v| v.as_str().map(String::from))
                                            .collect(),
                                    )
                                })
                            })
                            .collect()
                    })
                })
                .unwrap_or_default();
            let default_model = extract_default_model(&manifest);
            map.insert(
                name.clone(),
                ProviderMetadata {
                    name,
                    default_base_url,
                    api_mode,
                    api_modes,
                    default_model,
                },
            );
        }
    }
    map
}

/// Static cache of provider metadata loaded from plugin manifests.
/// Scans development sources first (plugins/providers/), then installed
/// plugins (data/plugins/installed/). Installed plugins override bundled ones.
pub static PROVIDER_METADATA: Lazy<HashMap<String, ProviderMetadata>> = Lazy::new(|| {
    let data_dir = match std::env::var("OMNI_DIR") {
        Ok(d) => d,
        Err(_) => {
            tracing::warn!("OMNI_DIR not set, provider metadata will be empty");
            return HashMap::new();
        }
    };

    let bundled = format!("{}/plugins/providers", data_dir);
    let installed = format!("{}/plugins/installed", data_dir);

    // Bundled first, then installed overrides
    // If no providers are found, the metadata stays empty and callers
    // handle it gracefully (resolve_default_model returns None).
    let map = scan_provider_manifests(&[&bundled, &installed]);

    map
});

/// Resolve the default base URL for a provider from the plugin metadata.
pub fn resolve_default_base_url(provider_name: &str) -> String {
    PROVIDER_METADATA
        .get(provider_name)
        .map(|m| m.default_base_url.clone())
        .unwrap_or_default()
}

/// Resolve the default model for a provider from the plugin metadata.
/// Returns None if no default is found.
pub fn resolve_default_model(provider_name: &str) -> Option<String> {
    PROVIDER_METADATA.get(provider_name).and_then(|m| {
        if m.default_model.is_empty() {
            None
        } else {
            Some(m.default_model.clone())
        }
    })
}

/// Resolve the API mode for a provider from the plugin metadata.
pub fn resolve_provider_api_mode(provider_name: &str) -> String {
    PROVIDER_METADATA
        .get(provider_name)
        .map(|m| m.api_mode.clone())
        .unwrap_or_else(|| "chat_completions".to_string())
}

// ---------------------------------------------------------------------------
// API mode: determines which endpoint format to use
// ---------------------------------------------------------------------------

/// API surface mode: some providers serve different endpoints per model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ApiMode {
    /// OpenAI-compatible `/chat/completions` (OpenAI SDK format).
    ChatCompletions,
    /// Anthropic Messages API `/messages` (Anthropic SDK format).
    AnthropicMessages,
}

/// Match a model against a provider's per-model API mode overrides.
/// Checks the provider's `api_modes` map (API mode → list of wildcard patterns).
/// Wildcards (`*`) match any sequence of characters. The first matching pattern wins.
/// Falls back to the provider's default `api_mode`.
fn match_model_api_mode(provider_name: &str, model_id: &str) -> Option<ApiMode> {
    let normalized = model_id.trim().to_lowercase();
    let metadata = PROVIDER_METADATA.get(provider_name)?;
    for (mode, patterns) in &metadata.api_modes {
        for pattern in patterns {
            let pattern_lower = pattern.to_lowercase();
            // Convert wildcard pattern to regex: escape all chars, then unescape `\*` → `.*`
            let escaped = regex::escape(&pattern_lower);
            let regex_str = escaped.replace(r"\*", ".*");
            if let Ok(re) = regex::Regex::new(&format!("^{}$", regex_str)) {
                if re.is_match(&normalized) {
                    return match mode.as_str() {
                        "anthropic_messages" => Some(ApiMode::AnthropicMessages),
                        _ => Some(ApiMode::ChatCompletions),
                    };
                }
            }
        }
    }
    None
}

impl ApiMode {
    /// Resolve the API mode for a given provider + model combination.
    /// Provider defaults come from the plugin manifest (PROVIDER_METADATA).
    /// If the provider has `api_modes` overrides, the model is checked against
    /// each prefix. The first match wins, otherwise the default `api_mode` is used.
    pub fn resolve(provider_name: &str, model_id: &str) -> Self {
        // First check per-model overrides
        if let Some(mode) = match_model_api_mode(provider_name, model_id) {
            return mode;
        }
        // Fall back to the provider's default api_mode
        let mode = resolve_provider_api_mode(provider_name);
        match mode.as_str() {
            "anthropic_messages" => ApiMode::AnthropicMessages,
            _ => ApiMode::ChatCompletions,
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Resolve LLM API key from a given string value.
/// Returns the value if non-empty, or an error if empty/not set.
/// Callers should look up api_key from the provider's resolved plugin config.
pub fn resolve_llm_api_key(provider_key: Option<&str>) -> AppResult<String> {
    provider_key
        .map(|k| k.to_string())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| Error::Message(
            "LLM provider key not set. Set the api_key in the provider's plugin config (providers.yml).".to_string()
        ))
}

/// Configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct LLMConfig {
    pub provider: ProviderId,
    pub api_mode: ApiMode,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    #[allow(dead_code)]
    pub max_tokens: u32,
    #[allow(dead_code)]
    pub temperature: f32,
}

impl LLMConfig {
    /// Build config from environment variables.
    ///
    /// Provider-specific config (api_key) comes from plugin config, not hardcoded
    /// env var names. No generic fallback env var is used.
    ///
    /// # Panics
    ///
    /// Panics if `LLM_PROVIDER` contains an unrecognised value.
    pub fn from_env() -> Self {
        let provider_name = crate::agent::config::get_global()
            .map(|g| g.read().unwrap().default_provider.clone())
            .unwrap_or_default(); // Empty string → provider must be configured

        let provider = ProviderId::new(&provider_name);
        let base_url = resolve_default_base_url(&provider_name);
        let default_model = resolve_default_model(&provider_name)
            .unwrap_or_default(); // Empty string → model must be configured
        let model = default_model;

        let api_mode = ApiMode::resolve(&provider_name, &model);

        // No generic API key fallback: provider api_key comes from plugin config
        // (providers.yml with $env: references), not from hardcoded env var names.
        let api_key = String::new();

        Self {
            provider,
            api_mode,
            api_key,
            base_url,
            model,
            max_tokens: 8192,
            temperature: 0.7,
        }
    }
}
// ---------------------------------------------------------------------------
// Per-provider throttling: limits concurrent API requests per provider
// ---------------------------------------------------------------------------

/// Per-provider concurrency throttler using semaphores.
///
/// Limits how many concurrent LLM API requests can be in-flight for a
/// given provider name (e.g. "deepseek", "anthropic", "openai").
///
/// Pre-populated from [`PROVIDER_METADATA`] at construction time so that
/// every known provider gets its own semaphore. Unknown providers fall
/// back to no throttling (permit is acquired but immediately released).
#[derive(Clone)]
pub struct ProviderThrottle {
    inner: Arc<HashMap<String, Arc<Semaphore>>>,
    max_permits: usize,
}

impl ProviderThrottle {
    /// Default maximum concurrent requests per provider.
    pub const DEFAULT_MAX_CONCURRENT: usize = 5;

    /// Create a new throttle with the default limit (5) per provider.
    pub fn new() -> Self {
        Self::with_max_permits(Self::DEFAULT_MAX_CONCURRENT)
    }

    /// Create a new throttle with a custom max concurrent limit per provider.
    pub fn with_max_permits(max: usize) -> Self {
        let mut map = HashMap::new();

        // Pre-populate from known provider metadata
        for name in PROVIDER_METADATA.keys() {
            map.entry(name.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(max)));
        }

        Self {
            inner: Arc::new(map),
            max_permits: max,
        }
    }

    /// Acquire a permit for the given provider, waiting if necessary.
    ///
    /// Returns `None` if the provider is unknown (no throttling applied).
    /// The returned permit is held for the lifetime of the returned guard;
    /// when dropped, the semaphore slot is released.
    pub async fn acquire(&self, provider: &str) -> Option<tokio::sync::SemaphorePermit<'_>> {
        let sem = self.inner.get(provider)?;
        sem.acquire().await.ok()
    }

    /// Returns the configured max permits per provider.
    #[allow(dead_code)]
    pub fn max_permits(&self) -> usize {
        self.max_permits
    }

    /// Returns the number of available permits for a given provider.
    #[allow(dead_code)]
    pub fn available_permits(&self, provider: &str) -> Option<u32> {
        self.inner
            .get(provider)
            .map(|s| s.available_permits() as u32)
    }
}

impl Default for ProviderThrottle {
    fn default() -> Self {
        Self::new()
    }
}

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
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    _index: u32,
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
    /// Refusal message: some providers return this instead of content
    /// when the model refuses to respond (e.g., content filter).
    #[serde(default)]
    refusal: Option<String>,
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
    #[allow(dead_code)]
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
    /// Per-provider concurrency throttle (limits concurrent API requests).
    throttle: ProviderThrottle,
}

impl LLMClient {
    /// Create a new client from the given configuration.
    pub fn new(config: LLMConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300)) // 5 minutes
            .build()
            .expect("Failed to build reqwest Client");
        Self {
            config,
            client,
            throttle: ProviderThrottle::new(),
        }
    }

    /// Create a new client with a custom per-provider throttle.
    pub fn new_with_throttle(config: LLMConfig, throttle: ProviderThrottle) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300)) // 5 minutes
            .build()
            .expect("Failed to build reqwest Client");
        Self {
            config,
            client,
            throttle,
        }
    }

    /// Send a completion request and return the response.
    ///
    /// Dispatches to the appropriate provider-specific implementation based on
    /// `self.config.provider` and `self.config.api_mode`.
    ///
    /// Before making the API call, a per-provider throttle permit is acquired
    /// to limit concurrent requests to the same provider.
    pub async fn completion(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let start = std::time::Instant::now();

        // Check if this provider is an external subprocess provider
        let provider_name = &self.config.provider.0;
        // Try external completion: clone Arc first, drop registry guard, then call complete
        let external_result = {
            let client_opt = {
                let registry = crate::provider::registry::PROVIDER_REGISTRY.read().unwrap();
                registry.get_cloned(provider_name)
            };
            // Registry guard is dropped here: we have an independent Arc<ExternalProviderClient>

            client_opt.map(|client| {
                let messages: Vec<serde_json::Value> = request.messages.iter()
                    .map(|m| serde_json::json!({
                        "role": m.role,
                        "content": m.content,
                    }))
                    .collect();

                let params = crate::provider::external::CompleteParams {
                    model: self.config.model.clone(),
                    messages,
                    max_tokens: request.max_tokens,
                    temperature: request.temperature,
                    stream: request.stream,
                    tools: request.tools.clone(),
                };

                async move {
                    match client.complete(&params).await {
                        Ok(result) => {
                            Ok(CompletionResponse {
                                content: result.content,
                                reasoning: result.reasoning,
                                tool_calls: result.tool_calls.iter()
                                    .filter_map(|tc| serde_json::from_value(tc.clone()).ok())
                                    .collect(),
                                usage: result.usage.map(|u| Usage {
                                    prompt_tokens: u.prompt_tokens,
                                    completion_tokens: u.completion_tokens,
                                    cached_tokens: u.cached_tokens,
                                    reasoning_tokens: u.reasoning_tokens,
                                }),
                                duration_ms: start.elapsed().as_millis() as u64,
                            })
                        }
                        Err(e) => {
                            tracing::warn!(
                                "External provider '{}' completion failed, falling back to HTTP: {}",
                                provider_name, e
                            );
                            Err(e)
                        }
                    }
                }
            })
        };
        if let Some(fut) = external_result {
            match fut.await {
                Ok(resp) => return Ok(resp),
                Err(_) => {} // fall through to HTTP
            }
        }

        // Acquire a per-provider throttle permit before making the request.
        let _permit = self.throttle.acquire(provider_name).await;

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

    async fn completion_openai(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        // Build the JSON body: the opencode-go provider gets an extra
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
            .ctx("Failed to send OpenAI-compatible completion request")?;

        let status = resp.status();
        let resp_text = resp.text().await.ctx("Failed to read response body")?;

        if !status.is_success() {
            err_msg!("OpenAI-compatible API returned {status}: {resp_text}");
        }

        let parsed: OpenAiResponse = serde_json::from_str(&resp_text)
            .ctx(format!("Failed to parse OpenAI response: {resp_text}"))?;

        Self::extract_from_openai_response(parsed)
    }

    fn extract_from_openai_response(response: OpenAiResponse) -> AppResult<CompletionResponse> {
        match response.object.as_deref() {
            Some("chat.completion.chunk") => Self::extract_openai_streaming(response),
            _ => Self::extract_openai_nonstreaming(response), // includes `chat.completion` and unknown
        }
    }

    fn extract_openai_nonstreaming(response: OpenAiResponse) -> AppResult<CompletionResponse> {
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| Error::Message("OpenAI response has no choices".to_string()))?;

        let finish_reason = choice.finish_reason.clone().unwrap_or_default();

        let content = choice
            .message
            .as_ref()
            .and_then(|m| m.content.clone())
            .or_else(|| choice.delta.as_ref().and_then(|d| d.content.clone()))
            .unwrap_or_default();

        let refusal = choice.message.as_ref().and_then(|m| m.refusal.clone());

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

        // Diagnostic: if we got no content/reasoning/tools but the API reports
        // completion tokens, or there's a refusal field, log it to understand
        // what the provider returned.
        if content.is_empty() && tool_calls.is_empty() && reasoning.is_none() {
            let has_refusal = refusal.as_ref().map(|r| !r.is_empty()).unwrap_or(false);
            let prompt_tokens = response
                .usage
                .as_ref()
                .map(|u| u.prompt_tokens)
                .unwrap_or(0);
            let completion_tokens = response
                .usage
                .as_ref()
                .map(|u| u.completion_tokens)
                .unwrap_or(0);
            if completion_tokens > 0 || has_refusal {
                warn!(
                    "[llm] Response has no content/reasoning/tools but has completion_tokens={}, refusal={:?}, finish_reason={}, prompt_tokens={}",
                    completion_tokens, refusal, finish_reason, prompt_tokens,
                );
            }
        }

        // If a refusal was returned, surface it as content so the user sees the reason
        // instead of an empty response error.
        let content = if content.is_empty() && tool_calls.is_empty() {
            if let Some(ref r) = refusal {
                if !r.is_empty() {
                    format!("[Model Refusal] {}", r)
                } else {
                    content
                }
            } else {
                content
            }
        } else {
            content
        };

        Ok(CompletionResponse {
            content,
            reasoning,
            tool_calls,
            usage: response.usage,
            duration_ms: 0,
        })
    }

    fn extract_openai_streaming(response: OpenAiResponse) -> AppResult<CompletionResponse> {
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

    async fn completion_anthropic(
        &self,
        request: CompletionRequest,
    ) -> AppResult<CompletionResponse> {
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

        // Build request: auth header differs by provider
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
            .ctx("Failed to send Anthropic completion request")?;

        let status = resp.status();
        let resp_text = resp
            .text()
            .await
            .ctx("Failed to read Anthropic response body")?;

        if !status.is_success() {
            err_msg!("Anthropic API returned {status}: {resp_text}");
        }

        let parsed: AnthropicResponse = serde_json::from_str(&resp_text)
            .ctx(format!("Failed to parse Anthropic response: {resp_text}"))?;

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
