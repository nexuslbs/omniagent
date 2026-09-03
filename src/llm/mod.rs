//! LLM provider abstraction: supports multiple backends with reasoning and caching.
//!
//! Providers are configured via plugin config (providers.yml with $env: references).
//! The only hardcoded env var names are the infrastructure defaults set by the
//! deployment repo: `OMNI_DIR` and `LLM_PROVIDER`.
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
use crate::plugins_yaml::{get_remote_plugin, PluginYamlType};
use once_cell::sync::Lazy;
use parking_lot::RwLock;
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
    /// Whether this provider supports reasoning/thinking tokens in responses.
    pub supports_reasoning: bool,
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

/// Parse a provider manifest from a plugin.json path, returning (name, metadata).
/// Returns None if the file doesn't exist, isn't a provider, or can't be parsed.
fn read_provider_manifest(manifest_path: &Path) -> Option<(String, ProviderMetadata)> {
    if !manifest_path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(manifest_path).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&content).ok()?;
    let plugin_type = manifest.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if plugin_type != "provider" {
        return None;
    }
    let name = manifest
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        return None;
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
    let supports_reasoning = manifest
        .get("supports_reasoning")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Some((
        name.clone(),
        ProviderMetadata {
            name,
            default_base_url,
            api_mode,
            api_modes,
            default_model,
            supports_reasoning,
        },
    ))
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
            if let Some((name, meta)) = read_provider_manifest(&manifest_path) {
                map.insert(name, meta);
            }
        }
    }
    map
}

/// Refreshable cache of provider metadata loaded from plugin manifests.
///
/// The `source` field in `plugins.yml` is authoritative: each enabled provider
/// is resolved from exactly one path based on its source:
///
/// - `built-in`  → `/app/plugins/providers/{name}/plugin.json` (image)
/// - `bundled`   → `{OMNI_DIR}/plugins/providers/{name}/plugin.json` (volume)
/// - `remote`    → `{OMNI_DIR}/plugins/providers/.remote/{name}/plugin.json` (git clone)
/// - `installed` → `{OMNI_DIR}/plugins/installed/{name}/plugin.json` (URL download)
///
/// No fallback ordering. If the manifest is missing at the source-declared
/// path, the provider is skipped with a warning.
///
/// Use [`refresh_provider_metadata`] after enable/disable/install to keep the
/// cache in sync without restarting.
pub static PROVIDER_METADATA: Lazy<RwLock<HashMap<String, ProviderMetadata>>> =
    Lazy::new(|| RwLock::new(build_provider_metadata()));

/// Re-read all provider manifests from disk and update the static cache.
/// Call this after enabling, disabling, or installing a provider plugin.
pub fn refresh_provider_metadata() {
    let new_map = build_provider_metadata();
    let mut map = PROVIDER_METADATA.write();
    *map = new_map;
}

/// Build the provider metadata map by reading plugins.yml and scanning manifests.
fn build_provider_metadata() -> HashMap<String, ProviderMetadata> {
    let data_dir = match std::env::var("OMNI_DIR") {
        Ok(d) => d,
        Err(_) => {
            tracing::warn!("OMNI_DIR not set, provider metadata will be empty");
            return HashMap::new();
        }
    };

    let entries = match crate::plugins_yaml::load_raw(
        &data_dir,
        &crate::plugins_yaml::PluginYamlType::Provider,
    ) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("Failed to load providers from plugins.yml: {:?}", e);
            return HashMap::new();
        }
    };

    let mut map = HashMap::new();
    for (name, entry) in &entries {
        if !entry.enabled {
            continue;
        }

        let manifest_path = match entry.source.as_str() {
            "built-in" => format!("/app/plugins/providers/{}/plugin.json", name),
            "bundled" => format!("{}/plugins/providers/{}/plugin.json", data_dir, name),
            "remote" => {
                if let Some(remote) = get_remote_plugin(&data_dir, &PluginYamlType::Provider, name)
                {
                    let subpath = remote.path.as_deref().unwrap_or("");
                    format!(
                        "{}/plugins/providers/.remote/{}/{}/plugin.json",
                        data_dir, name, subpath
                    )
                } else {
                    format!(
                        "{}/plugins/providers/.remote/{}/plugin.json",
                        data_dir, name
                    )
                }
            }
            "installed" => {
                format!("{}/plugins/installed/{}/plugin.json", data_dir, name)
            }
            other => {
                tracing::warn!("Provider '{}': unknown source '{}', skipping", name, other);
                continue;
            }
        };

        let path = std::path::Path::new(&manifest_path);
        if !path.exists() {
            tracing::warn!(
                "Provider '{}': manifest not found at {} (source: {}), skipping",
                name,
                manifest_path,
                entry.source
            );
            continue;
        }

        if let Some((_, meta)) = read_provider_manifest(path) {
            tracing::info!(
                "Loaded provider '{}' from {} (source: {})",
                name,
                manifest_path,
                entry.source
            );
            map.insert(name.clone(), meta);
        }
    }
    // models.yml overrides (config/models.yml): provider-level fields override
    // plugin metadata; plugin-less providers (builtin chat/anthropic) are added.
    crate::models_yaml::apply_provider_overrides(&data_dir, &mut map);
    map
}

