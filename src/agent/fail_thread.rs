use crate::agent::config::AgentContext;
use crate::agent::helpers;
use crate::db::types as queries;
use crate::db::types::{CompleteThreadStats, Message, MessageNew, Thread};
use crate::error::AppResult;

/// Create an error message, mark the thread as failed, deliver the error
/// back to the user's platform, and return the saved message.
///
/// Used by all validation-failure paths in process_thread.
pub(crate) async fn fail_thread(
    cfg: &AgentContext,
    thread: &Thread,
    cause_msg: &Message,
    next_seq: &mut i32,
    content: String,
    subtype: &str,
) -> AppResult<Message> {
    let seq = *next_seq;
    *next_seq += 1;

    let err_msg = MessageNew {
        thread_id: thread.id,
        role: "system".to_string(),
        content,
        thread_sequence: seq,
        external_id: Some(format!(
            "validation-error:{}:{}",
            thread.id,
            chrono::Utc::now().timestamp()
        )),
        metadata: serde_json::json!({
            "error_type": "configuration",
        }),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "error".to_string(),
        msg_subtype: Some(subtype.to_string()),
        iteration_number: 0,
        duration_ms: 0,
        token_usage: serde_json::json!({}),
    };

    let saved = queries::create_message(&cfg.pool, &err_msg).await?;

    if let Err(e) = queries::complete_thread(
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
    .await
    {
        tracing::warn!(
            "[executor] Failed to mark thread {} failed ({}): {:?}",
            thread.id,
            subtype,
            e
        );
    }

    // If this thread is linked to a kanban task, mark it as blocked so a
    // validation failure can't leave the task in "running" forever.
    crate::agent::kanban_updater::update_kanban_status(cfg, thread, "failed").await;

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

        // Send failure reaction (:x:) on the cause message
        if let Some(ref platform) = channel.platform {
            if let Some(ref resource) = channel.resource_identifier {
                if let Some(ref ext_id) = cause_msg.external_id {
                    helpers::enqueue_reaction(&cfg.ctx, platform, resource, ext_id, ":x:").await;
                }
            }
        }
    }

    Ok(saved)
}


// ─────────────────────────────────────────────────────────────────────────────


// ─────────────────────────────────────────────────────────────────────────────


// ─────────────────────────────────────────────────────────────────────────────
// Phase 2 — builtin fail-thread tool (spec §8 N1 + §3 F0-F4 matrix)
//
// Ends the current thread as FAILED with an Error-type last message, then
// applies the metadata.workflow_step kanban transition:
//   F0 ""       → executor default: task rests at its current status with
//                 thread_status = NULL (thread re-creation is Phase 3 wiring).
//   F1 running  → guard executions['running'] < retries+1 → increment counter
//                 → task 'running', thread_status NULL. Invalid caller /
//                 absent executor role / limit reached → blocked.
//   F2 testing  → guard executions['testing'] < retries+1 → increment counter
//                 → task 'testing', thread_status NULL. Invalid caller /
//                 absent tester role / limit reached → blocked.
//   F3 blocked  → task 'blocked', thread_status NULL, no thread.
//   F4 (other)  → task 'blocked' + auto comment, no thread (includes 'review'
//                 and role names — N6).
// ─────────────────────────────────────────────────────────────────────────────

/// Normalize the tool's `workflow_step` argument to the F-matrix outcome.
/// STEP keys only: "" | "running" | "testing" | "blocked". Everything else
/// (incl. `review` and role names) is INVALID → F4.
pub(crate) fn normalize_workflow_step(workflow_step: Option<&str>) -> &'static str {
    match workflow_step.unwrap_or("") {
        "" => "executor",
        "running" => "running",
        "testing" => "testing",
        "blocked" => "blocked",
        _ => "invalid",
    }
}

