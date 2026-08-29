//! In-process kanban dispatcher: promote the highest-priority eligible `todo`
//! task to a running thread.
//!
//! The dispatch decision logic (board gate, dependency gate, channel-busy
//! gate, priority ordering) lives here so it can be driven BOTH by the HTTP
//! handler (`POST /kanban/dispatch`) and by the core background loop
//! (`kanban_dispatcher_interval` in settings.yml, default 15s) - no external
//! cron/action required.

use serde::Serialize;
use sql_forge::sql_forge;
use sqlx::PgPool;
use tracing::error;

use crate::error::{AppResult, Error};

/// Outcome of a single dispatch attempt, shared by the HTTP handler and the
/// in-process loop.
#[derive(Debug, Clone, Serialize)]
pub struct DispatchSummary {
    pub dispatched: bool,
    pub task_id: Option<String>,
    pub thread_id: Option<i64>,
    /// Human-readable reason when nothing was dispatched (or empty on success).
    pub message: String,
}

/// Minimal row for the eligible-task scan.
#[derive(Clone, sqlx::FromRow)]
struct DispatchTaskRow {
    id: String,
    title: String,
    /// Task status; the scan SQL already filters `status = 'todo'`.
    status: String,
    /// Archived flag (NULL = not archived). Archived tasks must NEVER be
    /// dispatched - see `scan_row_eligible` and the scan SQL predicate.
    archived: Option<bool>,
    /// Channel name (yml key) the task targets; needed to gate dispatch on
    /// the channel's active threads without a per-task detail fetch.
    channel_id: Option<String>,
    /// Board the task belongs to (NULL = no board). When boards.yml is
    /// present, NULL/unknown-board tasks are skipped by the eligibility
    /// scan (invalid-board tasks are never promoted/dispatched).
    board: Option<String>,
    /// Goal machine phase (NULL = no goal state). Resume-eligibility input:
    /// a task blocked with the typed code `user-blocked` is never
    /// auto-redispatched (see goal_resume_eligible).
    goal_phase: Option<String>,
    /// Stable machine-routable blocked code (e.g. user-blocked,
    /// provider-unavailable). NULL when the task is not goal-blocked.
    goal_blocked_code: Option<String>,
}

/// Dispatch-scan eligibility for a fetched candidate: `todo` AND not
/// archived. Mirrors the scan SQL predicate (`WHERE status = :status AND
/// archived = false`) so the archived exclusion is unit-testable without a
/// DB. Archived tasks must NEVER be promoted/dispatched: PATCH
/// `archived:true` only flips the flag (the task's status stays 'todo'), so
/// without this exclusion an archived task would be picked up, promoted and
/// its executor thread would run (observed 2026-08-18). NULL `archived`
/// (legacy rows) counts as not archived, matching the app layer's
/// `unwrap_or(false)` semantics.
fn scan_row_eligible(status: &str, archived: Option<bool>) -> bool {
    status == "todo" && !archived.unwrap_or(false)
}

/// Goal resume-eligibility filter: a task whose goal machine is blocked with
/// the typed code `user-blocked` is NEVER auto-dispatched - even when its
/// kanban status is moved back to `todo` - manual review (and an explicit
/// goal clear via PATCH /kanban/tasks/{id}/goal) is required first. Other
/// blocked codes (e.g. provider-unavailable) stay dispatch-eligible: they
/// represent transient conditions that may clear on their own. This is a
/// per-task eligibility filter (like the board gate): the sequential
/// per-channel gate (channel_active_thread_count) is untouched.
fn goal_resume_eligible(goal_phase: Option<&str>, goal_blocked_code: Option<&str>) -> bool {
    !(goal_phase == Some("blocked") && goal_blocked_code == Some("user-blocked"))
}

