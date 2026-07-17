use crate::err_msg;
use crate::error::AppResult;
use tracing::{error, info, warn};

use crate::agent::config::AgentContext;
use crate::agent::helpers;
use crate::db::types as queries;
use crate::db::types::{CompleteThreadStats, Message, MessageNew, Thread};
use crate::llm::{ChatMessage, CompletionRequest, LLMClient, LLMConfig, ProviderId, Usage};
use crate::mcp::{truncate_content, McpToolCall, McpToolResult, WatchdogAction, DEFAULT_MAX_TOOL_OUTPUT_CHARS};
use tokio::task::JoinSet;

/// Process a single pending thread through the state machine:
///
/// 1. Claim the thread (status → 'processing')
/// 2. Get current message count for the thread
/// 3. Resolve profile, provider, model from thread
/// 4. Call the LLM with system + user messages (and tools if enabled)
/// 5. If tool calls are returned, execute them and loop back to LLM
/// 6. If reasoning exists, save as a separate `reasoning` record
/// 7. Save the main agent response (msg_type: `message`)
/// 8. Generate a per-turn summary (outside iteration limit)
/// 9. Update thread status → completed/failed, record token_usage + duration
/// 10. Trigger cross-thread summary if enough threads have accumulated
pub async fn process_thread(
    cfg: &AgentContext,
    thread: &Thread,
    cause_msg: &Message,
) -> AppResult<Message> {
    let start_time = std::time::Instant::now();

    // 1. Mark the thread as 'processing' (already done by claim_thread, but verify)
    // The claim_thread function already set status='processing' and started_at=NOW()

    // 2. Get current message count for this thread
    let _current_msg_count = queries::count_thread_messages(&cfg.pool, thread.id)
        .await
        .unwrap_or(0);

    // Track per-message sequence number within the thread
    // Query the max sequence so each new message gets a unique incrementing value.
    let max_seq = queries::get_max_thread_sequence(&cfg.pool, thread.id)
        .await
        .unwrap_or(0);
    let mut next_seq = max_seq + 1;

    // 3. Read profile, provider, model from the thread (not from messages)
    let profile_name = thread.profile.clone();
    let provider_name = thread.provider.clone();
    let model_name = thread.model.clone();

    let profile_registry = crate::profile::ProfileRegistry::new(&cfg.ctx.data_dir);

    // 3a. Check profile name is present
    if profile_name.is_empty() {
        let err_msg = MessageNew {
            thread_id: thread.id,
            role: "system".to_string(),
            content: format!(
                "Invalid configuration: profile='{}', provider={:?}, model={:?}: profile name is empty. Set a profile on the channel or thread.",
                profile_name, provider_name, model_name
            ),
            thread_sequence: {  next_seq },
            external_id: Some(format!("validation-error:{}:{}", thread.id, chrono::Utc::now().timestamp())),
            metadata: serde_json::json!({
                "error_type": "configuration",
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "error".to_string(),
            msg_subtype: Some("no-profile".to_string()),
            iteration_number: 0,
        };
        let saved = queries::create_message(&cfg.pool, &err_msg).await?;
        let _ = queries::complete_thread(
            &cfg.pool,
            thread.id,
            "failed",
            CompleteThreadStats {
                input_tokens: 0,
                cached_tokens: 0,
                output_tokens: 0,
                duration_ms: 0,
            },
        )
        .await;
        // Deliver the error message back to the user's platform
        if let Ok(Some(channel)) = queries::get_channel_by_id(&cfg.pool, thread.channel_id).await {
            helpers::enqueue_delivery(
                &cfg.ctx,
                &saved,
                &channel,
                thread,
                cause_msg.external_id.clone(),
            )
            .await;
        }
        return Ok(saved);
    }

    // 3b. Check profile exists
    if profile_registry.get(&profile_name).is_none() {
        let err_msg = MessageNew {
            thread_id: thread.id,
            role: "system".to_string(),
            content: format!(
                "Invalid configuration: profile='{}' does not exist.",
                profile_name
            ),
            thread_sequence: { next_seq },
            external_id: Some(format!(
                "validation-error:{}:{}",
                thread.id,
                chrono::Utc::now().timestamp()
            )),
            metadata: serde_json::json!({
                "error_type": "configuration",
                "original_thread_id": thread.id,
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "error".to_string(),
            msg_subtype: Some("invalid-profile".to_string()),
            iteration_number: 0,
        };
        let saved = queries::create_message(&cfg.pool, &err_msg).await?;
        let _ = queries::complete_thread(
            &cfg.pool,
            thread.id,
            "failed",
            CompleteThreadStats {
                input_tokens: 0,
                cached_tokens: 0,
                output_tokens: 0,
                duration_ms: 0,
            },
        )
        .await;
        // Deliver the error message back to the user's platform
        if let Ok(Some(channel)) = queries::get_channel_by_id(&cfg.pool, thread.channel_id).await {
            helpers::enqueue_delivery(
                &cfg.ctx,
                &saved,
                &channel,
                thread,
                cause_msg.external_id.clone(),
            )
            .await;
        }
        return Ok(saved);
    }

    // 3c. Check provider is set on the thread
    if provider_name.as_ref().is_none_or(|s| s.is_empty()) {
        let err_msg = MessageNew {
            thread_id: thread.id,
            role: "system".to_string(),
            content: format!(
                "Invalid configuration: provider is not set on thread {}. Ensure the thread has a provider stamped at creation time. Check channel.current_provider, profile provider, or LLM_PROVIDER env var.",
                thread.id
            ),
            thread_sequence: {  next_seq },
            external_id: Some(format!("validation-error:{}:{}", thread.id, chrono::Utc::now().timestamp())),
            metadata: serde_json::json!({
                "error_type": "configuration",
                "original_thread_id": thread.id,
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "error".to_string(),
            msg_subtype: Some("no-provider".to_string()),
            iteration_number: 0,
        };
        let saved = queries::create_message(&cfg.pool, &err_msg).await?;
        let _ = queries::complete_thread(
            &cfg.pool,
            thread.id,
            "failed",
            CompleteThreadStats {
                input_tokens: 0,
                cached_tokens: 0,
                output_tokens: 0,
                duration_ms: 0,
            },
        )
        .await;
        // Deliver the error message back to the user's platform
        if let Ok(Some(channel)) = queries::get_channel_by_id(&cfg.pool, thread.channel_id).await {
            helpers::enqueue_delivery(
                &cfg.ctx,
                &saved,
                &channel,
                thread,
                cause_msg.external_id.clone(),
            )
            .await;
        }
        return Ok(saved);
    }

    // 3d. Check model is set on the thread
    if model_name.as_ref().is_none_or(|s| s.is_empty()) {
        let err_msg = MessageNew {
            thread_id: thread.id,
            role: "system".to_string(),
            content: format!(
                "Invalid configuration: model is not set on thread {}. Ensure the thread has a model stamped at creation time. Check channel.current_model, profile model, or provider plugin default_model.",
                thread.id
            ),
            thread_sequence: {  next_seq },
            external_id: Some(format!("validation-error:{}:{}", thread.id, chrono::Utc::now().timestamp())),
            metadata: serde_json::json!({
                "error_type": "configuration",
                "original_thread_id": thread.id,
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "error".to_string(),
            msg_subtype: Some("no-model".to_string()),
            iteration_number: 0,
        };
        let saved = queries::create_message(&cfg.pool, &err_msg).await?;
        let _ = queries::complete_thread(
            &cfg.pool,
            thread.id,
            "failed",
            CompleteThreadStats {
                input_tokens: 0,
                cached_tokens: 0,
                output_tokens: 0,
                duration_ms: 0,
            },
        )
        .await;
        // Deliver the error message back to the user's platform
        if let Ok(Some(channel)) = queries::get_channel_by_id(&cfg.pool, thread.channel_id).await {
            helpers::enqueue_delivery(
                &cfg.ctx,
                &saved,
                &channel,
                thread,
                cause_msg.external_id.clone(),
            )
            .await;
        }
        return Ok(saved);
    }

    // Validation passed: load the profile for its settings (auto_retrieval_enabled, etc.)
    let prof = profile_registry
        .get(&profile_name)
        .cloned()
        .unwrap_or_else(|| crate::profile::Profile::default(&profile_name));

    // Use provider/model directly from the thread stamp (no fallback chain)
    let provider_name_val = provider_name.clone().unwrap_or_default();
    let model_name_val = model_name.clone().unwrap_or_default();
    // Create a per-thread LLM client using the thread's provider/model
    // (not the shared agent-level one which uses the env default provider).
    let per_thread_llm = {
        let base_url = crate::llm::resolve_default_base_url(&provider_name_val);
        let api_mode = crate::llm::ApiMode::resolve(&provider_name_val, &model_name_val);
        let api_key = match crate::plugins_yaml::get_plugin(&cfg.ctx.data_dir, &provider_name_val) {
            Ok(Some(mut detail)) => {
                // Resolve $secret: references in resolved_env for full resolution
                crate::plugins_yaml::resolve_config_refs(&mut detail.resolved_env, &cfg.pool).await;
                // Check resolved_env first (has all refs resolved), then fall back to config
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
        };
        LLMClient::new(llm_cfg)
    };

    // Query channel metadata for context assembly
    let channel = queries::get_channel_by_id(&cfg.pool, thread.channel_id)
        .await?
        .unwrap_or_default();

    // 4. Build the initial message history via the MCP prompt plugin
    // The plugin returns 5 parts: system, memory, soul, context, user
    // Omniagent assembles these parts into the message array
    let tool_names: Vec<String> = cfg
        .mcp
        .read()
        .await
        .all()
        .iter()
        .map(|t| t.name.clone())
        .collect();

    struct PromptParts {
        system: String,
        memory: String,
        soul: String,
        context: String,
        user: String,
        plan: bool,
    }



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
            .mcp
            .read()
            .await
            .execute(&mcp_call, cfg.ctx.clone())
            .await?;
        let parsed: serde_json::Value = serde_json::from_str(&result.content)
            .unwrap_or(serde_json::json!({}));

        // If the plugin returned a plan decision, persist it to the thread
        if parsed.get("plan").is_some() {
            let plan_val = parsed["plan"].as_bool().unwrap_or(false);
            sqlx::query("UPDATE threads SET plan = $1 WHERE id = $2")
                .bind(plan_val)
                .bind(thread.id)
                .execute(&cfg.pool)
                .await?;
        }

        PromptParts {
            system: parsed["system"].as_str().unwrap_or("").to_string(),
            memory: parsed["memory"].as_str().unwrap_or("").to_string(),
            soul: parsed["soul"].as_str().unwrap_or("").to_string(),
            context: parsed["context"].as_str().unwrap_or("").to_string(),
            user: parsed["user"].as_str().unwrap_or("").to_string(),
            plan: parsed.get("plan").and_then(|v| v.as_bool()).unwrap_or(false),
        }
    };    // 4a. Load template from cause message metadata (for kanban/cron/user tasks)
    let template_section: Option<String> = {
        let msg_type = cause_msg.msg_type.as_str();
        if msg_type == "kanban" || msg_type == "cron" || msg_type == "Cause" {
            let template_name = cause_msg
                .metadata
                .get("template")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            if let Some(template) = template_name {
                let template_path = if template.ends_with(".md") || template.contains('.') {
                    std::path::PathBuf::from(&cfg.ctx.data_dir)
                        .join("profiles")
                        .join(&profile_name)
                        .join("templates")
                        .join(template)
                } else {
                    std::path::PathBuf::from(&cfg.ctx.data_dir)
                        .join("profiles")
                        .join(&profile_name)
                        .join("templates")
                        .join(format!("{}.md", template))
                };
                let content = if template_path.exists() {
                    std::fs::read_to_string(&template_path).ok()
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


    // Track cumulative token usage across all LLM calls
    let mut cumulative_usage: Option<crate::llm::Usage> = None;
    let mut force_failed: bool = false;
    let mut current_iter: i32;

    // ── Planning Phase ──
    // Plan is a boolean resolved at thread creation time.
    // When true, the agent runs a planning iteration before the main loop.
    // The planning prompt itself is generated by the prompt plugin
    // the executor just orchestrates the calls.
    let should_plan = thread.plan;

    // Whether subtask tools are enabled for the main loop
    let enable_subtasks = should_plan;

    // Pre-read prompt log level for consistency across planning and main loop
    let prompt_log_level = cfg.config_snapshot().prompt_log_level;
    let prompt_log_level = prompt_log_level.as_str();
    let mut has_logged_first_prompt = false;

    let plan_content: Option<String> = if should_plan {
        let max_iter = 0; // one-shot, no refinement iterations
        let max_tokens = 2048u32; // planning token limit: previously from config
        let mut last_plan: Option<String> = None;
        let mut json_failure_count: u32 = 0;
        let mut json_error_msg: Option<String> = None;

        for iter in 0..(max_iter + 1) {
            // Build planning messages from prompt parts
            // User's request goes in context; planning instruction goes in user
            let mut planning_messages = vec![ChatMessage::system(&prompt_parts.system)];
            if !prompt_parts.memory.is_empty() {
                planning_messages.push(ChatMessage::system(&prompt_parts.memory));
            }
            if !prompt_parts.soul.is_empty() {
                planning_messages.push(ChatMessage::system(&prompt_parts.soul));
            }
            if !prompt_parts.context.is_empty() {
                planning_messages.push(ChatMessage::system(&format!(
                    "=== Context ===\n{}", prompt_parts.context
                )));
            }
            // Inject the task template so the plan is aware of the instructions
            if let Some(ref ts) = template_section {
                planning_messages.push(ChatMessage::system(ts));
            }
            if let Some(ref err) = json_error_msg {
                planning_messages.push(ChatMessage::system(err));
            }
            // Planning instruction as user message
            let tool_list = if tool_names.is_empty() {
                String::new()
            } else {
                format!("Your available tools: {}.", tool_names.join(", "))
            };
            let planning_prompt = if iter == 0 {
                format!(
                    "## Plan\nBefore responding, create a high-level plan with numbered steps. \
{tool_list}\nBe specific about which tool to use and what parameters to pass. \
Aim for the minimum number of steps to complete the task. \
Wrap your plan in a <plan> block. After delivering the final answer, \
evaluate: if the task was completed, call the completion tool."
                )
            } else {
                format!(
                    "## Revised Plan (iteration {}/{})\n\
Your previous plan did not fully complete the task. \
Review what was done vs what remains. Identify the specific \
blockage and create a revised plan. Each step must include \
which tool to use and what parameters.\n\n\
Previous plan:\n{}",
                    iter + 1,
                    max_iter,
                    last_plan.as_deref().unwrap_or("(none)")
                )
            };
            planning_messages.push(ChatMessage::user(&planning_prompt));

            // ── Optional: insert prompt message before planning LLM call ──
            // Logs the prompt *sent to* the LLM (not the returned plan, which is
            // already saved as a separate msg_type="plan" message). Does NOT count
            // as "the first prompt" for main-loop tracking: the main loop's
            // system prompt + context is the important one for debugging.
            // Subtype "plan" indicates this is the first prompt to create a plan.
            if prompt_log_level != "off" {
                let prompt_seq = {
                    let v = next_seq;
                    next_seq += 1;
                    v
                };
                let prompt_content =
                    serde_json::to_string(&planning_messages).unwrap_or_else(|_| String::new());
                let prompt_msg = MessageNew {
                    thread_id: thread.id,
                    role: "system".to_string(),
                    content: prompt_content,
                    thread_sequence: prompt_seq,
                    external_id: None,
                    metadata: serde_json::json!({
                        "prompt_log_level": prompt_log_level,
                        "prompt_subtype": "plan",
                        "num_messages": planning_messages.len(),
                    }),
                    embedding: None,
                    summary_text: None,
                    is_summary: false,
                    msg_type: "prompt".to_string(),
                    msg_subtype: Some("plan".to_string()),
                    iteration_number: 0,
                };
                if let Err(e) = queries::create_message(&cfg.pool, &prompt_msg).await {
                    warn!(
                        "[prompt] Failed to persist planning prompt for thread {}: {:?}",
                        thread.id, e
                    );
                }
            }

            let plan_request = CompletionRequest {
                messages: planning_messages,
                max_tokens,
                temperature: 0.3,
                stream: false,
                tools: None,
            };

            match per_thread_llm.completion(plan_request).await {
                Ok(resp) => {
                    helpers::merge_usage(&mut cumulative_usage, resp.usage.clone());
                    // Use reasoning as fallback when plan content is empty (e.g. DeepSeek
                    // puts everything in reasoning/thinking and leaves content empty).
                    let plan_content = if !resp.content.is_empty() {
                        resp.content.clone()
                    } else if let Some(ref r) = resp.reasoning {
                        if !r.is_empty() {
                            r.clone()
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };

                    info!(
                        "[plan] Generated plan for thread {} ({} chars from field '{}', iteration {}/{})",
                        thread.id,
                        plan_content.len(),
                        if !resp.content.is_empty() { "content" } else if resp.reasoning.as_ref().is_some_and(|r| !r.is_empty()) { "reasoning" } else { "empty" },
                        iter + 1,
                        max_iter + 1,
                    );

                    // Save the plan as a plan-type message (skip if both content and reasoning are empty)
                    if !plan_content.is_empty() {
                        let plan_msg = MessageNew {
                            thread_id: thread.id,
                            role: "agent".to_string(),
                            content: plan_content.clone(),
                            thread_sequence: {
                                let v = next_seq;
                                next_seq += 1;
                                v
                            },
                            external_id: None,
                            metadata: serde_json::json!({
                                "plan_iteration": iter,
                                "plan_accepted": iter == 0 && max_iter == 0,
                            }),
                            embedding: None,
                            summary_text: None,
                            is_summary: false,
                            msg_type: "plan".to_string(),
                            msg_subtype: Some("markdown".to_string()),
                            iteration_number: 1,
                        };
                        match queries::create_message(&cfg.pool, &plan_msg).await {
                            Ok(_) => {}
                            Err(e) => warn!(
                                "[plan] Failed to persist plan for thread {}: {:?}",
                                thread.id, e
                            ),
                        }
                    }

                    // Mark first prompt as already logged so the main loop doesn't log
                    // a duplicate "first" prompt that includes the plan content as context.
                    // The planning prompt (msg_subtype="plan") and the plan message itself
                    // already serve as the record: the main-loop "first" prompt would just
                    // embed the plan text again, duplicating what's already saved.
                    has_logged_first_prompt = true;

                    // For complex tasks, auto-create subtasks from JSON plan content
                    if enable_subtasks && plan_content.len() > 100 {
                        let max_json_retries: u32 =
                            cfg.config_snapshot().max_unfinished_subtask_retries;
                        match serde_json::from_str::<serde_json::Value>(&plan_content) {
                            Ok(plan_json) => {
                                if let Some(steps) =
                                    plan_json.get("steps").and_then(|v| v.as_array())
                                {
                                    // Valid JSON with steps: create subtasks
                                    let total = steps.len().min(6);
                                    for (i, step_val) in steps.iter().enumerate().take(6) {
                                        if let Some(step) = step_val.as_str() {
                                            let clean =
                                                step.trim().trim_end_matches(['*', '`']).trim();
                                            if !clean.is_empty() {
                                                let priority = (total - i) as i32;
                                                if let Err(e) = crate::subtask::add_subtask(
                                                    &cfg.pool, thread.id, clean, priority,
                                                )
                                                .await
                                                {
                                                    warn!("[plan] Failed to create subtask '{}': {:?}", clean, e);
                                                } else {
                                                    info!("[plan] Created subtask '{}' for complex thread {}", clean, thread.id);
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    // JSON valid but missing "steps" field
                                    json_failure_count += 1;
                                    if json_failure_count > max_json_retries {
                                        warn!("[plan] JSON validation exhausted: missing 'steps' field after {} retries for thread {}", max_json_retries, thread.id);
                                        force_failed = true;
                                        break;
                                    }
                                    json_error_msg = Some(format!(
                                        "ERROR: Your plan JSON is missing the required \"steps\" array. \
                                         You MUST return a JSON object with \"description\" (string) and \"steps\" (array of strings). \
                                         No surrounding markdown, no backticks, no extra text. \
                                         Attempt {}/{}: fix the JSON or the thread will fail.",
                                        json_failure_count, max_json_retries
                                    ));
                                    last_plan = Some(plan_content.clone());
                                    continue;
                                }
                            }
                            Err(e) => {
                                // Invalid JSON syntax
                                json_failure_count += 1;
                                if json_failure_count > max_json_retries {
                                    warn!("[plan] JSON validation exhausted: invalid JSON after {} retries for thread {}: {}", max_json_retries, thread.id, e);
                                    force_failed = true;
                                    break;
                                }
                                json_error_msg = Some(format!(
                                    "ERROR: Your response was not valid JSON. Parsing error: {}. \
                                     You MUST return a valid JSON object with \"description\" and \"steps\" fields. \
                                     No surrounding markdown, no backticks, no extra text. \
                                     Attempt {}/{}: fix the JSON or the thread will fail.",
                                    e, json_failure_count, max_json_retries
                                ));
                                last_plan = Some(plan_content.clone());
                                continue;
                            }
                        }
                    }

                    last_plan = Some(plan_content);

                    // One-shot: no refinement iterations: plan is final
                    break;
                }
                Err(e) => {
                    warn!(
                        "[plan] Failed to generate plan for thread {}: {:?}",
                        thread.id, e
                    );
                    break;
                }
            }
        }

        last_plan
    } else {
        None
    };

    // 5. Assemble messages from prompt parts
    let mut messages = vec![ChatMessage::system(&prompt_parts.system)];
    if !prompt_parts.memory.is_empty() {
        messages.push(ChatMessage::system(&prompt_parts.memory));
    }
    if !prompt_parts.soul.is_empty() {
        messages.push(ChatMessage::system(&prompt_parts.soul));
    }

    // Inject task template FIRST (right after system prompt): highest instruction priority
    // for template-backed tasks (kanban/cron with template).
    // Flush-left position ensures the template guides the model before any other context.
    if let Some(ref template_section) = template_section {
        messages.push(ChatMessage::system(template_section));
    }

    // Add context from plugin as system message (before the user message)
    if !prompt_parts.context.is_empty() {
        messages.push(ChatMessage::system(&format!(
            "=== Context ===\n{}",
            prompt_parts.context
        )));
    }

    // Inject the plan as execution context if one was generated
    if let Some(ref plan) = plan_content {
        messages.push(ChatMessage::system(&format!(
            "=== Generated Plan (use as guidance) ===\n\
             A plan was generated for the current task. Follow it unless tool results \
             contradict it. Do NOT explore alternative approaches that the plan already \
             considered: adapt only when necessary.\n\n{}",
            plan
        )));
        info!(
            "[plan] Injected plan as context for thread {} ({} chars)",
            thread.id,
            plan.len(),
        );
    }

    // Add the user message (from the prompt parts: the plugin provides this)
    messages.push(ChatMessage::user(&prompt_parts.user));

    // 5. Build tool definitions from the profile's allowed tools
    let tools_def = cfg.mcp.read().await.to_openai_tools(&prof.allowed_tools);

    // 6. Tool-calling loop: max iterations controls total LLM calls
    let iter_limit =
        queries::max_iterations_for_plan(&cfg.config_snapshot(), thread.plan)
            as i32;
    // The plan phase consumed 1 iteration (if it ran). Subtract it so the
    // tool-calling loop gets the remaining budget.
    let plan_consumed = if should_plan { 1 } else { 0 };
    let max_llm_calls = (iter_limit - plan_consumed).max(0) as u32;
    let mut final_content = String::new();
    let mut final_reasoning: Option<String> = None;
    let mut final_tool_call: bool = false;
    let mut limit_reached: bool = false;
    let mut _last_response_usage: Option<Usage> = None;
    current_iter = plan_consumed; // 0 for prompt_only, 1 if plan already ran
    let mut unfinished_subtask_retries: u32 = 0;
    let mut calls_since_subtask_management: u32 = 0;
    // Track when condensation last occurred so soft-budget triggers use
    // iteration-since-last-condense rather than a fixed modulo schedule.
    // This prevents aggressive condensation on every Nth iteration even when
    // the last condense just happened.
    let mut last_condense_iteration: i32 = 0;

    for _turn in 0..max_llm_calls {
        current_iter += 1; // increment before each LLM call

        // If this LLM call will reach the iteration limit, hint to the model
        // to produce a final answer rather than more tool calls.
        if current_iter >= iter_limit {
            messages.push(ChatMessage::system(
                "This is your last turn. You must provide your final answer now. \
                 Do not request additional tool calls.",
            ));
        }

        // ── Context management: budget enforcement ──
        // Soft budget exceeded → condense every state_block_update_interval turns
        // Hard budget exceeded → condense before ANY LLM call, bring below soft
        //
        // Uses tiktoken BPE tokenizer for accurate token counting instead of
        // character-based estimation (which had ~30% JSON overhead error margin).

        let cfg_snapshot = cfg.config_snapshot();
        let prompt_token_soft = cfg_snapshot.prompt_token_budget_soft;
        let prompt_token_hard = cfg_snapshot.prompt_token_budget_hard;
        let old_msg_budget = cfg_snapshot.old_message_char_budget;
        let keep_turns = cfg_snapshot.condense_keep_turns;
        let state_interval = cfg_snapshot.state_block_update_interval as i32;
        let tokenizer_enc = cfg_snapshot.tokenizer_encoding.clone();

        // Count actual tokens using tiktoken (fast BPE, no network calls),
        // then apply safety factor to account for provider tokenizer mismatch.
        // Include tool definitions in the count since they add 200-300K tokens.
        let token_tools = if tools_def.is_empty() {
            None
        } else {
            Some(tools_def.as_slice())
        };
        let raw_tokens = helpers::count_tokens(&messages, &tokenizer_enc, token_tools);
        let safety_factor = cfg_snapshot.prompt_token_safety_factor;
        let current_tokens = (raw_tokens as f64 * safety_factor) as usize;

        // Log token count every 5 iterations for diagnostics
        if current_iter % 5 == 0 || current_tokens > 50000 {
            info!(
                "[context] Iteration {}: ~{} raw tokens (×{:.1} factor = {} effective) in {} messages (soft: {}, hard: {})",
                current_iter, raw_tokens, safety_factor, current_tokens, messages.len(), prompt_token_soft, prompt_token_hard,
            );
        }

        let needs_hard_condense = current_tokens > prompt_token_hard;
        let needs_soft_condense = current_tokens > prompt_token_soft
            && state_interval > 0
            && (current_iter - last_condense_iteration) >= state_interval;

        if needs_hard_condense || needs_soft_condense {
            // Use the char budget for the condense_messages function's safety
            // check (which compares system message CHARS, not tokens).
            let condense_char_soft = cfg_snapshot.prompt_char_budget_soft;

            match helpers::condense_messages(
                std::mem::take(&mut messages),
                old_msg_budget,
                keep_turns,
                condense_char_soft,
            ) {
                Ok(condensed) => {
                    let condensed_raw =
                        helpers::count_tokens(&condensed, &tokenizer_enc, token_tools);
                    let condensed_tokens = (condensed_raw as f64 * safety_factor) as usize;
                    let saved = current_tokens.saturating_sub(condensed_tokens);
                    messages = condensed;

                    // If hard budget triggered, verify we're now below soft
                    let after_raw = helpers::count_tokens(&messages, &tokenizer_enc, token_tools);
                    let after_tokens = (after_raw as f64 * safety_factor) as usize;
                    if needs_hard_condense && after_tokens > prompt_token_soft {
                        // Second pass with more aggressive settings
                        // Use the configured keep_turns (not hardcoded 1) so the
                        // escalation is actually more aggressive than the first pass.
                        let aggressive_keep = if keep_turns > 0 {
                            keep_turns - 1
                        } else {
                            0_usize
                        };
                        match helpers::condense_messages(
                            std::mem::take(&mut messages),
                            old_msg_budget / 2, // halve the old message budget
                            aggressive_keep,    // at most keep_turns-1, but at least 0
                            condense_char_soft,
                        ) {
                            Ok(tighter) => {
                                let tighter_raw =
                                    helpers::count_tokens(&tighter, &tokenizer_enc, token_tools);
                                let tighter_tokens = (tighter_raw as f64 * safety_factor) as usize;
                                messages = tighter;
                                if tighter_tokens > prompt_token_soft {
                                    warn!(
                                        "[context] Hard condensation could not bring prompt below soft budget: {} effective tokens (budget: {})",
                                        tighter_tokens, prompt_token_soft
                                    );
                                    // Template is NOT stripped: even a small template (~600 tokens)
                                    // is critical for task instructions. Stripping it saves negligible
                                    // context but loses the entire task specification.
                                    // The context will self-resolve as old tool results get pruned
                                    // in subsequent iterations.
                                }
                            }
                            Err(e) => {
                                warn!("[context] Second-pass condensation failed: {}", e);
                            }
                        }
                    }

                    info!(
                        "[context] Condensed prompt: {} effective tokens [raw: {}] → {} effective tokens [raw: {}] (saved {}, iteration {})",
                        current_tokens, raw_tokens,
                        condensed_tokens, condensed_raw,
                        saved,
                        current_iter,
                    );
                }
                Err(e) => {
                    // Safety check failed: system messages too large
                    error!("[context] Condensation aborted: {}", e);
                    force_failed = true;
                    final_content = format!("Task failed: {}", e);
                    break;
                }
            }
            last_condense_iteration = current_iter;
        }

        // Layer 3: iteration-aware tool result pruning
        helpers::prune_old_tool_results(&mut messages, current_iter as u32);

        // Layer 4: compact old assistant tool_calls JSON (only keep recent turns)
        helpers::compact_old_assistant_messages(&mut messages, keep_turns);

        // ── Optional: insert prompt message before LLM call ──
        // Subtypes: "first" (first normal LLM call), "compaction" (after context
        // compaction), "follow_up" (subsequent normal calls).
        let prompt_subtype = if !has_logged_first_prompt {
            "first"
        } else if current_iter == last_condense_iteration {
            "compaction"
        } else {
            "follow_up"
        };
        let should_log_prompt = match prompt_log_level {
            "off" => false,
            "first" => !has_logged_first_prompt,
            "first+compact" => !has_logged_first_prompt || current_iter == last_condense_iteration,
            "all" => true,
            _ => false,
        };
        if should_log_prompt {
            let prompt_seq = {
                let v = next_seq;
                next_seq += 1;
                v
            };
            let prompt_content = serde_json::to_string(&messages).unwrap_or_else(|_| String::new());
            let prompt_msg = MessageNew {
                thread_id: thread.id,
                role: "system".to_string(),
                content: prompt_content,
                thread_sequence: prompt_seq,
                external_id: None,
                metadata: serde_json::json!({
                    "prompt_log_level": prompt_log_level,
                    "prompt_subtype": prompt_subtype,
                    "num_messages": messages.len(),
                    "iteration": current_iter,
                    "condensed": needs_hard_condense || needs_soft_condense,
                }),
                embedding: None,
                summary_text: None,
                is_summary: false,
                msg_type: "prompt".to_string(),
                msg_subtype: Some(prompt_subtype.to_string()),
                iteration_number: current_iter,
            };
            if let Err(e) = queries::create_message(&cfg.pool, &prompt_msg).await {
                warn!(
                    "[prompt] Failed to persist prompt for thread {}: {:?}",
                    thread.id, e
                );
            }
            has_logged_first_prompt = true;
        }

        // ── LLM completion call ──

        let request = CompletionRequest {
            messages: messages.clone(),
            max_tokens: cfg.config_snapshot().max_tokens,
            temperature: cfg.config_snapshot().temperature,
            stream: false,
            tools: if tools_def.is_empty() {
                None
            } else {
                Some(tools_def.clone())
            },
        };

        let response = match per_thread_llm.completion(request).await {
            Ok(resp) => resp,
            Err(e) => {
                error!("LLM call failed: {:?}", e);
                final_content = format!("I encountered an error: {}", e);
                break;
            }
        };

        // Track cumulative token usage
        helpers::merge_usage(&mut cumulative_usage, response.usage.clone());

        // Store reasoning if present
        if response.reasoning.is_some() {
            final_reasoning = response.reasoning.clone();
        }

        // Check for tool calls
        if response.tool_calls.is_empty() {
            // Subtask enforcement: only when subtask mode is active
            if enable_subtasks {
                // Check if all subtasks are completed/cancelled before allowing final answer
                let pending_subtasks =
                    match crate::subtask::list_subtasks(&cfg.pool, thread.id).await {
                        Ok(list) => list
                            .into_iter()
                            .filter(|st| st.status == "pending" || st.status == "in_progress")
                            .collect::<Vec<_>>(),
                        Err(_) => Vec::new(),
                    };

                if !pending_subtasks.is_empty()
                    && unfinished_subtask_retries
                        < cfg.config_snapshot().max_unfinished_subtask_retries
                {
                    unfinished_subtask_retries += 1;
                    let max_retries = cfg.config_snapshot().max_unfinished_subtask_retries;
                    let names: Vec<String> = pending_subtasks
                        .iter()
                        .map(|st| format!("#{}: {} ({})", st.id, st.description, st.status))
                        .collect();
                    let feedback = format!(
                        "[Subtask Required] You cannot end this thread while subtasks are still pending. \
                         BEFORE writing your final answer, call `manage_subtasks(action=\"update\", subtask_id=N, status=\"completed\")` \
                         for each subtask you've already finished. If any subtask is no longer needed, use status=\"cancelled\".\n\n\
                         Remaining unfinished subtasks:\n{}\n\n\
                         You will be retried (attempt {}/{}): use this chance to manage them.",
                        names.join("\n"),
                        unfinished_subtask_retries,
                        max_retries,
                    );
                    messages.push(ChatMessage::system(&feedback));
                    info!(
                        "[subtask] Enforcement: LLM tried to end with {} unfinished subtask(s) (retry {}/{})",
                        pending_subtasks.len(),
                        unfinished_subtask_retries,
                        max_retries,
                    );
                    // Don't consume from the iteration budget: this is enforcement overhead
                    current_iter -= 1;
                    continue;
                }

                if !pending_subtasks.is_empty() {
                    let max_retries = cfg.config_snapshot().max_unfinished_subtask_retries;
                    // Exhausted retries: force the thread to fail
                    warn!(
                        "[subtask] Enforcement exhausted after {} retries: {} subtask(s) still unfinished for thread {}",
                        max_retries,
                        pending_subtasks.len(),
                        thread.id,
                    );
                    final_content = format!(
                        "I ran out of attempts to complete all subtasks. The following remain unfinished:\n{}",
                        pending_subtasks.iter().map(|st| format!("- #{}: {} ({})", st.id, st.description, st.status)).collect::<Vec<_>>().join("\n"),
                    );
                    final_tool_call = false;
                    force_failed = true;
                    break;
                }
            }

            // Normal text response: all subtasks done (or subtask mode off)
            final_content = if response.content.is_empty() {
                // When both content and reasoning are empty (e.g. context too large
                // caused the LLM to return nothing), produce a fallback error message
                // and force the thread to fail.
                // Note: DeepSeek with reasoning always returns reasoning=Some(...),
                // even when the reasoning string is empty, so we must check the
                // content of reasoning too, not just whether it's Some/None.
                let reasoning_empty = response
                    .reasoning
                    .as_ref()
                    .map(|r| r.trim().is_empty())
                    .unwrap_or(true); // None means empty too

                // Check if the response has meaningful completion_tokens but empty
                // content: indicates a content filter or provider-side stripping.
                let has_completion = response
                    .usage
                    .as_ref()
                    .map(|u| u.completion_tokens > 0)
                    .unwrap_or(false);

                if reasoning_empty && has_completion {
                    // The API reports generated tokens but returned empty content.
                    // This indicates content was filtered/stripped (provider safety filter).
                    // Log it and produce a clear error rather than hiding it.
                    let prompt_toks = response
                        .usage
                        .as_ref()
                        .map(|u| u.prompt_tokens)
                        .unwrap_or(0);
                    let comp_toks = response
                        .usage
                        .as_ref()
                        .map(|u| u.completion_tokens)
                        .unwrap_or(0);
                    warn!(
                        "[executor] LLM returned empty content with {} completion tokens (prompt: {}): likely content filter",
                        comp_toks, prompt_toks,
                    );
                }

                if reasoning_empty && enable_subtasks {
                    let pending_subtasks =
                        match crate::subtask::list_subtasks(&cfg.pool, thread.id).await {
                            Ok(list) => list
                                .into_iter()
                                .filter(|st| st.status == "pending" || st.status == "in_progress")
                                .collect::<Vec<_>>(),
                            Err(_) => Vec::new(),
                        };
                    force_failed = true; // empty response: thread must fail
                    if pending_subtasks.is_empty() {
                        "The LLM returned an empty response with no pending subtasks: likely caused by context explosion.".to_string()
                    } else {
                        format!(
                            "The LLM returned an empty response. The following subtasks were never completed:\n{}",
                            pending_subtasks.iter().map(|st| format!("- #{}: {} ({})", st.id, st.description, st.status)).collect::<Vec<_>>().join("\n"),
                        )
                    }
                } else if reasoning_empty {
                    // No subtask mode, but content AND reasoning are both empty
                    force_failed = true; // empty response: thread must fail
                    "The LLM returned an empty response: likely caused by context explosion."
                        .to_string()
                } else {
                    // Reasoning has content but no response content. Leave
                    // final_content empty: the reasoning is already saved
                    // as a separate `reasoning` message (step 8 below).
                    // Using reasoning as final_content would cause the
                    // summary message to duplicate the reasoning text.
                    String::new()
                }
            } else {
                response.content
            };
            final_tool_call = false;
            break;
        }

        // If iterations will equal the max after this call, flag interruption
        if current_iter >= iter_limit {
            limit_reached = true;
            // Produce content from the last tool calls so final_content is
            // non-empty: prevents a false "empty response" detection when
            // the iteration budget runs out while the LLM was making tools.
            if !response.tool_calls.is_empty() {
                let tool_names: Vec<String> = response
                    .tool_calls
                    .iter()
                    .map(|tc| tc.function.name.clone())
                    .collect();
                final_content = format!(
                    "Iteration limit reached. Last tool calls issued: {}. The task was interrupted before completion.",
                    tool_names.join(", "),
                );
                final_tool_call = false;
            }
            break;
        }

        // We have tool calls: add assistant message with tool_calls
        final_tool_call = true;
        let mut assistant_msg = ChatMessage::assistant("");
        assistant_msg.tool_calls = Some(response.tool_calls.clone());
        messages.push(assistant_msg);

        // If multiple tool calls, persist a multi-tool message first
        if response.tool_calls.len() > 1 {
            let mcp_snapshot = cfg.mcp.read().await;
            let multi_content = response
                .tool_calls
                .iter()
                .map(|tc| {
                    format!(
                        "{}: {}",
                        mcp_snapshot.qualified_name(&tc.function.name),
                        tc.function.arguments
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let multi_msg = MessageNew {
                thread_id: thread.id,
                role: "agent".to_string(),
                content: multi_content,
                thread_sequence: {
                    let v = next_seq;
                    next_seq += 1;
                    v
                },
                external_id: None,
                metadata: serde_json::json!({}),
                embedding: None,
                summary_text: None,
                is_summary: false,
                msg_type: "multi-tool".to_string(),
                msg_subtype: None,
                iteration_number: current_iter,
            };
            match helpers::persist_or_abort(&cfg.pool, &multi_msg, thread.id).await {
                helpers::CreateMessageResult::FkViolation => {
                    err_msg!("FK violation: thread {} no longer exists", thread.id);
                }
                helpers::CreateMessageResult::OtherError(e) => {
                    error!("Failed to persist multi-tool message: {:?}", e)
                }
                helpers::CreateMessageResult::Success(saved) => {
                    helpers::enqueue_delivery(
                        &cfg.ctx,
                        &saved,
                        &channel,
                        thread,
                        cause_msg.external_id.clone(),
                    )
                    .await;
                }
            }
        }

        // ── Parallel tool execution ──
        // Execute all tool calls concurrently, each inserts its own consolidated
        // result message (JSON: {tool, input, output}) as it finishes.
        // LLM-facing ChatMessages are collected and pushed in original call order
        // after all tools complete.
        let tool_count = response.tool_calls.len();

        // Pre-allocate sequence numbers for each result message
        let result_seqs: Vec<i32> = (0..tool_count)
            .map(|_| {
                let v = next_seq;
                next_seq += 1;
                v
            })
            .collect();

        let pool = cfg.pool.clone();
        let mcp_registry = cfg.mcp.clone();
        let mut join_set = JoinSet::new();

        use std::sync::Arc as StdArc;

        for (idx, tc) in response.tool_calls.iter().enumerate() {
            let tool_name = tc.function.name.clone();
            let tool_args = tc.function.arguments.clone();
            let tc_id = tc.id.clone();
            let qualified_name = mcp_registry.read().await.qualified_name(&tool_name);

            let mcp_call = McpToolCall {
                id: tc.id.clone(),
                name: tool_name.clone(),
                arguments: serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(serde_json::json!({})),
            };

            let mut tool_ctx = cfg.ctx.clone();
            tool_ctx.current_thread_id = Some(thread.id);
            tool_ctx.current_channel_id = Some(thread.channel_id);
            tool_ctx.current_profile_name = Some(profile_name.clone());
            tool_ctx.current_channel_name = Some(channel.name.clone());
            tool_ctx.current_platform = channel.platform.clone();
            tool_ctx.current_allowed_tools = prof.allowed_tools.clone();

            let pool = pool.clone();
            let mcp = mcp_registry.clone();
            let seq = result_seqs[idx];
            let tid = thread.id;
            let iter_num = current_iter;

            // --- Phase 1: Read per-tool timeout from registry ---
            // Snapshot the registry outside the spawned task so we only read the lock once.
            let mcp_snapshot = mcp.read().await.clone();
            let timeout_secs = mcp_snapshot.get_timeout_secs(&tool_name);
            let timeout_dur = std::time::Duration::from_secs(timeout_secs);

            // --- Phase 3: Resolve watchdog config ---
            // 1. Per-tool watchdog from the tool's registration
            // 2. Fallback to global watchdog from cfg.config
            // 3. If neither, no watchdog
            let watchdog = mcp_snapshot.get_watchdog(&tool_name).or_else(|| {
                cfg.config.read().ok().and_then(|c| c.global_watchdog.clone())
            });

            // Shared cancel signal between watchdog and tool execution
            let cancel_token = StdArc::new(tokio::sync::Notify::new());
            let cancel_token_clone = cancel_token.clone();

            // Channel for progress messages from watchdog
            let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

            // --- Spawn watchdog task (Phase 2 + Phase 3) ---
            if let Some(wd) = watchdog {
                let wd_timeout = timeout_secs;
                let wd_cancel = cancel_token_clone;
                let wd_progress = progress_tx.clone();
                let wd_tool_name = tool_name.clone();
                tokio::spawn(async move {
                    let started = std::time::Instant::now();
                    let mut sorted = wd.thresholds.clone();
                    sorted.sort_by(|a, b| a.at_percent.partial_cmp(&b.at_percent).unwrap_or(std::cmp::Ordering::Equal));

                    for threshold in &sorted {
                        let delay_secs = (wd_timeout as f64 * threshold.at_percent).max(0.1);
                        tokio::time::sleep(std::time::Duration::from_secs_f64(delay_secs - started.elapsed().as_secs_f64().max(0.0))).await;
                        match &threshold.action {
                            WatchdogAction::Notify { message } => {
                                let elapsed = started.elapsed().as_secs_f64();
                                let pct = (elapsed / wd_timeout as f64 * 100.0) as u32;
                                let notif = format!("[Watchdog] {} ({}s elapsed, ~{}%)", message, elapsed as u64, pct);
                                tracing::info!("Watchdog({}): {}", wd_tool_name, notif);
                                let _ = wd_progress.send(notif);
                            }
                            WatchdogAction::Cancel => {
                                tracing::warn!("Watchdog cancelling tool '{}' after {}s ({}% of timeout)",
                                    wd_tool_name, started.elapsed().as_secs_f64() as u64,
                                    (started.elapsed().as_secs_f64() / wd_timeout as f64 * 100.0) as u32);
                                wd_cancel.notify_one();
                                break;
                            }
                        }
                    }
                });
            }

            // Snapshot short timeout BEFORE entering the spawned closure (cfg ref issue)
            let bg_threshold_secs = cfg.config_snapshot().tool_bg_secs;
            let bg_threshold = std::time::Duration::from_secs(bg_threshold_secs);

            join_set.spawn(async move {
                // --- Phase 2: Progress reporting ---
                // Forward progress notifications to the DB while tool executes.
                // Run this alongside the tool execution.
                let pool_for_progress = pool.clone();
                let tool_name_for_progress = tool_name.clone();
                let progress_task = tokio::spawn(async move {
                    while let Some(progress_msg) = progress_rx.recv().await {
                        let next_seq = queries::get_max_thread_sequence(&pool_for_progress, tid)
                            .await.unwrap_or(0) + 1;
                        let progress_entry = MessageNew {
                            thread_id: tid,
                            role: "agent".to_string(),
                            content: progress_msg,
                            thread_sequence: next_seq,
                            external_id: None,
                            metadata: serde_json::json!({"progress": true}),
                            embedding: None,
                            summary_text: None,
                            is_summary: false,
                            msg_type: "tool_progress".to_string(),
                            msg_subtype: Some(tool_name_for_progress.clone()),
                            iteration_number: iter_num,
                        };
                        let _ = queries::create_message(&pool_for_progress, &progress_entry).await;
                    }
                });

                // Execute with short timeout (fast path) + background fallback
                let tool_future = mcp_snapshot.execute(&mcp_call, tool_ctx.clone());

                let result = tokio::select! {
                    biased;
                    _ = cancel_token.notified() => {
                        let msg = format!(
                            "Tool '{}' was cancelled by watchdog",
                            tool_name,
                        );
                        error!("{}", msg);
                        Err(crate::error::Error::Message(msg))
                    }
                    result = tokio::time::timeout(bg_threshold, tool_future) => {
                        match result {
                            Ok(result) => result,
                            Err(_elapsed) => {
                                // Short timeout exceeded — switch to background mode.
                                // Register the tool in the task registry for polling.
                                let registry = crate::agent::task_registry::TASK_REGISTRY
                                    .get()
                                    .cloned()
                                    .expect("TASK_REGISTRY not initialized");
                                let (task_id, abort_rx, _log_buffer) = registry
                                    .register(tid, &tool_name)
                                    .await;
                                let task_id_bg = task_id.clone();

                                // Spawn background task with the full long timeout
                                let bg_mcp_call = mcp_call.clone();
                                let bg_mcp_snapshot = mcp_snapshot.clone();
                                let bg_timeout = timeout_dur;
                                let bg_tool_name = tool_name.clone();
                                let bg_registry = registry.clone();

                                tokio::spawn(async move {
                                    let bg_tool_future = bg_mcp_snapshot.execute(&bg_mcp_call, tool_ctx);
                                    let bg_result = tokio::select! {
                                        _ = abort_rx => {
                                            bg_registry.set_status(&task_id_bg,
                                                crate::agent::task_registry::TaskStatus::Cancelled).await;
                                            bg_registry.append_log(&task_id_bg,
                                                &format!("Tool '{}' was cancelled", bg_tool_name)).await;
                                            return;
                                        }
                                        result = tokio::time::timeout(bg_timeout, bg_tool_future) => {
                                            match result {
                                                Ok(Ok(res)) => {
                                                    let truncated = truncate_content(
                                                        &res.content, DEFAULT_MAX_TOOL_OUTPUT_CHARS);
                                                    bg_registry.set_status(&task_id_bg,
                                                        crate::agent::task_registry::TaskStatus::Completed(
                                                            truncated)).await;
                                                }
                                                Ok(Err(e)) => {
                                                    let err = format!("Error: {}", e);
                                                    bg_registry.set_status(&task_id_bg,
                                                        crate::agent::task_registry::TaskStatus::Failed(
                                                            err)).await;
                                                }
                                                Err(_) => {
                                                    let err = format!(
                                                        "Tool '{}' exceeded long timeout ({}s)",
                                                        bg_tool_name, bg_timeout.as_secs());
                                                    bg_registry.set_status(&task_id_bg,
                                                        crate::agent::task_registry::TaskStatus::Failed(
                                                            err)).await;
                                                }
                                            }
                                        }
                                    };
                                    let _ = bg_result;
                                });

                                // Return a McpToolResult containing processing status
                                let processing_json = serde_json::json!({
                                    "status": "processing",
                                    "task_id": task_id,
                                    "tool": qualified_name,
                                    "timeout_secs": bg_threshold.as_secs(),
                                    "message": format!(
                                        "Tool '{}' started. Use poll_task, wait_task, or read_task_logs to check progress.",
                                        tool_name
                                    ),
                                });
                                Ok(McpToolResult {
                                    call_id: tc_id.clone(),
                                    content: processing_json.to_string(),
                                    is_error: false,
                                })
                            }
                        }
                    }
                };
                // Stop progress task
                progress_task.abort();

                let (output, is_error) = match &result {
                    Ok(res) => {
                        let truncated =
                            truncate_content(&res.content, DEFAULT_MAX_TOOL_OUTPUT_CHARS);
                        (truncated, false)
                    }
                    Err(e) => (format!("Error executing tool '{}': {}", tool_name, e), true),
                };

                // Build consolidated JSON: {tool, input, output}
                let args_value: serde_json::Value =
                    serde_json::from_str(&tool_args).unwrap_or(serde_json::json!(tool_args));
                let json_content = serde_json::json!({
                    "tool": qualified_name,
                    "input": args_value,
                    "output": output,
                });

                // Persist single consolidated result message
                // (no separate "tool" call message anymore)
                let result_msg = MessageNew {
                    thread_id: tid,
                    role: "agent".to_string(),
                    content: json_content.to_string(),
                    thread_sequence: seq,
                    external_id: None,
                    metadata: serde_json::json!({"is_error": is_error}),
                    embedding: None,
                    summary_text: None,
                    is_summary: false,
                    msg_type: "tool-result".to_string(),
                    msg_subtype: Some(qualified_name.clone()),
                    iteration_number: iter_num,
                };

                match helpers::persist_or_abort(&pool, &result_msg, tid).await {
                    helpers::CreateMessageResult::FkViolation => {
                        error!("FK violation: thread {} no longer exists", tid);
                    }
                    helpers::CreateMessageResult::OtherError(e) => {
                        error!("Failed to persist tool result '{}': {:?}", tool_name, e)
                    }
                    helpers::CreateMessageResult::Success(_) => {}
                }

                (idx, tc_id, tool_name, output, is_error)
            });
        }

        // Collect results as they complete (order may differ from call order)
        let mut tool_results: Vec<Option<(String, String, String)>> = vec![None; tool_count];
        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((idx, tc_id, tool_name, output, _is_error)) => {
                    tool_results[idx] = Some((tc_id, tool_name, output));
                }
                Err(e) => {
                    error!("Tool execution task panicked: {:?}", e);
                }
            }
        }

        // Push LLM messages in original call order
        for (i, _tc) in response.tool_calls.iter().enumerate() {
            if let Some((tc_id, tool_name, output)) = &tool_results[i] {
                messages.push(ChatMessage::tool_result(tc_id, tool_name, output));
            }
        }

        // Proactive subtask reminder: if the LLM has made several tool call
        // rounds without managing subtasks, inject a gentle nudge.
        if enable_subtasks {
            // Check if any tool call in this round was manage_subtasks
            let called_manage = response
                .tool_calls
                .iter()
                .any(|tc| tc.function.name == "manage_subtasks");
            if called_manage {
                calls_since_subtask_management = 0;
            } else {
                calls_since_subtask_management += 1;
            }

            if calls_since_subtask_management >= 3 {
                if let Ok(subtasks) = crate::subtask::list_subtasks(&cfg.pool, thread.id).await {
                    let pending_count = subtasks
                        .iter()
                        .filter(|st| st.status == "pending" || st.status == "in_progress")
                        .count();
                    if pending_count > 0 {
                        let reminder = format!(
                            "[Progress Check] You've made {} tool call rounds without updating your subtasks. \
                             If you've completed any steps, call `manage_subtasks(action=\"update\", subtask_id=N, status=\"completed\")` \
                             for each finished subtask now. This keeps progress accurate.",
                            calls_since_subtask_management,
                        );
                        messages.push(ChatMessage::system(&reminder));
                        calls_since_subtask_management = 0;
                    }
                }
            }
        }
    } // end for _turn

    // If we exited the loop without a final text response, provide a fallback
    if final_content.is_empty() && !final_tool_call {
        final_content =
            "I've completed the requested operations using my available tools.".to_string();
    } else if final_content.is_empty() && final_tool_call {
        // The loop exhausted all iterations while the LLM was still issuing tool
        // calls: no final answer was produced. Set limit_reached (interrupted)
        // rather than force_failed so the thread is correctly marked as
        // interrupted (can be resumed) instead of failed (dead end).
        final_content = "The task ran out of iterations while still processing tools: no final answer was produced.".to_string();
        limit_reached = true;
    }

    // 7. Serialize cumulative token usage
    let token_usage_json = cumulative_usage.as_ref().map(|u| {
        serde_json::json!({
            "prompt_tokens": u.prompt_tokens,
            "completion_tokens": u.completion_tokens,
            "cached_tokens": u.cached_tokens,
            "reasoning_tokens": u.reasoning_tokens,
        })
    });

    // Build evidence metadata from context assembly
    let evidence_metadata = {
        let meta = serde_json::json!({
            "context": {
                "selected_message_ids": [],
                "wiki_files": [],
                "block_counts": {},
                "dropped_blocks": [],
                "total_chars": 0,
            },
            "grounding": {
                "policy_applied": true,
            }
        });
        /* ctx_assembly_meta removed: context comes from prompt tool */
        meta
    };

    // 8. If reasoning/thinking exists, save as its own record
    if let Some(ref reasoning_text) = final_reasoning {
        if !reasoning_text.is_empty() {
            let reasoning_msg = MessageNew {
                thread_id: thread.id,
                role: "agent".to_string(),
                content: reasoning_text.clone(),
                thread_sequence: {
                    let v = next_seq;
                    next_seq += 1;
                    v
                },
                external_id: None,
                metadata: serde_json::json!({
                    "context": evidence_metadata["context"],
                    "grounding": evidence_metadata["grounding"],
                }),
                embedding: None,
                summary_text: None,
                is_summary: false,
                msg_type: "reasoning".to_string(),
                msg_subtype: None,
                iteration_number: current_iter,
            };
            let reasoning_saved = queries::create_message(&cfg.pool, &reasoning_msg).await?;
            helpers::enqueue_delivery(
                &cfg.ctx,
                &reasoning_saved,
                &channel,
                thread,
                cause_msg.external_id.clone(),
            )
            .await;
        }
    }

    // 9. Save the main agent response (when limit_reached, generate LLM summary instead)
    let agent_elapsed_ms = start_time.elapsed().as_millis() as i32;
    let is_empty_response = final_content.trim().is_empty();

    let saved = if limit_reached {
        // ── Summary generation (when interrupted / iteration limit reached) ──
        // Generate an LLM summary that reports what was accomplished and what remains.
        // This replaces the hardcoded message so the summary is the only output.
        let mut summary_msgs: Vec<ChatMessage> = messages
            .iter()
            .filter(|m| m.role != "tool")
            .map(|m| {
                let mut cloned = m.clone();
                // Remove tool_calls from assistant messages since we removed
                // the corresponding tool results: DeepSeek requires tool_call
                // chains to be complete.
                if cloned.role == "assistant" && cloned.tool_calls.is_some() {
                    cloned.tool_calls = None;
                }
                cloned
            })
            .collect();
        let iter_summary = format!(
            "The iteration limit ({}/{}) was reached so the task may be incomplete. \
             Summarize what was accomplished (including what the agent did and found) and what remains to be done. \
             Inform the user they can request to continue.",
            current_iter, iter_limit,
        );
        summary_msgs.push(ChatMessage::system(&iter_summary));

        let summary_request = CompletionRequest {
            messages: summary_msgs,
            max_tokens: cfg.config_snapshot().thread_summary_tokens,
            temperature: 0.3,
            stream: false,
            tools: None,
        };

        let _summary_start = std::time::Instant::now();
        let (summary_text, _summary_token_usage) = match per_thread_llm
            .completion(summary_request)
            .await
        {
            Ok(resp) => {
                let usage = resp.usage.clone();
                helpers::merge_usage(&mut cumulative_usage, resp.usage);
                let tokens = usage.as_ref().map(|u| {
                    serde_json::json!({
                        "prompt_tokens": u.prompt_tokens,
                        "completion_tokens": u.completion_tokens,
                        "cached_tokens": u.cached_tokens,
                        "reasoning_tokens": u.reasoning_tokens,
                    })
                });
                info!(
                    "[summary] Generated summary for thread {} ({} chars, reasoning={}, limit_reached={})",
                    thread.id,
                    resp.content.len(),
                    resp.reasoning.as_ref().map(|r| r.len()).unwrap_or(0),
                    limit_reached,
                );
                let text = if resp.content.trim().is_empty() {
                    resp.reasoning.clone().unwrap_or_default()
                } else {
                    resp.content
                };
                (text, tokens)
            }
            Err(e) => {
                warn!(
                    "[summary] Failed to generate summary for thread {}: {:?}",
                    thread.id, e
                );
                (format!("Summary generation failed: {}", e), None)
            }
        };

        let summary_msg = MessageNew {
            thread_id: thread.id,
            role: "agent".to_string(),
            content: summary_text,
            thread_sequence: next_seq,
            external_id: None,
            metadata: serde_json::json!({}),
            embedding: None,
            summary_text: None,
            is_summary: true,
            msg_type: "summary".to_string(),
            msg_subtype: Some("interrupted".to_string()),

            iteration_number: current_iter,
        };

        let summary_saved = queries::create_message(&cfg.pool, &summary_msg).await?;
        info!("[summary] Saved summary message for thread {}", thread.id);
        helpers::enqueue_delivery(
            &cfg.ctx,
            &summary_saved,
            &channel,
            thread,
            cause_msg.external_id.clone(),
        )
        .await;
        summary_saved
    } else if is_empty_response {
        let agent_content = format!(
            "The LLM returned an empty response. The task failed.\n\
             Possible causes: token explosion (context too large), provider error, or LLM output limits.\n\
             Prompt tokens used in this turn: {}",
            token_usage_json.as_ref()
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(|v| v.as_i64())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );
        let agent_msg = MessageNew {
            thread_id: thread.id,
            role: "agent".to_string(),
            content: agent_content,
            thread_sequence: next_seq,
            external_id: None,
            metadata: serde_json::json!({
                "context": evidence_metadata["context"],
                "grounding": evidence_metadata["grounding"],
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "error".to_string(),
            msg_subtype: Some("empty_response".to_string()),

            iteration_number: current_iter,
        };
        let saved = queries::create_message(&cfg.pool, &agent_msg).await?;
        helpers::enqueue_delivery(
            &cfg.ctx,
            &saved,
            &channel,
            thread,
            cause_msg.external_id.clone(),
        )
        .await;
        saved
    } else {
        // Normal completion: the agent's final message IS the summary
        let agent_msg = MessageNew {
            thread_id: thread.id,
            role: "agent".to_string(),
            content: final_content.clone(),
            thread_sequence: next_seq,
            external_id: None,
            metadata: serde_json::json!({
                "context": evidence_metadata["context"],
                "grounding": evidence_metadata["grounding"],
            }),
            embedding: None,
            summary_text: None,
            is_summary: true,
            msg_type: "summary".to_string(),
            msg_subtype: None,

            iteration_number: current_iter,
        };
        let saved = queries::create_message(&cfg.pool, &agent_msg).await?;
        helpers::enqueue_delivery(
            &cfg.ctx,
            &saved,
            &channel,
            thread,
            cause_msg.external_id.clone(),
        )
        .await;
        saved
    };
    // Define final status before potential early return
    let final_status = if force_failed {
        "failed"
    } else if limit_reached {
        "interrupted"
    } else {
        "completed"
    };

    // Post-loop subtask enforcement: if any subtasks remain pending/in_progress
    // after the tool-calling loop ends (regardless of why it ended), fail the thread.
    // Subtasks must only be marked completed/cancelled by the LLM via manage_subtasks tool.
    // Exception: if the iteration limit was reached, unfinished subtasks are expected
    //: keep the interrupted status rather than downgrading to failed.
    if enable_subtasks && !force_failed && !limit_reached && final_status == "completed" {
        if let Ok(post_subtasks) = crate::subtask::list_subtasks(&cfg.pool, thread.id).await {
            let unfinished: Vec<_> = post_subtasks
                .iter()
                .filter(|st| st.status == "pending" || st.status == "in_progress")
                .collect();
            if !unfinished.is_empty() {
                warn!(
                    "[subtask] Post-loop enforcement: {} subtask(s) still unfinished for thread {}: forcing failure",
                    unfinished.len(),
                    thread.id,
                );
                force_failed = true;
            }
        }
    }

    // Recompute final status after post-loop enforcement
    let final_status = if force_failed {
        "failed"
    } else if limit_reached {
        "interrupted"
    } else {
        "completed"
    };

    queries::complete_thread(
        &cfg.pool,
        thread.id,
        final_status,
        CompleteThreadStats {
            input_tokens: cumulative_usage
                .as_ref()
                .map(|u| u.prompt_tokens as i32)
                .unwrap_or(0),
            cached_tokens: cumulative_usage
                .as_ref()
                .map(|u| u.cached_tokens.unwrap_or(0) as i32)
                .unwrap_or(0),
            output_tokens: cumulative_usage
                .as_ref()
                .map(|u| u.completion_tokens as i32)
                .unwrap_or(0),
            duration_ms: agent_elapsed_ms,
        },
    )
    .await?;

    // ── Send completion reaction to platform ──
    // Use the cause message's external_id if available; otherwise look it up
    // from the database (the async post-back from delivery may have set it).
    let reaction_ext_id = if cause_msg.external_id.is_some() {
        cause_msg.external_id.clone()
    } else {
        crate::db::threads::get_cause_message(&cfg.pool, thread.id)
            .await
            .ok()
            .flatten()
            .and_then(|m| m.external_id)
    };
    if let Some(ref ext_id) = reaction_ext_id {
        if let Some(ref platform) = channel.platform {
            if let Some(ref resource) = channel.resource_identifier {
                let emoji = match final_status {
                    "completed" => ":white_check_mark:",
                    "failed" => ":x:",
                    "interrupted" => ":broken_heart:",
                    _ => ":o:",
                };
                helpers::enqueue_reaction(&cfg.ctx, platform, resource, ext_id, emoji).await;
            }
        }
    }

    // If this thread is linked to a kanban task, update its status
    if let Some(ref task_id) = thread.task_id {
        let kanban_status = if final_status == "completed" {
            "review"
        } else {
            "blocked"
        };
        let _ = queries::update_kanban_task_status(&cfg.pool, task_id, kanban_status).await;
    }

    // 11. Trigger cross-thread summary check via memory plugin
    {
        let mcp_call = McpToolCall {
            id: "post-thread-summary".to_string(),
            name: "memory_generate_summary".to_string(),
            arguments: serde_json::json!({
                "channel_id": thread.channel_id,
            }),
        };
        let _ = cfg
            .mcp
            .read()
            .await
            .execute(&mcp_call, cfg.ctx.clone())
            .await;
    }

    // 12. Cancel any remaining background tasks for this thread
    {
        let registry = crate::agent::task_registry::TASK_REGISTRY
            .get()
            .cloned();
        if let Some(reg) = registry {
            let count = reg.cancel_all_for_thread(thread.id).await;
            if count > 0 {
                tracing::info!("Cancelled {} remaining background task(s) for thread {}", count, thread.id);
            }
        }
    }

    Ok(saved)
}
