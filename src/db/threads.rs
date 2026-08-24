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

/// Create a new thread — THE single INSERT for every thread creation path.
///
/// All thread rows (general message threads, kanban executor threads, workflow
/// step threads, engine re-runs, manual-review re-runs, skip-recovery
/// reschedules) MUST go through this function so the full column set
/// (plan, template, workflow_step, task_type, schedule_task_id, hook_caused)
/// is always persisted. Hand-rolled INSERTs elsewhere have repeatedly drifted:
/// step threads were created without `plan`/`template` (60-iteration no-plan
/// budget, no role guidance — threads 75-78, 82) and `hook_caused` was missed
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
    // profile/provider/model — creation fails instead of inserting empties.
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
/// 3. Profile setting (`profile_plan` — profiles.yml `plan`)
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

/// Copy a skipped thread's PERSISTED identity onto its re-scheduled
/// replacement (R3/startup recovery). Re-scheduled threads never re-resolve
/// provider/model/profile at runtime — they inherit the parent's creation-time
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
    // 'skipped' IS terminal — route it through the single choke point
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
                    // thread must carry the parent's full execution identity —
                    // including plan + template — or it silently runs with a
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

                    let cause = t.cause.clone().unwrap_or_else(|| "re-run".to_string());
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
/// thread row. Running threads never re-resolve it — the executor consumes
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
/// Returns `Err` when no profile/provider/model can be resolved — creation
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
    // with no channel (empty channel_id — the explicit -> default -> ''
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

    // Event-driven hooks: fire thread_finished on terminal transition.
    crate::hooks::fire_thread_finished(thread_id);

    Ok(())
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
                    let cause = t.cause.clone().unwrap_or_else(|| "re-run".to_string());
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
        r#"
        UPDATE threads
        SET status = :status,
            ended_at = NOW(),
            iterations = COALESCE(
                (SELECT MAX(iteration_number) FROM messages WHERE thread_id = :id),
                0
            ),
            terminal = true
        WHERE id = :id AND NOT terminal
        "#,
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

/// R8-J: an executor thread must ALWAYS carry a template — the role template
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
/// here — the caller owns the status transition.
///
/// Returns `Some(thread_id)` when a thread was created, `None` when the
/// status has no role to run (non-workflow `testing`/`review`, a workflow
/// without that role, or a non-workflow-column status).
///
/// `skip_stale` (true for status-change dispatch): any still-active
/// pending/processing threads for the task are marked skipped FIRST (through
/// the `mark_thread_terminal` choke point) so they cannot race the new step.
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
    skip_stale: bool,
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
    //     channel → global settings) — the universal resolution pattern. The
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

    // 3. Skip stale active threads (status-change dispatch only).
    if skip_stale {
        #[derive(sqlx::FromRow)]
        struct StaleThreadRow {
            id: i64,
        }
        let stale: Vec<StaleThreadRow> = sql_forge!(
            StaleThreadRow,
            r#"SELECT id FROM threads WHERE task_id = :task_id AND status IN ('pending', 'processing')"#,
            ( :task_id = task_id )
        )
        .fetch_all(pool)
        .await?;
        for t in &stale {
            mark_thread_terminal(pool, t.id, "skipped").await?;
            // Audit the skip in kanban history (best-effort, like the
            // existing re-schedule paths do).
            let comment = format!("Thread #{} skipped (status changed to '{}')", t.id, status);
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
                    "[kanban dispatch] history insert for skipped thread #{} failed: {:?}",
                    t.id, e
                )
            });
        }
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
    // 'failed' — the same shape the builtin fail-thread produces.
    let external_id = format!(
        "validation-error:{}:{}",
        thread.id,
        chrono::Utc::now().timestamp()
    );
    sql_forge!(
        r#"
        INSERT INTO messages
            (thread_id, role, content, thread_sequence, external_id, metadata,
             msg_type, msg_subtype, iteration_number, duration_ms, token_usage)
        VALUES
            (:tid, 'system', :content, 1, :external_id,
             :metadata::jsonb, 'error', :subtype, 0, 0, '{}'::jsonb)
        "#,
        (
            :tid = thread.id,
            :content = board_err,
            :external_id = external_id.as_str(),
            :metadata = serde_json::json!({ "error_type": "configuration" }),
            :subtype = format!("board:{}", task.id),
        )
    )
    .execute(pool)
    .await?;

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

