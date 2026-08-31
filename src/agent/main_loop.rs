use crate::agent::config::AgentContext;
use crate::agent::context_builder::PromptParts;
use crate::agent::context_compactor::{compact_oldest_segment, should_force_compact};
use crate::agent::helpers;
use crate::agent::response_handler::handle_response;
use crate::agent::tool_result_pruner::{is_context_length_error, prune_messages, PruneParams};
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

/// Build the live progress stats for a running thread from the cumulative
/// LLM usage and the elapsed time since the thread started. Mirrors the
/// terminal-state stats construction in `response_handler` so the live
/// intermediate values match the final write exactly.
fn thread_progress_stats(
    usage: &Option<Usage>,
    start_time: &std::time::Instant,
) -> queries::CompleteThreadStats {
    queries::CompleteThreadStats {
        input_tokens: usage.as_ref().map(|u| u.prompt_tokens as i32).unwrap_or(0),
        cached_tokens: usage.as_ref().and_then(|u| u.cached_tokens).unwrap_or(0) as i32,
        output_tokens: usage
            .as_ref()
            .map(|u| u.completion_tokens as i32)
            .unwrap_or(0),
        duration_ms: start_time.elapsed().as_millis() as i32,
    }
}

/// Extract up to 6 plan steps from plan content (markdown or JSON).
///
/// Real plans are markdown: `<plan>1. step one</plan>` or plain numbered/bulleted
/// lists. We no longer REQUIRE JSON `{"steps": [...]}` - that never matched real
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

// ── Phase 1.5: Self-restart guard (P2 #6) ─────────────────────────────────
// An agent must never tear down the container it runs inside: a
// `docker compose restart/down/stop/rm/kill` against its OWN compose
// project kills its own thread (thread 488 self-kill). The guard resolves
// the docker-authoritative compose PROJECT NAME of both sides - the agent's
// own container (`com.docker.compose.project` label via `docker inspect`)
// and the target project (`docker compose ... config --format json` →
// `.name`, the exact resolution compose itself performs) - and blocks a
// destructive verb ONLY when the two names are EQUAL. `up` is NEVER blocked
// for any project, and other projects (e.g. the omnidev dev stack) are
// always manageable. Resolution is DELEGATED to docker/compose; compose's
// precedence chain (name:, COMPOSE_PROJECT_NAME, --project-name, multiple
// -f files, project-directory) is never reimplemented here.

/// Destructive compose verbs that would tear down a running project.
/// `up` is deliberately absent: bringing containers up is never destructive.
const DESTRUCTIVE_COMPOSE_VERBS: &[&str] = &["restart", "down", "stop", "rm", "kill"];

/// Pure decision: block iff the verb is destructive AND both project names
/// resolved AND they are equal. `up`/any other verb → never blocked; an
/// unresolvable name on either side → never blocked (cannot prove self-kill).
fn guard_blocks(verb: &str, self_project: Option<&str>, target_project: Option<&str>) -> bool {
    if !DESTRUCTIVE_COMPOSE_VERBS.contains(&verb) {
        return false;
    }
    match (self_project, target_project) {
        (Some(s), Some(t)) => s == t,
        _ => false,
    }
}

/// Extract the compose verb (first whitespace-separated token of `command`).
fn compose_verb(args: &serde_json::Value) -> Option<&str> {
    let cmd = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
    let verb = cmd.split_whitespace().next().unwrap_or("");
    if verb.is_empty() {
        None
    } else {
        Some(verb)
    }
}

/// Build the delegated `docker compose ... config --format json` invocation
/// that resolves the TARGET project's effective name exactly as compose does.
fn build_target_config_cmd(
    project_dir: &str,
    compose_files: &[String],
    env_file: Option<&str>,
) -> Vec<String> {
    let mut cmd = vec![
        "compose".to_string(),
        "--project-directory".to_string(),
        project_dir.to_string(),
    ];
    for f in compose_files {
        cmd.push("-f".to_string());
        cmd.push(f.clone());
    }
    if let Some(env) = env_file {
        cmd.push("--env-file".to_string());
        cmd.push(env.to_string());
    }
    cmd.push("config".to_string());
    cmd.push("--format".to_string());
    cmd.push("json".to_string());
    cmd
}

/// Build the delegated `docker inspect` invocation that reads the agent's OWN
/// compose project name from the `com.docker.compose.project` label.
fn build_self_inspect_cmd(container_id: &str) -> Vec<String> {
    vec![
        "inspect".to_string(),
        container_id.to_string(),
        "--format".to_string(),
        "{{index .Config.Labels \"com.docker.compose.project\"}}".to_string(),
    ]
}

/// Resolve the agent's own compose project name (authoritative label).
async fn resolve_self_project() -> Option<String> {
    // In a container $HOSTNAME is the container ID.
    let cid = std::env::var("HOSTNAME").ok()?;
    let out = tokio::process::Command::new("docker")
        .args(build_self_inspect_cmd(&cid))
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() || name.contains("Error") {
        None
    } else {
        Some(name)
    }
}

