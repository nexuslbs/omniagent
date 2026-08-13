use crate::agent::config::AgentContext;
use crate::agent::helpers;
use crate::db::threads::create_thread;
use crate::db::types as queries;
use crate::db::types::{CompleteThreadStats, CreateThreadParams, Message, MessageNew, Thread};
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
    if let Ok(Some(channel)) = queries::get_channel_by_id(&cfg.pool, &thread.channel_id).await {
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

// ── Phase 4: manual/API review decisions (spec §8 R12) ──────────────────────

/// Outcome of a manual/API review decision (POST /review, kanban_review_task).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReviewOutcome {
    pub task_id: String,
    pub status: String,
    pub thread_id: Option<i64>,
    pub comment: String,
}

/// Validate a manual review `decision` value (whitelist, spec §8).
pub fn validate_review_decision(decision: &str) -> Result<(), String> {
    match decision {
        "approve" | "rework" | "retest" | "block" => Ok(()),
        _ => Err(format!(
            "invalid decision '{decision}': must be one of 'approve', 'rework', 'retest', 'block'"
        )),
    }
}

/// Pure routing for a manual review decision — no DB access (unit-tested).
///
/// Returns `(final_status, rerun_step, block_reason)`.
/// - R5: `review` is valid without a reviewer role (manual state); `testing`
///   without a tester role is INVALID → `blocked` + auto comment.
/// - D1/R2: rework/retest consume the target step's retry budget; at the
///   limit the decision is a no-op → `blocked` (retry guard).
pub fn route_manual_review(
    decision: &str,
    has_wf: bool,
    has_executor_role: bool,
    has_tester_role: bool,
    under_budget: bool,
) -> (String, Option<String>, Option<String>) {
    match decision {
        "approve" => ("done".to_string(), None, None),
        "block" => ("blocked".to_string(), None, None),
        "rework" => {
            if !has_wf {
                (
                    "blocked".to_string(),
                    None,
                    Some("task has no workflow".to_string()),
                )
            } else if !has_executor_role {
                (
                    "blocked".to_string(),
                    None,
                    Some("no executor role in workflow".to_string()),
                )
            } else if !under_budget {
                (
                    "blocked".to_string(),
                    None,
                    Some("executor retry limit reached".to_string()),
                )
            } else {
                ("running".to_string(), Some("running".to_string()), None)
            }
        }
        "retest" => {
            if !has_wf {
                (
                    "blocked".to_string(),
                    None,
                    Some("task has no workflow".to_string()),
                )
            } else if !has_tester_role {
                (
                    "blocked".to_string(),
                    None,
                    Some("no tester role in workflow".to_string()),
                )
            } else if !under_budget {
                (
                    "blocked".to_string(),
                    None,
                    Some("tester retry limit reached".to_string()),
                )
            } else {
                ("testing".to_string(), Some("testing".to_string()), None)
            }
        }
        _ => ("".to_string(), None, Some("invalid decision".to_string())),
    }
}

/// Row used by [`manual_review_decision`].
#[derive(sqlx::FromRow)]
struct ReviewTaskRow {
    status: String,
    workflow_id: Option<String>,
    workflow_state: Option<String>,
    channel_id: Option<String>,
    profile: Option<String>,
    plan: bool,
}

/// Apply a MANUAL/API review decision to a kanban task. This is the shared
/// implementation behind `POST /review` and the `kanban_review_task` MCP tool
/// (spec §8, R12) — the reviewer AGENT does not call it: it signals approve
/// via normal thread completion and issues via fail-thread with
/// `workflow_step` = running/testing/blocked (N6).
///
/// Decisions:
/// - `approve` → task `done` (manual state — valid without a reviewer role, R5)
/// - `rework`  → task `running` + scheduled executor thread (retry-guarded)
/// - `retest`  → task `testing` + scheduled tester thread (R5: no tester role
///   → `blocked` + auto comment; retry-guarded)
/// - `block`   → task `blocked` (no thread)
///
/// R4: a no-op when the task is already terminal (`done`/`blocked`).
/// Convert any error into a String (for API error surfaces).
fn err_str<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn workflow_role_for_step(step: &str) -> Option<&'static str> {
    match step {
        "running" => Some("executor"),
        "testing" => Some("tester"),
        "review" => Some("reviewer"),
        _ => None,
    }
}

