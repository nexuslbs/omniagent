use crate::agent::config::AgentContext;
use crate::agent::fail_thread::{engine_transition, RerunKind};
use crate::db::types as queries;
use crate::db::types::{CreateThreadParams, Thread};
use crate::workflows::{WorkflowsFile, MODE_ACTION};
use sql_forge::sql_forge;

/// If the thread is linked to a kanban task, update its status based on
/// the thread's final outcome.
///
/// Phase 3: failed / interrupted / skipped terminals go through the atomic
/// engine transition (re-run with retry guard, or blocked).
///
/// Phase 4 (reviewer/tester decisions - spec §3 rows 7-17):
/// - reviewer success (`review` step, clean completion) → `done` (R12)
/// - reviewer half-finished (completed with a failed final tool result) → `blocked`
/// - tester success (`testing` step, clean completion) → `review`; a reviewer
///   role in the workflow gets a scheduled review thread (row 7), otherwise
///   the task waits for manual review (no thread)
/// - tester failure (`testing` step, any error - D5) → executor step: task
///   `running` + scheduled executor thread (consumes the executor retry
///   budget; the guard blocks the task at the limit - rows 8/9)
///
/// Role-mode + auto_approve extension (workflows.yml):
/// - action-mode roles (executor/tester/reviewer) run actions.yml tools via
///   `create_kanban_step_thread`/`run_action_role_step` instead of the agent
///   loop; their terminal threads route through `route_step_completion`.
/// - `auto_approve` workflows skip review entirely: review-bound outcomes go
///   straight to `done` and `review_on_fail` is forced false.
/// - `review_on_fail` workflows send failed executor/tester steps to review
///   instead of blocked / executor re-run.
pub async fn update_kanban_status(cfg: &AgentContext, thread: &Thread, final_status: &str) {
    let Some(ref task_id) = thread.task_id else {
        return;
    };

    // Phase 3: non-success terminals → atomic engine transition.
    if matches!(final_status, "failed" | "interrupted" | "skipped") {
        // review_on_fail: a hard-failed executor/tester step goes to review
        // instead of blocked / executor re-run (auto_approve forces the flag
        // off via workflow_policy). Interrupted/skipped keep the existing
        // engine_transition retry guard unchanged.
        if final_status == "failed" {
            let step = thread.workflow_step.as_deref().unwrap_or("");
            let wf_id = thread_workflow_id(&cfg.pool, thread.id).await;
            let policy = workflow_policy(&cfg.ctx.data_dir, wf_id.as_deref(), step);
            if policy.review_on_fail && matches!(step, "running" | "testing") {
                route_step_completion(&cfg.pool, &cfg.ctx.data_dir, thread, true).await;
                return;
            }
        }
        let kind = match final_status {
            "interrupted" => RerunKind::Interrupted,
            "skipped" => RerunKind::Skipped,
            _ => RerunKind::Failed,
        };
        match engine_transition(&cfg.pool, &cfg.ctx.data_dir, thread, kind).await {
            Ok(Some(new_id)) => {
                tracing::info!(
                    "[workflow] thread #{} terminal '{}': created re-run thread #{}",
                    thread.id,
                    final_status,
                    new_id
                );
            }
            Ok(None) => {
                tracing::info!(
                    "[workflow] thread #{} terminal '{}': blocked or no transition",
                    thread.id,
                    final_status
                );
            }
            Err(e) => {
                tracing::warn!(
                    "[workflow] thread #{} terminal '{}': engine transition failed ({e}); falling back to blocked",
                    thread.id,
                    final_status
                );
                if let Err(e2) =
                    queries::update_kanban_task_status(&cfg.pool, task_id, "blocked").await
                {
                    tracing::warn!(
                        "[workflow] failed to block kanban task {}: {:?}",
                        task_id,
                        e2
                    );
                }
            }
        }
        return;
    }

    if final_status != "completed" {
        return;
    }

    // Phase 4: completed agent threads → route through the same matrix used
    // by terminal action-mode threads.
    let errored = last_tool_result_errored(&cfg.pool, thread.id).await;
    route_step_completion(&cfg.pool, &cfg.ctx.data_dir, thread, errored).await;
}

/// Workflow-level routing policy (workflows.yml), resolved for a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct WorkflowPolicy {
    /// `auto_approve: true` → reviewer role ignored; review-bound outcomes go
    /// straight to `done`; `review_on_fail` forced false.
    pub auto_approve: bool,
    /// `review_on_fail: true` → failed executor/tester steps go to review
    /// instead of blocked / executor re-run. Forced false under auto_approve.
    pub review_on_fail: bool,
    /// The CURRENT step's role runs in `mode: action`.
    pub role_is_action: bool,
}

/// Resolve the routing policy for `step` from the task's workflow.
pub(crate) fn workflow_policy(data_dir: &str, wf_id: Option<&str>, step: &str) -> WorkflowPolicy {
    let mut policy = WorkflowPolicy::default();
    let Some(wf_id) = wf_id else {
        return policy;
    };
    let Ok(wfs) = WorkflowsFile::load(&crate::config_path::config_path(data_dir, "workflows.yml"))
    else {
        return policy;
    };
    if let Some(wf) = wfs.workflows.get(wf_id) {
        policy.auto_approve = wf.auto_approve;
        policy.review_on_fail = wf.review_on_fail && !wf.auto_approve;
        if let Some(role) = crate::workflows::role_for_step(step) {
            policy.role_is_action = wf.role_is_action(role);
        }
    }
    policy
}

