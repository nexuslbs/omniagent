use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::agent::AgentConfig;
use crate::db::types::{
    CompleteThreadStats, CreateThreadParams, Message, MessageDb, MessageNew, Thread,
    ThreadCauseParams, ThreadDb,
};
use crate::err_msg;
use crate::error::{AppResult, Error};

/// True when a cause-message content is a placeholder that must never be used
/// as a thread prompt: empty/whitespace, or the literal cause-kind strings
/// ('system'/'user') that were historically written into message content
/// instead of the real prompt (threads 240-263). Retry/reschedule/review
/// paths copy the parent's seq-0 cause content; a placeholder there means the
/// parent never carried a real prompt, so callers must fall back.
pub fn is_placeholder_cause_content(content: &str) -> bool {
    let t = content.trim();
    t.is_empty() || t == "system" || t == "user"
}

// ---------------------------------------------------------------------------
// Thread query functions
// ---------------------------------------------------------------------------

/// Create a new thread - THE single INSERT for every thread creation path.
///
/// All thread rows (general message threads, kanban executor threads, workflow
/// step threads, engine re-runs, manual-review re-runs, skip-recovery
/// reschedules) MUST go through this function so the full column set
/// (plan, template, workflow_step, task_type, schedule_task_id, hook_caused)
/// is always persisted. Hand-rolled INSERTs elsewhere have repeatedly drifted:
/// step threads were created without `plan`/`template` (60-iteration no-plan
/// budget, no role guidance - threads 75-78, 82) and `hook_caused` was missed
/// in several paths.
///
/// `executor` accepts either a `&PgPool` or `&mut PgTransaction` (both
/// implement `sqlx::Executor`), so callers inside a transaction keep
/// transactional semantics.
pub async fn create_thread<'e, E>(
    executor: E,
    status: &str,
    cause: &str,
    channel_id: &str,
    profile: &str,
    p: CreateThreadParams,
) -> AppResult<Thread>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    // Validate cause: must be 'user' or 'system'
    if cause != "user" && cause != "system" {
        err_msg!(
            "Invalid thread cause '{}': must be 'user' or 'system'",
            cause
        );
    }
    // Identity invariant: a thread MUST carry a valid persisted
    // profile/provider/model - creation fails instead of inserting empties.
    if profile.trim().is_empty() {
        err_msg!("Cannot create thread: profile is empty");
    }
    if p.provider.as_deref().is_none_or(|s| s.trim().is_empty()) {
        err_msg!("Cannot create thread: provider is empty");
    }
    if p.model.as_deref().is_none_or(|s| s.trim().is_empty()) {
        err_msg!("Cannot create thread: model is empty");
    }
    let row: ThreadDb = sql_forge!(
        ThreadDb,
        r#"
        INSERT INTO threads (status, cause, channel_id, profile, provider, model, task_id, schedule_task_id, plan, parent_id, workflow_id, workflow_step, template, task_type, hook_caused)
        VALUES (:status, :cause, :channel_id, :profile, NULLIF(:provider, '')::text, NULLIF(:model, '')::text, NULLIF(:task_id, '')::text, NULLIF(:schedule_task_id, '')::text, :plan, NULLIF(:parent_id, -1::bigint)::bigint, NULLIF(:workflow_id, '')::text, NULLIF(:workflow_step, '')::text, NULLIF(:template, '')::text, :task_type, :hook_caused)
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
            workflow_step,
            template
        "#,
        ( :status = status, :cause = cause, :channel_id = channel_id, :profile = profile, :provider = p.provider.as_deref().unwrap_or(""), :model = p.model.as_deref().unwrap_or(""), :task_id = p.task_id.as_deref().unwrap_or(""), :schedule_task_id = p.schedule_task_id.as_deref().unwrap_or(""), :plan = p.plan, :parent_id = p.parent_id.unwrap_or(-1i64), :workflow_id = p.workflow_id.as_deref().unwrap_or(""), :workflow_step = p.workflow_step.as_deref().unwrap_or(""), :template = p.template.as_deref().unwrap_or(""), :task_type = p.task_id.as_ref().map(|_| "kanban").unwrap_or(""), :hook_caused = p.hook_caused )
    )
    .fetch_one(executor)
    .await?;

    row.try_into()
}

/// Set a thread's status to 'system' (terminal: init messages like /start).
/// These threads should never be picked up by the executor.
pub async fn set_thread_system(pool: &PgPool, thread_id: i64) -> AppResult<()> {
    // Single choke point: status + ended_at + terminal=true + iterations.
    mark_thread_terminal(pool, thread_id, "system").await?;
    // Event-driven hooks: fire thread_finished (fire-and-forget, isolated).
    crate::hooks::fire_thread_finished(thread_id);
    Ok(())
}

/// Set a thread's status to 'failed' (terminal: action execution failure).
/// These threads should never be picked up by the executor.
#[allow(dead_code)]
pub async fn set_thread_failed(pool: &PgPool, thread_id: i64) -> AppResult<()> {
    // Single choke point: status + ended_at + terminal=true + iterations.
    mark_thread_terminal(pool, thread_id, "failed").await?;
    // Event-driven hooks: fire thread_finished (fire-and-forget, isolated).
    crate::hooks::fire_thread_finished(thread_id);
    Ok(())
}

