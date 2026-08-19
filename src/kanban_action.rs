//! Kanban workflow action-mode execution.
//!
//! Workflow roles (executor/tester/reviewer) may declare `mode: action` in
//! workflows.yml: the step runs a predefined actions.yml tool via the plugin
//! manager INSTEAD of the agent loop (mirroring hooks and schedule/cron
//! action modes). This module holds:
//!
//! - the global runtime (plugin manager + app context) registered at startup,
//!   so action-mode step creation works from every dispatch path (in-process
//!   kanban dispatcher, HTTP status-change/redispatch, startup redispatch)
//!   without threading two extra params through every caller;
//! - `run_action_step`, the shared action execution helper: resolves the
//!   actions.yml tool (`scheduler::resolve_action` pattern), executes it via
//!   the plugin manager, persists the result as a kanban step thread
//!   (msg_type='kanban', workflow_step set, TERMINAL — system on success /
//!   failed on error) and returns the outcome.
//!
//! The routing of the step outcome (success/failure → next column) lives in
//! `agent::kanban_updater::route_step_completion` (action-mode matrix:
//! executor fail→blocked, tester fail→review, reviewer fail→blocked; agent
//! defaults unchanged).

use std::sync::Arc;
use std::sync::OnceLock;

use sql_forge::sql_forge;
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::agent::plugin_manager::PluginManager;
use crate::db::types as queries;
use crate::error::{AppResult, Error};
use crate::mcp::AppContext;

/// Runtime handles needed to execute actions.yml tools from workflow step
/// creation/routing — registered once at startup (mirrors GLOBAL_CONFIG).
struct KanbanActionRuntime {
    plugin_manager: Arc<dyn PluginManager>,
    app_context: AppContext,
}

static RUNTIME: OnceLock<KanbanActionRuntime> = OnceLock::new();

/// Register the plugin manager + app context for action-mode execution.
/// Called once at startup after both are constructed. Idempotent.
pub fn init(plugin_manager: Arc<dyn PluginManager>, app_context: AppContext) {
    let _ = RUNTIME.set(KanbanActionRuntime {
        plugin_manager,
        app_context,
    });
}

/// Access the registered runtime. `None` when not initialized (unit tests or
/// a partial startup) — callers treat it as "action execution unavailable".
pub(crate) fn runtime() -> Option<(&'static Arc<dyn PluginManager>, &'static AppContext)> {
    RUNTIME.get().map(|r| (&r.plugin_manager, &r.app_context))
}

/// Outcome of an action-mode workflow step.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ActionStepOutcome {
    pub thread_id: i64,
    /// True when the action tool failed or could not be resolved/executed.
    pub errored: bool,
}

/// Context for `run_action_step`: groups the inputs (task, role, step,
/// pre-resolved channel/profile/plan) to stay under clippy's 7-arg limit.
pub(crate) struct ActionStepCtx<'a> {
    pub pool: &'a PgPool,
    pub data_dir: &'a str,
    pub plugin_manager: &'a Arc<dyn PluginManager>,
    pub app_context: &'a AppContext,
    pub task_id: &'a str,
    /// Pre-resolved channel id (task → board → default_kanban_channel).
    pub channel_id: &'a str,
    /// Pre-resolved profile name.
    pub profile: &'a str,
    /// Pre-resolved plan budget.
    pub plan: Option<bool>,
    pub workflow_id: Option<&'a str>,
    /// workflow_step for the thread: running | testing | review.
    pub step: &'a str,
    /// Role key: executor | tester | reviewer.
    pub role: &'a str,
    /// actions.yml action id (mode: action requires one).
    pub action_id: &'a str,
}

/// Minimal task row for action-mode step execution.
#[derive(sqlx::FromRow)]
struct ActionTaskRow {
    title: String,
    body: Option<String>,
}