/// Route a completed (or terminal action-mode) workflow step thread to the
/// next kanban column. Shared by:
/// - `update_kanban_status` (agent-loop finalization),
/// - the action-mode hook in `create_kanban_step_thread` (terminal action
///   threads route synchronously - nobody else finalizes them),
/// - the action-mode step-thread creation in `create_review_thread` /
///   `create_testing_thread` (a terminal action thread must be routed).
pub(crate) async fn route_step_completion(
    pool: &sqlx::PgPool,
    data_dir: &str,
    thread: &Thread,
    errored: bool,
) {
    let Some(ref task_id) = thread.task_id else {
        return;
    };

    let step = thread.workflow_step.as_deref().unwrap_or("");
    let wf_id = thread_workflow_id(pool, thread.id).await;
    let has_reviewer = workflow_has_role(data_dir, &wf_id, "reviewer");
    let has_tester = workflow_has_role(data_dir, &wf_id, "tester");
    let policy = workflow_policy(data_dir, wf_id.as_deref(), step);
    match route_completed_thread(step, errored, has_reviewer, has_tester, policy) {
        // R12: reviewer approves via normal completion + summary → done.
        CompletedRoute::Done => {
            let comment = format!("Reviewer approved (thread #{}). Task done.", thread.id);
            match transition_with_comment(pool, task_id, "done", None, &comment).await {
                Ok(()) => tracing::info!(
                    "[workflow] reviewer approved (thread #{}) → task {} done",
                    thread.id,
                    task_id
                ),
                Err(e) => tracing::warn!("[workflow] failed to mark task {} done: {}", task_id, e),
            }
        }
        // Inconclusive review - the final tool result errored, so there is no
        // clean approve signal (R12); block for manual intervention.
        CompletedRoute::BlockedInconclusiveReview => {
            let comment = format!(
                "Task blocked: review thread #{} completed with a failed tool result (inconclusive). Manual review required.",
                thread.id
            );
            match transition_with_comment(pool, task_id, "blocked", None, &comment).await {
                Ok(()) => tracing::info!(
                    "[workflow] inconclusive review (thread #{}) → task {} blocked",
                    thread.id,
                    task_id
                ),
                Err(e) => tracing::warn!("[workflow] failed to block task {}: {}", task_id, e),
            }
        }
        // Row 7: tester pass → review step, with a scheduled review thread.
        CompletedRoute::ReviewWithThread => {
            match create_review_thread(pool, data_dir, thread).await {
                Ok(StepThreadOutcome::Pending { thread_id }) => {
                    let comment = format!(
                        "Tester passed (thread #{}). Task in review - review thread #{thread_id}.",
                        thread.id
                    );
                    match transition_with_comment(
                        pool,
                        task_id,
                        "review",
                        Some("scheduled"),
                        &comment,
                    )
                    .await
                    {
                        Ok(()) => tracing::info!(
                            "[workflow] tester passed (thread #{}) → task {} in review (review thread #{})",
                            thread.id,
                            task_id,
                            thread_id
                        ),
                        Err(e) => tracing::warn!(
                            "[workflow] failed to move task {} to review: {}",
                            task_id,
                            e
                        ),
                    }
                }
                Ok(StepThreadOutcome::Action { thread_id, errored }) => {
                    // The reviewer role runs in action mode: the action already
                    // executed synchronously. Record the task in review, then
                    // route the terminal action outcome through the matrix
                    // (reviewer action success → done, failure → blocked).
                    let comment = format!(
                        "Tester passed (thread #{}). Task in review - review action executed (thread #{thread_id}).",
                        thread.id
                    );
                    match transition_with_comment(
                        pool,
                        task_id,
                        "review",
                        Some("scheduled"),
                        &comment,
                    )
                    .await
                    {
                        Ok(()) => tracing::info!(
                            "[workflow] tester passed (thread #{}) → task {} in review (review action thread #{})",
                            thread.id,
                            task_id,
                            thread_id
                        ),
                        Err(e) => tracing::warn!(
                            "[workflow] failed to move task {} to review: {}",
                            task_id,
                            e
                        ),
                    }
                    if let Ok(Some(action_thread)) =
                        crate::db::threads::get_thread_by_id(pool, thread_id).await
                    {
                        Box::pin(route_step_completion(
                            pool,
                            data_dir,
                            &action_thread,
                            errored,
                        ))
                        .await;
                    }
                }
                Ok(StepThreadOutcome::None) => {
                    let comment = format!(
                        "Tester passed (thread #{}). Task in review - review thread creation failed, manual review required.",
                        thread.id
                    );
                    match transition_with_comment(pool, task_id, "review", None, &comment).await {
                        Ok(()) => tracing::warn!(
                            "[workflow] tester passed (thread #{}) but review thread creation failed; task {} in review (manual)",
                            thread.id,
                            task_id
                        ),
                        Err(e) => tracing::warn!(
                            "[workflow] failed to move task {} to review: {}",
                            task_id,
                            e
                        ),
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "[workflow] tester passed (thread #{}) but review thread creation FAILED: {}; task {} in review (manual)",
                        thread.id, e, task_id
                    );
                    let comment = format!(
                        "Tester passed (thread #{}). Task in review - review thread creation failed: {}. Manual review required.",
                        thread.id, e
                    );
                    match transition_with_comment(pool, task_id, "review", None, &comment).await {
                        Ok(()) => tracing::warn!(
                            "[workflow] tester passed (thread #{}) but review thread creation failed; task {} in review (manual)",
                            thread.id,
                            task_id
                        ),
                        Err(e2) => tracing::warn!(
                            "[workflow] failed to move task {} to review: {}",
                            task_id,
                            e2
                        ),
                    }
                }
            }
        }
        // R7-D4: executor success with a tester role → testing step thread, task → 'testing'.
        CompletedRoute::TestingWithThread => {
            match create_testing_thread(pool, data_dir, thread).await {
                Ok(StepThreadOutcome::Pending { thread_id }) => {
                    let comment = format!(
                        "Executor done (thread #{}). Task in testing - tester step thread #{thread_id}.",
                        thread.id
                    );
                    match transition_with_comment(
                        pool,
                        task_id,
                        "testing",
                        Some("scheduled"),
                        &comment,
                    )
                    .await
                    {
                        Ok(()) => tracing::info!(
                            "[workflow] executor done (thread #{}) -> task {} in testing (tester thread #{})",
                            thread.id,
                            task_id,
                            thread_id
                        ),
                        Err(e) => tracing::warn!(
                            "[workflow] failed to move task {} to testing: {}",
                            task_id,
                            e
                        ),
                    }
                }
                Ok(StepThreadOutcome::Action { thread_id, errored }) => {
                    // The tester role runs in action mode: the action already
                    // executed synchronously. Record the task in testing, then
                    // route the terminal action outcome through the matrix
                    // (tester action success → review; failure → review).
                    let comment = format!(
                        "Executor done (thread #{}). Task in testing - testing action executed (thread #{thread_id}).",
                        thread.id
                    );
                    match transition_with_comment(
                        pool,
                        task_id,
                        "testing",
                        Some("scheduled"),
                        &comment,
                    )
                    .await
                    {
                        Ok(()) => tracing::info!(
                            "[workflow] executor done (thread #{}) -> task {} in testing (tester action thread #{})",
                            thread.id,
                            task_id,
                            thread_id
                        ),
                        Err(e) => tracing::warn!(
                            "[workflow] failed to move task {} to testing: {}",
                            task_id,
                            e
                        ),
                    }
                    if let Ok(Some(action_thread)) =
                        crate::db::threads::get_thread_by_id(pool, thread_id).await
                    {
                        Box::pin(route_step_completion(
                            pool,
                            data_dir,
                            &action_thread,
                            errored,
                        ))
                        .await;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "[workflow] executor done (thread #{}) but tester thread creation FAILED: {}; task {} in review (manual)",
                        thread.id, e, task_id
                    );
                    let comment = format!(
                        "Executor done (thread #{}). Task in review - tester thread creation failed: {}. Manual review required.",
                        thread.id, e
                    );
                    match transition_with_comment(pool, task_id, "review", None, &comment).await {
                        Ok(()) => {}
                        Err(e2) => tracing::warn!(
                            "[workflow] failed to move task {} to review: {}",
                            task_id,
                            e2
                        ),
                    }
                }
                Ok(StepThreadOutcome::None) => {
                    let comment = format!(
                        "Executor done (thread #{}). Task in review - tester step skipped (no task_id). Manual review required.",
                        thread.id
                    );
                    match transition_with_comment(pool, task_id, "review", None, &comment).await {
                        Ok(()) => tracing::warn!(
                            "[workflow] executor done (thread #{}) but tester thread creation failed; task {} in review (manual)",
                            thread.id,
                            task_id
                        ),
                        Err(e) => tracing::warn!(
                            "[workflow] failed to move task {} to review: {}",
                            task_id,
                            e
                        ),
                    }
                }
            }
        }
        // Row 7: tester pass, no reviewer role → manual review (no thread).
        CompletedRoute::ReviewManual => {
            let comment = format!(
                "Tester passed (thread #{}). Task in review (manual review - no reviewer role).",
                thread.id
            );
            match transition_with_comment(pool, task_id, "review", None, &comment).await {
                Ok(()) => tracing::info!(
                    "[workflow] tester passed (thread #{}) → task {} in review (manual)",
                    thread.id,
                    task_id
                ),
                Err(e) => tracing::warn!(
                    "[workflow] failed to move task {} to review: {}",
                    task_id,
                    e
                ),
            }
        }
        // D5: any test error → executor step (running + scheduled executor thread).
        // Note: action-mode tester errors and review_on_fail route to review
        // (handled by route_completed_thread); this arm is the agent-mode
        // default D5 executor re-run.
        CompletedRoute::TesterErrorToExecutor => {
            match engine_transition(pool, data_dir, thread, RerunKind::Failed).await {
                Ok(Some(new_id)) => tracing::info!(
                    "[workflow] tester error (thread #{}) → executor re-run thread #{}",
                    thread.id,
                    new_id
                ),
                Ok(None) => {
                    tracing::info!("[workflow] tester error (thread #{}) → blocked", thread.id)
                }
                Err(e) => {
                    tracing::warn!(
                        "[workflow] tester error transition failed ({e}); falling back to blocked"
                    );
                    if let Err(e2) =
                        queries::update_kanban_task_status(pool, task_id, "blocked").await
                    {
                        tracing::warn!(
                            "[workflow] failed to block kanban task {}: {:?}",
                            task_id,
                            e2
                        );
                    }
                }
            }
        }
        // Half-finished executor run - cannot promote to review.
        CompletedRoute::BlockedHalfFinished => {
            if let Err(e) = queries::update_kanban_task_status(pool, task_id, "blocked").await {
                tracing::warn!(
                    "[executor] Failed to update kanban task {} status: {:?}",
                    task_id,
                    e
                );
            }
        }
    }
}

