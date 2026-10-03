//! Read-only prompt API for the dashboard (Prompt page + Memory page).
//!
//! HARD RULE (operator, telegram thread 2259, 2026-09-18): these endpoints
//! MUST obtain the prompt from the ONE prompt-generation path the executor
//! uses - [`crate::agent::context_builder::assemble_prompt`], which calls the
//! configured prompt tool (settings `prompt_generate_tool`, default
//! `prompt__generate`) and assembles its parts. They must NEVER assemble a
//! prompt locally: a preview that silently diverges from the real prompt (for
//! example a hardcoded system string) can return a wrong prompt for years
//! without anyone noticing (telegram threads 2254-2257).
//!
//! Everything here is read-only: `persist_plan` is always `false`, so a
//! preview never writes `threads.plan` (nor any other row), and the initial
//! message list comes from the SAME shared assembly the executor's main loop
//! uses ([`crate::agent::context_builder::initial_prompt_messages`]).

use super::AppState;
use crate::agent::context_builder::{
    assemble_prompt, initial_prompt_messages, messages_to_json, PromptAssemblyDeps, PromptParts,
};
use crate::db::types as queries;
use crate::db::types::{Channel, Message, Thread};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;
use tracing::error;

/// Placeholder used as the `user_message` input when a preview has no real
/// cause message to work with (mirrors the long-standing `<<<prompt>>>`
/// documentation of `GET /prompt/{channel_name}`). It is an INPUT to the
/// prompt tool - never a locally built prompt.
const PLACEHOLDER_USER_MESSAGE: &str = "<<<prompt>>>";

/// Request body of `POST /prompt-preview/{channel_name}`.
#[derive(Deserialize)]
pub(crate) struct PromptPreviewRequest {
    prompt: String,
    /// Force the planning decision on (`plan = true` for the prompt tool).
    /// When false the executor's mapping is used (`null`), so the prompt
    /// tool's own complexity config decides.
    #[serde(default)]
    plan: bool,
}

/// Everything a preview needs, resolved read-only from the DB.
struct PreviewTarget {
    channel: Channel,
    thread: Thread,
    profile_name: String,
}

/// Resolve the preview target: the real channel when the name exists (else a
/// synthetic channel + the default profile, preserving the old semantics) and
/// the channel's latest thread when one exists (else a synthetic thread 0).
///
/// NOTHING is written to the database.
async fn resolve_preview_target(
    state: &Arc<AppState>,
    channel_name: &str,
    user_message: &str,
) -> Result<PreviewTarget, String> {
    let channel = queries::get_channel_by_name(&state.pool, channel_name)
        .await
        .map_err(|e| format!("Database error: {e}"))?;

    let profile_name = match channel.as_ref() {
        Some(ch) if !ch.current_profile.is_empty() => ch.current_profile.clone(),
        _ => state.default_profile.clone(),
    };

    // Latest seq-0 message of the channel -> its thread. That is the thread the
    // next real message would continue, so the preview shows the context that
    // would actually be assembled.
    let mut thread: Option<Thread> = None;
    if let Some(ch) = channel.as_ref() {
        if let Ok(Some(latest)) = queries::get_latest_seq0_message(&state.pool, &ch.id).await {
            if let Ok(Some(tid)) = queries::get_message_thread(&state.pool, latest.id).await {
                thread = crate::db::threads::get_thread_by_id(&state.pool, tid)
                    .await
                    .ok()
                    .flatten();
            }
        }
    }

    let channel = channel.unwrap_or_else(|| synthetic_channel(channel_name, &profile_name));
    let thread = thread.unwrap_or_else(|| synthetic_thread(&channel, &profile_name, user_message));
    Ok(PreviewTarget {
        channel,
        thread,
        profile_name,
    })
}

/// Channel not in the DB: the preview still resolves a profile (the default
/// one) and the channel NAME is the stable identifier the prompt tool gets.
fn synthetic_channel(name: &str, profile_name: &str) -> Channel {
    Channel {
        id: name.to_string(),
        name: name.to_string(),
        current_profile: profile_name.to_string(),
        ..Channel::default()
    }
}

/// Thread not in the DB (channel without history, or a channel-less preview):
/// thread id 0. The prompt tool's thread-scoped lookups simply find nothing.
fn synthetic_thread(channel: &Channel, profile_name: &str, cause: &str) -> Thread {
    Thread {
        id: 0,
        status: "preview".to_string(),
        cause: cause.to_string(),
        channel_id: channel.id.clone(),
        profile: profile_name.to_string(),
        provider: None,
        model: None,
        input_tokens: 0,
        cached_tokens: 0,
        output_tokens: 0,
        duration_ms: 0,
        created_at: chrono::Utc::now(),
        started_at: None,
        ended_at: None,
        terminal: false,
        task_id: None,
        schedule_task_id: None,
        // Undecided: the executor maps `false` to a null plan input, so the
        // prompt tool's own complexity config decides.
        plan: false,
        parent_id: None,
        iterations: 0,
        workflow_step: None,
        template: None,
        toolset: None,
    }
}

