use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::agent::AgentConfig;
use crate::db::types::{
    CompleteThreadStats, CreateThreadParams, Message, MessageDb, MessageNew, Thread,
    ThreadCauseParams, ThreadDb,
};
use crate::error::{Error, AppResult};
use crate::err_msg;

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
        INSERT INTO threads (status, cause, channel_id, profile, provider, model, task_id, schedule_task_id, planning_mode, parent_id)
        VALUES ('created', :cause, :channel_id, :profile, NULLIF(:provider, '')::text, NULLIF(:model, '')::text, NULLIF(:task_id, '')::text, NULLIF(:schedule_task_id, '')::text, :planning_mode, NULLIF(:parent_id, -1::bigint)::bigint)
        RETURNING
            id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            ''::text AS "started_at",
            ''::text AS "ended_at",
            terminal,
            planning_mode,
            parent_id
        "#,
        ( :cause = cause, :channel_id = channel_id, :profile = profile, :provider = p.provider.as_deref().unwrap_or(""), :model = p.model.as_deref().unwrap_or(""), :task_id = p.task_id.as_deref().unwrap_or(""), :schedule_task_id = p.schedule_task_id.as_deref().unwrap_or(""), :planning_mode = &p.planning_mode, :parent_id = p.parent_id.unwrap_or(-1i64) )
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

/// Internal version that also accepts the
/// prompt content for complexity-based classification (user/cron default).
/// Used internally by [`create_thread_with_cause`] — callers should not need
/// to pass content directly.
fn resolve_thread_planning_mode_with_content(
    channel_planning_mode: &str,
    task_planning_mode: &str,
    msg_type: &str,
    global_planning_mode: &str,
    content: &str,
) -> String {
    // 1. Cron task with explicit mode (highest priority — cron > channel)
    if msg_type == "cron" && !task_planning_mode.is_empty() {
        return resolve_cron_planning_mode(task_planning_mode, global_planning_mode);
    }

    // 2. Channel override
    if !channel_planning_mode.is_empty() {
        return normalize_task_planning_mode(channel_planning_mode);
    }

    // 3. Kanban — always use max plan mode currently enabled
    if msg_type == "kanban" {
        return resolve_max_plan(global_planning_mode);
    }

    // 4. User / Cron default — classify by prompt complexity
    classify_complexity_for_planning(content, msg_type)
}

/// Normalize a planning mode value to one of the canonical values.
fn normalize_task_planning_mode(mode: &str) -> String {
    match mode {
        "never" => "prompt_only".to_string(),
        "always" => "auto_subtasks".to_string(),
        other => other.to_string(),
    }
}

/// Resolve a cron task planning mode to a canonical planning mode value.
/// Supports:
/// - `"no_plan"` → `"prompt_only"` (no planning)
/// - `"simple_plan"` → `"auto_plan"` (single planning step)
/// - `"plan_with_subtasks"` → `"auto_subtasks"` (full subtask decomposition)
/// - `"max_plan"` → max of global mode (backward compat)
/// - anything else → pass through (allows direct values like "auto_subtasks")
fn resolve_cron_planning_mode(task_mode: &str, global_mode: &str) -> String {
    match task_mode {
        "no_plan" => "prompt_only".to_string(),
        "simple_plan" => "auto_plan".to_string(),
        "plan_with_subtasks" => "auto_subtasks".to_string(),
        "max_plan" => resolve_max_plan(global_mode),
        other => normalize_task_planning_mode(other),
    }
}

/// Calculate the maximum plan mode that should be used based on the
/// global PLANNING_MODE setting. Kanban tasks and max_plan cron jobs use this.
fn resolve_max_plan(global_mode: &str) -> String {
    match global_mode {
        "auto_subtasks" | "always" => "auto_subtasks".to_string(),
        "auto_plan" => "auto_plan".to_string(),
        _ => "prompt_only".to_string(),
    }
}

/// Classify a prompt by complexity and return the appropriate planning mode.
///
/// Reads threshold settings from environment variables:
/// - `PLANNING_COMPLEXITY_SIMPLE_MAX_CHARS` (default 60)
/// - `PLANNING_COMPLEXITY_STANDARD_MAX_CHARS` (default 200)
/// - `PLANNING_COMPLEXITY_KEYWORDS` (default comma-separated list)
///
/// Returns one of: "prompt_only", "auto_plan", "auto_subtasks".
fn classify_complexity_for_planning(content: &str, msg_type: &str) -> String {
    use crate::complexity::{classify_complexity, Complexity};

    match classify_complexity(content, msg_type, None) {
        Complexity::Simple => "prompt_only".to_string(),
        Complexity::Standard => "auto_plan".to_string(),
        Complexity::Complex => "auto_subtasks".to_string(),
    }
}

/// Resolve the max tool-call iterations based on the thread's planning mode.
/// These replace the old single MAX_ITERATIONS setting.
pub fn max_iterations_for_planning_mode(config: &AgentConfig, planning_mode: &str) -> u32 {
    match planning_mode {
        "auto_subtasks" | "always" => config.max_iterations_complex_plan,
        "auto_plan" => config.max_iterations_simple_plan,
        _ => config.max_iterations_no_plan, // no plan or complexity-based
    }
}

