use crate::agent::config::AgentContext;
use crate::agent::context_builder::PromptParts;
use crate::agent::helpers;
use crate::agent::response_handler::handle_response;
use crate::db::types as queries;
use crate::db::types::{Channel, Message, MessageNew, Thread};
use crate::err_msg;
use crate::error::AppResult;
use crate::llm::{ChatMessage, CompletionRequest, LLMClient, Usage};
use crate::mcp::{
    spill_tool_result, truncate_content, McpToolCall, McpToolResult, DEFAULT_MAX_TOOL_OUTPUT_CHARS,
};
use futures::FutureExt;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

/// Exponential backoff delay for LLM provider retries: base 1s, doubling each
/// attempt (1s/2s/4s), capped at ~8s, with ~+/-30% jitter derived from the
/// system clock (no `rand` dependency).
async fn backoff_delay(attempt: u32) -> Duration {
    let base_ms = 1000u64 << attempt.min(3); // 1s, 2s, 4s, 8s (cap)
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let jitter = (nanos % 600) as u64; // 0..=599 -> +/-30% around the base
    let delay_ms = (base_ms * (700 + jitter)) / 1000;
    Duration::from_millis(delay_ms)
}

/// Extract up to 6 plan steps from plan content (markdown or JSON).
///
/// Real plans are markdown: `<plan>1. step one</plan>` or plain numbered/bulleted
/// lists. We no longer REQUIRE JSON `{"steps": [...]}` — that never matched real
/// plans (every live plan is markdown), so no subtasks were ever auto-created.
/// JSON steps are still honored as a fallback. Priority is preserved: the FIRST
/// step gets the HIGHEST priority.
fn extract_plan_steps(content: &str) -> Vec<String> {
    // 1. JSON fallback: {"steps": ["a", "b"]}
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
        if let Some(steps) = v.get("steps").and_then(|s| s.as_array()) {
            let mut out = Vec::new();
            for s in steps.iter().take(6) {
                if let Some(t) = s.as_str() {
                    let clean = t.trim().trim_end_matches(['*', '`']).trim();
                    if !clean.is_empty() {
                        out.push(clean.to_string());
                    }
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
    }
    // 2. Markdown: extract the <plan>...</plan> block if present (case-insensitive).
    let lower = content.to_lowercase();
    let body = if let Some(start) = lower.find("<plan>") {
        let after = &content[start + "<plan>".len()..];
        if let Some(end_rel) = lower[start + "<plan>".len()..].find("</plan>") {
            &after[..end_rel]
        } else {
            after
        }
    } else {
        content
    };
    // 3. Parse numbered/bulleted lines.
    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let step = if let Some(rest) = trimmed
            .strip_prefix('-')
            .or_else(|| trimmed.strip_prefix('*'))
        {
            rest.trim()
        } else {
            // numbered: "1.", "1)", "1:" optionally followed by space
            let bytes = trimmed.as_bytes();
            let mut i = 0;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i > 0 && i < bytes.len() && matches!(bytes[i], b'.' | b')' | b':') {
                trimmed[i + 1..].trim()
            } else {
                continue;
            }
        };
        let clean = step.trim().trim_end_matches(['*', '`']).trim();
        if !clean.is_empty() && !out.iter().any(|o| o == clean) {
            out.push(clean.to_string());
            if out.len() >= 6 {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod plan_extract_tests {
    use super::*;

    #[test]
    fn markdown_plan_numbered() {
        let content = "<plan>\n1. Read the task body\n2. Implement the change\n3. Run tests\n4. Commit\n</plan>";
        let steps = extract_plan_steps(content);
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0], "Read the task body");
        assert_eq!(steps[2], "Run tests");
    }

    #[test]
    fn markdown_plan_bullets() {
        let content = "<plan>\n- First step\n- Second step\n- Third step\n</plan>";
        let steps = extract_plan_steps(content);
        assert_eq!(steps, vec!["First step", "Second step", "Third step"]);
    }

    #[test]
    fn markdown_plain_list_without_tags() {
        let content = "Plan:\n1. orient\n2. edit\n3. test\n4. commit\n5. push\n6. report\n7. extra";
        let steps = extract_plan_steps(content);
        assert_eq!(steps.len(), 6, "max 6 steps");
        assert_eq!(steps[0], "orient");
        assert_eq!(steps[5], "report");
    }

    #[test]
    fn json_steps_fallback() {
        let content = r#"{"description": "task", "steps": ["a", "b", "c"]}"#;
        let steps = extract_plan_steps(content);
        assert_eq!(steps, vec!["a", "b", "c"]);
    }

    #[test]
    fn empty_and_plain_text() {
        assert!(extract_plan_steps("<plan></plan>").is_empty());
        assert!(extract_plan_steps("no steps here just prose").is_empty());
        assert!(extract_plan_steps("").is_empty());
    }

    #[test]
    fn priority_order_preserved() {
        let content = "<plan>\n1. first\n2. second\n3. third\n</plan>";
        let steps = extract_plan_steps(content);
        assert_eq!(steps[0], "first");
        assert_eq!(steps[2], "third");
    }

    #[test]
    fn markdown_inline_formatting_stripped() {
        let content = "<plan>\n1. **bold step**\n2. `code step`\n</plan>";
        let steps = extract_plan_steps(content);
        assert_eq!(steps.len(), 2);
        assert!(steps[0].contains("bold step"));
        assert!(steps[1].contains("code step"));
    }

    #[test]
    fn dedupes_repeated_lines() {
        let content = "<plan>\n1. same\n2. same\n3. other\n</plan>";
        let steps = extract_plan_steps(content);
        assert_eq!(steps, vec!["same", "other"]);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_main_loop(
    cfg: &AgentContext,
    thread: &Thread,
    cause_msg: &Message,
    channel: &Channel,
    profile_name: &str,
    tool_names: &[String],
    prompt_parts: PromptParts,
    template_section: Option<String>,
    next_seq: &mut i32,
    per_thread_llm: &LLMClient,
    prof: &crate::profile::Profile,
    start_time: std::time::Instant,
) -> AppResult<Message> {
    // Track cumulative token usage across all LLM calls
    let mut cumulative_usage: Option<crate::llm::Usage> = None;
    let mut force_failed: bool = false;
    let mut current_iter: i32;

    // ── Planning Phase ──
    // Plan is a boolean resolved at thread creation time.
    // When true, the agent runs a planning iteration before the main loop.
    // The planning prompt itself is generated by the prompt plugin
    // the executor just orchestrates the calls.
    // Plan mode is decided by the prompt plugin at runtime (complexity-based
    // when the thread has no explicit preference). prompt_parts.plan carries
    // that decision; it also covers threads with an explicit plan setting
    // (the plugin echoes the explicit value back).
    let should_plan = prompt_parts.plan;

    // Snapshot config once for consistency across planning and main loop.
    let cfg_snapshot = cfg.config_snapshot();

    // Per-thread (provider+model) effective config from models.yml:
    // model_config > provider > global settings (token budgets, max_tokens).
    let eff_model_cfg = crate::models_yaml::resolve_effective(
        &cfg.ctx.data_dir,
        &per_thread_llm.config.provider.0,
        &per_thread_llm.config.model,
        &crate::models_yaml::ModelGlobalDefaults {
            token_budget_soft: cfg_snapshot.token_budget_soft,
            token_budget_hard: cfg_snapshot.token_budget_hard,
            max_tokens: cfg_snapshot.max_tokens,
            max_tokens_on_truncation: cfg_snapshot.max_tokens_on_truncation,
        },
    );

    // Whether subtask tools are enabled for the main loop
    let enable_subtasks = should_plan;
    // Pre-read prompt log level for consistency across planning and main loop
    let prompt_log_level = cfg_snapshot.prompt_log_level.clone();
    let prompt_log_level = prompt_log_level.as_str();
    let mut has_logged_first_prompt = false;

    let plan_content: Option<String> = if should_plan {
        let max_iter = 0; // one-shot, no refinement iterations
        let max_tokens = eff_model_cfg.max_tokens; // Option<u32>: None = provider default (planning shares the global output budget)
        let mut last_plan: Option<String> = None;

        'plan: {
            let iter: u32 = 0; // one-shot: no refinement iterations
                               // Build planning messages from prompt parts
                               // User's request goes in context; planning instruction goes in user
            let mut planning_messages = vec![ChatMessage::system(&prompt_parts.system)];
            if !prompt_parts.memory.is_empty() {
                planning_messages.push(ChatMessage::system(&prompt_parts.memory));
            }
            if !prompt_parts.context.is_empty() {
                planning_messages.push(ChatMessage::system(&format!(
                    "=== Context ===\n{}",
                    prompt_parts.context
                )));
            }
            // Inject the task template so the plan is aware of the instructions
            if let Some(ref ts) = template_section {
                planning_messages.push(ChatMessage::system(ts));
            }
            // Include the actual user request (task body for kanban/cron tasks,
            // original message for user threads) so the plan phase sees WHAT the
            // task is — not just the generic planning instruction. The context
            // block also carries the seq-0 cause message (prompt plugin), but
            // this guarantees the request reaches the plan LLM even if context
            // assembly drops it.
            if !prompt_parts.user.is_empty() {
                planning_messages.push(ChatMessage::user(&prompt_parts.user));
            }
            // Output-limit awareness for the plan phase: keep the plan itself
            // within budget; large deliverables get chunked in the execution
            // phase, not emitted in the plan.
            planning_messages.push(ChatMessage::system(&format!(
                "=== Output Limit ===\n\
                 Keep this plan concise. Your maximum output per response is {} \
                 tokens. If a step would produce a very large deliverable \
                 (e.g. a big file), note in the plan that it must be written in \
                 chunks via filesystem_write append=true — never let an output \
                 limit cause failure.",
                fmt_output_budget(max_tokens)
            )));
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
                    let v = *next_seq;
                    *next_seq += 1;
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
                    original_thread_id: None,
                    msg_type: "prompt".to_string(),
                    msg_subtype: Some("plan".to_string()),
                    iteration_number: 0,
                    duration_ms: 0,
                    token_usage: serde_json::json!({}),
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
                    let plan_token_usage = resp
                        .usage
                        .as_ref()
                        .map(|u| {
                            serde_json::json!({
                                "prompt_tokens": u.prompt_tokens,
                                "completion_tokens": u.completion_tokens,
                                "cached_tokens": u.cached_tokens,
                                "reasoning_tokens": u.reasoning_tokens,
                            })
                        })
                        .unwrap_or(serde_json::json!({}));
                    let plan_duration_ms = resp.duration_ms as i32;
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
                                let v = *next_seq;
                                *next_seq += 1;
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
                            original_thread_id: None,
                            msg_type: "plan".to_string(),
                            msg_subtype: Some("markdown".to_string()),
                            iteration_number: 1,
                            duration_ms: plan_duration_ms,
                            token_usage: plan_token_usage,
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

                    // For complex tasks, auto-create subtasks from the plan content.
                    // Plans are markdown (`<plan>1. step</plan>`), not JSON: parse
                    // numbered/bulleted lines (max 6, priority preserved). JSON
                    // `{"steps": [...]}` plans are still honored as a fallback.
                    // No force-fail: a plan with no parseable steps simply skips
                    // subtask auto-create (a markdown plan is never an error).
                    if enable_subtasks && plan_content.len() > 100 {
                        let steps = extract_plan_steps(&plan_content);
                        if steps.is_empty() {
                            warn!(
                                "[plan] No parseable steps in plan for thread {} — skipping subtask auto-create",
                                thread.id
                            );
                        } else {
                            let total = steps.len();
                            for (i, step) in steps.iter().enumerate() {
                                let priority = (total - i) as i32;
                                if let Err(e) = crate::subtask::add_subtask(
                                    &cfg.pool, thread.id, step, priority,
                                )
                                .await
                                {
                                    warn!("[plan] Failed to create subtask '{}': {:?}", step, e);
                                } else {
                                    info!(
                                        "[plan] Created subtask '{}' for complex thread {}",
                                        step, thread.id
                                    );
                                }
                            }
                        }
                    }
                    last_plan = Some(plan_content);

                    // One-shot: no refinement iterations: plan is final
                    break 'plan;
                }
                Err(e) => {
                    warn!(
                        "[plan] Failed to generate plan for thread {}: {:?}",
                        thread.id, e
                    );
                    break 'plan;
                }
            }
        }

        last_plan
    } else {
        None
    };

    // 5. Assemble messages from prompt parts
    // Inverse role mapping (R7): for tester/reviewer STEP threads the role
    // template (dev-tester/dev-reviewer) is the USER prompt, and the task
    // description (title + body carried in the cause message) is the SYSTEM
    // prompt — the opposite of the executor layout (template = system,
    // task body = user). The step-thread cause message carries the task
    // description; template_section carries the role template.
    let is_step_thread = matches!(thread.workflow_step.as_deref(), Some("testing" | "review"));
    let mut messages = vec![ChatMessage::system(&prompt_parts.system)];
    if !prompt_parts.memory.is_empty() {
        messages.push(ChatMessage::system(&prompt_parts.memory));
    }

    // Inject task template FIRST (right after system prompt): highest instruction priority
    // for template-backed tasks (kanban/cron with template).
    // Flush-left position ensures the template guides the model before any other context.
    // For step threads the template is deferred to the USER slot (see below).
    if let Some(ref template_section) = template_section {
        if !is_step_thread {
            messages.push(ChatMessage::system(template_section));
        }
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

    // Step threads: task description goes in the SYSTEM slot, the role
    // template in the USER slot (inverse of the executor layout).
    if is_step_thread {
        messages.push(ChatMessage::system(&format!(
            "=== Task Description ===\n{}",
            prompt_parts.user
        )));
        if let Some(ref template_section) = template_section {
            messages.push(ChatMessage::user(template_section));
        } else {
            messages.push(ChatMessage::user(&prompt_parts.user));
        }
    } else {
        // Add the user message (from the prompt parts: the plugin provides this)
        messages.push(ChatMessage::user(&prompt_parts.user));
    }

    // ── Truncation escalation (global max_tokens_on_truncation) ──
    // Normal LLM calls use the configured `max_tokens` budget (None = no cap:
    // the provider's own default applies — no max_tokens sent in the request).
    // When the provider reports finish_reason=length (the output ceiling was
    // hit), the retry uses the escalated budget `max_tokens_on_truncation`
    // (also optional) with the truncated reasoning preserved; a second
    // consecutive truncation fails fast (no third retry) and FORCES the
    // thread to fail. With no caps configured anywhere, a truncation still
    // retries once, then fails fast — the safety valve stays.
    let mut escalated_max_tokens: Option<u32> = None;
    let mut truncation_escalated: bool = false;
    let base_max_tokens: Option<u32> = eff_model_cfg.max_tokens;
    let max_tokens_on_truncation: Option<u32> = eff_model_cfg.max_tokens_on_truncation;

    // Output-limit awareness: tell the model its per-response output ceiling so
    // it plans large deliverables (big file writes, long reports) in chunks
    // instead of hitting finish_reason=length and failing. Chunked writes use
    // filesystem_write with append=true for subsequent parts (see TOOL_GUIDANCE
    // rule 3 in the prompt plugin).
    // Use the EFFECTIVE budget for the current attempt so the hint matches
    // the actual output ceiling on truncation retries (escalated value).
    let max_output_tokens = effective_max_tokens(escalated_max_tokens, base_max_tokens);
    messages.push(ChatMessage::system(&format!(
        "=== Output Limit ===\n\
         Your maximum output per response is {} tokens. If a single tool call \
         (e.g. writing a large file) or your final answer would exceed this, \
         SPLIT the work across multiple calls: write the first chunk with \
         filesystem_write (append=false), then append the remaining chunks with \
         append=true. Never abandon a task because of the output limit — chunk \
         the output instead.",
        fmt_output_budget(max_output_tokens)
    )));

    // 5. Build tool definitions from the profile's allowed tools
    let tools_def = cfg
        .plugin_manager
        .snapshot_registry()
        .await
        .to_openai_tools(&prof.allowed_tools);

    // 6. Tool-calling loop: max iterations controls total LLM calls
    // Use the plugin's runtime plan decision for the iteration budget too,
    // so complex tasks get the plan budget (max_iterations_plan) and simple
    // ones stay within max_iterations_no_plan.
    let iter_limit =
        queries::max_iterations_for_plan(&cfg.config_snapshot(), prompt_parts.plan) as i32;
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
    // How many consecutive LLM errors (provider errors, truncation,
    // empty responses) we tolerate before marking the thread failed.
    // A correct (non-error) response resets the counter to 0. The limit
    // comes from config `provider_max_retries` (default 3); MAX_LLM_RETRIES
    // is the fallback when the setting is 0. This bounds token waste even
    // if a tool misbehaves (e.g. compaction) — we stop after this many
    // consecutive errors instead of burning tokens re-sending a bloated
    // context.
    const MAX_LLM_RETRIES: u32 = 3;
    let llm_max_retries = {
        let configured = cfg.config_snapshot().provider_max_retries;
        if configured > 0 {
            configured
        } else {
            MAX_LLM_RETRIES
        }
    };
    let mut llm_error_retries: u32 = 0;
    // Track when condensation last occurred so soft-budget triggers use
    // iteration-since-last-condense rather than a fixed modulo schedule.
    // This prevents aggressive condensation on every Nth iteration even when
    // the last condense just happened.
    let mut last_condense_iteration: i32 = 0;
    // Sub-prompts (feature): cumulative char budget + exhaustion flag for
    // appended pending user prompts, scoped to this thread run (persisted
    // across iterations of the same run).
    let mut used_sub_prompt_chars: usize = 0;
    let mut sub_prompts_exhausted: bool = false;

    // WS-4b: engine-level read guard — (tool, args-hash) -> (iteration, len)
    // for read-only tools. Cleared whenever a state-changing tool runs.
    let mut read_guard: std::collections::HashMap<(String, u64), (u32, usize)> =
        std::collections::HashMap::new();
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

        // ── Sub-prompts: append pending user prompts to this running thread ──
        // When a channel has a user task RUNNING and there are PENDING user
        // tasks for the same channel/profile/parent-context (or children of
        // this thread), their prompts are appended to THIS thread's full
        // prompt — BEFORE the condense call so compaction never drops them.
        // Each pending thread is marked skipped and a sub_cause message
        // records the original thread id (messages.original_thread_id).
        // Gates: iteration-percent (feature enabled when > 0; lookups only
        // within the first N% of the iteration budget) + cumulative char
        // budget (sub_prompt_max_chars per running thread).
        let sub_prompt_enabled =
            cfg_snapshot.sub_prompt_iteration_percent > 0 && cfg_snapshot.sub_prompt_max_chars > 0;
        if sub_prompt_enabled
            && !sub_prompts_exhausted
            && thread.cause == "user"
            && sub_prompt_gate_ok(
                current_iter,
                iter_limit,
                cfg_snapshot.sub_prompt_iteration_percent,
            )
        {
            match queries::list_appendable_pending_threads(
                &cfg.pool,
                &thread.channel_id,
                &thread.profile,
                thread.id,
            )
            .await
            {
                Ok(pending) => {
                    for pt in pending {
                        if used_sub_prompt_chars >= cfg_snapshot.sub_prompt_max_chars {
                            sub_prompts_exhausted = true;
                            break;
                        }
                        // Read the pending thread's cause (seq-0) prompt.
                        let prompt_text = match queries::get_thread_messages(&cfg.pool, pt.id).await
                        {
                            Ok(msgs) => msgs
                                .iter()
                                .find(|m| m.thread_sequence == 0)
                                .map(|m| m.content.clone())
                                .unwrap_or_default(),
                            Err(e) => {
                                warn!(
                                    "[sub-prompt] Failed to read cause of pending thread #{}: {:?}",
                                    pt.id, e
                                );
                                continue;
                            }
                        };
                        if prompt_text.trim().is_empty() {
                            continue;
                        }
                        let appended = format!(
                            "=== Sub-Prompt (from thread #{}, appended) ===\n{}",
                            pt.id, prompt_text
                        );
                        let next_used = used_sub_prompt_chars + appended.chars().count();
                        if next_used > cfg_snapshot.sub_prompt_max_chars {
                            sub_prompts_exhausted = true;
                            break;
                        }
                        // Record the sub_cause message (msg_type='sub_cause',
                        // msg_subtype + original_thread_id = pending id) and
                        // mark the pending thread skipped (terminal choke point).
                        if let Err(e) = queries::insert_sub_cause_message(
                            &cfg.pool,
                            thread.id,
                            pt.id,
                            &appended,
                            current_iter,
                        )
                        .await
                        {
                            warn!(
                                "[sub-prompt] Failed to record sub_cause for thread #{}: {:?}",
                                pt.id, e
                            );
                            continue;
                        }
                        if let Err(e) =
                            queries::mark_thread_skipped_for_sub_prompt(&cfg.pool, pt.id).await
                        {
                            warn!(
                                "[sub-prompt] Failed to mark pending thread #{} skipped: {:?}",
                                pt.id, e
                            );
                        }
                        // Push into the in-memory prompt BEFORE condensation.
                        messages.push(ChatMessage::user(&appended));
                        used_sub_prompt_chars = next_used;
                        info!(
                                "[sub-prompt] Appended prompt from pending thread #{} to running thread #{} ({} chars)",
                                pt.id, thread.id, appended.chars().count(),
                            );
                    }
                }
                Err(e) => {
                    warn!(
                            "[sub-prompt] Failed to list appendable pending threads for thread #{}: {:?}",
                            thread.id, e
                        );
                }
            }
        }

        // ── Context management: call condense tool ──
        // Before each LLM call, invoke the configured condense MCP tool.
        // The tool (plugin-specific) decides whether to condense based on
        // its own thresholds (configurable via plugin config). The agent
        // is agnostic to condensation logic : it passes messages and
        // iteration info and applies whatever the tool returns.
        // WS-2/WS-3: durable thread dir (notes + context dumps).
        let thread_dir = std::path::Path::new(&cfg.ctx.data_dir)
            .join("data")
            .join("threads")
            .join(thread.id.to_string());
        let mut was_compacted = false;
        let mut dump_file: Option<String> = None;
        let mut dump_entries = 0usize;
        let condense_tool = cfg_snapshot.compact_messages_tool_name.clone();
        if !condense_tool.is_empty() {
            let condense_call = McpToolCall {
                name: condense_tool.clone(),
                arguments: serde_json::json!({
                    "messages": messages,
                    "current_iteration": current_iter,
                    "last_condense_iteration": last_condense_iteration,
                    "thread_dir": thread_dir,
                    "soft_budget": eff_model_cfg.token_budget_soft,
                    "hard_budget": eff_model_cfg.token_budget_hard,
                }),
                id: String::new(),
            };
            match cfg
                .plugin_manager
                .snapshot_registry()
                .await
                .execute(&condense_call, cfg.ctx.clone())
                .await
            {
                Ok(res) => {
                    if res.is_error {
                        warn!(
                            "[context] Condense tool '{}' raised an error: {} : continuing without condensation",
                            condense_tool, res.content
                        );
                    } else if let Ok(result) =
                        serde_json::from_str::<serde_json::Value>(&res.content)
                    {
                        // Contract: the tool returns the compacted messages array
                        // (apply it) OR null/absent (no change). The core is
                        // deliberately AGNOSTIC: it applies whatever the tool
                        // returns without verifying. The tool alone decides when
                        // compaction happens and whether it succeeded — it may
                        // compact by chars or by tokens (tokenizer-dependent),
                        // so the core cannot and must not re-check correctness.
                        if let Some(condensed) = result.get("messages").and_then(|v| v.as_array()) {
                            let before = result
                                .get("before_count")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let after = result
                                .get("after_count")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            was_compacted = result
                                .get("was_compacted")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(after < before);
                            dump_file = result
                                .get("dump_file")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            dump_entries =
                                result.get("entries").and_then(|v| v.as_u64()).unwrap_or(0)
                                    as usize;
                            messages =
                                serde_json::from_value(serde_json::Value::Array(condensed.clone()))
                                    .unwrap_or(messages);
                            last_condense_iteration = current_iter;
                            info!(
                                "[context] Condensed messages via {}: {} → {} (iteration {})",
                                condense_tool, before, after, current_iter,
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "[context] Condense tool '{}' failed: {} : continuing without condensation",
                        condense_tool, e
                    );
                }
            }
        }

        // WS-4c: budget hint every iteration (anti-death-spiral backstop).
        helpers::upsert_system_message(
                &mut messages,
                "=== Budget ===",
                format!(
                    "=== Budget ===\nIteration {}/{}.\nRemaining: {}.\nIf remaining < 20, stop exploring and start producing.",
                    current_iter,
                    iter_limit,
                    (iter_limit - current_iter).max(0)
                ),
            );
        // WS-3: durable working notes survive compaction — injected every
        // iteration AFTER condense so notes are always in context.
        if let Ok(notes_content) = std::fs::read_to_string(thread_dir.join("notes.md")) {
            let notes_total = notes_content.chars().count();
            let notes_content = if notes_total > 8192 {
                let head: String = notes_content.chars().take(8192).collect();
                format!(
                    "{head}\n[note truncated: showing chars 0-8192 of {notes_total} total chars]"
                )
            } else {
                notes_content
            };
            if !notes_content.trim().is_empty() {
                helpers::upsert_system_message(
                    &mut messages,
                    "=== Working Notes (durable) ===",
                    format!("=== Working Notes (durable) ===\n{notes_content}"),
                );
            }
        }
        // WS-5: ENGINE auto-notes — read-type tool results are auto-saved to
        // auto-notes.md by prune/compact before their context copy is
        // destroyed. Inject the TAIL (most recent reads first) so the agent
        // always remembers what it read, even if it never wrote a note
        // itself (thread 700: zero notes + 117 re-reads of the same ranges).
        if let Ok(auto_notes_content) = std::fs::read_to_string(thread_dir.join("auto-notes.md")) {
            let auto_total = auto_notes_content.chars().count();
            let auto_notes_content = if auto_total > 12000 {
                let tail_start = auto_total.saturating_sub(12000);
                let tail: String = auto_notes_content.chars().skip(tail_start).collect();
                format!(
                    "{tail}\n[auto-notes truncated: showing last 12000 of {auto_total} total chars]"
                )
            } else {
                auto_notes_content
            };
            if !auto_notes_content.trim().is_empty() {
                helpers::upsert_system_message(
                    &mut messages,
                    "=== Auto-Saved Reads (engine) ===",
                    format!("=== Auto-Saved Reads (engine) ===\n{auto_notes_content}"),
                );
            }
        }
        // WS-3: compaction notice — never re-read the dump (rule 12).
        if was_compacted {
            helpers::upsert_system_message(
                    &mut messages,
                    "=== Context Compacted",
                    format!(
                        "=== Context Compacted (iteration {current_iter}) ===\nDump: {} ({} entries).\nNever re-read context-{current_iter}.json — rule 12.",
                        dump_file.as_deref().unwrap_or("context dump"),
                        dump_entries
                    ),
                );
        }

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
                let v = *next_seq;
                *next_seq += 1;
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
                    "condensed": current_iter == last_condense_iteration,
                }),
                embedding: None,
                summary_text: None,
                is_summary: false,
                original_thread_id: None,
                msg_type: "prompt".to_string(),
                msg_subtype: Some(prompt_subtype.to_string()),
                iteration_number: current_iter,
                duration_ms: 0,
                token_usage: serde_json::json!({}),
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
            max_tokens: effective_max_tokens(escalated_max_tokens, base_max_tokens),
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
                llm_error_retries += 1;
                if llm_error_retries >= llm_max_retries {
                    warn!(
                        "[executor] LLM provider failed {} consecutive time(s) (max {}) for thread {}: {:?}; marking thread failed",
                        llm_error_retries, llm_max_retries, thread.id, e,
                    );
                    final_content = format!(
                        "The LLM provider returned an error {} consecutive times (max {}). Last error: {}. The thread was marked as failed.",
                        llm_error_retries, llm_max_retries, e,
                    );
                    force_failed = true;
                    break;
                }
                info!(
                    "[executor] LLM provider error (attempt {}/{}): retrying for thread {}",
                    llm_error_retries, llm_max_retries, thread.id,
                );
                // Don't consume from the iteration budget for provider retries.
                current_iter -= 1;
                if let Some(retry_after) = e.retry_after_secs() {
                    // Rate-limited (HTTP 429): honor Retry-After, capped at 60s.
                    let wait = Duration::from_secs(retry_after.min(60));
                    info!(
                        "[executor] LLM provider rate-limited (HTTP 429): sleeping {}s before retry (thread {})",
                        wait.as_secs(), thread.id,
                    );
                    tokio::time::sleep(wait).await;
                } else {
                    // Exponential backoff with jitter so a down provider isn't hammered.
                    tokio::time::sleep(backoff_delay(llm_error_retries).await).await;
                }
                continue;
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
            // ── Truncation escalation (global max_tokens_on_truncation) ──
            // finish_reason=length means the provider hit the output ceiling
            // before the model could emit its action/answer (e.g. reasoning
            // consumed the whole budget). First truncation → retry ONCE with
            // the escalated budget (max_tokens_on_truncation), preserving the
            // truncated reasoning so the model does NOT re-derive it. Second
            // consecutive truncation → fail fast (give up truthfully; no
            // third retry with the same budget).
            let truncated = response
                .finish_reason
                .as_deref()
                .map(|f| f == "length")
                .unwrap_or(false);
            match truncation_action(truncation_escalated, truncated) {
                TruncationAction::Escalate => {
                    escalated_max_tokens = max_tokens_on_truncation;
                    truncation_escalated = true;
                    info!(
                        "[executor] response truncated (finish_reason=length, attempt 1/2): retrying with escalated max_tokens={} (thread {})",
                        fmt_output_budget(max_tokens_on_truncation), thread.id,
                    );
                    // Reasoning-forward: preserve the truncated reasoning and
                    // any partial content, then nudge for a SHORTER response.
                    messages.extend(truncation_retry_messages(
                        response.reasoning.as_deref(),
                        &response.content,
                    ));
                    // The Output Limit hint must match the real ceiling on
                    // the retry, not the original small budget.
                    helpers::upsert_system_message(
                        &mut messages,
                        "=== Output Limit ===",
                        format!(
                            "=== Output Limit ===\nYour maximum output per response is {} tokens (escalated from {}). \
                             If a single tool call (e.g. writing a large file) or your final answer would exceed this, \
                             SPLIT the work across multiple calls: write the first chunk with filesystem_write \
                             (append=false), then append the remaining chunks with append=true. Never abandon a task \
                             because of the output limit — chunk the output instead.",
                            fmt_output_budget(max_tokens_on_truncation), fmt_output_budget(base_max_tokens),
                        ),
                    );
                    // Don't consume the iteration budget for this retry overhead.
                    current_iter -= 1;
                    tokio::time::sleep(backoff_delay(1).await).await;
                    continue;
                }
                TruncationAction::FailFast => {
                    warn!(
                        "[executor] response truncated by token budget 2 consecutive times (including once with escalated max_tokens={}) for thread {}: giving up truthfully",
                        fmt_output_budget(max_tokens_on_truncation), thread.id,
                    );
                    final_content = format!(
                        "The response was truncated by the output token limit twice (attempt 2/2, including once with the escalated budget of {} tokens). Giving up truthfully.",
                        fmt_output_budget(max_tokens_on_truncation),
                    );
                    final_tool_call = false;
                    // Truncated twice: give up truthfully, but the thread MUST
                    // fail (status "failed" → blocked / review_on_fail), never
                    // complete and advance to the tester.
                    force_failed = true;
                    break;
                }
                TruncationAction::Continue => {
                    // Successful (non-truncated) response: reset the escalation
                    // state so a later truncation escalates from the base budget.
                    escalated_max_tokens = None;
                    truncation_escalated = false;
                }
            }

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
                         BEFORE writing your final answer, call `subtasks_manage-subtasks(action=\"update\", subtask_id=N, status=\"completed\")` \
                         for each subtask you've already finished. If any subtask is no longer needed, use status=\"cancelled\".\n\n\
                         Remaining unfinished subtasks:\n{}\n\n\
                         You will be retried (attempt {}/{}): use this chance to manage them.",
                        names.join("\n"),
                        unfinished_subtask_retries,
                        max_retries,
                    );
                    messages.push(ChatMessage::user(&feedback));
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
                    llm_error_retries += 1; // empty response counts as an LLM error
                    if llm_error_retries >= llm_max_retries {
                        warn!(
                            "[executor] LLM returned empty response {} consecutive time(s) (max {}) for thread {}: marking thread failed",
                            llm_error_retries, llm_max_retries, thread.id,
                        );
                        force_failed = true; // empty response: thread must fail
                        if pending_subtasks.is_empty() {
                            "The LLM returned an empty response with no pending subtasks: likely caused by context explosion.".to_string()
                        } else {
                            format!(
                                "The LLM returned an empty response. The following subtasks were never completed:\n{}",
                                pending_subtasks.iter().map(|st| format!("- #{}: {} ({})", st.id, st.description, st.status)).collect::<Vec<_>>().join("\n"),
                            )
                        }
                    } else {
                        info!(
                            "[executor] LLM empty response (attempt {}/{}): retrying with a nudge (thread {})",
                            llm_error_retries, llm_max_retries, thread.id,
                        );
                        messages.push(ChatMessage::system(&format!(
                            "[System] Your previous response was empty (attempt {}/{}). \
                             Emit your next tool call, or if the task is complete, write your final answer.",
                            llm_error_retries, llm_max_retries,
                        )));
                        // Don't consume the iteration budget for this retry overhead.
                        current_iter -= 1;
                        tokio::time::sleep(backoff_delay(llm_error_retries).await).await;
                        continue;
                    }
                } else if reasoning_empty {
                    // No subtask mode, but content AND reasoning are both empty
                    llm_error_retries += 1; // empty response counts as an LLM error
                    if llm_error_retries >= llm_max_retries {
                        warn!(
                            "[executor] LLM returned empty response {} consecutive time(s) (max {}) for thread {}: marking thread failed",
                            llm_error_retries, llm_max_retries, thread.id,
                        );
                        force_failed = true; // empty response: thread must fail
                        "The LLM returned an empty response: likely caused by context explosion."
                            .to_string()
                    } else {
                        info!(
                            "[executor] LLM empty response (attempt {}/{}): retrying with a nudge (thread {})",
                            llm_error_retries, llm_max_retries, thread.id,
                        );
                        messages.push(ChatMessage::system(&format!(
                            "[System] Your previous response was empty (attempt {}/{}). \
                             Emit your next tool call, or if the task is complete, write your final answer.",
                            llm_error_retries, llm_max_retries,
                        )));
                        // Don't consume the iteration budget for this retry overhead.
                        current_iter -= 1;
                        tokio::time::sleep(backoff_delay(llm_error_retries).await).await;
                        continue;
                    }
                } else {
                    // Reasoning has content but no response content and no
                    // tool calls. A reasoning-only response with no tool
                    // call is a TERMINAL state for the agent: the model has
                    // decided to stop. We do NOT nudge or retry — forcing a
                    // stopped model to continue produces degraded or
                    // fabricated continuations. Leave final_content empty:
                    // the post-loop fallback reports the give-up truthfully
                    // (thread fails) and the reasoning is saved separately
                    // as a `reasoning` message (step 8 below).
                    //
                    // Genuine truncation (finish_reason=length) is handled above the
                    // subtask/content handling: it escalates the output budget once,
                    // then fails fast — it never reaches this voluntary-stop path.

                    // Voluntary stop: terminal. Empty final_content triggers
                    // the truthful give-up fallback after the loop.
                    String::new()
                }
            } else {
                // Correct response with content (loop ends right after this,
                // so no counter reset needed here — the tool-call path below
                // resets it for iterations that continue).
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
        // A tool-calling response is correct: reset the consecutive-error counter.
        llm_error_retries = 0;
        final_tool_call = true;
        let mut assistant_msg = ChatMessage::assistant("");
        assistant_msg.tool_calls = Some(response.tool_calls.clone());
        // Echo reasoning back to providers that require the round-trip
        // (e.g. opencode-go / DeepSeek in thinking mode).
        assistant_msg.reasoning_content = response.reasoning.clone();
        messages.push(assistant_msg);

        // Persist a message showing what tool(s) the agent called
        // (single tool → msg_type: \"tool\", batch → msg_type: \"multi-tool\")
        // Previously only multi-tool was persisted; single tool calls were invisible in the thread.
        let tool_content = response
            .tool_calls
            .iter()
            .map(|tc| format!("{}: {}", tc.function.name.clone(), tc.function.arguments))
            .collect::<Vec<_>>()
            .join("\n");

        let tool_msg_type = if response.tool_calls.len() > 1 {
            "multi-tool"
        } else {
            "tool"
        };

        let tool_call_msg = MessageNew {
            thread_id: thread.id,
            role: "agent".to_string(),
            content: tool_content,
            thread_sequence: {
                let v = *next_seq;
                *next_seq += 1;
                v
            },
            external_id: None,
            metadata: serde_json::json!({}),
            embedding: None,
            summary_text: None,
            is_summary: false,
            original_thread_id: None,
            msg_type: tool_msg_type.to_string(),
            msg_subtype: None,
            iteration_number: current_iter,
            duration_ms: response.duration_ms as i32,
            token_usage: response
                .usage
                .as_ref()
                .map(|u| {
                    serde_json::json!({
                        "prompt_tokens": u.prompt_tokens,
                        "completion_tokens": u.completion_tokens,
                        "cached_tokens": u.cached_tokens,
                        "reasoning_tokens": u.reasoning_tokens,
                    })
                })
                .unwrap_or(serde_json::json!({})),
        };
        match helpers::persist_or_abort(&cfg.pool, &tool_call_msg, thread.id).await {
            helpers::CreateMessageResult::FkViolation => {
                err_msg!("FK violation: thread {} no longer exists", thread.id);
            }
            helpers::CreateMessageResult::OtherError(e) => {
                error!("Failed to persist tool call message: {:?}", e)
            }
            helpers::CreateMessageResult::Success(ref saved) => {
                helpers::enqueue_delivery(
                    &cfg.ctx,
                    saved,
                    channel,
                    thread,
                    cause_msg.external_id.clone(),
                )
                .await;
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
                let v = *next_seq;
                *next_seq += 1;
                v
            })
            .collect();

        let pool = cfg.pool.clone();
        // mcp_registry removed - use cfg.plugin_manager instead
        let mut join_set = JoinSet::new();

        let mut tool_results: Vec<Option<(String, String, String)>> =
            vec![None; response.tool_calls.len()];
        for (idx, tc) in response.tool_calls.iter().enumerate() {
            let tool_name = tc.function.name.clone();
            let tool_args = tc.function.arguments.clone();
            let tc_id = tc.id.clone();

            // WS-4b: exact-repeat read guard for read-only tools.
            let args_hash = helpers::hash_tool_args(&tool_args);
            let guard_key = (tool_name.clone(), args_hash);
            if helpers::is_guarded_read_only(&tool_name) {
                if let Some((guard_iter, _len)) = read_guard.get(&guard_key) {
                    tool_results[idx] = Some((
                        tc_id.clone(),
                        tool_name.clone(),
                        format!(
                            "[duplicate of {tool_name} at iteration {guard_iter} — see your notes; re-reading the same input is forbidden by rule 11]"
                        ),
                    ));
                    continue;
                }
                read_guard.insert(guard_key, (current_iter as u32, 0));
            } else {
                read_guard.clear();
            }
            let qualified_name = tool_name.clone(); // qualified_name is identity, no registry needed

            let mcp_call = McpToolCall {
                id: tc.id.clone(),
                name: tool_name.clone(),
                arguments: serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(serde_json::json!({})),
            };

            let mut tool_ctx = cfg.ctx.clone();
            tool_ctx.current_thread_id = Some(thread.id);
            tool_ctx.current_channel_id = Some(thread.channel_id.clone());
            tool_ctx.current_profile_name = Some(profile_name.to_string());
            tool_ctx.current_channel_name = Some(channel.name.clone());
            tool_ctx.current_platform = channel.platform.clone();
            tool_ctx.current_allowed_tools = prof.allowed_tools.clone();

            let pool = pool.clone();
            let pm = cfg.plugin_manager.clone();
            let seq = result_seqs[idx];
            let tid = thread.id;
            let iter_num = current_iter;

            // --- Phase 1: Read per-tool timeout from registry ---
            // Snapshot the registry outside the spawned task so we only read the lock once.
            // `None` = NO timeout: the tool runs until it finishes, errors, or the
            // agent cancels it (background tasks give full tracking control).
            let mcp_snapshot = pm.snapshot_registry().await;
            let timeout_secs = mcp_snapshot.get_timeout_secs(&tool_name);
            let timeout_dur = timeout_secs.map(std::time::Duration::from_secs);

            // Snapshot bg threshold BEFORE entering the spawned closure (cfg ref issue)
            let bg_threshold_secs = cfg.config_snapshot().tool_bg_secs;
            let bg_threshold = std::time::Duration::from_secs(bg_threshold_secs);
            // Snapshot spill config BEFORE entering the spawned closure (cfg ref issue)
            let spill_cfg = cfg.config_snapshot();
            let spill_root = std::path::PathBuf::from(spill_cfg.spill_dir);
            let max_inline_chars = spill_cfg.max_inline_chars;
            let is_multi_tool = tool_count > 1;

            // --- Phase 1.5: Self-restart guard (P2 #6) ---
            // An agent must never restart the container it runs inside: a
            // `docker compose restart/down/stop/rm/up` against its own stack
            // kills its own thread (thread 488 self-kill). Block destructive
            // verbs that target the omni stack directory (where the agent's
            // own compose project lives).
            let mut self_restart_block: Option<String> = None;
            if tool_name == "docker_compose" {
                if let Ok(args_val) =
                    serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                {
                    let cmd = args_val
                        .get("command")
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    let verb = cmd.split_whitespace().next().unwrap_or("");
                    if matches!(verb, "restart" | "down" | "stop" | "rm" | "kill" | "up") {
                        let target_project = args_val
                            .get("project_dir")
                            .and_then(|p| p.as_str())
                            .unwrap_or("");
                        // The omni stack is the agent's own runtime: its compose
                        // project files live under an omni-stack directory.
                        let targets_own_stack = target_project.contains("omni-stack");
                        if targets_own_stack {
                            self_restart_block = Some(format!(
                                "Blocked: docker_compose '{verb}' targets the omni-stack (project_dir '{target_project}') \
                                 you run inside. Restarting your own container kills this thread. Only Hermes may restart the stack.",
                            ));
                        }
                    }
                }
            }

            let self_restart_block_for_task = self_restart_block.clone();
            let panic_idx = idx;
            let panic_tc_id = tc_id.clone();
            let panic_tool_name = tool_name.clone();
            join_set.spawn(async move {
                let task_result = std::panic::AssertUnwindSafe(async move {
                    // Phase 1.5 guard: if this docker_compose call would restart the
                // agent's own stack, return a synthetic error result instead of
                // executing it — the message plumbing below records it as a
                // tool result with is_error=true so the model sees the block.
                if let Some(block_msg) = self_restart_block_for_task {
                    return (
                        idx,
                        tc_id.clone(),
                        tool_name.clone(),
                        block_msg,
                        true, // is_error
                    );
                }

                // Execute with short timeout (fast path) + background fallback.
                // Builtin task tools (wait/poll/cancel/read-task-logs) are the
                // INTERFACE to the background system — they must never be
                // backgrounded themselves. wait-task declares timeout_secs=310
                // and blocks polling; applying the 5s bg switch to it would
                // return a NEW task_id instead of the awaited result, so the
                // agent loops forever waiting on a task that never resolves
                // (deploy Groups 13/14 regression). External tools get the
                // bg-threshold switch so long operations run in background.
                //
                // The tool future is created ONCE with owned data so the bg
                // fallback can hand the SAME in-flight request to the spawned
                // task. Sending the call twice (as an earlier implementation
                // did) made serial MCP plugins like docker_compose execute the
                // command TWICE: the fast-path future was dropped but its
                // request was already executing at the plugin, and the re-sent
                // request queued behind it — the bg task resolved only after
                // the second execution, or never when the agent re-dispatched
                // repeatedly (each retry queued another duplicate).
                let bg_mcp_call = mcp_call.clone();
                let bg_mcp_snapshot = mcp_snapshot.clone();
                let mut tool_future = Box::pin(async move {
                    bg_mcp_snapshot.execute(&bg_mcp_call, tool_ctx).await
                });

                let is_builtin_task_tool = matches!(
                    tool_name.as_str(),
                    "builtin_wait-task"
                        | "builtin_poll-task"
                        | "builtin_cancel-task"
                        | "builtin_read-task-logs"
                        | "builtin_read-attached-file"
                );

                let result = if is_builtin_task_tool {
                    // Run synchronously with the tool's own declared timeout
                    // (wait-task declares 310s; poll/cancel/read-task-logs are
                    // fast). If the tool declares NO timeout, await it directly
                    // — the tool decides when it's done.
                    match timeout_dur {
                        Some(dur) => {
                            match tokio::time::timeout(dur, tool_future.as_mut()).await {
                                Ok(result) => result,
                                Err(_) => Ok(McpToolResult {
                                    call_id: tc_id.clone(),
                                    content: format!(
                                        "Tool '{}' timed out after {}s",
                                        tool_name,
                                        dur.as_secs()
                                    ),
                                    is_error: true,
                                }),
                            }
                        }
                        None => tool_future.as_mut().await,
                    }
                } else {
                match tokio::time::timeout(bg_threshold, tool_future.as_mut()).await {
                    Ok(result) => result,
                    Err(_elapsed) => {
                        // Short timeout exceeded : switch to background mode.
                        // Register the tool in the task registry for polling.
                        let registry = crate::agent::task_registry::TASK_REGISTRY
                            .get()
                            .cloned()
                            .expect("TASK_REGISTRY not initialized");
                        let (task_id, abort_rx, _log_buffer) = registry
                            .register(tid, &tool_name)
                            .await;
                        let task_id_bg = task_id.clone();

                        // Spawn a background task that CONTINUES awaiting the
                        // same in-flight future. The tool's declared timeout
                        // (if any) still bounds it; with NO declared timeout
                        // (`None`) the task runs until it completes, errors,
                        // or the agent cancels it via cancel-task.
                        // Do NOT execute the call again — the request was
                        // already sent to the plugin; a serial plugin would
                        // run the command twice (and each agent re-dispatch
                        // would queue another duplicate behind it).
                        let bg_timeout = timeout_dur;
                        let bg_tool_name = tool_name.clone();
                        let bg_registry = registry.clone();
                        let mut bg_future = tool_future;

                        tokio::spawn(async move {
                            tokio::select! {
                                _ = abort_rx => {
                                    bg_registry.set_status(&task_id_bg,
                                        crate::agent::task_registry::TaskStatus::Cancelled).await;
                                    bg_registry.append_log(&task_id_bg,
                                        &format!("Tool '{}' was cancelled", bg_tool_name)).await;
                                }
                                result = async {
                                    match bg_timeout {
                                        Some(dur) => {
                                            tokio::time::timeout(dur, bg_future.as_mut()).await
                                        }
                                        None => Ok(bg_future.as_mut().await),
                                    }
                                } => {
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
                                                bg_tool_name, bg_timeout.map(|d| d.as_secs()).unwrap_or(0));
                                            bg_registry.set_status(&task_id_bg,
                                                crate::agent::task_registry::TaskStatus::Failed(
                                                    err)).await;
                                        }
                                    }
                                }
                            };
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
                };

                let (output, is_error) = match &result {
                    Ok(res) => {
                        // Tool-result spill: oversized results (> max_inline_chars)
                        // are persisted in full to a session-scoped spill file and
                        // replaced inline by a preview + locator so the model can
                        // recover the full output via filesystem_read.
                        let spilled = spill_tool_result(
                            &res.content,
                            tid,
                            &tc_id,
                            &tool_name,
                            &spill_root,
                            max_inline_chars,
                        );
                        (spilled.inline, false)
                    }
                    Err(e) => (format!("Error executing tool '{}': {}", tool_name, e), true),
                };

                // For multi-tool calls: JSON with tool/input/output for disambiguation.
                // For single tool calls: just the raw output (no wrapping).
                let content_str = if is_multi_tool {
                    let args_value: serde_json::Value =
                        serde_json::from_str(&tool_args).unwrap_or(serde_json::json!(tool_args));
                    serde_json::json!({
                        "tool": qualified_name,
                        "input": args_value,
                        "output": output,
                    }).to_string()
                } else {
                    output.clone()
                };

                // Persist single consolidated result message
                // (no separate "tool" call message anymore)
                let result_msg = MessageNew {
                    thread_id: tid,
                    role: "agent".to_string(),
                    content: content_str,
                    thread_sequence: seq,
                    external_id: None,
                    metadata: serde_json::json!({"is_error": is_error}),
                    embedding: None,
                    summary_text: None,
                    is_summary: false,
                    original_thread_id: None,
                    msg_type: "tool-result".to_string(),
                    msg_subtype: Some(qualified_name.clone()),
                    iteration_number: iter_num,
                    duration_ms: 0,
                    token_usage: serde_json::json!({}),
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
                })
                .catch_unwind()
                .await;

                match task_result {
                    Ok(result) => result,
                    Err(panic_payload) => {
                        let panic_message = panic_payload
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic payload".to_string());
                        let output = format!(
                            "Error executing tool '{}': tool task panicked: {}. Retry the tool or handle this error.",
                            panic_tool_name, panic_message
                        );
                        error!("{}", output);
                        (panic_idx, panic_tc_id, panic_tool_name, output, true)
                    }
                }
            });
        }

        // Collect results as they complete (order may differ from call order).
        //
        // IMPORTANT: JoinSet returns Err(JoinError) when a tool task panics. A
        // previous implementation only logged that error, leaving the result
        // slot as None. The message-building loop below then silently skipped
        // that tool call, which left the provider with an unmatched tool call
        // in multi-tool rounds and could derail the entire agent loop.
        //
        // Every tool call MUST produce a result for the LLM, including a panic
        // result. Handle the panic at the omniagent boundary rather than
        // requiring every plugin to catch its own panics.
        let mut tool_results: Vec<Option<(String, String, String)>> = vec![None; tool_count];
        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((idx, tc_id, tool_name, output, _is_error)) => {
                    tool_results[idx] = Some((tc_id, tool_name, output));
                }
                Err(e) => {
                    // The per-tool catch_unwind above should make this
                    // unreachable for plugin panics. Keep a defensive log for
                    // cancellation/runtime join failures; missing slots are
                    // filled below before messages are sent to the provider.
                    error!("Tool execution task could not be joined: {:?}", e);
                }
            }
        }

        // Defensive last line: every provider tool call must have a result,
        // even if a task was cancelled or failed to join for a reason other
        // than a caught plugin panic.
        for (idx, tc) in response.tool_calls.iter().enumerate() {
            if tool_results[idx].is_none() {
                let tool_name = tc.function.name.clone();
                let output = format!(
                    "Error executing tool '{}': no tool result was produced. Retry the tool or handle this error.",
                    tool_name
                );
                error!("{}", output);
                tool_results[idx] = Some((tc.id.clone(), tool_name, output));
            }
        }

        // WS-4b: record output length for executed read-only tools.
        for (idx, tc) in response.tool_calls.iter().enumerate() {
            if helpers::is_guarded_read_only(&tc.function.name) {
                if let Some(Some((_, _, output))) = tool_results.get(idx) {
                    read_guard.insert(
                        (
                            tc.function.name.clone(),
                            helpers::hash_tool_args(&tc.function.arguments),
                        ),
                        (current_iter as u32, output.len()),
                    );
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
            let called_manage = response.tool_calls.iter().any(|tc| {
                tc.function.name == "subtasks_manage-subtasks"
                    || tc.function.name == "manage_subtasks"
            });
            if called_manage {
                calls_since_subtask_management = 0;
            } else {
                calls_since_subtask_management += 1;
            }

            if calls_since_subtask_management >= 10 {
                if let Ok(subtasks) = crate::subtask::list_subtasks(&cfg.pool, thread.id).await {
                    let pending_count = subtasks
                        .iter()
                        .filter(|st| st.status == "pending" || st.status == "in_progress")
                        .count();
                    if pending_count > 0 {
                        let reminder = format!(
                            "[Progress Check] You've made {} tool call rounds without updating your subtasks. \
                             If you've completed any steps, call `subtasks_manage-subtasks(action=\"update\", subtask_id=N, status=\"completed\")` \
                             for each finished subtask now. This keeps progress accurate.",
                            calls_since_subtask_management,
                        );
                        messages.push(ChatMessage::user(&reminder));
                        calls_since_subtask_management = 0;
                    }
                }
            }
        }
    } // end for _turn

    // If we exited the loop without a final text response, provide a truthful
    // fallback. The old hardcoded "I've completed the requested operations"
    // string was a FALSE SUCCESS: when the LLM returns reasoning-only content
    // (i.e. it gives up without producing a final answer), that fallback made
    // the agent claim completion it never achieved. Report the give-up clearly
    // and force the thread to fail so callers see the task was NOT done.
    if final_content.is_empty() && !final_tool_call {
        final_content = "The agent gave up without producing a final answer. \
The task was NOT completed: no final response was generated after the tool-calling loop ended. \
Review the tool results above to see what was attempted and what remains."
            .to_string();
        force_failed = true;
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
                    let v = *next_seq;
                    *next_seq += 1;
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
                original_thread_id: None,
                msg_type: "reasoning".to_string(),
                msg_subtype: None,
                iteration_number: current_iter,
                duration_ms: 0,
                token_usage: serde_json::json!({}),
            };
            let reasoning_saved = queries::create_message(&cfg.pool, &reasoning_msg).await?;
            helpers::enqueue_delivery(
                &cfg.ctx,
                &reasoning_saved,
                channel,
                thread,
                cause_msg.external_id.clone(),
            )
            .await;
        }
    }

    // 9. Save the main agent response (when limit_reached, generate LLM summary instead)
    // 9. Save the main agent response + cleanup
    let saved = handle_response(
        cfg,
        thread,
        cause_msg,
        channel,
        *next_seq,
        start_time,
        &messages,
        &mut cumulative_usage,
        &mut force_failed,
        limit_reached,
        current_iter,
        iter_limit,
        per_thread_llm,
        final_content,
        token_usage_json,
        evidence_metadata,
        enable_subtasks,
    )
    .await?;
    Ok(saved)
}