/// Dispatch a kanban task for a target status: skip any stale active
/// threads, create the mapped role thread (running -> executor, testing ->
/// tester, review -> reviewer) and mark the task `thread_status='scheduled'`.
/// Does NOT change the task's own status — the caller owns the transition.
///
/// Returns `Some(thread_id)` when a thread was created, `None` when the
/// status has no role to run.
pub(crate) async fn dispatch_task_for_status(
    pool: &PgPool,
    data_dir: &str,
    task_id: &str,
    new_status: &str,
) -> AppResult<Option<i64>> {
    create_kanban_step_thread(pool, data_dir, task_id, new_status, true).await
}

/// Skip all pending/processing threads on startup, then redispatch every
/// kanban task sitting in a workflow column without an active thread.
///
/// The old per-thread re-schedule branch is gone: the unified startup
/// recovery marks every pending/processing thread terminal (single choke
/// point) and then re-creates the role thread for each kanban task in
/// `running`/`testing`/`review` that has NO active thread — the SAME code
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
        if let Err(e) =
            create_kanban_step_thread(pool, data_dir, &task.id, &task.status, false).await
        {
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
// Phase 2 — getters used by the builtin fail-thread tool / finalization guard
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
    //    DDL: rollback restores it) — old-data cleanup is the sanctioned
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

    // ---------- Thread identity resolution precedence ----------
    // Regression guard for 83f461b: the Aug 11 refactor reversed the
    // provider chain (profile.provider was checked BEFORE
    // channel.current_provider). That made kanban executor threads on the
    // wf-test channel (current_provider=noop) resolve to the omni profile's
    // deepseek provider — a REAL LLM call in tests that must never hit one.
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
        // Step threads (testing/review) without a role template stay None —
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
}

#[allow(clippy::items_after_test_module)]
/// List PENDING user threads whose prompt should be appended into the running
/// thread as a sub-prompt (feature: sub-prompts).
///
/// Match condition (per feature spec): same channel + same profile +
/// cause='user' + status='pending' + NOT terminal, AND
/// (pending.parent_id IS NOT DISTINCT FROM running.parent_id   -- same parent
///  context as the running thread, incl. both NULL = same top-level
///  OR pending.parent_id = running.id)                          -- child of the
///  running thread). Ordered by id ASC (oldest first).
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
          AND (t.parent_id IS NOT DISTINCT FROM (SELECT parent_id FROM threads WHERE id = :running_thread_id)
               OR t.parent_id = :running_thread_id)
        ORDER BY t.id ASC
        "#,
        ( :channel_id = channel_id, :profile = profile, :running_thread_id = running_thread_id )
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(|r| r.try_into()).collect()
}

#[allow(clippy::items_after_test_module)]
/// Mark a pending thread 'skipped' after its prompt was appended as a
/// sub-prompt into a running thread (feature: sub-prompts).
///
/// Uses the single terminal choke point (mark_thread_terminal) so
/// terminal=true is always set with 'skipped'. When the skipped thread is
/// linked to a kanban task, the task's thread_status is cleared and a history
/// entry records the skip (best-effort, mirrors skip_channel_threads).
pub async fn mark_thread_skipped_for_sub_prompt(pool: &PgPool, pending_id: i64) -> AppResult<u64> {
    let result = mark_thread_terminal(pool, pending_id, "skipped").await?;
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
                "Thread #{} skipped (prompt appended as sub-prompt to running thread)",
                pending_id
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
