use crate::agent::config::AgentContext;
use crate::agent::helpers;
use crate::db::types as queries;
use crate::db::types::{CompleteThreadStats, Message, MessageNew, Thread};
use crate::error::AppResult;
use sql_forge::sql_forge;

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
    let content = reason
        .unwrap_or_else(|| "The thread was ended as FAILED by the fail-thread tool.".to_string());
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
            let _ =
                crate::agent::helpers::enqueue_reaction(ctx, platform, resource, &cause_ext, ":x:")
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
} // ===========================================================================
  // Phase 3: atomic engine transitions (R8) + retry guards (D1/R2/R3) + I1 reruns
  // ===========================================================================

/// What kind of transition is being requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RerunKind {
    /// fail-task tool routing (F0-F4) — decided by the tool's metadata `workflow_step`.
    FailTool {
        /// The `workflow_step` metadata value the tool was called with.
        step: String,
    },
    /// Executor thread ended as FAILED — re-run the executor step (transition table row 2 / F0).
    Failed,
    /// Thread interrupted by the LLM-call iteration limit — re-run the SAME step (I1).
    Interrupted,
    /// Thread skipped before starting (channel closure/deletion) — re-schedule the
    /// same step (R3: no retry consumed, kanban status unchanged).
    Skipped,
}

/// Map a workflow step key to the role key used for retry limits (workflows.yml).
fn role_for_step(step: &str) -> &'static str {
    match step {
        "testing" => "tester",
        "review" => "reviewer",
        _ => "executor",
    }
}

/// Pure routing for the fail-task tool matrix (F0-F4) — no I/O.
///
/// Returns `(target_step, target_status)`:
/// - `target_step == None` → blocked, no re-run thread (F3/F4, absent role,
///   invalid caller, non-workflow task).
/// - `Some("running")` → new executor thread, kanban status `running` (F0/F1).
/// - `Some("testing")` → new tester thread, kanban status `testing` (F2).
fn route_fail_tool(
    normalized: &str,
    caller_step: Option<&str>,
    has_wf: bool,
    has_executor_role: bool,
    has_tester_role: bool,
) -> (Option<&'static str>, &'static str) {
    match normalized {
        // F0 — executor default (no metadata workflow_step): re-run the executor step.
        "executor" => {
            if has_wf {
                (Some("running"), "running")
            } else {
                (None, "blocked")
            }
        }
        // F1 — a tester or reviewer thread requests executor rework.
        "running" => {
            let valid_caller = matches!(caller_step, Some("testing") | Some("review"));
            if !has_wf || !valid_caller || !has_executor_role {
                (None, "blocked")
            } else {
                (Some("running"), "running")
            }
        }
        // F2 — a reviewer thread requests a re-test.
        "testing" => {
            let valid_caller = matches!(caller_step, Some("review"));
            if !has_wf || !valid_caller || !has_tester_role {
                (None, "blocked")
            } else {
                (Some("testing"), "testing")
            }
        }
        // F3 — explicit blocked target: blocked, no thread.
        "blocked" => (None, "blocked"),
        // F4 — any invalid value (incl. 'review', N6): blocked, no thread.
        _ => (None, "blocked"),
    }
}