/// Resolve the default base URL for a provider from the plugin metadata.
/// Falls back to reading the plugin.json from disk if metadata is stale.
pub fn resolve_default_base_url(provider_name: &str) -> String {
    // First try the in-memory cache
    if let Some(url) = PROVIDER_METADATA
        .read()
        .get(provider_name)
        .filter(|m| !m.default_base_url.is_empty())
        .map(|m| m.default_base_url.clone())
    {
        return url;
    }
    // Fallback: read plugin.json from disk (handles stale metadata after git checkout)
    let data_dir = std::env::var("OMNI_DIR").unwrap_or_else(|_| "/opt/omni".to_string());
    for base in [
        format!(
            "{}/plugins/providers/{}/plugin.json",
            data_dir, provider_name
        ),
        format!("/app/plugins/providers/{}/plugin.json", provider_name),
    ] {
        if let Some(meta) = read_provider_manifest(&std::path::PathBuf::from(&base)) {
            if !meta.1.default_base_url.is_empty() {
                return meta.1.default_base_url;
            }
        }
    }
    String::new()
}

/// Resolve the default model for a provider from the plugin metadata.
/// Returns None if no default is found.
pub fn resolve_default_model(provider_name: &str) -> Option<String> {
    let meta = PROVIDER_METADATA.read();
    meta.get(provider_name).and_then(|m| {
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
        .read()
        .get(provider_name)
        .map(|m| m.api_mode.clone())
        .unwrap_or_else(|| "chat_completions".to_string())
}

/// Resolve the effective API mode for (provider, model): models.yml
/// `model_config.<model>.api_mode` first, else the provider-level mode
/// (models.yml provider api_mode, else plugin manifest / prefix match).
pub fn resolve_model_api_mode_effective(provider_name: &str, model_name: &str) -> ApiMode {
    let data_dir = std::env::var("OMNI_DIR").unwrap_or_default();
    if let Some(mode) =
        crate::models_yaml::resolve_model_api_mode(&data_dir, provider_name, model_name)
    {
        return match mode.as_str() {
            "anthropic_messages" => ApiMode::AnthropicMessages,
            _ => ApiMode::ChatCompletions,
        };
    }
    ApiMode::resolve(provider_name, model_name)
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
    let meta = PROVIDER_METADATA.read();
    let metadata = meta.get(provider_name)?;
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

/// Default User-Agent sent on every outgoing provider HTTP request so that
/// providers can attribute traffic to omniagent (some providers flag or
/// reject clients that send no user agent at all).
pub const DEFAULT_USER_AGENT: &str = concat!("omniagent/", env!("CARGO_PKG_VERSION"));

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
    /// Whether the provider supports reasoning/thinking tokens.
    /// Set from provider metadata at config construction time.
    pub supports_reasoning: bool,
    /// Additional HTTP headers attached to every provider request. Values are
    /// already resolved by the caller: typed config values (channel / profile)
    /// are turned into concrete strings before the client is built. A default
    /// User-Agent identifying omniagent is always sent first; an entry here
    /// overrides any header (including the User-Agent) by name.
    pub extra_headers: Vec<(String, String)>,
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
            .map(|g| g.read().default_provider.clone())
            .unwrap_or_default(); // Empty string → provider must be configured

        let provider = ProviderId::new(&provider_name);
        let base_url = resolve_default_base_url(&provider_name);
        let default_model = resolve_default_model(&provider_name).unwrap_or_default(); // Empty string → model must be configured
        let model = default_model;

        let api_mode = ApiMode::resolve(&provider_name, &model);

        // No generic API key fallback: provider api_key comes from plugin config
        // (providers.yml with $env: references), not from hardcoded env var names.
        let api_key = String::new();

        let supports_reasoning = PROVIDER_METADATA
            .read()
            .get(&provider_name)
            .map(|m| m.supports_reasoning)
            .unwrap_or(false);

        Self {
            provider,
            api_mode,
            api_key,
            base_url,
            model,
            max_tokens: 8192,
            temperature: 0.7,
            supports_reasoning,
            extra_headers: vec![],
        }
    }
}
// ---------------------------------------------------------------------------
// Per-provider throttling: limits concurrent API requests per provider
// ---------------------------------------------------------------------------

/// Per-provider concurrency throttler using semaphores.
///
/// Limits how many concurrent LLM API requests can be in-flight for a
/// given provider name.
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
        let meta = PROVIDER_METADATA.read();
        for name in meta.keys() {
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
    /// Reasoning/thinking content, echoed back to providers that require the
    /// round-trip (e.g. opencode-go / DeepSeek in thinking mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl ChatMessage {
    pub fn system(content: &str) -> Self {
        Self {
            role: "system".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
            name: None,
            reasoning_content: None,
        }
    }

    pub fn user(content: &str) -> Self {
        Self {
            role: "user".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
            name: None,
            reasoning_content: None,
        }
    }

    pub fn assistant(content: &str) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
            name: None,
            reasoning_content: None,
        }
    }

    pub fn tool_result(tool_call_id: &str, name: &str, content: &str) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.to_string(),
            tool_call_id: Some(tool_call_id.to_string()),
            tool_calls: None,
            name: Some(name.to_string()),
            reasoning_content: None,
        }
    }
}