// ── Truncation escalation helpers (pure, unit-tested) ─────────────────────

/// Cap for the preserved reasoning note injected on a truncation retry
/// (reasoning-forward: the model must NOT re-derive the chain).
const PRESERVED_REASONING_CHARS: usize = 16000;

/// Action to take for one LLM response based on truncation + escalation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruncationAction {
    /// Response not truncated: proceed normally and reset the escalation state.
    Continue,
    /// First truncation: retry once with the escalated budget + preserved reasoning.
    Escalate,
    /// Second consecutive truncation: fail fast (no third retry).
    FailFast,
}

/// Pure decision function: (is_escalated, is_truncated) → action.
fn truncation_action(escalated: bool, truncated: bool) -> TruncationAction {
    match (escalated, truncated) {
        (_, false) => TruncationAction::Continue,
        (false, true) => TruncationAction::Escalate,
        (true, true) => TruncationAction::FailFast,
    }
}

/// Effective output budget for the current attempt: the escalated budget
/// after a truncation, otherwise the normal `max_tokens`. `None` = no cap:
/// the provider's own default output limit applies.
fn effective_max_tokens(escalated: Option<u32>, base: Option<u32>) -> Option<u32> {
    escalated.or(base)
}

/// Human-readable output budget for LLM-facing messages: the numeric cap,
/// or "provider default" when no cap is configured (`None`).
fn fmt_output_budget(max_tokens: Option<u32>) -> String {
    max_tokens
        .map(|v| v.to_string())
        .unwrap_or_else(|| "provider default".to_string())
}

