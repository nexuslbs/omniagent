use crate::agent::config::AgentContext;
use crate::agent::context_builder::build_prompt_context;
use crate::agent::fail_thread::fail_thread;
use crate::agent::helpers;
use crate::agent::main_loop::run_main_loop;
use crate::db::types as queries;
use crate::db::types::{Message, Thread};
use crate::error::AppResult;
use crate::llm::{LLMClient, LLMConfig, ProviderId};

struct PromptParts {
    system: String,
    memory: String,
    soul: String,
    context: String,
    user: String,
    plan: bool,
}

pub async fn process_thread(
    cfg: &AgentContext,
    thread: &Thread,
    cause_msg: &Message,
) -> AppResult<Message> {
    let start_time = std::time::Instant::now();

    let _current_msg_count = queries::count_thread_messages(&cfg.pool, thread.id)
        .await
        .unwrap_or(0);

    let max_seq = queries::get_max_thread_sequence(&cfg.pool, thread.id)
        .await
        .unwrap_or(0);
    let mut next_seq = max_seq + 1;

    let profile_name = thread.profile.clone();
    let provider_name = thread.provider.clone();
    let model_name = thread.model.clone();

    let profile_registry = crate::profile::ProfileRegistry::new(&cfg.ctx.data_dir);

    if profile_name.is_empty() {
        return fail_thread(
            cfg, thread, cause_msg, &mut next_seq,
            format!(
                "Invalid configuration: profile='{}', provider={:?}, model={:?}: profile name is empty.",
                profile_name, provider_name, model_name
            ),
            "no-profile",
        ).await;
    }

    if profile_registry.get(&profile_name).is_none() {
        return fail_thread(
            cfg, thread, cause_msg, &mut next_seq,
            format!("Invalid configuration: profile='{}' does not exist.", profile_name),
            "invalid-profile",
        ).await;
    }

    if provider_name.as_ref().is_none_or(|s| s.is_empty()) {
        return fail_thread(
            cfg, thread, cause_msg, &mut next_seq,
            format!("Invalid configuration: provider is not set on thread {}.", thread.id),
            "no-provider",
        ).await;
    }

    if model_name.as_ref().is_none_or(|s| s.is_empty()) {
        return fail_thread(
            cfg, thread, cause_msg, &mut next_seq,
            format!("Invalid configuration: model is not set on thread {}.", thread.id),
            "no-model",
        ).await;
    }

    let prof = profile_registry
        .get(&profile_name)
        .cloned()
        .unwrap_or_else(|| crate::profile::Profile::default(&profile_name));

    let provider_name_val = provider_name.clone().unwrap_or_default();
    let model_name_val = model_name.clone().unwrap_or_default();
    let per_thread_llm = {
        let base_url = crate::llm::resolve_default_base_url(&provider_name_val);
        let api_mode = crate::llm::ApiMode::resolve(&provider_name_val, &model_name_val);
        let api_key = match crate::plugins_yaml::get_plugin(&cfg.ctx.data_dir, &provider_name_val) {
            Ok(Some(mut detail)) => {
                crate::plugins_yaml::resolve_config_refs(&mut detail.resolved_env, &cfg.pool).await;
                detail
                    .resolved_env
                    .get("api_key")
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .or_else(|| {
                        detail
                            .config
                            .get("api_key")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(crate::plugins_yaml::resolve_config_value)
                    })
                    .unwrap_or_default()
            }
            _ => String::new(),
        };
        let llm_cfg = LLMConfig {
            provider: ProviderId::new(&provider_name_val),
            api_key,
            base_url,
            model: model_name_val,
            api_mode,
            max_tokens: cfg.config_snapshot().max_tokens,
            temperature: cfg.config_snapshot().temperature,
            supports_reasoning: crate::llm::PROVIDER_METADATA
                .get(&provider_name_val)
                .map(|m| m.supports_reasoning)
                .unwrap_or(false),
        };
        LLMClient::new(llm_cfg)
    };

    let channel = queries::get_channel_by_id(&cfg.pool, thread.channel_id)
        .await?
        .unwrap_or_default();

    if let Some(ref platform_name) = channel.platform {
        if let Some(ref resource) = channel.resource_identifier {
            let parent_id = cause_msg.external_id.clone();
            helpers::enqueue_typing(&cfg.ctx, platform_name, resource, parent_id).await;
        }
    }

    let tool_names: Vec<String> = cfg.plugin_manager.all_tool_names().await;

    let (prompt_parts, template_section) = build_prompt_context(
        cfg, thread, cause_msg, &channel, &profile_name, &tool_names,
    ).await?;
    let saved = run_main_loop(
        cfg, thread, cause_msg, &channel, &profile_name, &tool_names,
        prompt_parts, template_section,
        &mut next_seq, &per_thread_llm, &prof, start_time,
    ).await?;
    Ok(saved)
}