/// Read the execution counter for a step from workflow_state.executions.
fn execution_count(workflow_state: &serde_json::Value, step_key: &str) -> u64 {
    workflow_state
        .get("executions")
        .and_then(|e| e.get(step_key))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

/// Increment the execution counter for a step in workflow_state.executions.
fn increment_execution(workflow_state: &mut serde_json::Value, step_key: &str) {
    if let Some(exec) = workflow_state
        .get_mut("executions")
        .and_then(|e| e.as_object_mut())
    {
        let next = exec.get(step_key).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        exec.insert(step_key.to_string(), serde_json::json!(next));
    } else {
        let mut exec = serde_json::Map::new();
        exec.insert(step_key.to_string(), serde_json::json!(1u64));
        let mut state = serde_json::Map::new();
        state.insert("executions".to_string(), serde_json::Value::Object(exec));
        *workflow_state = serde_json::Value::Object(state);
    }
}

/// Retry limit for a role: per-role override, else workflow default, else 0.
fn retry_limit(wf: &crate::workflows::Workflow, role: &str) -> u64 {
    wf.roles
        .get(role)
        .and_then(|r| r.overrides.retries)
        .or(wf.defaults.retries)
        .unwrap_or(0) as u64
        + 1
}

/// Execute the builtin fail-thread tool for the current thread.
///
/// Mirrors `fail_thread` (Error-type message + FAILED + delivery + reaction)
/// and additionally applies the workflow_step kanban transition (F0-F4).
pub(crate) async fn fail_thread_tool(
    ctx: &crate::mcp::AppContext,
    thread: &crate::db::types::Thread,
    workflow_step: Option<&str>,
    reason: Option<String>,
) -> AppResult<crate::db::types::Message> {
    use crate::db::types::{CompleteThreadStats, MessageNew};

    // 1. Normalize the requested workflow_step (F0-F4).
    let step = normalize_workflow_step(workflow_step);

    // 2. Build + persist the Error-type last message.
    let next_seq = crate::db::threads::get_max_thread_sequence(&ctx.pool, thread.id).await? + 1;
    let content = reason.unwrap_or_else(|| {
        "The thread was ended as FAILED by the fail-thread tool.".to_string()
    });
    let err_msg = MessageNew {
        thread_id: thread.id,
        role: "system".to_string(),
        content,
        thread_sequence: next_seq,
        external_id: Some(format!(
            "fail-thread:{}:{}",
            thread.id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        )),
        metadata: serde_json::json!({
            "error_type": "workflow_failure",
            "workflow_step": step,
            "source": "builtin_fail-thread",
        }),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "error".to_string(),
        msg_subtype: Some("fail_thread".to_string()),
        iteration_number: 0,
        duration_ms: 0,
        token_usage: serde_json::json!({}),
    };
    let saved = crate::db::messages::create_message(&ctx.pool, &err_msg).await?;

    // 3. End the thread as FAILED (terminal; the DB layer only transitions
    //    non-terminal threads, so a second completion is a no-op).
    if let Err(e) = crate::db::threads::complete_thread(
        &ctx.pool,
        thread.id,
        "failed",
        CompleteThreadStats {
            input_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            duration_ms: 0,
        },
    )
    .await
    {
        tracing::warn!(
            "[fail-thread] complete_thread failed for thread {}: {:?}",
            thread.id,
            e
        );
    }

    // 4. Apply the workflow_step kanban transition (F0-F4).
    let transition = apply_fail_step_transition(ctx, thread, step).await;

    // 5. Deliver the error message + failure reaction (mirrors fail_thread).
    if let Ok(Some(channel)) =
        crate::db::channels::get_channel_by_id(&ctx.pool, thread.channel_id).await
    {
        let cause_ext = crate::db::threads::get_cause_message(&ctx.pool, thread.id)
            .await
            .ok()
            .flatten()
            .and_then(|m| m.external_id)
            .unwrap_or_default();
        crate::agent::helpers::enqueue_delivery(
            ctx,
            &saved,
            &channel,
            thread,
            Some(cause_ext.clone()),
        )
        .await;
        if let (Some(ref platform), Some(ref resource)) =
            (channel.platform, channel.resource_identifier)
        {
            let _ = crate::agent::helpers::enqueue_reaction(
                ctx,
                platform,
                resource,
                &cause_ext,
                ":x:",
            )
            .await;
        }
    }

    tracing::info!(
        "fail-thread: thread {} ended as FAILED (workflow_step='{}', transition={})",
        thread.id,
        step,
        transition
    );

    Ok(saved)
}

/// Apply the workflow_step kanban transition for a failing thread (F0-F4).
/// Returns a short description for logs / tool result.
///
/// Uses runtime sqlx (not sql_forge!) for the same reason as db/threads.rs:
/// these queries are not part of the SQLX_OFFLINE query cache.
async fn apply_fail_step_transition(
    ctx: &crate::mcp::AppContext,
    thread: &crate::db::types::Thread,
    step: &str,
) -> String {
    let task_id = match thread.task_id.as_deref() {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => return "no_kanban_task".to_string(),
    };

    // Read the task's workflow columns + the calling thread's workflow_step.
    #[derive(sqlx::FromRow)]
    struct FailStepRow {
        workflow_id: Option<String>,
        thread_status: Option<String>,
        workflow_state: Option<serde_json::Value>,
        caller_step: Option<String>,
    }
    let row: Option<FailStepRow> = match sqlx::query_as::<_, FailStepRow>(
        r#"
        SELECT kt.workflow_id, kt.thread_status, kt.workflow_state, t.workflow_step AS "caller_step"
        FROM kanban_tasks kt
        JOIN threads t ON t.id = $2
        WHERE kt.id = $1
        "#,
    )
    .bind(&task_id)
    .bind(thread.id)
    .fetch_optional(&ctx.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[fail-thread] workflow columns read failed: {:?}", e);
            return "task_read_error".to_string();
        }
    };
    let Some(row) = row else {
        return "task_not_found".to_string();
    };

    let workflow_id = row.workflow_id.clone().unwrap_or_default();
    let caller_step = row.caller_step.clone().unwrap_or_default();
    let mut executions = row.workflow_state.clone().unwrap_or_else(|| {
        serde_json::json!({ "executions": {} })
    });

    // Load the workflow definition (roles / retries) for the F1/F2 guards.
    let workflow = if workflow_id.is_empty() {
        None
    } else {
        let wf_path = format!("{}/workflows.yml", ctx.data_dir);
        crate::workflows::WorkflowsFile::load(std::path::Path::new(&wf_path))
            .ok()
            .and_then(|wf| wf.workflows.get(&workflow_id).cloned())
    };

    // F-matrix outcome computation.
    let mut final_status: Option<&str> = None; // None = leave status unchanged (F0)
    let comment; // assigned in every match arm below

    match step {
        "executor" => {
            // F0 — executor default. With a workflow: task rests at its current
            // status, thread_status = NULL (thread re-creation is Phase 3).
            // Without a workflow: mirror normal failure semantics → blocked.
            if workflow_id.is_empty() {
                final_status = Some("blocked");
                comment =
                    "fail-thread: executor thread ended as FAILED (no workflow → blocked)."
                        .to_string();
            } else {
                comment =
                    "fail-thread: executor step failed — step re-run pending (Phase 3)."
                        .to_string();
            }
        }
        "running" => {
            // F1 — a tester/reviewer thread requests executor rework.
            if caller_step == "testing" || caller_step == "review" {
                match &workflow {
                    Some(wf) if wf.roles.contains_key("executor") => {
                        let limit = retry_limit(wf, "executor");
                        let count = execution_count(&executions, "running");
                        if count < limit {
                            increment_execution(&mut executions, "running");
                            final_status = Some("running");
                            comment = format!(
                                "fail-thread: tester/reviewer failure → executor rework (execution {}/{}).",
                                count + 1,
                                limit
                            );
                        } else {
                            comment = format!(
                                "fail-thread: executor retry limit reached ({}/{} → blocked).",
                                count, limit
                            );
                        }
                    }
                    Some(_) => {
                        comment =
                            "fail-thread: executor role absent from workflow → blocked.".to_string();
                    }
                    None => {
                        comment =
                            "fail-thread: workflow not found / not loadable → blocked.".to_string();
                    }
                }
            } else {
                comment = "fail-thread: invalid caller for step 'running' (caller must be tester/reviewer) → blocked.".to_string();
            }
        }
        "testing" => {
            // F2 — a reviewer thread requests re-test.
            if caller_step == "review" {
                match &workflow {
                    Some(wf) if wf.roles.contains_key("tester") => {
                        let limit = retry_limit(wf, "tester");
                        let count = execution_count(&executions, "testing");
                        if count < limit {
                            increment_execution(&mut executions, "testing");
                            final_status = Some("testing");
                            comment = format!(
                                "fail-thread: reviewer failure → re-test (execution {}/{}).",
                                count + 1,
                                limit
                            );
                        } else {
                            comment = format!(
                                "fail-thread: tester retry limit reached ({}/{} → blocked).",
                                count, limit
                            );
                        }
                    }
                    Some(_) => {
                        comment =
                            "fail-thread: tester role absent from workflow → blocked.".to_string();
                    }
                    None => {
                        comment =
                            "fail-thread: workflow not found / not loadable → blocked.".to_string();
                    }
                }
            } else {
                comment = "fail-thread: invalid caller for step 'testing' (caller must be reviewer) → blocked.".to_string();
            }
        }
        "blocked" => {
            // F3 — any role may block the task.
            final_status = Some("blocked");
            comment = "fail-thread: task blocked via fail-thread.".to_string();
        }
        _ => {
            // F4 — invalid workflow_step (incl. review / role names) → blocked + auto comment.
            final_status = Some("blocked");
            comment = "fail-thread: invalid workflow_step → task blocked.".to_string();
        }
    }

    // Persist the transition atomically (R8): status + thread_status +
    // workflow_state + kanban_history comment in one transaction.
    let mut tx = match ctx.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::warn!("[fail-thread] begin tx failed: {:?}", e);
            return "tx_error".to_string();
        }
    };

    let initial_status: String = match sqlx::query_scalar::<_, String>(
        "SELECT status FROM kanban_tasks WHERE id = $1 FOR UPDATE",
    )
    .bind(&task_id)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("[fail-thread] task status read failed: {:?}", e);
            let _ = tx.rollback().await;
            return "task_read_error".to_string();
        }
    };

    let status_sql = match final_status {
        Some(status) => sqlx::query(
            "UPDATE kanban_tasks SET status = $1, thread_status = NULL, workflow_state = CAST($2 AS jsonb), updated_at = NOW() WHERE id = $3",
        )
        .bind(status)
        .bind(executions.to_string())
        .bind(&task_id)
        .execute(&mut *tx)
        .await,
        None => sqlx::query(
            "UPDATE kanban_tasks SET thread_status = NULL, workflow_state = CAST($1 AS jsonb), updated_at = NOW() WHERE id = $2",
        )
        .bind(executions.to_string())
        .bind(&task_id)
        .execute(&mut *tx)
        .await,
    };
    if let Err(e) = status_sql {
        tracing::warn!("[fail-thread] task update failed: {:?}", e);
        let _ = tx.rollback().await;
        return "update_error".to_string();
    }

    let final_board = final_status.unwrap_or(&initial_status);
    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
        VALUES ($1, 'workflow', $2, $3, $4)
        "#,
    )
    .bind(&task_id)
    .bind(&initial_status)
    .bind(final_board)
    .bind(&comment)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!("[fail-thread] kanban_history insert failed: {:?}", e);
        let _ = tx.rollback().await;
        return "history_error".to_string();
    }

    if let Err(e) = tx.commit().await {
        tracing::warn!("[fail-thread] tx commit failed: {:?}", e);
        return "commit_error".to_string();
    }

    format!(
        "task {} {} (thread_status=NULL, comment='{}')",
        task_id,
        match final_status {
            Some(s) => format!("→ {}", s),
            None => format!("resting at {}", initial_status),
        },
        comment
    )
}