/// Create the seq-0 (cause) message and set the thread to pending in a single transaction.
pub async fn create_cause_and_set_pending(
    pool: &PgPool,
    msg: &MessageNew,
) -> AppResult<Message> {
    let mut tx = pool.begin().await?;
    let metadata_val: serde_json::Value =
        serde_json::from_str(&msg.metadata.to_string()).unwrap_or_default();
    let token_usage_val: serde_json::Value =
        msg.token_usage.clone().unwrap_or(serde_json::Value::Null);
    let saved: MessageDb = sql_forge!(
        MessageDb,
        r#"
        INSERT INTO messages (
            thread_id, role, content, thread_sequence, external_id,
            metadata, embedding, summary_text, is_summary,
            msg_type, msg_subtype, processing_time_ms, token_usage, iteration_number
        )
        VALUES (:thread_id, :role, :content, :thread_sequence, NULLIF(:external_id, '')::text,
            :metadata, NULLIF(:embedding, '')::text, NULLIF(:summary_text, '')::text, :is_summary,
            :msg_type, NULLIF(:msg_subtype, '')::text, NULLIF(:processing_time_ms, -1)::int, NULLIF(:token_usage, 'null')::jsonb, :iteration_number)
        RETURNING
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype,
            token_usage::text AS "token_usage", processing_time_ms, iteration_number,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        "#,
        ( :thread_id = msg.thread_id, :role = &msg.role, :content = &msg.content, :thread_sequence = msg.thread_sequence, :external_id = msg.external_id.as_deref().unwrap_or(""), :metadata = &metadata_val, :embedding = msg.embedding.as_deref().unwrap_or(""), :summary_text = msg.summary_text.as_deref().unwrap_or(""), :is_summary = msg.is_summary, :msg_type = &msg.msg_type, :msg_subtype = msg.msg_subtype.as_deref().unwrap_or(""), :processing_time_ms = msg.processing_time_ms.unwrap_or(-1), :token_usage = &token_usage_val.to_string(), :iteration_number = msg.iteration_number )
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
    }

    tx.commit().await?;
    saved.try_into()
}

/// Create a thread and its seq-0 cause message in a single operation.
///
/// Resolves the planning mode internally using the prompt content for
/// complexity-based classification (user/cron default). Callers don't
/// need to pass planning_mode or resolve it separately.
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
        err_msg!(
            "msg_type 'user' is no longer valid for seq-0 messages — use 'Cause' instead"
        );
    }
    // 1. Get channel for its planning_mode override and current_* fields
    let channel = crate::db::channels::get_channel_by_id(pool, channel_id)
        .await?
        .ok_or_else(|| Error::Message(format!("Channel {} not found", channel_id)))?;

    // 2. Get global PLANNING_MODE
    let global_mode = std::env::var("PLANNING_MODE").unwrap_or_else(|_| "auto_plan".to_string());

    // 3. Resolve planning mode (internal — uses content for complexity classification)
    let channel_pm = channel
        .metadata
        .get("planning_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let planning_mode = resolve_thread_planning_mode_with_content(
        channel_pm,
        &p.task_planning_mode,
        &p.msg_type,
        &global_mode,
        &p.content,
    );

    // 4. Resolve provider and model when not explicitly provided
    // Chain: explicit param → channel.current_* → profile → env var → provider default
    let resolved_provider = p
        .provider
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            channel
                .current_provider
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            let registry = crate::profile::ProfileRegistry::new(data_dir);
            registry.get(profile).and_then(|prof| {
                prof.provider
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            })
        })
        .unwrap_or_else(|| {
            std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "opencode-go".to_string())
        });

    let resolved_model = p
        .model
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            channel
                .current_model
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            let registry = crate::profile::ProfileRegistry::new(data_dir);
            registry.get(profile).and_then(|prof| {
                prof.model
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            })
        })
        .or_else(|| crate::llm::resolve_default_model(&resolved_provider));

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
            model: p.model.clone().or(resolved_model.clone()),
            task_id: p.task_id.clone(),
            schedule_task_id: p.schedule_task_id.clone(),
            planning_mode: planning_mode.clone(),
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
        processing_time_ms: None,
        token_usage: None,
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
            planning_mode,
            parent_id
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
    _stats: CompleteThreadStats,
) -> AppResult<()> {
    sql_forge!(
        r#"
        UPDATE threads
        SET status = :status,
            input_tokens = COALESCE(
                (SELECT SUM((token_usage->>'prompt_tokens')::int)
                 FROM messages WHERE thread_id = :id AND token_usage IS NOT NULL),
                0
            ),
            cached_tokens = COALESCE(
                (SELECT SUM((token_usage->>'cached_tokens')::int)
                 FROM messages WHERE thread_id = :id AND token_usage IS NOT NULL),
                0
            ),
            output_tokens = COALESCE(
                (SELECT SUM((token_usage->>'completion_tokens')::int)
                 FROM messages WHERE thread_id = :id AND token_usage IS NOT NULL),
                0
            ),
            duration_ms = COALESCE(
                EXTRACT(EPOCH FROM (NOW() - COALESCE(started_at, NOW())))::int * 1000,
                0
            ),
            ended_at = NOW(),
            terminal = true
        WHERE id = :id AND NOT terminal
        "#,
        ( :status = status, :id = thread_id )
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Set all pending/processing threads for a channel to 'skipped'.
pub async fn skip_channel_threads(pool: &PgPool, channel_id: i64) -> AppResult<u64> {
    // Mark all pending/processing threads as skipped
    let result = sql_forge!(
        "UPDATE threads SET status = 'skipped', ended_at = NOW(), terminal = true WHERE channel_id = :channel_id AND status IN ('pending', 'processing') AND NOT terminal",
        ( :channel_id = channel_id )
    )
    .execute(pool)
    .await?;

    // Update associated kanban tasks:
    // - pending (never started) → todo (can be retried)
    // - processing (was started) → blocked (needs investigation)
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
        "UPDATE threads SET status = 'skipped', ended_at = NOW(), terminal = true WHERE id = :id AND status IN ('pending', 'processing')",
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
            token_usage::text AS "token_usage", processing_time_ms,
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
                    planning_mode,
                    parent_id
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
                    planning_mode,
                    parent_id
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
                    planning_mode,
                    parent_id
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
