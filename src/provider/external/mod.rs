//! External provider plugin integration: protocol types for subprocess-based providers.
//!
//! Provider plugins can run as standalone subprocesses (like platform plugins) instead
//! of being HTTP endpoints that omniagent calls directly. The subprocess communicates
//! via JSON-lines over stdin/stdout.
//!
//! Protocol:
//! 1. Agent sends `{"id": 1, "method": "initialize", "params": {}}`
//! 2. Plugin responds with `{"id": 1, "result": {"name": "...", "models": [...]}}`
//! 3. Agent sends `{"id": 2, "method": "complete", "params": {...}}`
//! 4. Plugin responds with `{"id": 2, "result": {"content": "...", ...}}`

pub mod client;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Protocol types
// ---------------------------------------------------------------------------

/// A request sent from the agent to a provider plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A response from a provider plugin to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProviderResponse {
    Success { id: u64, result: serde_json::Value },
    Error { id: u64, error: ProviderError },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderError {
    pub code: i64,
    pub message: String,
}

/// Result of the initialize handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub name: String,
    /// List of models this provider supports.
    #[serde(default)]
    pub models: Vec<String>,
}

/// Parameters for the complete method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteParams {
    pub model: String,
    pub messages: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
}

fn default_temperature() -> f32 {
    0.7
}

/// Result of a complete operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteResult {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageResult>,
    /// Provider finish reason (e.g. "stop", "length", "tool_calls").
    /// `length` signals the response was truncated by the output budget -
    /// the model may not have finished emitting its action or answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageResult {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Cached (prefix-hit) input tokens. Providers report this under
    /// different names depending on their API surface:
    /// - OpenAI-compatible / DeepSeek: `cached_tokens` or `prompt_cache_hit_tokens`
    /// - Anthropic: `cache_read_input_tokens` / `cache_creation_input_tokens`
    ///
    /// The serde aliases cover deserialization; the manual `parse_usage`
    /// helper covers the JSON-lines subprocess path (which extracts fields
    /// by key name rather than via serde).
    #[serde(default)]
    #[serde(alias = "prompt_cache_hit_tokens")]
    #[serde(alias = "cache_read_input_tokens")]
    #[serde(alias = "cache_creation_input_tokens")]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

/// Parse a provider `usage` object into `UsageResult`, accepting every
/// cache-field naming convention:
/// - `cached_tokens` (OpenAI-compatible passthrough)
/// - `prompt_cache_hit_tokens` (DeepSeek official API field name)
/// - `cache_read_input_tokens` / `cache_creation_input_tokens` (Anthropic)
///
/// Returns `None` when the value is missing or not an object. Missing
/// numeric fields default to 0 (prompt/completion) or `None` (cache,
/// reasoning) - identical to the old inline extraction, so behavior for
/// providers that omit cache fields is unchanged.
pub fn parse_usage(value: &serde_json::Value) -> Option<UsageResult> {
    let u = value.as_object()?;
    let num = |k: &str| u.get(k).and_then(|v| v.as_u64()).map(|v| v as u32);
    let cached = num("cached_tokens")
        .or_else(|| num("prompt_cache_hit_tokens"))
        .or_else(|| num("cache_read_input_tokens"))
        .or_else(|| num("cache_creation_input_tokens"));
    Some(UsageResult {
        prompt_tokens: num("prompt_tokens")
            .or_else(|| num("input_tokens"))
            .unwrap_or(0),
        completion_tokens: num("completion_tokens")
            .or_else(|| num("output_tokens"))
            .unwrap_or(0),
        cached_tokens: cached,
        reasoning_tokens: num("reasoning_tokens"),
    })
}