/// Request payload for LLM completion.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages: Vec<ChatMessage>,
    pub max_tokens: Option<u32>,
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
    /// Provider finish reason (e.g. "stop", "length", "tool_calls").
    /// `length` means the response was truncated by the token budget -
    /// the model may not have finished emitting its action or answer.
    pub finish_reason: Option<String>,
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
// Transport hardening
// ---------------------------------------------------------------------------

/// Total request timeout (unchanged from before): 5 minutes.
const LLM_TOTAL_TIMEOUT_SECS: u64 = 300;
/// TCP/TLS connect timeout: a hung connect fails fast instead of waiting for
/// the total request timeout.
const LLM_CONNECT_TIMEOUT_SECS: u64 = 30;
/// Idle keep-alive socket lifetime: stale pooled connections closed by the
/// peer or a CloudFront edge are recycled instead of reused.
const LLM_POOL_IDLE_TIMEOUT_SECS: u64 = 90;
/// Maximum attempts for transient transport errors (initial + 2 retries).
const LLM_TRANSPORT_RETRY_ATTEMPTS: u32 = 3;
/// Base backoff between transport retries, in ms; doubles per retry.
const LLM_TRANSPORT_RETRY_BASE_DELAY_MS: u64 = 500;

/// Build the hardened reqwest client used for LLM completion requests.
///
/// - `timeout`: total request timeout (5 minutes) - unchanged.
/// - `connect_timeout`: fail fast on hung TCP/TLS connects (30s).
/// - `pool_idle_timeout`: recycle keep-alive sockets after 90s idle so stale
///   connections closed by the peer are not reused for new requests.
fn build_llm_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(LLM_TOTAL_TIMEOUT_SECS))
        .connect_timeout(std::time::Duration::from_secs(LLM_CONNECT_TIMEOUT_SECS))
        .pool_idle_timeout(std::time::Duration::from_secs(LLM_POOL_IDLE_TIMEOUT_SECS))
        .build()
        .expect("Failed to build reqwest Client")
}

/// Transient transport failure categories the retry layer understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportFailure {
    /// Connection establishment failed, or a pooled keep-alive socket was
    /// found dead (connection reset/closed by the peer or a CloudFront edge).
    Connect,
    /// The request timed out (connect timeout or total timeout).
    Timeout,
    /// Failed while reading the response body.
    Body,
    /// Failed while decoding the response body.
    Decode,
    /// Failed while sending the request with a transient IO error
    /// (connection reset, broken pipe, unexpected EOF).
    SendIo,
}

/// Classify a reqwest error as a retryable transient transport failure.
///
/// Retryable: connect failures, timeouts, body-read and decode errors, and
/// IO-level send failures. NOT retryable: deterministic errors such as
/// request-build failures (invalid URL) and redirect loops. HTTP status
/// errors are not `reqwest::Error` at all - a 4xx/5xx response arrives as
/// `Ok(response)` and never reaches this function, so the transport layer
/// never retries on status codes (429 has its own `RateLimited` path).
fn classify_transport_error(err: &reqwest::Error) -> Option<TransportFailure> {
    if err.is_timeout() {
        return Some(TransportFailure::Timeout);
    }
    if err.is_connect() {
        return Some(TransportFailure::Connect);
    }
    if err.is_body() {
        return Some(TransportFailure::Body);
    }
    if err.is_decode() {
        return Some(TransportFailure::Decode);
    }
    if err.is_request() && error_source_is_transient_io(err) {
        return Some(TransportFailure::SendIo);
    }
    None
}

/// Walk an error's source chain looking for a transient IO error (connection
/// reset, connection aborted, broken pipe, unexpected EOF) or hyper-level
/// "connection closed" text. Used to decide whether a send-phase failure
/// (e.g. `error sending request for url ...`) is worth retrying.
fn error_source_is_transient_io(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut next = err.source();
    while let Some(source) = next {
        if let Some(io_err) = source.downcast_ref::<std::io::Error>() {
            return matches!(
                io_err.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::TimedOut
            );
        }
        // Hyper surfaces some stale-pool failures (e.g. "connection closed
        // before message completed") without an io::Error in the chain.
        if error_text_says_transient(source) {
            return true;
        }
        next = source.source();
    }
    false
}