/// Messages appended for a truncation retry: preserved reasoning note (if
/// any), any partial content, and the SHORTER-answer nudge.
fn truncation_retry_messages(reasoning: Option<&str>, content: &str) -> Vec<ChatMessage> {
    let mut msgs = Vec::new();
    if let Some(r) = reasoning {
        if !r.trim().is_empty() {
            msgs.push(ChatMessage::system(&format!(
                "=== Preserved Reasoning (from truncated response) ===\n{}",
                truncate_content(r, PRESERVED_REASONING_CHARS),
            )));
        }
    }
    if !content.is_empty() {
        msgs.push(ChatMessage::assistant(content));
    }
    msgs.push(ChatMessage::system(
        "[System] Your previous response was cut off by the token limit (attempt 1/2). \
         The reasoning above is preserved. Produce a SHORTER response now: emit a single \
         small tool call or a concise final answer. Do NOT regenerate the long reasoning chain.",
    ));
    msgs
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn first_truncation_escalates() {
        assert_eq!(truncation_action(false, true), TruncationAction::Escalate);
    }

    #[test]
    fn second_consecutive_truncation_fails_fast() {
        assert_eq!(truncation_action(true, true), TruncationAction::FailFast);
        // No third retry: escalate (1/2) → fail-fast (2/2), loop ends.
    }

    #[test]
    fn successful_response_resets_escalation() {
        assert_eq!(truncation_action(true, false), TruncationAction::Continue);
        assert_eq!(truncation_action(false, false), TruncationAction::Continue);
    }

    #[test]
    fn failfast_contract_forces_failed_status() {
        // Truncation FailFast contract: the base truncation escalates once, a
        // second consecutive truncation fails fast — the executor loop sets
        // force_failed=true so the thread's final status is "failed" (NOT
        // "completed"). A failed executor thread goes blocked (or review with
        // review_on_fail) — it never advances to the tester.
        assert_eq!(truncation_action(false, true), TruncationAction::Escalate);
        assert_eq!(truncation_action(true, true), TruncationAction::FailFast);
        // Mirror of the post-loop status computation in handle_response:
        // force_failed wins over everything (including limit_reached).
        assert_eq!(
            crate::agent::response_handler::post_loop_final_status(true, false),
            "failed"
        );
        assert_eq!(
            crate::agent::response_handler::post_loop_final_status(true, true),
            "failed"
        );
        assert_ne!(
            crate::agent::response_handler::post_loop_final_status(true, false),
            "completed"
        );
    }

    #[test]
    fn effective_budget_uses_escalated_value() {
        assert_eq!(effective_max_tokens(None, Some(4096)), Some(4096));
        assert_eq!(effective_max_tokens(Some(16384), None), Some(16384));
        assert_eq!(effective_max_tokens(None, None), None);
    }

    #[test]
    fn retry_messages_preserve_reasoning_and_nudge_short() {
        let msgs = truncation_retry_messages(Some("think step by step"), "partial");
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[0].content.contains("Preserved Reasoning"));
        assert!(msgs[0].content.contains("think step by step"));
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content, "partial");
        let nudge = &msgs[2];
        assert_eq!(nudge.role, "system");
        assert!(nudge.content.contains("attempt 1/2"));
        assert!(nudge.content.contains("SHORTER"));
        assert!(nudge.content.contains("Do NOT regenerate"));
    }

    #[test]
    fn retry_messages_skip_empty_reasoning() {
        let msgs = truncation_retry_messages(Some("   "), "partial");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[0].content, "partial");
        assert!(msgs[1].content.contains("attempt 1/2"));
    }

    #[test]
    fn retry_messages_include_partial_content() {
        let msgs = truncation_retry_messages(None, "half of a sentence");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[0].content, "half of a sentence");
        assert!(msgs[1].content.contains("attempt 1/2"));
    }
}