/// The preview cause message: an in-memory message carrying the operator's
/// prompt. It is NEVER persisted; the prompt tool receives its content as
/// `user_message`, exactly like the executor receives the real cause message.
fn preview_cause_message(thread_id: i64, content: &str) -> Message {
    Message {
        id: 0,
        thread_id,
        role: "cause".to_string(),
        content: content.to_string(),
        thread_sequence: 0,
        external_id: None,
        metadata: serde_json::json!({}),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "cause".to_string(),
        msg_subtype: None,
        original_thread_id: None,
        created_at: chrono::Utc::now(),
        // Documented 0: in-memory preview message, NEVER persisted (see the doc
        // comment above) - it has no iteration because it has no row.
        iteration_number: 0,
        duration_ms: 0,
        token_usage: serde_json::json!({}),
    }
}

/// Tool names for the preview: the SAME resolution the executor uses
/// (`threads.toolset` intersected with the live plugin registry, full names).
/// A toolset id that is not defined in `config/toolsets.yml` is an error,
/// exactly like in the executor - never a silent "all tools" fallback.
async fn preview_tool_names(state: &Arc<AppState>, thread: &Thread) -> Result<Vec<String>, String> {
    let effective_allowed_tools: Option<Vec<String>> = match thread.toolset.as_deref() {
        None => None,
        Some(id) => match crate::toolsets::load_map(&state.data_dir).get(id) {
            Some(tools) => Some(tools.clone()),
            None => {
                return Err(format!(
                    "toolset '{id}' defined by thread {} is not defined in config/toolsets.yml",
                    thread.id
                ))
            }
        },
    };
    Ok(state
        .plugin_manager
        .snapshot_registry()
        .await
        .allowed_opt(effective_allowed_tools.as_deref())
        .iter()
        .map(|t| t.name.clone())
        .collect())
}

/// Build the prompt through the ONE shared path. Returns an explicit error
/// when the prompt tool cannot be invoked - NEVER a locally assembled
/// fallback prompt (a silent fallback is exactly the divergence this API
/// removes).
async fn build_preview_parts(
    state: &Arc<AppState>,
    target: &PreviewTarget,
    user_message: &str,
    plan_override: Option<bool>,
) -> Result<(PromptParts, Option<String>), String> {
    let tool_names = preview_tool_names(state, &target.thread).await?;
    let cause_msg = preview_cause_message(target.thread.id, user_message);

    // Plan fidelity: the executor maps `thread.plan == true` to Bool(true) and
    // anything else to null (the prompt tool's complexity config decides).
    // POST preview passes the requested value; GET endpoints leave the
    // thread's real value untouched.
    let mut thread = target.thread.clone();
    if let Some(plan) = plan_override {
        thread.plan = plan;
    }

    let deps = PromptAssemblyDeps {
        data_dir: &state.data_dir,
        pool: &state.pool,
        plugin_manager: &state.plugin_manager,
        app_context: &state.app_context,
        prompt_tool_name: state.shared_config.read().prompt_tool_name.clone(),
        // HARD: a preview never writes the plugin's plan decision.
        persist_plan: false,
    };
    assemble_prompt(
        &deps,
        &thread,
        &cause_msg,
        &target.channel,
        &target.profile_name,
        &tool_names,
    )
    .await
    .map_err(|e| format!("prompt generation failed: {e}"))
}

/// The response payload: the REAL prompt parts the prompt tool produced, plus
/// the initial message list the executor builds from those same parts (shared
/// assembly - never a local re-implementation).
fn parts_payload(
    parts: &PromptParts,
    template_section: Option<&str>,
    plan_requested: Option<bool>,
    thread: &Thread,
) -> serde_json::Value {
    let is_step_thread = matches!(thread.workflow_step.as_deref(), Some("testing" | "review"));
    let messages = initial_prompt_messages(parts, template_section, None, is_step_thread);
    let mut payload = serde_json::json!({
        "system": parts.system,
        "memory": parts.memory,
        "context": parts.context,
        "template": template_section,
        "user": parts.user,
        "plan": parts.plan,
        "messages": messages_to_json(&messages),
    });
    if let Some(requested) = plan_requested {
        payload["plan_requested"] = serde_json::json!(requested);
    }
    payload
}

