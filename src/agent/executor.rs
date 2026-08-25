use crate::agent::config::AgentContext;
use crate::agent::context_builder::build_prompt_context;
use crate::agent::fail_thread::fail_thread;
use crate::agent::helpers;
use crate::agent::main_loop::run_main_loop;
use crate::db::types as queries;
use crate::db::types::{Message, Thread};
use crate::error::AppResult;
use crate::llm::{LLMClient, LLMConfig, ProviderId};

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
            cfg,
            thread,
            cause_msg,
            &mut next_seq,
            format!(
                "Invalid configuration: profile='{}' does not exist.",
                profile_name
            ),
            "invalid-profile",
        )
        .await;
    }

    if provider_name.as_ref().is_none_or(|s| s.is_empty()) {
        return fail_thread(
            cfg,
            thread,
            cause_msg,
            &mut next_seq,
            format!(
                "Invalid configuration: provider is not set on thread {}.",
                thread.id
            ),
            "no-provider",
        )
        .await;
    }

    if model_name.as_ref().is_none_or(|s| s.is_empty()) {
        return fail_thread(
            cfg,
            thread,
            cause_msg,
            &mut next_seq,
            format!(
                "Invalid configuration: model is not set on thread {}.",
                thread.id
            ),
            "no-model",
        )
        .await;
    }

    let prof = profile_registry
        .get(&profile_name)
        .cloned()
        .unwrap_or_else(|| crate::profile::Profile::default(&profile_name));

    let provider_name_val = provider_name.clone().unwrap_or_default();
    let model_name_val = model_name.clone().unwrap_or_default();
    let per_thread_llm = {
        let base_url = crate::llm::resolve_default_base_url(&provider_name_val);
        // Effective API mode: models.yml model_config.<model>.api_mode first,
        // else provider-level (models.yml api_mode / plugin manifest).
        let api_mode =
            crate::llm::resolve_model_api_mode_effective(&provider_name_val, &model_name_val);
        // Per-thread effective config from models.yml (model > provider > settings).
        let model_defaults = crate::models_yaml::ModelGlobalDefaults {
            token_budget_soft: cfg.config_snapshot().token_budget_soft,
            token_budget_hard: cfg.config_snapshot().token_budget_hard,
            max_tokens: cfg.config_snapshot().max_tokens,
            max_tokens_on_truncation: cfg.config_snapshot().max_tokens_on_truncation,
        };
        let eff_cfg = crate::models_yaml::resolve_effective(
            &cfg.ctx.data_dir,
            &provider_name_val,
            &model_name_val,
            &model_defaults,
        );
        let api_key = match crate::models_yaml::resolve_models_api_key(
            &cfg.ctx.data_dir,
            &provider_name_val,
            &cfg.pool,
        )
        .await
        {
            Some(k) if !k.is_empty() => k,
            _ => match crate::plugins_yaml::get_plugin(
                &cfg.ctx.data_dir,
                &provider_name_val,
                &crate::plugins_yaml::PluginYamlType::Provider,
            ) {
                Ok(Some(mut detail)) => {
                    crate::plugins_yaml::resolve_config_refs(&mut detail.resolved_env, &cfg.pool)
                        .await;
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
            },
        };
        let llm_cfg = LLMConfig {
            provider: ProviderId::new(&provider_name_val),
            api_key,
            base_url,
            model: model_name_val,
            api_mode,
            max_tokens: eff_cfg.max_tokens.unwrap_or(8192),
            temperature: cfg.config_snapshot().temperature,
            supports_reasoning: eff_cfg.supports_reasoning,
        };
        LLMClient::new(llm_cfg)
    };

    let channel = queries::get_channel_by_id(&cfg.pool, &thread.channel_id)
        .await?
        .unwrap_or_default();

    // ── System-thread seq-0 delivery fix ──
    // For system-originated threads (kanban, cron) whose seq-0 cause message
    // has no external_id yet (created via direct SQL by the kanban dispatcher,
    // which bypasses enqueue_delivery), deliver the cause to the platform FIRST
    // so it creates the root post. Without this, the platform has no thread
    // context and skips every subsequent delivery with
    // "no thread context available (cause_external_id=None, cause_root_id=None)".
    // The platform's deliver response is saved back as the cause external_id
    // (client.rs save-back path), which gives seq-1+ messages a reply target.
    if cause_msg.external_id.is_none() && cause_msg.thread_sequence == 0 {
        if let Some(ref platform_name) = channel.platform {
            if let Some(ref resource) = channel.resource_identifier {
                tracing::info!(
                    "[executor] Delivering seq-0 cause for system thread {} to platform '{}' (resource {})",
                    thread.id,
                    platform_name,
                    resource
                );
                helpers::enqueue_delivery(&cfg.ctx, cause_msg, &channel, thread, None).await;
            }
        }
    }

    if let Some(ref platform_name) = channel.platform {
        if let Some(ref resource) = channel.resource_identifier {
            let parent_id = cause_msg.external_id.clone();
            helpers::enqueue_typing(&cfg.ctx, platform_name, resource, parent_id).await;
        }
    }

    // Tool names surfaced to the prompt must be the SAME set the
    // function-calling schema exposes: the profile's allowed tools, filtered
    // to what is actually registered, using FULL names. Previously this used
    // the unfiltered registry (short `t.name`), so the prompt advertised
    // tools the model could not call (e.g. code_exec) — and names that did
    // not match the schema (builtin_code-exec). Always full names, except
    // plugin-internal references.
    let tool_names: Vec<String> = cfg
        .plugin_manager
        .snapshot_registry()
        .await
        .allowed(&prof.allowed_tools)
        .iter()
        .map(|t| t.full_name.clone())
        .collect();

    let (prompt_parts, template_section) =
        build_prompt_context(cfg, thread, cause_msg, &channel, &profile_name, &tool_names).await?;
    let saved = run_main_loop(
        cfg,
        thread,
        cause_msg,
        &channel,
        &profile_name,
        &tool_names,
        prompt_parts,
        template_section,
        &mut next_seq,
        &per_thread_llm,
        &prof,
        start_time,
    )
    .await?;
    Ok(saved)
}
