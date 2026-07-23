//! LLM proxy endpoint: allows MCP server plugins (e.g. memory) to make
//! LLM completion calls through omniagent's provider infrastructure without
//! knowing API keys or URLs.
//!
//! POST /api/llm/chat
//! Body: { provider, model, messages: [{role, content}], max_tokens?, temperature? }
//! Returns: { content: "..." }

use super::AppState;
use crate::llm::{ApiMode, ChatMessage, CompletionRequest, LLMClient, LLMConfig};
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct LlmChatRequest {
    pub provider: String,
    pub model: String,
    pub messages: Vec<LlmMessage>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

#[derive(Debug, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
}

fn default_max_tokens() -> u32 {
    4096
}
fn default_temperature() -> f32 {
    0.3
}

#[derive(Debug, Serialize)]
pub struct LlmChatResponse {
    pub content: String,
}

pub(crate) async fn llm_chat_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LlmChatRequest>,
) -> impl IntoResponse {
    let provider_name = &body.provider;
    let model_name = &body.model;

    // Resolve base URL from provider plugin metadata
    let base_url = crate::llm::resolve_default_base_url(provider_name);

    // Look up api_key from the provider's resolved plugin config
    let api_key = match crate::plugins_yaml::get_plugin(&state.data_dir, provider_name) {
        Ok(Some(detail)) => detail
            .config
            .get("api_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_default(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("Provider '{}' not found or not configured", provider_name)
                })),
            );
        }
    };

    let api_mode = ApiMode::resolve(provider_name, model_name);
    let resolved_provider = crate::llm::ProviderId::new(provider_name);

    let llm_config = LLMConfig {
        provider: resolved_provider,
        api_key,
        base_url,
        model: model_name.clone(),
        api_mode,
        max_tokens: body.max_tokens,
        temperature: body.temperature,
        supports_reasoning: false,
    };

    let llm = LLMClient::new(llm_config);

    let messages: Vec<ChatMessage> = body
        .messages
        .iter()
        .map(|m| ChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            tool_call_id: None,
            tool_calls: None,
            name: None,
        })
        .collect();

    let request = CompletionRequest {
        messages,
        max_tokens: body.max_tokens,
        temperature: body.temperature,
        stream: false,
        tools: None,
    };

    match llm.completion(request).await {
        Ok(resp) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "content": resp.content
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("LLM completion failed: {}", e)
            })),
        ),
    }
}
