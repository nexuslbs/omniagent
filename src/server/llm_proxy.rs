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
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// Optional request context used to resolve typed headers declared for
    /// this provider in models.yml (e.g. `{ type: channel }`). When omitted,
    /// typed headers cannot be resolved and are skipped.
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
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

    // Known provider? Either a plugin (disk/YAML) or a models.yml-only
    // (plugin-less) provider declared with `plugin: false`.
    let known = matches!(
        crate::plugins_yaml::get_plugin(
            &state.data_dir,
            provider_name,
            &crate::plugins_yaml::PluginYamlType::Provider,
        ),
        Ok(Some(_))
    ) || crate::models_yaml::is_plugin_less(&state.data_dir, provider_name);
    if !known {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Provider '{}' not found or not configured", provider_name)
            })),
        );
    }

    // Single shared resolver: models.yml api_key ($env:/$secret: expanded by
    // core at request time) first, else the provider plugin config (same
    // expansion path as every other plugin config value).
    let api_key =
        crate::models_yaml::resolve_provider_api_key(&state.data_dir, provider_name, &state.pool)
            .await;

    let api_mode = ApiMode::resolve(provider_name, model_name);
    let resolved_provider = crate::llm::ProviderId::new(provider_name);

    let llm_config = LLMConfig {
        provider: resolved_provider,
        api_key,
        base_url,
        model: model_name.clone(),
        api_mode,
        max_tokens: body.max_tokens.unwrap_or(8192),
        temperature: body.temperature,
        supports_reasoning: false,
        // Custom headers declared for this provider in models.yml, merged with
        // the provider plugin config headers and resolved with the optional
        // request context - the same provider-agnostic resolver the agent path
        // uses, so a code-less (plugin: false) provider whose endpoint needs a
        // header works through this endpoint too.
        extra_headers: crate::models_yaml::resolve_extra_headers(
            &state.data_dir,
            provider_name,
            model_name,
            body.channel.as_deref(),
            body.profile.as_deref(),
        ),
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
            reasoning_content: None,
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