/// Apply one atomic engine transition (R8): in a SINGLE DB transaction, optionally
/// create a re-run thread + its seq-0 cause message, update the kanban task's
/// status / thread_status / workflow_state (executions), and record a
/// kanban-history comment. Retry guards (limit = retries + 1) are enforced
/// INSIDE the transaction, so a guard hit never leaves a dangling thread and
/// the whole transition commits or rolls back atomically.
///
/// Returns `Ok(Some(new_thread_id))` when a re-run thread was created,
/// `Ok(None)` when the transition was a blocked/no-thread outcome, and
/// `Err(msg)` when the transition failed and nothing was committed.
pub(crate) async fn engine_transition(
    pool: &sqlx::PgPool,
    data_dir: &str,
    thread: &crate::db::types::Thread,
    kind: RerunKind,
) -> Result<Option<i64>, String> {
    let Some(task_id) = thread.task_id.as_deref() else {
        return Ok(None); // thread not linked to a kanban task
    };

    let mut tx = pool.begin().await.map_err(|e| format!("begin: {e}"))?;

    #[derive(sqlx::FromRow)]
    struct TaskRow {
        status: String,
        workflow_id: Option<String>,
        workflow_state: Option<serde_json::Value>,
        caller_step: Option<String>,
    }

    let task = sql_forge!(
        TaskRow,
        "SELECT kt.status, kt.workflow_id, kt.workflow_state,
                t.workflow_step AS caller_step
         FROM kanban_tasks kt
         LEFT JOIN threads t ON t.id = :thread_id
         WHERE kt.id = :task_id
         FOR UPDATE OF kt",
        ( :thread_id = thread.id, :task_id = task_id )
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| format!("select task: {e}"))?;

    let Some(task) = task else {
        return Ok(None); // task disappeared — nothing to transition
    };

    // R4: blocked/done tasks never transition.
    if task.status == "blocked" || task.status == "done" {
        return Ok(None);
    }

    let wf_id = task.workflow_id.as_deref();
    let has_wf = wf_id.is_some();
    let caller_step = task.caller_step.as_deref();
    let mut executions = task.workflow_state.unwrap_or_else(|| serde_json::json!({}));

    // Load the workflow definition (retry limits / roles).
    let workflow = if let Some(id) = wf_id {
        let path = std::path::Path::new(data_dir).join("workflows.yml");
        match crate::workflows::WorkflowsFile::load(&path) {
            Ok(file) => file.workflows.get(id).cloned(),
            Err(_) => None,
        }
    } else {
        None
    };
    let has_executor_role = workflow
        .as_ref()
        .is_some_and(|w| w.roles.contains_key("executor"));
    let has_tester_role = workflow
        .as_ref()
        .is_some_and(|w| w.roles.contains_key("tester"));
    let limit_for = |step: &str| -> u64 {
        match &workflow {
            Some(w) => retry_limit(w, role_for_step(step)),
            // No config for this workflow: treat as retries = 0 (limit = 1).
            None => 1,
        }
    };

    // ---- Decide the transition (inside the locked transaction) --------------
    let mut rerun_step: Option<String> = None;
    let mut final_status = task.status.clone();
    let mut increment = true;
    // Reason used in the kanban-history comment when the outcome is "blocked".
    let mut block_reason: &'static str = "blocked";

    match &kind {
        RerunKind::FailTool { step } => {
            let normalized = normalize_workflow_step(Some(step.as_str()));
            let (target, status) = route_fail_tool(
                normalized,
                caller_step,
                has_wf,
                has_executor_role,
                has_tester_role,
            );
            rerun_step = target.map(|s| s.to_string());
            final_status = status.to_string();
            block_reason = match normalized {
                "executor" => "no workflow",
                "running" if !matches!(caller_step, Some("testing") | Some("review")) => {
                    "invalid caller for workflow_step 'running'"
                }
                "running" if !has_executor_role => "no executor role in workflow",
                "testing" if caller_step != Some("review") => {
                    "invalid caller for workflow_step 'testing'"
                }
                "testing" if !has_tester_role => "no tester role in workflow",
                "blocked" => "workflow_step 'blocked'",
                _ => "invalid workflow_step",
            };
        }
        RerunKind::Failed => {
            // Row 2: executor non-success terminal → re-run the executor step (F0).
            if has_wf {
                rerun_step = Some("running".to_string());
                final_status = "running".to_string();
            } else {
                block_reason = "no workflow";
            }
        }
        RerunKind::Interrupted => {
            // I1: re-run the SAME step; kanban status unchanged.
            if has_wf
                && matches!(
                    caller_step,
                    Some("running") | Some("testing") | Some("review")
                )
            {
                rerun_step = caller_step.map(|s| s.to_string());
            } else {
                block_reason = "no workflow";
            }
        }
        RerunKind::Skipped => {
            // R3: channel closure/deletion — re-schedule the same step with NO
            // retry consumed and the kanban status UNCHANGED (workflow or not).
            if matches!(
                caller_step,
                Some("running") | Some("testing") | Some("review")
            ) {
                rerun_step = caller_step.map(|s| s.to_string());
                increment = false;
            } else if has_wf {
                // Workflow executor thread without an explicit step: re-run the
                // executor step, status unchanged.
                rerun_step = Some("running".to_string());
                increment = false;
            } else {
                // Non-workflow kanban-linked thread: re-schedule (status unchanged).
                rerun_step = Some("running".to_string());
                increment = false;
            }
        }
    }

    // Retry guard (D1/R2): limit = retries + 1; a re-entry that would exceed the
    // limit is converted to "blocked" BEFORE any thread is created.
    if increment {
        if let Some(step) = rerun_step.as_deref() {
            if execution_count(&executions, step) >= limit_for(step) {
                rerun_step = None;
                final_status = "blocked".to_string();
                block_reason = "retry limit reached";
            }
        }
    }

    // ---- Execute (one transaction) ------------------------------------------
    let initial_status = task.status.clone();
    let new_thread_id: Option<i64> = if let Some(step) = rerun_step.as_deref() {
        #[derive(sqlx::FromRow)]
        struct IdRow {
            id: i64,
        }
        let new_id = sql_forge!(
            IdRow,
            "INSERT INTO threads (status, cause, channel_id, profile, provider, model,
                                  task_id, parent_id, workflow_id, workflow_step)
             VALUES ('pending', :cause, :channel_id, :profile, :provider, :model,
                     :task_id, :parent_id, :workflow_id, :workflow_step)
             RETURNING id",
            (
                :cause = thread.cause.as_str(),
                :channel_id = thread.channel_id,
                :profile = thread.profile.as_str(),
                :provider = thread.provider.as_deref().unwrap_or(""),
                :model = thread.model.as_deref().unwrap_or(""),
                :task_id = task_id,
                :parent_id = thread.id,
                :workflow_id = wf_id.unwrap_or(""),
                :workflow_step = step
            )
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| format!("insert rerun thread: {e}"))?;

        // seq-0 cause message for the re-run thread (same task context).
        let cause = if thread.cause.is_empty() {
            "re-run".to_string()
        } else {
            thread.cause.clone()
        };
        sql_forge!(
            "INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
             VALUES (:tid, 'cause', :content, 0, 'cause')",
            ( :tid = new_id.id, :content = cause )
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("insert rerun cause message: {e}"))?;

        // Count the completed run of this step (R8: same transaction).
        if increment {
            increment_execution(&mut executions, step);
        }
        Some(new_id.id)
    } else {
        None
    };

    let comment = match new_thread_id {
        Some(new_id) => match &kind {
            RerunKind::Interrupted => format!(
                "Task interrupted due to LLM calls iteration limit reached in thread #{}. Creating thread #{}",
                thread.id, new_id
            ),
            RerunKind::Skipped => format!(
                "Thread #{} skipped before start. Creating thread #{}",
                thread.id, new_id
            ),
            _ => format!("Task failed in thread #{}. Creating thread #{}", thread.id, new_id),
        },
        None => format!(
            "Task failed in thread #{}. Moving kanban task to \"blocked\" status due to {} for status {}",
            thread.id, block_reason, initial_status
        ),
    };

    let thread_status: Option<&str> = if new_thread_id.is_some() {
        Some("scheduled")
    } else {
        None
    };
    sql_forge!(
        "UPDATE kanban_tasks
         SET status = :status, thread_status = NULLIF(:tstatus, '')::text,
             workflow_state = CAST(:state AS jsonb)
         WHERE id = :task_id",
        (
            :status = final_status.as_str(),
            :tstatus = thread_status.unwrap_or(""),
            :state = executions,
            :task_id = task_id
        )
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("update kanban task: {e}"))?;

    sql_forge!(
        "INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
         VALUES (:task_id, 'workflow', :initial, :to_status, :comment)",
        (
            :task_id = task_id,
            :initial = initial_status.as_str(),
            :to_status = final_status.as_str(),
            :comment = comment.as_str()
        )
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("insert kanban history: {e}"))?;

    tx.commit().await.map_err(|e| format!("commit: {e}"))?;
    Ok(new_thread_id)
}

