/// Built-in task management tools for non-blocking tool execution.
/// Registered in default_registry() alongside list_tool_details and read_attached_file.
use crate::error::AppResult;
use crate::mcp::{AppContext, McpToolResult};
use crate::agent::task_registry;
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
    let timeout_secs = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(30);
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
        let truncated: String = joined.chars().rev().take(tail).collect::<String>().chars().rev().collect();
        format!("...(showing last {} of {} chars)\n{}", tail, joined.len(), truncated)
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let info = registry.get_info(&task_id).await;
        match info {
            Some(info) => {
                let done = matches!(&info.status, 
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
                    content: serde_json::json!({"status": "not_found", "task_id": task_id}).to_string(),
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
                    content: serde_json::json!({"status": "not_found", "task_id": task_id}).to_string(),
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

    let cancelled = registry.cancel(&task_id).await;
    Ok(McpToolResult {
        call_id: String::new(),
        content: serde_json::json!({
            "status": if cancelled { "cancelled" } else { "not_found" },
            "task_id": task_id,
        }).to_string(),
        is_error: false,
    })
}

pub async fn handle_read_task_logs(args: Value, _ctx: AppContext) -> AppResult<McpToolResult> {
    let task_id = get_task_id(&args).unwrap_or_default();
    let cursor = args.get("cursor").and_then(|v| v.as_u64()).map(|c| c as usize);
    let limit = args.get("limit").and_then(|v| v.as_u64()).map(|l| l as usize);
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
        }).to_string(),
        is_error: false,
    })
}
