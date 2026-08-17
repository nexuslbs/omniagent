//! In-process kanban dispatcher: promote the highest-priority eligible `todo`
//! task to a running thread.
//!
//! The dispatch decision logic (board gate, dependency gate, channel-busy
//! gate, priority ordering) lives here so it can be driven BOTH by the HTTP
//! handler (`POST /kanban/dispatch`) and by the core background loop
//! (`kanban_dispatcher_interval` in settings.yml, default 15s) — no external
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
    /// Channel name (yml key) the task targets; needed to gate dispatch on
    /// the channel's active threads without a per-task detail fetch.
    channel_id: Option<String>,
    /// Board the task belongs to (NULL = no board). When boards.yml is
    /// present, NULL/unknown-board tasks are skipped by the eligibility
    /// scan (invalid-board tasks are never promoted/dispatched).
    board: Option<String>,
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

/// Index of the first task that is BOTH dependency-eligible AND whose channel
/// has no active (queued/running) thread. `channel_active_counts[i]` is the
/// number of active threads on candidate i's channel (0 = free to dispatch).
fn first_dispatchable_index(
    task_deps: &[Vec<DepState>],
    channel_active_counts: &[i64],
) -> Option<usize> {
    task_deps
        .iter()
        .zip(channel_active_counts)
        .position(|(deps, &active)| deps_satisfied(deps) && active == 0)
}

/// Number of ACTIVE (queued/running) threads on a channel.
///
/// The dispatch gate blocks a channel that has any of these — the in-flight
/// task's full workflow (executor -> tester -> reviewer -> done) must finish
/// before the next task on the same channel begins. The filter is STATUS-based
/// (`pending` = queued, `processing` = running) — deliberately NOT
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
/// wins (even when unknown — the caller then fails the thread with "channel
/// not found"), else the `default_kanban_channel` setting, else "".
fn resolve_task_channel(task_channel: Option<&str>) -> String {
    match task_channel {
        Some(id) => {
            crate::channels_yaml::resolve_default_channel(Some(id), "default_kanban_channel")
                .unwrap_or_default()
        }
        None => crate::channels_yaml::resolve_default_channel(None, "default_kanban_channel")
            .unwrap_or_default(),
    }
}

/// Run ONE dispatch pass: promote the highest-priority eligible `todo` task
/// to `running` and start a thread for it. A task is eligible when every
/// non-archived dependency is `done` AND its channel has no active
/// (queued/running) thread — the channel gate lets the current task's full
/// workflow (executor -> tester -> reviewer -> done) finish before the next
/// task on the same channel begins.
///
/// Returns `dispatched: false` (with a reason message) when nothing is
/// eligible, and `Err` on internal failures (caller decides how to surface:
/// HTTP error response or loop log).
pub async fn dispatch_todo_tasks(pool: &PgPool, data_dir: &str) -> AppResult<DispatchSummary> {
    // 1. Scan 'todo' tasks in priority order.
    let tasks = match sql_forge!(
        DispatchTaskRow,
        r#"
        SELECT id, title, channel_id, board
        FROM kanban_tasks
        WHERE status = :status
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

    // 1b. Board gate (feature-flagged on the presence of config/boards.yml):
    //     when boards are enabled, tasks with no board or an unknown board
    //     are INVALID-BOARD tasks — skipped exactly like backlog/archived
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
    // (queued/running) thread — the status-based gate, NOT terminal-based (an
    // operator stop leaves `skipped` threads with terminal=false, and a
    // terminal gate would block dispatch on that channel forever). Skipping a
    // busy channel lets the in-flight task's full workflow (executor ->
    // tester -> reviewer -> done) finish before the next task on the same
    // channel begins. Pick the first task that is BOTH dependency-eligible
    // AND channel-free.
    let mut channel_active_counts: Vec<i64> = Vec::with_capacity(tasks.len());
    let mut busy_channel: Option<(String, i64)> = None;
    for (i, task) in tasks.iter().enumerate() {
        // A dependency-blocked candidate can never be picked; its channel
        // state is irrelevant (0 keeps the index alignment).
        if !deps_satisfied(&all_deps[i]) {
            channel_active_counts.push(0);
            continue;
        }
        let channel_id = resolve_task_channel(task.channel_id.as_deref().or_else(|| {
            boards_file.as_ref().and_then(|file| {
                task.board
                    .as_deref()
                    .and_then(|b| file.boards.get(b))
                    .and_then(|cfg| cfg.channel.as_deref())
            })
        }));
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
            busy_channel = Some((channel_id, active));
        }
        channel_active_counts.push(active);
    }

    let picked = match first_dispatchable_index(&all_deps, &channel_active_counts)
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
                    None => "No eligible kanban tasks".to_string(),
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

    // 4. Mark the task running ("ready" was retired — see VALID_STATUSES; the
    //    executor would flip it to "running" on pickup anyway).
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
        assert_eq!(first_dispatchable_index(&[blocked], &[0]), None);

        // Missing dependency rows also block.
        assert!(!deps_satisfied(&[None]));
        assert_eq!(first_dispatchable_index(&[vec![None]], &[0]), None);
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
        assert_eq!(first_dispatchable_index(&task_deps, &[0, 0, 0]), Some(1));

        // All blocked -> None (dispatched: false).
        let all_blocked = vec![vec![Some(("todo".to_string(), Some(false)))], vec![None]];
        assert_eq!(first_dispatchable_index(&all_blocked, &[0, 0]), None);

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
            active_thread_count(&["completed", "failed", "skipped", "interrupted", "created"]),
            0
        );
        // (d) Regression: an operator-stop `skipped` thread with
        // terminal=false does NOT block — the gate never looks at `terminal`.
        assert_eq!(active_thread_count(&["skipped"]), 0);
    }

    #[test]
    fn dispatch_channel_busy_gate_picks_first_free_channel() {
        // All candidates dependency-eligible.
        let free = vec![vec![], vec![], vec![]];
        // (a)+(b): first candidate's channel has a queued/running thread ->
        // skipped in favor of the next channel-free candidate.
        assert_eq!(first_dispatchable_index(&free, &[1, 0]), Some(1));
        assert_eq!(first_dispatchable_index(&free, &[2, 0]), Some(1));
        // (c): only terminal threads on the channels -> first candidate wins.
        assert_eq!(first_dispatchable_index(&free, &[0, 0, 0]), Some(0));
        // (d): a skipped thread is NOT counted -> channel free, dispatched.
        assert_eq!(first_dispatchable_index(&free, &[0, 1]), Some(0));
        // A dependency-blocked candidate is skipped even when its channel is
        // free; the first free dep-eligible candidate is picked.
        let mixed = vec![vec![Some(("todo".to_string(), Some(false)))], vec![]];
        assert_eq!(first_dispatchable_index(&mixed, &[0, 0]), Some(1));
        // All channels busy -> None (dispatch returns dispatched:false).
        assert_eq!(first_dispatchable_index(&free, &[1, 1, 1]), None);
    }
}