/// Build an initialize request.
pub fn build_initialize_request(id: u64) -> String {
    let req = ProviderRequest {
        id: Some(id),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({})),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

/// Build a complete request.
pub fn build_complete_request(id: u64, params: &CompleteParams) -> String {
    let req = ProviderRequest {
        id: Some(id),
        method: "complete".to_string(),
        params: Some(serde_json::to_value(params).unwrap_or_default()),
    };
    serde_json::to_string(&req).unwrap_or_default()
}

/// Build a list_models request.
pub fn build_list_models_request(id: u64) -> String {
    let req = ProviderRequest {
        id: Some(id),
        method: "list_models".to_string(),
        params: None,
    };
    serde_json::to_string(&req).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_usage_accepts_cached_tokens() {
        let u = serde_json::json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "cached_tokens": 900,
        });
        let parsed = parse_usage(&u).expect("usage object parses");
        assert_eq!(parsed.prompt_tokens, 1000);
        assert_eq!(parsed.completion_tokens, 50);
        assert_eq!(parsed.cached_tokens, Some(900));
        assert_eq!(parsed.reasoning_tokens, None);
    }

    #[test]
    fn parse_usage_accepts_deepseek_prompt_cache_hit_tokens() {
        // DeepSeek's official API reports cache hits as prompt_cache_hit_tokens.
        let u = serde_json::json!({
            "prompt_tokens": 2000,
            "completion_tokens": 10,
            "prompt_cache_hit_tokens": 1800,
            "prompt_cache_miss_tokens": 200,
        });
        let parsed = parse_usage(&u).expect("usage object parses");
        assert_eq!(parsed.prompt_tokens, 2000);
        assert_eq!(parsed.cached_tokens, Some(1800));
    }

    #[test]
    fn parse_usage_accepts_anthropic_cache_fields() {
        // Anthropic-style usage on a subprocess provider response.
        let u = serde_json::json!({
            "input_tokens": 1500,
            "output_tokens": 25,
            "cache_read_input_tokens": 1200,
        });
        let parsed = parse_usage(&u).expect("usage object parses");
        assert_eq!(parsed.prompt_tokens, 1500);
        assert_eq!(parsed.completion_tokens, 25);
        assert_eq!(parsed.cached_tokens, Some(1200));

        let u2 = serde_json::json!({
            "input_tokens": 1500,
            "output_tokens": 25,
            "cache_creation_input_tokens": 300,
        });
        let parsed2 = parse_usage(&u2).expect("usage object parses");
        assert_eq!(parsed2.cached_tokens, Some(300));
    }

    #[test]
    fn parse_usage_prefers_cached_tokens_when_multiple_present() {
        let u = serde_json::json!({
            "prompt_tokens": 1000,
            "completion_tokens": 5,
            "cached_tokens": 700,
            "prompt_cache_hit_tokens": 600,
        });
        let parsed = parse_usage(&u).expect("usage object parses");
        assert_eq!(parsed.cached_tokens, Some(700));
    }

    #[test]
    fn parse_usage_missing_cache_fields_yields_none() {
        // Providers that omit cache fields (observed: the opencode-go gateway
        // strips them) must parse to cached_tokens=None - never a wrong number.
        let u = serde_json::json!({
            "prompt_tokens": 250000,
            "completion_tokens": 1000,
        });
        let parsed = parse_usage(&u).expect("usage object parses");
        assert_eq!(parsed.prompt_tokens, 250000);
        assert_eq!(parsed.cached_tokens, None);
    }

    #[test]
    fn parse_usage_non_object_or_missing_returns_none() {
        assert!(parse_usage(&serde_json::Value::Null).is_none());
        assert!(parse_usage(&serde_json::json!("nope")).is_none());
        assert!(parse_usage(&serde_json::json!([])).is_none());
    }

    #[test]
    fn usage_result_serde_aliases_cover_deepseek_field_name() {
        // The serde path (used if a plugin response is ever deserialized
        // directly) must also accept prompt_cache_hit_tokens.
        let json = r#"{"prompt_tokens": 10, "completion_tokens": 2, "prompt_cache_hit_tokens": 8}"#;
        let parsed: UsageResult = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.prompt_tokens, 10);
        assert_eq!(parsed.cached_tokens, Some(8));
    }
}
