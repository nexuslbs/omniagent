use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::agent::AgentConfig;
use crate::db::types::{
    CompleteThreadStats, CreateThreadParams, Message, MessageDb, MessageNew, Thread,
    ThreadCauseParams, ThreadDb,
};
use crate::err_msg;
use crate::error::{AppResult, Error};

// ---------------------------------------------------------------------------
// Thread query functions
// ---------------------------------------------------------------------------

/// Create a new thread with status 'created'.
pub async fn create_thread(
    pool: &PgPool,
    cause: &str,
    channel_id: i64,
    profile: &str,
    p: CreateThreadParams,
) -> AppResult<Thread> {
    // Validate cause: must be 'user' or 'system'
    if cause != "user" && cause != "system" {
        err_msg!(
            "Invalid thread cause '{}': must be 'user' or 'system'",
            cause
        );
    }
    let row: ThreadDb = sql_forge!(
        ThreadDb,
        r#"
        INSERT INTO threads (status, cause, channel_id, profile, provider, model, task_id, schedule_task_id, plan, parent_id, workflow_id, workflow_step)
        VALUES ('created', :cause, :channel_id, :profile, NULLIF(:provider, '')::text, NULLIF(:model, '')::text, NULLIF(:task_id, '')::text, NULLIF(:schedule_task_id, '')::text, :plan, NULLIF(:parent_id, -1::bigint)::bigint, NULLIF(:workflow_id, '')::text, NULLIF(:workflow_step, '')::text)
        RETURNING
            id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            ''::text AS "started_at",
            ''::text AS "ended_at",
            terminal,
            plan,
            parent_id,
            iterations,
            workflow_step
        "#,
        ( :cause = cause, :channel_id = channel_id, :profile = profile, :provider = p.provider.as_deref().unwrap_or(""), :model = p.model.as_deref().unwrap_or(""), :task_id = p.task_id.as_deref().unwrap_or(""), :schedule_task_id = p.schedule_task_id.as_deref().unwrap_or(""), :plan = p.plan, :parent_id = p.parent_id.unwrap_or(-1i64), :workflow_id = p.workflow_id.as_deref().unwrap_or(""), :workflow_step = p.workflow_step.as_deref().unwrap_or("") )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
}

/// Set a thread's status to 'system' (terminal: init messages like /start).
/// These threads should never be picked up by the executor.
pub async fn set_thread_system(pool: &PgPool, thread_id: i64) -> AppResult<()> {
    sql_forge!(
        "UPDATE threads SET status = 'system', terminal = true WHERE id = :id",
        ( :id = thread_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Set a thread's status to 'failed' (terminal: action execution failure).
/// These threads should never be picked up by the executor.
#[allow(dead_code)]
pub async fn set_thread_failed(pool: &PgPool, thread_id: i64) -> AppResult<()> {
    sql_forge!(
        "UPDATE threads SET status = 'failed', terminal = true WHERE id = :id",
        ( :id = thread_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Resolve the plan boolean for a thread.
///
/// Priority order (highest first):
/// 1. Task/Cron explicit setting (`task_plan`)
/// 2. Channel setting (`channel_plan`)
/// 3. None (let the plugin decide at runtime)
///
/// Returns `None` when no explicit preference is set: the plugin
/// will decide based on its own config (max chars, keywords, etc.).
pub fn resolve_thread_plan(channel_plan: Option<bool>, task_plan: Option<bool>) -> Option<bool> {
    // 1. Task/Cron explicit setting (highest priority)
    if let Some(val) = task_plan {
        return Some(val);
    }
    // 2. Channel setting
    if let Some(val) = channel_plan {
        return Some(val);
    }
    // 3. None: plugin decides at runtime
    None
}

/// Resolve the max tool-call iterations based on the thread's plan setting.
pub fn max_iterations_for_plan(config: &AgentConfig, plan: bool) -> u32 {
    if plan {
        config.max_iterations_plan
    } else {
        config.max_iterations_no_plan
    }
}

/// Create the seq-0 (cause) message and set the thread to pending in a single transaction.
/// Phase 6 (R3): what a skipped thread (channel closure/deletion, or startup
/// recovery) means for its kanban task.
///
/// The rule is the same on every path: a skipped thread NEVER consumes retry
/// and NEVER moves the task back to todo (the old "return to prior status"
/// behavior is gone). The step is RE-SCHEDULED — a fresh thread is created
/// carrying the same cause, thread_status is set back to 'scheduled', and the
/// kanban status is left unchanged (completed workflow steps are never re-run
/// and never lost).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkipRecovery {
    /// Thread is not linked to a kanban task: mark it skipped, nothing else.
    SkipOnly,
    /// Task is still active (any status except blocked/done): re-schedule it.
    /// `task_status` is preserved unchanged.
    Reschedule { task_status: String },
    /// Task is blocked or done: leave it untouched (no re-schedule, no move).
    Noop,
}

/// Pure decision for the skip → re-schedule rule (R3). There is deliberately
/// NO "move to todo" outcome.
pub(crate) fn skip_recovery(task_id: Option<&str>, task_status: Option<&str>) -> SkipRecovery {
    match (task_id, task_status) {
        (None, _) => SkipRecovery::SkipOnly,
        (Some(_), None) => SkipRecovery::SkipOnly,
        (Some(_), Some(status)) if status == "blocked" || status == "done" => SkipRecovery::Noop,
        (Some(_), Some(status)) => SkipRecovery::Reschedule {
            task_status: status.to_string(),
        },
    }
}
/// Phase 6b: outcome of an explicit operator stop (stop-thread / stop / close)
/// for a kanban-linked thread. Unlike failure/startup recovery (which
/// re-schedules), an explicit stop BLOCKS the task and clears its
/// thread_status - no retry is consumed, no re-run thread is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopRecovery {
    /// Move the kanban task to `blocked` and clear its thread_status.
    Block {
        new_status: &'static str,
        clear_thread_status: bool,
    },
    /// No kanban transition: non-kanban thread, terminal status, or manual
    /// review (thread_status NULL). The thread itself is still skipped.
    Noop,
}

/// Decide what happens to a kanban task when its thread is stopped explicitly
/// (operator stop-thread / stop / close).
///
/// Rules (Phase 6b):
/// - task `running` (11) or `testing` (11a)                  -> Block
/// - task `review` + thread_status scheduled/running (11b)   -> Block
/// - task `review` + thread_status NULL, manual review (11c) -> Noop
/// - task backlog/todo/done/blocked                          -> Noop
/// - no task (task_id NULL, non-kanban thread)               -> Noop (skip only)
pub(crate) fn stop_thread_recovery(
    task_status: Option<&str>,
    thread_status: Option<&str>,
) -> StopRecovery {
    match (task_status, thread_status) {
        (Some("running"), _) | (Some("testing"), _) => StopRecovery::Block {
            new_status: "blocked",
            clear_thread_status: true,
        },
        (Some("review"), Some("scheduled" | "running")) => StopRecovery::Block {
            new_status: "blocked",
            clear_thread_status: true,
        },
        _ => StopRecovery::Noop,
    }
}

pub async fn create_cause_and_set_pending(pool: &PgPool, msg: &MessageNew) -> AppResult<Message> {
    let mut tx = pool.begin().await?;
    let metadata_val: serde_json::Value =
        serde_json::from_str(&msg.metadata.to_string()).unwrap_or_default();
    let saved: MessageDb = sql_forge!(
        MessageDb,
        r#"
        INSERT INTO messages (
            thread_id, role, content, thread_sequence, external_id,
            metadata, embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_number,
            duration_ms, token_usage, channel_id
        )
        VALUES (:thread_id, :role, :content, :thread_sequence, NULLIF(:external_id, '')::text,
            :metadata, NULLIF(:embedding, '')::text, NULLIF(:summary_text, '')::text, :is_summary,
            :msg_type, NULLIF(:msg_subtype, '')::text, :iteration_number,
            :duration_ms, COALESCE(NULLIF(:token_usage, '')::jsonb, '{}'::jsonb),
            (SELECT channel_id FROM threads WHERE id = :thread_id))
        RETURNING
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_number,
            duration_ms, token_usage::text AS "token_usage",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        "#,
        ( :thread_id = msg.thread_id, :role = &msg.role, :content = &msg.content, :thread_sequence = msg.thread_sequence, :external_id = msg.external_id.as_deref().unwrap_or(""), :metadata = &metadata_val, :embedding = msg.embedding.as_deref().unwrap_or(""), :summary_text = msg.summary_text.as_deref().unwrap_or(""), :is_summary = msg.is_summary, :msg_type = &msg.msg_type, :msg_subtype = msg.msg_subtype.as_deref().unwrap_or(""), :iteration_number = msg.iteration_number, :duration_ms = msg.duration_ms, :token_usage = &msg.token_usage.to_string() )
    )
    .fetch_one(&mut *tx)
    .await?;

    // Determine thread status based on channel state
    // If the channel is closed, set to 'skipped' unless the message role is 'system' (for /open etc.)
    let thread_status = {
        let channel_closed: Option<bool> = sql_forge!(
            scalar Option<bool>,
            r#"
            SELECT ch.closed
            FROM channels ch
            JOIN threads t ON t.channel_id = ch.id
            WHERE t.id = :thread_id
            "#,
            ( :thread_id = msg.thread_id )
        )
        .fetch_one(&mut *tx)
        .await?;

        if channel_closed.unwrap_or(false) && msg.role != "system" {
            "skipped"
        } else {
            "pending"
        }
    };

    sql_forge!(
        "UPDATE threads SET status = :status WHERE id = :id AND NOT terminal",
        ( :status = thread_status, :id = msg.thread_id )
    )
    .execute(&mut *tx)
    .await?;

    // R3 (Phase 6): channel closure/deletion is a pre-start/external skip — it
    // NEVER consumes retry and NEVER moves the task back to 'todo' (the old
    // "return to prior status" behavior is replaced by re-scheduling, so
    // completed workflow steps are never re-run). A fresh thread carrying the
    // same cause is created, thread_status is set back to 'scheduled', and the
    // kanban status is left unchanged.
    if thread_status == "skipped" {
        #[derive(sqlx::FromRow)]
        struct SkipRow {
            id: i64,
            cause: Option<String>,
            channel_id: i64,
            profile: Option<String>,
            provider: Option<String>,
            model: Option<String>,
            task_id: Option<String>,
            workflow_id: Option<String>,
            workflow_step: Option<String>,
        }

        let t: Option<SkipRow> = sql_forge!(
            SkipRow,
            r#"
            SELECT id, cause, channel_id, profile, provider, model, task_id, workflow_id, workflow_step
            FROM threads
            WHERE id = :id
            "#,
            ( :id = msg.thread_id )
        )
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(t) = t {
            let task_status: Option<String> = match t.task_id.as_deref() {
                Some(tid) => {
                    sql_forge!(
                        scalar String,
                        "SELECT status FROM kanban_tasks WHERE id = :task_id FOR UPDATE",
                        ( :task_id = tid )
                    )
                    .fetch_optional(&mut *tx)
                    .await?
                }
                None => None,
            };

            match skip_recovery(t.task_id.as_deref(), task_status.as_deref()) {
                SkipRecovery::Reschedule { .. } => {
                    let task_id = t.task_id.as_deref().unwrap_or("");
                    let status = task_status.as_deref().unwrap_or("todo");
                    let reason = "channel closed";
                    #[derive(sqlx::FromRow)]
                    struct IdRow {
                        id: i64,
                    }
                    let new_id = sql_forge!(
                        IdRow,
                        r#"
                        INSERT INTO threads
                            (status, cause, channel_id, profile, provider, model, task_id, parent_id, workflow_id, workflow_step)
                        VALUES
                            ('pending', :cause, :channel_id, :profile, :provider, :model, :task_id, :parent_id, :workflow_id, :workflow_step)
                        RETURNING id
                        "#,
                        (
                            :cause = t.cause.clone().unwrap_or_else(|| "re-run".to_string()),
                            :channel_id = t.channel_id,
                            :profile = t.profile.clone().unwrap_or_else(|| "default".to_string()),
                            :provider = t.provider.clone().unwrap_or_else(|| "openai".to_string()),
                            :model = t.model.clone().unwrap_or_else(|| "gpt-4o-mini".to_string()),
                            :task_id = task_id,
                            :parent_id = t.id,
                            :workflow_id = t.workflow_id.clone().unwrap_or_default(),
                            :workflow_step = t.workflow_step.clone().unwrap_or_default()
                        )
                    )
                    .fetch_one(&mut *tx)
                    .await?;

                    let cause = t.cause.clone().unwrap_or_else(|| "re-run".to_string());
                    sql_forge!(
                        r#"
                        INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
                        VALUES (:tid, 'cause', :content, 0, 'cause')
                        "#,
                        ( :tid = new_id.id, :content = cause )
                    )
                    .execute(&mut *tx)
                    .await?;

                    sql_forge!(
                        "UPDATE kanban_tasks SET thread_status = 'scheduled' WHERE id = :task_id",
                        ( :task_id = task_id )
                    )
                    .execute(&mut *tx)
                    .await?;

                    let comment = format!(
                        "Thread #{} skipped ({}). Creating thread #{}",
                        t.id, reason, new_id.id
                    );
                    sql_forge!(
                        r#"
                        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
                        VALUES (:task_id, 'workflow', :initial, :to_status, :comment)
                        "#,
                        (
                            :task_id = task_id,
                            :initial = status,
                            :to_status = status,
                            :comment = comment.as_str()
                        )
                    )
                    .execute(&mut *tx)
                    .await?;
                }
                SkipRecovery::SkipOnly | SkipRecovery::Noop => {}
            }
        }
    }

    tx.commit().await?;
    saved.try_into()
}

/// Create a thread and its seq-0 cause message in a single operation.
///
/// Resolves the planning mode internally using the prompt content for
/// complexity-based classification (user/cron default). Callers don't
/// need to pass plan or resolve it separately.
///
/// Returns the (Thread, Message) pair.
pub async fn create_thread_with_cause(
    pool: &PgPool,
    data_dir: &str,
    cause: &str,
    channel_id: i64,
    profile: &str,
    p: ThreadCauseParams,
) -> AppResult<(Thread, Message)> {
    // Validate cause: must be 'user' or 'system'
    if cause != "user" && cause != "system" {
        err_msg!(
            "Invalid thread cause '{}': must be 'user' or 'system'",
            cause
        );
    }
    // Validate msg_type: 'user' is no longer valid for seq-0 messages
    if p.msg_type == "user" {
        err_msg!("msg_type 'user' is no longer valid for seq-0 messages: use 'Cause' instead");
    }
    // 1. Get channel for its plan override and current_* fields
    let channel = crate::db::channels::get_channel_by_id(pool, channel_id)
        .await?
        .ok_or_else(|| Error::Message(format!("Channel {} not found", channel_id)))?;

    // 3. Resolve planning mode (internal: lets plugin decide at runtime)
    // Channel-level plan comes from the plan column (if explicitly set) or
    // from metadata (deprecated JSON field for backward compatibility).
    // When neither is set, the prompt plugin decides at runtime.
    // Priority: task_plan > channel.plan (column, if not NULL) > channel.metadata["plan"]
    let channel_plan_from_column: Option<bool> =
        crate::db::channels::get_channel_plan(pool, channel_id).await?;
    let channel_plan_from_metadata = channel.metadata.get("plan").and_then(|v| v.as_bool());
    let channel_plan = channel_plan_from_column.or(channel_plan_from_metadata);
    let plan = resolve_thread_plan(channel_plan, p.task_plan).unwrap_or(false); // false = placeholder, plugin may override at runtime

    // 4. Resolve provider and model
    //
    // Provider chain:  channel.current_provider → profile.provider → LLM_PROVIDER env
    // Model depends on which level the provider came from:
    //   - Channel level:   use channel.current_model, or provider default_model
    //   - Profile level:   use profile.model,         or provider default_model
    //   - Env var level:   always use provider default_model
    //   - Not set:         error: no model to use
    //
    // When explicit p.provider is passed (e.g. from platform client or scheduler),
    // it represents an already-resolved value and takes precedence over the chain.
    // Its accompanying model follows the same rule: p.model or provider default.
    let registry = crate::profile::ProfileRegistry::new(data_dir);
    let profile_data = registry.get(profile);

    let (resolved_provider, resolved_model) = {
        // If the caller already resolved provider+model (cron, platform), use those
        if let Some(prov) = p.provider.as_deref().filter(|s| !s.is_empty()) {
            let model = p
                .model
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .or_else(|| crate::llm::resolve_default_model(prov));
            (prov.to_string(), model)
        }
        // Channel level: provider in channel → use model from channel or provider default
        else if let Some(prov) = channel
            .current_provider
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            let model = channel
                .current_model
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .or_else(|| crate::llm::resolve_default_model(prov));
            (prov.to_string(), model)
        }
        // Profile level: provider in profile → use model from profile or provider default
        else if let Some(prov) =
            profile_data.and_then(|p| p.provider.as_deref().filter(|s| !s.is_empty()))
        {
            let model = profile_data
                .and_then(|p| p.model.as_deref())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .or_else(|| crate::llm::resolve_default_model(prov));
            (prov.to_string(), model)
        }
        // Global config level: default_provider from settings.yml
        else {
            let prov = crate::agent::config::get_global()
                .map(|g| g.read().default_provider.clone())
                .unwrap_or_default(); // Empty string hits the error path below
            if !prov.is_empty() {
                let model = crate::llm::resolve_default_model(&prov);
                (prov, model)
            } else {
                return Err(Error::Message(
                    "No LLM provider configured. Set LLM_PROVIDER env var, or configure a provider in the channel or profile.".to_string()
                ));
            }
        }
    };

    // If model was not resolved at any level, that's an error
    let resolved_model = resolved_model.ok_or_else(|| {
        Error::Message(format!(
            "No model configured for provider '{}'. Set a default_model in the provider plugin config, or specify a model in the channel or profile.",
            resolved_provider
        ))
    })?;

    // 5. Resolve parent_id from parent_external_id
    // If parent_external_id is provided and different from the message's own external_id,
    // look for the thread in this channel whose cause message (seq-0) has that external_id.
    let resolved_parent_id = if let Some(ref parent_ext_id) = p.parent_external_id {
        let same_as_self = p.external_id.as_deref() == Some(parent_ext_id.as_str());
        if !same_as_self && !parent_ext_id.is_empty() {
            #[derive(Debug, sqlx::FromRow)]
            struct ParentRow {
                thread_id: i64,
            }
            let found: Option<ParentRow> = sql_forge!(
                ParentRow,
                r#"
                SELECT m.thread_id
                FROM messages m
                JOIN threads t ON t.id = m.thread_id
                WHERE t.channel_id = :channel_id
                  AND m.external_id = :parent_ext_id
                  AND m.thread_sequence = 0
                LIMIT 1
                "#,
                ( :channel_id = channel_id, :parent_ext_id = parent_ext_id.as_str() )
            )
            .fetch_optional(pool)
            .await?;
            found.map(|f| f.thread_id)
        } else {
            None
        }
    } else {
        None
    };

    // 6. Create the thread (with resolved parent_id, if any)
    let thread = create_thread(
        pool,
        cause,
        channel_id,
        profile,
        CreateThreadParams {
            provider: p.provider.clone().or(Some(resolved_provider.clone())),
            model: p.model.clone().or(Some(resolved_model.clone())),
            task_id: p.task_id.clone(),
            schedule_task_id: p.schedule_task_id.clone(),
            plan,
            parent_id: resolved_parent_id,
            workflow_id: None,
            workflow_step: None,
        },
    )
    .await?;

    // 7. Create the cause (seq-0) message and set thread status
    let msg = MessageNew {
        thread_id: thread.id,
        role: "cause".to_string(),
        content: p.content.clone(),
        thread_sequence: 0,
        external_id: p.external_id,
        metadata: p.metadata,
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: p.msg_type.clone(),
        msg_subtype: p.msg_subtype,
        iteration_number: 0,
        duration_ms: 0,
        token_usage: serde_json::json!({}),
    };

    let saved = create_cause_and_set_pending(pool, &msg).await?;

    Ok((thread, saved))
}

/// Find pending threads for a channel.
pub async fn find_pending_threads_by_channel(
    pool: &PgPool,
    channel_id: i64,
) -> AppResult<Vec<Thread>> {
    let rows: Vec<ThreadDb> = sql_forge!(
        ThreadDb,
        r#"
        SELECT
            id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(started_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "started_at",
            COALESCE(TO_CHAR(ended_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "ended_at",
            terminal,
            plan,
            parent_id,
            iterations,
            workflow_step
        FROM threads
        WHERE channel_id = :channel_id AND status = 'pending'
        ORDER BY created_at ASC
        "#,
        ( :channel_id = channel_id )
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

/// Atomically claim a thread by setting status to 'processing' and started_at to NOW().
/// Returns true if the thread was successfully claimed.
pub async fn claim_thread(pool: &PgPool, thread_id: i64) -> bool {
    let result = sql_forge!(
        "UPDATE threads SET status = 'processing', started_at = NOW() WHERE id = :id AND status = 'pending' AND NOT terminal",
        ( :id = thread_id )
    )
    .execute(pool)
    .await;

    match result {
        Ok(r) => r.rows_affected() > 0,
        Err(e) => {
            tracing::error!("Failed to claim thread {}: {:?}", thread_id, e);
            false
        }
    }
}

/// Complete a thread with final status and usage stats.
pub async fn complete_thread(
    pool: &PgPool,
    thread_id: i64,
    status: &str,
    stats: CompleteThreadStats,
) -> AppResult<()> {
    sql_forge!(
        r#"
        UPDATE threads
        SET status = :status,
            input_tokens = :input_tokens,
            cached_tokens = :cached_tokens,
            output_tokens = :output_tokens,
            duration_ms = :duration_ms,
            ended_at = NOW(),
            iterations = COALESCE(
                (SELECT MAX(iteration_number)
                 FROM messages WHERE thread_id = :id),
                0
            ),
            terminal = true
        WHERE id = :id AND NOT terminal
        "#,
        ( :status = status, :id = thread_id, :input_tokens = stats.input_tokens, :cached_tokens = stats.cached_tokens, :output_tokens = stats.output_tokens, :duration_ms = stats.duration_ms )
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Set all pending/processing threads for a channel to 'skipped'.
pub async fn skip_channel_threads(pool: &PgPool, channel_id: i64) -> AppResult<usize> {
    // Phase 3 (R3): channel closure/deletion re-schedules instead of dropping work.
    // Every pending/processing thread linked to a kanban task is marked skipped and a
    // re-run thread (thread_status='scheduled') is created in the SAME transaction,
    // with the kanban task status UNCHANGED and NO retry consumed. Threads that are
    // not linked to a kanban task are simply marked skipped. Blocked/done tasks never
    // get a re-run thread (R4).
    #[derive(sqlx::FromRow)]
    struct SkipRow {
        id: i64,
        cause: Option<String>,
        channel_id: i64,
        profile: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        task_id: Option<String>,
        workflow_id: Option<String>,
        workflow_step: Option<String>,
    }
    let threads: Vec<SkipRow> = sql_forge!(
        SkipRow,
        "SELECT id, cause, channel_id, profile, provider, model, task_id,
                workflow_id, workflow_step
         FROM threads
         WHERE channel_id = :channel_id AND status IN ('pending', 'processing')
         ORDER BY id",
        ( :channel_id = channel_id )
    )
    .fetch_all(pool)
    .await?;

    for t in &threads {
        let mut tx = pool.begin().await?;
        sql_forge!(
            "UPDATE threads SET status = 'skipped' WHERE id = :id",
            ( :id = t.id )
        )
        .execute(&mut *tx)
        .await?;

        if let Some(ref task_id) = t.task_id {
            #[derive(sqlx::FromRow)]
            struct IdRow {
                id: i64,
            }
            let task_status: Option<String> = sql_forge!(
                scalar String,
                "SELECT status FROM kanban_tasks WHERE id = :task_id FOR UPDATE",
                ( :task_id = task_id )
            )
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(status) = task_status {
                if status != "blocked" && status != "done" {
                    let new_id = sql_forge!(
                        IdRow,
                        "INSERT INTO threads (status, cause, channel_id, profile, provider, model,
                                              task_id, parent_id, workflow_id, workflow_step)
                         VALUES ('pending', :cause, :channel_id, :profile, :provider, :model,
                                 :task_id, :parent_id, :workflow_id, :workflow_step)
                         RETURNING id",
                        (
                            :cause = t.cause.as_deref().unwrap_or(""),
                            :channel_id = t.channel_id,
                            :profile = t.profile.as_deref().unwrap_or(""),
                            :provider = t.provider.as_deref().unwrap_or(""),
                            :model = t.model.as_deref().unwrap_or(""),
                            :task_id = task_id.as_str(),
                            :parent_id = t.id,
                            :workflow_id = t.workflow_id.as_deref().unwrap_or(""),
                            :workflow_step = t.workflow_step.as_deref().unwrap_or("")
                        )
                    )
                    .fetch_one(&mut *tx)
                    .await?;
                    let cause = t.cause.clone().unwrap_or_else(|| "re-run".to_string());
                    sql_forge!(
                        "INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
                         VALUES (:tid, 'cause', :content, 0, 'cause')",
                        ( :tid = new_id.id, :content = cause )
                    )
                    .execute(&mut *tx)
                    .await?;
                    sql_forge!(
                        "UPDATE kanban_tasks SET thread_status = 'scheduled' WHERE id = :task_id",
                        ( :task_id = task_id.as_str() )
                    )
                    .execute(&mut *tx)
                    .await?;
                    let comment = format!(
                        "Thread #{} skipped (channel closed). Creating thread #{}",
                        t.id, new_id.id
                    );
                    sql_forge!(
                        "INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
                         VALUES (:task_id, 'workflow', :initial, :to_status, :comment)",
                        (
                            :task_id = task_id.as_str(),
                            :initial = status.as_str(),
                            :to_status = status.as_str(),
                            :comment = comment.as_str()
                        )
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        tx.commit().await?;
    }
    Ok(threads.len())
}

/// Skip a single pending/processing thread by setting its status to 'skipped'.
pub async fn skip_thread(pool: &PgPool, thread_id: i64) -> AppResult<u64> {
    let result = sql_forge!(
        "UPDATE threads SET status = 'skipped', ended_at = NOW(), terminal = true, iterations = COALESCE((SELECT MAX(iteration_number) FROM messages WHERE thread_id = :id), 0) WHERE id = :id AND status IN ('pending', 'processing')",
        ( :id = thread_id )
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Count messages in a thread.
pub async fn count_thread_messages(pool: &PgPool, thread_id: i64) -> AppResult<i32> {
    let count: Option<i64> = sql_forge!(
        scalar Option<i64>,
        "SELECT COUNT(*) FROM messages WHERE thread_id = :thread_id",
        ( :thread_id = thread_id )
    )
    .fetch_one(pool)
    .await?;

    Ok(count.unwrap_or(0) as i32)
}

/// Get the maximum thread_sequence in a thread (for computing the next sequence).
/// Returns 0 if the thread has no messages.
pub async fn get_max_thread_sequence(pool: &PgPool, thread_id: i64) -> AppResult<i32> {
    let max_seq: Option<i32> = sql_forge!(
        scalar Option<i32>,
        "SELECT MAX(thread_sequence) FROM messages WHERE thread_id = :thread_id",
        ( :thread_id = thread_id )
    )
    .fetch_one(pool)
    .await?;

    Ok(max_seq.unwrap_or(0))
}

/// Skip all pending/processing threads on startup.
pub async fn skip_all_pending_threads(pool: &PgPool) -> AppResult<u64> {
    #[derive(sqlx::FromRow)]
    struct SkipRow {
        id: i64,
        cause: Option<String>,
        channel_id: i64,
        profile: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        task_id: Option<String>,
        workflow_id: Option<String>,
        workflow_step: Option<String>,
    }

    let threads: Vec<SkipRow> = sql_forge!(
        SkipRow,
        r#"
        SELECT id, cause, channel_id, profile, provider, model, task_id, workflow_id, workflow_step
        FROM threads
        WHERE status IN ('pending', 'processing')
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut tx = pool.begin().await?;
    for t in &threads {
        sql_forge!(
            "UPDATE threads SET status = 'skipped', ended_at = now() WHERE id = :id",
            ( :id = t.id )
        )
        .execute(&mut *tx)
        .await?;

        // Phase 6 (R3): re-schedule kanban-linked threads at startup — same rule
        // as channel closure: fresh thread, thread_status = 'scheduled', kanban
        // status unchanged, NO retry consumed, NEVER moved back to todo.
        let task_status: Option<String> = match t.task_id.as_deref() {
            Some(tid) => {
                sql_forge!(
                    scalar String,
                    "SELECT status FROM kanban_tasks WHERE id = :task_id FOR UPDATE",
                    ( :task_id = tid )
                )
                .fetch_optional(&mut *tx)
                .await?
            }
            None => None,
        };

        match skip_recovery(t.task_id.as_deref(), task_status.as_deref()) {
            SkipRecovery::Reschedule { .. } => {
                let task_id = t.task_id.as_deref().unwrap_or("");
                let status = task_status.as_deref().unwrap_or("todo");
                let reason = "startup recovery";
                #[derive(sqlx::FromRow)]
                struct IdRow {
                    id: i64,
                }
                let new_id = sql_forge!(
                        IdRow,
                        r#"
                        INSERT INTO threads
                            (status, cause, channel_id, profile, provider, model, task_id, parent_id, workflow_id, workflow_step)
                        VALUES
                            ('pending', :cause, :channel_id, :profile, :provider, :model, :task_id, :parent_id, :workflow_id, :workflow_step)
                        RETURNING id
                        "#,
                        (
                            :cause = t.cause.clone().unwrap_or_else(|| "re-run".to_string()),
                            :channel_id = t.channel_id,
                            :profile = t.profile.clone().unwrap_or_else(|| "default".to_string()),
                            :provider = t.provider.clone().unwrap_or_else(|| "openai".to_string()),
                            :model = t.model.clone().unwrap_or_else(|| "gpt-4o-mini".to_string()),
                            :task_id = task_id,
                            :parent_id = t.id,
                            :workflow_id = t.workflow_id.clone().unwrap_or_default(),
                            :workflow_step = t.workflow_step.clone().unwrap_or_default()
                        )
                    )
                    .fetch_one(&mut *tx)
                    .await?;

                let cause = t.cause.clone().unwrap_or_else(|| "re-run".to_string());
                sql_forge!(
                    r#"
                        INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
                        VALUES (:tid, 'cause', :content, 0, 'cause')
                        "#,
                    ( :tid = new_id.id, :content = cause )
                )
                .execute(&mut *tx)
                .await?;

                sql_forge!(
                    "UPDATE kanban_tasks SET thread_status = 'scheduled' WHERE id = :task_id",
                    ( :task_id = task_id )
                )
                .execute(&mut *tx)
                .await?;

                let comment = format!(
                    "Thread #{} skipped ({}). Creating thread #{}",
                    t.id, reason, new_id.id
                );
                sql_forge!(
                        r#"
                        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
                        VALUES (:task_id, 'workflow', :initial, :to_status, :comment)
                        "#,
                        (
                            :task_id = task_id,
                            :initial = status,
                            :to_status = status,
                            :comment = comment.as_str()
                        )
                    )
                    .execute(&mut *tx)
                    .await?;
            }
            SkipRecovery::SkipOnly | SkipRecovery::Noop => {}
        }
    }
    tx.commit().await?;
    Ok(threads.len() as u64)
}

/// Get the cause message (first message, role='cause') for a thread.
pub async fn get_cause_message(pool: &PgPool, thread_id: i64) -> AppResult<Option<Message>> {
    let row: Option<MessageDb> = sql_forge!(
        MessageDb,
        r#"
        SELECT
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_number,
            duration_ms, token_usage::text AS "token_usage",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM messages
        WHERE thread_id = :thread_id AND role = 'cause'
        ORDER BY thread_sequence ASC, id ASC
        LIMIT 1
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

/// Get completed seq-0 threads (thread roots) with id > since_id,
/// ordered by id ASC, limited to `limit` rows.
/// Now queries the threads table instead of messages.
/// Get completed seq-0 threads in a channel since a given thread id.
///
/// When `parent_id` is:
/// - `None`: returns ALL completed threads (no parent filter): used by summary generation
/// - `Some(None)`: returns only root threads (parent_id IS NULL): used by context for root threads
/// - `Some(Some(p))`: returns sibling threads (parent_id = p) plus the parent thread itself (id = p)
pub async fn get_completed_seq0_threads_since(
    pool: &PgPool,
    channel_id: i64,
    since_id: i64,
    limit: i64,
    parent_id: Option<Option<i64>>,
) -> AppResult<Vec<ThreadDb>> {
    let rows: Vec<ThreadDb> = match parent_id {
        Some(Some(pid)) => {
            // Reply thread: siblings + parent
            sql_forge!(
                ThreadDb,
                r#"
                SELECT
                    id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
                    input_tokens, cached_tokens, output_tokens, duration_ms,
                    COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
                    COALESCE(TO_CHAR(started_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "started_at",
                    COALESCE(TO_CHAR(ended_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "ended_at",
                    terminal,
                    plan,
                    parent_id,
                    iterations,
                    workflow_step
                FROM threads
                WHERE channel_id = :channel_id
                  AND status = 'completed'
                  AND id > :since_id
                  AND (parent_id = :parent_id OR id = :parent_id)
                ORDER BY id ASC
                LIMIT :limit
                "#,
                ( :channel_id = channel_id, :since_id = since_id, :limit = limit, :parent_id = pid )
            )
            .fetch_all(pool)
            .await?
        }
        Some(None) => {
            // Root thread: only parent-less threads
            sql_forge!(
                ThreadDb,
                r#"
                SELECT
                    id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
                    input_tokens, cached_tokens, output_tokens, duration_ms,
                    COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
                    COALESCE(TO_CHAR(started_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "started_at",
                    COALESCE(TO_CHAR(ended_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "ended_at",
                    terminal,
                    plan,
                    parent_id,
                    iterations,
                    workflow_step
                FROM threads
                WHERE channel_id = :channel_id
                  AND status = 'completed'
                  AND id > :since_id
                  AND parent_id IS NULL
                ORDER BY id ASC
                LIMIT :limit
                "#,
                ( :channel_id = channel_id, :since_id = since_id, :limit = limit )
            )
            .fetch_all(pool)
            .await?
        }
        None => {
            // No parent filter: all threads (used by summary generation)
            sql_forge!(
                ThreadDb,
                r#"
                SELECT
                    id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
                    input_tokens, cached_tokens, output_tokens, duration_ms,
                    COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
                    COALESCE(TO_CHAR(started_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "started_at",
                    COALESCE(TO_CHAR(ended_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "ended_at",
                    terminal,
                    plan,
                    parent_id,
                    iterations,
                    workflow_step
                FROM threads
                WHERE channel_id = :channel_id
                  AND status = 'completed'
                  AND id > :since_id
                ORDER BY id ASC
                LIMIT :limit
                "#,
                ( :channel_id = channel_id, :since_id = since_id, :limit = limit )
            )
            .fetch_all(pool)
            .await?
        }
    };

    Ok(rows)
}

// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// Phase 2 — getters used by the builtin fail-thread tool / finalization guard
// ─────────────────────────────────────────────────────────────────────────────

/// Get a thread row by ID (used by the builtin fail-thread tool to resolve
/// the current thread). Uses runtime sqlx instead of sql_forge! because these
/// queries are not part of the SQLX_OFFLINE query cache (see db/messages.rs
/// for the same pattern).
pub async fn get_thread_by_id(pool: &PgPool, thread_id: i64) -> AppResult<Option<Thread>> {
    let row: Option<ThreadDb> = sqlx::query_as::<_, ThreadDb>(
        r#"
        SELECT
            id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(started_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "started_at",
            COALESCE(TO_CHAR(ended_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "ended_at",
            terminal, plan, parent_id, iterations
        FROM threads
        WHERE id = $1
        "#,
    )
    .bind(thread_id)
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

/// Get the current status of a thread (None if the thread does not exist).
/// Used by handle_response to detect a FAILED state already applied by the
/// builtin fail-thread tool before normal finalization runs.
pub async fn get_thread_status(pool: &PgPool, thread_id: i64) -> AppResult<Option<String>> {
    let status: Option<String> =
        sqlx::query_scalar::<_, String>("SELECT status FROM threads WHERE id = $1")
            .bind(thread_id)
            .fetch_optional(pool)
            .await?;

    Ok(status)
}

/// Get the last message of a thread (highest thread_sequence, then id).
/// Used by handle_response to return the fail-thread tool's Error message
/// as the final thread message.
pub async fn get_last_message(pool: &PgPool, thread_id: i64) -> AppResult<Option<Message>> {
    let row: Option<MessageDb> = sqlx::query_as::<_, MessageDb>(
        r#"
        SELECT
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_number,
            duration_ms, token_usage::text AS "token_usage",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM messages
        WHERE thread_id = $1
        ORDER BY thread_sequence DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(thread_id)
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_closed_with_scheduled_thread_reschedules() {
        // thread_status='scheduled', never started: re-schedule (fresh thread,
        // thread_status back to 'scheduled', kanban status unchanged, no retry).
        match skip_recovery(Some("task-1"), Some("todo")) {
            SkipRecovery::Reschedule { task_status } => {
                assert_eq!(task_status, "todo", "status must stay unchanged");
            }
            other => panic!("expected Reschedule, got {:?}", other),
        }
    }

    #[test]
    fn channel_closed_with_running_thread_reschedules() {
        // thread_status='running', interrupted by channel closure: thread is
        // skipped and the task re-scheduled the same way — no retry consumed,
        // no re-run, never moved to todo.
        match skip_recovery(Some("task-1"), Some("running")) {
            SkipRecovery::Reschedule { task_status } => {
                assert_eq!(task_status, "running", "status must stay unchanged");
            }
            other => panic!("expected Reschedule, got {:?}", other),
        }
    }

    #[test]
    fn skip_never_moves_to_todo_and_never_consumes_retry() {
        // R3: pre-start/external skips never consume retry and never move to
        // todo — the recovery plan has no todo variant and touches no counters.
        for status in ["todo", "running", "testing", "review"] {
            match skip_recovery(Some("task-1"), Some(status)) {
                SkipRecovery::Reschedule { task_status } => {
                    assert_eq!(task_status, status, "status must stay unchanged");
                }
                other => panic!("status {}: expected Reschedule, got {:?}", status, other),
            }
        }
    }

    #[test]
    fn startup_skip_reschedules_instead_of_moving_to_todo() {
        // Same rule at omniagent start: scheduled or running task threads are
        // re-scheduled (fresh thread, thread_status='scheduled', status
        // unchanged) — never moved to todo.
        for status in ["todo", "running"] {
            assert!(
                matches!(
                    skip_recovery(Some("t1"), Some(status)),
                    SkipRecovery::Reschedule { .. }
                ),
                "startup skip of status {} must re-schedule, not move to todo",
                status
            );
        }
    }

    #[test]
    fn blocked_or_done_tasks_are_not_rescheduled() {
        assert_eq!(
            skip_recovery(Some("t1"), Some("blocked")),
            SkipRecovery::Noop
        );
        assert_eq!(skip_recovery(Some("t1"), Some("done")), SkipRecovery::Noop);
    }

    #[test]
    fn non_task_threads_are_only_skipped() {
        assert_eq!(skip_recovery(None, Some("todo")), SkipRecovery::SkipOnly);
        assert_eq!(skip_recovery(None, None), SkipRecovery::SkipOnly);
        assert_eq!(skip_recovery(Some("t1"), None), SkipRecovery::SkipOnly);
    }

    // ---------- Phase 6b: explicit stop/close blocks kanban tasks ----------

    #[test]
    fn stop_running_task_blocks_it() {
        // (11) task `running` -> blocked, thread_status cleared.
        assert_eq!(
            stop_thread_recovery(Some("running"), Some("running")),
            StopRecovery::Block {
                new_status: "blocked",
                clear_thread_status: true,
            }
        );
    }

    #[test]
    fn stop_testing_task_blocks_it() {
        // (11a) task `testing` -> blocked.
        assert_eq!(
            stop_thread_recovery(Some("testing"), Some("scheduled")),
            StopRecovery::Block {
                new_status: "blocked",
                clear_thread_status: true,
            }
        );
    }

    #[test]
    fn stop_review_task_with_active_thread_blocks_it() {
        // (11b) `review` + thread_status scheduled/running -> blocked.
        for ts in ["scheduled", "running"] {
            assert_eq!(
                stop_thread_recovery(Some("review"), Some(ts)),
                StopRecovery::Block {
                    new_status: "blocked",
                    clear_thread_status: true,
                },
                "review + thread_status {:?} must block",
                ts
            );
        }
    }

    #[test]
    fn stop_review_task_without_thread_status_is_noop() {
        // (11c) `review` + thread_status NULL (manual review) -> no-op.
        assert_eq!(
            stop_thread_recovery(Some("review"), None),
            StopRecovery::Noop
        );
    }

    #[test]
    fn stop_terminal_tasks_is_noop() {
        // backlog/todo/done/blocked -> no-op.
        for status in ["backlog", "todo", "done", "blocked"] {
            assert_eq!(
                stop_thread_recovery(Some(status), Some("running")),
                StopRecovery::Noop,
                "task {:?} must not be blocked by an explicit stop",
                status
            );
        }
    }

    #[test]
    fn stop_non_kanban_thread_skips_only() {
        // task_id NULL / missing task -> skip only, no task transition.
        assert_eq!(
            stop_thread_recovery(None, Some("running")),
            StopRecovery::Noop
        );
        assert_eq!(stop_thread_recovery(None, None), StopRecovery::Noop);
    }

    #[test]
    fn stop_channel_with_mixed_threads_applies_per_thread() {
        // Channel-level stop: every pending/processing thread is evaluated
        // independently - a mixed channel ends with the running/testing/
        // review-active tasks blocked and the rest untouched.
        let threads: Vec<(Option<&str>, Option<&str>)> = vec![
            (Some("running"), Some("running")),   // running -> blocked
            (Some("testing"), Some("scheduled")), // testing -> blocked
            (Some("review"), Some("scheduled")),  // review-active -> blocked
            (Some("review"), None),               // manual review -> no-op
            (Some("todo"), Some("running")),      // todo -> no-op
            (None, Some("processing")),           // non-kanban -> skip only
        ];
        let expected: Vec<StopRecovery> = vec![
            StopRecovery::Block {
                new_status: "blocked",
                clear_thread_status: true,
            },
            StopRecovery::Block {
                new_status: "blocked",
                clear_thread_status: true,
            },
            StopRecovery::Block {
                new_status: "blocked",
                clear_thread_status: true,
            },
            StopRecovery::Noop,
            StopRecovery::Noop,
            StopRecovery::Noop,
        ];
        for ((task_status, thread_status), exp) in threads.iter().zip(expected.iter()) {
            assert_eq!(
                &stop_thread_recovery(*task_status, *thread_status),
                exp,
                "stop recovery for task {:?} / thread {:?}",
                task_status,
                thread_status
            );
        }
    }
}