/// Resolve the plan boolean for a thread.
///
/// Priority order (highest first):
/// 1. Task/Cron explicit setting (`task_plan`)
/// 2. Channel setting (`channel_plan`)
/// 3. Profile setting (`profile_plan` - profiles.yml `plan`)
/// 4. None (let the plugin decide at runtime)
///
/// Returns `None` when no explicit preference is set: the plugin
/// will decide based on its own config (max chars, keywords, etc.).
pub fn resolve_thread_plan(
    channel_plan: Option<bool>,
    task_plan: Option<bool>,
    profile_plan: Option<bool>,
) -> Option<bool> {
    // 1. Task/Cron explicit setting (highest priority)
    if let Some(val) = task_plan {
        return Some(val);
    }
    // 2. Channel setting
    if let Some(val) = channel_plan {
        return Some(val);
    }
    // 3. Profile setting (profiles.yml `plan`)
    if let Some(val) = profile_plan {
        return Some(val);
    }
    // 4. None: plugin decides at runtime
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
/// behavior is gone). The step is RE-SCHEDULED - a fresh thread is created
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

/// Copy a skipped thread's PERSISTED identity onto its re-scheduled
/// replacement (R3/startup recovery). Re-scheduled threads never re-resolve
/// provider/model/profile at runtime - they inherit the parent's creation-time
/// identity. Returns `Err` when the parent lacks any part of the identity:
/// the re-schedule fails instead of fabricating defaults or inserting empties.
pub(crate) fn copied_thread_identity(
    profile: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<(String, String, String), String> {
    let clean = |s: Option<&str>| s.filter(|v| !v.trim().is_empty()).map(str::to_string);
    match (clean(profile), clean(provider), clean(model)) {
        (Some(p), Some(prov), Some(m)) => Ok((p, prov, m)),
        _ => Err(
            "parent thread has no persisted profile/provider/model; refusing to re-schedule with an empty identity"
                .to_string(),
        ),
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
            msg_type, msg_subtype, original_thread_id, iteration_number,
            duration_ms, token_usage, channel_id
        )
        VALUES (:thread_id, :role, :content, :thread_sequence, NULLIF(:external_id, '')::text,
            :metadata, NULLIF(:embedding, '')::text, NULLIF(:summary_text, '')::text, :is_summary,
            :msg_type, NULLIF(:msg_subtype, '')::text, NULLIF(:original_thread_id, -1::bigint)::bigint, :iteration_number,
            :duration_ms, COALESCE(NULLIF(:token_usage, '')::jsonb, '{}'::jsonb),
            (SELECT channel_id FROM threads WHERE id = :thread_id))
        RETURNING
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype, original_thread_id, iteration_number,
            duration_ms, token_usage::text AS "token_usage",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        "#,
        ( :thread_id = msg.thread_id, :role = &msg.role, :content = &msg.content, :thread_sequence = msg.thread_sequence, :external_id = msg.external_id.as_deref().unwrap_or(""), :metadata = &metadata_val, :embedding = msg.embedding.as_deref().unwrap_or(""), :summary_text = msg.summary_text.as_deref().unwrap_or(""), :is_summary = msg.is_summary, :msg_type = &msg.msg_type, :msg_subtype = msg.msg_subtype.as_deref().unwrap_or(""), :original_thread_id = msg.original_thread_id.unwrap_or(-1i64), :iteration_number = msg.iteration_number, :duration_ms = msg.duration_ms, :token_usage = &msg.token_usage.to_string() )
    )
    .fetch_one(&mut *tx)
    .await?;

    // Determine thread status based on channel state
    // If the channel is closed, set to 'skipped' unless the message role is 'system' (for /open etc.)
    let thread_status = {
        let thread_channel: Option<String> = sql_forge!(
            scalar String,
            "SELECT channel_id FROM threads WHERE id = :thread_id",
            ( :thread_id = msg.thread_id )
        )
        .fetch_optional(&mut *tx)
        .await?;
        let channel_closed = if let Some(name) = thread_channel.as_deref() {
            crate::db::channels::is_channel_closed(pool, name)
                .await
                .unwrap_or(false)
        } else {
            false
        };

        if channel_closed && msg.role != "system" {
            "skipped"
        } else {
            "pending"
        }
    };

    // 'pending' is NOT a terminal status: a plain status flip is enough.
    // 'skipped' IS terminal - route it through the single choke point
    // (mark_thread_terminal) so terminal=true is always set with it.
    if thread_status == "skipped" {
        mark_thread_terminal(&mut *tx, msg.thread_id, "skipped").await?;
    } else {
        sql_forge!(
            "UPDATE threads SET status = :status WHERE id = :id AND NOT terminal",
            ( :status = thread_status, :id = msg.thread_id )
        )
        .execute(&mut *tx)
        .await?;
    }

    // R3 (Phase 6): channel closure/deletion is a pre-start/external skip - it
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
            channel_id: String,
            profile: Option<String>,
            provider: Option<String>,
            model: Option<String>,
            task_id: Option<String>,
            workflow_id: Option<String>,
            workflow_step: Option<String>,
            plan: bool,
            template: Option<String>,
        }

        let t: Option<SkipRow> = sql_forge!(
            SkipRow,
            r#"
            SELECT id, cause, channel_id, profile, provider, model, task_id, workflow_id, workflow_step,
                   plan, template
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
                    // Identity invariant: copy the parent's persisted identity;
                    // fail the re-schedule if it is missing (never fabricate).
                    let (profile, provider, model) = copied_thread_identity(
                        t.profile.as_deref(),
                        t.provider.as_deref(),
                        t.model.as_deref(),
                    )
                    .map_err(|e| Error::Message(format!("Thread #{}: {e}", t.id)))?;
                    // Single canonical INSERT (create_thread): the re-scheduled
                    // thread must carry the parent's full execution identity -
                    // including plan + template - or it silently runs with a
                    // no-plan iteration budget and no role guidance.
                    let new_thread = create_thread(
                        &mut *tx,
                        "pending",
                        t.cause.as_deref().unwrap_or("system"),
                        &t.channel_id,
                        &profile,
                        CreateThreadParams {
                            provider: Some(provider.clone()),
                            model: Some(model.clone()),
                            task_id: t.task_id.clone(),
                            schedule_task_id: None,
                            plan: t.plan,
                            parent_id: Some(t.id),
                            workflow_id: t.workflow_id.clone(),
                            workflow_step: t.workflow_step.clone(),
                            template: t.template.clone(),
                            hook_caused: false,
                        },
                    )
                    .await?;
                    let new_id = new_thread.id;

                    // The re-scheduled thread's seq-0 cause message must carry the
                    // PARENT's real prompt (its seq-0 cause message content),
                    // never threads.cause: that column is the 'system'/'user'
                    // cause-kind enum, not content - using it produced literal
                    // 'system' prompts (threads 240-263).
                    #[derive(sqlx::FromRow)]
                    struct CauseContentRow {
                        content: String,
                    }
                    let cause = match sql_forge!(
                            CauseContentRow,
                            "SELECT content FROM messages WHERE thread_id = :tid AND thread_sequence = 0 ORDER BY id LIMIT 1",
                            ( :tid = t.id )
                        )
                        .fetch_optional(&mut *tx)
                        .await?
                        .map(|r| r.content)
                        {
                            Some(c) if !is_placeholder_cause_content(&c) => c,
                            _ => format!("Re-run of thread #{} (channel closed)", t.id),
                        };
                    sql_forge!(
                        r#"
                        INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
                        VALUES (:tid, 'cause', :content, 0, 'cause')
                        "#,
                        ( :tid = new_id, :content = cause )
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
                        t.id, reason, new_id
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

    // Event-driven hooks: fire new_message for the inserted seq-0 message.
    crate::hooks::fire_new_message(msg.thread_id, saved.id);

    saved.try_into()
}

/// Execution identity resolved once at thread creation and persisted on the
/// thread row. Running threads never re-resolve it - the executor consumes
/// the persisted profile/provider/model and fails when they are absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedThreadIdentity {
    pub profile: String,
    pub provider: String,
    pub model: String,
}

/// Resolve the execution identity (profile/provider/model) for a NEW thread.
///
/// Called ONLY at thread creation (never at runtime): the result is persisted
/// and every thread creator shares exactly the same precedence.
///
/// Precedence (highest first):
/// - profile: workflow role → workflow defaults → caller base profile →
///   channel.current_profile → global default profile
/// - provider: workflow role → workflow defaults → explicit caller →
///   channel.current_provider → resolved profile's provider →
///   global default provider
///
/// Channel MUST beat profile: a channel's current_provider/current_model is
/// the operator's explicit per-channel override (e.g. the wf-test channel
/// pins noop/test-tool-caller so tests never hit a real LLM), while the
/// profile's provider is only a default for channels that don't override.
/// - model: resolved at the same tier as the provider (explicit model,
///   profile model, channel model, or the provider's default model)
///
/// Returns `Err` when no profile/provider/model can be resolved - creation
/// must fail rather than persist an empty/invalid identity.
pub fn resolve_thread_identity(
    data_dir: &str,
    base_profile: &str,
    channel: Option<&crate::db::types::Channel>,
    workflow: Option<&crate::workflows::Workflow>,
    step: Option<&str>,
    explicit_provider: Option<&str>,
    explicit_model: Option<&str>,
) -> Result<ResolvedThreadIdentity, String> {
    let role_cfg = step
        .and_then(crate::workflows::role_for_step)
        .and_then(|role| workflow.and_then(|wf| wf.resolve_role(role)));

    // --- Profile ----------------------------------------------------------
    let profile = role_cfg
        .as_ref()
        .and_then(|r| r.profile.as_deref())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            workflow
                .and_then(|wf| wf.defaults.profile.as_deref())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| (!base_profile.is_empty()).then_some(base_profile))
        .or_else(|| {
            channel
                .and_then(|c| (!c.current_profile.is_empty()).then_some(c.current_profile.as_str()))
        })
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "no LLM profile configured: set a profile on the task, channel, or workflow".to_string()
        })?;

    // --- Provider (model resolved at the same tier as the provider) -------
    let registry = crate::profile::ProfileRegistry::new(data_dir);
    let profile_data = registry.get(&profile).cloned();

    let (provider, model) = if let Some(prov) = role_cfg
        .as_ref()
        .and_then(|r| r.provider.as_deref())
        .filter(|s| !s.is_empty())
    {
        let model = role_cfg
            .as_ref()
            .and_then(|r| r.model.as_deref())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| crate::llm::resolve_default_model(prov));
        (prov.to_string(), model)
    } else if let Some(prov) = workflow
        .and_then(|wf| wf.defaults.provider.as_deref())
        .filter(|s| !s.is_empty())
    {
        let model = workflow
            .and_then(|wf| wf.defaults.model.as_deref())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| crate::llm::resolve_default_model(prov));
        (prov.to_string(), model)
    } else if let Some(prov) = explicit_provider.filter(|s| !s.is_empty()) {
        let model = explicit_model
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| crate::llm::resolve_default_model(prov));
        (prov.to_string(), model)
    } else if let Some(prov) = channel
        .and_then(|c| c.current_provider.as_deref())
        .filter(|s| !s.is_empty())
    {
        let model = channel
            .and_then(|c| c.current_model.as_deref())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| crate::llm::resolve_default_model(prov));
        (prov.to_string(), model)
    } else if let Some(prov) = profile_data
        .as_ref()
        .and_then(|p| p.provider.as_deref())
        .filter(|s| !s.is_empty())
    {
        let model = profile_data
            .as_ref()
            .and_then(|p| p.model.as_deref())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| crate::llm::resolve_default_model(prov));
        (prov.to_string(), model)
    } else {
        // Global config level: default_provider from settings.yml / env.
        let prov = crate::agent::config::get_global()
            .map(|g| g.read().default_provider.clone())
            .unwrap_or_default();
        if prov.is_empty() {
            return Err(
                "No LLM provider configured. Set LLM_PROVIDER env var, or configure a provider in the channel or profile.".to_string(),
            );
        }
        let model = crate::llm::resolve_default_model(&prov);
        (prov, model)
    };

    let model = model.ok_or_else(|| {
        format!(
            "No model configured for provider '{}'. Set a default_model in the provider plugin config, or specify a model in the channel or profile.",
            provider
        )
    })?;

    Ok(ResolvedThreadIdentity {
        profile,
        provider,
        model,
    })
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
    channel_id: &str,
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
    // 1. Get channel for its plan override and current_* fields. A thread
    // with no channel (empty channel_id - the explicit -> default -> ''
    // resolution chain came up empty) is still CREATED so the record
    // persists for audit; it is then marked failed with "no channel
    // defined" below. Unknown channel names are treated the same way.
    let channel = if channel_id.trim().is_empty() {
        None
    } else {
        crate::db::channels::get_channel_by_id(pool, channel_id).await?
    };

    // 3. Resolve planning mode (internal: lets plugin decide at runtime)
    // Channel-level plan comes from the channels.yml `plan` field (single
    // bool). When unset, the prompt plugin decides at runtime.
    // Priority: task_plan > channel.plan (yml `plan` bool).
    let channel_plan_from_column: Option<bool> =
        crate::db::channels::get_channel_plan(pool, channel_id).await?;
    let channel_plan = channel_plan_from_column;
    let profile_plan = crate::profile::ProfileRegistry::new(data_dir)
        .get(profile)
        .and_then(|p| p.plan);
    let plan = resolve_thread_plan(channel_plan, p.task_plan, profile_plan).unwrap_or(false); // false = placeholder, plugin may override at runtime

    // 4. Resolve provider/model/profile once, at thread creation.
    // Workflow role overrides are applied here so every thread creator shares
    // exactly the same precedence and running threads never re-resolve them.
    // A missing workflows.yml simply means "no workflow"; a parse/validation
    // error is propagated (never silently swallowed).
    let workflow = match p.workflow_id.as_deref() {
        Some(id) => {
            let path = crate::config_path::config_path(data_dir, "workflows.yml");
            match crate::workflows::WorkflowsFile::load(&path) {
                Ok(file) => file.workflows.get(id).cloned(),
                Err(crate::workflows::WorkflowConfigError::NotFound { .. }) => None,
                Err(e) => return Err(Error::Message(format!("failed to load workflows.yml: {e}"))),
            }
        }
        None => None,
    };
    let identity = resolve_thread_identity(
        data_dir,
        profile,
        channel.as_ref(),
        workflow.as_ref(),
        p.workflow_step.as_deref(),
        p.provider.as_deref(),
        p.model.as_deref(),
    )
    .map_err(Error::Message)?;
    let resolved_profile = identity.profile;
    let resolved_provider = identity.provider;
    let resolved_model = identity.model;

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
        "created",
        cause,
        channel_id,
        &resolved_profile,
        CreateThreadParams {
            provider: Some(resolved_provider.clone()),
            model: Some(resolved_model.clone()),
            task_id: p.task_id.clone(),
            schedule_task_id: p.schedule_task_id.clone(),
            plan,
            parent_id: resolved_parent_id,
            template: p.template.clone(),
            workflow_id: p.workflow_id.clone(),
            workflow_step: p.workflow_step.clone(),
            hook_caused: p.hook_caused,
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
        original_thread_id: None,
        msg_type: p.msg_type.clone(),
        msg_subtype: p.msg_subtype,
        iteration_number: 0,
        duration_ms: 0,
        token_usage: serde_json::json!({}),
    };

    let saved = create_cause_and_set_pending(pool, &msg).await?;

    // 8. Fail-with-record: a thread with no channel cannot be executed.
    //    The thread row (status='failed', terminal=true) and its cause
    //    message persist for audit; thread_started hooks do not fire for
    //    a doomed thread.
    if channel.is_none() {
        set_thread_failed(pool, thread.id).await?;
        return Err(Error::Message("no channel defined".to_string()));
    }

    // Event-driven hooks: fire thread_started (fire-and-forget, isolated).
    crate::hooks::fire_thread_started(thread.id);

    Ok((thread, saved))
}