/// Same-channel active-task claim: a non-archived task in `running` /
/// `testing` / `review` claims its resolved channel for dispatch purposes ONLY
/// when it carries a live thread_status (`scheduled` = a thread is queued,
/// `running` = the thread is processing). A task whose thread_status is
/// empty/NULL (e.g. a manual review with no auto thread) does NOT occupy the
/// channel with a live thread and must NOT block a `todo` task from
/// dispatching. Mirrors the scan SQL predicate in `dispatch_todo_tasks`
/// (status IN running/testing/review AND archived = false) so the exclusion
/// is unit-testable without a DB; NULL `archived` (legacy rows) counts as not
/// archived, matching `scan_row_eligible`.
fn active_task_claims_channel(
    status: &str,
    thread_status: Option<&str>,
    archived: Option<bool>,
) -> bool {
    !archived.unwrap_or(false)
        && matches!(status, "running" | "testing" | "review")
        && matches!(thread_status, Some("scheduled") | Some("running"))
}

/// Whether the candidate `todo` task's resolved channel is claimed by another
/// active task (see `active_task_claims_channel`): same resolved channel
/// string. Tasks on different channels are never affected.
fn channel_claimed(candidate_channel: &str, claimed_channels: &[String]) -> bool {
    claimed_channels.iter().any(|c| c == candidate_channel)
}

#[derive(sqlx::FromRow)]
struct DispatchDependencyRow {
    depends_on_id: String,
}

#[derive(sqlx::FromRow)]
struct DispatchDepStatusRow {
    status: String,
    archived: Option<bool>,
}

/// Row for the same-channel active-task scan (step 1d): non-archived tasks in
/// `running`/`testing`/`review`. `thread_status` (NULL | 'scheduled' |
/// 'running') decides whether the task actually occupies its channel with a
/// live/auto thread (see `active_task_claims_channel`).
#[derive(sqlx::FromRow)]
struct DispatchActiveTaskRow {
    id: String,
    status: String,
    archived: Option<bool>,
    /// Channel name (yml key) the task targets; resolved the same way as the
    /// candidate's channel (task -> board -> default).
    channel_id: Option<String>,
    board: Option<String>,
    thread_status: Option<String>,
}

/// A dependency's dispatch-relevant state: `None` when the dependency row is
/// missing (which blocks dispatch). Otherwise `(status, archived)`.
type DepState = Option<(String, Option<bool>)>;

/// Eligibility gate: every dependency must be archived or `done`.
/// Mirrors the dispatcher semantics: archived -> ok, missing row -> blocks,
/// status != "done" -> blocks.
fn deps_satisfied(deps: &[DepState]) -> bool {
    deps.iter().all(|dep| match dep {
        None => false,
        Some((status, archived)) => *archived == Some(true) || status == "done",
    })
}

/// Index of the first task that is dependency-eligible, whose channel has no
/// active (queued/running) thread, AND whose channel is not claimed by
/// another active task. `channel_active_counts[i]` is the number of active
/// threads on candidate i's channel (0 = free to dispatch);
/// `channel_claimed[i]` is whether candidate i's resolved channel is claimed
/// by another non-archived running/testing/review task with a live
/// thread_status (see `active_task_claims_channel`).
fn first_dispatchable_index(
    task_deps: &[Vec<DepState>],
    channel_active_counts: &[i64],
    channel_claimed: &[bool],
) -> Option<usize> {
    task_deps
        .iter()
        .zip(channel_active_counts)
        .zip(channel_claimed)
        .position(|((deps, &active), &claimed)| deps_satisfied(deps) && active == 0 && !claimed)
}

