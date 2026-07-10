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
    // Validate cause — must be 'user' or 'system'
    if cause != "user" && cause != "system" {
        err_msg!(
            "Invalid thread cause '{}': must be 'user' or 'system'",
            cause
        );
    }
    let row: ThreadDb = sql_forge!(
        ThreadDb,
        r#"
        INSERT INTO threads (status, cause, channel_id, profile, provider, model, task_id, schedule_task_id, plan, parent_id)
        VALUES ('created', :cause, :channel_id, :profile, NULLIF(:provider, '')::text, NULLIF(:model, '')::text, NULLIF(:task_id, '')::text, NULLIF(:schedule_task_id, '')::text, :plan, NULLIF(:parent_id, -1::bigint)::bigint)
        RETURNING
            id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            ''::text AS "started_at",
            ''::text AS "ended_at",
            terminal,
            plan,
            parent_id,
            iterations
        "#,
        ( :cause = cause, :channel_id = channel_id, :profile = profile, :provider = p.provider.as_deref().unwrap_or(""), :model = p.model.as_deref().unwrap_or(""), :task_id = p.task_id.as_deref().unwrap_or(""), :schedule_task_id = p.schedule_task_id.as_deref().unwrap_or(""), :plan = p.plan, :parent_id = p.parent_id.unwrap_or(-1i64) )
    )
    .fetch_one(pool)
    .await?;

    row.try_into()
}

/// Set a thread's status to 'system' (terminal — init messages like /start).
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

/// Set a thread's status to 'failed' (terminal — action execution failure).
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
/// Returns `None` when no explicit preference is set — the plugin
/// will decide based on its own config (max chars, keywords, etc.).
pub fn resolve_thread_plan(
    channel_plan: Option<bool>,
    task_plan: Option<bool>,
) -> Option<bool> {
    // 1. Task/Cron explicit setting (highest priority)
    if let Some(val) = task_plan {
        return Some(val);
    }
    // 2. Channel setting
    if let Some(val) = channel_plan {
        return Some(val);
    }
    // 3. None — plugin decides at runtime
    None
}

/// Resolve the max tool-call iterations based on the thread's plan setting.
pub fn max_iterations_for_plan(config: &AgentConfig, plan: bool) -> u32 {
    if plan {
        config.max_iterations_complex_plan
    } else {
        config.max_iterations_no_plan
    }
}

