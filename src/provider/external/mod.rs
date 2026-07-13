//! External provider plugin integration - protocol types for subprocess-based providers.
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
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
}

fn default_max_tokens() -> u32 { 4096 }
fn default_temperature() -> f32 { 0.7 }

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageResult {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
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