/// True when an error's message names a transient transport condition.
fn error_text_says_transient(err: &(dyn std::error::Error + 'static)) -> bool {
    let msg = err.to_string().to_lowercase();
    [
        "connection reset",
        "broken pipe",
        "unexpected eof",
        "connection closed",
        "connection aborted",
        "reset by peer",
        "eof while",
    ]
    .iter()
    .any(|pat| msg.contains(pat))
}

/// Send a request, retrying transient transport errors with short backoff.
///
/// Retries connect failures, timeouts, and body/decode errors up to
/// [`LLM_TRANSPORT_RETRY_ATTEMPTS`] times with exponential backoff starting
/// at [`LLM_TRANSPORT_RETRY_BASE_DELAY_MS`]. HTTP status codes are NEVER
/// retried here: the response is returned as-is and the caller decides
/// (429 becomes `Error::RateLimited`, other statuses surface immediately).
async fn send_with_transport_retry(
    req: reqwest::RequestBuilder,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut attempt: u32 = 1;
    let mut delay = std::time::Duration::from_millis(LLM_TRANSPORT_RETRY_BASE_DELAY_MS);
    loop {
        // JSON payloads are replayable, so clone per attempt. Non-replayable
        // bodies fall back to a single attempt.
        let attempt_req = match req.try_clone() {
            Some(clone) => clone,
            None => return req.send().await,
        };
        match attempt_req.send().await {
            Ok(resp) => return Ok(resp),
            Err(err) => {
                let retryable = classify_transport_error(&err).is_some();
                if attempt >= LLM_TRANSPORT_RETRY_ATTEMPTS || !retryable {
                    return Err(err);
                }
                warn!(
                    "[llm] transient transport error (attempt {}/{}) - retrying in {}ms: {}",
                    attempt,
                    LLM_TRANSPORT_RETRY_ATTEMPTS,
                    delay.as_millis(),
                    err
                );
                tokio::time::sleep(delay).await;
                delay *= 2;
                attempt += 1;
            }
        }
    }
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
        let client = build_llm_http_client();
        Self {
            config,
            client,
            throttle: ProviderThrottle::new(),
        }
    }

    /// Create a new client with a custom per-provider throttle.
    pub fn new_with_throttle(config: LLMConfig, throttle: ProviderThrottle) -> Self {
        let client = build_llm_http_client();
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
    /// Attach the default User-Agent plus the configured extra headers to a
    /// request builder. Header names/values that are not valid HTTP header
    /// material are skipped with a warning instead of failing the request.
    fn with_provider_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut req = req.header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT);
        for (name, value) in &self.config.extra_headers {
            let name_res = reqwest::header::HeaderName::from_bytes(name.as_bytes());
            let value_res = <reqwest::header::HeaderValue as std::str::FromStr>::from_str(value);
            match (name_res, value_res) {
                (Ok(name), Ok(value)) => req = req.header(name, value),
                _ => tracing::warn!(
                    "[llm] skipping invalid custom header {:?}={:?} (provider {})",
                    name,
                    value,
                    self.config.provider.0
                ),
            }
        }
        req
    }

    pub async fn completion(&self, request: CompletionRequest) -> AppResult<CompletionResponse> {
        let start = std::time::Instant::now();

        // Use-site validation: an empty provider is a valid *resolution*
        // terminal state (profile.provider → settings default_provider →
        // None), but it cannot be *consumed*. Fail loudly instead of
        // silently substituting a hardcoded vendor name.
        if self.config.provider.0.is_empty() {
            return Err(Error::Message(
                "no LLM provider configured: set settings.yml default_provider or a profile/channel provider"
                    .into(),
            ));
        }

        // Check if this provider is an external subprocess provider
        let provider_name = &self.config.provider.0;
        // Try external completion: clone Arc first, drop registry guard, then call complete
        let external_result = {
            let client_opt = {
                let registry = crate::provider::registry::PROVIDER_REGISTRY.read();
                registry.get_cloned(provider_name)
            };
            // Registry guard is dropped here: we have an independent Arc<ExternalProviderClient>

            client_opt.map(|client| {
                let messages: Vec<serde_json::Value> = request.messages.iter()
                    .map(|m| {
                        let mut msg = serde_json::json!({
                            "role": m.role,
                            "content": m.content,
                        });
                        if let Some(ref r) = m.reasoning_content {
                            msg["reasoning_content"] = serde_json::Value::String(r.clone());
                        }
                        if let Some(ref tc) = m.tool_calls {
                            msg["tool_calls"] = serde_json::to_value(tc).unwrap_or_default();
                        }
                        msg
                    })
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
                                finish_reason: result.finish_reason.clone(),
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
            if let Ok(resp) = fut.await {
                return Ok(resp);
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
            "temperature": request.temperature,
            "stream": request.stream,
        });
        // max_tokens is optional: when None (not configured), the provider's
        // own default output limit applies - no cap is sent in the request.
        if let Some(mt) = request.max_tokens {
            body["max_tokens"] = serde_json::Value::from(mt);
        }

        if self.config.supports_reasoning {
            body["include_reasoning"] = serde_json::Value::Bool(true);
        }

        // Include tools if provided
        if let Some(ref tools) = request.tools {
            if !tools.is_empty() {
                body["tools"] = serde_json::Value::Array(tools.clone());
            }
        }

        let resp = send_with_transport_retry(
            self.with_provider_headers(
                self.client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.config.api_key))
                    .header("Content-Type", "application/json"),
            )
            .json(&body),
        )
        .await
        .ctx("Failed to send OpenAI-compatible completion request")?;

        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let status = resp.status();
        let resp_text = resp.text().await.ctx("Failed to read response body")?;

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(Error::RateLimited { retry_after });
        }
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
            finish_reason: if finish_reason.is_empty() {
                None
            } else {
                Some(finish_reason)
            },
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

        // finish_reason arrives on the final chunk of a stream.
        let finish_reason = response
            .choices
            .iter()
            .rev()
            .find_map(|c| c.finish_reason.clone().filter(|f| !f.is_empty()));

        Ok(CompletionResponse {
            content,
            reasoning,
            tool_calls: vec![],
            usage: response.usage,
            duration_ms: 0,
            finish_reason,
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
            "temperature": request.temperature,
        });
        // max_tokens is optional: when None (not configured), the provider's
        // own default output limit applies - no cap is sent in the request.
        if let Some(mt) = request.max_tokens {
            body["max_tokens"] = serde_json::Value::from(mt);
        }

        if let Some(s) = system {
            body["system"] = serde_json::Value::String(s);
        }

        // Enable thinking if we want to capture reasoning (only for Anthropic provider)
        if self.config.provider.0 == "anthropic" {
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": request.max_tokens.unwrap_or(32000),
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

        let req = self.with_provider_headers(req);

        let resp = send_with_transport_retry(req.json(&body))
            .await
            .ctx("Failed to send Anthropic completion request")?;

        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let status = resp.status();
        let resp_text = resp
            .text()
            .await
            .ctx("Failed to read Anthropic response body")?;

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(Error::RateLimited { retry_after });
        }
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
            finish_reason: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completion_rejects_empty_provider() {
        // An empty provider is a valid *resolution* terminal state, but it
        // must NOT be consumable: completion() fails loudly instead of
        // silently substituting a hardcoded vendor name.
        let config = LLMConfig {
            provider: ProviderId::new(""),
            api_mode: ApiMode::ChatCompletions,
            api_key: String::new(),
            base_url: String::new(),
            model: String::new(),
            max_tokens: 8192,
            temperature: 0.7,
            supports_reasoning: false,
            extra_headers: vec![],
        };
        let client = LLMClient::new(config);
        let request = CompletionRequest {
            messages: vec![],
            max_tokens: Some(100),
            temperature: 0.0,
            stream: false,
            tools: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(client.completion(request)).unwrap_err();
        assert!(err.to_string().contains("no LLM provider configured"));
    }

    // ── resolve_llm_api_key ─────────────────────────────────────────────────

    #[test]
    fn test_resolve_llm_api_key_valid() {
        let result = resolve_llm_api_key(Some("sk-test-key-12345"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "sk-test-key-12345");
    }

    #[test]
    fn test_resolve_llm_api_key_empty() {
        let result = resolve_llm_api_key(Some(""));
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_llm_api_key_none() {
        let result = resolve_llm_api_key(None);
        assert!(result.is_err());
    }

    // ── resolve_default_base_url ────────────────────────────────────────────
    // NOTE: In the test environment without OMNI_DIR set and without provider
    // plugin manifests on disk, PROVIDER_METADATA is empty and the fallback
    // disk reads also fail, so resolve_default_base_url returns an empty string
    // for all providers. These tests verify the function handles all cases
    // without panicking.

    #[test]
    fn test_resolve_default_base_url_known_provider() {
        // The function resolves the URL from plugin.json on disk when
        // found, or returns empty when no manifest is available (e.g.
        // during Docker builds where /app/plugins/ may not exist).
        // Accept both outcomes - the test verifies the function doesn't
        // panic or return garbage.
        let url = resolve_default_base_url("openai");
        let ok = url == "https://api.openai.com/v1" || url.is_empty();
        assert!(ok, "expected known URL or empty, got: '{url}'");
    }

    #[test]
    fn test_resolve_default_base_url_anthropic() {
        // No anthropic plugin.json on disk → returns empty.
        let url = resolve_default_base_url("anthropic");
        assert_eq!(url, "");
    }

    #[test]
    fn test_resolve_default_base_url_unknown() {
        let url = resolve_default_base_url("nonexistent-provider");
        assert_eq!(url, "");
    }

    #[test]
    fn test_resolve_default_base_url_empty_name() {
        let url = resolve_default_base_url("");
        assert_eq!(url, "");
    }

    // ── resolve_default_model ───────────────────────────────────────────────

    #[test]
    fn test_resolve_default_model_returns_none_in_test() {
        // Without provider manifests, all providers return None.
        let model = resolve_default_model("openai");
        assert!(model.is_none());
    }

    #[test]
    fn test_resolve_default_model_unknown() {
        let model = resolve_default_model("nonexistent");
        assert!(model.is_none());
    }

    #[test]
    fn test_resolve_default_model_empty_name() {
        let model = resolve_default_model("");
        assert!(model.is_none());
    }

    // ── resolve_provider_api_mode ───────────────────────────────────────────

    #[test]
    fn test_resolve_provider_api_mode_unknown_returns_default() {
        // Without provider manifests, returns the default "chat_completions".
        let mode = resolve_provider_api_mode("openai");
        assert_eq!(mode, "chat_completions");
    }

    #[test]
    fn test_resolve_provider_api_mode_unknown_provider() {
        let mode = resolve_provider_api_mode("nonexistent");
        assert_eq!(mode, "chat_completions");
    }

    #[test]
    fn test_resolve_provider_api_mode_empty_name() {
        let mode = resolve_provider_api_mode("");
        assert_eq!(mode, "chat_completions");
    }

    // ── ProviderThrottle ────────────────────────────────────────────────────

    #[test]
    fn test_provider_throttle_new_defaults() {
        let throttle = ProviderThrottle::new();
        assert_eq!(
            throttle.max_permits(),
            ProviderThrottle::DEFAULT_MAX_CONCURRENT
        );
    }

    #[test]
    fn test_provider_throttle_with_custom_max() {
        let throttle = ProviderThrottle::with_max_permits(2);
        assert_eq!(throttle.max_permits(), 2);
    }

    #[test]
    fn test_provider_throttle_available_permits_unknown_provider() {
        // Without provider metadata, all providers are unknown.
        let throttle = ProviderThrottle::new();
        assert!(throttle.available_permits("openai").is_none());
    }

    #[test]
    fn test_provider_throttle_default_max() {
        assert_eq!(ProviderThrottle::DEFAULT_MAX_CONCURRENT, 5);
    }

    // ── ProviderId ──────────────────────────────────────────────────────────

    #[test]
    fn test_provider_id_new_and_display() {
        let pid = ProviderId::new("openai");
        assert_eq!(pid.to_string(), "openai");
        assert_eq!(pid.0, "openai");
    }

    #[test]
    fn test_provider_id_equality() {
        assert_eq!(ProviderId::new("a"), ProviderId::new("a"));
        assert_ne!(ProviderId::new("a"), ProviderId::new("b"));
    }

    // ── ApiMode ─────────────────────────────────────────────────────────────

    #[test]
    fn test_api_mode_resolve_default() {
        // Without metadata, defaults to ChatCompletions for unknown providers.
        let mode = ApiMode::resolve("nonexistent", "gpt-4");
        assert_eq!(mode, ApiMode::ChatCompletions);
    }

    #[test]
    fn test_api_mode_equality() {
        assert_eq!(ApiMode::ChatCompletions, ApiMode::ChatCompletions);
        assert_eq!(ApiMode::AnthropicMessages, ApiMode::AnthropicMessages);
        assert_ne!(ApiMode::ChatCompletions, ApiMode::AnthropicMessages);
    }

    // ── ChatMessage ─────────────────────────────────────────────────────────

    #[test]
    fn test_chat_message_system() {
        let msg = ChatMessage::system("You are a helpful assistant.");
        assert_eq!(msg.role, "system");
        assert_eq!(msg.content, "You are a helpful assistant.");
        assert!(msg.tool_call_id.is_none());
        assert!(msg.tool_calls.is_none());
        assert!(msg.name.is_none());
    }

    #[test]
    fn test_chat_message_user() {
        let msg = ChatMessage::user("Hello!");
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, "Hello!");
    }

    #[test]
    fn test_chat_message_assistant() {
        let msg = ChatMessage::assistant("I can help with that.");
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "I can help with that.");
    }

    #[test]
    fn test_chat_message_tool_result() {
        let msg = ChatMessage::tool_result("call_123", "get_weather", "Sunny, 72°F");
        assert_eq!(msg.role, "tool");
        assert_eq!(msg.content, "Sunny, 72°F");
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(msg.name.as_deref(), Some("get_weather"));
    }

    #[test]
    fn test_chat_message_reasoning_content_roundtrip() {
        // Constructors default reasoning_content to None.
        let msg = ChatMessage::assistant("hello");
        assert!(msg.reasoning_content.is_none());

        // Serialization must OMIT the field when None (serde skip_serializing_if).
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            !json.contains("reasoning_content"),
            "None must not serialize: {json}"
        );

        // Deserialization of a message without the field defaults to None.
        let parsed: ChatMessage = serde_json::from_str(&json).unwrap();
        assert!(parsed.reasoning_content.is_none());

        // When set, the field MUST round-trip through serde (this is the
        // contract opencode-go / DeepSeek thinking mode requires).
        let mut with_reasoning = ChatMessage::assistant("final answer");
        with_reasoning.reasoning_content = Some("thinking step by step".to_string());
        let json2 = serde_json::to_string(&with_reasoning).unwrap();
        assert!(
            json2.contains("reasoning_content"),
            "set reasoning_content must serialize: {json2}"
        );
        let parsed2: ChatMessage = serde_json::from_str(&json2).unwrap();
        assert_eq!(
            parsed2.reasoning_content.as_deref(),
            Some("thinking step by step")
        );
    }

    // ── Transport hardening: retry classification ──────────────────────────

    /// Minimal error wrapper for building synthetic source chains in tests.
    #[derive(Debug)]
    struct TestChainError {
        msg: String,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    }

    impl TestChainError {
        fn leaf(msg: &str) -> Self {
            Self {
                msg: msg.to_string(),
                source: None,
            }
        }
        fn chain(msg: &str, source: Box<dyn std::error::Error + Send + Sync>) -> Self {
            Self {
                msg: msg.to_string(),
                source: Some(source),
            }
        }
    }

    impl std::fmt::Display for TestChainError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.msg)
        }
    }

    impl std::error::Error for TestChainError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source
                .as_deref()
                .map(|s| s as &(dyn std::error::Error + 'static))
        }
    }

    fn io_error(kind: std::io::ErrorKind, msg: &str) -> std::io::Error {
        std::io::Error::new(kind, msg)
    }

    fn chain(msg: &str, source: Box<dyn std::error::Error + Send + Sync>) -> TestChainError {
        TestChainError::chain(msg, source)
    }

    #[test]
    fn test_transient_io_detects_connection_reset() {
        let err = chain(
            "error sending request for url (https://api.deepseek.com/v1/chat/completions)",
            Box::new(io_error(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            )),
        );
        assert!(error_source_is_transient_io(&err));
    }

    #[test]
    fn test_transient_io_detects_broken_pipe() {
        let err = chain(
            "error sending request",
            Box::new(io_error(std::io::ErrorKind::BrokenPipe, "broken pipe")),
        );
        assert!(error_source_is_transient_io(&err));
    }

    #[test]
    fn test_transient_io_detects_unexpected_eof() {
        let err = chain(
            "error sending request",
            Box::new(io_error(
                std::io::ErrorKind::UnexpectedEof,
                "unexpected EOF",
            )),
        );
        assert!(error_source_is_transient_io(&err));
    }

    #[test]
    fn test_transient_io_detects_connection_aborted() {
        let err = chain(
            "error sending request",
            Box::new(io_error(
                std::io::ErrorKind::ConnectionAborted,
                "connection aborted",
            )),
        );
        assert!(error_source_is_transient_io(&err));
    }

    #[test]
    fn test_transient_io_detects_io_timeout() {
        let err = chain(
            "error sending request",
            Box::new(io_error(std::io::ErrorKind::TimedOut, "timed out")),
        );
        assert!(error_source_is_transient_io(&err));
    }

    #[test]
    fn test_transient_io_rejects_non_transient_io() {
        // A non-transient IO kind (file-not-found) must not be retried.
        let err = chain(
            "error sending request",
            Box::new(io_error(
                std::io::ErrorKind::NotFound,
                "no such file or directory",
            )),
        );
        assert!(!error_source_is_transient_io(&err));
    }

    #[test]
    fn test_transient_io_rejects_non_io_source() {
        // A deterministic failure (e.g. "builder error: invalid url") with no
        // transient IO in its chain must not be retried.
        let err = chain(
            "error sending request",
            Box::new(TestChainError::leaf("builder error: invalid url")),
        );
        assert!(!error_source_is_transient_io(&err));
    }

    #[test]
    fn test_transient_io_detects_hyper_connection_closed_text() {
        // Hyper surfaces stale-pool failures without an io::Error in the
        // chain; the message text identifies the transient condition.
        let err = chain(
            "error sending request",
            Box::new(TestChainError::leaf(
                "connection closed before message completed",
            )),
        );
        assert!(error_source_is_transient_io(&err));
    }

    #[test]
    fn test_transient_io_walks_deep_chains() {
        let inner = chain(
            "connection error",
            Box::new(io_error(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            )),
        );
        let outer = chain("error sending request", Box::new(inner));
        assert!(error_source_is_transient_io(&outer));
    }

    // ── Transport hardening: retry behaviour (end-to-end) ──────────────────

    /// Spawn a minimal HTTP/1.1 server on an ephemeral port. Each accepted
    /// connection is answered with `responder(request_index)` - the raw HTTP
    /// response text. Returns (base_url, request counter).
    fn spawn_http_server(
        responder: impl Fn(usize) -> String + Send + 'static,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = Arc::clone(&count);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let idx = count2.fetch_add(1, Ordering::SeqCst) + 1;
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf); // consume the request
                let resp = responder(idx);
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}"), count)
    }

    fn http_response(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status_line,
            body.len(),
            body
        )
    }

    #[tokio::test]
    async fn test_transport_retry_does_not_retry_http_statuses() {
        // 5xx and 4xx (incl. 429) responses must NOT be retried by the
        // transport layer: each status gets exactly one request. 429 is
        // converted to Error::RateLimited by the completion callers.
        for (status_line, status) in [
            (
                "500 Internal Server Error",
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            ),
            ("502 Bad Gateway", reqwest::StatusCode::BAD_GATEWAY),
            (
                "503 Service Unavailable",
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
            ),
            ("404 Not Found", reqwest::StatusCode::NOT_FOUND),
            ("400 Bad Request", reqwest::StatusCode::BAD_REQUEST),
            (
                "429 Too Many Requests",
                reqwest::StatusCode::TOO_MANY_REQUESTS,
            ),
        ] {
            let (base, count) = spawn_http_server(move |_| http_response(status_line, "{}"));
            let client = reqwest::Client::new();
            let resp = send_with_transport_retry(
                client
                    .post(format!("{base}/chat/completions"))
                    .json(&serde_json::json!({"model": "t"})),
            )
            .await
            .expect("response must be returned without retry");
            assert_eq!(resp.status(), status);
            assert_eq!(
                count.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "status {status} must not be retried"
            );
        }
    }

    #[tokio::test]
    async fn test_transport_retry_retries_connection_failure() {
        // A server that accepts and then closes without responding produces a
        // transient transport error (EOF / connection closed). The request
        // must be retried up to LLM_TRANSPORT_RETRY_ATTEMPTS times.
        use std::io::Read;
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = Arc::clone(&count);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                count2.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                // Consume the request, then close without a response.
                let _ = stream.read(&mut buf);
            }
        });

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("build client");
        let err = send_with_transport_retry(
            client
                .post(format!("http://{addr}/chat/completions"))
                .json(&serde_json::json!({"model": "t"})),
        )
        .await
        .expect_err("all retries must be exhausted");
        assert!(
            classify_transport_error(&err).is_some(),
            "final error must still be a transport error: {err}"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            LLM_TRANSPORT_RETRY_ATTEMPTS as usize,
            "transient connection failure must be retried LLM_TRANSPORT_RETRY_ATTEMPTS times"
        );
    }

    #[tokio::test]
    async fn test_transport_retry_does_not_retry_builder_error() {
        // An invalid URL is a deterministic request-build failure: it must
        // not be retried and classify_transport_error must reject it.
        let client = reqwest::Client::new();
        let err = send_with_transport_retry(client.post("not a valid url"))
            .await
            .expect_err("invalid URL must fail");
        assert!(err.is_builder());
        assert_eq!(classify_transport_error(&err), None);
    }

    #[tokio::test]
    async fn test_completion_429_still_returns_rate_limited() {
        // The existing 429 → Error::RateLimited path is unchanged: a 429
        // response surfaces as RateLimited { retry_after } and is NOT retried
        // at the transport layer.
        let (base, count) = spawn_http_server(|_| {
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                .to_string()
        });
        let config = LLMConfig {
            provider: ProviderId::new("test-provider"),
            api_mode: ApiMode::ChatCompletions,
            api_key: "test-key".to_string(),
            base_url: base,
            model: "test-model".to_string(),
            max_tokens: 128,
            temperature: 0.0,
            supports_reasoning: false,
            extra_headers: vec![],
        };
        let client = LLMClient::new(config);
        let request = CompletionRequest {
            messages: vec![ChatMessage::user("hi")],
            max_tokens: Some(128),
            temperature: 0.0,
            stream: false,
            tools: None,
        };
        let err = client
            .completion(request)
            .await
            .expect_err("429 must error");
        match &err {
            Error::RateLimited { retry_after } => {
                assert_eq!(*retry_after, Some(7));
            }
            other => panic!("expected RateLimited, got: {other}"),
        }
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "429 must not be retried at the transport layer"
        );
    }

    #[tokio::test]
    async fn test_completion_sends_default_ua_and_custom_headers() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        // Local HTTP/1.1 server that captures the raw request so the test can
        // assert what headers the client actually sends on the wire.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let received: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let received2 = Arc::clone(&received);
        let done = Arc::new(AtomicUsize::new(0));
        let done2 = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut stream = listener.incoming().next().expect("accept").expect("stream");
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            *received2.lock().unwrap() = String::from_utf8_lossy(&buf).to_string();
            done2.store(1, Ordering::SeqCst);
            let body = r#"{"id":"1","object":"chat.completion","model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        });

        let config = LLMConfig {
            provider: ProviderId::new("test-provider"),
            api_mode: ApiMode::ChatCompletions,
            api_key: "test-key".to_string(),
            base_url: format!("http://{addr}"),
            model: "test-model".to_string(),
            max_tokens: 128,
            temperature: 0.0,
            supports_reasoning: false,
            extra_headers: vec![
                ("x-opencode-session".to_string(), "main".to_string()),
                ("x-profile".to_string(), "omni".to_string()),
            ],
        };
        let client = LLMClient::new(config);
        let request = CompletionRequest {
            messages: vec![ChatMessage::user("hi")],
            max_tokens: Some(128),
            temperature: 0.0,
            stream: false,
            tools: None,
        };
        let resp = client.completion(request).await.expect("completion ok");
        assert_eq!(resp.content, "hi");
        assert_eq!(done.load(Ordering::SeqCst), 1, "server never saw a request");
        let raw = received.lock().unwrap().clone();
        let lower = raw.to_ascii_lowercase();
        assert!(
            lower.contains("user-agent: omniagent/"),
            "expected default User-Agent, got request: {raw}"
        );
        assert!(
            lower.contains("x-opencode-session: main"),
            "expected x-opencode-session: main header, got request: {raw}"
        );
        assert!(
            lower.contains("x-profile: omni"),
            "expected x-profile: omni header, got request: {raw}"
        );
    }
}
#[cfg(test)]
mod usage_parse_tests {
    use super::*;

