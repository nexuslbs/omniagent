use crate::agent::task_registry;
/// Built-in task management tools for non-blocking tool execution.
/// Registered in default_registry() alongside list_tool_details and read_attached_file.
use crate::error::AppResult;
use crate::mcp::{AppContext, McpToolResult};
use serde_json::Value;

/// Build the arguments for poll_task, wait_task, cancel_task, read_task_logs
fn get_task_id(args: &Value) -> Option<String> {
    args.get("task_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub async fn handle_poll_task(args: Value, _ctx: AppContext) -> AppResult<McpToolResult> {
    let task_id = get_task_id(&args).unwrap_or_default();
    let registry = task_registry::TASK_REGISTRY
        .get()
        .cloned()
        .expect("TASK_REGISTRY not initialized");

    let info = registry.get_info(&task_id).await;
    match info {
        Some(info) => {
            let status_str = match &info.status {
                task_registry::TaskStatus::Running => "running",
                task_registry::TaskStatus::Completed(_) => "completed",
                task_registry::TaskStatus::Failed(_) => "failed",
                task_registry::TaskStatus::Cancelled => "cancelled",
            };
            let mut result = serde_json::json!({
                "status": status_str,
                "task_id": task_id,
                "tool": info.tool_name,
                "elapsed_secs": info.start_time.elapsed().as_secs_f64(),
            });
            if let task_registry::TaskStatus::Completed(output) = &info.status {
                result["result"] = Value::String(output.clone());
            }
            if let task_registry::TaskStatus::Failed(err) = &info.status {
                result["error"] = Value::String(err.clone());
            }
            Ok(McpToolResult {
                call_id: String::new(),
                content: result.to_string(),
                is_error: false,
            })
        }
        None => Ok(McpToolResult {
            call_id: String::new(),
            content: serde_json::json!({"status": "not_found", "task_id": task_id}).to_string(),
            is_error: false,
        }),
    }
}

pub async fn handle_wait_task(args: Value, _ctx: AppContext) -> AppResult<McpToolResult> {
    let task_id = get_task_id(&args).unwrap_or_default();
    // Default to a GENEROUS wait: the loop polls every 500ms and returns as
    // soon as the task completes, so a long default costs nothing when the
    // task is fast. A short default (30s) made agents that omit timeout_secs
    // burn one iteration per 30s of every long build/test - observed killing
    // threads at the iteration cap mid-cargo-build (Aug 2026). 900s covers a
    // full Rust release build / dev-stack setup in a single call.
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(900);
    let tail = args.get("tail").and_then(|v| v.as_u64()).unwrap_or(1000) as usize;
    let registry = task_registry::TASK_REGISTRY
        .get()
        .cloned()
        .expect("TASK_REGISTRY not initialized");

    // Helper: read all logs and return last `tail` chars as a truncated string
    let get_log_tail = || async {
        let (lines, _) = registry.read_logs(&task_id, None, Some(10_000)).await;
        let joined = lines.join("\n");
        if joined.is_empty() || tail == 0 {
            return joined;
        }
        if joined.len() <= tail {
            return joined;
        }
        let truncated: String = joined
            .chars()
            .rev()
            .take(tail)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!(
            "...(showing last {} of {} chars)\n{}",
            tail,
            joined.len(),
            truncated
        )
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let info = registry.get_info(&task_id).await;
        match info {
            Some(info) => {
                let done = matches!(
                    &info.status,
                    task_registry::TaskStatus::Completed(_)
                        | task_registry::TaskStatus::Failed(_)
                        | task_registry::TaskStatus::Cancelled
                );
                if done {
                    let logs = get_log_tail().await;
                    let mut result = serde_json::json!({
                        "status": "completed",
                        "task_id": task_id,
                        "tool": info.tool_name,
                        "elapsed_secs": info.start_time.elapsed().as_secs_f64(),
                        "logs": logs,
                    });
                    match &info.status {
                        task_registry::TaskStatus::Completed(output) => {
                            result["result"] = Value::String(output.clone());
                        }
                        task_registry::TaskStatus::Failed(err) => {
                            result["error"] = Value::String(err.clone());
                        }
                        _ => {}
                    }
                    return Ok(McpToolResult {
                        call_id: String::new(),
                        content: result.to_string(),
                        is_error: false,
                    });
                }
            }
            None => {
                return Ok(McpToolResult {
                    call_id: String::new(),
                    content: serde_json::json!({"status": "not_found", "task_id": task_id})
                        .to_string(),
                    is_error: false,
                });
            }
        }
        if std::time::Instant::now() >= deadline {
            let logs = get_log_tail().await;
            let info = registry.get_info(&task_id).await;
            return match info {
                Some(info) => {
                    let elapsed = info.start_time.elapsed().as_secs_f64();
                    Ok(McpToolResult {
                        call_id: String::new(),
                        content: serde_json::json!({
                            "status": "timeout",
                            "task_id": task_id,
                            "tool": info.tool_name,
                            "elapsed_secs": elapsed,
                            "message": format!("Task still running after {}s timeout", timeout_secs),
                            "logs": logs,
                        }).to_string(),
                        is_error: false,
                    })
                }
                None => Ok(McpToolResult {
                    call_id: String::new(),
                    content: serde_json::json!({"status": "not_found", "task_id": task_id})
                        .to_string(),
                    is_error: false,
                }),
            };
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

pub async fn handle_cancel_task(args: Value, _ctx: AppContext) -> AppResult<McpToolResult> {
    let task_id = get_task_id(&args).unwrap_or_default();
    let registry = task_registry::TASK_REGISTRY
        .get()
        .cloned()
        .expect("TASK_REGISTRY not initialized");

    let outcome = registry.cancel(&task_id).await;
    // Honest report: a task that already finished is NOT relabelled
    // `cancelled` - the caller is told what actually happened (and, when the
    // task is still running but the underlying operation cannot be aborted,
    // the task is still marked cancelled because the agent-side lifetime ends
    // here; the answer names that in `note`).
    let mut body = serde_json::json!({
        "task_id": task_id,
    });
    match outcome {
        task_registry::CancelOutcome::Cancelled => {
            body["status"] = Value::String("cancelled".to_string());
            body["note"] = Value::String(
                "the in-flight call was torn down (the MCP request is cancelled, so a plugin with \
                 a kill-on-drop guard also stops its underlying process/request)"
                    .to_string(),
            );
        }
        task_registry::CancelOutcome::AlreadyFinished => {
            let current = registry
                .get_info(&task_id)
                .await
                .map(|info| match info.status {
                    task_registry::TaskStatus::Completed(_) => "completed".to_string(),
                    task_registry::TaskStatus::Failed(_) => "failed".to_string(),
                    task_registry::TaskStatus::Cancelled => "cancelled".to_string(),
                    task_registry::TaskStatus::Running => "running".to_string(),
                })
                .unwrap_or_else(|| "unknown".to_string());
            body["status"] = Value::String("already_finished".to_string());
            body["current_status"] = Value::String(current);
        }
        task_registry::CancelOutcome::NotFound => {
            body["status"] = Value::String("not_found".to_string());
        }
    }
    Ok(McpToolResult {
        call_id: String::new(),
        content: body.to_string(),
        is_error: false,
    })
}

pub async fn handle_read_task_logs(args: Value, _ctx: AppContext) -> AppResult<McpToolResult> {
    let task_id = get_task_id(&args).unwrap_or_default();
    let cursor = args
        .get("cursor")
        .and_then(|v| v.as_u64())
        .map(|c| c as usize);
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|l| l as usize);
    let registry = task_registry::TASK_REGISTRY
        .get()
        .cloned()
        .expect("TASK_REGISTRY not initialized");

    let (lines, next_cursor) = registry.read_logs(&task_id, cursor, limit).await;
    Ok(McpToolResult {
        call_id: String::new(),
        content: serde_json::json!({
            "status": "ok",
            "task_id": task_id,
            "lines": lines,
            "next_cursor": next_cursor,
        })
        .to_string(),
        is_error: false,
    })
}

/// Handle the builtin `call-and-wait` tool: call a tool with the given params
/// and immediately wait for its background task, in a SINGLE call (operator
/// request 2026-09-30: "2 tool calls in 1").
///
/// Flow: (1) the agent must have permission to call the WRAPPED tool - the
/// same check as if it called it directly (an agent without permission gets a
/// permission error and the tool is NOT invoked); (2) the wrapped tool is
/// executed with the params, bounded by the outer `timeout`; (3) if it returns
/// a background task handle (`status=processing` + `task_id`), the core
/// wait-task behavior is applied to that task id with the same timeout; (4) a
/// completed result is returned immediately, and a timeout returns the
/// timeout outcome the core wait task would return.
pub async fn handle_call_and_wait(args: Value, ctx: AppContext) -> AppResult<McpToolResult> {
    let tool_name = match get_wrapped_tool(&args) {
        Some(name) => name,
        None => {
            return Ok(McpToolResult {
                call_id: String::new(),
                content: "Error: 'tool' parameter is required: the fully qualified name of the tool to call, e.g. ssh__run or docker__compose."
                    .to_string(),
                is_error: true,
            });
        }
    };
    let params = args.get("params").cloned().unwrap_or(serde_json::json!({}));
    let timeout_secs = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(900);

    // Snapshot the live registry through the global plugin-manager handle
    // (the same registry the agent loop dispatches against, including plugin
    // reloads). Absent handle = startup not finished: fail loudly.
    let registry = crate::agent::plugin_manager::PLUGIN_MANAGER
        .get()
        .ok_or_else(|| {
            crate::error::Error::Message(
                "core__call_and_wait: plugin manager not initialized".to_string(),
            )
        })?
        .snapshot_registry()
        .await;

    // MANDATORY permission check: the agent must have permission to call the
    // WRAPPED tool, exactly as if it called it directly. Without this gate the
    // toolset filter (which hides disallowed tools from the LLM) could be
    // bypassed by routing a call through core__call_and_wait.
    if !wrapped_tool_permitted(&registry, ctx.current_allowed_tools.as_deref(), &tool_name) {
        return Ok(McpToolResult {
            call_id: String::new(),
            content: format!(
                "Permission denied: '{}' is not in the effective allowed tools for this thread, so core__call_and_wait cannot invoke it. Call the tool directly if you believe it should be allowed.",
                tool_name
            ),
            is_error: true,
        });
    }

    if registry.get(&tool_name).is_none() {
        return Ok(McpToolResult {
            call_id: String::new(),
            content: format!(
                "Error: unknown tool '{}' - it is not registered in the current tool registry.",
                tool_name
            ),
            is_error: true,
        });
    }

    execute_call_and_wait(registry, tool_name, params, timeout_secs, ctx).await
}

/// The timeout-bounded execution core shared by `handle_call_and_wait` and the
/// unit tests: run the wrapped tool, wait on a returned background task handle
/// (status=processing) with the outer timeout, return completed results
/// immediately and shape a timeout outcome like the core wait task's.
async fn execute_call_and_wait(
    registry: crate::mcp::McpRegistry,
    tool_name: String,
    params: Value,
    timeout_secs: u64,
    ctx: AppContext,
) -> AppResult<McpToolResult> {
    let call = crate::mcp::McpToolCall {
        id: String::new(),
        name: tool_name.clone(),
        arguments: params,
    };
    let started = std::time::Instant::now();
    let wrapped = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        registry.execute(&call, ctx.clone()),
    )
    .await;

    match wrapped {
        Ok(Ok(res)) => {
            // The wrapped tool returned a background task handle: wait on it
            // with the timeout given in the outer call (the wait-task
            // behavior, unchanged). A completed result is returned as-is.
            if let Some(task_id) = processing_task_id(&res.content) {
                return handle_wait_task(
                    serde_json::json!({
                        "task_id": task_id,
                        "timeout_secs": timeout_secs,
                    }),
                    ctx,
                )
                .await;
            }
            Ok(res)
        }
        Ok(Err(e)) => Err(e),
        Err(_) => {
            // Timeout outcome, shaped like the core wait-task timeout: the
            // in-flight call was torn down, so the agent knows the work did
            // NOT continue in the background.
            let elapsed = started.elapsed().as_secs_f64();
            Ok(McpToolResult {
                call_id: String::new(),
                content: serde_json::json!({
                    "status": "timeout",
                    "tool": tool_name,
                    "elapsed_secs": elapsed,
                    "message": format!(
                        "Tool '{}' still running after {}s timeout (the in-flight call was torn down)",
                        tool_name, timeout_secs
                    ),
                })
                .to_string(),
                is_error: false,
            })
        }
    }
}

/// The wrapped tool name from call-and-wait args (trimmed, non-empty).
fn get_wrapped_tool(args: &Value) -> Option<String> {
    args.get("tool")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Extract a background task id from a tool result that used the standard
/// `status=processing` envelope; `None` for any completed/errored/non-JSON
/// result (the common case: direct execution returns the final result).
fn processing_task_id(content: &str) -> Option<String> {
    let v: Value = serde_json::from_str(content).ok()?;
    if v.get("status").and_then(|s| s.as_str()) == Some("processing") {
        v.get("task_id")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
    } else {
        None
    }
}

/// The permission gate for the wrapped tool: the agent must be permitted to
/// call it directly. `None` (no restriction) permits everything; otherwise the
/// registry's own allow-list logic (`McpRegistry::allowed`, the SAME logic the
/// toolset filter applies) decides.
fn wrapped_tool_permitted(
    registry: &crate::mcp::McpRegistry,
    allowed: Option<&[String]>,
    tool_name: &str,
) -> bool {
    match allowed {
        None => true,
        Some(names) => registry.allowed(names).iter().any(|t| t.name == tool_name),
    }
}

/// Handle the builtin `wait-for-status` tool: wait until a kanban task or
/// thread reaches one of the target statuses (bounded by timeout_s). The
/// waiting core is crate::status_wait (DB status observation); this handler
/// only parses args and shapes the outcome JSON.
pub async fn handle_wait_for_status(args: Value, ctx: AppContext) -> AppResult<McpToolResult> {
    let task_id = args
        .get("task_id")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let thread_id: Option<i64> = match args.get("thread_id") {
        Some(v) => v
            .as_i64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok())),
        None => None,
    };

    // Exactly one of task_id / thread_id.
    let (entity, id) = match (task_id, thread_id) {
        (Some(tid), None) => (crate::status_wait::WaitEntity::KanbanTask, tid),
        (None, Some(th)) => (crate::status_wait::WaitEntity::Thread, th.to_string()),
        (Some(_), Some(_)) => {
            return Ok(McpToolResult {
                call_id: String::new(),
                content: "Error: pass exactly ONE of task_id (kanban task) or thread_id (thread), not both."
                    .to_string(),
                is_error: true,
            });
        }
        (None, None) => {
            return Ok(McpToolResult {
                call_id: String::new(),
                content: "Error: pass task_id (kanban task) or thread_id (thread) to wait on."
                    .to_string(),
                is_error: true,
            });
        }
    };

    let until_raw = args.get("until").and_then(|v| v.as_str()).unwrap_or("");
    let until = crate::status_wait::parse_until(until_raw);
    if until.is_empty() {
        return Ok(McpToolResult {
            call_id: String::new(),
            content: "Error: 'until' must be a non-empty comma-separated status list, e.g. until=done,blocked or until=completed,failed."
                .to_string(),
            is_error: true,
        });
    }

    // CORE GUARD against a self-referential wait (incident telegram
    // 4122/4126): a thread that waits on the task it is itself running can
    // only be answered when that thread ends, so the call would block for the
    // whole timeout while the agent polls pointlessly. Refuse it BEFORE any
    // polling starts.
    if let Some(refusal) =
        crate::status_wait::self_wait_error(&ctx.pool, entity, &id, &until, ctx.current_thread_id)
            .await?
    {
        return Ok(McpToolResult {
            call_id: String::new(),
            content: format!("Error: {refusal}"),
            is_error: true,
        });
    }

    let timeout_s = args
        .get("timeout_s")
        .and_then(|v| v.as_u64())
        .unwrap_or(900);
    let outcome = crate::status_wait::wait_for_status(
        &ctx.pool,
        entity,
        &id,
        &until,
        timeout_s,
        std::time::Duration::from_millis(1000),
    )
    .await?;

    let result = if outcome.reached {
        "matched"
    } else if outcome.status.is_none() {
        "not_found"
    } else {
        "timeout"
    };
    Ok(McpToolResult {
        call_id: String::new(),
        content: serde_json::json!({
            "status": result,
            "entity": outcome.entity.as_str(),
            "entity_id": outcome.id,
            "current_status": outcome.status,
            "until": until,
            "elapsed_secs": outcome.elapsed_secs,
            "detail": outcome.detail,
        })
        .to_string(),
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_get_task_id_with_valid_string() {
        let args = json!({"task_id": "abc-123"});
        assert_eq!(get_task_id(&args), Some("abc-123".to_string()));
    }

    #[test]
    fn test_get_task_id_missing_key() {
        let args = json!({"other": "value"});
        assert_eq!(get_task_id(&args), None);
    }

    #[test]
    fn test_get_task_id_non_string_value() {
        let args = json!({"task_id": 42});
        assert_eq!(get_task_id(&args), None);
    }

    #[test]
    fn test_get_task_id_null_value() {
        let args = json!({"task_id": null});
        assert_eq!(get_task_id(&args), None);
    }

    #[test]
    fn test_get_task_id_empty_string() {
        let args = json!({"task_id": ""});
        assert_eq!(get_task_id(&args), Some("".to_string()));
    }

    #[test]
    fn test_get_task_id_empty_object() {
        let args = json!({});
        assert_eq!(get_task_id(&args), None);
    }
}

/// Handle the builtin `fail-thread` tool (Phase 2): ends the current thread
/// as FAILED with an Error-type last message and applies the
/// metadata.workflow_step kanban transition (spec §3 F0-F4, §8 N1/N6).
pub async fn handle_fail_thread(args: Value, ctx: AppContext) -> AppResult<McpToolResult> {
    let thread_id = match ctx.current_thread_id {
        Some(id) => id,
        None => {
            return Ok(McpToolResult {
                call_id: String::new(),
                content: "Error: builtin_fail-thread requires an active thread (current_thread_id is None). It can only be called from inside a thread execution.".to_string(),
                is_error: true,
            });
        }
    };

    let workflow_step = args
        .get("workflow_step")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let thread = match crate::db::threads::get_thread_by_id(&ctx.pool, thread_id).await? {
        Some(t) => t,
        None => {
            return Ok(McpToolResult {
                call_id: String::new(),
                content: format!("Error: thread {} not found", thread_id),
                is_error: true,
            });
        }
    };

    // The fail-thread tool result IS the last message of the thread, so the
    // full reason must live in THIS single JSON message: one failure = one
    // trailing JSON message carrying the reason (no separate ack that leaves
    // the summary in an earlier message). Precedence (audit HV-E3): explicit
    // reason -> profile override -> operator setting -> code default.
    let reason_text = reason.clone().unwrap_or_else(|| {
        crate::agent::fail_thread::default_fail_reason_for_profile(Some(&thread.profile))
    });

    let saved = crate::agent::fail_thread::fail_thread_tool(
        &ctx,
        &thread,
        workflow_step.as_deref(),
        Some(reason_text.clone()),
    )
    .await?;

    Ok(McpToolResult {
        call_id: String::new(),
        content: serde_json::json!({
            "ok": true,
            "thread_id": thread_id,
            "status": "failed",
            "error_message_id": saved.id,
            "workflow_step": workflow_step.unwrap_or_default(),
            "reason": reason_text,
        })
        .to_string(),
        is_error: false,
    })
}

#[cfg(test)]
mod fail_thread_tests {

    #[test]
    fn normalize_workflow_step_accepts_only_step_keys() {
        use crate::agent::fail_thread::normalize_workflow_step;
        assert_eq!(normalize_workflow_step(None), "executor");
        assert_eq!(normalize_workflow_step(Some("")), "executor");
        assert_eq!(normalize_workflow_step(Some("running")), "running");
        assert_eq!(normalize_workflow_step(Some("testing")), "testing");
        assert_eq!(normalize_workflow_step(Some("blocked")), "blocked");
        // N6: review and role names are NOT valid step keys → F4 (invalid).
        assert_eq!(normalize_workflow_step(Some("review")), "invalid");
        assert_eq!(normalize_workflow_step(Some("executor")), "invalid");
        assert_eq!(normalize_workflow_step(Some("tester")), "invalid");
        assert_eq!(normalize_workflow_step(Some("reviewer")), "invalid");
        assert_eq!(normalize_workflow_step(Some("bogus")), "invalid");
    }
}
#[cfg(test)]
mod call_and_wait_tests {
    use super::*;
    use crate::mcp::{McpRegistry, McpTool, ToolBehavior};
    use serde_json::json;
    use std::sync::Arc;

    /// A lazy (never-connecting) test AppContext; the fake tools below never
    /// touch the pool, so the connection is never attempted.
    fn test_ctx(allowed: Option<Vec<String>>) -> AppContext {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://user:pass@127.0.0.1:1/none")
            .expect("lazy pool");
        let mut ctx = AppContext::new(
            pool.clone(),
            pool,
            "/tmp",
            std::collections::HashMap::new(),
            Arc::new(crate::mcp::external::client::ExternalMcpClients::new()),
        );
        ctx.current_allowed_tools = allowed;
        ctx
    }

    /// A fake tool whose handler sleeps `sleep_secs` and then returns
    /// `output` verbatim. The plugin (server_name) is derived from the name
    /// prefix so the tool passes the registry's exposed-name validation (a
    /// `core` plugin is reserved and would be rejected).
    fn fake_tool(name: &str, sleep_secs: u64, output: &'static str) -> McpTool {
        let plugin = name
            .split_once("__")
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| "fake".to_string());
        McpTool {
            name: name.to_string(),
            description: "fake test tool".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            server_name: Some(plugin),
            timeout_secs: None,
            behavior: ToolBehavior::default(),
            handler: Arc::new(move |_args: Value, _ctx: AppContext| {
                let output = output.to_string();
                Box::pin(async move {
                    if sleep_secs > 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
                    }
                    Ok(McpToolResult {
                        call_id: String::new(),
                        content: output,
                        is_error: false,
                    })
                })
            }),
        }
    }

    // ─── arg parsing ───

    #[test]
    fn get_wrapped_tool_parses_qualified_name() {
        assert_eq!(
            get_wrapped_tool(&json!({"tool": "ssh__run", "params": {}})),
            Some("ssh__run".to_string())
        );
        assert_eq!(
            get_wrapped_tool(&json!({"tool": "  docker__compose "})),
            Some("docker__compose".to_string())
        );
    }

    #[test]
    fn get_wrapped_tool_rejects_missing_empty_non_string() {
        assert_eq!(get_wrapped_tool(&json!({})), None);
        assert_eq!(get_wrapped_tool(&json!({"tool": ""})), None);
        assert_eq!(get_wrapped_tool(&json!({"tool": "  "})), None);
        assert_eq!(get_wrapped_tool(&json!({"tool": 42})), None);
    }

    // ─── processing-envelope detection ───

    #[test]
    fn processing_task_id_detects_processing_envelope() {
        assert_eq!(
            processing_task_id(r#"{"status":"processing","task_id":"abc-123","tool":"ssh__run"}"#),
            Some("abc-123".to_string())
        );
    }

    #[test]
    fn processing_task_id_returns_none_for_completed_or_non_json() {
        assert_eq!(
            processing_task_id(r#"{"status":"completed","task_id":"x"}"#),
            None
        );
        assert_eq!(
            processing_task_id(r#"{"status":"timeout","task_id":"x"}"#),
            None
        );
        assert_eq!(processing_task_id("plain text output"), None);
        assert_eq!(processing_task_id(""), None);
    }

    // ─── permission gate ───

    #[test]
    fn wrapped_tool_permitted_without_restriction() {
        let registry = McpRegistry::new();
        assert!(wrapped_tool_permitted(&registry, None, "ssh__run"));
    }

    #[test]
    fn wrapped_tool_permitted_follows_the_allow_list() {
        let mut registry = McpRegistry::new();
        registry.register(fake_tool("ssh__run", 0, "ok"));
        registry.register(fake_tool("docker__compose", 0, "ok"));
        let allowed = vec!["ssh__run".to_string()];
        assert!(wrapped_tool_permitted(
            &registry,
            Some(&allowed),
            "ssh__run"
        ));
        assert!(!wrapped_tool_permitted(
            &registry,
            Some(&allowed),
            "docker__compose"
        ));
        // Empty allow-list = no tool allowed.
        assert!(!wrapped_tool_permitted(&registry, Some(&[]), "ssh__run"));
    }

    // ─── execution core ───

    #[tokio::test]
    async fn long_running_tool_returns_final_result_in_a_single_call() {
        let mut registry = McpRegistry::new();
        registry.register(fake_tool("slow_tool", 1, "FINAL-RESULT"));
        let result = execute_call_and_wait(
            registry,
            "slow_tool".to_string(),
            json!({}),
            30,
            test_ctx(None),
        )
        .await
        .expect("call_and_wait should succeed");
        assert!(
            result.content.contains("FINAL-RESULT"),
            "content: {}",
            result.content
        );
        assert!(
            !result.content.contains("processing"),
            "must not return the processing envelope"
        );
        assert!(!result.content.contains("timeout"));
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn fast_tool_returns_its_direct_result_immediately() {
        let mut registry = McpRegistry::new();
        registry.register(fake_tool("fast_tool", 0, "direct-done"));
        let started = std::time::Instant::now();
        let result = execute_call_and_wait(
            registry,
            "fast_tool".to_string(),
            json!({}),
            30,
            test_ctx(None),
        )
        .await
        .expect("call_and_wait should succeed");
        assert_eq!(result.content, "direct-done");
        assert!(started.elapsed().as_secs() < 5, "fast tool must not wait");
    }

    #[tokio::test]
    async fn short_timeout_returns_the_timeout_outcome() {
        let mut registry = McpRegistry::new();
        registry.register(fake_tool("slow_tool", 30, "never"));
        let started = std::time::Instant::now();
        let result = execute_call_and_wait(
            registry,
            "slow_tool".to_string(),
            json!({}),
            1,
            test_ctx(None),
        )
        .await
        .expect("timeout is a status, not an error");
        assert!(!result.is_error);
        let v: Value = serde_json::from_str(&result.content).expect("timeout outcome is JSON");
        assert_eq!(v["status"], "timeout");
        assert_eq!(v["tool"], "slow_tool");
        assert!(started.elapsed().as_secs_f64() >= 1.0);
        assert!(
            started.elapsed().as_secs() < 10,
            "must return right after the timeout"
        );
    }

    // ─── handler-level flow (permission gate + global registry) ───
    // Single test so the PLUGIN_MANAGER global is set exactly once (OnceLock)
    // and every scenario runs against the same shared registry.

    #[tokio::test]
    async fn handler_permission_gate_and_unknown_tool() {
        let mut reg = McpRegistry::new();
        reg.register(fake_tool("ssh__run", 0, "ssh-done"));
        reg.register(fake_tool("docker__compose", 0, "docker-done"));
        let pm: Arc<dyn crate::agent::plugin_manager::PluginManager> =
            Arc::new(crate::agent::plugin_manager::LegacyPluginManager::new(
                Arc::new(tokio::sync::RwLock::new(reg)),
                Arc::new(crate::mcp::external::client::ExternalMcpClients::new()),
                None,
            ));
        let _ = crate::agent::plugin_manager::PLUGIN_MANAGER.set(pm);

        // Agent WITHOUT permission: permission error, tool NOT invoked.
        let ctx = test_ctx(Some(vec!["docker__compose".to_string()]));
        let result = handle_call_and_wait(
            json!({"tool": "ssh__run", "params": {}, "timeout": 30}),
            ctx,
        )
        .await
        .expect("permission denial is a result, not an error");
        assert!(result.is_error);
        assert!(
            result.content.contains("Permission denied"),
            "content: {}",
            result.content
        );
        assert!(result.content.contains("ssh__run"));

        // Agent WITH permission: succeeds.
        let ctx = test_ctx(Some(vec!["ssh__run".to_string()]));
        let result = handle_call_and_wait(
            json!({"tool": "ssh__run", "params": {}, "timeout": 30}),
            ctx,
        )
        .await
        .expect("permitted call should succeed");
        assert!(!result.is_error);
        assert_eq!(result.content, "ssh-done");

        // Unknown tool: clear error, nothing invoked.
        let result =
            handle_call_and_wait(json!({"tool": "nope__nope", "params": {}}), test_ctx(None))
                .await
                .expect("unknown tool is a result, not an error");
        assert!(result.is_error);
        assert!(
            result.content.contains("unknown tool"),
            "content: {}",
            result.content
        );

        // Missing tool param: clear error.
        let result = handle_call_and_wait(json!({"params": {}}), test_ctx(None))
            .await
            .expect("missing tool is a result, not an error");
        assert!(result.is_error);
        assert!(result.content.contains("'tool' parameter is required"));
    }
}

#[cfg(test)]
mod wait_for_status_self_guard_tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// End-to-end (tool handler) proof of the self-wait guard: a call issued
    /// from the thread that is the target task's running thread returns an
    /// error IMMEDIATELY (no polling, no timeout wait); a different caller
    /// keeps the normal path. Skipped when DATABASE_URL is absent.
    #[tokio::test]
    async fn wait_for_status_refuses_a_self_wait_immediately() {
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");

        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let task_id = format!("task_wfs_self_guard_{}_{}", std::process::id(), n);
        let channel = format!("test-channel-wfs-self-guard-{}", std::process::id());
        sqlx::query(
            "INSERT INTO kanban_tasks (id, title, status, board, channel_id, profile, thread_status, created_at, updated_at)
             VALUES ($1, 'wait_for_status self-wait guard test', 'running', 'main', $2, 'test-profile', 'scheduled', NOW(), NOW())",
        )
        .bind(&task_id)
        .bind(&channel)
        .execute(&pool)
        .await
        .expect("insert task");
        let thread_id: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile, task_id, workflow_step)
             VALUES ('processing', 'user', $1, 'test-profile', $2, 'running') RETURNING id",
        )
        .bind(&channel)
        .bind(&task_id)
        .fetch_one(&pool)
        .await
        .expect("insert thread");

        let ctx_for = |tid: i64| {
            let mut ctx = AppContext::new(
                pool.clone(),
                pool.clone(),
                "/tmp",
                std::collections::HashMap::new(),
                Arc::new(crate::mcp::external::client::ExternalMcpClients::new()),
            );
            ctx.current_thread_id = Some(tid);
            ctx
        };

        // (1) The calling thread IS the task's running thread: immediate error.
        let started = Instant::now();
        let res = handle_wait_for_status(
            json!({"task_id": &task_id, "timeout_s": 420, "until": "done,blocked"}),
            ctx_for(thread_id),
        )
        .await
        .expect("handler ok");
        let elapsed = started.elapsed();
        assert!(res.is_error, "must be an error result: {}", res.content);
        assert!(res.content.contains("cannot wait"), "{}", res.content);
        assert!(res.content.contains(&task_id), "{}", res.content);
        assert!(
            res.content.contains(&format!("#{thread_id}")),
            "names the calling thread: {}",
            res.content
        );
        assert!(res.content.contains("wait_task"), "{}", res.content);
        assert!(
            elapsed < Duration::from_secs(2),
            "self-wait must be refused without polling: {elapsed:?}"
        );

        // (2) A different caller is NOT refused: the normal wait runs and
        // times out (short timeout) exactly as before.
        let res2 = handle_wait_for_status(
            json!({"task_id": &task_id, "timeout_s": 1, "until": "done"}),
            ctx_for(thread_id + 5_000_000),
        )
        .await
        .expect("handler ok");
        assert!(!res2.is_error, "{}", res2.content);
        assert!(res2.content.contains("\"timeout\""), "{}", res2.content);

        let _ = sqlx::query("DELETE FROM threads WHERE task_id = $1")
            .bind(&task_id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM kanban_tasks WHERE id = $1")
            .bind(&task_id)
            .execute(&pool)
            .await;
    }
}