/// Find pending threads for a channel.
pub async fn find_pending_threads_by_channel(
    pool: &PgPool,
    channel_id: &str,
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
            workflow_step,
            template
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
///
/// Token stats come from the caller's `stats` (the thread's actual cumulative
/// usage) when nonzero; otherwise they fall back to aggregating the thread's
/// `messages.token_usage` JSONB, so failure/interrupt paths without live usage
/// still persist real values. Non-agentic threads (no LLM calls) end at 0,
/// never NULL. `iterations` is set to the real LLM call count (the highest
/// message `iteration_number`).
pub async fn complete_thread(
    pool: &PgPool,
    thread_id: i64,
    status: &str,
    stats: CompleteThreadStats,
) -> AppResult<()> {
    sql_forge!(
        r#"        UPDATE threads t
            SET status = :status,
                input_tokens = CASE WHEN :input_tokens > 0 THEN :input_tokens
                                    ELSE usage_agg.input_tokens END,
                cached_tokens = CASE WHEN :cached_tokens > 0 THEN :cached_tokens
                                     ELSE usage_agg.cached_tokens END,
                output_tokens = CASE WHEN :output_tokens > 0 THEN :output_tokens
                                     ELSE usage_agg.output_tokens END,
                duration_ms = :duration_ms,
                ended_at = NOW(),
                iterations = COALESCE(
                    (SELECT MAX(iteration_number)
                     FROM messages WHERE thread_id = :id),
                    0
                ),
                terminal = true
            FROM (
                SELECT
                    COALESCE(SUM(COALESCE((m.token_usage->>'prompt_tokens')::bigint, 0)), 0)::int AS input_tokens,
                    COALESCE(SUM(COALESCE((m.token_usage->>'cached_tokens')::bigint, 0)), 0)::int AS cached_tokens,
                    COALESCE(SUM(COALESCE((m.token_usage->>'completion_tokens')::bigint, 0)), 0)::int AS output_tokens
                FROM messages m
                WHERE m.thread_id = :id AND m.msg_type <> 'error'
            ) usage_agg
            WHERE t.id = :id AND NOT t.terminal"#,
        ( :status = status, :id = thread_id, :input_tokens = stats.input_tokens, :cached_tokens = stats.cached_tokens, :output_tokens = stats.output_tokens, :duration_ms = stats.duration_ms )
    )
    .execute(pool)
    .await?;

    // Event-driven hooks: fire thread_finished on terminal transition.
    crate::hooks::fire_thread_finished(thread_id);

    Ok(())
}

/// Aggregate the thread's real token usage from its messages' token_usage
/// (prompt/cached/completion sums), excluding terminal error messages (which
/// carry the same aggregate for message-level UI and must not be
/// double-counted). Used by fail paths that lack a live cumulative usage so
/// the final error message still shows the tokens already spent.
pub async fn aggregate_thread_token_usage(
    pool: &PgPool,
    thread_id: i64,
) -> AppResult<(i32, i32, i32)> {
    #[derive(sqlx::FromRow)]
    struct UsageAggRow {
        input_tokens: Option<i64>,
        cached_tokens: Option<i64>,
        output_tokens: Option<i64>,
    }
    let row: UsageAggRow = sql_forge!(
            UsageAggRow,
            r#"
            SELECT
                COALESCE(SUM(COALESCE((token_usage->>'prompt_tokens')::bigint, 0)), 0)::bigint AS input_tokens,
                COALESCE(SUM(COALESCE((token_usage->>'cached_tokens')::bigint, 0)), 0)::bigint AS cached_tokens,
                COALESCE(SUM(COALESCE((token_usage->>'completion_tokens')::bigint, 0)), 0)::bigint AS output_tokens
            FROM messages
            WHERE thread_id = :thread_id AND msg_type <> 'error'
            "#,
            ( :thread_id = thread_id )
        )
        .fetch_one(pool)
        .await?;
    Ok((
        row.input_tokens.unwrap_or(0) as i32,
        row.cached_tokens.unwrap_or(0) as i32,
        row.output_tokens.unwrap_or(0) as i32,
    ))
}

/// Set all pending/processing threads for a channel to 'skipped'.
pub async fn skip_channel_threads(pool: &PgPool, channel_id: &str) -> AppResult<usize> {
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
        channel_id: String,
        profile: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        task_id: Option<String>,
        workflow_id: Option<String>,
        workflow_step: Option<String>,
        plan: bool,
        template: Option<String>,
    }
    let threads: Vec<SkipRow> = sql_forge!(
        SkipRow,
        "SELECT id, cause, channel_id, profile, provider, model, task_id,
                workflow_id, workflow_step, plan, template
         FROM threads
         WHERE channel_id = :channel_id AND status IN ('pending', 'processing')
         ORDER BY id",
        ( :channel_id = channel_id )
    )
    .fetch_all(pool)
    .await?;

    for t in &threads {
        let mut tx = pool.begin().await?;
        // Terminal write: single choke point sets terminal=true with 'skipped'.
        mark_thread_terminal(&mut *tx, t.id, "skipped").await?;

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
                    // Single canonical INSERT (create_thread): carries the
                    // parent's plan + template so a re-scheduled tester/reviewer
                    // keeps its iteration budget and role guidance.
                    let new_thread = create_thread(
                        &mut *tx,
                        "pending",
                        t.cause.as_deref().unwrap_or("system"),
                        &t.channel_id,
                        t.profile.as_deref().unwrap_or(""),
                        CreateThreadParams {
                            provider: t.provider.clone(),
                            model: t.model.clone(),
                            task_id: t.task_id.clone(),
                            schedule_task_id: None,
                            plan: t.plan,
                            parent_id: Some(t.id),
                            workflow_id: t.workflow_id.clone(),
                            workflow_step: t.workflow_step.clone(),
                            template: t.template.clone(),
                            hook_caused: false,
                        },
                    )
                    .await?;
                    let new_id = new_thread.id;
                    // The re-scheduled thread's seq-0 cause message must carry the
                    // PARENT's real prompt (its seq-0 cause message content),
                    // never threads.cause: that column is the 'system'/'user'
                    // cause-kind enum, not content - using it produced literal
                    // 'system' prompts (threads 240-263).
                    #[derive(sqlx::FromRow)]
                    struct CauseContentRow {
                        content: String,
                    }
                    let cause = match sql_forge!(
                            CauseContentRow,
                            "SELECT content FROM messages WHERE thread_id = :tid AND thread_sequence = 0 ORDER BY id LIMIT 1",
                            ( :tid = t.id )
                        )
                        .fetch_optional(&mut *tx)
                        .await?
                        .map(|r| r.content)
                        {
                            Some(c) if !is_placeholder_cause_content(&c) => c,
                            _ => format!("Re-run of thread #{} (channel closed)", t.id),
                        };
                    sql_forge!(
                        "INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
                         VALUES (:tid, 'cause', :content, 0, 'cause')",
                        ( :tid = new_id, :content = cause )
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
                        t.id, new_id
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

/// Incrementally update a RUNNING thread's usage stats (iterations, tokens,
/// elapsed time) after every LLM call, so processing threads show live,
/// up-to-date values (e.g. via the thread retrieval API) while they execute.
///
/// Cheap single-row UPDATE (no subqueries). This is purely additive to the
/// terminal-state write: `complete_thread` / `mark_thread_terminal` remain the
/// final word, and the `NOT terminal` guard means a row that has already been
/// completed/failed/cancelled is never re-touched (no race with the final
/// write, which uses the same guard).
pub async fn update_thread_progress(
    pool: &PgPool,
    thread_id: i64,
    iterations: i32,
    stats: CompleteThreadStats,
) -> AppResult<u64> {
    let result = sql_forge!(
        r#"
        UPDATE threads
        SET iterations = :iterations,
            input_tokens = :input_tokens,
            cached_tokens = :cached_tokens,
            output_tokens = :output_tokens,
            duration_ms = :duration_ms
        WHERE id = :id AND NOT terminal
        "#,
        ( :iterations = iterations, :id = thread_id, :input_tokens = stats.input_tokens, :cached_tokens = stats.cached_tokens, :output_tokens = stats.output_tokens, :duration_ms = stats.duration_ms )
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Mark a thread terminal: set status + ended_at + terminal=true + iterations.
///
/// THE single choke point for every write that flips a thread into a terminal
/// status ('skipped' / 'failed' / 'interrupted' / 'completed' / 'system'). The
/// DB CHECK constraint `chk_thread_terminal_status` enforces the same
/// invariant structurally: a terminal-status row MUST have terminal=true, so a
/// skipped/completed/failed thread can never look like active work to code
/// checking `terminal` (e.g. a dispatch gate `WHERE terminal = false`).
///
/// `executor` accepts either a `&PgPool` or a `&mut PgTransaction` (both
/// implement `sqlx::Executor`), so single-thread writes and batch loops
/// (channel-wide skips, startup recovery, closed-channel skips) all funnel
/// through the same statement. Only `NOT terminal` rows are touched: an
/// already-terminal thread is never re-flipped.
pub async fn mark_thread_terminal<'e, E>(
    executor: E,
    thread_id: i64,
    status: &str,
) -> AppResult<u64>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sql_forge!(
        r#"        UPDATE threads t
            SET status = :status,
                ended_at = NOW(),
                input_tokens = usage_agg.input_tokens,
                cached_tokens = usage_agg.cached_tokens,
                output_tokens = usage_agg.output_tokens,
                iterations = COALESCE(
                    (SELECT MAX(iteration_number) FROM messages WHERE thread_id = :id),
                    0
                ),
                terminal = true
            FROM (
                SELECT
                    COALESCE(SUM(COALESCE((m.token_usage->>'prompt_tokens')::bigint, 0)), 0)::int AS input_tokens,
                    COALESCE(SUM(COALESCE((m.token_usage->>'cached_tokens')::bigint, 0)), 0)::int AS cached_tokens,
                    COALESCE(SUM(COALESCE((m.token_usage->>'completion_tokens')::bigint, 0)), 0)::int AS output_tokens
                FROM messages m
                WHERE m.thread_id = :id AND m.msg_type <> 'error'
            ) usage_agg
            WHERE t.id = :id AND NOT t.terminal"#,
        ( :status = status, :id = thread_id )
    )
    .execute(executor)
    .await?;

    Ok(result.rows_affected())
}

/// Skip a single pending/processing thread by setting its status to 'skipped'.
pub async fn skip_thread(pool: &PgPool, thread_id: i64) -> AppResult<u64> {
    // Single choke point: status + ended_at + terminal=true + iterations.
    let result = mark_thread_terminal(pool, thread_id, "skipped").await?;

    if result > 0 {
        // Event-driven hooks: fire thread_finished on terminal transition.
        crate::hooks::fire_thread_finished(thread_id);
    }

    Ok(result)
}

/// Stop a kanban task's old-status threads when the task's status changes.
///
/// Marks terminal 'skipped' every pending/processing thread of `task_id`
/// whose workflow step does NOT serve `new_status` (threads tied to a
/// status the task has LEFT). A thread serves a status when its
/// `workflow_step` equals that status; legacy kanban threads with a NULL
/// or empty step are treated as serving `running` (same mapping as the
/// supervisor pickup in src/agent/mod.rs). Callers invoke this AFTER the
/// task's status update on every status-change path (UI move, API status
/// update, workflow-driven transitions) so an old-status thread can never
/// keep running against a task that moved away from it.
///
/// Same marker semantics as the existing skip paths: single choke point
/// `mark_thread_terminal(..., "skipped")`; no hook is fired (the caller
/// owns the transition - firing thread_finished here could double-route
/// the workflow). Returns the number of threads skipped.
pub(crate) async fn skip_stale_threads_for_status(
    pool: &PgPool,
    task_id: &str,
    new_status: &str,
) -> AppResult<u64> {
    #[derive(sqlx::FromRow)]
    struct ActiveStepRow {
        id: i64,
        workflow_step: Option<String>,
    }
    let active: Vec<ActiveStepRow> = sql_forge!(
        ActiveStepRow,
        r#"SELECT id, workflow_step FROM threads
           WHERE task_id = :task_id AND status IN ('pending', 'processing')"#,
        ( :task_id = task_id )
    )
    .fetch_all(pool)
    .await?;

    let mut skipped: u64 = 0;
    for t in &active {
        // Effective step of the thread: NULL/empty legacy threads serve
        // 'running' (same mapping as the supervisor pickup).
        let step = t
            .workflow_step
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("running");
        if step == new_status {
            // Thread already serves the task's new status: keep it running.
            continue;
        }
        let n = mark_thread_terminal(pool, t.id, "skipped").await?;
        if n == 0 {
            continue;
        }
        skipped += 1;
        // Audit the skip in kanban history (best-effort, like the existing
        // status-change dispatch skip).
        let comment = format!(
            "Thread #{} skipped (task status changed to '{}')",
            t.id, new_status
        );
        let _ = sql_forge!(
            r#"
            INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
            VALUES (:task_id, 'workflow', :to_status, :to_status, :comment)
            "#,
            ( :task_id = task_id, :to_status = new_status, :comment = comment.as_str() )
        )
        .execute(pool)
        .await
        .map_err(|e| {
            tracing::warn!(
                "[kanban] history insert for skipped thread #{} failed: {:?}",
                t.id,
                e
            )
        });
    }
    Ok(skipped)
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

// ---------------------------------------------------------------------------
// Kanban status-change dispatch / redispatch
// ---------------------------------------------------------------------------

/// Gate: does `status` map to a runnable workflow role for this task?
/// `running` always runs (plain executor path even without a workflow);
/// `testing`/`review` only when the task has a workflow that defines the
/// role. Other statuses never dispatch.
pub(crate) fn kanban_step_actionable(
    status: &str,
    workflow_id: Option<&str>,
    has_role: bool,
) -> bool {
    match status {
        "running" => true,
        "testing" | "review" => workflow_id.is_some() && has_role,
        _ => false,
    }
}

/// R8-J: an executor thread must ALWAYS carry a template - the role template
/// wins; for the running step the fallback chain is task.template ->
/// channel.template -> "dev-development" (never None). Step threads
/// (testing/review) carry their role template only (required by workflow
/// validation); without one they stay None exactly like kanban_updater's
/// step-thread creation.
fn resolve_kanban_thread_template(
    role_template: Option<String>,
    is_running: bool,
    task_template: Option<&str>,
    channel_template: Option<&str>,
    profile_template: Option<&str>,
) -> Option<String> {
    role_template.or_else(|| {
        is_running.then(|| {
            task_template
                .filter(|t| !t.is_empty())
                .or_else(|| channel_template.filter(|t| !t.is_empty()))
                .or_else(|| profile_template.filter(|t| !t.is_empty()))
                .unwrap_or("dev-development")
                .to_string()
        })
    })
}

/// Build the thread body from a task's title and body (body may be empty).
fn kanban_thread_content(title: &str, body: Option<&str>) -> String {
    match body.map(str::trim).filter(|b| !b.is_empty()) {
        Some(body) => format!("{title}\n\n{body}"),
        None => title.to_string(),
    }
}

/// Core of kanban status-change dispatch and `/redispatch`: create the
/// workflow-role thread for `status` on `task_id` and mark the task's
/// `thread_status` as 'scheduled'. The task's OWN status is NEVER changed
/// here - the caller owns the status transition.
///
/// Returns `Some(thread_id)` when a thread was created, `None` when the
/// status has no role to run (non-workflow `testing`/`review`, a workflow
/// without that role, or a non-workflow-column status).
///
/// Task row used by kanban status-change dispatch / redispatch / startup
/// redispatch. The `board` column participates in the board-gated thread
/// creation (src/boards.rs).
#[derive(sqlx::FromRow)]
struct KanbanDispatchRow {
    id: String,
    title: String,
    body: Option<String>,
    status: String,
    archived: Option<bool>,
    channel_id: Option<String>,
    profile: Option<String>,
    template: Option<String>,
    plan: Option<bool>,
    workflow_id: Option<String>,
    board: Option<String>,
}

pub(crate) async fn create_kanban_step_thread(
    pool: &PgPool,
    data_dir: &str,
    task_id: &str,
    status: &str,
) -> AppResult<Option<i64>> {
    // 1. Load the task detail.
    let task = match sql_forge!(
        KanbanDispatchRow,
        r#"
        SELECT id, title, body, status, archived, channel_id, profile, template, plan, workflow_id, board
        FROM kanban_tasks WHERE id = :task_id
        "#,
        ( :task_id = task_id )
    )
    .fetch_optional(pool)
    .await?
    {
        Some(t) => t,
        None => return Ok(None),
    };

    // 1a. ARCHIVED GATE: an archived task is never dispatched, regardless of
    //     status. PATCH `archived:true` only flips the flag (it does NOT move
    //     the status), so without this gate a status-change dispatch,
    //     /redispatch or startup redispatch would still create a thread for
    //     an archived task. The auto-dispatcher additionally excludes
    //     archived tasks in its scan SQL (src/kanban_dispatch.rs); this gate
    //     backstops every other dispatch path.
    if task.archived.unwrap_or(false) {
        return Ok(None);
    }

    // 1b. Resolve the task's effective defaults ONCE at load (task → board →
    //     channel → global settings) - the universal resolution pattern. The
    //     board gate is part of the resolution: boards.yml present + invalid
    //     board (NULL or not in the file) fails LOUD here, and the thread is
    //     created and IMMEDIATELY terminated as 'failed' with a clear Error
    //     message (mirrors the no-channel failure path in
    //     create_thread_with_cause).
    let resolved = match crate::resolution::resolve_task_defaults(
        data_dir,
        &crate::resolution::TaskFallbackFields {
            board: task.board.as_deref(),
            workflow_id: task.workflow_id.as_deref(),
            channel_id: task.channel_id.as_deref(),
            profile: task.profile.as_deref(),
            plan: task.plan,
            template: task.template.as_deref(),
        },
    ) {
        Ok(r) => r,
        Err(board_err) => {
            fail_kanban_thread_no_board(pool, data_dir, &task, status, &board_err).await?;
            return Ok(None);
        }
    };

    // 2. Resolve the workflow role config and gate on role availability.
    //    Workflow: resolved (task → board), then the role config.
    let role = crate::workflows::role_for_step(status);
    let workflow_id = resolved.workflow_id.clone();
    let workflow = workflow_id.as_deref().and_then(|wf_id| {
        let path = crate::config_path::config_path(data_dir, "workflows.yml");
        crate::workflows::WorkflowsFile::load(&path)
            .ok()
            .and_then(|f| f.workflows.get(wf_id).cloned())
    });
    let role_cfg = role.and_then(|r| workflow.as_ref().and_then(|wf| wf.resolve_role(r)));
    if !kanban_step_actionable(status, workflow_id.as_deref(), role_cfg.is_some()) {
        return Ok(None);
    }

    // 4. Effective channel/profile/plan come from the resolved task defaults
    //    (task → board → channel → global settings), computed once at load.
    let channel_id = resolved.channel_id.clone();
    let channel = if channel_id.trim().is_empty() {
        None
    } else {
        crate::db::channels::get_channel_by_id(pool, &channel_id).await?
    };
    let effective_profile = resolved.profile.clone();
    // Plan budget: role plan_mode ('on'/'off') wins; fall back to the
    // workflow defaults, then the resolved task/board plan.
    let plan = role_cfg
        .as_ref()
        .and_then(|r| r.plan_mode.as_deref())
        .or_else(|| {
            workflow
                .as_ref()
                .and_then(|wf| wf.defaults.plan_mode.as_deref())
        })
        .map(|mode| matches!(mode, "on"))
        .or(resolved.plan);
    let profile_template = crate::profile::ProfileRegistry::new(data_dir)
        .get(&effective_profile)
        .and_then(|p| p.template.clone());
    let resolved_template = resolve_kanban_thread_template(
        role_cfg.as_ref().and_then(|r| r.template.clone()),
        status == "running",
        resolved.template.as_deref(),
        channel.as_ref().and_then(|c| c.template.as_deref()),
        profile_template.as_deref(),
    );

    // 4b. ACTION-MODE hook: when the resolved role declares `mode: action`,
    //     execute the actions.yml tool via the plugin manager INSTEAD of
    //     spawning the agent loop (mirrors hooks/schedule action modes).
    //     The action thread is created TERMINAL (system on success / failed
    //     on error) and the task routed through the workflow matrix
    //     (kanban_updater::route_step_completion).
    if let Some(role_key) = role {
        let is_action = role_cfg
            .as_ref()
            .map(|r| r.effective_mode() == crate::workflows::MODE_ACTION)
            .unwrap_or(false);
        if is_action {
            let Some(action_id) = role_cfg.as_ref().and_then(|r| r.action_id.clone()) else {
                tracing::error!(
                    "[workflow] role '{}' has mode=action for step '{}' but no action_id (task {})",
                    role_key,
                    status,
                    task_id
                );
                return Ok(None);
            };
            let Some((plugin_manager, app_context)) = crate::kanban_action::runtime() else {
                tracing::error!(
                    "[workflow] kanban_action runtime unavailable; cannot run action '{}' for step '{}' (task {})",
                    action_id, status, task_id
                );
                return Ok(None);
            };
            let outcome =
                match crate::kanban_action::run_action_step(crate::kanban_action::ActionStepCtx {
                    pool,
                    data_dir,
                    plugin_manager,
                    app_context,
                    task_id,
                    channel_id: &channel_id,
                    profile: &effective_profile,
                    plan: Some(plan.unwrap_or(false)),
                    workflow_id: workflow_id.as_deref(),
                    step: status,
                    role: role_key,
                    action_id: &action_id,
                })
                .await
                {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        tracing::error!(
                            "[workflow] action step '{}' for task {} failed: {}",
                            status,
                            task_id,
                            e
                        );
                        return Ok(None);
                    }
                };
            // Audit + mark scheduled (mirrors step 6 below).
            sql_forge!(
                "UPDATE kanban_tasks SET thread_status = 'scheduled' WHERE id = :task_id",
                ( :task_id = task_id )
            )
            .execute(pool)
            .await?;
            let comment = format!(
                "Action-mode {} thread #{} created for step '{}' (action {})",
                if outcome.errored { "failure" } else { "result" },
                outcome.thread_id,
                status,
                action_id
            );
            let _ = sql_forge!(
                r#"
                INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
                VALUES (:task_id, 'workflow', :initial, :to_status, :comment)
                "#,
                ( :task_id = task_id, :initial = &task.status, :to_status = &task.status, :comment = comment.as_str() )
            )
            .execute(pool)
            .await
            .map_err(|e| {
                tracing::warn!(
                    "[kanban dispatch] history insert for action thread #{} failed: {:?}",
                    outcome.thread_id, e
                )
            });
            // Route the terminal action outcome through the workflow matrix
            // (action-mode: executor fail->blocked, tester fail->review,
            // reviewer fail->blocked; successes follow the agent matrix).
            if let Ok(Some(action_thread)) =
                crate::db::threads::get_thread_by_id(pool, outcome.thread_id).await
            {
                crate::agent::kanban_updater::route_step_completion(
                    pool,
                    data_dir,
                    &action_thread,
                    outcome.errored,
                )
                .await;
            }
            return Ok(Some(outcome.thread_id));
        }
    }

    // 5. Create the thread via the single canonical creation path
    //    (create_thread_with_cause resolves provider/model from the workflow
    //    role via workflow_step; the role template is passed explicitly).
    let params = ThreadCauseParams {
        provider: None,
        model: None,
        task_id: Some(task.id.clone()),
        schedule_task_id: None,
        content: kanban_thread_content(&task.title, task.body.as_deref()),
        external_id: None,
        parent_external_id: None,
        metadata: serde_json::json!({
            "kanban_task_id": task.id,
            "kanban_task_title": task.title,
            "template": resolved_template.clone(),
        }),
        msg_type: "kanban".to_string(),
        msg_subtype: Some(task.id.clone()),
        task_plan: plan,
        template: resolved_template,
        workflow_id: workflow_id.clone(),
        workflow_step: Some(status.to_string()),
        hook_caused: false,
    };
    let (thread, _message) = create_thread_with_cause(
        pool,
        data_dir,
        "system",
        &channel_id,
        &effective_profile,
        params,
    )
    .await?;

    // 6. Mark the task as queued for the agent loop + audit history.
    sql_forge!(
        "UPDATE kanban_tasks SET thread_status = 'scheduled' WHERE id = :task_id",
        ( :task_id = task_id )
    )
    .execute(pool)
    .await?;
    let comment = format!("Thread #{} created for step '{}'", thread.id, status);
    let _ = sql_forge!(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
        VALUES (:task_id, 'workflow', :initial, :to_status, :comment)
        "#,
        ( :task_id = task_id, :initial = &task.status, :to_status = &task.status, :comment = comment.as_str() )
    )
    .execute(pool)
    .await
    .map_err(|e| {
        tracing::warn!(
            "[kanban dispatch] history insert for new thread #{} failed: {:?}",
            thread.id, e
        )
    });

    Ok(Some(thread.id))
}

/// Invalid-board task handling (boards.yml feature enabled): create the
/// workflow-role thread and IMMEDIATELY terminate it as 'failed' with a
/// clear Error message, reusing the no-channel failure pattern. The task
/// is never dispatched; every thread-creation attempt produces a doomed
/// thread so the failure is auditable and surfaced truthfully.
async fn fail_kanban_thread_no_board(
    pool: &PgPool,
    data_dir: &str,
    task: &KanbanDispatchRow,
    status: &str,
    board_err: &str,
) -> AppResult<()> {
    // Resolve channel/profile WITHOUT the board (it is invalid): task ->
    // channel -> global defaults.
    let channel_id = match task.channel_id.as_deref() {
        Some(cid) => {
            crate::channels_yaml::resolve_default_channel(Some(cid), "default_kanban_channel")
                .unwrap_or_default()
        }
        None => crate::channels_yaml::resolve_default_channel(None, "default_kanban_channel")
            .unwrap_or_default(),
    };
    let channel = if channel_id.trim().is_empty() {
        None
    } else {
        crate::db::channels::get_channel_by_id(pool, &channel_id).await?
    };
    let effective_profile = task
        .profile
        .clone()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| {
            channel.as_ref().and_then(|c| {
                (!c.current_profile.trim().is_empty()).then(|| c.current_profile.clone())
            })
        })
        .unwrap_or_else(crate::profile::default_profile_name);
    let role = crate::workflows::role_for_step(status);
    let workflow = task.workflow_id.as_deref().and_then(|wf_id| {
        let path = crate::config_path::config_path(data_dir, "workflows.yml");
        crate::workflows::WorkflowsFile::load(&path)
            .ok()
            .and_then(|f| f.workflows.get(wf_id).cloned())
    });
    let role_cfg = role.and_then(|r| workflow.as_ref().and_then(|wf| wf.resolve_role(r)));
    let profile_template = crate::profile::ProfileRegistry::new(data_dir)
        .get(&effective_profile)
        .and_then(|p| p.template.clone());
    let resolved_template = resolve_kanban_thread_template(
        role_cfg.as_ref().and_then(|r| r.template.clone()),
        status == "running",
        task.template.as_deref(),
        channel.as_ref().and_then(|c| c.template.as_deref()),
        profile_template.as_deref(),
    );
    let params = ThreadCauseParams {
        provider: None,
        model: None,
        task_id: Some(task.id.clone()),
        schedule_task_id: None,
        content: kanban_thread_content(&task.title, task.body.as_deref()),
        external_id: None,
        parent_external_id: None,
        metadata: serde_json::json!({
            "kanban_task_id": task.id,
            "kanban_task_title": task.title,
            "error": board_err,
        }),
        msg_type: "kanban".to_string(),
        msg_subtype: Some(task.id.clone()),
        task_plan: task.plan,
        template: resolved_template,
        workflow_id: task.workflow_id.clone(),
        workflow_step: Some(status.to_string()),
        hook_caused: false,
    };
    let (thread, _msg) = create_thread_with_cause(
        pool,
        data_dir,
        "system",
        &channel_id,
        &effective_profile,
        params,
    )
    .await?;

    // Error message (msg_type='error', error_type=configuration) + terminal
    // 'failed' - the same shape the builtin fail-thread produces.
    let external_id = format!(
        "validation-error:{}:{}",
        thread.id,
        chrono::Utc::now().timestamp()
    );
    let msg_id: i64 = sql_forge!(
        scalar i64,
        r#"
        INSERT INTO messages
            (thread_id, role, content, thread_sequence, external_id, metadata,
             msg_type, msg_subtype, iteration_number, duration_ms, token_usage)
        VALUES
            (:tid, 'system', :content, 1, :external_id,
             :metadata::jsonb, 'error', :subtype, 0, 0, '{}'::jsonb)
        RETURNING id
        "#,
        (
            :tid = thread.id,
            :content = board_err,
            :external_id = external_id.as_str(),
            :metadata = serde_json::json!({ "error_type": "configuration" }),
            :subtype = format!("board:{}", task.id),
        )
    )
    .fetch_one(pool)
    .await?;
    // Event-driven hooks: validation errors are real messages in non-hook
    // threads - fire new_message exactly once (GROUP 27 CI invariant).
    crate::hooks::fire_new_message(thread.id, msg_id);

    // Terminal write through the single choke point (status='failed',
    // terminal=true, ended_at, iterations) + hooks.
    set_thread_failed(pool, thread.id).await?;

    // Audit the board rejection in kanban history (best-effort).
    let _ = sql_forge!(
        r#"
        INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
        VALUES (:task_id, 'workflow', :initial, :initial, :comment)
        "#,
        (
            :task_id = task.id.as_str(),
            :initial = task.status.as_str(),
            :comment = format!("Thread #{} failed: {}", thread.id, board_err),
        )
    )
    .execute(pool)
    .await
    .map_err(|e| {
        tracing::warn!(
            "[kanban dispatch] history insert for invalid-board thread #{} failed: {:?}",
            thread.id,
            e
        )
    });

    Err(crate::error::Error::Message(board_err.to_string()))
}

/// Board validity guard for workflow-transition thread creators that do NOT
/// go through `create_kanban_step_thread` (tester/reviewer step threads in
/// kanban_updater). Boards disabled -> always Ok. Boards enabled -> Err(msg)
/// when the task's board is NULL or unknown.
pub async fn ensure_task_board_valid(
    pool: &PgPool,
    data_dir: &str,
    task_id: &str,
) -> Result<(), String> {
    if !crate::boards::boards_enabled(data_dir) {
        return Ok(());
    }
    let board: Option<String> = sql_forge!(
        scalar Option<String>,
        "SELECT board FROM kanban_tasks WHERE id = :id",
        ( :id = task_id )
    )
    .fetch_one(pool)
    .await
    .map_err(|e| format!("failed to load task board: {e}"))?;
    match board {
        None => Err("task has no board".to_string()),
        Some(name) => {
            let path = crate::config_path::config_path(data_dir, "boards.yml");
            let file = crate::boards::BoardsFile::load(&path)
                .map_err(|e| format!("failed to load boards.yml: {e}"))?;
            if file.boards.contains_key(&name) {
                Ok(())
            } else {
                Err(format!("task board '{name}' not found in boards.yml"))
            }
        }
    }
}

/// Dispatch a kanban task AFTER its status changed (UI move / API status
/// update): first STOP the task's old-status threads (every pending or
/// processing thread whose workflow step does not serve the new status is
/// marked skipped - it must never keep running against a task that moved
/// away from it), then create the mapped role thread (running -> executor,
/// testing -> tester, review -> reviewer) and mark the task
/// `thread_status='scheduled'`. Does NOT change the task's own status -
/// the caller owns the transition.
///
/// The stale-thread skip runs even when the new status has no role to run
/// (done/blocked/todo/backlog): the old threads are stopped regardless, and
/// `Ok(None)` is returned when no new thread applies.
///
/// Returns `Some(thread_id)` when a thread was created, `None` when the
/// status has no role to run.
pub(crate) async fn dispatch_task_for_status(
    pool: &PgPool,
    data_dir: &str,
    task_id: &str,
    new_status: &str,
) -> AppResult<Option<i64>> {
    skip_stale_threads_for_status(pool, task_id, new_status).await?;
    create_kanban_step_thread(pool, data_dir, task_id, new_status).await
}

/// Skip all pending/processing threads on startup, then redispatch every
/// kanban task sitting in a workflow column without an active thread.
///
/// The old per-thread re-schedule branch is gone: the unified startup
/// recovery marks every pending/processing thread terminal (single choke
/// point) and then re-creates the role thread for each kanban task in
/// `running`/`testing`/`review` that has NO active thread - the SAME code
/// path as status-change dispatch and `/redispatch`. Safeguards preserved:
/// no retry consumed, task status never moved back to `todo`, blocked/done
/// tasks untouched.
pub async fn skip_all_pending_threads(pool: &PgPool, data_dir: &str) -> AppResult<u64> {
    #[derive(sqlx::FromRow)]
    struct SkipRow {
        id: i64,
    }

    let threads: Vec<SkipRow> = sql_forge!(
        SkipRow,
        r#"SELECT id FROM threads WHERE status IN ('pending', 'processing')"#,
    )
    .fetch_all(pool)
    .await?;

    let mut tx = pool.begin().await?;
    for t in &threads {
        // Terminal write: single choke point sets terminal=true with 'skipped'.
        mark_thread_terminal(&mut *tx, t.id, "skipped").await?;
    }
    tx.commit().await?;

    // Unified startup redispatch: every kanban task in a workflow column
    // WITHOUT an active thread gets its role thread re-created (the skipped
    // threads above leave those tasks without a runner). Blocked/done tasks
    // are untouched; task status is never changed; no retry is consumed.
    #[derive(sqlx::FromRow)]
    struct StuckTaskRow {
        id: String,
        status: String,
    }
    let stuck: Vec<StuckTaskRow> = sql_forge!(
        StuckTaskRow,
        r#"
        SELECT t.id, t.status
        FROM kanban_tasks t
        WHERE t.status IN ('running', 'testing', 'review')
          AND t.archived = false
          AND NOT EXISTS (
              SELECT 1 FROM threads th
              WHERE th.task_id = t.id AND th.status IN ('pending', 'processing')
          )
        "#,
    )
    .fetch_all(pool)
    .await?;
    for task in &stuck {
        if let Err(e) = create_kanban_step_thread(pool, data_dir, &task.id, &task.status).await {
            tracing::warn!(
                "[startup] failed to redispatch kanban task {} (step {}): {:?}",
                task.id,
                task.status,
                e
            );
        }
    }

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
            msg_type, msg_subtype, original_thread_id, iteration_number,
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
    channel_id: String,
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
                    workflow_step,
                    template
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
                    workflow_step,
                    template
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
                    workflow_step,
                    template
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
// Phase 2 - getters used by the builtin fail-thread tool / finalization guard
// ─────────────────────────────────────────────────────────────────────────────

/// Get a thread row by ID (used by the builtin fail-thread tool to resolve
/// the current thread).
pub async fn get_thread_by_id(pool: &PgPool, thread_id: i64) -> AppResult<Option<Thread>> {
    let row: Option<ThreadDb> =     sql_forge!(
        ThreadDb,
        r#"
        SELECT
            id, status, cause, channel_id, profile, provider, model, task_id, schedule_task_id,
            input_tokens, cached_tokens, output_tokens, duration_ms,
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(started_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "started_at",
            COALESCE(TO_CHAR(ended_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "ended_at",
            terminal, plan, parent_id, iterations, workflow_step, template
        FROM threads
        WHERE id = :thread_id
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

/// Get the current status of a thread (None if the thread does not exist).
/// Used by handle_response to detect a FAILED state already applied by the
/// builtin fail-thread tool before normal finalization runs.
pub async fn get_thread_status(pool: &PgPool, thread_id: i64) -> AppResult<Option<String>> {
    let status: Option<String> = sql_forge!(
        scalar String,
        "SELECT status FROM threads WHERE id = :thread_id",
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await?;

    Ok(status)
}

/// Get the last message of a thread (highest thread_sequence, then id).
/// Used by handle_response to return the fail-thread tool's Error message
/// as the final thread message.
pub async fn get_last_message(pool: &PgPool, thread_id: i64) -> AppResult<Option<Message>> {
    let row: Option<MessageDb> =     sql_forge!(
        MessageDb,
        r#"
        SELECT
            id, thread_id, role, content, thread_sequence, external_id,
            metadata::text AS "metadata", embedding, summary_text, is_summary,
            msg_type, msg_subtype, original_thread_id, iteration_number,
            duration_ms, token_usage::text AS "token_usage",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at"
        FROM messages
        WHERE thread_id = :thread_id
        ORDER BY thread_sequence DESC, id DESC
        LIMIT 1
        "#,
        ( :thread_id = thread_id )
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| r.try_into()).transpose()
}

/// Delete terminal threads (and their messages + subtasks) older than `before`.
/// Non-terminal threads (pending/processing) are NEVER deleted, even when old.
/// Handles the threads self-ref: children whose parent is being deleted get
/// `parent_id = NULL` first so no FK violation and no orphaned pointer remains.
/// Delete order: messages -> thread_subtasks -> threads (FK-safe).
pub async fn delete_old_threads(
    pool: &PgPool,
    before: chrono::DateTime<chrono::Utc>,
) -> AppResult<u64> {
    use sqlx::Transaction;

    let mut tx: Transaction<'_, sqlx::Postgres> = pool.begin().await?;
    // 1. Messages of candidate threads (FK messages_thread_id_fkey). The
    //    append-only trigger (trg_messages_append_only) blocks DELETE on
    //    messages, so it is disabled for this transaction only (transactional
    //    DDL: rollback restores it) - old-data cleanup is the sanctioned
    //    purge path.
    sqlx::query("ALTER TABLE messages DISABLE TRIGGER trg_messages_append_only")
        .execute(&mut *tx)
        .await?;
    sql_forge!(
        "DELETE FROM messages WHERE thread_id IN (SELECT id FROM threads WHERE terminal = true AND created_at < :cutoff)",
        ( :cutoff = before )
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("ALTER TABLE messages ENABLE TRIGGER trg_messages_append_only")
        .execute(&mut *tx)
        .await?;
    // 2. Subtasks of candidate threads (FK thread_subtasks_thread_id_fkey).
    sql_forge!(
        "DELETE FROM thread_subtasks WHERE thread_id IN (SELECT id FROM threads WHERE terminal = true AND created_at < :cutoff)",
        ( :cutoff = before )
    )
    .execute(&mut *tx)
    .await?;
    // 3. Detach children whose parent is being deleted (threads_parent_id_fkey).
    sql_forge!(
        "UPDATE threads SET parent_id = NULL WHERE parent_id IN (SELECT id FROM threads WHERE terminal = true AND created_at < :cutoff)",
        ( :cutoff = before )
    )
    .execute(&mut *tx)
    .await?;
    // 4. Delete the candidate threads.
    let result = sql_forge!(
        "DELETE FROM threads WHERE terminal = true AND created_at < :cutoff",
        ( :cutoff = before )
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_cause_content_never_used_as_prompt() {
        // Regression for threads 240-263: retry/reschedule/review paths
        // historically wrote the cause-kind enum ('system'/'user') into the
        // seq-0 cause message content. These strings must be treated as
        // placeholders so the retry path falls back to a real prompt.
        for placeholder in ["system", "user", "", "   ", "\n"] {
            assert!(
                is_placeholder_cause_content(placeholder),
                "{placeholder:?} must be a placeholder"
            );
        }
        for real in [
            "Implement feature X\n## Body",
            "  a real prompt  ",
            "kanban task body",
        ] {
            assert!(
                !is_placeholder_cause_content(real),
                "{real:?} must NOT be a placeholder"
            );
        }
    }

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
        // skipped and the task re-scheduled the same way - no retry consumed,
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
        // todo - the recovery plan has no todo variant and touches no counters.
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
        // unchanged) - never moved to todo.
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

    // ---------- Thread identity resolution precedence ----------
    // Regression guard for 83f461b: the Aug 11 refactor reversed the
    // provider chain (profile.provider was checked BEFORE
    // channel.current_provider). That made kanban executor threads on the
    // wf-test channel (current_provider=noop) resolve to the omni profile's
    // deepseek provider - a REAL LLM call in tests that must never hit one.
    // Channel override MUST win: it is the operator's explicit per-channel
    // choice; the profile's provider is only a default.

    fn test_channel(provider: Option<&str>, model: Option<&str>) -> crate::db::types::Channel {
        use chrono::Utc;
        crate::db::types::Channel {
            id: "test-channel".to_string(),
            name: "test-channel".to_string(),
            platform: None,
            resource_identifier: None,
            external_id: None,
            current_profile: "omni".to_string(),
            current_model: model.map(|s| s.to_string()),
            current_provider: provider.map(|s| s.to_string()),
            readonly: false,
            closed: false,
            plan: true,
            metadata: serde_json::json!({}),
            template: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn channel_provider_beats_profile_provider() {
        // Channel pins noop/test-tool-caller (like the wf-test channel);
        // profile (default/omni) declares deepseek. Channel must win so the
        // thread NEVER resolves to a real LLM.
        let data_dir = std::env::temp_dir().to_str().unwrap().to_string();
        let ch = test_channel(Some("noop"), Some("test-tool-caller"));
        let identity =
            resolve_thread_identity(&data_dir, "omni", Some(&ch), None, None, None, None)
                .expect("identity must resolve from channel override");
        assert_eq!(
            identity.provider, "noop",
            "channel provider must win over profile"
        );
        assert_eq!(
            identity.model, "test-tool-caller",
            "channel model must win over profile"
        );
    }

    #[test]
    fn profile_provider_used_when_channel_has_none() {
        // No channel override → fall back to the profile's provider, sourced
        // from profiles.yml (config.json is no longer read).
        let data_dir = std::env::temp_dir().join(format!("threads-profile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        std::fs::create_dir_all(data_dir.join("config")).unwrap();
        std::fs::write(
            data_dir.join("config").join("profiles.yml"),
            "profiles:\n  omni:\n    provider: opencode-go\n    model: deepseek-v4-flash\n",
        )
        .unwrap();
        let ch = test_channel(None, None);
        let identity = resolve_thread_identity(
            data_dir.to_str().unwrap(),
            "omni",
            Some(&ch),
            None,
            None,
            None,
            None,
        )
        .expect("identity must resolve from profile");
        assert_eq!(
            identity.provider, "opencode-go",
            "provider must come from the profile tier (profiles.yml)"
        );
        assert_eq!(
            identity.model, "deepseek-v4-flash",
            "model must come from the profile tier (profiles.yml)"
        );
    }

    // ---------- Kanban status-change dispatch / redispatch ----------

    #[test]
    fn kanban_step_actionable_gates() {
        // running always runs, even without a workflow.
        assert!(kanban_step_actionable("running", None, false));
        assert!(kanban_step_actionable("running", Some("wf"), false));
        // testing/review require a workflow that defines the role.
        assert!(!kanban_step_actionable("testing", None, false));
        assert!(!kanban_step_actionable("testing", Some("wf"), false));
        assert!(kanban_step_actionable("testing", Some("wf"), true));
        assert!(!kanban_step_actionable("review", None, true));
        assert!(!kanban_step_actionable("review", Some("wf"), false));
        assert!(kanban_step_actionable("review", Some("wf"), true));
        // Other statuses never dispatch.
        for s in ["backlog", "todo", "blocked", "done", ""] {
            assert!(!kanban_step_actionable(s, Some("wf"), true), "status {s}");
        }
    }

    #[test]
    fn resolve_kanban_thread_template_precedence() {
        // Role template wins for workflow steps.
        assert_eq!(
            resolve_kanban_thread_template(
                Some("dev-tester".to_string()),
                false,
                Some("t"),
                Some("c"),
                None
            ),
            Some("dev-tester".to_string())
        );
        // Running without a role template: task -> channel -> dev-development.
        assert_eq!(
            resolve_kanban_thread_template(None, true, Some("task-tpl"), Some("channel-tpl"), None),
            Some("task-tpl".to_string())
        );
        assert_eq!(
            resolve_kanban_thread_template(None, true, None, Some("channel-tpl"), None),
            Some("channel-tpl".to_string())
        );
        assert_eq!(
            resolve_kanban_thread_template(None, true, Some(""), Some(""), None),
            Some("dev-development".to_string())
        );
        assert_eq!(
            resolve_kanban_thread_template(None, true, None, None, None),
            Some("dev-development".to_string())
        );
        // Profile template fills the tier between channel and dev-development.
        assert_eq!(
            resolve_kanban_thread_template(None, true, None, None, Some("profile-tpl")),
            Some("profile-tpl".to_string())
        );
        assert_eq!(
            resolve_kanban_thread_template(
                None,
                true,
                None,
                Some("channel-tpl"),
                Some("profile-tpl")
            ),
            Some("channel-tpl".to_string())
        );
        // Step threads (testing/review) without a role template stay None -
        // same as kanban_updater's step-thread creation.
        assert_eq!(
            resolve_kanban_thread_template(
                None,
                false,
                Some("task-tpl"),
                Some("channel-tpl"),
                None
            ),
            None
        );
    }

    #[test]
    fn kanban_thread_content_title_and_body() {
        assert_eq!(kanban_thread_content("Title", None), "Title");
        assert_eq!(kanban_thread_content("Title", Some("")), "Title");
        assert_eq!(kanban_thread_content("Title", Some("  ")), "Title");
        assert_eq!(
            kanban_thread_content("Title", Some("Body")),
            "Title\n\nBody"
        );
    }

    #[tokio::test]
    async fn update_thread_progress_shows_live_values_then_terminal_write() {
        // DB-backed test: exercises the incremental thread-progress UPDATE
        // against a real (dev) database. Skipped when DATABASE_URL is absent
        // (offline/CI without a DB). It only ever touches rows it creates
        // itself, and never runs against a production database.
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");

        let thread_id: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile) \
             VALUES ('pending', 'user', 'test-channel-merged-pending', 'test-profile') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert test thread");

        // Claim: status -> 'processing' (the live/running state).
        assert!(claim_thread(&pool, thread_id).await, "claim must succeed");

        // Simulate three LLM calls; after each, the row must already show the
        // cumulative live values while the thread is still processing.
        for i in 1..=3 {
            let stats = CompleteThreadStats {
                input_tokens: 100 * i,
                cached_tokens: 20 * i,
                output_tokens: 50 * i,
                duration_ms: 500 * i,
            };
            update_thread_progress(&pool, thread_id, i, stats)
                .await
                .expect("incremental update must succeed");
            let (iterations, input_tokens, cached_tokens, output_tokens, duration_ms, status, terminal): (
                i32,
                i32,
                i32,
                i32,
                i32,
                String,
                bool,
            ) = sqlx::query_as(
                "SELECT iterations, input_tokens, cached_tokens, output_tokens, duration_ms, status, terminal \
                 FROM threads WHERE id = $1",
            )
            .bind(thread_id)
            .fetch_one(&pool)
            .await
            .expect("fetch live progress");
            assert_eq!(iterations, i, "live iterations after call {i}");
            assert_eq!(input_tokens, 100 * i, "live input_tokens after call {i}");
            assert_eq!(cached_tokens, 20 * i, "live cached_tokens after call {i}");
            assert_eq!(output_tokens, 50 * i, "live output_tokens after call {i}");
            assert_eq!(duration_ms, 500 * i, "live duration_ms after call {i}");
            assert_eq!(
                status, "processing",
                "thread must stay processing while live"
            );
            assert!(!terminal, "live thread must not be terminal");
        }

        // Mirror production: each LLM call persisted a message with the
        // matching iteration_number (terminal iterations = MAX(iteration_number)).
        for i in 1..=3 {
            sqlx::query(
                "INSERT INTO messages (thread_id, thread_sequence, role, content, msg_type, iteration_number) \
                 VALUES ($1, $2, 'agent', 'x', 'message', $2)",
            )
            .bind(thread_id)
            .bind(i)
            .execute(&pool)
            .await
            .expect("insert test message");
        }

        // Terminal write must remain exactly as before: final stats win and
        // terminal=true is set.
        let final_stats = CompleteThreadStats {
            input_tokens: 300,
            cached_tokens: 60,
            output_tokens: 150,
            duration_ms: 1500,
        };
        complete_thread(&pool, thread_id, "completed", final_stats)
            .await
            .expect("terminal write must succeed");
        let (iterations, input_tokens, cached_tokens, output_tokens, duration_ms, status, terminal): (
            i32,
            i32,
            i32,
            i32,
            i32,
            String,
            bool,
        ) = sqlx::query_as(
            "SELECT iterations, input_tokens, cached_tokens, output_tokens, duration_ms, status, terminal \
             FROM threads WHERE id = $1",
        )
        .bind(thread_id)
        .fetch_one(&pool)
        .await
        .expect("fetch terminal row");
        assert_eq!(iterations, 3, "terminal iterations from messages");
        assert_eq!(input_tokens, 300);
        assert_eq!(cached_tokens, 60);
        assert_eq!(output_tokens, 150);
        assert_eq!(duration_ms, 1500);
        assert_eq!(status, "completed");
        assert!(terminal, "terminal flag must be set");

        // Cleanup: delete only the rows this test created.
        let _ = sqlx::query("DELETE FROM messages WHERE thread_id = $1")
            .bind(thread_id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM threads WHERE id = $1")
            .bind(thread_id)
            .execute(&pool)
            .await;
    }

    #[tokio::test]
    async fn terminal_write_persists_real_token_stats_from_messages() {
        // DB-backed test: a terminal write with zero passed stats must fall
        // back to aggregating messages.token_usage, and a non-agentic system
        // thread (no LLM calls) ends at 0/0 - never NULL/'-'. Skipped when
        // DATABASE_URL is absent (offline/CI without a DB). It only ever
        // touches rows it creates itself, and never runs against a
        // production database.
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");

        let thread_id: i64 = sqlx::query_scalar(
                "INSERT INTO threads (status, cause, channel_id, profile) \
                 VALUES ('pending', 'user', 'test-channel-token-stats', 'test-profile') RETURNING id",
            )
            .fetch_one(&pool)
            .await
            .expect("insert test thread");

        // Two agentic LLM calls (iteration 1 and 2) persisted their usage on
        // the tool-call messages, exactly like main_loop does.
        for (seq, iteration, prompt, completion, cached) in
            [(1, 1, 100, 20, 30), (2, 2, 150, 25, 40)]
        {
            sqlx::query(
                    "INSERT INTO messages \
                         (thread_id, thread_sequence, role, content, msg_type, iteration_number, token_usage) \
                     VALUES ($1, $2, 'agent', 'call', 'tool', $3, $4::jsonb)",
                )
                .bind(thread_id)
                .bind(seq)
                .bind(iteration)
                .bind(
                    serde_json::json!({
                        "prompt_tokens": prompt,
                        "completion_tokens": completion,
                        "cached_tokens": cached,
                    })
                    .to_string(),
                )
                .execute(&pool)
                .await
                .expect("insert usage message");
        }

        // Failure path passes ZERO stats: the fallback must persist the REAL
        // aggregated usage (100+150 prompt, 30+40 cached, 20+25 completion)
        // and the real LLM call count (iterations = 2).
        complete_thread(
            &pool,
            thread_id,
            "failed",
            CompleteThreadStats {
                input_tokens: 0,
                cached_tokens: 0,
                output_tokens: 0,
                duration_ms: 0,
            },
        )
        .await
        .expect("terminal write must succeed");
        let (iterations, input_tokens, cached_tokens, output_tokens, terminal): (
            i32,
            i32,
            i32,
            i32,
            bool,
        ) = sqlx::query_as(
            "SELECT iterations, input_tokens, cached_tokens, output_tokens, terminal \
                 FROM threads WHERE id = $1",
        )
        .bind(thread_id)
        .fetch_one(&pool)
        .await
        .expect("fetch terminal row");
        assert_eq!(iterations, 2, "real LLM call count");
        assert_eq!(input_tokens, 250, "fallback prompt sum");
        assert_eq!(cached_tokens, 70, "fallback cached sum");
        assert_eq!(output_tokens, 45, "fallback completion sum");
        assert!(terminal, "terminal flag must be set");

        // Non-agentic system thread: no usage messages -> 0/0, never NULL.
        let sys_id: i64 = sqlx::query_scalar(
                "INSERT INTO threads (status, cause, channel_id, profile) \
                 VALUES ('pending', 'system', 'test-channel-token-stats', 'test-profile') RETURNING id",
            )
            .fetch_one(&pool)
            .await
            .expect("insert system thread");
        sqlx::query(
                "INSERT INTO messages (thread_id, thread_sequence, role, content, msg_type, token_usage) \
                 VALUES ($1, 0, 'cause', 'init', 'cause', '{}')",
            )
            .bind(sys_id)
            .execute(&pool)
            .await
            .expect("insert cause message");
        set_thread_system(&pool, sys_id)
            .await
            .expect("mark system thread terminal");
        let (iterations, input_tokens, output_tokens, terminal): (i32, i32, i32, bool) =
            sqlx::query_as(
                "SELECT iterations, input_tokens, output_tokens, terminal \
                     FROM threads WHERE id = $1",
            )
            .bind(sys_id)
            .fetch_one(&pool)
            .await
            .expect("fetch system row");
        assert_eq!(iterations, 0, "system thread iterations stay 0");
        assert_eq!(input_tokens, 0, "system thread tokens stay 0");
        assert_eq!(output_tokens, 0, "system thread tokens stay 0");
        assert!(terminal, "system thread must be terminal");

        // Cleanup: delete only the rows this test created.
        let _ = sqlx::query("DELETE FROM messages WHERE thread_id IN ($1, $2)")
            .bind(thread_id)
            .bind(sys_id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM threads WHERE id IN ($1, $2)")
            .bind(thread_id)
            .bind(sys_id)
            .execute(&pool)
            .await;
    }
}

#[allow(clippy::items_after_test_module)]
/// List PENDING user threads whose prompt should be appended into the running
/// thread as a sub-prompt (feature: sub-prompts).
///
/// Match condition (per feature spec): same channel + same profile +
/// cause='user' + status='pending' + NOT terminal, and the pending
/// thread's parent relation to the running thread is one of:
///   1. direct child: pending.parent_id = running.id (a reply to the
///      running thread's own seq-0 cause message);
///   2. resolved sibling: pending.parent_id IS NOT NULL and equal to
///      running.parent_id (a reply inside the same Mattermost thread
///      whose root thread row still exists);
///   3. shared parent external id: the seq-0 cause messages of both
///      threads carry the same non-empty parent external id (metadata
///      key 'root_id'). This covers parent-by-chat platforms (telegram:
///      root_id = chat id, never a message external id, so it never
///      resolves to a threads.parent_id) and Mattermost sibling replies
///      after the root thread row was deleted or never existed.
///
/// Top-level channel messages (no parent external id) never match here.
///
/// Ordered by id ASC (oldest first).
pub async fn list_appendable_pending_threads(
    pool: &PgPool,
    channel_id: &str,
    profile: &str,
    running_thread_id: i64,
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
            workflow_step,
            template
        FROM threads t
        WHERE t.channel_id = :channel_id
          AND t.profile = :profile
          AND t.cause = 'user'
          AND t.status = 'pending'
          AND NOT t.terminal
          AND t.id <> :running_thread_id
          AND (
               t.parent_id = :running_thread_id
               OR (t.parent_id IS NOT NULL AND t.parent_id = (SELECT parent_id FROM threads WHERE id = :running_thread_id))
               OR EXISTS (
                    SELECT 1
                    FROM messages r0, messages p0
                    WHERE r0.thread_id = :running_thread_id AND r0.thread_sequence = 0
                      AND p0.thread_id = t.id AND p0.thread_sequence = 0
                      AND r0.metadata->>'root_id' IS NOT NULL
                      AND r0.metadata->>'root_id' <> ''
                      AND p0.metadata->>'root_id' = r0.metadata->>'root_id'
               )
          )
        ORDER BY t.id ASC
        "#,
        ( :channel_id = channel_id, :profile = profile, :running_thread_id = running_thread_id )
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

#[allow(clippy::items_after_test_module)]
/// Mark a pending thread 'merged' after its prompt was appended as a
/// sub-prompt into a running thread (feature: sub-prompts).
///
/// 'merged' is a terminal status, semantically identical to 'skipped' (the
/// thread produces no own run/result) but distinguishable in history/UI: the
/// prompt was absorbed by the executing thread `running_thread_id`. The
/// target link is recorded by the sub_cause message (see
/// insert_sub_cause_message; the threads API exposes it as
/// merged_into_thread_id).
///
/// Uses the single terminal choke point (mark_thread_terminal) so
/// terminal=true is always set with 'merged'. When the merged thread is
/// linked to a kanban task, the task's thread_status is cleared and a history
/// entry records the merge (best-effort, mirrors skip_channel_threads).
pub async fn mark_thread_merged_for_sub_prompt(
    pool: &PgPool,
    pending_id: i64,
    running_thread_id: i64,
) -> AppResult<u64> {
    let result = mark_thread_terminal(pool, pending_id, "merged").await?;
    if result > 0 {
        crate::hooks::fire_thread_finished(pending_id);
        #[derive(sqlx::FromRow)]
        struct TaskRow {
            task_id: Option<String>,
        }
        let t: Option<TaskRow> = sql_forge!(
            TaskRow,
            "SELECT task_id FROM threads WHERE id = :id",
            ( :id = pending_id )
        )
        .fetch_optional(pool)
        .await?;
        if let Some(task_id) = t.and_then(|r| r.task_id) {
            let _ = sql_forge!(
                "UPDATE kanban_tasks SET thread_status = NULL WHERE id = :task_id",
                ( :task_id = task_id.as_str() )
            )
            .execute(pool)
            .await;
            let comment = format!(
                "Thread #{} merged (prompt appended as sub-prompt to running thread #{})",
                pending_id, running_thread_id
            );
            let _ = sql_forge!(
                r#"
                INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, comment)
                VALUES (:task_id, 'workflow', '', '', :comment)
                "#,
                ( :task_id = task_id.as_str(), :comment = comment.as_str() )
            )
            .execute(pool)
            .await;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod merged_status_tests {
    use super::*;

    /// DB-backed: exercises the sub-prompt merge terminal transition against a
    /// real (dev) database: a pending thread becomes terminal 'merged' while
    /// the target running thread is untouched. Skipped when DATABASE_URL is
    /// absent (offline/CI without a DB). Only touches rows it creates itself,
    /// never production.
    #[tokio::test]
    async fn merged_for_sub_prompt_is_terminal_with_target_link() {
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");

        let pending_id: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile) VALUES ('pending', 'user', 'test-channel', 'test-profile') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert pending thread");
        let running_id: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile) VALUES ('processing', 'user', 'test-channel-merged-running', 'test-profile') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert running thread");

        let n = mark_thread_merged_for_sub_prompt(&pool, pending_id, running_id)
            .await
            .expect("mark merged");
        assert_eq!(n, 1, "exactly one row flipped to merged");

        let row: (String, bool) =
            sqlx::query_as("SELECT status, terminal FROM threads WHERE id = $1")
                .bind(pending_id)
                .fetch_one(&pool)
                .await
                .expect("fetch merged thread");
        assert_eq!(row.0, "merged", "status is merged");
        assert!(row.1, "merged is terminal");

        // The target running thread is left untouched (still processing, not
        // terminal). The merged_into_thread_id link itself is derived from the
        // sub_cause message that main_loop inserts before marking merged (see
        // insert_sub_cause_message / the server threads API / the dashboard
        // merged-into badge); messages are append-only so this DB-backed test
        // does not insert one.
        let trow: (String, bool) =
            sqlx::query_as("SELECT status, terminal FROM threads WHERE id = $1")
                .bind(running_id)
                .fetch_one(&pool)
                .await
                .expect("fetch running thread");
        assert_eq!(trow.0, "processing", "running thread still processing");
        assert!(!trow.1, "running thread is not terminal");

        sqlx::query("DELETE FROM threads WHERE id = $1 OR id = $2")
            .bind(pending_id)
            .bind(running_id)
            .execute(&pool)
            .await
            .expect("cleanup threads");
    }
}

#[cfg(test)]
mod status_move_skip_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// DB-backed regression for stopping a task's old-status threads when the
    /// task MOVES (UI move / API status update / workflow transition): every
    /// pending/processing thread tied to a status the task LEFT is marked
    /// skipped through the terminal choke point, while a thread already
    /// serving the task's NEW status is untouched. Skipped when DATABASE_URL
    /// is absent (offline/CI without a DB). Only touches rows it creates
    /// itself, never production.
    #[tokio::test]
    async fn status_move_skips_only_old_status_threads() {
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let task_id = format!(
            "task-status-stop-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );

        // Old-status thread: a processing executor thread (workflow_step
        // 'running') of a task that is moving to 'testing'.
        let old_id: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile, task_id, workflow_step)
             VALUES ('processing', 'user', 'test-channel-status-stop', 'test-profile', $1, 'running')
             RETURNING id",
        )
        .bind(&task_id)
        .fetch_one(&pool)
        .await
        .expect("insert old-status thread");

        // Legacy thread with no workflow_step: treated as serving 'running'.
        let legacy_id: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile, task_id)
             VALUES ('pending', 'user', 'test-channel-status-stop', 'test-profile', $1)
             RETURNING id",
        )
        .bind(&task_id)
        .fetch_one(&pool)
        .await
        .expect("insert legacy thread");

        // New-status thread: already pending for 'testing' - must survive.
        let new_id: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile, task_id, workflow_step)
             VALUES ('pending', 'user', 'test-channel-status-stop', 'test-profile', $1, 'testing')
             RETURNING id",
        )
        .bind(&task_id)
        .fetch_one(&pool)
        .await
        .expect("insert new-status thread");

        let n = skip_stale_threads_for_status(&pool, &task_id, "testing")
            .await
            .expect("skip stale threads");
        assert_eq!(n, 2, "exactly the two running-serving threads are skipped");

        let row: (String, bool) =
            sqlx::query_as("SELECT status, terminal FROM threads WHERE id = $1")
                .bind(old_id)
                .fetch_one(&pool)
                .await
                .expect("fetch old thread");
        assert_eq!(row.0, "skipped", "old-status thread marked skipped");
        assert!(row.1, "old-status thread is terminal");

        let row: (String, bool) =
            sqlx::query_as("SELECT status, terminal FROM threads WHERE id = $1")
                .bind(legacy_id)
                .fetch_one(&pool)
                .await
                .expect("fetch legacy thread");
        assert_eq!(row.0, "skipped", "legacy thread marked skipped");
        assert!(row.1, "legacy thread is terminal");

        let row: (String, bool) =
            sqlx::query_as("SELECT status, terminal FROM threads WHERE id = $1")
                .bind(new_id)
                .fetch_one(&pool)
                .await
                .expect("fetch new thread");
        assert_eq!(row.0, "pending", "new-status thread untouched");
        assert!(!row.1, "new-status thread not terminal");

        // Idempotent: a second call (or a no-op status update) skips nothing.
        let n = skip_stale_threads_for_status(&pool, &task_id, "testing")
            .await
            .expect("second skip");
        assert_eq!(n, 0, "nothing left to skip");

        sqlx::query("DELETE FROM kanban_history WHERE kanban_task_id = $1")
            .bind(&task_id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM threads WHERE id = $1 OR id = $2 OR id = $3")
            .bind(old_id)
            .bind(legacy_id)
            .bind(new_id)
            .execute(&pool)
            .await
            .expect("cleanup threads");
    }
}
#[cfg(test)]
mod sub_prompt_appendable_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static CHAN_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Insert a user thread row plus its seq-0 cause message (role 'cause',
    /// msg_type 'Cause'), mirroring create_thread_with_cause for an inbound
    /// platform message. parent_root_id is stored in metadata 'root_id'
    /// (the parent external id). Returns the new thread id.
    async fn insert_user_thread(
        pool: &PgPool,
        channel: &str,
        status: &str,
        external_id: &str,
        parent_root_id: Option<&str>,
        parent_id: Option<i64>,
        msg_subtype: &str,
    ) -> i64 {
        let tid: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile, parent_id) \
             VALUES ($1, 'user', $2, 'test-profile', $3) RETURNING id",
        )
        .bind(status)
        .bind(channel)
        .bind(parent_id)
        .fetch_one(pool)
        .await
        .expect("insert test thread");
        let mut meta = serde_json::json!({});
        if let Some(root) = parent_root_id {
            meta["root_id"] = serde_json::json!(root);
        }
        sqlx::query(
            "INSERT INTO messages (thread_id, thread_sequence, role, content, msg_type, \
             msg_subtype, external_id, metadata, channel_id) \
             VALUES ($1, 0, 'cause', 'a user prompt', 'Cause', $2, $3, $4::jsonb, $5)",
        )
        .bind(tid)
        .bind(msg_subtype)
        .bind(external_id)
        .bind(meta.to_string())
        .bind(channel)
        .execute(pool)
        .await
        .expect("insert cause message");
        tid
    }

    async fn cleanup_threads(pool: &PgPool, ids: &[i64]) {
        for id in ids {
            let _ = sqlx::query("DELETE FROM messages WHERE thread_id = $1")
                .bind(id)
                .execute(pool)
                .await;
            let _ = sqlx::query("DELETE FROM threads WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await;
        }
    }

    fn chan(prefix: &str) -> String {
        format!(
            "test-channel-{prefix}-{}-{}",
            std::process::id(),
            CHAN_SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[tokio::test]
    async fn telegram_same_chat_follow_up_merges_into_running_thread() {
        // Telegram parent_by_chat: every inbound message of a chat carries
        // the chat id as its parent external id (metadata 'root_id'), never
        // a message external_id, so no threads.parent_id is ever resolved.
        // A follow-up arriving while a same-chat thread is running must
        // merge into that running thread. Pre-fix this listed nothing.
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");
        let channel = chan("tg-merge");

        let running_id = insert_user_thread(
            &pool,
            &channel,
            "processing",
            "tg-1",
            Some("chat-9"),
            None,
            "telegram",
        )
        .await;
        let pending_id = insert_user_thread(
            &pool,
            &channel,
            "pending",
            "tg-2",
            Some("chat-9"),
            None,
            "telegram",
        )
        .await;

        let appendable =
            list_appendable_pending_threads(&pool, &channel, "test-profile", running_id)
                .await
                .expect("list appendable");
        let ids: Vec<i64> = appendable.iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            vec![pending_id],
            "same-chat follow-up must merge into the running telegram thread"
        );

        // End-to-end: mark the pending thread merged through the terminal
        // choke point used by main_loop after appending its sub-prompt.
        let n = mark_thread_merged_for_sub_prompt(&pool, pending_id, running_id)
            .await
            .expect("mark merged");
        assert_eq!(n, 1, "exactly the follow-up thread is flipped to merged");
        let row: (String, bool) =
            sqlx::query_as("SELECT status, terminal FROM threads WHERE id = $1")
                .bind(pending_id)
                .fetch_one(&pool)
                .await
                .expect("fetch merged thread");
        assert_eq!(row.0, "merged", "follow-up thread status is merged");
        assert!(row.1, "merged thread is terminal");

        cleanup_threads(&pool, &[running_id, pending_id]).await;
    }

    #[tokio::test]
    async fn follow_up_after_thread_completed_runs_standalone() {
        // Control: an idle follow-up sent after the previous same-chat
        // thread COMPLETED is not merged anywhere. The merge only fires
        // from inside a processing thread's main loop; with no active
        // same-chat runner the follow-up is claimed later and runs as its
        // own thread (single final answer for itself).
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");
        let channel = chan("tg-idle");

        // Previous same-chat thread already finished (terminal).
        let prev_id: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile, terminal) \
             VALUES ('completed', 'user', $1, 'test-profile', true) RETURNING id",
        )
        .bind(&channel)
        .fetch_one(&pool)
        .await
        .expect("insert completed thread");
        sqlx::query(
            "INSERT INTO messages (thread_id, thread_sequence, role, content, msg_type, \
             msg_subtype, external_id, metadata, channel_id) \
             VALUES ($1, 0, 'cause', 'old prompt', 'Cause', 'telegram', 'tg-old', $2::jsonb, $3)",
        )
        .bind(prev_id)
        .bind(serde_json::json!({"root_id": "chat-9"}).to_string())
        .bind(&channel)
        .execute(&pool)
        .await
        .expect("insert completed cause");

        // The idle follow-up is pending at arrival time, then claimed as the
        // new runner: nothing else is pending in the chat, so there is no
        // sibling to absorb and it is never marked merged.
        let follow_up = insert_user_thread(
            &pool,
            &channel,
            "processing",
            "tg-new",
            Some("chat-9"),
            None,
            "telegram",
        )
        .await;

        let appendable =
            list_appendable_pending_threads(&pool, &channel, "test-profile", follow_up)
                .await
                .expect("list appendable");
        assert!(
            appendable.is_empty(),
            "idle follow-up after a completed sibling has nothing to merge"
        );
        let row: (String, bool) =
            sqlx::query_as("SELECT status, terminal FROM threads WHERE id = $1")
                .bind(follow_up)
                .fetch_one(&pool)
                .await
                .expect("fetch follow-up");
        assert_eq!(row.0, "processing", "idle follow-up runs as its own thread");
        assert!(!row.1, "idle follow-up is not terminal");

        cleanup_threads(&pool, &[prev_id, follow_up]).await;
    }

    #[tokio::test]
    async fn mattermost_same_root_siblings_merge_after_root_thread_gone() {
        // Mattermost: two sequential replies inside the same root thread,
        // sent after the root thread finished and its row no longer exists.
        // threads.parent_id was never resolved (NULL), but both seq-0
        // messages carry the same root post id in metadata 'root_id', so
        // the second reply merges into the first once the first runs.
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");
        let channel = chan("mm-siblings");

        let running_id = insert_user_thread(
            &pool,
            &channel,
            "processing",
            "mm-r1",
            Some("root-post-77"),
            None,
            "mattermost",
        )
        .await;
        let pending_id = insert_user_thread(
            &pool,
            &channel,
            "pending",
            "mm-r2",
            Some("root-post-77"),
            None,
            "mattermost",
        )
        .await;

        let appendable =
            list_appendable_pending_threads(&pool, &channel, "test-profile", running_id)
                .await
                .expect("list appendable");
        let ids: Vec<i64> = appendable.iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            vec![pending_id],
            "same-root sibling reply must merge into the running sibling"
        );

        cleanup_threads(&pool, &[running_id, pending_id]).await;
    }

    #[tokio::test]
    async fn top_level_messages_without_parent_never_merge() {
        // Control: top-level Mattermost channel messages carry no parent
        // external id. Two of them in one channel remain separate threads
        // even while the first is running.
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");
        let channel = chan("mm-top");

        let running_id = insert_user_thread(
            &pool,
            &channel,
            "processing",
            "mm-1",
            None,
            None,
            "mattermost",
        )
        .await;
        let pending_id =
            insert_user_thread(&pool, &channel, "pending", "mm-2", None, None, "mattermost").await;

        let appendable =
            list_appendable_pending_threads(&pool, &channel, "test-profile", running_id)
                .await
                .expect("list appendable");
        assert!(
            appendable.is_empty(),
            "top-level messages without a parent external id must not merge"
        );

        cleanup_threads(&pool, &[running_id, pending_id]).await;
    }

    #[tokio::test]
    async fn direct_child_of_running_thread_still_appendable() {
        // Case A unchanged: a pending thread whose resolved parent_id IS the
        // running thread (a reply to its seq-0 cause) still merges.
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");
        let channel = chan("case-a");

        let running_id = insert_user_thread(
            &pool,
            &channel,
            "processing",
            "mm-1",
            None,
            None,
            "mattermost",
        )
        .await;
        let child_id = insert_user_thread(
            &pool,
            &channel,
            "pending",
            "mm-2",
            None,
            Some(running_id),
            "mattermost",
        )
        .await;

        let appendable =
            list_appendable_pending_threads(&pool, &channel, "test-profile", running_id)
                .await
                .expect("list appendable");
        let ids: Vec<i64> = appendable.iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            vec![child_id],
            "direct child of the running thread still merges"
        );

        cleanup_threads(&pool, &[running_id, child_id]).await;
    }

    #[tokio::test]
    async fn resolved_shared_parent_siblings_still_appendable() {
        // Case B unchanged (resolved): two threads whose parent_id resolved
        // to the same existing root thread row still merge via the internal
        // parent_id relation.
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");
        let channel = chan("case-b-resolved");

        let root_id: i64 = sqlx::query_scalar(
            "INSERT INTO threads (status, cause, channel_id, profile, terminal) \
             VALUES ('completed', 'user', $1, 'test-profile', true) RETURNING id",
        )
        .bind(&channel)
        .fetch_one(&pool)
        .await
        .expect("insert root thread");

        let running_id = insert_user_thread(
            &pool,
            &channel,
            "processing",
            "mm-c1",
            None,
            Some(root_id),
            "mattermost",
        )
        .await;
        let pending_id = insert_user_thread(
            &pool,
            &channel,
            "pending",
            "mm-c2",
            None,
            Some(root_id),
            "mattermost",
        )
        .await;

        let appendable =
            list_appendable_pending_threads(&pool, &channel, "test-profile", running_id)
                .await
                .expect("list appendable");
        let ids: Vec<i64> = appendable.iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            vec![pending_id],
            "resolved same-parent siblings still merge"
        );

        cleanup_threads(&pool, &[running_id, pending_id, root_id]).await;
    }

    #[tokio::test]
    async fn different_parent_external_id_does_not_merge() {
        // Control for the external-id branch: a different parent external
        // id (another telegram chat, another Mattermost root) must never
        // merge even inside the same channel.
        let Ok(db_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let _db_guard = crate::db::DB_TEST_LOCK.lock().await;
        let pool = sqlx::PgPool::connect(&db_url)
            .await
            .expect("connect dev db");
        let channel = chan("diff-root");

        let running_id = insert_user_thread(
            &pool,
            &channel,
            "processing",
            "tg-1",
            Some("chat-a"),
            None,
            "telegram",
        )
        .await;
        let other_id = insert_user_thread(
            &pool,
            &channel,
            "pending",
            "tg-2",
            Some("chat-b"),
            None,
            "telegram",
        )
        .await;

        let appendable =
            list_appendable_pending_threads(&pool, &channel, "test-profile", running_id)
                .await
                .expect("list appendable");
        assert!(
            appendable.is_empty(),
            "different parent external ids must not merge"
        );

        cleanup_threads(&pool, &[running_id, other_id]).await;
    }
}
