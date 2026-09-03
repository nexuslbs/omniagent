use crate::agent::config::AgentContext;
use crate::agent::context_builder::build_prompt_context;
use crate::agent::fail_thread::fail_thread;
use crate::agent::helpers;
use crate::agent::main_loop::run_main_loop;
use crate::db::types as queries;
use crate::db::types::{Message, Thread};
use crate::error::AppResult;
use crate::llm::{LLMClient, LLMConfig, ProviderId};

/// Aborts the periodic typing-indicator task when dropped, so the "typing"
/// signal stops as soon as thread processing ends (every exit path).
struct TypingGuard(tokio::task::JoinHandle<()>);

impl Drop for TypingGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
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

    // Channel row (name, platform, resource) is fetched up front so custom
    // provider headers of type `channel` can resolve to the channel name.
    let channel = queries::get_channel_by_id(&cfg.pool, &thread.channel_id)
        .await?
        .unwrap_or_default();

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

        // Custom per-provider HTTP headers: base layer from the provider
        // plugin config `headers` (same schema as models.yml), then models.yml
        // provider/model headers override per header name. Channel/profile
        // typed values resolve against the current channel and profile.
        let header_specs: Vec<(String, crate::models_yaml::HeaderValue)> = {
            let mut merged: std::collections::BTreeMap<String, crate::models_yaml::HeaderValue> =
                crate::plugins_yaml::provider_plugin_config_headers(
                    &cfg.ctx.data_dir,
                    &provider_name_val,
                );
            for (name, spec) in eff_cfg.headers.clone() {
                merged.insert(name, spec);
            }
            merged.into_iter().collect()
        };
        let extra_headers = crate::models_yaml::resolve_header_specs(
            &header_specs,
            Some(channel.name.as_str()),
            Some(profile_name.as_str()),
        );

        let llm_cfg = LLMConfig {
            provider: ProviderId::new(&provider_name_val),
            api_key,
            base_url,
            model: model_name_val,
            api_mode,
            max_tokens: eff_cfg.max_tokens.unwrap_or(8192),
            temperature: cfg.config_snapshot().temperature,
            supports_reasoning: eff_cfg.supports_reasoning,
            extra_headers,
        };
        LLMClient::new(llm_cfg)
    };

    // ── System-thread seq-0 delivery fix ──
    // For system-originated threads (kanban, cron) whose seq-0 cause message
    // has no REAL platform external_id yet (created via direct SQL by the
    // kanban dispatcher / hooks / scheduler, which bypass enqueue_delivery),
    // deliver the cause to the platform FIRST so it creates the root post.
    // System threads carry either no external_id (kanban) or a synthetic one
    // ("hook:", "cron:", "kanban-action:") that is not a real platform post
    // id; both must be posted automatically by the bot. Without this, the
    // platform has no thread context and skips every subsequent delivery with
    // "no thread context available (cause_external_id=None, cause_root_id=None)".
    // The platform's deliver response is saved back as the cause external_id
    // (client.rs save-back path, which overwrites synthetic ids), giving
    // seq-1+ messages a reply target.
    let seq0_needs_delivery = cause_msg.thread_sequence == 0
        && cause_msg
            .external_id
            .as_deref()
            .is_none_or(helpers::is_synthetic_external_id);
    if seq0_needs_delivery {
        if let Some(ref platform_name) = channel.platform {
            if let Some(ref resource) = channel.resource_identifier {
                tracing::info!(
                    "[executor] Delivering seq-0 cause for system thread {} to platform '{}' (resource {})",
                    thread.id,
                    platform_name,
                    resource
                );
                helpers::enqueue_delivery(&cfg.ctx, cause_msg, &channel, thread, None, false).await;
            }
        }
    }

    // Start a periodic typing indicator while this thread is being processed.
    // Mattermost "is typing..." and Telegram sendChatAction both expire after
    // a few seconds, so the signal is repeated every 5s until processing
    // finishes (the guard aborts the task on every exit path).
    let _typing_guard = if let Some(ref platform_name) = channel.platform {
        if let Some(ref resource) = channel.resource_identifier {
            let parent_id = cause_msg
                .external_id
                .clone()
                .filter(|id| !helpers::is_synthetic_external_id(id));
            let typing_ctx = cfg.ctx.clone();
            let typing_platform = platform_name.clone();
            let typing_resource = resource.clone();
            let typing_handle = tokio::spawn(async move {
                loop {
                    helpers::enqueue_typing(
                        &typing_ctx,
                        &typing_platform,
                        &typing_resource,
                        parent_id.clone(),
                    )
                    .await;
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            });
            Some(TypingGuard(typing_handle))
        } else {
            None
        }
    } else {
        None
    };

    // Tool names surfaced to the prompt must be the SAME set the
    // function-calling schema exposes: the profile's allowed tools, filtered
    // to what is actually registered, using FULL names. Previously this used
    // the unfiltered registry (short `t.name`), so the prompt advertised
    // tools the model could not call and names that did not match the
    // schema. Always full names, except plugin-internal references.
    let tool_names: Vec<String> = cfg
        .plugin_manager
        .snapshot_registry()
        .await
        .allowed(&prof.allowed_tools)
        .iter()
        .map(|t| t.name.clone())
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

#[cfg(test)]
mod tests {
    use crate::agent::helpers::{is_synthetic_external_id, needs_cause_external_id_lookup};

    // ─── is_synthetic_external_id tests ───

    #[test]
    fn test_is_synthetic_external_id_hook_prefix() {
        // Real-world shape from hooks.rs: "hook:{id}:{ts}"
        assert!(is_synthetic_external_id(
            "hook:wiki-maintenance:1788016954478"
        ));
        assert!(is_synthetic_external_id("hook:123:456"));
    }

    #[test]
    fn test_is_synthetic_external_id_cron_prefix() {
        // Real-world shape from scheduler.rs: "cron:{job}:{ts}"
        assert!(is_synthetic_external_id("cron:daily_report:1788016954478"));
        assert!(is_synthetic_external_id("cron:0:1"));
    }

    #[test]
    fn test_is_synthetic_external_id_kanban_action_prefix() {
        // Real-world shape from kanban_action.rs: "kanban-action:{task}:{step}:{ts}"
        assert!(is_synthetic_external_id(
            "kanban-action:task_abc:step:12345"
        ));
    }

    #[test]
    fn test_is_synthetic_external_id_real_platform_ids_false() {
        // A real Mattermost post id (26 chars, exactly like the channel ids in
        // channels.yml) is NOT synthetic and must never be re-delivered.
        assert!(!is_synthetic_external_id("3nt7qohominz9fxmz7bcujms9c"));
        // A Telegram message id is NOT synthetic either.
        assert!(!is_synthetic_external_id("123456789:abcdefg"));
        // UUID-style ids (some plugins) are not synthetic.
        assert!(!is_synthetic_external_id(
            "550e8400-e29b-41d4-a716-446655440000"
        ));
    }

    #[test]
    fn test_is_synthetic_external_id_edge_cases_false() {
        assert!(!is_synthetic_external_id(""));
        assert!(!is_synthetic_external_id("hook")); // prefix must include the colon
        assert!(!is_synthetic_external_id("hookish:123")); // must not prefix-match substrings
        assert!(!is_synthetic_external_id("HOOK:123")); // matching is case-sensitive
        assert!(!is_synthetic_external_id("cronjob:123"));
        assert!(!is_synthetic_external_id("kanban:123")); // kanban threads use None, not "kanban:"
    }

    // ─── seq-0 delivery decision tests ───

    #[test]
    fn test_seq0_delivery_decision_matches_executor_predicate() {
        // Mirrors the exact predicate in process_thread:
        //   cause_msg.thread_sequence == 0
        //     && cause_msg.external_id.as_deref().is_none_or(is_synthetic_external_id)
        let needs_delivery =
            |seq: i64, ext: Option<&str>| seq == 0 && ext.is_none_or(is_synthetic_external_id);
        // Kanban workflow threads: no external_id -> seq-0 must be delivered.
        assert!(needs_delivery(0, None));
        // Hooks/cron/kanban-action threads: synthetic id -> seq-0 must be delivered.
        assert!(needs_delivery(0, Some("hook:1:2")));
        assert!(needs_delivery(0, Some("cron:1:2")));
        assert!(needs_delivery(0, Some("kanban-action:1:2")));
        // User-originated threads: real platform post id -> never re-delivered
        // (no double post regression).
        assert!(!needs_delivery(0, Some("3nt7qohominz9fxmz7bcujms9c")));
        // Only the seq-0 message is delivered here; replies are threaded.
        assert!(!needs_delivery(1, None));
        assert!(!needs_delivery(1, Some("hook:1:2")));
        assert!(!needs_delivery(2, Some("cron:1:2")));
    }
    // needs_cause_external_id_lookup tests (shared follow-up resolution)

    #[test]
    fn test_needs_lookup_kanban_none_id() {
        // Kanban threads pass None: follow-ups must resolve the real post id.
        assert!(needs_cause_external_id_lookup(None, 1));
        assert!(needs_cause_external_id_lookup(None, 5));
    }

    #[test]
    fn test_needs_lookup_synthetic_ids() {
        // Cron/hooks/kanban-action threads pass a synthetic id before their
        // seq-0 is posted; follow-ups must resolve the real post id too.
        assert!(needs_cause_external_id_lookup(Some("hook:1:2"), 1));
        assert!(needs_cause_external_id_lookup(Some("cron:job:3"), 2));
        assert!(needs_cause_external_id_lookup(
            Some("kanban-action:t:s:4"),
            1
        ));
    }

    #[test]
    fn test_needs_lookup_seq0_never() {
        // seq-0 posts top-level: it never needs a reply-target lookup.
        assert!(!needs_cause_external_id_lookup(None, 0));
        assert!(!needs_cause_external_id_lookup(Some("hook:1:2"), 0));
    }

    #[test]
    fn test_needs_lookup_real_id_no_lookup() {
        // A real platform post id (user thread) is used as-is.
        assert!(!needs_cause_external_id_lookup(
            Some("3nt7qohominz9fxmz7bcujms9c"),
            1
        ));
    }
}