/// Run one action-mode workflow step: resolve the actions.yml tool, execute
/// it via the plugin manager, persist the result as a terminal kanban step
/// thread (msg_type='kanban', workflow_step = step), and return the outcome.
///
/// The thread is created TERMINAL (system on success, failed on error) — the
/// agent loop never sees it; the caller routes the task via
/// [`crate::agent::kanban_updater::route_step_completion`].
pub(crate) async fn run_action_step(ctx: ActionStepCtx<'_>) -> AppResult<ActionStepOutcome> {
    let task: Option<ActionTaskRow> = sql_forge!(
        ActionTaskRow,
        "SELECT title, body FROM kanban_tasks WHERE id = :task_id",
        ( :task_id = ctx.task_id )
    )
    .fetch_optional(ctx.pool)
    .await?;
    let (title, body) = match task {
        Some(t) => (t.title, t.body),
        None => {
            return Err(Error::Message(format!(
                "kanban task '{}' not found for action step",
                ctx.task_id
            )))
        }
    };

    // Resolve the actions.yml tool (mirrors scheduler::resolve_action).
    let tool_call = match crate::scheduler::resolve_action(ctx.data_dir, ctx.action_id) {
        Ok(tc) => tc,
        Err(e) => {
            error!(
                "[kanban-action] Failed to resolve action '{}' for task {} step '{}': {}",
                ctx.action_id, ctx.task_id, ctx.step, e
            );
            let outcome = create_action_thread(
                ctx.pool,
                ctx.data_dir,
                ctx.task_id,
                &title,
                body.as_deref(),
                ctx.channel_id,
                ctx.profile,
                ctx.plan,
                ctx.workflow_id,
                ctx.step,
                ctx.role,
                ctx.action_id,
                &format!("Action execution failed: {}", e),
                true,
            )
            .await?;
            return Ok(outcome);
        }
    };

    info!(
        "[kanban-action] Executing action step '{}' for task {} (tool: {}, action_id: {})",
        ctx.step, ctx.task_id, tool_call.name, ctx.action_id
    );

    // Execute the tool first, then persist the thread with the result
    // (mirrors scheduler::handle_action_mode). Snapshot the registry under
    // the lock; tokio::sync::RwLockReadGuard is Send.
    let snapshot = ctx.plugin_manager.snapshot_registry().await;
    match snapshot.execute(&tool_call, ctx.app_context.clone()).await {
        Ok(result) => {
            let is_error = result.is_error;
            if is_error {
                error!(
                    "[kanban-action] Action '{}' for task {} step '{}' returned error: {}",
                    ctx.action_id, ctx.task_id, ctx.step, result.content
                );
            } else {
                info!(
                    "[kanban-action] Action '{}' for task {} step '{}' completed successfully",
                    ctx.action_id, ctx.task_id, ctx.step
                );
            }
            create_action_thread(
                ctx.pool,
                ctx.data_dir,
                ctx.task_id,
                &title,
                body.as_deref(),
                ctx.channel_id,
                ctx.profile,
                ctx.plan,
                ctx.workflow_id,
                ctx.step,
                ctx.role,
                ctx.action_id,
                &result.content,
                is_error,
            )
            .await
        }
        Err(e) => {
            error!(
                "[kanban-action] Action '{}' for task {} step '{}' execution failed: {}",
                ctx.action_id, ctx.task_id, ctx.step, e
            );
            create_action_thread(
                ctx.pool,
                ctx.data_dir,
                ctx.task_id,
                &title,
                body.as_deref(),
                ctx.channel_id,
                ctx.profile,
                ctx.plan,
                ctx.workflow_id,
                ctx.step,
                ctx.role,
                ctx.action_id,
                &format!("Action execution failed: {}", e),
                true,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn create_action_thread(
    pool: &PgPool,
    data_dir: &str,
    task_id: &str,
    title: &str,
    body: Option<&str>,
    channel_id: &str,
    profile: &str,
    plan: Option<bool>,
    workflow_id: Option<&str>,
    step: &str,
    role: &str,
    action_id: &str,
    result_content: &str,
    is_error: bool,
) -> AppResult<ActionStepOutcome> {
    let content = match body.map(str::trim).filter(|b| !b.is_empty()) {
        Some(body) => format!("{title}\n\n{body}"),
        None => title.to_string(),
    };
    let ts = chrono::Utc::now().timestamp();
    let (thread, _cause_msg) = queries::create_thread_with_cause(
        pool,
        data_dir,
        "system",
        channel_id,
        profile,
        queries::ThreadCauseParams {
            provider: None,
            model: None,
            task_id: Some(task_id.to_string()),
            schedule_task_id: None,
            content,
            external_id: Some(format!("kanban-action:{}:{}:{}", task_id, step, ts)),
            parent_external_id: None,
            metadata: serde_json::json!({
                "kanban_task_id": task_id,
                "kanban_task_title": title,
                "mode": "action",
                "role": role,
                "action_id": action_id,
                "is_error": is_error,
            }),
            msg_type: "kanban".to_string(),
            msg_subtype: Some(task_id.to_string()),
            task_plan: plan,
            template: None,
            workflow_id: workflow_id.map(|s| s.to_string()),
            workflow_step: Some(step.to_string()),
            hook_caused: false,
        },
    )
    .await?;

    // Persist the tool result as a seq-1 message (role='agent',
    // msg_type='tool-result' with metadata.is_error) so the action outcome is
    // auditable and `last_tool_result_errored` sees the correct signal.
    let result_msg = queries::MessageNew {
        thread_id: thread.id,
        role: "agent".to_string(),
        content: result_content.to_string(),
        thread_sequence: 1,
        external_id: Some(format!("kanban-action:{}:{}:{}:result", task_id, step, ts)),
        metadata: serde_json::json!({
            "kanban_task_id": task_id,
            "is_error": is_error,
        }),
        embedding: None,
        summary_text: None,
        is_summary: false,
        original_thread_id: None,
        msg_type: "tool-result".to_string(),
        msg_subtype: None,
        iteration_number: 0,
        duration_ms: 0,
        token_usage: serde_json::json!({}),
    };
    if let Err(e) = queries::create_message(pool, &result_msg).await {
        warn!(
            "[kanban-action] Failed to persist action result message: {:?}",
            e
        );
    }

    // Terminal write through the single choke point: system (success) or
    // failed (error). The agent loop never picks this thread up.
    if is_error {
        queries::set_thread_failed(pool, thread.id).await?;
        info!(
            "[kanban-action] Created failure thread {} for task {} step '{}' (action {})",
            thread.id, task_id, step, action_id
        );
    } else {
        queries::set_thread_system(pool, thread.id).await?;
        info!(
            "[kanban-action] Created result thread {} for task {} step '{}' (action {})",
            thread.id, task_id, step, action_id
        );
    }

    Ok(ActionStepOutcome {
        thread_id: thread.id,
        errored: is_error,
    })
}
