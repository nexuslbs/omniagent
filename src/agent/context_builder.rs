use crate::error::AppResult;
use tracing::info;

use crate::agent::config::AgentContext;
use crate::agent::helpers;
use crate::db::types::{Channel, Message, Thread};
use crate::mcp::McpToolCall;
use sql_forge::sql_forge;

/// structured-message template. Returns the prompt parts and optional template section.
pub(crate) struct PromptParts {
    pub system: String,
    pub memory: String,
    pub soul: String,
    pub context: String,
    pub user: String,
    pub plan: bool,
}

pub(crate) async fn build_prompt_context(
    cfg: &AgentContext,
    thread: &Thread,
    cause_msg: &Message,
    channel: &Channel,
    profile_name: &str,
    tool_names: &[String],
) -> AppResult<(PromptParts, Option<String>)> {
    let prompt_parts = {
        let prompt_tool_name = cfg.config_snapshot().prompt_tool_name;
        let mcp_call = McpToolCall {
            id: "sys-prompt-gen".to_string(),
            name: prompt_tool_name,
            arguments: serde_json::json!({
                "profile_name": profile_name,
                "platform": channel.platform.as_deref().unwrap_or(""),
                "user_message": cause_msg.content,
                "tool_names": tool_names,
                "thread_id": thread.id,
                "channel_id": thread.channel_id,
                "plan": thread.plan,
            }),
        };
        let result = cfg
            .plugin_manager
            .snapshot_registry()
            .await
            .execute(&mcp_call, cfg.ctx.clone())
            .await?;
        let parsed: serde_json::Value =
            serde_json::from_str(&result.content).unwrap_or(serde_json::json!({}));

        // If the plugin returned a plan decision, persist it to the thread
        if parsed.get("plan").is_some() {
            let plan_val = parsed["plan"].as_bool().unwrap_or(false);
            sql_forge!(
                "UPDATE threads SET plan = :plan WHERE id = :thread_id",
                ( :plan = plan_val, :thread_id = thread.id )
            )
            .execute(&cfg.pool)
            .await?;
        }

        PromptParts {
            system: parsed["system"].as_str().unwrap_or("").to_string(),
            memory: parsed["memory"].as_str().unwrap_or("").to_string(),
            soul: parsed["soul"].as_str().unwrap_or("").to_string(),
            context: parsed["context"].as_str().unwrap_or("").to_string(),
            user: parsed["user"].as_str().unwrap_or("").to_string(),
            plan: parsed
                .get("plan")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }
    };

    let template_section: Option<String> = {
        let msg_type = cause_msg.msg_type.as_str();
        if helpers::is_structured_msg_type(msg_type) {
            let template_name = cause_msg
                .metadata
                .get("template")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            if let Some(template) = template_name {
                let template_path = if template.ends_with(".md") || template.contains('.') {
                    std::path::PathBuf::from(&cfg.ctx.data_dir)
                        .join("profiles")
                        .join(profile_name)
                        .join("templates")
                        .join(template)
                } else {
                    std::path::PathBuf::from(&cfg.ctx.data_dir)
                        .join("profiles")
                        .join(profile_name)
                        .join("templates")
                        .join(format!("{}.md", template))
                };
                let content = if template_path.exists() {
                    std::fs::read_to_string(&template_path)
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                } else {
                    None
                };
                if let Some(ref tmpl) = content {
                    info!(
                        "Loaded template '{}' for thread {} ({} chars)",
                        template,
                        thread.id,
                        tmpl.len()
                    );
                    Some(format!(
                        "=== Task Template ===\nThe following template provides structured guidance for this task type:\n\n{}",
                        tmpl
                    ))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    };

    Ok((prompt_parts, template_section))
}
