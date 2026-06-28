use crate::error::AppResult;
use crate::err_msg;
use tracing::{error, info, warn};

use crate::agent::config::AgentContext;
use crate::agent::helpers;
use crate::db::types as queries;
use crate::db::types::{CompleteThreadStats, Message, MessageNew, Thread};
use crate::llm::{ChatMessage, CompletionRequest, Usage};
use crate::mcp::McpToolCall;
use crate::mcp::{truncate_content, DEFAULT_MAX_TOOL_OUTPUT_CHARS};
use crate::prompt_builder::{format_subtask_section, PlanningPromptParams};

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
                "Invalid configuration: profile='{}', provider={:?}, model={:?} — profile name is empty. Set a profile on the channel or thread.",
                profile_name, provider_name, model_name
            ),
            thread_sequence: { let v = next_seq; v },
            external_id: Some(format!("validation-error:{}:{}", thread.id, chrono::Utc::now().timestamp())),
            metadata: serde_json::json!({
                "error_type": "configuration",
            }),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "error".to_string(),
            msg_subtype: Some("no-profile".to_string()),
            processing_time_ms: None,
            token_usage: None,
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
            thread_sequence: {
                let v = next_seq;
                v
            },
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
            processing_time_ms: None,
            token_usage: None,
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
            thread_sequence: { let v = next_seq; v },
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
            processing_time_ms: None,
            token_usage: None,
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
            thread_sequence: { let v = next_seq; v },
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
            processing_time_ms: None,
            token_usage: None,
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

    // Validation passed — load the profile for its settings (auto_retrieval_enabled, etc.)
    let prof = profile_registry
        .get(&profile_name)
        .cloned()
        .unwrap_or_else(|| crate::profile::Profile::default(&profile_name));

    // Use provider/model directly from the thread stamp (no fallback chain)
    let _provider_name = provider_name;
    let _model_name = model_name;

    // 4. Build the initial message history with the structured system prompt
    let tool_names: Vec<String> = cfg.mcp.all().iter().map(|t| t.name.clone()).collect();
    let system_prompt = crate::prompt_builder::build_system_prompt(
        &cfg.ctx.memory_store,
        "",   // platform — will be enriched from channel metadata in the future
        None, // system_message
        &profile_name,
        &tool_names,
    );

    // 4a. Inject subtask context if the thread has subtasks
    let subtask_section: Option<String> =
        match crate::subtask::list_subtasks(&cfg.pool, thread.id).await {
            Ok(subtask_rows) => {
                if subtask_rows.is_empty() {
                    None
                } else {
                    let thread_subtasks: Vec<crate::prompt_builder::ThreadSubtask> = subtask_rows
                        .iter()
                        .enumerate()
                        .map(|(i, row)| {
                            let status = match row.status.as_str() {
                                "completed" => crate::prompt_builder::SubtaskStatus::Completed,
                                "cancelled" => crate::prompt_builder::SubtaskStatus::Cancelled,
                                "error" => crate::prompt_builder::SubtaskStatus::Error,
                                _ => crate::prompt_builder::SubtaskStatus::Pending,
                            };
                            crate::prompt_builder::ThreadSubtask {
                                name: row.description.clone(),
                                status,
                                step_index: i,
                                total_steps: subtask_rows.len(),
                            }
                        })
                        .collect();
                    let section = format_subtask_section(&thread_subtasks, thread.id);
                    if section.is_some() {
                        info!(
                            "Injected subtask section into system prompt for thread {}",
                            thread.id
                        );
                    }
                    section
                }
            }
            Err(e) => {
                warn!("Failed to query subtasks for thread {}: {:?}", thread.id, e);
                None
            }
        };

    // 4b. Load template from cause message metadata (for kanban/cron/user tasks)
    let template_section: Option<String> = {
        let msg_type = cause_msg.msg_type.as_str();
        if msg_type == "kanban" || msg_type == "cron" || msg_type == "user" {
            let template_name = cause_msg
                .metadata
                .get("template")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    cause_msg
                        .metadata
                        .get("template")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                });
            if let Some(template) = template_name {
                let content = crate::prompt_builder::load_template(
                    &cfg.ctx.data_dir,
                    &profile_name,
                    template,
                );
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

    // 4c. Assemble additional context blocks via ContextBuilder
    let ctx_assembly_meta: Option<crate::context_builder::ContextAssemblyMeta>;
    // For kanban/cron tasks, use task_context mode (skip conversation history, summaries)
    // even when no template is loaded. The task body IS the primary context.
    let is_task = cause_msg.msg_type == "kanban" || cause_msg.msg_type == "cron";

    // ── System thread: deliver seq-0 cause to channel's platform ──
    // If this system thread's channel has a platform (e.g. Mattermost),
    // enqueue the cause message so it appears as a new post in that channel.
    // Subsequent messages in this thread will be delivered as replies.
    if thread.cause == "system" && cause_msg.external_id.is_none() {
        if let Ok(Some(channel)) = queries::get_channel_by_id(&cfg.pool, thread.channel_id).await {
            if channel.platform.is_some() && channel.resource_identifier.is_some() {
                info!(
                    "Delivering system cause (thread {}) to platform {:?}:{:?}",
                    thread.id, channel.platform, channel.resource_identifier
                );
                helpers::enqueue_delivery(
                    &cfg.ctx,
                    cause_msg,
                    &channel,
                    thread,
                    None,
                )
                .await;
            }
        }
    }
    let context_messages = {
        let (context_text, meta) = crate::context_builder::build_thread_context(
            &cfg.pool,
            &crate::context_builder::ThreadContextIdentifiers {
                thread_id: thread.id,
                channel_id: thread.channel_id,
                cause_msg_id: cause_msg.id,
                parent_id: thread.parent_id,
            },
            &crate::context_builder::ThreadContextConfig {
                cause_content: &cause_msg.content,
                profile_name: &profile_name,
                data_dir: &cfg.ctx.data_dir,
                qdrant_url: cfg.ctx.qdrant_url.as_deref(),
                prompt_budget: prof
                    .prompt_budget
                    .unwrap_or(crate::profile::PROMPT_BUDGET_DEFAULT),
                auto_retrieval_enabled: prof.auto_retrieval_enabled,
                retrieval_aggressiveness: if is_task || template_section.is_some() {
                    prof.retrieval_aggressiveness.min(1)
                } else {
                    prof.retrieval_aggressiveness
                },
                task_context: is_task || template_section.is_some(),
            },
        )
        .await;
        ctx_assembly_meta = Some(meta);
        context_text
    };

    // Track cumulative token usage across all LLM calls
    let mut cumulative_usage: Option<crate::llm::Usage> = None;
    let mut force_failed: bool = false;
    let mut current_iter: i32;

    // ── Planning Phase ──
    // Read planning_mode from thread (single source of truth, resolved at creation time)
    let planning_mode = if thread.planning_mode.is_empty() {
        // Safety net for any remaining empty threads (backfilled to prompt_only)
        "prompt_only".to_string()
    } else {
        thread.planning_mode.clone()
    };

    // Fetch channel for delivery — cached for use throughout the function
    let channel = queries::get_channel_by_id(&cfg.pool, thread.channel_id)
        .await?
        .unwrap_or_default();

    // Whether subtask creation and enforcement are enabled
    let enable_subtasks = planning_mode == "auto_subtasks";

    // Determine if we should run the planning phase
    // The thread's planning_mode was resolved at creation time and is the
    // single source of truth. Threads always have exactly one of:
    // "prompt_only" (no plan), "auto_plan" (simple plan), "auto_subtasks" (plan + subtasks).
    let should_plan = matches!(
        planning_mode.as_str(),
        "always" | "auto_plan" | "auto_subtasks"
    );

    let plan_content: Option<String> = if should_plan {
        let max_iter = 0; // one-shot, no refinement iterations
        let max_tokens = cfg.config.prompt_plan_max_tokens;
        let mut last_plan: Option<String> = None;
        let mut json_failure_count: u32 = 0;
        let mut json_error_msg: Option<String> = None;

        for iter in 0..(max_iter + 1) {
            // Build the planning prompt (lightweight — no tools, no heavy context)
            let planning_prompt = crate::prompt_builder::build_planning_prompt(
                &cfg.ctx.memory_store,
                PlanningPromptParams {
                    platform: "", // platform
                    profile_name: &profile_name,
                    user_message: &cause_msg.content,
                    plan_iteration: iter,
                    max_iterations: max_iter,
                    previous_plan: last_plan.as_deref(),
                    use_json_plan: enable_subtasks,
                },
                &tool_names,
            );

            let mut planning_messages = vec![ChatMessage::system(&planning_prompt)];
            if let Some(ref err) = json_error_msg {
                planning_messages.push(ChatMessage::system(err));
            }
            // Inject the task template so the plan is aware of the instructions
            if let Some(ref ts) = template_section {
                planning_messages.push(ChatMessage::system(ts));
            }

            let plan_request = CompletionRequest {
                messages: planning_messages,
                max_tokens,
                temperature: 0.3,
                stream: false,
                tools: None,
            };

            match cfg.llm.completion(plan_request).await {
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

                    // Convert Usage to JSON value for token_usage field
                    let plan_token_usage = resp.usage.map(|u| {
                        serde_json::json!({
                            "prompt_tokens": u.prompt_tokens,
                            "completion_tokens": u.completion_tokens,
                            "cached_tokens": u.cached_tokens,
                            "reasoning_tokens": u.reasoning_tokens,
                        })
                    });
                    let plan_duration_ms = Some(resp.duration_ms as i32);

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
                            processing_time_ms: plan_duration_ms,
                            token_usage: plan_token_usage,
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

                    // For complex tasks, auto-create subtasks from JSON plan content
                    if enable_subtasks && plan_content.len() > 100 {
                        let max_json_retries: u32 = std::env::var("MAX_UNFINISHED_SUBTASK_RETRIES")
                            .ok()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(3);
                        match serde_json::from_str::<serde_json::Value>(&plan_content) {
                            Ok(plan_json) => {
                                if let Some(steps) =
                                    plan_json.get("steps").and_then(|v| v.as_array())
                                {
                                    // Valid JSON with steps — create subtasks
                                    let total = steps.len().min(6);
                                    for (i, step_val) in steps.iter().enumerate().take(6) {
                                        if let Some(step) = step_val.as_str() {
                                            let clean = step
                                                .trim()
                                                .trim_end_matches(|c: char| c == '*' || c == '`')
                                                .trim();
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
                                        warn!("[plan] JSON validation exhausted — missing 'steps' field after {} retries for thread {}", max_json_retries, thread.id);
                                        force_failed = true;
                                        break;
                                    }
                                    json_error_msg = Some(format!(
                                        "ERROR: Your plan JSON is missing the required \"steps\" array. \
                                         You MUST return a JSON object with \"description\" (string) and \"steps\" (array of strings). \
                                         No surrounding markdown, no backticks, no extra text. \
                                         Attempt {}/{} — fix the JSON or the thread will fail.",
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
                                    warn!("[plan] JSON validation exhausted — invalid JSON after {} retries for thread {}: {}", max_json_retries, thread.id, e);
                                    force_failed = true;
                                    break;
                                }
                                json_error_msg = Some(format!(
                                    "ERROR: Your response was not valid JSON. Parsing error: {}. \
                                     You MUST return a valid JSON object with \"description\" and \"steps\" fields. \
                                     No surrounding markdown, no backticks, no extra text. \
                                     Attempt {}/{} — fix the JSON or the thread will fail.",
                                    e, json_failure_count, max_json_retries
                                ));
                                last_plan = Some(plan_content.clone());
                                continue;
                            }
                        }
                    }

                    last_plan = Some(plan_content);

                    // One-shot: no refinement iterations — plan is final
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

    let mut messages = vec![ChatMessage::system(&system_prompt)];

    // Inject task template FIRST (right after system prompt) — highest instruction priority
    // for template-backed tasks (kanban/cron with template).
    // Flush-left position ensures the template guides the model before any other context.
    if let Some(ref template_section) = template_section {
        messages.push(ChatMessage::system(template_section));
    }

    // Inject subtask context section if the thread has active subtasks
    if let Some(ref subtask_section) = subtask_section {
        messages.push(ChatMessage::system(subtask_section));
    }

    // Add context blocks as system messages (before the user message)
    // For template-backed tasks (kanban/cron with template), skip conversation context
    // (recent messages, summaries, hindsight recall) — the template IS the context.
    if !context_messages.is_empty() {
        messages.push(ChatMessage::system(&format!(
            "=== Additional Context ===\n{}",
            context_messages
        )));
    }

    // Inject the plan as execution context if one was generated
    if let Some(ref plan) = plan_content {
        messages.push(ChatMessage::system(&format!(
            "=== Generated Plan (use as guidance) ===\n\
             A plan was generated for the current task. Follow it unless tool results \
             contradict it. Do NOT explore alternative approaches that the plan already \
             considered — adapt only when necessary.\n\n{}",
            plan
        )));
        info!(
            "[plan] Injected plan as context for thread {} ({} chars)",
            thread.id,
            plan.len(),
        );
    }

    // Add the user message (from the cause message)
    messages.push(ChatMessage::user(&cause_msg.content));

    // 5. Build tool definitions from the profile's allowed tools
    let tools_def = cfg.mcp.to_openai_tools(&prof.allowed_tools);

    // 6. Tool-calling loop — max iterations controls total LLM calls
    let iter_limit =
        queries::max_iterations_for_planning_mode(&cfg.config, &thread.planning_mode) as i32;
    // The plan phase consumed 1 iteration (if it ran). Subtract it so the
    // tool-calling loop gets the remaining budget.
    let plan_consumed = if should_plan { 1 } else { 0 };
    let max_llm_calls = (iter_limit - plan_consumed).max(0) as u32;
    let mut final_content = String::new();
    let mut final_reasoning: Option<String> = None;
    let mut final_tool_call: bool = false;
    let mut limit_reached: bool = false;
    let mut last_response_usage: Option<Usage> = None;
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

        let prompt_token_soft = cfg.config.prompt_token_budget_soft;
        let prompt_token_hard = cfg.config.prompt_token_budget_hard;
        let old_msg_budget = cfg.config.old_message_char_budget;
        let keep_turns = cfg.config.condense_keep_turns;
        let state_interval = cfg.config.state_block_update_interval as i32;
        let tokenizer_enc = &cfg.config.tokenizer_encoding;

        // Count actual tokens using tiktoken (fast BPE, no network calls),
        // then apply safety factor to account for provider tokenizer mismatch.
        // Include tool definitions in the count since they add 200-300K tokens.
        let token_tools = if tools_def.is_empty() {
            None
        } else {
            Some(tools_def.as_slice())
        };
        let raw_tokens = helpers::count_tokens(&messages, tokenizer_enc, token_tools);
        let safety_factor = cfg.config.prompt_token_safety_factor;
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
            let condense_char_soft = cfg.config.prompt_char_budget_soft;

            match helpers::condense_messages(
                std::mem::take(&mut messages),
                old_msg_budget,
                keep_turns,
                condense_char_soft,
            ) {
                Ok(condensed) => {
                    let condensed_raw =
                        helpers::count_tokens(&condensed, tokenizer_enc, token_tools);
                    let condensed_tokens = (condensed_raw as f64 * safety_factor) as usize;
                    let saved = current_tokens.saturating_sub(condensed_tokens);
                    messages = condensed;

                    // If hard budget triggered, verify we're now below soft
                    let after_raw = helpers::count_tokens(&messages, tokenizer_enc, token_tools);
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
                                    helpers::count_tokens(&tighter, tokenizer_enc, token_tools);
                                let tighter_tokens = (tighter_raw as f64 * safety_factor) as usize;
                                messages = tighter;
                                if tighter_tokens > prompt_token_soft {
                                    warn!(
                                        "[context] Hard condensation could not bring prompt below soft budget: {} effective tokens (budget: {})",
                                        tighter_tokens, prompt_token_soft
                                    );
                                    // Template is NOT stripped — even a small template (~600 tokens)
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
                    // Safety check failed — system messages too large
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

        // ── LLM completion call ──

        let request = CompletionRequest {
            messages: messages.clone(),
            max_tokens: cfg.config.max_tokens,
            temperature: cfg.config.temperature,
            stream: false,
            tools: if tools_def.is_empty() {
                None
            } else {
                Some(tools_def.clone())
            },
        };

        let response = match cfg.llm.completion(request).await {
            Ok(resp) => resp,
            Err(e) => {
                error!("LLM call failed: {:?}", e);
                final_content = format!("I encountered an error: {}", e);
                break;
            }
        };

        // Track cumulative token usage
        helpers::merge_usage(&mut cumulative_usage, response.usage.clone());
        last_response_usage = response.usage.clone();

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
                        < std::env::var("MAX_UNFINISHED_SUBTASK_RETRIES")
                            .ok()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(3u32)
                {
                    unfinished_subtask_retries += 1;
                    let names: Vec<String> = pending_subtasks
                        .iter()
                        .map(|st| format!("#{}: {} ({})", st.id, st.description, st.status))
                        .collect();
                    let feedback = format!(
                        "[Subtask Required] You cannot end this thread while subtasks are still pending. \
                         BEFORE writing your final answer, call `manage_subtasks(action=\"update\", subtask_id=N, status=\"completed\")` \
                         for each subtask you've already finished. If any subtask is no longer needed, use status=\"cancelled\".\n\n\
                         Remaining unfinished subtasks:\n{}\n\n\
                         You will be retried (attempt {}/{}) — use this chance to manage them.",
                        names.join("\n"),
                        unfinished_subtask_retries,
                        std::env::var("MAX_UNFINISHED_SUBTASK_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(3u32),
                    );
                    messages.push(ChatMessage::system(&feedback));
                    info!(
                        "[subtask] Enforcement: LLM tried to end with {} unfinished subtask(s) (retry {}/{})",
                        pending_subtasks.len(),
                        unfinished_subtask_retries,
                        std::env::var("MAX_UNFINISHED_SUBTASK_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(3u32),
                    );
                    // Don't consume from the iteration budget — this is enforcement overhead
                    current_iter -= 1;
                    continue;
                }

                if !pending_subtasks.is_empty() {
                    // Exhausted retries — force the thread to fail
                    warn!(
                        "[subtask] Enforcement exhausted after {} retries — {} subtask(s) still unfinished for thread {}",
                        std::env::var("MAX_UNFINISHED_SUBTASK_RETRIES").ok().and_then(|v| v.parse().ok()).unwrap_or(3u32),
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

            // Normal text response — all subtasks done (or subtask mode off)
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
                // content — indicates a content filter or provider-side stripping.
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
                        "[executor] LLM returned empty content with {} completion tokens (prompt: {}) — likely content filter",
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
                    force_failed = true; // empty response — thread must fail
                    if pending_subtasks.is_empty() {
                        "The LLM returned an empty response with no pending subtasks — likely caused by context explosion.".to_string()
                    } else {
                        format!(
                            "The LLM returned an empty response. The following subtasks were never completed:\n{}",
                            pending_subtasks.iter().map(|st| format!("- #{}: {} ({})", st.id, st.description, st.status)).collect::<Vec<_>>().join("\n"),
                        )
                    }
                } else if reasoning_empty {
                    // No subtask mode, but content AND reasoning are both empty
                    force_failed = true; // empty response — thread must fail
                    "The LLM returned an empty response — likely caused by context explosion."
                        .to_string()
                } else {
                    // Reasoning has content but no response content. Leave
                    // final_content empty — the reasoning is already saved
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
            // non-empty — prevents a false "empty response" detection when
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

        // We have tool calls — add assistant message with tool_calls
        final_tool_call = true;
        let mut assistant_msg = ChatMessage::assistant("");
        assistant_msg.tool_calls = Some(response.tool_calls.clone());
        messages.push(assistant_msg);

        // Per-message token/time from the LLM response that generated these tool calls
        let tool_token_usage = response.usage.map(|u| {
            serde_json::json!({
                "prompt_tokens": u.prompt_tokens,
                "completion_tokens": u.completion_tokens,
                "cached_tokens": u.cached_tokens,
                "reasoning_tokens": u.reasoning_tokens,
            })
        });
        let tool_duration_ms = Some(response.duration_ms as i32);
        let is_multi_tool = response.tool_calls.len() > 1;

        // If multiple tool calls, persist a multi-tool message first (holds time/tokens)
        if is_multi_tool {
            let multi_content = response
                .tool_calls
                .iter()
                .map(|tc| {
                    format!(
                        "{}: {}",
                        cfg.mcp.qualified_name(&tc.function.name),
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
                processing_time_ms: tool_duration_ms,
                token_usage: tool_token_usage.clone(),
                iteration_number: current_iter as i32,
            };
            match helpers::persist_or_abort(&cfg.pool, &multi_msg, thread.id).await {
                helpers::CreateMessageResult::FkViolation => {
                    err_msg!("FK violation — thread {} no longer exists", thread.id);
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

        // Execute each tool call
        for tc in &response.tool_calls {
            let tool_name = tc.function.name.clone();
            let tool_args = tc.function.arguments.clone();

            // Single tool: attach time/tokens to the tool message.
            // Multi-tool: time/tokens are on the multi-tool message, tools get null.
            let (tool_ptime, tool_tu) = if is_multi_tool {
                (None, None)
            } else {
                (tool_duration_ms, tool_token_usage.clone())
            };

            // Persist the tool call as an agent message with msg_type="tool"
            let tool_call_msg = MessageNew {
                thread_id: thread.id,
                role: "agent".to_string(),
                content: tool_args,
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
                msg_type: "tool".to_string(),
                msg_subtype: Some(cfg.mcp.qualified_name(&tool_name)),
                processing_time_ms: tool_ptime,
                token_usage: tool_tu,
                iteration_number: current_iter as i32,
            };
            match helpers::persist_or_abort(&cfg.pool, &tool_call_msg, thread.id).await {
                helpers::CreateMessageResult::FkViolation => {
                    err_msg!("FK violation — thread {} no longer exists", thread.id);
                }
                helpers::CreateMessageResult::OtherError(e) => {
                    error!("Failed to persist tool call '{}': {:?}", tool_name, e)
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

            let mcp_call = McpToolCall {
                id: tc.id.clone(),
                name: tool_name.clone(),
                arguments: serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(serde_json::json!({})),
            };

            let tool_start = std::time::Instant::now();
            let mut tool_ctx = cfg.ctx.clone();
            tool_ctx.current_thread_id = Some(thread.id);
            tool_ctx.current_allowed_tools = prof.allowed_tools.clone();
            let result = cfg.mcp.execute(&mcp_call, tool_ctx).await;
            let tool_elapsed_ms = tool_start.elapsed().as_millis() as i32;

            match result {
                Ok(res) => {
                    // Layer 2: truncate first — DB stores what the LLM will see
                    let content = truncate_content(&res.content, DEFAULT_MAX_TOOL_OUTPUT_CHARS);

                    // Persist the tool result as an agent message with msg_type="tool-result"
                    let tool_result_msg = MessageNew {
                        thread_id: thread.id,
                        role: "agent".to_string(),
                        content: content.clone(),
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
                        msg_type: "tool-result".to_string(),
                        msg_subtype: Some(cfg.mcp.qualified_name(&tool_name)),
                        processing_time_ms: Some(tool_elapsed_ms),
                        token_usage: None,
                        iteration_number: current_iter as i32,
                    };
                    match helpers::persist_or_abort(&cfg.pool, &tool_result_msg, thread.id).await {
                        helpers::CreateMessageResult::FkViolation => {
                            err_msg!("FK violation — thread {} no longer exists", thread.id);
                        }
                        helpers::CreateMessageResult::OtherError(e) => {
                            error!("Failed to persist tool result '{}': {:?}", tool_name, e)
                        }
                        helpers::CreateMessageResult::Success(_) => {}
                    }

                    messages.push(ChatMessage::tool_result(
                        &tc.id,
                        &tc.function.name,
                        &content,
                    ));
                }
                Err(e) => {
                    let err_msg = format!("Error executing tool '{}': {}", tool_name, e);

                    // Persist error as tool result
                    let tool_result_msg = MessageNew {
                        thread_id: thread.id,
                        role: "agent".to_string(),
                        content: err_msg.clone(),
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
                        msg_type: "tool-result".to_string(),
                        msg_subtype: Some(cfg.mcp.qualified_name(&tool_name)),
                        processing_time_ms: Some(tool_elapsed_ms),
                        token_usage: None,
                        iteration_number: current_iter as i32,
                    };
                    match helpers::persist_or_abort(&cfg.pool, &tool_result_msg, thread.id).await {
                        helpers::CreateMessageResult::FkViolation => {
                            err_msg!("FK violation — thread {} no longer exists", thread.id);
                        }
                        helpers::CreateMessageResult::OtherError(e2) => {
                            error!("Failed to persist tool error '{}': {:?}", tool_name, e2)
                        }
                        helpers::CreateMessageResult::Success(_) => {}
                    }

                    messages.push(ChatMessage::tool_result(
                        &tc.id,
                        &tc.function.name,
                        &err_msg,
                    ));
                }
            }
        } // end for tc

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
        // calls — no final answer was produced. Set limit_reached (interrupted)
        // rather than force_failed so the thread is correctly marked as
        // interrupted (can be resumed) instead of failed (dead end).
        final_content = "The task ran out of iterations while still processing tools — no final answer was produced.".to_string();
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
        let mut meta = serde_json::json!({
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
        if let Some(ref assembly) = ctx_assembly_meta {
            meta["context"]["selected_message_ids"] =
                serde_json::json!(assembly.selected_message_ids);
            meta["context"]["wiki_files"] = serde_json::json!(assembly.wiki_files);
            meta["context"]["block_counts"] = serde_json::json!(assembly.block_counts);
            meta["context"]["dropped_blocks"] = serde_json::json!(assembly.dropped_blocks);
            meta["context"]["total_chars"] = serde_json::json!(assembly.total_chars);
        }
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
                processing_time_ms: None,
                token_usage: last_response_usage.as_ref().map(|u| {
                    serde_json::json!({
                        "prompt_tokens": u.prompt_tokens,
                        "completion_tokens": u.completion_tokens,
                        "cached_tokens": u.cached_tokens,
                        "reasoning_tokens": u.reasoning_tokens,
                    })
                }),
                iteration_number: current_iter as i32,
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
                // the corresponding tool results — DeepSeek requires tool_call
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
            max_tokens: cfg.config.thread_summary_tokens,
            temperature: 0.3,
            stream: false,
            tools: None,
        };

        let summary_start = std::time::Instant::now();
        let (summary_text, summary_token_usage) = match cfg.llm.completion(summary_request).await {
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
        let summary_elapsed_ms = summary_start.elapsed().as_millis() as i32;

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
            processing_time_ms: Some(summary_elapsed_ms),
            token_usage: summary_token_usage,
            iteration_number: current_iter as i32,
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
            processing_time_ms: Some(agent_elapsed_ms),
            token_usage: token_usage_json.clone(),
            iteration_number: current_iter as i32,
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
        // Normal completion — the agent's final message IS the summary
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
            processing_time_ms: Some(agent_elapsed_ms),
            token_usage: token_usage_json.clone(),
            iteration_number: current_iter as i32,
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
    // — keep the interrupted status rather than downgrading to failed.
    if enable_subtasks && !force_failed && !limit_reached && final_status == "completed" {
        if let Ok(post_subtasks) = crate::subtask::list_subtasks(&cfg.pool, thread.id).await {
            let unfinished: Vec<_> = post_subtasks
                .iter()
                .filter(|st| st.status == "pending" || st.status == "in_progress")
                .collect();
            if !unfinished.is_empty() {
                warn!(
                    "[subtask] Post-loop enforcement: {} subtask(s) still unfinished for thread {} — forcing failure",
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
            input_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            duration_ms: 0,
        },
    )
    .await?;

    // ── Send completion reaction to platform ──
    if let Some(ref ext_id) = cause_msg.external_id {
        if let Some(ref platform) = channel.platform {
            if let Some(ref resource) = channel.resource_identifier {
                let emoji = match final_status {
                    "completed" => ":white_check_mark:",
                    "failed" => ":x:",
                    "interrupted" => ":broken_heart:",
                    _ => ":o:",
                };
                helpers::enqueue_reaction(
                    &cfg.ctx,
                    platform,
                    resource,
                    ext_id,
                    emoji,
                ).await;
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
        let _ = queries::update_kanban_status(&cfg.pool, task_id, kanban_status).await;
    }

    // 11. Trigger cross-thread summary check
    helpers::check_and_generate_summary(&cfg.pool, &cfg.llm, &cfg.config, thread.channel_id).await;

    Ok(saved)
}