/// Resolve the TARGET project's effective name by delegating to compose
/// (`config --format json` → `.name`). Fallback: a RUNNING project's
/// containers carry the `com.docker.compose.project` label - read it via
/// `docker ps` filtered by the project's working_dir label.
async fn resolve_target_project(
    project_dir: &str,
    compose_files: &[String],
    env_file: Option<&str>,
) -> Option<String> {
    let out = tokio::process::Command::new("docker")
        .args(build_target_config_cmd(
            project_dir,
            compose_files,
            env_file,
        ))
        .output()
        .await
        .ok()?;
    if out.status.success() {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    // Fallback: match the running project by its working_dir label.
    let filter = format!(
        "label=com.docker.compose.project.working_dir={}",
        project_dir
    );
    let out = tokio::process::Command::new("docker")
        .args(vec![
            "ps".to_string(),
            "-a".to_string(),
            "--filter".to_string(),
            filter,
            "--format".to_string(),
            "{{json .Labels}}".to_string(),
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Ok(labels) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(name) = labels
                .get("com.docker.compose.project")
                .and_then(|n| n.as_str())
            {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Evaluate the Phase 1.5 guard for one docker_compose tool call. Returns the
/// block message when the call would tear down the agent's own project.
async fn self_restart_guard_block(args_json: &str) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let verb = compose_verb(&args)?;
    // `up` (and any non-destructive verb) is NEVER blocked - skip the
    // resolution overhead entirely.
    if !DESTRUCTIVE_COMPOSE_VERBS.contains(&verb) {
        return None;
    }
    let project_dir = args
        .get("project_dir")
        .and_then(|p| p.as_str())
        .unwrap_or("");
    if project_dir.is_empty() {
        return None;
    }
    let compose_files: Vec<String> = match args.get("compose_file") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    let env_file = args.get("env_file").and_then(|e| e.as_str());
    let self_project = resolve_self_project().await;
    let target_project = resolve_target_project(project_dir, &compose_files, env_file).await;
    if guard_blocks(verb, self_project.as_deref(), target_project.as_deref()) {
        Some(format!(
            "Blocked: docker_compose '{verb}' targets compose project '{target}' - the project this agent runs inside (self project '{self_name}'). \
             Tearing down your own container kills this thread. Only Hermes may restart the stack. \
             You may manage OTHER compose projects (e.g. the omnidev dev stack) freely; `up` is never blocked.",
            target = target_project.as_deref().unwrap_or("?"),
            self_name = self_project.as_deref().unwrap_or("?"),
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod self_restart_guard_tests {
    use super::*;

    #[test]
    fn blocks_when_self_equals_target() {
        assert!(guard_blocks(
            "restart",
            Some("omnistable"),
            Some("omnistable")
        ));
        assert!(guard_blocks("down", Some("omnidev"), Some("omnidev")));
        assert!(guard_blocks("stop", Some("omnistable"), Some("omnistable")));
        assert!(guard_blocks("rm", Some("omnistable"), Some("omnistable")));
        assert!(guard_blocks("kill", Some("omnistable"), Some("omnistable")));
    }

    #[test]
    fn allows_different_projects() {
        // An omnistable agent MUST be able to manage the omnidev dev stack.
        assert!(!guard_blocks(
            "restart",
            Some("omnistable"),
            Some("omnidev")
        ));
        assert!(!guard_blocks("down", Some("omnidev"), Some("omnistable")));
        assert!(!guard_blocks("stop", Some("omnistable"), Some("omnidev")));
    }

    #[test]
    fn up_is_never_blocked() {
        assert!(!guard_blocks("up", Some("omnistable"), Some("omnistable")));
        assert!(!guard_blocks("up", Some("omnistable"), Some("omnidev")));
        assert!(!guard_blocks("up", None, None));
    }

    #[test]
    fn unresolvable_names_are_never_blocked() {
        // Cannot prove self-kill → allow (the compose call itself will fail
        // if the project does not exist).
        assert!(!guard_blocks("restart", None, Some("omnistable")));
        assert!(!guard_blocks("restart", Some("omnistable"), None));
        assert!(!guard_blocks("restart", None, None));
    }

    #[test]
    fn non_destructive_verbs_never_block() {
        assert!(!guard_blocks("ps", Some("omnistable"), Some("omnistable")));
        assert!(!guard_blocks(
            "logs",
            Some("omnistable"),
            Some("omnistable")
        ));
        assert!(!guard_blocks(
            "exec",
            Some("omnistable"),
            Some("omnistable")
        ));
    }

    #[test]
    fn resolution_is_delegated_to_compose_config() {
        // The guard must NOT reimplement compose's precedence chain: the
        // target name comes from `docker compose ... config --format json`
        // and the self name from the docker-inspect label.
        let cmd = build_target_config_cmd(
            "/opt/workspace/omni-stack",
            &[
                "docker-compose.yml".to_string(),
                "docker-compose.dev.yml".to_string(),
            ],
            Some("/opt/workspace/omni-deployer/omnidev.env"),
        );
        assert_eq!(
            cmd,
            vec![
                "compose",
                "--project-directory",
                "/opt/workspace/omni-stack",
                "-f",
                "docker-compose.yml",
                "-f",
                "docker-compose.dev.yml",
                "--env-file",
                "/opt/workspace/omni-deployer/omnidev.env",
                "config",
                "--format",
                "json",
            ]
        );
        let inspect = build_self_inspect_cmd("abc123");
        assert_eq!(inspect[0], "inspect");
        assert_eq!(inspect[1], "abc123");
        assert!(inspect[3].contains("com.docker.compose.project"));
    }

    #[test]
    fn effective_name_differs_from_project_dir_basename() {
        // `docker compose config` resolves the REAL project name (which may
        // differ from the project-dir basename due to `name:`,
        // COMPOSE_PROJECT_NAME, --project-name, or -f overrides). Delegation
        // means the guard compares docker-authoritative names, never paths.
        let cmd = build_target_config_cmd("/opt/workspace/omni-stack", &[], None);
        assert!(cmd.contains(&"config".to_string()));
        assert!(cmd.contains(&"--format".to_string()));
        assert!(cmd.contains(&"json".to_string()));
        // The only path that appears is the delegated --project-directory
        // argument; there is no path-derived project-name logic.
        assert_eq!(cmd.iter().filter(|c| c.contains("omni-stack")).count(), 1);
    }

    #[test]
    fn verb_parsed_from_command_arg() {
        let args = serde_json::json!({"command": "restart", "project_dir": "/p"});
        assert_eq!(compose_verb(&args), Some("restart"));
        let args = serde_json::json!({"command": "up -d", "project_dir": "/p"});
        assert_eq!(compose_verb(&args), Some("up"));
        let args = serde_json::json!({});
        assert_eq!(compose_verb(&args), None);
    }
}

/// Iteration accounting for the plan phase: the number of iteration slots
/// the plan phase actually consumed. Returns 1 only when a plan was generated
/// (plan_content.is_some()); 0 when planning was skipped or FAILED.
///
/// The plan prompt is logged with its own numbering (msg_subtype "plan",
/// iteration 0) and the plan message itself is logged at iteration 1, so a
/// successful plan means the first main-loop prompt lands on iteration 2.
/// A failed plan generation must NOT consume an iteration slot: the first
/// main-loop prompt then lands on iteration 1 with no gap in the
/// user-visible iteration sequence (thread 263 showed the jump 0 -> 2 with
/// no message at iteration 1 because plan_consumed was derived from
/// should_plan instead of actual plan success).
fn plan_iterations_consumed(plan_content: &Option<String>) -> i32 {
    if plan_content.is_some() {
        1
    } else {
        0
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
    // Telegram first/last-only collapse: when the telegram platform plugin is
    // configured with first_last_only=true, only the FIRST and LAST messages
    // of this run are delivered to the chat (intermediate output collapsed).
    // Mattermost is never affected by this flag.
    let mut collapse = helpers::FirstLastCollapse::new(
        helpers::telegram_first_last_only(&cfg.ctx.data_dir)
            && channel.platform.as_deref() == Some("telegram"),
    );
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

    // Deterministic tool-result pruning thresholds (task 2): shrinks
    // over-budget tool results to a bounded head/middle/tail preview before
    // LLM calls (assembly), summaries, and provider context-length retries.
    let prune_params = PruneParams::from_config(&cfg_snapshot);

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
            // task is - not just the generic planning instruction. The context
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
                 chunks via filesystem_write append=true - never let an output \
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
                    // Live progress: persist intermediate usage stats after the
                    // planning LLM call so processing threads show live values.
                    if let Err(e) = queries::update_thread_progress(
                        &cfg.pool,
                        thread.id,
                        (iter + 1) as i32,
                        thread_progress_stats(&cumulative_usage, &start_time),
                    )
                    .await
                    {
                        warn!(
                            "[plan] Failed to update thread {} progress after plan call: {:?}",
                            thread.id, e
                        );
                    }
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
                                "[plan] No parseable steps in plan for thread {} - skipping subtask auto-create",
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
    // prompt - the opposite of the executor layout (template = system,
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

    // ── Truncation recovery (finish_reason=length) ──
    // Normal LLM calls use the configured `max_tokens` budget (None = no cap:
    // the provider's own default applies - no max_tokens sent in the request).
    // When the provider reports finish_reason=length (the output ceiling was
    // hit) the thread RECOVERS: it retries a bounded number of times
    // (`max_truncation_retries`, default 3) with an escalating nudge that
    // demands a progressively SHORTER response (or multi-part emission) - NOT
    // "continue where you left off", which regenerates the same oversized
    // output. The retry budget is per-thread and resets whenever the model
    // produces a valid (non-truncated) response, so in-progress tool work is
    // never discarded and a later truncation gets fresh recovery attempts.
    // Only after the budget is exhausted does the thread give up truthfully
    // (surfacing exactly what was trimmed) - a recoverable output truncation
    // never trips the stuck/empty-thread safety valve below.
    let mut escalated_max_tokens: Option<u32> = None;
    // Per-thread truncation-recovery counter: incremented on each recovery
    // retry, reset on any valid (non-truncated) response.
    let mut truncation_retries_used: u32 = 0;
    let base_max_tokens: Option<u32> = eff_model_cfg.max_tokens;
    let max_tokens_on_truncation: Option<u32> = eff_model_cfg.max_tokens_on_truncation;
    let max_truncation_retries = cfg_snapshot.max_truncation_retries;

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
         append=true. Never abandon a task because of the output limit - chunk \
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
    // The plan phase consumed an iteration slot ONLY when a plan was actually
    // generated (plan_content.is_some()). A failed or skipped plan consumes
    // nothing: the first main-loop prompt then stays at iteration 1 instead
    // of jumping to 2 with a gap (thread 263 showed the 0 -> 2 jump).
    let plan_consumed = plan_iterations_consumed(&plan_content);
    let max_llm_calls = (iter_limit - plan_consumed).max(0) as u32;
    let mut final_content = String::new();
    let mut final_reasoning: Option<String> = None;
    let mut final_tool_call: bool = false;
    let mut limit_reached: bool = false;
    let mut _last_response_usage: Option<Usage> = None;
    current_iter = plan_consumed; // 0 when plan failed/skipped, 1 when a plan was generated
    let mut unfinished_subtask_retries: u32 = 0;
    let mut calls_since_subtask_management: u32 = 0;
    // How many consecutive LLM errors (provider errors, truncation,
    // empty responses) we tolerate before marking the thread failed.
    // A correct (non-error) response resets the counter to 0. The limit
    // comes from config `provider_max_retries` (default 3); MAX_LLM_RETRIES
    // is the fallback when the setting is 0. This bounds token waste even
    // if a tool misbehaves (e.g. compaction) - we stop after this many
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
    // Task 3: bounded forced-compaction retries on provider context-length
    // errors (the death-spiral recovery). When the accumulated thread history
    // exceeds the model's context window, pruning tool results (task 2) is not
    // enough - force a summary compaction of the OLDEST segment and retry.
    // Bounded per thread by `max_compaction_retries` (default 2) so a hopeless
    // thread fails honestly instead of looping forever. Both counters reset on
    // a successful tool-calling response (see below).
    let max_compaction_retries = cfg_snapshot.max_compaction_retries;
    let mut compaction_retries_used: u32 = 0;
    // True after the FIRST context-length overflow already retried once with
    // pruned tool results: a second overflow means pruning is exhausted, so
    // escalate to summary compaction.
    let mut pruned_for_overflow: bool = false;
    // Track when condensation last occurred so soft-budget triggers use
    // iteration-since-last-condense rather than a fixed modulo schedule.
    // This prevents aggressive condensation on every Nth iteration even when
    // the last condense just happened.
    let mut last_condense_iteration: i32 = 0;
    // The iteration on which the last prompt message was logged. Retry
    // iterations of a failed LLM call (provider error / empty response /
    // truncation / forced compaction all do `current_iter -= 1` + continue)
    // carry the SAME current_iter as the attempt that already logged a
    // prompt, so they must NOT log a second prompt: every prompt message must
    // be immediately followed by the LLM response (or its failure), never by
    // another prompt (observed double insertion: thread 467, #37482+#37484).
    let mut last_prompt_iter: i32 = -1;
    // Sub-prompts (feature): cumulative char budget + exhaustion flag for
    // appended pending user prompts, scoped to this thread run (persisted
    // across iterations of the same run).
    let mut used_sub_prompt_chars: usize = 0;
    let mut sub_prompts_exhausted: bool = false;

    // WS-4b: engine-level read guard - (tool, args-hash) -> (iteration, len)
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
        // prompt - BEFORE the condense call so compaction never drops them.
        // Each pending thread is marked merged and a sub_cause message
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
                            queries::mark_thread_merged_for_sub_prompt(&cfg.pool, pt.id, thread.id)
                                .await
                        {
                            warn!(
                                "[sub-prompt] Failed to mark pending thread #{} merged: {:?}",
                                pt.id, e
                            );
                        }
                        // Send the merged reaction to the platform for the
                        // pending thread (merged => ":handshake:"). Uses the
                        // shared choke-point resolution: only a REAL
                        // cause-message target is used (never synthetic
                        // hook/cron ids, never empty).
                        helpers::enqueue_status_reaction(
                            &cfg.ctx,
                            &cfg.pool,
                            pt.id,
                            None,
                            Some(channel),
                            "merged",
                        )
                        .await;
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
                        // compaction happens and whether it succeeded - it may
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
        // WS-3: durable working notes survive compaction - injected every
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
        // WS-5: ENGINE auto-notes - read-type tool results are auto-saved to
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
        // WS-3: compaction notice - never re-read the dump (rule 12).
        if was_compacted {
            helpers::upsert_system_message(
                    &mut messages,
                    "=== Context Compacted",
                    format!(
                        "=== Context Compacted (iteration {current_iter}) ===\nDump: {} ({} entries).\nNever re-read context-{current_iter}.json - rule 12.",
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
        let should_log_prompt = should_log_prompt_for(
            prompt_log_level,
            has_logged_first_prompt,
            current_iter,
            last_prompt_iter,
            last_condense_iteration,
        );
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
            last_prompt_iter = current_iter;
        }

        // ── LLM completion call ──

        // Deterministic tool-result pruning (task 2): shrink over-budget tool
        // results to a bounded head/middle/tail preview BEFORE this LLM call so
        // the request never pays for huge dumps. Pure slicing (zero LLM cost);
        // spill locators (task 1) are preserved and tool-CALL messages are
        // never touched, so the tool-call/result pairing stays intact.
        let prune_report = prune_messages(&mut messages, &prune_params);
        if !prune_report.is_empty() {
            info!(
                "[prune] Pre-LLM prune for thread {}: {} result(s) pruned, {} chars -> {} (saved {})",
                thread.id,
                prune_report.entries.len(),
                prune_report.chars_before,
                prune_report.chars_after,
                prune_report.chars_saved(),
            );
        }

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
                // Task 3: context-overflow recovery (kill the death spiral).
                // A provider context-length error (input too long) can never be
                // fixed by retrying the SAME oversized context - the thread
                // would just die after exhausting retries. Recovery, bounded by
                // max_compaction_retries:
                //   (a) prune over-budget tool results (task 2, zero LLM cost),
                //   (b) if pruning is exhausted (nothing to prune, or a pruned
                //       retry already overflowed again) FORCE a summary
                //       compaction of the OLDEST segment of the thread and
                //       retry with the compacted context,
                //   (c) once the compaction budget is spent, fail honestly.
                if is_context_length_error(&format!("{:?}", e)) {
                    let prune_report = prune_messages(&mut messages, &prune_params);
                    info!(
                        "[prune] Context-length error: pruned {} result(s) for thread {} ({} chars -> {}, saved {})",
                        prune_report.entries.len(),
                        thread.id,
                        prune_report.chars_before,
                        prune_report.chars_after,
                        prune_report.chars_saved(),
                    );
                    if should_force_compact(
                        !prune_report.is_empty(),
                        pruned_for_overflow,
                        compaction_retries_used,
                        max_compaction_retries,
                    ) {
                        pruned_for_overflow = false;
                        match compact_oldest_segment(per_thread_llm, &mut messages, thread.id).await
                        {
                            Ok(Some(report)) => {
                                compaction_retries_used += 1;
                                info!(
                                    "[compact] Forced compaction for thread {} (attempt {}/{}): shadowed {} message(s) ({} chars -> {}, est. {} tokens saved; summary model call)",
                                    thread.id,
                                    compaction_retries_used,
                                    max_compaction_retries,
                                    report.msgs_compacted,
                                    report.chars_before,
                                    report.chars_after,
                                    report.est_tokens_saved(),
                                );
                                // The compacted retry must not consume the
                                // iteration budget (mirrors the truncation
                                // escalation path) and is not a provider error.
                                current_iter -= 1;
                                tokio::time::sleep(backoff_delay(1).await).await;
                                continue;
                            }
                            Ok(None) => {
                                warn!(
                                    "[compact] Context-length error for thread {} but no compactable range (tiny thread); falling through to provider retry",
                                    thread.id
                                );
                            }
                            Err(ce) => {
                                warn!(
                                    "[compact] Summary generation failed for thread {}: {}; falling through to provider retry",
                                    thread.id, ce
                                );
                            }
                        }
                    } else if !prune_report.is_empty() && !pruned_for_overflow {
                        // First overflow and pruning reduced the context: give
                        // the pruned retry a chance before escalating.
                        pruned_for_overflow = true;
                        info!(
                            "[prune] Context-length error for thread {}: pruned tool results, retrying before summary compaction",
                            thread.id
                        );
                    } else {
                        warn!(
                            "[compact] Context-length error for thread {}: compaction unavailable or budget exhausted ({} used / max {}) - will fail honestly",
                            thread.id, compaction_retries_used, max_compaction_retries
                        );
                    }
                }
                llm_error_retries += 1;
                if llm_error_retries >= llm_max_retries {
                    warn!(
                        "[executor] LLM provider failed {} consecutive time(s) (max {}) for thread {}: {:?}; marking thread failed",
                        llm_error_retries, llm_max_retries, thread.id, e,
                    );
                    final_content = if is_context_length_error(&format!("{:?}", e)) {
                        format!(
                            "The thread's accumulated context exceeded the model's context window, and {} forced compaction(s) (max {}) could not bring it under the limit. Last error: {}. The thread was marked as failed.",
                            compaction_retries_used, max_compaction_retries, e,
                        )
                    } else {
                        format!(
                            "The LLM provider returned an error {} consecutive times (max {}). Last error: {}. The thread was marked as failed.",
                            llm_error_retries, llm_max_retries, e,
                        )
                    };
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

        // Live progress: incrementally update the threads table after EVERY
        // LLM call (iterations, tokens, elapsed time) so processing threads
        // show up-to-date stats while they run. Single cheap row update; the
        // terminal-state write (complete_thread) stays the final word.
        if let Err(e) = queries::update_thread_progress(
            &cfg.pool,
            thread.id,
            current_iter,
            thread_progress_stats(&cumulative_usage, &start_time),
        )
        .await
        {
            warn!(
                "[executor] Failed to update thread {} progress: {:?}",
                thread.id, e
            );
        }

        // Store reasoning if present
        if response.reasoning.is_some() {
            final_reasoning = response.reasoning.clone();
        }

        // Check for tool calls
        if response.tool_calls.is_empty() {
            // ── Truncation recovery (finish_reason=length) ──
            // finish_reason=length means the provider hit the output ceiling
            // before the model could emit its action/answer. This is a
            // RECOVERABLE condition, not a stuck/empty thread: retry a bounded
            // number of times (max_truncation_retries, default 3) with an
            // escalating nudge that demands a progressively SHORTER response
            // (or multi-part emission) - never "continue where you left off",
            // which regenerates the same oversized output. Reasoning-only
            // truncation (content empty, reasoning consumed the budget) drops
            // the reasoning and demands a thinking-free response, because
            // reasoning models restart their chain on each retry and would
            // re-trigger length. Only when the retry budget is exhausted does
            // the thread give up truthfully (surfacing exactly what was
            // trimmed). The stuck/empty-thread safety valve below never fires
            // on a recoverable truncation.
            let truncated = response
                .finish_reason
                .as_deref()
                .map(|f| f == "length")
                .unwrap_or(false);
            let content_empty = response.content.trim().is_empty();
            let has_reasoning = response
                .reasoning
                .as_ref()
                .map(|r| !r.trim().is_empty())
                .unwrap_or(false);
            match truncation_action(
                truncated,
                truncation_retries_used,
                max_truncation_retries,
                content_empty,
                has_reasoning,
            ) {
                TruncationAction::Retry { attempt, kind } => {
                    truncation_retries_used += 1;
                    if escalated_max_tokens.is_none() {
                        escalated_max_tokens = max_tokens_on_truncation;
                    }
                    info!(
                        "[executor] response truncated (finish_reason=length, attempt {}/{}): retrying with shorter-response nudge (kind={:?}, max_tokens={}) (thread {})",
                        attempt, max_truncation_retries, kind,
                        fmt_output_budget(escalated_max_tokens), thread.id,
                    );
                    // Reasoning-aware recovery: preserve truncated reasoning +
                    // partial content (content truncation), or drop the
                    // budget-eating reasoning (reasoning-only truncation);
                    // then nudge for a SHORTER response, escalating per retry.
                    messages.extend(truncation_retry_messages(
                        response.reasoning.as_deref(),
                        &response.content,
                        attempt,
                        max_truncation_retries,
                        kind,
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
                             because of the output limit - chunk the output instead.",
                            fmt_output_budget(escalated_max_tokens), fmt_output_budget(base_max_tokens),
                        ),
                    );
                    // Don't consume the iteration budget for this retry overhead.
                    current_iter -= 1;
                    tokio::time::sleep(backoff_delay(1).await).await;
                    continue;
                }
                TruncationAction::GiveUp => {
                    let reasoning_chars = response
                        .reasoning
                        .as_ref()
                        .map(|r| r.chars().count())
                        .unwrap_or(0);
                    let content_chars = response.content.chars().count();
                    warn!(
                        "[executor] response truncated after {} recovery retry(ies) (max {}) for thread {}: giving up truthfully (last truncation: {} content chars, {} reasoning chars)",
                        truncation_retries_used, max_truncation_retries, thread.id,
                        content_chars, reasoning_chars,
                    );
                    // Give up truthfully: surface exactly what was trimmed.
                    // In-progress tool work is preserved in the thread history
                    // (messages/DB) - only the final response was lost, so the
                    // step can be resumed rather than redone. The thread MUST
                    // fail (status "failed" → blocked / review_on_fail), never
                    // complete and advance to the tester.
                    final_content = format!(
                        "The response was truncated by the output token limit after {} recovery retries (max {}), \
                         even with progressively shorter-response retries and an escalated budget of {} tokens. \
                         Last truncated response: {} content chars, {} reasoning chars. \
                         In-progress tool work up to this point is preserved in the thread history; only the \
                         final response was lost. Giving up truthfully.",
                        truncation_retries_used,
                        max_truncation_retries,
                        fmt_output_budget(escalated_max_tokens),
                        content_chars,
                        reasoning_chars,
                    );
                    final_tool_call = false;
                    force_failed = true;
                    break;
                }
                TruncationAction::Continue => {
                    // Successful (non-truncated) response: reset the recovery
                    // state so a later truncation retries from the base budget.
                    escalated_max_tokens = None;
                    truncation_retries_used = 0;
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
                    // decided to stop. We do NOT nudge or retry - forcing a
                    // stopped model to continue produces degraded or
                    // fabricated continuations. Leave final_content empty:
                    // the post-loop fallback reports the give-up truthfully
                    // (thread fails) and the reasoning is saved separately
                    // as a `reasoning` message (step 8 below).
                    //
                    // Genuine truncation (finish_reason=length) is handled above the
                    // subtask/content handling: it escalates the output budget once,
                    // then fails fast - it never reaches this voluntary-stop path.

                    // Voluntary stop: terminal. Empty final_content triggers
                    // the truthful give-up fallback after the loop.
                    String::new()
                }
            } else {
                // Correct response with content (loop ends right after this,
                // so no counter reset needed here - the tool-call path below
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
        // A tool-calling response is correct: reset the consecutive-error counter
        // and the task-3 compaction budget (a healthy thread is never compacted;
        // each overflow chain gets its own bounded compaction budget).
        llm_error_retries = 0;
        compaction_retries_used = 0;
        pruned_for_overflow = false;
        // A successful tool round is a healthy response: reset the truncation
        // recovery state too, so a later truncation gets fresh retries instead
        // of inheriting stale escalation (in-progress tool work is preserved).
        truncation_retries_used = 0;
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
                helpers::enqueue_delivery_collapsed(
                    &cfg.ctx,
                    saved,
                    channel,
                    thread,
                    cause_msg.external_id.clone(),
                    &mut collapse,
                    false,
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
                            "[duplicate of {tool_name} at iteration {guard_iter} - see your notes; re-reading the same input is forbidden by rule 11]"
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
            // An agent must never tear down the container it runs inside: a
            // `docker compose restart/down/stop/rm/kill` against its OWN
            // compose project kills its own thread (thread 488 self-kill).
            // The guard resolves the authoritative compose PROJECT NAME of
            // both sides (self: docker-inspect label; target: `docker compose
            // config --format json` -> `.name`) and blocks a destructive verb
            // ONLY when the two names are EQUAL. `up` is NEVER blocked for
            // any project; other projects (e.g. the omnidev dev stack) are
            // always manageable.
            let mut self_restart_block: Option<String> = None;
            if tool_name == "docker_compose" {
                self_restart_block = self_restart_guard_block(&tc.function.arguments).await;
            }
            let self_restart_block_for_task = self_restart_block.clone();
            let panic_idx = idx;
            let panic_tc_id = tc_id.clone();
            let panic_tool_name = tool_name.clone();
            join_set.spawn(async move {
                let task_result = std::panic::AssertUnwindSafe(async move {
                    // Phase 1.5 guard: if this docker_compose call would restart the
                // agent's own stack, return a synthetic error result instead of
                // executing it - the message plumbing below records it as a
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
                // INTERFACE to the background system - they must never be
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
                // request queued behind it - the bg task resolved only after
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
                    // - the tool decides when it's done.
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
                        // Do NOT execute the call again - the request was
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
                // Seq uniqueness invariant (thread 467: the fail-thread
                // Error message and its tool-result shared seq 38): the
                // pre-allocated `seq` may already be taken by a message the
                // tool itself inserted while executing (builtin_fail-thread
                // persists at get_max_thread_sequence + 1, which equals the
                // pre-allocated result seq). max(db_max + 1, preallocated)
                // guarantees the result never collides with any persisted row.
                let db_max = crate::db::threads::get_max_thread_sequence(&pool, tid)
                    .await
                    .unwrap_or(0);
                let result_msg = MessageNew {
                    thread_id: tid,
                    role: "agent".to_string(),
                    content: content_str,
                    thread_sequence: effective_result_seq(db_max, seq),
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

        // ── Terminal-thread check (fail-thread lifecycle fix) ──
        // A tool may have ended this thread while executing: builtin_fail-thread
        // marks the thread terminal (status 'failed') and persists its
        // Error-type message. The loop MUST terminate immediately at the fail
        // message: no further LLM calls, no further messages. handle_response
        // short-circuits on the terminal status and returns the last message
        // as-is, so the Error-type message stays the thread's last word.
        let terminal_status = queries::get_thread_status(&cfg.pool, thread.id)
            .await
            .ok()
            .flatten()
            .filter(|s| is_terminal_loop_status(s));
        if let Some(terminal_status) = terminal_status {
            // The fail round's reasoning must never be persisted after the
            // fail message (step 8 below would insert it as a later message).
            final_reasoning = None;
            info!(
                "[executor] Thread {} ended by a tool (status='{}') - terminating the loop immediately",
                thread.id,
                terminal_status
            );
            break;
        }

        // Resync the in-memory sequence counter with the DB: a tool may have
        // inserted its own messages (e.g. fail-thread's Error message), so
        // the pre-allocated result seqs may have been bumped. The next
        // message must never reuse a seq that is already persisted.
        let db_max_after = queries::get_max_thread_sequence(&cfg.pool, thread.id)
            .await
            .unwrap_or(0);
        if db_max_after >= *next_seq {
            *next_seq = db_max_after + 1;
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
            helpers::enqueue_delivery_collapsed(
                &cfg.ctx,
                &reasoning_saved,
                channel,
                thread,
                cause_msg.external_id.clone(),
                &mut collapse,
                false,
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
        &mut collapse,
    )
    .await?;
    Ok(saved)
}

// ── Truncation escalation helpers (pure, unit-tested) ─────────────────────

/// Cap for the preserved reasoning note injected on a truncation retry
/// (reasoning-forward: the model must NOT re-derive the chain). Only applied
/// for CONTENT truncation - reasoning-only truncation drops the reasoning
/// entirely (it IS the overflow cause).
const PRESERVED_REASONING_CHARS: usize = 16000;

/// Classification of a truncated (finish_reason=length) response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruncationKind {
    /// Partial content was produced (content non-empty); reasoning may or may
    /// not be present. Recovery preserves the partial content + reasoning.
    ContentTruncated,
    /// The output budget was consumed ENTIRELY by reasoning (content empty).
    /// Recovery must NOT preserve the reasoning - it caused the overflow, and
    /// reasoning models restart their chain on each retry, regenerating the
    /// same oversized thought and re-triggering length. The retry instead
    /// demands a thinking-free response so the budget frees up.
    ReasoningOnly,
}

/// Action to take for one LLM response based on truncation + recovery state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruncationAction {
    /// Response not truncated: proceed normally and reset the recovery state.
    Continue,
    /// Truncated and recovery retries remain: retry with an escalating
    /// shorter-response nudge. `attempt` is 1-based (1..=max_retries).
    Retry { attempt: u32, kind: TruncationKind },
    /// Truncated and the per-thread retry budget is exhausted: give up
    /// truthfully (surfacing exactly what was trimmed), never loop forever.
    GiveUp,
}

/// Pure decision function. A recoverable length truncation NEVER fails the
/// thread outright: it retries (bounded by `max_retries`) with escalating
/// shorter-response nudges. Only when the retry budget is exhausted does it
/// GiveUp - and even then the give-up reports precisely what was trimmed and
/// preserves the thread's in-progress tool work. The stuck/empty-thread
/// safety valve lives in the empty-content / reasoning-only voluntary-stop
/// paths BELOW the truncation branch, so a recoverable truncation (which is
/// classified Retry/GiveUp here) never trips it.
fn truncation_action(
    truncated: bool,
    retries_used: u32,
    max_retries: u32,
    content_empty: bool,
    has_reasoning: bool,
) -> TruncationAction {
    if !truncated {
        return TruncationAction::Continue;
    }
    if retries_used >= max_retries {
        return TruncationAction::GiveUp;
    }
    let kind = if content_empty && has_reasoning {
        TruncationKind::ReasoningOnly
    } else {
        TruncationKind::ContentTruncated
    };
    TruncationAction::Retry {
        attempt: retries_used + 1,
        kind,
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

/// Messages appended for a truncation retry: preserved reasoning note (only
/// for CONTENT truncation; reasoning-only truncation drops the reasoning
/// because it IS the overflow), any partial content, and an ESCALATING
/// shorter-response nudge. Each retry demands a smaller output (attempt 1:
/// SHORTER; attempt 2: MUCH SHORTER + multi-part emission across turns;
/// attempt 3+: EXTREMELY condensed) so the loop converges instead of
/// regenerating the same oversized response.
fn truncation_retry_messages(
    reasoning: Option<&str>,
    content: &str,
    attempt: u32,
    max_retries: u32,
    kind: TruncationKind,
) -> Vec<ChatMessage> {
    let mut msgs = Vec::new();
    match kind {
        TruncationKind::ContentTruncated => {
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
        }
        TruncationKind::ReasoningOnly => {
            // Nothing preserved: the reasoning consumed the whole budget and
            // produced no content. Preserving it would re-derive the chain.
        }
    }
    let nudge = match (attempt, kind) {
        (_, TruncationKind::ReasoningOnly) => format!(
            "[System] Your previous response was cut off by the token limit: your REASONING consumed the entire output budget and produced no content (attempt {attempt}/{max_retries}). \
             Do NOT think out loud - SKIP reasoning entirely and emit your response directly: a single small tool call, or a concise final answer."
        ),
        (1, TruncationKind::ContentTruncated) => format!(
            "[System] Your previous response was cut off by the token limit (attempt 1/{max_retries}). \
             The reasoning above is preserved. Produce a SHORTER response now: emit a single small \
             tool call or a concise final answer. Do NOT regenerate the long reasoning chain."
        ),
        (2, TruncationKind::ContentTruncated) => format!(
            "[System] Your previous response was again cut off by the token limit (attempt 2/{max_retries}). \
             Produce a MUCH SHORTER, condensed response: strip all elaboration and emit only the essential \
             next action (one small tool call) or a terse final answer. If the deliverable cannot fit in a \
             single response, emit it in PARTS across successive turns: write each part with a tool call \
             (filesystem_write append=true, notes_note-append), keeping every response small, then close \
             with a concise answer."
        ),
        _ => format!(
            "[System] Your responses keep hitting the token limit (attempt {attempt}/{max_retries}). \
             Produce an EXTREMELY condensed response: at most a few lines. Emit one minimal tool call or a \
             one-paragraph final answer. If the deliverable is large, deliver it in PARTS via successive \
             small tool calls - never one oversized block."
        ),
    };
    msgs.push(ChatMessage::system(&nudge));
    msgs
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn not_truncated_continues() {
        assert_eq!(
            truncation_action(false, 0, 3, false, false),
            TruncationAction::Continue
        );
        // A non-truncated response with empty content + reasoning is the
        // voluntary-stop case handled by the safety valve BELOW the truncation
        // branch - never classified as a truncation.
        assert_eq!(
            truncation_action(false, 3, 3, true, true),
            TruncationAction::Continue
        );
    }

    #[test]
    fn truncated_retries_with_escalating_attempt() {
        assert_eq!(
            truncation_action(true, 0, 3, false, true),
            TruncationAction::Retry {
                attempt: 1,
                kind: TruncationKind::ContentTruncated,
            }
        );
        assert_eq!(
            truncation_action(true, 1, 3, false, false),
            TruncationAction::Retry {
                attempt: 2,
                kind: TruncationKind::ContentTruncated,
            }
        );
        assert_eq!(
            truncation_action(true, 2, 3, true, true),
            TruncationAction::Retry {
                attempt: 3,
                kind: TruncationKind::ReasoningOnly,
            }
        );
    }

    #[test]
    fn reasoning_only_classified_when_content_empty() {
        // content empty + reasoning present -> ReasoningOnly (budget consumed by thinking)
        assert_eq!(
            truncation_action(true, 0, 3, true, true),
            TruncationAction::Retry {
                attempt: 1,
                kind: TruncationKind::ReasoningOnly,
            }
        );
        // content empty + NO reasoning -> ContentTruncated (nothing to preserve)
        assert_eq!(
            truncation_action(true, 0, 3, true, false),
            TruncationAction::Retry {
                attempt: 1,
                kind: TruncationKind::ContentTruncated,
            }
        );
    }

    #[test]
    fn retry_budget_is_bounded_give_up_not_infinite() {
        // max_retries=3: attempts 1..=3 retry, the 4th consecutive truncation
        // gives up - bounded, never an infinite loop.
        assert!(matches!(
            truncation_action(true, 0, 3, false, false),
            TruncationAction::Retry { .. }
        ));
        assert!(matches!(
            truncation_action(true, 1, 3, false, false),
            TruncationAction::Retry { .. }
        ));
        assert!(matches!(
            truncation_action(true, 2, 3, false, false),
            TruncationAction::Retry { .. }
        ));
        assert_eq!(
            truncation_action(true, 3, 3, false, false),
            TruncationAction::GiveUp
        );
        // Zero retries configured: first truncation gives up immediately.
        assert_eq!(
            truncation_action(true, 0, 0, false, false),
            TruncationAction::GiveUp
        );
        // Never loops: retries_used >= max always gives up.
        assert_eq!(
            truncation_action(true, 5, 3, false, false),
            TruncationAction::GiveUp
        );
    }

    #[test]
    fn giveup_contract_forces_failed_status() {
        // GiveUp contract: the executor loop sets force_failed=true so the
        // thread's final status is "failed" (NOT "completed") - a failed
        // executor thread goes blocked (or review with review_on_fail), it
        // never advances to the tester. This is NOT the old bare "3x length ->
        // truthful fail": GiveUp only fires after escalating shorter-response
        // retries were attempted, and the give-up surfaces what was trimmed.
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
        let msgs = truncation_retry_messages(
            Some("think step by step"),
            "partial",
            1,
            3,
            TruncationKind::ContentTruncated,
        );
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[0].content.contains("Preserved Reasoning"));
        assert!(msgs[0].content.contains("think step by step"));
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content, "partial");
        let nudge = &msgs[2];
        assert_eq!(nudge.role, "system");
        assert!(nudge.content.contains("attempt 1/3"));
        assert!(nudge.content.contains("SHORTER"));
        assert!(nudge.content.contains("Do NOT regenerate"));
    }

    #[test]
    fn retry_messages_skip_empty_reasoning() {
        let msgs = truncation_retry_messages(
            Some("   "),
            "partial",
            1,
            3,
            TruncationKind::ContentTruncated,
        );
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[0].content, "partial");
        assert!(msgs[1].content.contains("attempt 1/3"));
    }

    #[test]
    fn retry_messages_include_partial_content() {
        let msgs = truncation_retry_messages(
            None,
            "half of a sentence",
            1,
            3,
            TruncationKind::ContentTruncated,
        );
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[0].content, "half of a sentence");
        assert!(msgs[1].content.contains("attempt 1/3"));
    }

    #[test]
    fn retry_messages_escalate_to_parts_on_second_attempt() {
        let msgs = truncation_retry_messages(None, "", 2, 3, TruncationKind::ContentTruncated);
        assert_eq!(msgs.len(), 1); // no reasoning, no partial content
        let nudge = &msgs[0];
        assert!(nudge.content.contains("attempt 2/3"));
        assert!(nudge.content.contains("MUCH SHORTER"));
        assert!(nudge.content.contains("PARTS"));
        assert!(nudge.content.contains("filesystem_write append=true"));
    }

    #[test]
    fn retry_messages_extreme_condensation_on_third_attempt() {
        let msgs = truncation_retry_messages(None, "", 3, 3, TruncationKind::ContentTruncated);
        let nudge = &msgs[0];
        assert!(nudge.content.contains("attempt 3/3"));
        assert!(nudge.content.contains("EXTREMELY condensed"));
    }

    #[test]
    fn reasoning_only_retry_drops_reasoning_and_demands_no_thinking() {
        // Reasoning-only truncation: the reasoning IS the problem - it must
        // NOT be preserved (preserving it re-triggers the same oversized chain
        // on the retry) and the nudge must demand a thinking-free response.
        let msgs = truncation_retry_messages(
            Some("huge reasoning chain"),
            "",
            1,
            3,
            TruncationKind::ReasoningOnly,
        );
        assert_eq!(msgs.len(), 1);
        let nudge = &msgs[0];
        assert!(!nudge.content.contains("Preserved Reasoning"));
        assert!(!nudge.content.contains("huge reasoning chain"));
        assert!(nudge.content.contains("SKIP reasoning"));
        assert!(nudge.content.contains("attempt 1/3"));
    }

    #[test]
    fn recoverable_truncation_never_trips_voluntary_stop_path() {
        // The safety valve (stuck/empty thread) must NOT fire on a recoverable
        // truncation: every truncated response is classified Retry (or GiveUp
        // after the budget) - never Continue. Continue is reserved for
        // genuinely non-truncated responses, the only input that reaches the
        // reasoning-only voluntary-stop terminal below the truncation branch.
        assert!(!matches!(
            truncation_action(true, 0, 3, true, true),
            TruncationAction::Continue
        ));
        assert_eq!(
            truncation_action(false, 0, 3, true, true),
            TruncationAction::Continue
        );
    }
}

#[allow(dead_code)]
#[allow(clippy::items_after_test_module)]
/// Pure: should the prompt message be logged before this LLM call?
/// Levels: "off" never; "first" only the first; "first+compact" the first or
/// right after a condensation; "all" every non-retry call. A retry of a
/// failed LLM call (provider error / empty response / truncation / forced
/// compaction: all do `current_iter -= 1` then `continue`) carries the SAME
/// current_iter as the attempt that already logged a prompt, so it is
/// skipped - the invariant is that every prompt message is immediately
/// followed by the LLM response (or its failure), never by another prompt.
fn should_log_prompt_for(
    log_level: &str,
    has_logged_first: bool,
    current_iter: i32,
    last_prompt_iter: i32,
    last_condense_iter: i32,
) -> bool {
    if log_level == "off" {
        return false;
    }
    if current_iter == last_prompt_iter {
        return false;
    }
    match log_level {
        "first" => !has_logged_first,
        "first+compact" => !has_logged_first || current_iter == last_condense_iter,
        "all" => true,
        _ => false,
    }
}

/// Pure: the sequence number for a persisted tool result. The pre-allocated
/// `preallocated` seq may already be taken by a message the tool itself
/// inserted while executing (e.g. builtin_fail-thread's Error message at
/// get_max_thread_sequence + 1). max(db_max + 1, preallocated) guarantees the
/// result never collides with anything already persisted.
fn effective_result_seq(db_max: i32, preallocated: i32) -> i32 {
    (db_max + 1).max(preallocated)
}

/// Pure: is a thread status terminal for the executor loop? When a tool
/// ended the thread (e.g. builtin_fail-thread -> 'failed'), the loop must
/// stop immediately and write no further messages.
fn is_terminal_loop_status(status: &str) -> bool {
    matches!(
        status,
        "failed" | "completed" | "interrupted" | "skipped" | "system" | "merged"
    )
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod thread_lifecycle_tests {
    use super::*;

    #[test]
    fn prompt_logged_once_per_attempt_not_on_retries() {
        // First attempt at iteration 1: logged.
        assert!(should_log_prompt_for("all", false, 1, -1, 0));
        // Retry of the same attempt (current_iter unchanged): NOT logged.
        assert!(!should_log_prompt_for("all", true, 1, 1, 0));
        // A real next iteration: logged again.
        assert!(should_log_prompt_for("all", true, 2, 1, 0));
        // "first" level: only while never logged.
        assert!(should_log_prompt_for("first", false, 1, -1, 0));
        assert!(!should_log_prompt_for("first", true, 2, 1, 0));
        // "first+compact": first + post-compaction (iter == last condense).
        assert!(should_log_prompt_for("first+compact", true, 5, 2, 5));
        assert!(!should_log_prompt_for("first+compact", true, 6, 5, 5));
        // Retry after a compaction attempt: skipped (same iteration).
        assert!(!should_log_prompt_for("first+compact", true, 5, 5, 5));
        // "off": never.
        assert!(!should_log_prompt_for("off", false, 1, -1, 0));
    }

    #[test]
    fn retry_after_empty_response_does_not_double_prompt() {
        // Observed bug: provider error / empty response did `current_iter -= 1`
        // + continue, re-logging the prompt on the retry (two consecutive
        // prompt messages, no LLM response between them - thread 467,
        // messages #37482 + #37484).
        let mut current_iter = 3;
        let mut last_prompt_iter = -1;
        let mut has_logged_first = false;
        assert!(should_log_prompt_for(
            "all",
            has_logged_first,
            current_iter,
            last_prompt_iter,
            0
        ));
        has_logged_first = true;
        last_prompt_iter = current_iter;
        // LLM call fails -> retry path does current_iter -= 1, then the loop
        // top increments again: the retry sees the SAME current_iter.
        current_iter -= 1;
        current_iter += 1;
        assert!(!should_log_prompt_for(
            "all",
            has_logged_first,
            current_iter,
            last_prompt_iter,
            0
        ));
    }

    #[test]
    fn effective_result_seq_never_collides_with_tool_inserted_message() {
        // fail-thread tool inserted its Error message at get_max_thread_sequence
        // + 1, which equals the pre-allocated result seq: the result must be
        // bumped past it (thread 467 shared seq 38).
        assert_eq!(effective_result_seq(38, 38), 39);
        // A parallel sibling result: unique vs the error at 38.
        assert_eq!(effective_result_seq(38, 39), 39);
        // Tool inserted two messages: bump past both.
        assert_eq!(effective_result_seq(40, 38), 41);
        // Normal case (no tool-inserted messages): pre-allocated seq wins.
        assert_eq!(effective_result_seq(9, 10), 10);
        assert_eq!(effective_result_seq(9, 11), 11);
    }

    #[test]
    fn terminal_loop_statuses_stop_the_loop() {
        assert!(is_terminal_loop_status("failed"));
        assert!(is_terminal_loop_status("completed"));
        assert!(is_terminal_loop_status("interrupted"));
        assert!(is_terminal_loop_status("skipped"));
        assert!(is_terminal_loop_status("system"));
        assert!(is_terminal_loop_status("merged"));
        for s in ["running", "pending", "processing", "review", ""] {
            assert!(!is_terminal_loop_status(s), "{s:?} must not be terminal");
        }
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

#[cfg(test)]
mod plan_iteration_tests {
    use super::*;

    #[test]
    fn failed_plan_does_not_skip_first_main_loop_iteration() {
        // should_plan=true but plan generation FAILED: plan_content is None.
        // plan_consumed = 0, so the first main-loop prompt (current_iter
        // starts at plan_consumed and is incremented before each call) is
        // iteration 1, not 2: no gap in the user-visible sequence.
        let consumed = plan_iterations_consumed(&None);
        assert_eq!(consumed, 0);
        assert_eq!(1 + consumed, 1); // first main-loop prompt iteration
    }

    #[test]
    fn successful_plan_moves_first_main_loop_prompt_to_iteration_two() {
        // should_plan=true and a plan WAS generated: plan_content is Some.
        // plan_consumed = 1 (the plan prompt is logged at iteration 0 and the
        // plan message at iteration 1), so the first main-loop prompt is
        // iteration 2.
        let consumed = plan_iterations_consumed(&Some("<plan>1. do it</plan>".to_string()));
        assert_eq!(consumed, 1);
        assert_eq!(1 + consumed, 2); // first main-loop prompt iteration
    }

    #[test]
    fn skipped_plan_leaves_first_iteration_at_one() {
        // should_plan=false: no plan phase at all; first prompt is iteration 1.
        assert_eq!(plan_iterations_consumed(&None), 0);
        assert_eq!(1 + plan_iterations_consumed(&None), 1);
    }
}