/// Phase 4 decision for a completed thread (pure - unit-tested).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletedRoute {
    /// Reviewer approved via clean normal completion (R12) → task done.
    Done,
    /// Review thread finished but the final tool result errored → blocked.
    BlockedInconclusiveReview,
    /// Tester passed + reviewer role → review step with a scheduled review thread (row 7).
    ReviewWithThread,
    /// Tester passed, no reviewer role → manual review, no thread (row 7).
    ReviewManual,
    /// Tester error (D5) → executor step (running + scheduled executor thread).
    TesterErrorToExecutor,
    /// Executor half-finished → blocked.
    BlockedHalfFinished,
    /// Executor success with a tester role -> spawn the testing step thread; task -> 'testing' (R7-D4).
    TestingWithThread,
}

/// Map a completed thread's (workflow step, errored flag, reviewer/tester
/// roles, workflow policy) to the kanban decision (spec §3 rows 7-17, R12,
/// D5 + role-mode/auto_approve/review_on_fail extension).
pub(crate) fn route_completed_thread(
    step: &str,
    errored: bool,
    has_reviewer: bool,
    has_tester: bool,
    policy: WorkflowPolicy,
) -> CompletedRoute {
    match (step, errored) {
        // Reviewer step: success → done (R12); failure → blocked (inconclusive),
        // in agent AND action modes.
        ("review", false) => CompletedRoute::Done,
        ("review", true) => CompletedRoute::BlockedInconclusiveReview,
        // Tester pass → review step; auto_approve → done directly (reviewer
        // role ignored).
        ("testing", false) if policy.auto_approve => CompletedRoute::Done,
        ("testing", false) if has_reviewer => CompletedRoute::ReviewWithThread,
        ("testing", false) => CompletedRoute::ReviewManual,
        // Tester failure: action-mode → review (user rule: NOT executor
        // re-run); agent-mode review_on_fail → review; else D5 executor re-run.
        ("testing", true) if policy.role_is_action || policy.review_on_fail => {
            if has_reviewer {
                CompletedRoute::ReviewWithThread
            } else {
                CompletedRoute::ReviewManual
            }
        }
        ("testing", true) => CompletedRoute::TesterErrorToExecutor,
        // Executor pass → testing step when a tester exists; auto_approve
        // with no tester → done directly; else manual review.
        ("running", false) if has_tester => CompletedRoute::TestingWithThread,
        ("running", false) if policy.auto_approve => CompletedRoute::Done,
        ("running", false) => CompletedRoute::ReviewManual,
        // Executor failure: review_on_fail → review; else blocked (agent
        // half-finished AND action-mode executor fail → blocked).
        ("running", true) if policy.review_on_fail => {
            if has_reviewer {
                CompletedRoute::ReviewWithThread
            } else {
                CompletedRoute::ReviewManual
            }
        }
        (_, true) => CompletedRoute::BlockedHalfFinished,
        _ => CompletedRoute::ReviewManual,
    }
}