/// Number of ACTIVE (queued/running) threads on a channel.
///
/// The dispatch gate blocks a channel that has any of these - the in-flight
/// task's full workflow (executor -> tester -> reviewer -> done) must finish
/// before the next task on the same channel begins. The filter is STATUS-based
/// (`pending` = queued, `processing` = running) - deliberately NOT
/// terminal-based: an operator stop leaves `skipped` threads with
/// terminal=false, and a terminal gate would block dispatch on that channel
/// forever. The `idx_threads_channel_status (channel_id, status)` index keeps
/// the count cheap.
async fn channel_active_thread_count(pool: &PgPool, channel_id: &str) -> Result<i64, sqlx::Error> {
    let count: i64 = sql_forge!(
        scalar i64,
        r#"
        SELECT COUNT(*) FROM threads
        WHERE channel_id = :channel_id AND status IN ('pending', 'processing')
        "#,
        ( :channel_id = channel_id )
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Resolve the effective channel NAME for a task: the explicit task channel
/// wins (even when unknown - the caller then fails the thread with "channel
/// not found"), else the board's channel, else the `default_kanban_channel`
/// setting, else "". Shared resolver (src/resolution.rs) - no per-consumer
/// fallback logic.
fn resolve_task_channel(
    data_dir: &str,
    task_channel: Option<&str>,
    board_channel: Option<&str>,
) -> String {
    crate::resolution::effective_channel_name(
        data_dir,
        task_channel
            .filter(|s| !s.trim().is_empty())
            .or(board_channel),
        "default_kanban_channel",
    )
}

/// Run ONE dispatch pass: promote the highest-priority eligible `todo` task
/// to `running` and start a thread for it. A task is eligible when every
/// non-archived dependency is `done` AND its channel has no active
/// (queued/running) thread AND its channel is not claimed by another active
/// task - the channel gates let the current task's full workflow
/// (executor -> tester -> reviewer -> done) finish before the next task on
/// the same channel begins.
///
/// Returns `dispatched: false` (with a reason message) when nothing is
/// eligible, and `Err` on internal failures (caller decides how to surface:
/// HTTP error response or loop log).
pub async fn dispatch_todo_tasks(pool: &PgPool, data_dir: &str) -> AppResult<DispatchSummary> {
    // 1. Scan 'todo' tasks in priority order. `archived = false` is REQUIRED:
    //    PATCH `archived:true` only flips the flag (it does NOT move the
    //    status), so an archived task left in `todo` must never be picked up
    //    and promoted (observed 2026-08-18). The Rust-side
    //    `scan_row_eligible` filter below backstops the SQL for NULL
    //    `archived` rows.
    let tasks = match sql_forge!(
        DispatchTaskRow,
        r#"
        SELECT id, title, status, archived, channel_id, board,
               goal_phase, goal_blocked_code
        FROM kanban_tasks
        WHERE status = :status AND archived = false
        ORDER BY priority ASC, position ASC
        "#,
        ( :status = "todo" )
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[kanban/dispatch] failed to list todo tasks: {:?}", e);
            return Err(Error::Message(format!("Failed to list todo tasks: {e}")));
        }
    };

    // 1a. Archived gate (Rust-side backstop): archived candidates must never
    //     be dispatched even if a NULL `archived` row slips past the SQL
    //     predicate.
    let tasks: Vec<DispatchTaskRow> = tasks
        .into_iter()
        .filter(|t| scan_row_eligible(&t.status, t.archived))
        .collect();

    // 1b. Board gate (feature-flagged on the presence of config/boards.yml):
    //     when boards are enabled, tasks with no board or an unknown board
    //     are INVALID-BOARD tasks - skipped exactly like backlog/archived
    //     tasks (never promoted/dispatched). Thread creation for them is
    //     additionally blocked/failed in create_kanban_step_thread.
    let boards_enabled = crate::boards::boards_enabled(data_dir);
    let boards_file: Option<crate::boards::BoardsFile> = if boards_enabled {
        match crate::boards::BoardsFile::load(&crate::config_path::config_path(
            data_dir,
            "boards.yml",
        )) {
            Ok(file) => Some(file),
            Err(e) => {
                error!("[kanban/dispatch] failed to load boards.yml: {:?}", e);
                return Err(Error::Message("Failed to load boards.yml".to_string()));
            }
        }
    } else {
        None
    };
    let tasks: Vec<DispatchTaskRow> = match &boards_file {
        Some(file) => tasks
            .into_iter()
            .filter(|t| {
                t.board
                    .as_deref()
                    .map(|b| file.boards.contains_key(b))
                    .unwrap_or(false)
            })
            .collect(),
        None => tasks,
    };

    // 1c. Goal resume-eligibility gate (per-task filter, like the board
    //     gate): a task whose goal machine is blocked with the typed code
    //     `user-blocked` is never auto-dispatched - even when its status is
    //     moved back to `todo`, manual review + an explicit goal clear is
    //     required first. Does NOT touch the sequential per-channel gate.
    let tasks: Vec<DispatchTaskRow> = tasks
        .into_iter()
        .filter(|t| goal_resume_eligible(t.goal_phase.as_deref(), t.goal_blocked_code.as_deref()))
        .collect();

    // 1d. Same-channel active-task scan (task-level gate): non-archived tasks
    //     in `running`/`testing`/`review` whose thread_status is live
    //     ('scheduled' = thread queued, 'running' = thread processing) claim
    //     their resolved channel - a `todo` candidate on that channel must NOT
    //     be dispatched while the predecessor's workflow still has a live
    //     thread. This closes the window the threads-table gate alone misses
    //     (e.g. between an executor finishing and its testing/review thread
    //     spawning, or while a review thread still occupies the channel).
    //     Tasks with empty/NULL thread_status (manual review, no auto thread)
    //     are IGNORED - they do not occupy the channel. Archived tasks never
    //     count. Channels are resolved with the same task -> board -> default
    //     resolution used for the candidates (resolve_task_channel).
    let active_tasks = match sql_forge!(
        DispatchActiveTaskRow,
        r#"
        SELECT id, status, archived, channel_id, board, thread_status
        FROM kanban_tasks
        WHERE status IN ('running', 'testing', 'review') AND archived = false
        "#,
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[kanban/dispatch] failed to list active tasks: {:?}", e);
            return Err(Error::Message(format!("Failed to list active tasks: {e}")));
        }
    };
    let claimed_channels: Vec<String> = active_tasks
        .iter()
        .filter(|t| active_task_claims_channel(&t.status, t.thread_status.as_deref(), t.archived))
        .map(|t| {
            resolve_task_channel(
                data_dir,
                t.channel_id.as_deref(),
                boards_file.as_ref().and_then(|file| {
                    t.board
                        .as_deref()
                        .and_then(|b| file.boards.get(b))
                        .and_then(|cfg| cfg.channel.as_deref())
                }),
            )
        })
        .collect();

    // 2. Resolve dependency state for each candidate.
    let mut all_deps: Vec<Vec<DepState>> = Vec::with_capacity(tasks.len());
    for task in &tasks {
        let dep_ids = match sql_forge!(
            DispatchDependencyRow,
            r#"
            SELECT depends_on_id
            FROM kanban_task_dependencies
            WHERE task_id = :task_id
            "#,
            ( :task_id = task.id.as_str() )
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                error!(
                    "[kanban/dispatch] failed to load dependencies for {}: {:?}",
                    task.id, e
                );
                return Err(Error::Message(format!(
                    "Failed to load dependencies for task {}: {e}",
                    task.id
                )));
            }
        };

        let mut dep_states: Vec<DepState> = Vec::with_capacity(dep_ids.len());
        for dep in &dep_ids {
            let row = match sql_forge!(
                DispatchDepStatusRow,
                r#"
                SELECT status, archived
                FROM kanban_tasks
                WHERE id = :id
                "#,
                ( :id = dep.depends_on_id.as_str() )
            )
            .fetch_optional(pool)
            .await
            {
                Ok(row) => row,
                Err(e) => {
                    error!(
                        "[kanban/dispatch] failed to resolve dependency {}: {:?}",
                        dep.depends_on_id, e
                    );
                    return Err(Error::Message(format!(
                        "Failed to resolve dependency {}: {e}",
                        dep.depends_on_id
                    )));
                }
            };
            dep_states.push(row.map(|r| (r.status, r.archived)));
        }
        all_deps.push(dep_states);
    }

    // 2b. Channel-busy gate: skip candidates whose channel has an active
    // (queued/running) thread - the status-based gate, NOT terminal-based (an
    // operator stop leaves `skipped` threads with terminal=false, and a
    // terminal gate would block dispatch on that channel forever). Skipping a
    // busy channel lets the in-flight task's full workflow (executor ->
    // tester -> reviewer -> done) finish before the next task on the same
    // channel begins.
    //
    // 2c. Same-channel active-task gate (task-level, see step 1d): skip
    // candidates whose resolved channel is claimed by another non-archived
    // running/testing/review task with a live thread_status. Pick the first
    // task that is dependency-eligible, channel-free AND not claimed.
    let mut channel_active_counts: Vec<i64> = Vec::with_capacity(tasks.len());
    let mut channel_claimed_flags: Vec<bool> = Vec::with_capacity(tasks.len());
    let mut busy_channel: Option<(String, i64)> = None;
    let mut claimed_channel: Option<String> = None;
    for (i, task) in tasks.iter().enumerate() {
        // A dependency-blocked candidate can never be picked; its channel
        // state is irrelevant (0/false keeps the index alignment).
        if !deps_satisfied(&all_deps[i]) {
            channel_active_counts.push(0);
            channel_claimed_flags.push(false);
            continue;
        }
        let channel_id = resolve_task_channel(
            data_dir,
            task.channel_id.as_deref(),
            boards_file.as_ref().and_then(|file| {
                task.board
                    .as_deref()
                    .and_then(|b| file.boards.get(b))
                    .and_then(|cfg| cfg.channel.as_deref())
            }),
        );
        let active = match channel_active_thread_count(pool, &channel_id).await {
            Ok(n) => n,
            Err(e) => {
                error!(
                    "[kanban/dispatch] failed to count active threads for channel {}: {:?}",
                    channel_id, e
                );
                return Err(Error::Message(format!(
                    "Failed to count active threads for channel {channel_id}: {e}"
                )));
            }
        };
        if busy_channel.is_none() && active > 0 {
            busy_channel = Some((channel_id.clone(), active));
        }
        let claimed = channel_claimed(&channel_id, &claimed_channels);
        if claimed_channel.is_none() && claimed {
            claimed_channel = Some(channel_id);
        }
        channel_active_counts.push(active);
        channel_claimed_flags.push(claimed);
    }

    let picked =
        match first_dispatchable_index(&all_deps, &channel_active_counts, &channel_claimed_flags)
            .and_then(|i| tasks.get(i))
        {
            Some(task) => task,
            None => {
                return Ok(DispatchSummary {
                    dispatched: false,
                    task_id: None,
                    thread_id: None,
                    message: match busy_channel {
                        Some((channel_id, active)) => {
                            format!("Channel busy: {channel_id} has {active} active thread(s)")
                        }
                        None => match claimed_channel {
                            Some(channel_id) => format!(
                                "Channel claimed by active task: {channel_id} has a live thread"
                            ),
                            None => "No eligible kanban tasks".to_string(),
                        },
                    },
                });
            }
        };

    // 3. Start the executor thread via the shared status-dispatch path (the
    //    same code as status-change dispatch and /redispatch): it skips any
    //    stale active threads, resolves the executor role/template/plan and
    //    creates the thread with workflow_step='running'.
    let thread_id =
        match crate::db::threads::dispatch_task_for_status(pool, data_dir, &picked.id, "running")
            .await
        {
            Ok(Some(tid)) => tid,
            Ok(None) => {
                error!(
                    "[kanban/dispatch] no executor role to run for task {}",
                    picked.id
                );
                return Err(Error::Message("No executor role to run".to_string()));
            }
            Err(e) => {
                error!(
                    "[kanban/dispatch] failed to create thread for {}: {:?}",
                    picked.id, e
                );
                return Err(Error::Message(format!("Failed to create thread: {e}")));
            }
        };

    // 4. Mark the task running ("ready" was retired - see VALID_STATUSES; the
    //    executor would flip it to "running" on pickup anyway).
    //
    //    ACTION-MODE EXCEPTION: for an action-mode executor, step 3 ran the
    //    action SYNCHRONOUSLY inside create_kanban_step_thread and the hook
    //    already routed the task through the workflow matrix
    //    (review/blocked/done/testing) via route_step_completion. Never
    //    clobber that routed status back to `running` - the task would sit in
    //    `running` forever with only a terminal action thread (GROUP 40-A).
    //    Only mark `running` when the task is still `todo` (agent-mode
    //    executor: the pending thread runs asynchronously).
    let current_status: Option<String> = sql_forge!(
        scalar String,
        "SELECT status FROM kanban_tasks WHERE id = :task_id",
        ( :task_id = picked.id.as_str() )
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        error!(
            "[kanban/dispatch] failed to read status for task {}: {:?}",
            picked.id, e
        );
        Error::Message(format!("Failed to read task status: {e}"))
    })?;
    if current_status.as_deref() != Some("todo") {
        tracing::info!(
            "[kanban/dispatch] task {} already transitioned by action-mode hook (status={}) - leaving as-is",
            picked.id,
            current_status.as_deref().unwrap_or("?")
        );
        return Ok(DispatchSummary {
            dispatched: true,
            task_id: Some(picked.id.clone()),
            thread_id: Some(thread_id),
            message: format!(
                "Action-mode executor ran synchronously; task routed to {}",
                current_status.as_deref().unwrap_or("unknown")
            ),
        });
    }
    if let Err(e) = crate::db::kanban::update_kanban_task_status(pool, &picked.id, "running").await
    {
        error!(
            "[kanban/dispatch] failed to mark task {} running: {:?}",
            picked.id, e
        );
        return Err(Error::Message(format!("Failed to update task status: {e}")));
    }

    Ok(DispatchSummary {
        dispatched: true,
        task_id: Some(picked.id.clone()),
        thread_id: Some(thread_id),
        message: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_no_eligible_tasks() {
        // A task whose dependency is still 'todo' is not eligible -> no dispatch.
        let blocked = vec![Some(("todo".to_string(), Some(false)))];
        assert!(!deps_satisfied(&blocked));
        assert_eq!(first_dispatchable_index(&[blocked], &[0], &[false]), None);

        // Missing dependency rows also block.
        assert!(!deps_satisfied(&[None]));
        assert_eq!(
            first_dispatchable_index(&[vec![None]], &[0], &[false]),
            None
        );
    }

    #[test]
    fn dispatch_deps_gate_skips_unsatisfied() {
        // Task 0 has a non-done dep (blocked); task 1 has a done dep (eligible);
        // task 2 has no deps (eligible). The first eligible is task 1.
        let task_deps = vec![
            vec![Some(("todo".to_string(), Some(false)))],
            vec![Some(("done".to_string(), Some(false)))],
            vec![],
        ];
        // All channels free -> same result as the old dep-only picker.
        assert_eq!(
            first_dispatchable_index(&task_deps, &[0, 0, 0], &[false, false, false]),
            Some(1)
        );

        // All blocked -> None (dispatched: false).
        let all_blocked = vec![vec![Some(("todo".to_string(), Some(false)))], vec![None]];
        assert_eq!(
            first_dispatchable_index(&all_blocked, &[0, 0], &[false, false]),
            None
        );

        // Archived dependencies never block, regardless of status.
        let archived = vec![
            Some(("todo".to_string(), Some(true))),
            Some(("backlog".to_string(), Some(true))),
        ];
        assert!(deps_satisfied(&archived));
    }

    /// Mirror of the gate's status filter in `channel_active_thread_count`:
    /// only queued (`pending`) or running (`processing`) threads count as
    /// active. Kept in sync with the SQL.
    fn active_thread_count(statuses: &[&str]) -> i64 {
        statuses
            .iter()
            .copied()
            .filter(|s| matches!(*s, "pending" | "processing"))
            .count() as i64
    }

    #[test]
    fn dispatch_channel_busy_gate_status_based() {
        // (a) A queued thread on the channel blocks dispatch.
        assert_eq!(active_thread_count(&["pending"]), 1);
        // (b) A running thread on the channel blocks dispatch.
        assert_eq!(active_thread_count(&["processing"]), 1);
        assert_eq!(active_thread_count(&["pending", "processing"]), 2);
        // (c) Terminal-status threads never block dispatch.
        assert_eq!(
            active_thread_count(&[
                "completed",
                "failed",
                "skipped",
                "merged",
                "interrupted",
                "created"
            ]),
            0
        );
        // (d) Regression: an operator-stop `skipped` thread with
        // terminal=false does NOT block - the gate never looks at `terminal`.
        assert_eq!(active_thread_count(&["skipped"]), 0);
    }

    #[test]
    fn dispatch_channel_busy_gate_picks_first_free_channel() {
        // All candidates dependency-eligible.
        let free = vec![vec![], vec![], vec![]];
        // (a)+(b): first candidate's channel has a queued/running thread ->
        // skipped in favor of the next channel-free candidate.
        assert_eq!(
            first_dispatchable_index(&free, &[1, 0], &[false, false]),
            Some(1)
        );
        assert_eq!(
            first_dispatchable_index(&free, &[2, 0], &[false, false]),
            Some(1)
        );
        // (c): only terminal threads on the channels -> first candidate wins.
        assert_eq!(
            first_dispatchable_index(&free, &[0, 0, 0], &[false, false, false]),
            Some(0)
        );
        // (d): a skipped thread is NOT counted -> channel free, dispatched.
        assert_eq!(
            first_dispatchable_index(&free, &[0, 1], &[false, false]),
            Some(0)
        );
        // A dependency-blocked candidate is skipped even when its channel is
        // free; the first free dep-eligible candidate is picked.
        let mixed = vec![vec![Some(("todo".to_string(), Some(false)))], vec![]];
        assert_eq!(
            first_dispatchable_index(&mixed, &[0, 0], &[false, false]),
            Some(1)
        );
        // All channels busy -> None (dispatch returns dispatched:false).
        assert_eq!(
            first_dispatchable_index(&free, &[1, 1, 1], &[false, false, false]),
            None
        );
    }

    #[test]
    fn dispatch_scan_skips_archived_tasks() {
        // Regression (2026-08-18): archived kanban tasks must never be
        // promoted/dispatched. PATCH `archived:true` only flips the flag -
        // the task's status stays 'todo' - so scan eligibility MUST exclude
        // archived tasks. On the old scan SQL (`WHERE status = :status`
        // only) this exact scenario promoted the archived task and ran its
        // executor thread.
        assert!(
            !scan_row_eligible("todo", Some(true)),
            "an archived 'todo' task must never be dispatch-eligible"
        );
        // Non-archived 'todo' tasks stay eligible (NULL archived = not
        // archived, matching the app layer's unwrap_or(false) semantics).
        assert!(scan_row_eligible("todo", Some(false)));
        assert!(scan_row_eligible("todo", None));
        // Only 'todo' is scanned; other statuses are never eligible here.
        assert!(!scan_row_eligible("backlog", Some(false)));
        assert!(!scan_row_eligible("running", Some(false)));
        assert!(!scan_row_eligible("done", Some(false)));
    }

    #[test]
    fn dispatch_goal_resume_eligibility() {
        // No goal state -> eligible (zero behavior change for non-goal tasks).
        assert!(goal_resume_eligible(None, None));
        // Active/paused/complete phases -> eligible.
        assert!(goal_resume_eligible(Some("active"), None));
        assert!(goal_resume_eligible(Some("paused"), Some("user-blocked")));
        assert!(goal_resume_eligible(
            Some("complete"),
            Some("provider-unavailable")
        ));
        // Blocked with a transient code (or no code) -> eligible: the
        // sequential per-channel gate alone decides dispatch.
        assert!(goal_resume_eligible(
            Some("blocked"),
            Some("provider-unavailable")
        ));
        assert!(goal_resume_eligible(Some("blocked"), None));
        // Blocked + user-blocked -> NEVER auto-redispatched.
        assert!(!goal_resume_eligible(Some("blocked"), Some("user-blocked")));
    }

    #[test]
    fn dispatch_goal_filter_preserves_channel_gate() {
        // Goal state is per-task state: it never changes the channel-busy
        // gate, so the sequential-per-channel invariant holds. A channel
        // with a queued/running thread still blocks dispatch regardless of
        // any candidate's goal state...
        assert_eq!(active_thread_count(&["pending"]), 1);
        assert_eq!(active_thread_count(&["processing"]), 1);
        // ...and first_dispatchable_index still honors the channel gate.
        assert_eq!(first_dispatchable_index(&[vec![]], &[1], &[false]), None);
        assert_eq!(first_dispatchable_index(&[vec![]], &[0], &[false]), Some(0));
    }

    #[test]
    fn dispatch_active_task_claims_channel() {
        // running/testing/review with a LIVE thread_status ('scheduled' =
        // thread queued, 'running' = thread processing) claim their channel.
        for status in ["running", "testing", "review"] {
            assert!(active_task_claims_channel(
                status,
                Some("scheduled"),
                Some(false)
            ));
            assert!(active_task_claims_channel(
                status,
                Some("running"),
                Some(false)
            ));
        }
        // A task with empty/NULL thread_status (manual review / no auto
        // thread) never claims the channel -> the todo task CAN dispatch.
        for status in ["running", "testing", "review"] {
            assert!(!active_task_claims_channel(status, None, Some(false)));
            assert!(!active_task_claims_channel(status, Some(""), Some(false)));
        }
        // Archived tasks never count, even with a live thread_status.
        assert!(!active_task_claims_channel(
            "running",
            Some("scheduled"),
            Some(true)
        ));
        assert!(!active_task_claims_channel(
            "review",
            Some("running"),
            Some(true)
        ));
        // NULL archived (legacy rows) counts as not archived.
        assert!(active_task_claims_channel(
            "running",
            Some("scheduled"),
            None
        ));
        // Only running/testing/review statuses are in the scan.
        assert!(!active_task_claims_channel(
            "todo",
            Some("scheduled"),
            Some(false)
        ));
        assert!(!active_task_claims_channel(
            "done",
            Some("scheduled"),
            Some(false)
        ));
        assert!(!active_task_claims_channel(
            "blocked",
            Some("scheduled"),
            Some(false)
        ));
        assert!(!active_task_claims_channel(
            "backlog",
            Some("scheduled"),
            Some(false)
        ));
    }

    #[test]
    fn dispatch_blocked_by_same_channel_active_task() {
        // A todo candidate whose channel is claimed by another active task
        // (running/testing/review + live thread_status) is NOT dispatched -
        // even when the channel has no queued/running thread (the
        // threads-table gate alone would miss the window where the
        // predecessor's thread_status is still live).
        assert_eq!(first_dispatchable_index(&[vec![]], &[0], &[true]), None);
        // ...and when both gates fire.
        assert_eq!(first_dispatchable_index(&[vec![]], &[1], &[true]), None);
        // The claimed gate is per-candidate: a later candidate on a free,
        // unclaimed channel is still picked.
        let free = vec![vec![], vec![]];
        assert_eq!(
            first_dispatchable_index(&free, &[0, 0], &[true, false]),
            Some(1)
        );
    }

    #[test]
    fn dispatch_allowed_when_no_same_channel_claim() {
        // No active task on the candidate's channel -> dispatched.
        assert_eq!(first_dispatchable_index(&[vec![]], &[0], &[false]), Some(0));
        // Full chain: same-channel task with NULL/empty thread_status (manual
        // review) -> not claimed -> todo dispatched.
        let manual_review_claimed = vec![active_task_claims_channel("review", None, Some(false))];
        assert_eq!(
            first_dispatchable_index(&[vec![]], &[0], &manual_review_claimed),
            Some(0)
        );
        // Different channels never interact: candidate on chan-a, active task
        // on chan-b.
        assert!(!channel_claimed("chan-a", &["chan-b".to_string()]));
        assert!(channel_claimed("chan-a", &["chan-a".to_string()]));
        assert!(!channel_claimed("chan-a", &[]));
    }

    #[test]
    fn dispatch_archived_tasks_never_claim() {
        // Archived tasks are ignored by the claim scan (SQL predicate + Rust
        // backstop): they never block a todo candidate, and their
        // thread_status never counts as a live channel claim.
        assert!(!active_task_claims_channel(
            "running",
            Some("scheduled"),
            Some(true)
        ));
        assert!(!active_task_claims_channel(
            "testing",
            Some("running"),
            Some(true)
        ));
        assert!(!active_task_claims_channel(
            "review",
            Some("scheduled"),
            Some(true)
        ));
    }
}