/// Entry point for the fail-task tool (F0-F4). Wraps [`engine_transition`] and
/// returns a short summary string for the tool result / logs.
async fn apply_fail_step_transition(
    ctx: &crate::mcp::AppContext,
    thread: &crate::db::types::Thread,
    step: &str,
) -> String {
    match engine_transition(
        &ctx.pool,
        &ctx.data_dir,
        thread,
        RerunKind::FailTool {
            step: step.to_string(),
        },
    )
    .await
    {
        Ok(Some(new_id)) => format!("created re-run thread #{new_id}"),
        Ok(None) => "no re-run thread (blocked or non-workflow task)".to_string(),
        Err(e) => format!("transition failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_workflow_step_maps_values() {
        assert_eq!(normalize_workflow_step(None), "executor");
        assert_eq!(normalize_workflow_step(Some("")), "executor");
        assert_eq!(normalize_workflow_step(Some("running")), "running");
        assert_eq!(normalize_workflow_step(Some("testing")), "testing");
        assert_eq!(normalize_workflow_step(Some("blocked")), "blocked");
        // 'review' is not a valid fail target (N6) — routed to blocked (F4).
        assert_eq!(normalize_workflow_step(Some("review")), "invalid");
        assert_eq!(normalize_workflow_step(Some("bogus")), "invalid");
    }

    #[test]
    fn route_fail_tool_f0_executor() {
        // F0 with workflow → re-run executor step.
        let (step, status) = route_fail_tool("executor", Some("running"), true, true, true);
        assert_eq!(step, Some("running"));
        assert_eq!(status, "running");
        // F0 without workflow → blocked.
        let (step, status) = route_fail_tool("executor", Some("running"), false, false, false);
        assert_eq!(step, None);
        assert_eq!(status, "blocked");
    }

    #[test]
    fn route_fail_tool_f1_tester_reviewer_rework() {
        // Tester requests executor rework.
        let (step, status) = route_fail_tool("running", Some("testing"), true, true, true);
        assert_eq!(step, Some("running"));
        assert_eq!(status, "running");
        // Reviewer requests executor rework.
        let (step, _) = route_fail_tool("running", Some("review"), true, true, true);
        assert_eq!(step, Some("running"));
        // Executor itself cannot request "running" rework (invalid caller).
        let (step, _) = route_fail_tool("running", Some("running"), true, true, true);
        assert_eq!(step, None);
        // No executor role in workflow → blocked.
        let (step, _) = route_fail_tool("running", Some("testing"), true, false, true);
        assert_eq!(step, None);
        // Non-workflow task → blocked.
        let (step, _) = route_fail_tool("running", Some("testing"), false, false, false);
        assert_eq!(step, None);
    }

    #[test]
    fn route_fail_tool_f2_reviewer_retest() {
        let (step, status) = route_fail_tool("testing", Some("review"), true, true, true);
        assert_eq!(step, Some("testing"));
        assert_eq!(status, "testing");
        // Tester cannot request its own step.
        let (step, _) = route_fail_tool("testing", Some("testing"), true, true, true);
        assert_eq!(step, None);
        // No tester role → blocked.
        let (step, _) = route_fail_tool("testing", Some("review"), true, true, false);
        assert_eq!(step, None);
    }

    #[test]
    fn route_fail_tool_f3_f4_blocked() {
        let (step, status) = route_fail_tool("blocked", Some("review"), true, true, true);
        assert_eq!(step, None);
        assert_eq!(status, "blocked");
        let (step, status) = route_fail_tool("invalid", Some("review"), true, true, true);
        assert_eq!(step, None);
        assert_eq!(status, "blocked");
    }

    #[test]
    fn execution_count_and_increment_roundtrip() {
        let mut state = serde_json::json!({});
        assert_eq!(execution_count(&state, "running"), 0);
        increment_execution(&mut state, "running");
        increment_execution(&mut state, "running");
        increment_execution(&mut state, "testing");
        assert_eq!(execution_count(&state, "running"), 2);
        assert_eq!(execution_count(&state, "testing"), 1);
        assert_eq!(execution_count(&state, "review"), 0);
    }

    #[test]
    fn retry_limit_is_retries_plus_one() {
        let file = crate::workflows::WorkflowsFile::from_yaml(
            "workflows:\n  wf:\n    defaults:\n      profile: executor\n      retries: 0\n    roles:\n      executor: {}\n      tester:\n        template: t\n        retries: 2\n",
        )
        .expect("parse workflows yaml");
        let wf = file.workflows.get("wf").unwrap();
        // retries = 0 → limit = 1; per-role override retries = 2 → limit = 3.
        assert_eq!(retry_limit(wf, "executor"), 1);
        assert_eq!(retry_limit(wf, "tester"), 3);
        assert_eq!(retry_limit(wf, "reviewer"), 1);
    }

    #[test]
    fn role_for_step_maps_keys() {
        assert_eq!(role_for_step("running"), "executor");
        assert_eq!(role_for_step("testing"), "tester");
        assert_eq!(role_for_step("review"), "reviewer");
        assert_eq!(role_for_step("other"), "executor");
    }
}
