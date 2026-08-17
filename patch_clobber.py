#!/usr/bin/env python3
"""Patch kanban_dispatch.rs: don't clobber a status the action-mode hook already routed."""
import io, sys

PATH = "/app/src/kanban_dispatch.rs"

old = """    // 4. Mark the task running ("ready" was retired — see VALID_STATUSES; the
    //    executor would flip it to "running" on pickup anyway).
    if let Err(e) = crate::db::kanban::update_kanban_task_status(pool, &picked.id, "running").await
    {
        error!(
            "[kanban/dispatch] failed to mark task {} running: {:?}",
            picked.id, e
        );
        return Err(Error::Message(format!("Failed to update task status: {e}")));
    }"""

new = """    // 4. Mark the task running ("ready" was retired — see VALID_STATUSES; the
    //    executor would flip it to "running" on pickup anyway).
    //
    //    ACTION-MODE EXCEPTION: for an action-mode executor, step 3 ran the
    //    action SYNCHRONOUSLY inside create_kanban_step_thread and the hook
    //    already routed the task through the workflow matrix
    //    (review/blocked/done/testing) via route_step_completion. Never
    //    clobber that routed status back to `running` — the task would sit in
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
            "[kanban/dispatch] task {} already transitioned by action-mode hook (status={}) — leaving as-is",
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
    }"""

with io.open(PATH, "r", encoding="utf-8") as f:
    content = f.read()

if old not in content:
    print("ERROR: old block not found in " + PATH)
    sys.exit(1)

content = content.replace(old, new, 1)
with io.open(PATH, "w", encoding="utf-8") as f:
    f.write(content)
print("OK: kanban_dispatch.rs patched (clobber fix)")