/// Iteration-percent gate for sub-prompt lookups: lookups only happen while
/// the current iteration is within the first `percent`% of the iteration
/// budget (`current_iter * 100 <= iter_limit * percent`). percent=0 disables
/// the feature at the call site (the gate is never consulted).
#[allow(dead_code)]
#[allow(clippy::items_after_test_module)]
pub(crate) fn sub_prompt_gate_ok(current_iter: i32, iter_limit: i32, percent: u32) -> bool {
    if percent == 0 {
        return false;
    }
    current_iter * 100 <= iter_limit * percent as i32
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod sub_prompt_gate_tests {
    use super::*;

    #[test]
    fn gate_allows_early_iterations_only() {
        // percent=50, iter_limit=300: lookups allowed while current_iter <= 150.
        assert!(sub_prompt_gate_ok(1, 300, 50));
        assert!(sub_prompt_gate_ok(150, 300, 50));
        assert!(!sub_prompt_gate_ok(151, 300, 50));
        assert!(!sub_prompt_gate_ok(300, 300, 50));
    }

    #[test]
    fn gate_100_checks_every_call() {
        assert!(sub_prompt_gate_ok(1, 300, 100));
        assert!(sub_prompt_gate_ok(300, 300, 100));
        assert!(sub_prompt_gate_ok(1, 30, 100));
    }

    #[test]
    fn gate_zero_disables() {
        // percent=0 disables the feature entirely.
        assert!(!sub_prompt_gate_ok(1, 300, 0));
        assert!(!sub_prompt_gate_ok(0, 300, 0));
    }
}