/// `GET /prompt/{channel_name}`: the REAL prompt parts for a channel (the
/// dashboard Memory page's system-prompt card). Read-only.
pub(crate) async fn prompt_handler(
    Path(channel_name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let target = match resolve_preview_target(&state, &channel_name, PLACEHOLDER_USER_MESSAGE).await
    {
        Ok(t) => t,
        Err(e) => {
            error!("Prompt preview: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e })),
            );
        }
    };
    let (parts, template_section) =
        match build_preview_parts(&state, &target, PLACEHOLDER_USER_MESSAGE, None).await {
            Ok(v) => v,
            Err(e) => {
                error!("Prompt preview: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e })),
                );
            }
        };
    (
        StatusCode::OK,
        Json(parts_payload(
            &parts,
            template_section.as_deref(),
            None,
            &target.thread,
        )),
    )
}

/// `POST /prompt-preview/{channel_name}`: preview the full prompt for a
/// channel with the operator's message as the cause. No DB writes.
pub(crate) async fn prompt_preview_handler(
    Path(channel_name): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<PromptPreviewRequest>,
) -> impl IntoResponse {
    let target = match resolve_preview_target(&state, &channel_name, &body.prompt).await {
        Ok(t) => t,
        Err(e) => {
            error!("Prompt preview: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e })),
            );
        }
    };
    let (parts, template_section) =
        match build_preview_parts(&state, &target, &body.prompt, Some(body.plan)).await {
            Ok(v) => v,
            Err(e) => {
                error!("Prompt preview: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e })),
                );
            }
        };
    (
        StatusCode::OK,
        Json(parts_payload(
            &parts,
            template_section.as_deref(),
            Some(body.plan),
            &target.thread,
        )),
    )
}

/// `GET /api/context/{channel_name}`: preview section [3] Context for the
/// latest thread of a channel, read-only. Same shared path as the two
/// endpoints above.
pub(crate) async fn context_preview_handler(
    Path(channel_name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let channel = match queries::get_channel_by_name(&state.pool, &channel_name).await {
        Ok(Some(ch)) => ch,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    serde_json::json!({ "error": format!("Channel '{}' not found", channel_name) }),
                ),
            );
        }
        Err(e) => {
            error!("Failed to look up channel '{}': {:?}", channel_name, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Database error: {}", e) })),
            );
        }
    };

    // The latest seq-0 message is the cause (real content, real retrieval).
    let user_message = match queries::get_latest_seq0_message(&state.pool, &channel.id).await {
        Ok(Some(msg)) => msg.content,
        Ok(None) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({ "context": "", "info": "No messages in this channel" })),
            );
        }
        Err(e) => {
            error!(
                "Failed to get latest message for channel {}: {:?}",
                channel.id, e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Database error: {}", e) })),
            );
        }
    };

    let target = match resolve_preview_target(&state, &channel_name, &user_message).await {
        Ok(t) => t,
        Err(e) => {
            error!("Context preview: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e })),
            );
        }
    };
    let (parts, template_section) =
        match build_preview_parts(&state, &target, &user_message, None).await {
            Ok(v) => v,
            Err(e) => {
                error!("Context preview: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e })),
                );
            }
        };
    (
        StatusCode::OK,
        Json(parts_payload(
            &parts,
            template_section.as_deref(),
            None,
            &target.thread,
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> PromptParts {
        PromptParts {
            system: "SYS".to_string(),
            memory: "MEM".to_string(),
            context: "CTX".to_string(),
            user: "USER".to_string(),
            plan: false,
        }
    }

    /// Regression guard for the operator's hard rule (thread 2259): the API
    /// returns the prompt PARTS the prompt generator produced - `system`
    /// comes from the tool, never from a locally built string - and the
    /// message list is the shared executor assembly.
    #[test]
    fn payload_exposes_the_real_prompt_parts_and_shared_assembly() {
        let channel = synthetic_channel("chan", "prof");
        let thread = synthetic_thread(&channel, "prof", "hello");
        let payload = parts_payload(&parts(), Some("TPL"), Some(false), &thread);

        assert_eq!(payload["system"], "SYS", "system must be the tool's output");
        assert_eq!(payload["memory"], "MEM");
        assert_eq!(payload["context"], "CTX");
        assert_eq!(payload["template"], "TPL");
        assert_eq!(payload["user"], "USER");
        assert_eq!(payload["plan"], false);
        assert_eq!(payload["plan_requested"], false);

        let msgs = payload["messages"].as_array().expect("messages array");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "SYS");
        assert!(msgs.iter().any(|m| m["content"] == "MEM"));
        assert!(msgs.iter().any(|m| m["content"] == "TPL"));
        assert!(msgs.iter().any(|m| m["content"]
            .as_str()
            .unwrap_or("")
            .starts_with("=== Context ===")));
        assert_eq!(msgs.last().unwrap()["role"], "user");
        assert_eq!(msgs.last().unwrap()["content"], "USER");
    }

    /// A preview must never invent a plan decision: the executor maps
    /// `thread.plan == false` to a NULL plan input (the prompt tool decides).
    #[test]
    fn preview_plan_input_follows_the_executor_mapping() {
        let channel = synthetic_channel("chan", "prof");
        let thread = synthetic_thread(&channel, "prof", "hello");
        assert!(!thread.plan);
        let payload = parts_payload(&parts(), None, None, &thread);
        assert!(payload["plan_requested"].is_null());
        assert!(payload.get("plan_requested").is_none() || payload["plan_requested"].is_null());
    }
}