    // Cache-hit accounting: the OpenAI-compatible usage object must parse the
    // DeepSeek `prompt_cache_hit_tokens` field into `cached_tokens` (serde
    // alias) - this is the field the threads-table cached_tokens column is
    // fed from via merge_usage + complete_thread.
    #[test]
    fn usage_parses_deepseek_prompt_cache_hit_tokens() {
        let json = r#"{
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 900,
            "prompt_cache_miss_tokens": 100
        }"#;
        let u: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(u.prompt_tokens, 1000);
        assert_eq!(u.completion_tokens, 50);
        assert_eq!(u.cached_tokens, Some(900));
    }

    #[test]
    fn usage_parses_cached_tokens_directly() {
        let json = r#"{"prompt_tokens": 10, "completion_tokens": 2, "cached_tokens": 7}"#;
        let u: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(u.cached_tokens, Some(7));
    }

    #[test]
    fn usage_missing_cache_fields_yields_none() {
        // The observed opencode-go gateway usage omits cache fields entirely:
        // cached_tokens must be None (never a wrong number) - the threads
        // table then records 0, which is exactly the reported symptom.
        let json = r#"{"prompt_tokens": 250000, "completion_tokens": 1000}"#;
        let u: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(u.prompt_tokens, 250000);
        assert_eq!(u.completion_tokens, 1000);
        assert_eq!(u.cached_tokens, None);
    }
}