/// Move the task to `to` and record a workflow history entry with `comment`
/// (D3: transitions persist a comment).
pub(crate) async fn transition_with_comment(
    pool: &sqlx::PgPool,
    task_id: &str,
    to: &str,
    thread_status: Option<&str>,
    comment: &str,
) -> Result<(), String> {
    let from: Option<String> = sql_forge!(
        scalar String,
        "SELECT status FROM kanban_tasks WHERE id = :task_id",
        ( :task_id = task_id )
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("fetch task status: {e}"))?;
    let from = from.unwrap_or_else(|| to.to_string());

    sql_forge!(
        "UPDATE kanban_tasks SET status = :to, thread_status = NULLIF(:thread_status, '')::text WHERE id = :task_id",
        (
            :to = to,
            :thread_status = thread_status.unwrap_or(""),
            :task_id = task_id,
        )
    )
    .execute(pool)
    .await
    .map_err(|e| format!("transition task: {e}"))?;

    sql_forge!(
        "INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
         VALUES (:id, 'workflow', :from, :to, :comment)",
        (:id = task_id, :from = from, :to = to, :comment = comment)
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(())
}

/// Outcome of creating a workflow step thread (testing/review).
enum StepThreadOutcome {
    /// A pending agent thread was created - the agent loop runs the step.
    Pending { thread_id: i64 },
    /// The step role runs in action mode: the action already executed
    /// synchronously and the thread is terminal (system on success / failed
    /// on error). The caller must route its outcome via `route_step_completion`.
    Action { thread_id: i64, errored: bool },
    /// No thread was created (step skipped / task_id missing / failure).
    None,
}

/// Resolve the execution identity for a newly-created workflow step.
/// Returns (profile, provider, model, plan, template).
async fn resolve_step_identity(
    pool: &sqlx::PgPool,
    data_dir: &str,
    thread: &Thread,
    step: &str,
) -> Result<(String, String, String, bool, Option<String>), String> {
    let workflow_id = thread_workflow_id(pool, thread.id)
        .await
        .unwrap_or_default();
    let role = match step {
        "running" => "executor",
        "testing" => "tester",
        "review" => "reviewer",
        _ => "",
    };
    let role_cfg = if workflow_id.is_empty() || role.is_empty() {
        None
    } else {
        let path = crate::config_path::config_path(data_dir, "workflows.yml");
        crate::workflows::WorkflowsFile::load(&path)
            .ok()
            .and_then(|f| {
                f.workflows
                    .get(&workflow_id)
                    .and_then(|w| w.resolve_role(role))
            })
    };
    let profile = role_cfg
        .as_ref()
        .and_then(|r| r.profile.clone())
        .unwrap_or_else(|| thread.profile.clone());
    let provider = role_cfg
        .as_ref()
        .and_then(|r| r.provider.clone())
        .or_else(|| thread.provider.clone())
        .ok_or_else(|| format!("workflow step {step} has no provider"))?;
    let model = role_cfg
        .as_ref()
        .and_then(|r| r.model.clone())
        .or_else(|| thread.model.clone())
        .ok_or_else(|| format!("workflow step {step} has no model"))?;
    // Plan budget for step threads: role plan_mode ('on'/'off') wins; fall
    // back to the parent thread's plan so tester/reviewer threads created
    // without an explicit column keep a sane default. Without this, step
    // threads always ran with plan=false -> max_iterations_no_plan (60),
    // while the executor got max_iterations_plan (300) - testers writing
    // + running integration tests blew the 60-iteration budget repeatedly
    // (threads 75/76 interrupted at exactly 60).
    let plan = match role_cfg.as_ref().and_then(|r| r.plan_mode.as_deref()) {
        Some("on") => true,
        Some("off") => false,
        _ => thread.plan,
    };
    // Template for step threads: the role template (e.g. dev-tester /
    // dev-reviewer) must be applied so the thread actually loads the
    // role guidance (timeout rules, verification requirements). Without
    // this, tester/reviewer threads ran with template=NULL and never saw
    // the template instructions (observed: thread 77 called wait-task
    // with no timeout_secs and burned 15 iterations polling a build).
    let template = role_cfg.as_ref().and_then(|r| r.template.clone());
    Ok((profile, provider, model, plan, template))
}

/// Build the task description (title + body) for a step-thread cause message.
/// Mirrors `dispatch_content` in server/kanban.rs: the executor thread's cause
/// carries the full task text; step threads must carry the same so the prompt
/// builder can place the task description as the SYSTEM prompt for
/// tester/reviewer threads (inverse role mapping).
async fn task_description(pool: &sqlx::PgPool, task_id: &str) -> String {
    #[derive(sqlx::FromRow)]
    struct TaskTextRow {
        title: String,
        body: Option<String>,
    }
    let row = sql_forge!(
        TaskTextRow,
        "SELECT title, body FROM kanban_tasks WHERE id = :task_id",
        ( :task_id = task_id )
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    match row {
        Some(r) => match r
            .body
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
        {
            Some(body) => format!("{}\n\n{}", r.title.trim(), body),
            None => r.title.trim().to_string(),
        },
        None => String::new(),
    }
}

/// Create a scheduled `review` thread for a passed tester thread (row 7).
/// When the reviewer role is `mode: action`, the action runs synchronously
/// instead and the outcome is returned as `StepThreadOutcome::Action`.
/// Mirrors the thread creation in `engine_transition` (cause message seq-0).
async fn create_review_thread(
    pool: &sqlx::PgPool,
    data_dir: &str,
    thread: &Thread,
) -> Result<StepThreadOutcome, String> {
    let task_id = thread.task_id.clone().unwrap_or_default();
    // Board gate (feature-flagged): never create a review thread for an
    // invalid-board task (board NULL or not in boards.yml).
    if !task_id.is_empty() {
        if let Err(board_err) =
            crate::db::threads::ensure_task_board_valid(pool, data_dir, &task_id).await
        {
            tracing::warn!(
                "[workflow] not creating review thread for task {}: {}",
                task_id,
                board_err
            );
            return Ok(StepThreadOutcome::None);
        }
    }
    // Action-mode reviewer: run the action instead of a pending agent thread.
    if let Some(outcome) = run_action_role_step(pool, data_dir, thread, "review", "reviewer").await
    {
        return Ok(outcome);
    }
    // Step threads carry the TASK DESCRIPTION in the cause message so the
    // prompt builder can place it as the SYSTEM prompt for reviewer threads
    // (inverse role mapping: role template -> USER, task description ->
    // SYSTEM). A bare cause left the reviewer without the task description.
    let task_desc = if task_id.is_empty() {
        String::new()
    } else {
        task_description(pool, &task_id).await
    };
    let cause_msg = if task_desc.is_empty() {
        thread.cause.clone()
    } else {
        format!("Workflow step: review. Task: {task_id}\n\n{task_desc}")
    };
    let wf_id = thread_workflow_id(pool, thread.id)
        .await
        .unwrap_or_default();
    let identity = resolve_step_identity(pool, data_dir, thread, "review").await?;
    // Single canonical INSERT (create_thread) - carries plan + template so
    // the reviewer keeps the role's iteration budget and guidance.
    let new_thread = crate::db::threads::create_thread(
        pool,
        "pending",
        &thread.cause,
        &thread.channel_id,
        &identity.0,
        CreateThreadParams {
            provider: Some(identity.1),
            model: Some(identity.2),
            task_id: Some(task_id.clone()),
            schedule_task_id: None,
            plan: identity.3,
            parent_id: Some(thread.id),
            workflow_id: Some(wf_id),
            workflow_step: Some("review".to_string()),
            template: identity.4,
            hook_caused: false,
        },
    )
    .await
    .map_err(|e| format!("create review thread: {e}"))?;
    let new_id = new_thread.id;

    let msg_id: i64 = sql_forge!(
        scalar i64,
        "INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
         VALUES (:new_id, 'cause', :cause_msg, 0, 'cause')
         RETURNING id",
        (
            :new_id = new_id,
            :cause_msg = cause_msg,
        )
    )
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;
    // Event-driven hooks: the seq-0 cause message is a real new message in a
    // non-hook thread - fire new_message exactly once so hook counters see
    // every message (GROUP 27 CI invariant: SQL ground truth == fired events).
    crate::hooks::fire_new_message(new_id, msg_id);

    Ok(StepThreadOutcome::Pending { thread_id: new_id })
}

/// R7-D4: cause used for the testing step thread INSERT. `threads.cause` has
/// CHECK chk_thread_cause (cause IN ('user','system')); kanban executor threads
/// carry cause='system', so reusing the parent's cause keeps the INSERT valid.
/// (Pre-fix code used a free-text description that always violated the CHECK
/// and silently broke the testing step-thread chain.)
fn testing_step_cause(parent_cause: &str) -> String {
    parent_cause.to_string()
}

/// Create a scheduled `testing` step thread for a completed executor thread (R7-D4,
/// spec §5). When the tester role is `mode: action`, the action runs
/// synchronously instead and the outcome is returned as
/// `StepThreadOutcome::Action`. Mirrors `create_review_thread` with
/// workflow_step='testing' and task_type='kanban' so the workflow test harness
/// can find it.
async fn create_testing_thread(
    pool: &sqlx::PgPool,
    data_dir: &str,
    thread: &Thread,
) -> Result<StepThreadOutcome, String> {
    let task_id = match thread.task_id.clone() {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(StepThreadOutcome::None),
    };
    // Board gate (feature-flagged): never create a testing thread for an
    // invalid-board task (board NULL or not in boards.yml).
    if let Err(board_err) =
        crate::db::threads::ensure_task_board_valid(pool, data_dir, &task_id).await
    {
        tracing::warn!(
            "[workflow] not creating testing thread for task {}: {}",
            task_id,
            board_err
        );
        return Ok(StepThreadOutcome::None);
    }
    // Action-mode tester: run the action instead of a pending agent thread.
    if let Some(outcome) = run_action_role_step(pool, data_dir, thread, "testing", "tester").await {
        return Ok(outcome);
    }
    let cause = testing_step_cause(&thread.cause);
    // The step thread's cause message carries the TASK DESCRIPTION (title +
    // body) so the prompt builder can place it as the SYSTEM prompt for
    // tester/reviewer threads (inverse role mapping: role template -> USER,
    // task description -> SYSTEM). A bare "Workflow step: testing. Task: <id>"
    // left the tester without the task description in its prompt at all.
    let task_desc = task_description(pool, &task_id).await;
    let cause_msg = format!("Workflow step: testing. Task: {task_id}\n\n{task_desc}");
    let wf_id = thread_workflow_id(pool, thread.id)
        .await
        .unwrap_or_default();
    let identity = resolve_step_identity(pool, data_dir, thread, "testing").await?;
    // Single canonical INSERT (create_thread) - carries plan + template so
    // the tester keeps the role's iteration budget and guidance.
    let new_thread = crate::db::threads::create_thread(
        pool,
        "pending",
        &cause,
        &thread.channel_id,
        &identity.0,
        CreateThreadParams {
            provider: Some(identity.1),
            model: Some(identity.2),
            task_id: Some(task_id.clone()),
            schedule_task_id: None,
            plan: identity.3,
            parent_id: Some(thread.id),
            workflow_id: Some(wf_id),
            workflow_step: Some("testing".to_string()),
            template: identity.4,
            hook_caused: false,
        },
    )
    .await
    .map_err(|e| format!("create testing thread: {e}"))?;
    let new_id = new_thread.id;

    let msg_id: i64 = sql_forge!(
        scalar i64,
        "INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
         VALUES (:new_id, 'cause', :cause_msg, 0, 'cause')
         RETURNING id",
        (
            :new_id = new_id,
            :cause_msg = cause_msg,
        )
    )
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;
    // Event-driven hooks: the seq-0 cause message is a real new message in a
    // non-hook thread - fire new_message exactly once so hook counters see
    // every message (GROUP 27 CI invariant: SQL ground truth == fired events).
    crate::hooks::fire_new_message(new_id, msg_id);

    Ok(StepThreadOutcome::Pending { thread_id: new_id })
}

/// Run a step role in action mode (mode: action): execute the actions.yml tool
/// via the plugin manager and create the terminal kanban thread, mirroring the
/// create_kanban_step_thread action hook. Returns `Some(outcome)` when the role
/// is action-mode and the action executed (or failed to resolve); `None` when
/// the role is agent-mode / runtime unavailable / no action_id - the caller
/// falls back to a pending agent thread.
async fn run_action_role_step(
    pool: &sqlx::PgPool,
    data_dir: &str,
    thread: &Thread,
    step: &str,
    role: &str,
) -> Option<StepThreadOutcome> {
    let task_id = thread.task_id.as_deref()?;
    let workflow_id = thread_workflow_id(pool, thread.id)
        .await
        .unwrap_or_default();
    let role_cfg = if workflow_id.is_empty() {
        None
    } else {
        let path = crate::config_path::config_path(data_dir, "workflows.yml");
        crate::workflows::WorkflowsFile::load(&path)
            .ok()
            .and_then(|f| {
                f.workflows
                    .get(&workflow_id)
                    .and_then(|w| w.resolve_role(role))
            })
    };
    let role_cfg = role_cfg?;
    if role_cfg.effective_mode() != MODE_ACTION {
        return None;
    }
    let Some(action_id) = role_cfg.action_id.clone() else {
        tracing::error!(
            "[workflow] role '{}' has mode=action for step '{}' but no action_id (task {})",
            role,
            step,
            task_id
        );
        return None;
    };
    let Some((plugin_manager, app_context)) = crate::kanban_action::runtime() else {
        tracing::error!(
            "[workflow] kanban_action runtime not initialized; cannot run action '{}' for step '{}' (task {})",
            action_id,
            step,
            task_id
        );
        return None;
    };
    match crate::kanban_action::run_action_step(crate::kanban_action::ActionStepCtx {
        pool,
        data_dir,
        plugin_manager,
        app_context,
        task_id,
        channel_id: &thread.channel_id,
        profile: &thread.profile,
        plan: Some(thread.plan),
        workflow_id: (!workflow_id.is_empty()).then_some(workflow_id.as_str()),
        step,
        role,
        action_id: &action_id,
    })
    .await
    {
        Ok(outcome) => Some(StepThreadOutcome::Action {
            thread_id: outcome.thread_id,
            errored: outcome.errored,
        }),
        Err(e) => {
            tracing::error!(
                "[workflow] action step '{}' for task {} failed: {}",
                step,
                task_id,
                e
            );
            None
        }
    }
}

/// True when the workflow referenced by the thread declares `role`.
/// Fetch the workflow_id of a thread (the `Thread` struct does not carry it).
async fn thread_workflow_id(pool: &sqlx::PgPool, thread_id: i64) -> Option<String> {
    sql_forge!(
        scalar Option<String>,
        "SELECT workflow_id FROM threads WHERE id = :thread_id",
        ( :thread_id = thread_id )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten()
}

fn workflow_has_role(data_dir: &str, workflow_id: &Option<String>, role: &str) -> bool {
    let Some(wf_id) = workflow_id else {
        return false;
    };
    let Ok(wfs) = WorkflowsFile::load(&crate::config_path::config_path(data_dir, "workflows.yml"))
    else {
        return false;
    };
    wfs.workflows
        .get(wf_id.as_str())
        .map(|wf| wf.roles.contains_key(role))
        .unwrap_or(false)
}

/// Returns true if the thread's LAST tool-result message had
/// `metadata.is_error = true` (the final operation of the thread failed).
async fn last_tool_result_errored(pool: &sqlx::PgPool, thread_id: i64) -> bool {
    let is_err: Option<String> = sql_forge!(
        scalar String,
        r#"
        SELECT COALESCE(metadata->>'is_error', 'false')
        FROM messages
        WHERE thread_id = :thread_id AND msg_type = 'tool-result'
        ORDER BY id DESC
        LIMIT 1
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    is_err.as_deref() == Some("true")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> WorkflowPolicy {
        WorkflowPolicy::default()
    }
    fn action() -> WorkflowPolicy {
        WorkflowPolicy {
            role_is_action: true,
            ..WorkflowPolicy::default()
        }
    }
    fn auto_approve() -> WorkflowPolicy {
        WorkflowPolicy {
            auto_approve: true,
            ..WorkflowPolicy::default()
        }
    }
    fn review_on_fail() -> WorkflowPolicy {
        WorkflowPolicy {
            review_on_fail: true,
            ..WorkflowPolicy::default()
        }
    }

    #[test]
    fn route_completed_thread_reviewer_approve_goes_done() {
        // R12: reviewer approves via normal completion (clean) → done,
        // regardless of whether a reviewer role is declared (manual state).
        assert_eq!(
            route_completed_thread("review", false, true, false, agent()),
            CompletedRoute::Done
        );
        assert_eq!(
            route_completed_thread("review", false, false, false, agent()),
            CompletedRoute::Done
        );
    }

    #[test]
    fn route_completed_thread_reviewer_half_finished_blocks() {
        // A review that completed with a failed final tool result is
        // inconclusive - it cannot count as an approve (R12). Also the
        // action-mode reviewer failure → blocked (user rule).
        assert_eq!(
            route_completed_thread("review", true, true, false, agent()),
            CompletedRoute::BlockedInconclusiveReview
        );
        assert_eq!(
            route_completed_thread("review", true, true, false, action()),
            CompletedRoute::BlockedInconclusiveReview
        );
        // review_on_fail does NOT send a failed review back to review.
        assert_eq!(
            route_completed_thread("review", true, true, false, review_on_fail()),
            CompletedRoute::BlockedInconclusiveReview
        );
    }

    #[test]
    fn route_completed_thread_tester_pass_goes_review() {
        // Row 7: tester pass → review step (scheduled review thread when a
        // reviewer role exists, otherwise manual review with no thread).
        assert_eq!(
            route_completed_thread("testing", false, true, false, agent()),
            CompletedRoute::ReviewWithThread
        );
        assert_eq!(
            route_completed_thread("testing", false, false, false, agent()),
            CompletedRoute::ReviewManual
        );
    }

    #[test]
    fn route_completed_thread_tester_error_goes_executor_agent_mode() {
        // D5 (agent mode, no flags): any test error → executor step.
        assert_eq!(
            route_completed_thread("testing", true, false, false, agent()),
            CompletedRoute::TesterErrorToExecutor
        );
        assert_eq!(
            route_completed_thread("testing", true, true, false, agent()),
            CompletedRoute::TesterErrorToExecutor
        );
    }

    #[test]
    fn route_completed_thread_tester_error_goes_review_action_mode() {
        // USER RULE: action-mode tester fail → review (NOT executor re-run).
        assert_eq!(
            route_completed_thread("testing", true, true, false, action()),
            CompletedRoute::ReviewWithThread
        );
        assert_eq!(
            route_completed_thread("testing", true, false, false, action()),
            CompletedRoute::ReviewManual
        );
    }

    #[test]
    fn route_completed_thread_tester_error_goes_review_review_on_fail() {
        // review_on_fail: agent-mode tester error → review (not re-run).
        assert_eq!(
            route_completed_thread("testing", true, true, false, review_on_fail()),
            CompletedRoute::ReviewWithThread
        );
        assert_eq!(
            route_completed_thread("testing", true, false, false, review_on_fail()),
            CompletedRoute::ReviewManual
        );
    }

    #[test]
    fn route_completed_thread_executor_paths() {
        // Executor success without a tester role → review (manual); half-finished
        // executor runs are blocked.
        assert_eq!(
            route_completed_thread("running", false, false, false, agent()),
            CompletedRoute::ReviewManual
        );
        assert_eq!(
            route_completed_thread("", false, false, false, agent()),
            CompletedRoute::ReviewManual
        );
        assert_eq!(
            route_completed_thread("running", true, false, false, agent()),
            CompletedRoute::BlockedHalfFinished
        );
        // Action-mode executor fail → blocked (user rule).
        assert_eq!(
            route_completed_thread("running", true, false, false, action()),
            CompletedRoute::BlockedHalfFinished
        );
        assert_eq!(
            route_completed_thread("running", true, true, false, action()),
            CompletedRoute::BlockedHalfFinished
        );
    }

    #[test]
    fn route_completed_thread_executor_fail_review_on_fail_goes_review() {
        // review_on_fail: failed executor step → review instead of blocked.
        assert_eq!(
            route_completed_thread("running", true, true, false, review_on_fail()),
            CompletedRoute::ReviewWithThread
        );
        assert_eq!(
            route_completed_thread("running", true, false, false, review_on_fail()),
            CompletedRoute::ReviewManual
        );
    }

    #[test]
    fn route_completed_thread_auto_approve_goes_done() {
        // auto_approve: reviewer ignored - tester pass and executor pass
        // (without tester) go straight to done; review_on_fail forced false.
        assert_eq!(
            route_completed_thread("testing", false, true, false, auto_approve()),
            CompletedRoute::Done
        );
        assert_eq!(
            route_completed_thread("testing", false, false, false, auto_approve()),
            CompletedRoute::Done
        );
        assert_eq!(
            route_completed_thread("running", false, false, false, auto_approve()),
            CompletedRoute::Done
        );
        // Executor failure under auto_approve → blocked (review_on_fail ignored).
        assert_eq!(
            route_completed_thread("running", true, true, false, auto_approve()),
            CompletedRoute::BlockedHalfFinished
        );
        assert_eq!(
            route_completed_thread("testing", true, true, false, auto_approve()),
            CompletedRoute::TesterErrorToExecutor
        );
    }

    #[test]
    fn testing_step_route_r7d4() {
        // Executor success with a tester role → testing step thread; without → manual review.
        assert_eq!(
            route_completed_thread("running", false, false, true, agent()),
            CompletedRoute::TestingWithThread
        );
        assert_eq!(
            route_completed_thread("running", false, true, true, agent()),
            CompletedRoute::TestingWithThread
        );
        assert_eq!(
            route_completed_thread("running", false, false, false, agent()),
            CompletedRoute::ReviewManual
        );
        // auto_approve with a tester: testing still runs.
        assert_eq!(
            route_completed_thread("running", false, true, true, auto_approve()),
            CompletedRoute::TestingWithThread
        );
    }

    #[test]
    fn testing_thread_cause_is_check_valid() {
        // R7-D4 regression: create_testing_thread binds threads.cause, which has
        // CHECK chk_thread_cause (cause IN ('user','system')). The pre-fix
        // free-text cause ("Workflow step: testing. Task: {id}") always violated
        // the constraint, silently breaking the testing step-thread chain. The
        // cause must be inherited from the parent thread ('system' for
        // kanban-dispatched executor threads).
        for parent in ["user", "system"] {
            let cause = testing_step_cause(parent);
            assert_eq!(cause, parent);
            assert!(
                cause == "user" || cause == "system",
                "testing thread cause must satisfy chk_thread_cause, got {:?}",
                cause
            );
        }
    }
}