/// Create the seq-0 (cause) message and set the thread to pending in a single transaction.
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
            msg_type, msg_subtype, iteration_number
        )
        VALUES (:thread_id, :role, :content, :thread_sequence, NULLIF(:external_id, '')::text,
            :metadata, NULLIF(:embedding, '')::text, NULLIF(:summary_text, '')::text, :is_summary,
            :msg_type, NULLIF(:msg_subtype, '')::text, :iteration_number)
        RETURNING
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype, iteration_number,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        "#,
        ( :thread_id = msg.thread_id, :role = &msg.role, :content = &msg.content, :thread_sequence = msg.thread_sequence, :external_id = msg.external_id.as_deref().unwrap_or(""), :metadata = &metadata_val, :embedding = msg.embedding.as_deref().unwrap_or(""), :summary_text = msg.summary_text.as_deref().unwrap_or(""), :is_summary = msg.is_summary, :msg_type = &msg.msg_type, :msg_subtype = msg.msg_subtype.as_deref().unwrap_or(""), :iteration_number = msg.iteration_number )
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

    // If the thread was skipped because the channel is closed, move the
    // associated kanban task back to 'todo' so it can be retried later.
    if thread_status == "skipped" {
        let _ = sql_forge!(
            "UPDATE kanban_tasks SET status = 'todo', updated_at = NOW() WHERE id = (
                SELECT task_id FROM threads WHERE id = :tid AND task_id IS NOT NULL
            )",
            ( :tid = msg.thread_id )
        )
        .execute(&mut *tx)
        .await;

        // Record the transition in history
        let _ = sql_forge!(
            r#"
            INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board)
            SELECT task_id, 'moved', 'running', 'todo'
            FROM threads WHERE id = :tid AND task_id IS NOT NULL
            "#,
            ( :tid = msg.thread_id )
        )
        .execute(&mut *tx)
        .await;
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
    // Validate cause — must be 'user' or 'system'
    if cause != "user" && cause != "system" {
        err_msg!(
            "Invalid thread cause '{}': must be 'user' or 'system'",
            cause
        );
    }
    // Validate msg_type — 'user' is no longer valid for seq-0 messages
    if p.msg_type == "user" {
        err_msg!("msg_type 'user' is no longer valid for seq-0 messages — use 'Cause' instead");
    }
    // 1. Get channel for its plan override and current_* fields
    let channel = crate::db::channels::get_channel_by_id(pool, channel_id)
        .await?
        .ok_or_else(|| Error::Message(format!("Channel {} not found", channel_id)))?;

    // 3. Resolve planning mode (internal — lets plugin decide at runtime)
    let channel_plan = channel
        .metadata
        .get("plan")
        .and_then(|v| v.as_bool());
    let plan = resolve_thread_plan(
        channel_plan,
        p.task_plan,
    ).unwrap_or(false); // false = placeholder, plugin may override at runtime

    // 4. Resolve provider and model
    //
    // Provider chain:  channel.current_provider → profile.provider → LLM_PROVIDER env
    // Model depends on which level the provider came from:
    //   - Channel level:   use channel.current_model, or provider default_model
    //   - Profile level:   use profile.model,         or provider default_model
    //   - Env var level:   always use provider default_model
    //   - Not set:         error — no model to use
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
        // Env var level: LLM_PROVIDER → always use provider default_model
        else {
            match std::env::var("LLM_PROVIDER") {
                Ok(prov) if !prov.is_empty() => {
                    let model = crate::llm::resolve_default_model(&prov);
                    (prov, model)
                }
                _ => {
                    return Err(Error::Message(
                        "No LLM provider configured. Set LLM_PROVIDER env var, or configure a provider in the channel or profile.".to_string()
                    ));
                }
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
            plan: plan.clone(),
            parent_id: resolved_parent_id,
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
            iterations
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
pub async fn skip_channel_threads(pool: &PgPool, channel_id: i64) -> AppResult<u64> {
    // Mark all pending/processing threads as skipped
    let result = sql_forge!(
        "UPDATE threads SET status = 'skipped', ended_at = NOW(), terminal = true, iterations = COALESCE((SELECT MAX(iteration_number) FROM messages WHERE thread_id = threads.id), 0) WHERE channel_id = :channel_id AND status IN ('pending', 'processing') AND NOT terminal",
        ( :channel_id = channel_id )
    )
    .execute(pool)
    .await?;

    // Update associated kanban tasks:
    // - pending (never started) → todo (can be retried)
    // - processing (was started) → blocked (needs investigation)
    // Record transitions in history before updating status
    let _ = sql_forge!(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board)
        SELECT kt.id, 'moved', kt.status, 'todo'
        FROM kanban_tasks kt
        JOIN threads t ON t.task_id = kt.id
        WHERE t.channel_id = :ch AND t.status = 'pending' AND kt.status = 'ready'
        "#,
        ( :ch = channel_id )
    )
    .execute(pool)
    .await;

    let _ = sql_forge!(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board)
        SELECT kt.id, 'moved', kt.status, 'blocked'
        FROM kanban_tasks kt
        JOIN threads t ON t.task_id = kt.id
        WHERE t.channel_id = :ch AND t.status = 'processing' AND kt.status = 'running'
        "#,
        ( :ch = channel_id )
    )
    .execute(pool)
    .await;

    // Now perform the status updates
    let _ = sql_forge!(
        "UPDATE kanban_tasks SET status = 'todo', updated_at = NOW() WHERE id IN (
            SELECT task_id FROM threads
            WHERE channel_id = :ch AND task_id IS NOT NULL AND status = 'pending'
        ) AND status = 'ready'",
        ( :ch = channel_id )
    )
    .execute(pool)
    .await;

    let _ = sql_forge!(
        "UPDATE kanban_tasks SET status = 'blocked', updated_at = NOW() WHERE id IN (
            SELECT task_id FROM threads
            WHERE channel_id = :ch AND task_id IS NOT NULL AND status = 'processing'
        ) AND status = 'running'",
        ( :ch = channel_id )
    )
    .execute(pool)
    .await;

    Ok(result.rows_affected())
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
    let result = sql_forge!(
        r#"
        UPDATE threads
        SET status = 'skipped', ended_at = NOW(), terminal = true
        WHERE status IN ('pending', 'processing') AND NOT terminal
        "#
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
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
/// - `None`: returns ALL completed threads (no parent filter) — used by summary generation
/// - `Some(None)`: returns only root threads (parent_id IS NULL) — used by context for root threads
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
                    iterations
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
                    iterations
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
            // No parent filter — all threads (used by summary generation)
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
                    iterations
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