pub async fn manual_review_decision(
    pool: &sqlx::PgPool,
    data_dir: &str,
    task_id: &str,
    decision: &str,
    comment: Option<&str>,
) -> Result<ReviewOutcome, String> {
    validate_review_decision(decision)?;

    let mut tx = pool.begin().await.map_err(err_str)?;

    let task: Option<ReviewTaskRow> = sql_forge!(
        ReviewTaskRow,
        "SELECT status, workflow_id, CAST(workflow_state AS text) AS workflow_state, channel_id, profile, plan
         FROM kanban_tasks WHERE id = :task_id FOR UPDATE",
        ( :task_id = task_id )
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(err_str)?;

    let Some(task) = task else {
        return Err(format!("kanban task {task_id} not found"));
    };

    // R4: terminal tasks are a no-op (same semantics as engine_transition).
    if is_terminal_status(&task.status) {
        let status = task.status.clone();
        return Ok(ReviewOutcome {
            task_id: task_id.to_string(),
            status,
            thread_id: None,
            comment: format!("no-op: task already {} ({decision})", task.status),
        });
    }

    // Workflow config: role presence + retry budgets (workflows.yml).
    let wfs = crate::workflows::WorkflowsFile::load(&crate::config_path::config_path(
        data_dir,
        "workflows.yml",
    ))
    .map_err(err_str)?;
    let wf = task
        .workflow_id
        .as_deref()
        .and_then(|id| wfs.workflows.get(id))
        .cloned()
        .unwrap_or_default();
    let has_wf = task.workflow_id.is_some();
    let has_executor_role = wf.roles.contains_key("executor");
    let has_tester_role = wf.roles.contains_key("tester");

    // workflow_state JSON — executions live under the "executions" key.
    let mut state: serde_json::Value = task
        .workflow_state
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let budget_ok = match decision {
        "rework" => execution_count(&state, "running") < retry_limit(&wf, "executor"),
        "retest" => execution_count(&state, "testing") < retry_limit(&wf, "tester"),
        _ => true,
    };

    let (to_status, rerun_step, block_reason) = route_manual_review(
        decision,
        has_wf,
        has_executor_role,
        has_tester_role,
        budget_ok,
    );
    if let Some(step) = rerun_step.as_deref() {
        increment_execution(&mut state, step);
    }

    // Create the scheduled re-run thread for rework/retest (same pattern as
    // engine_transition: pending thread + seq-0 cause message).
    let new_thread_id: Option<i64> = if let Some(step) = rerun_step.as_deref() {
        let cause_msg = format!("Manual review decision: {decision}. Task: {task_id}");
        let role_cfg = workflow_role_for_step(step).and_then(|role| wf.resolve_role(role));
        let plan = match role_cfg.as_ref().and_then(|r| r.plan_mode.as_deref()) {
            Some("on") => true,
            Some("off") => false,
            _ => task.plan,
        };
        let template = role_cfg.as_ref().and_then(|r| r.template.clone());
        // Single canonical INSERT (create_thread). Note: threads.cause has
        // CHECK chk_thread_cause (cause IN ('user','system')) — the free-text
        // "Manual review decision: ..." text goes in the cause MESSAGE content,
        // never in threads.cause.
        let new_thread = create_thread(
            &mut *tx,
            "pending",
            "system",
            task.channel_id.as_deref().unwrap_or(""),
            task.profile.as_deref().unwrap_or(""),
            CreateThreadParams {
                provider: role_cfg.as_ref().and_then(|r| r.provider.clone()),
                model: role_cfg.as_ref().and_then(|r| r.model.clone()),
                task_id: Some(task_id.to_string()),
                schedule_task_id: None,
                plan,
                parent_id: None,
                workflow_id: task.workflow_id.clone(),
                workflow_step: Some(step.to_string()),
                template,
                hook_caused: false,
            },
        )
        .await
        .map_err(|e| format!("insert manual review thread: {e}"))?;
        let new_id = new_thread.id;

        sql_forge!(
            "INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
             VALUES (:tid, 'cause', :content, 0, 'cause')",
            ( :tid = new_id, :content = cause_msg )
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("insert manual review cause message: {e}"))?;

        Some(new_id)
    } else {
        None
    };

    let auto_comment = if let Some(reason) = block_reason {
        format!(
            "Task blocked: {reason}. Manual review decision: {decision}.{}",
            comment.map(|c| format!(" {c}")).unwrap_or_default()
        )
    } else if let Some(tid) = new_thread_id {
        format!(
            "Manual review decision: {decision}. Creating thread #{tid}.{}",
            comment.map(|c| format!(" {c}")).unwrap_or_default()
        )
    } else {
        format!(
            "Manual review decision: {decision}.{}",
            comment.map(|c| format!(" {c}")).unwrap_or_default()
        )
    };

    let thread_status = new_thread_id.map(|_| "scheduled".to_string());
    sql_forge!(
        "UPDATE kanban_tasks SET status = :p1, thread_status = NULLIF(:thread_status, '')::text,
                workflow_state = CAST(:p3 AS jsonb)
         WHERE id = :task_id",
        (
            :p1 = to_status.as_str(),
            :thread_status = thread_status.as_deref().unwrap_or(""),
            :p3 = state,
            :task_id = task_id,
        )
    )
    .execute(&mut *tx)
    .await
    .map_err(err_str)?;

    sql_forge!(
        "INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
         VALUES (:id, 'workflow', :from, :to, :comment)",
        (:id = task_id, :from = task.status, :to = to_status.clone(), :comment = auto_comment.clone())
    )
    .execute(&mut *tx)
    .await
    .map_err(err_str)?;

    tx.commit().await.map_err(err_str)?;

    Ok(ReviewOutcome {
        task_id: task_id.to_string(),
        status: to_status,
        thread_id: new_thread_id,
        comment: auto_comment,
    })
}

/// R4 terminal kanban statuses: a workflow task in one of these states never
/// spawns a new thread and never transitions again (no-op guards in
/// engine_transition and manual_review_decision).
pub(crate) fn is_terminal_status(status: &str) -> bool {
    matches!(status, "blocked" | "done")
}

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
/// Outcome of the D7-aware retry guard when a step re-entry would exceed the
/// workflow's retry limit.
#[derive(Debug, PartialEq, Eq)]
struct RetryGuard {
    /// Final kanban status for the task: "review" (D7, executor/tester step
    /// with `clear_executions_on_review`) or "blocked" (default / reviewer).
    final_status: &'static str,
    /// Zero the per-step `running`/`testing` execution counters (D7).
    clear: bool,
    /// Create a `review` thread (D7, only when the workflow has a reviewer
    /// role; otherwise the task lands in `review` as a manual state).
    review_thread: bool,
}

/// D7 retry-guard decision. The reviewer step is NEVER overridden and NEVER
/// cleared — that is the boundedness guarantee (max total executions =
/// [(executor + tester + 1) * reviewer]).
fn guard_at_retry_limit(step: &str, clear_on_review: bool, has_reviewer_role: bool) -> RetryGuard {
    if clear_on_review && matches!(step, "running" | "testing") {
        RetryGuard {
            final_status: "review",
            clear: true,
            review_thread: has_reviewer_role,
        }
    } else {
        RetryGuard {
            final_status: "blocked",
            clear: false,
            review_thread: false,
        }
    }
}

/// D7: zero the per-step `running`/`testing` execution counters. The reviewer
/// counter (`review`) is NEVER cleared.
fn clear_running_testing(workflow_state: &mut serde_json::Value) {
    if let Some(exec) = workflow_state
        .get_mut("executions")
        .and_then(|e| e.as_object_mut())
    {
        exec.insert("running".to_string(), serde_json::json!(0u64));
        exec.insert("testing".to_string(), serde_json::json!(0u64));
    }
}

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
        crate::db::channels::get_channel_by_id(&ctx.pool, &thread.channel_id).await
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
        // F1 — tester/reviewer requests executor rework, or the executor
        // itself fails with an explicit 'running' target (R7 row-2: rerun it).
        "running" => {
            let valid_caller = matches!(
                caller_step,
                Some("testing") | Some("review") | Some("running")
            );
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
/// WS-5: retry inheritance — copy the parent thread's notes.md into the
/// child thread's dir so a re-run/review thread starts with everything the
/// interrupted parent learned. Best-effort file copy; returns false when the
/// parent has no notes.
fn copy_thread_notes(data_dir: &str, parent_id: i64, child_id: i64) -> bool {
    let root = std::path::Path::new(data_dir).join("data").join("threads");
    let src = root.join(parent_id.to_string()).join("notes.md");
    let Ok(content) = std::fs::read_to_string(&src) else {
        return false; // parent had no notes — nothing to inherit
    };
    if content.trim().is_empty() {
        return false;
    }
    let dst_dir = root.join(child_id.to_string());
    if std::fs::create_dir_all(&dst_dir).is_err() {
        return false;
    }
    std::fs::write(dst_dir.join("notes.md"), content).is_ok()
}

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
    if is_terminal_status(&task.status) {
        return Ok(None);
    }

    let wf_id = task.workflow_id.as_deref();
    let has_wf = wf_id.is_some();
    let caller_step = task.caller_step.as_deref();
    let mut executions = task.workflow_state.unwrap_or_else(|| serde_json::json!({}));

    // Load the workflow definition (retry limits / roles).
    let workflow = if let Some(id) = wf_id {
        let path = crate::config_path::config_path(data_dir, "workflows.yml");
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
    let has_reviewer_role = workflow
        .as_ref()
        .is_some_and(|w| w.roles.contains_key("reviewer"));
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
                "running"
                    if !matches!(
                        caller_step,
                        Some("testing") | Some("review") | Some("running")
                    ) =>
                {
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
                // R8-N: plain task (no workflow) — a failed thread must land
                // on 'blocked' (visible fail), not stay a zombie 'running'.
                final_status = "blocked".to_string();
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
                if !has_wf {
                    // R8-N: plain task (no workflow) — an interrupted thread
                    // must land on 'blocked' (visible fail), not stay a zombie
                    // 'running'.
                    final_status = "blocked".to_string();
                }
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

    // Retry guard (D1/R2 + D7): limit = retries + 1; a re-entry that would
    // exceed the limit is converted BEFORE any thread is created. With
    // `clear_executions_on_review` (D7) an executor/tester limit sends the
    // task to `review` instead of `blocked` and zeroes the running/testing
    // counters; the reviewer step is ALWAYS blocked (boundedness guarantee).
    let mut review_thread = false;
    if increment {
        if let Some(step) = rerun_step.as_deref() {
            if execution_count(&executions, step) >= limit_for(step) {
                let clear_on_review = workflow
                    .as_ref()
                    .is_some_and(|w| w.clear_executions_on_review);
                let outcome = guard_at_retry_limit(step, clear_on_review, has_reviewer_role);
                rerun_step = None;
                final_status = outcome.final_status.to_string();
                block_reason = "retry limit reached";
                review_thread = outcome.review_thread;
                if outcome.clear {
                    // D7: zero the per-step running/testing counters; the
                    // reviewer counter (`review`) is NEVER cleared.
                    clear_running_testing(&mut executions);
                }
            }
        }
    }

    // Resolve the next step's execution identity before inserting it. The
    // executor must never inherit stale parent settings when a workflow role
    // defines its own identity. Returns (profile, provider, model, plan, template)
    // — mirroring kanban_updater::resolve_step_identity so re-run threads get
    // the role's plan_mode and template (thread 82 ran with plan=false +
    // template=NULL because this closure only resolved profile/provider/model).
    let resolve_step_identity = |step: &str| {
        let role = role_for_step(step);
        let role_cfg = workflow.as_ref().and_then(|wf| wf.resolve_role(role));
        let plan = match role_cfg.as_ref().and_then(|r| r.plan_mode.as_deref()) {
            Some("on") => true,
            Some("off") => false,
            _ => thread.plan,
        };
        let template = role_cfg.as_ref().and_then(|r| r.template.clone());
        (
            role_cfg
                .as_ref()
                .and_then(|r| r.profile.clone())
                .unwrap_or_else(|| thread.profile.clone()),
            role_cfg
                .as_ref()
                .and_then(|r| r.provider.clone())
                .or_else(|| {
                    workflow
                        .as_ref()
                        .and_then(|wf| wf.defaults.provider.clone())
                })
                .or_else(|| thread.provider.clone()),
            role_cfg
                .as_ref()
                .and_then(|r| r.model.clone())
                .or_else(|| workflow.as_ref().and_then(|wf| wf.defaults.model.clone()))
                .or_else(|| thread.model.clone()),
            plan,
            template,
        )
    };

    // ---- Execute (one transaction) ------------------------------------------
    let initial_status = task.status.clone();
    let new_thread_id: Option<i64> = if let Some(step) = rerun_step.as_deref() {
        #[derive(sqlx::FromRow)]
        struct IdRow {
            id: i64,
        }
        let (profile, provider, model, plan, template) = resolve_step_identity(step);
        let provider =
            provider.ok_or_else(|| format!("no provider configured for workflow step {step}"))?;
        let model = model.ok_or_else(|| format!("no model configured for workflow step {step}"))?;
        if profile.trim().is_empty() {
            return Err(format!("no profile configured for workflow step {step}"));
        }
        // Single canonical INSERT (create_thread) — carries plan + template so
        // re-run threads keep the role's iteration budget and guidance.
        let new_thread = create_thread(
            &mut *tx,
            "pending",
            thread.cause.as_str(),
            &thread.channel_id,
            &profile,
            CreateThreadParams {
                provider: Some(provider.clone()),
                model: Some(model.clone()),
                task_id: Some(task_id.to_string()),
                schedule_task_id: None,
                plan,
                parent_id: Some(thread.id),
                workflow_id: wf_id.map(str::to_string),
                workflow_step: Some(step.to_string()),
                template,
                hook_caused: false,
            },
        )
        .await
        .map_err(|e| format!("insert rerun thread: {e}"))?;
        let new_id = new_thread.id;

        // seq-0 cause message for the re-run thread — copy the PARENT's
        // msg_type='kanban' message content (the actual script), NOT
        // threads.cause (which is the CHECK-enum 'system'/'user', not content).
        // Without the script the noop provider echoes and the rerun "completes"
        // vacuously → task routes to review instead of re-failing (GROUP 22 T4/T6/T7).
        #[derive(sqlx::FromRow)]
        struct KanbanMsgRow {
            content: String,
        }
        let script_content = sql_forge!(
            KanbanMsgRow,
            "SELECT content FROM messages WHERE thread_id = :parent AND msg_type = 'kanban'
             ORDER BY id LIMIT 1",
            ( :parent = thread.id )
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("select parent kanban message: {e}"))?
        .map(|r| r.content)
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| {
            if thread.cause.is_empty() {
                "re-run".to_string()
            } else {
                thread.cause.clone()
            }
        });
        sql_forge!(
            "INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
             VALUES (:tid, 'cause', :content, 0, 'kanban')",
            ( :tid = new_id, :content = script_content )
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("insert rerun cause message: {e}"))?;

        // Count the completed run of this step (R8: same transaction).
        if increment {
            increment_execution(&mut executions, step);
        }
        Some(new_id)
    } else if review_thread {
        // D7: retry-limit → review with a reviewer role creates a review
        // thread (same shape as the normal-completion review path, row 7);
        // without a reviewer role the task lands in `review` as a manual
        // state (no thread) — handled by the None arm below.
        #[derive(sqlx::FromRow)]
        struct IdRow {
            id: i64,
        }
        let (profile, provider, model, plan, template) = resolve_step_identity("review");
        let provider = provider
            .ok_or_else(|| "no provider configured for workflow step review".to_string())?;
        let model =
            model.ok_or_else(|| "no model configured for workflow step review".to_string())?;
        if profile.trim().is_empty() {
            return Err("no profile configured for workflow step review".to_string());
        }
        // Single canonical INSERT (create_thread) — carries plan + template.
        let new_thread = create_thread(
            &mut *tx,
            "pending",
            thread.cause.as_str(),
            &thread.channel_id,
            &profile,
            CreateThreadParams {
                provider: Some(provider.clone()),
                model: Some(model.clone()),
                task_id: Some(task_id.to_string()),
                schedule_task_id: None,
                plan,
                parent_id: Some(thread.id),
                workflow_id: wf_id.map(str::to_string),
                workflow_step: Some("review".to_string()),
                template,
                hook_caused: false,
            },
        )
        .await
        .map_err(|e| format!("insert review thread: {e}"))?;
        let new_id = new_thread.id;

        // seq-0 cause message for the review thread (same task context).
        let cause = if thread.cause.is_empty() {
            "retry limit reached; review".to_string()
        } else {
            thread.cause.clone()
        };
        sql_forge!(
            "INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
             VALUES (:tid, 'cause', :content, 0, 'cause')",
            ( :tid = new_id, :content = cause )
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("insert review cause message: {e}"))?;

        // NOTE: the review execution counter is NOT incremented here — the
        // reviewer has not run yet; it increments when a review thread runs.
        Some(new_id)
    } else {
        None
    };

    let comment = match new_thread_id {
        Some(new_id) if review_thread => format!(
            "Task failed in thread #{}. Retry limit reached; creating review thread #{}.",
            thread.id, new_id
        ),
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
            "Task failed in thread #{}. Moving kanban task to \"{}\" status due to {} for status {}",
            thread.id, final_status, block_reason, initial_status
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

    // WS-5: retry inheritance — the re-run/review thread starts with the
    // interrupted parent's durable notes (best-effort file copy).
    if let Some(child_id) = new_thread_id {
        copy_thread_notes(data_dir, thread.id, child_id);
    }

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
        // Executor itself can request its own rework (R7 row-2 semantics).
        let (step, status) = route_fail_tool("running", Some("running"), true, true, true);
        assert_eq!(step, Some("running"));
        assert_eq!(status, "running");
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

#[test]
fn is_terminal_status_blocks_terminal_statuses() {
    assert!(is_terminal_status("blocked"));
    assert!(is_terminal_status("done"));
}

#[test]
fn is_terminal_status_allows_active_and_retired_statuses() {
    // 'ready' is retired: it must never be treated as terminal (and, after
    // Phase 7, no production code defaults a status to it either).
    for s in [
        "backlog", "todo", "running", "testing", "review", "ready", "",
    ] {
        assert!(
            !is_terminal_status(s),
            "status {s:?} must never be terminal"
        );
    }
}

#[cfg(test)]
mod tests_review {
    use super::*;

    #[test]
    fn validate_review_decision_accepts_all_four_decisions() {
        for d in ["approve", "rework", "retest", "block"] {
            assert!(validate_review_decision(d).is_ok(), "'{d}' should be valid");
        }
    }

    #[test]
    fn validate_review_decision_rejects_invalid() {
        for d in ["", "approve2", "reject", "APPROVE", "done"] {
            assert!(
                validate_review_decision(d).is_err(),
                "'{d}' should be invalid"
            );
        }
    }

    #[test]
    fn route_manual_review_approve_and_block() {
        // approve → done (manual state — valid without reviewer, R5)
        let (s, step, reason) = route_manual_review("approve", true, false, false, true);
        assert_eq!(s, "done");
        assert_eq!(step, None);
        assert_eq!(reason, None);
        let (s, _, _) = route_manual_review("approve", false, false, false, true);
        assert_eq!(s, "done");
        // block → blocked, never a thread
        let (s, step, reason) = route_manual_review("block", true, false, false, true);
        assert_eq!(s, "blocked");
        assert_eq!(step, None);
        assert_eq!(reason, None);
    }

    #[test]
    fn route_manual_review_rework() {
        // rework → running + executor thread when the executor role exists
        let (s, step, reason) = route_manual_review("rework", true, true, false, true);
        assert_eq!(s, "running");
        assert_eq!(step.as_deref(), Some("running"));
        assert_eq!(reason, None);
        // no executor role → blocked (R5 analog)
        let (s, step, reason) = route_manual_review("rework", true, false, false, true);
        assert_eq!(s, "blocked");
        assert_eq!(step, None);
        assert_eq!(reason.as_deref(), Some("no executor role in workflow"));
        // no workflow → blocked
        let (s, _, reason) = route_manual_review("rework", false, false, false, true);
        assert_eq!(s, "blocked");
        assert_eq!(reason.as_deref(), Some("task has no workflow"));
        // retry guard (D1/R2): at the limit → blocked
        let (s, _, reason) = route_manual_review("rework", true, true, false, false);
        assert_eq!(s, "blocked");
        assert_eq!(reason.as_deref(), Some("executor retry limit reached"));
    }

    #[test]
    fn route_manual_review_retest_without_tester_blocks() {
        // R5: testing without a tester role is INVALID → blocked + auto comment
        let (s, step, reason) = route_manual_review("retest", true, true, false, true);
        assert_eq!(s, "blocked");
        assert_eq!(step, None);
        assert_eq!(reason.as_deref(), Some("no tester role in workflow"));
        // with tester role + budget → testing + tester thread
        let (s, step, reason) = route_manual_review("retest", true, true, true, true);
        assert_eq!(s, "testing");
        assert_eq!(step.as_deref(), Some("testing"));
        assert_eq!(reason, None);
        // tester budget exhausted → blocked
        let (s, _, reason) = route_manual_review("retest", true, true, true, false);
        assert_eq!(s, "blocked");
        assert_eq!(reason.as_deref(), Some("tester retry limit reached"));
    }

    // ---- Phase 4b (D7): clear_executions_on_review -------------------------
    // The workflows.yml parse round-trip for the field lives in workflows.rs
    // (test `parse_round_trip_clear_executions_on_review`, commit fda5849).
    // These tests cover the retry-guard behavior in engine_transition.

    #[test]
    fn guard_d7_executor_limit_reviews() {
        // (b) executor (running) at limit + flag true → review (NOT blocked),
        // running/testing cleared, review counter untouched.
        let outcome = guard_at_retry_limit("running", true, true);
        assert_eq!(outcome.final_status, "review");
        assert!(outcome.clear);
        assert!(outcome.review_thread);
        let mut executions = serde_json::json!({
            "executions": {"running": 3, "testing": 2, "review": 5}
        });
        clear_running_testing(&mut executions);
        let exec = &executions["executions"];
        assert_eq!(exec["running"], 0);
        assert_eq!(exec["testing"], 0);
        assert_eq!(exec["review"], 5);
    }

    #[test]
    fn guard_d7_tester_limit_reviews() {
        // (c) tester (testing) at limit + flag true → review, counters cleared.
        let outcome = guard_at_retry_limit("testing", true, true);
        assert_eq!(outcome.final_status, "review");
        assert!(outcome.clear);
        assert!(outcome.review_thread);
        let mut executions = serde_json::json!({
            "executions": {"running": 1, "testing": 2, "review": 0}
        });
        clear_running_testing(&mut executions);
        let exec = &executions["executions"];
        assert_eq!(exec["running"], 0);
        assert_eq!(exec["testing"], 0);
        assert_eq!(exec["review"], 0);
    }

    #[test]
    fn guard_d7_flag_false_blocks() {
        // (d) flag false/absent → exactly today's behavior: blocked, and the
        // counters are never cleared.
        for step in ["running", "testing"] {
            let outcome = guard_at_retry_limit(step, false, true);
            assert_eq!(outcome.final_status, "blocked");
            assert!(!outcome.clear);
            assert!(!outcome.review_thread);
        }
        let outcome = guard_at_retry_limit("running", false, false);
        assert_eq!(outcome.final_status, "blocked");
        assert!(!outcome.clear);
        assert!(!outcome.review_thread);
    }

    #[test]
    fn guard_d7_review_step_never_overridden() {
        // (e) reviewer (review) at limit + flag true → STILL blocked, never
        // overridden, and the review counter is never cleared.
        let outcome = guard_at_retry_limit("review", true, true);
        assert_eq!(outcome.final_status, "blocked");
        assert!(!outcome.clear);
        assert!(!outcome.review_thread);
        let mut executions = serde_json::json!({
            "executions": {"running": 0, "testing": 0, "review": 7}
        });
        clear_running_testing(&mut executions);
        assert_eq!(executions["executions"]["review"], 7);
    }

    #[test]
    fn guard_d7_no_reviewer_role_manual_review() {
        // (f) flag true but no reviewer role → the task lands in `review`
        // with NO thread (manual review state): status review, no review
        // thread, counters still cleared.
        let outcome = guard_at_retry_limit("running", true, false);
        assert_eq!(outcome.final_status, "review");
        assert!(outcome.clear);
        assert!(!outcome.review_thread);
        let outcome = guard_at_retry_limit("testing", true, false);
        assert_eq!(outcome.final_status, "review");
        assert!(outcome.clear);
        assert!(!outcome.review_thread);
    }
}

#[cfg(test)]
mod tests_rerun_script {
    #[test]
    fn copy_thread_notes_copies_parent_notes_to_child() {
        let data_dir =
            std::env::temp_dir().join(format!("retry-notes-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        std::fs::create_dir_all(data_dir.join("data/threads/111")).unwrap();
        std::fs::create_dir_all(data_dir.join("data/threads/222")).unwrap();
        std::fs::write(data_dir.join("data/threads/111/notes.md"), "learned fact").unwrap();
        assert!(super::copy_thread_notes(
            data_dir.to_str().unwrap(),
            111,
            222
        ));
        let copied = std::fs::read_to_string(data_dir.join("data/threads/222/notes.md")).unwrap();
        assert_eq!(copied, "learned fact");
        // parent with no notes -> false, and no file is created for the child
        assert!(!super::copy_thread_notes(
            data_dir.to_str().unwrap(),
            999,
            444
        ));
        assert!(!data_dir.join("data/threads/444/notes.md").exists());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    use super::*;

    #[tokio::test]
    #[ignore = "requires a live DATABASE_URL"]
    async fn rerun_thread_copies_kanban_script_message() {
        // Bug B regression (GROUP 22 T4/T6/T7): a rerun thread's seq-0 cause
        // message must copy the PARENT's msg_type='kanban' message content
        // (the actual workflow script). The old code inserted threads.cause
        // (CHECK-enum 'system'/'user', not content) — the noop provider then
        // had no script and the rerun "completed" vacuously, routing the task
        // to review instead of re-failing. This test fails against the old
        // code (content == 'user', no "builtin_fail-thread" marker).
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let Ok(pool) = sqlx::PgPool::connect(&url).await else {
            eprintln!("skipping: cannot connect to {url}");
            return;
        };

        // throwaway data_dir with a minimal workflow (executor/tester/reviewer roles)
        let data_dir =
            std::env::temp_dir().join(format!("rerun-script-test-{}", std::process::id()));
        std::fs::create_dir_all(data_dir.join("config")).unwrap();
        std::fs::write(
            data_dir.join("config").join("workflows.yml"),
            "workflows:\n  test-wf:\n    profile: test\n    provider: noop\n    model: noop\n    plan_mode: manual\n    retries: 0\n    clear_executions_on_review: false\n    roles:\n      executor:\n        template: \"executor system prompt\"\n        provider: noop\n        model: noop\n      tester:\n        template: \"tester system prompt\"\n        provider: noop\n        model: noop\n      reviewer:\n        template: \"reviewer system prompt\"\n        provider: noop\n        model: noop\n",
        )
        .unwrap();

        let task_id = format!("rerun-script-test-{}", std::process::id());
        let script = r#"{"title":"RerunScriptTest","body":"run","tools":[{"name":"builtin_fail-thread","arguments":{"step":"running"}}]}"#;

        // clean leftovers from any previous crashed run, then set up parent rows
        let _ =         sql_forge!(
            "DELETE FROM messages WHERE thread_id IN (SELECT id FROM threads WHERE task_id = :task_id)",
            ( :task_id = &task_id )
        )
        .execute(&pool)
        .await;
        let _ = sql_forge!(
            "DELETE FROM threads WHERE task_id = :task_id",
            ( :task_id = &task_id )
        )
        .execute(&pool)
        .await;
        let _ = sql_forge!(
            "DELETE FROM kanban_history WHERE kanban_task_id = :task_id",
            ( :task_id = &task_id )
        )
        .execute(&pool)
        .await;
        let _ = sql_forge!(
            "DELETE FROM kanban_tasks WHERE id = :task_id",
            ( :task_id = &task_id )
        )
        .execute(&pool)
        .await;

        sql_forge!(
            "INSERT INTO kanban_tasks (id, title, body, status, priority, channel_id, profile, position, template, plan, planning_mode, workflow_id)
             VALUES (:task_id, 'RerunScriptTest', '', 'running', 1, 'kanban', 'test', 0, NULL, false, 'manual', 'test-wf')",
            ( :task_id = &task_id )
        )
        .execute(&pool)
        .await
        .expect("insert kanban task");

        let parent_id: i64 =         sql_forge!(
            scalar i64,
            "INSERT INTO threads (status, cause, channel_id, profile, provider, model, task_id, parent_id, workflow_id, workflow_step, task_type)
             VALUES ('running', 'user', 'kanban', 'test', 'noop', 'noop', :task_id, NULL, 'test-wf', 'running', 'kanban')
             RETURNING id",
            ( :task_id = &task_id )
        )
        .fetch_one(&pool)
        .await
        .expect("insert parent thread");

        sql_forge!(
            "INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
             VALUES (:parent_id, 'user', :script, 0, 'kanban')",
            (
                :parent_id = parent_id,
                :script = script,
            )
        )
        .execute(&pool)
        .await
        .expect("insert parent kanban message");

        let parent = crate::db::types::Thread {
            id: parent_id,
            status: "running".to_string(),
            cause: "user".to_string(),
            channel_id: "kanban".to_string(),
            profile: "test".to_string(),
            provider: Some("noop".to_string()),
            model: Some("noop".to_string()),
            input_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            duration_ms: 0,
            created_at: chrono::Utc::now(),
            started_at: None,
            ended_at: None,
            terminal: false,
            task_id: Some(task_id.clone()),
            schedule_task_id: None,
            plan: false,
            parent_id: None,
            iterations: 0,
            workflow_step: Some("running".to_string()),
            template: None,
        };

        let new_id = engine_transition(
            &pool,
            data_dir.to_str().unwrap(),
            &parent,
            RerunKind::FailTool {
                step: "running".to_string(),
            },
        )
        .await
        .expect("engine_transition should create a rerun thread")
        .expect("rerun thread id");

        let content: String =         sql_forge!(
            scalar String,
            "SELECT content FROM messages WHERE thread_id = :new_id AND msg_type = 'kanban' ORDER BY id LIMIT 1",
            ( :new_id = new_id )
        )
        .fetch_one(&pool)
        .await
        .expect("fetch rerun cause message");

        // old code inserted threads.cause ('user') here -> this assertion fails
        assert!(
            content.contains("builtin_fail-thread"),
            "rerun cause message must copy the parent's kanban script, got: {content}"
        );

        // cleanup (new thread first — it references the parent)
        let _ = sql_forge!(
            "DELETE FROM messages WHERE thread_id IN (:new_id, :parent_id)",
            (
                :new_id = new_id,
                :parent_id = parent_id,
            )
        )
        .execute(&pool)
        .await;
        let _ = sql_forge!(
            "DELETE FROM threads WHERE id IN (:new_id, :parent_id)",
            (
                :new_id = new_id,
                :parent_id = parent_id,
            )
        )
        .execute(&pool)
        .await;
        let _ = sql_forge!(
            "DELETE FROM kanban_history WHERE kanban_task_id = :task_id",
            ( :task_id = &task_id )
        )
        .execute(&pool)
        .await;
        let _ = sql_forge!(
            "DELETE FROM kanban_tasks WHERE id = :task_id",
            ( :task_id = &task_id )
        )
        .execute(&pool)
        .await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}

#[cfg(test)]
mod tests_r8n_no_workflow_blocked {
    use super::*;

    /// Throwaway data_dir; when `wf` is Some, write workflows.yml containing
    /// that workflow (executor role, retries 0 → retry limit 1).
    fn temp_data_dir(tag: &str, wf: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("r8n-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        if let Some(name) = wf {
            std::fs::write(
                dir.join("config").join("workflows.yml"),
                format!(
                    "workflows:\n  {name}:\n    profile: test\n    provider: noop\n    model: noop\n    plan_mode: manual\n    retries: 0\n    clear_executions_on_review: false\n    roles:\n      executor:\n        template: \"executor system prompt\"\n        provider: noop\n        model: noop\n"
                ),
            )
            .unwrap();
        }
        dir
    }

    /// Insert a kanban task (status 'running') + its parent thread; return the
    /// parent thread id. `workflow_id == None` → plain (no-workflow) task.
    async fn setup(
        pool: &sqlx::PgPool,
        task_id: &str,
        workflow_id: Option<&str>,
        thread_step: Option<&str>,
    ) -> i64 {
        let _ =         sql_forge!(
            "DELETE FROM messages WHERE thread_id IN (SELECT id FROM threads WHERE task_id = :task_id)",
            ( :task_id = task_id )
        )
        .execute(pool)
        .await;
        let _ = sql_forge!(
            "DELETE FROM threads WHERE task_id = :task_id",
            ( :task_id = task_id )
        )
        .execute(pool)
        .await;
        let _ = sql_forge!(
            "DELETE FROM kanban_history WHERE kanban_task_id = :task_id",
            ( :task_id = task_id )
        )
        .execute(pool)
        .await;
        let _ = sql_forge!(
            "DELETE FROM kanban_tasks WHERE id = :task_id",
            ( :task_id = task_id )
        )
        .execute(pool)
        .await;

        sql_forge!(
            "INSERT INTO kanban_tasks (id, title, body, status, priority, channel_id, profile, position, template, plan, planning_mode, workflow_id)
             VALUES (:task_id, 'R8N', '', 'running', 1, 'kanban', 'test', 0, NULL, false, 'manual', NULLIF(:workflow_id, '')::text)",
            (
                :task_id = task_id,
                :workflow_id = workflow_id.unwrap_or(""),
            )
        )
        .execute(pool)
        .await
        .expect("insert kanban task");

        sql_forge!(
            scalar i64,
            "INSERT INTO threads (status, cause, channel_id, profile, provider, model, task_id, parent_id, workflow_id, workflow_step, task_type)
             VALUES ('running', 'user', 'kanban', 'test', 'noop', 'noop', :task_id, NULL, NULLIF(:workflow_id, '')::text, NULLIF(:thread_step, '')::text, 'kanban')
             RETURNING id",
            (
                :task_id = task_id,
                :workflow_id = workflow_id.unwrap_or(""),
                :thread_step = thread_step.unwrap_or(""),
            )
        )
        .fetch_one(pool)
        .await
        .expect("insert parent thread")
    }

    fn parent_thread(
        thread_id: i64,
        task_id: String,
        step: Option<String>,
    ) -> crate::db::types::Thread {
        crate::db::types::Thread {
            id: thread_id,
            status: "running".to_string(),
            cause: "user".to_string(),
            channel_id: "kanban".to_string(),
            profile: "test".to_string(),
            provider: Some("noop".to_string()),
            model: Some("noop".to_string()),
            input_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            duration_ms: 0,
            created_at: chrono::Utc::now(),
            started_at: None,
            ended_at: None,
            terminal: false,
            task_id: Some(task_id.clone()),
            schedule_task_id: None,
            plan: false,
            parent_id: None,
            iterations: 0,
            workflow_step: step,
            template: None,
        }
    }

    async fn task_status(pool: &sqlx::PgPool, task_id: &str) -> String {
        sql_forge!(
            scalar String,
            "SELECT status FROM kanban_tasks WHERE id = :task_id",
            ( :task_id = task_id )
        )
        .fetch_one(pool)
        .await
        .expect("fetch task status")
    }

    async fn latest_history_comment(pool: &sqlx::PgPool, task_id: &str) -> String {
        sql_forge!(
            scalar String,
            "SELECT comment FROM kanban_history WHERE kanban_task_id = :task_id ORDER BY id DESC LIMIT 1",
            ( :task_id = task_id )
        )
        .fetch_one(pool)
        .await
        .expect("fetch kanban history comment")
    }

    async fn cleanup(pool: &sqlx::PgPool, task_id: &str, thread_ids: &[i64]) {
        for tid in thread_ids {
            let _ = sql_forge!(
                "DELETE FROM messages WHERE thread_id = :tid",
                ( :tid = tid )
            )
            .execute(pool)
            .await;
        }
        let _ = sql_forge!(
            "DELETE FROM threads WHERE task_id = :task_id",
            ( :task_id = task_id )
        )
        .execute(pool)
        .await;
        let _ = sql_forge!(
            "DELETE FROM kanban_history WHERE kanban_task_id = :task_id",
            ( :task_id = task_id )
        )
        .execute(pool)
        .await;
        let _ = sql_forge!(
            "DELETE FROM kanban_tasks WHERE id = :task_id",
            ( :task_id = task_id )
        )
        .execute(pool)
        .await;
    }

    #[tokio::test]
    #[ignore = "requires a live DATABASE_URL"]
    async fn no_workflow_interrupted_lands_blocked() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let Ok(pool) = sqlx::PgPool::connect(&url).await else {
            eprintln!("skipping: cannot connect to {url}");
            return;
        };
        let data_dir = temp_data_dir("interrupted", None);
        let task_id = format!("r8n-int-{}", std::process::id());
        let parent = parent_thread(
            setup(&pool, &task_id, None, Some("running")).await,
            task_id.clone(),
            Some("running".to_string()),
        );

        let result = engine_transition(
            &pool,
            data_dir.to_str().unwrap(),
            &parent,
            RerunKind::Interrupted,
        )
        .await
        .expect("engine_transition should succeed");

        assert_eq!(
            result, None,
            "a no-workflow interrupted task must NOT create a re-run thread"
        );
        assert_eq!(task_status(&pool, &task_id).await, "blocked");
        let comment = latest_history_comment(&pool, &task_id).await;
        assert!(
            comment.contains("Moving kanban task to \"blocked\" status due to no workflow"),
            "unexpected kanban_history comment: {comment}"
        );

        cleanup(&pool, &task_id, &[parent.id]).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    #[ignore = "requires a live DATABASE_URL"]
    async fn no_workflow_failed_lands_blocked() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let Ok(pool) = sqlx::PgPool::connect(&url).await else {
            eprintln!("skipping: cannot connect to {url}");
            return;
        };
        let data_dir = temp_data_dir("failed", None);
        let task_id = format!("r8n-fail-{}", std::process::id());
        let parent = parent_thread(
            setup(&pool, &task_id, None, Some("running")).await,
            task_id.clone(),
            Some("running".to_string()),
        );

        let result = engine_transition(
            &pool,
            data_dir.to_str().unwrap(),
            &parent,
            RerunKind::Failed,
        )
        .await
        .expect("engine_transition should succeed");

        assert_eq!(
            result, None,
            "a no-workflow failed task must NOT create a re-run thread"
        );
        assert_eq!(task_status(&pool, &task_id).await, "blocked");
        let comment = latest_history_comment(&pool, &task_id).await;
        assert!(
            comment.contains("Moving kanban task to \"blocked\" status due to no workflow"),
            "unexpected kanban_history comment: {comment}"
        );

        cleanup(&pool, &task_id, &[parent.id]).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    #[ignore = "requires a live DATABASE_URL"]
    async fn workflow_interrupted_reruns_same_step() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let Ok(pool) = sqlx::PgPool::connect(&url).await else {
            eprintln!("skipping: cannot connect to {url}");
            return;
        };
        let data_dir = temp_data_dir("wf-int", Some("test-wf"));
        let task_id = format!("r8n-wfint-{}", std::process::id());
        let parent = parent_thread(
            setup(&pool, &task_id, Some("test-wf"), Some("running")).await,
            task_id.clone(),
            Some("running".to_string()),
        );

        let new_id = engine_transition(
            &pool,
            data_dir.to_str().unwrap(),
            &parent,
            RerunKind::Interrupted,
        )
        .await
        .expect("engine_transition should succeed")
        .expect("workflow interrupted must create a re-run thread");

        let step: String = sql_forge!(
            scalar String,
            "SELECT workflow_step FROM threads WHERE id = :new_id",
            ( :new_id = new_id )
        )
        .fetch_one(&pool)
        .await
        .expect("fetch rerun thread step");
        assert_eq!(step, "running", "I1: re-run the SAME step");

        // kanban status unchanged (task stays 'running', not 'blocked')
        assert_eq!(task_status(&pool, &task_id).await, "running");

        cleanup(&pool, &task_id, &[parent.id, new_id]).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    #[ignore = "requires a live DATABASE_URL"]
    async fn workflow_failed_reruns_executor() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let Ok(pool) = sqlx::PgPool::connect(&url).await else {
            eprintln!("skipping: cannot connect to {url}");
            return;
        };
        let data_dir = temp_data_dir("wf-fail", Some("test-wf"));
        let task_id = format!("r8n-wffail-{}", std::process::id());
        let parent = parent_thread(
            setup(&pool, &task_id, Some("test-wf"), Some("running")).await,
            task_id.clone(),
            Some("running".to_string()),
        );

        let new_id = engine_transition(
            &pool,
            data_dir.to_str().unwrap(),
            &parent,
            RerunKind::Failed,
        )
        .await
        .expect("engine_transition should succeed")
        .expect("workflow failed must create a re-run thread");

        let step: String = sql_forge!(
            scalar String,
            "SELECT workflow_step FROM threads WHERE id = :new_id",
            ( :new_id = new_id )
        )
        .fetch_one(&pool)
        .await
        .expect("fetch rerun thread step");
        assert_eq!(
            step, "running",
            "F0: failed executor re-runs the executor step"
        );

        // kanban status reflects the re-run ('running', not 'blocked')
        assert_eq!(task_status(&pool, &task_id).await, "running");

        cleanup(&pool, &task_id, &[parent.id, new_id]).await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
