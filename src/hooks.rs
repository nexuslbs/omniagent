//! Event-driven Hooks engine.
//!
//! Hooks mirror cron schedule jobs but are triggered by events instead of a
//! time schedule. Supported events:
//!   - `thread_started`  : a thread is created
//!   - `thread_finished` : a thread reaches a terminal state
//!   - `new_message`     : a message is inserted
//!
//! Definitions live in `{data_dir}/config/tasks.yml` (`hooks:` key - the
//! git-tracked source of truth), NOT in the (dormant) `hooks` table. The
//! only runtime state is the per-hook JSON counter, stored in the small
//! `hook_counters (hook_key, counter)` table.
//!
//! Semantics:
//!   - Each hook has a JSON counter (per-scope keys) that increments on every
//!     matching event. When the counter reaches the hook's `count` threshold
//!     the hook triggers and the specific counter resets.
//!   - Scope: `global` (single counter), `channel` (per-channel counters,
//!     optional single named channel filter), `profile` (per-profile
//!     counters, optional single named profile filter).
//!   - Execution modes: `agentic` (spawn a hook-caused agent thread) and
//!     `action` (execute a predefined action from actions.yml).
//!   - Infinite-loop protection: hook-caused threads (`threads.hook_caused`)
//!     and their messages never trigger events.
//!   - Error isolation: every `fire_*` function spawns a fire-and-forget
//!     tokio task; all failures are logged and NEVER propagate to the caller
//!     (the main agent / message processing loop).

use chrono::Utc;
use serde_json::{json, Value};
use sql_forge::sql_forge;
use sqlx::{FromRow, PgPool};
use std::sync::{Arc, OnceLock};
use tracing::{error, info, warn};

use crate::agent::plugin_manager::PluginManager;
use crate::db::types as queries;
use crate::error::{AppResult, Error};
use crate::mcp::{AppContext, McpToolCall};

// ── Event / scope / mode constants ──────────────────────────────────────────

pub const EVENT_THREAD_STARTED: &str = "thread_started";
pub const EVENT_THREAD_FINISHED: &str = "thread_finished";
pub const EVENT_NEW_MESSAGE: &str = "new_message";

pub const SCOPE_GLOBAL: &str = "global";
pub const SCOPE_CHANNEL: &str = "channel";
pub const SCOPE_PROFILE: &str = "profile";

pub const MODE_AGENTIC: &str = "agentic";
pub const MODE_ACTION: &str = "action";

pub const VALID_EVENTS: [&str; 3] = [
    EVENT_THREAD_STARTED,
    EVENT_THREAD_FINISHED,
    EVENT_NEW_MESSAGE,
];
pub const VALID_SCOPES: [&str; 3] = [SCOPE_GLOBAL, SCOPE_CHANNEL, SCOPE_PROFILE];
pub const VALID_MODES: [&str; 2] = [MODE_AGENTIC, MODE_ACTION];

// ── Engine ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct HooksEngine {
    pool: PgPool,
    data_dir: String,
    plugin_manager: Arc<dyn PluginManager>,
    app_context: AppContext,
}

impl HooksEngine {
    pub fn new(
        pool: PgPool,
        data_dir: String,
        plugin_manager: Arc<dyn PluginManager>,
        app_context: AppContext,
    ) -> Self {
        Self {
            pool,
            data_dir,
            plugin_manager,
            app_context,
        }
    }
}

static ENGINE: OnceLock<HooksEngine> = OnceLock::new();

/// Initialize the global hooks engine (called once at startup).
pub fn init(engine: HooksEngine) {
    let _ = ENGINE.set(engine);
}

/// Fire a `thread_started` event. Never blocks or fails the caller.
pub fn fire_thread_started(thread_id: i64) {
    dispatch(
        move |engine| async move { engine.handle_event(EVENT_THREAD_STARTED, thread_id).await },
    );
}

/// Fire a `thread_finished` event. Never blocks or fails the caller.
pub fn fire_thread_finished(thread_id: i64) {
    dispatch(
        move |engine| async move { engine.handle_event(EVENT_THREAD_FINISHED, thread_id).await },
    );
}

/// Fire a `new_message` event. Never blocks or fails the caller.
pub fn fire_new_message(thread_id: i64, message_id: i64) {
    dispatch(move |engine| async move {
        engine
            .handle_event_with_message(EVENT_NEW_MESSAGE, thread_id, message_id)
            .await
    });
}

fn dispatch<F, Fut>(f: F)
where
    F: FnOnce(HooksEngine) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = AppResult<()>> + Send + 'static,
{
    let Some(engine) = ENGINE.get() else {
        return; // engine not initialized (unit tests / early boot): hooks inert
    };
    tokio::spawn(async move {
        if let Err(e) = f(engine.clone()).await {
            error!("[hooks] event handler failed: {:#}", e);
        }
    });
}

// ── Row structs ─────────────────────────────────────────────────────────────

/// Thread projection used for scope resolution + infinite-loop protection.
#[derive(Debug, FromRow)]
struct EventThreadRow {
    id: i64,
    channel_id: String,
    channel_name: String,
    profile: String,
    hook_caused: bool,
}

/// A resolved hook definition (from tasks.yml + channel resolution).
#[derive(Debug, Clone)]
struct HookRow {
    id: String,
    name: String,
    event: String,
    scope: String,
    target: Option<String>,
    count: i32,
    mode: String,
    prompt: Option<String>,
    action_id: Option<String>,
    profile: Option<String>,
    channel_id: Option<String>,
    plan: Option<bool>,
    template: Option<String>,
}

impl HookRow {
    fn from_yml(key: &str, def: &crate::tasks_yaml::HookDef, channel_id: Option<String>) -> Self {
        let plan = def.plan();
        Self {
            id: key.to_string(),
            name: key.to_string(),
            event: def.event.clone(),
            scope: def.scope.clone(),
            target: def.target.clone(),
            count: def.count,
            mode: def.mode(),
            prompt: def.prompt.clone(),
            action_id: def.action.clone(),
            profile: def.profile.clone(),
            channel_id,
            plan,
            template: def.template.clone(),
        }
    }
}

// ── Event handling ──────────────────────────────────────────────────────────

impl HooksEngine {
    async fn load_event_thread(&self, thread_id: i64) -> AppResult<Option<EventThreadRow>> {
        let row: Option<EventThreadRow> = sql_forge!(
            EventThreadRow,
            r#"
            SELECT t.id, t.channel_id, t.channel_id AS channel_name, t.profile, t.hook_caused
            FROM threads t
            WHERE t.id = :thread_id
            "#,
            ( :thread_id = thread_id )
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Resolve the current message id for thread_started / thread_finished
    /// events: the thread's last message (highest `messages.id`); when the
    /// thread has no messages, the last message id in the DB; None when the
    /// DB has no messages at all.
    async fn resolve_current_message(&self, thread_id: i64) -> AppResult<Option<i64>> {
        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT MAX(id) FROM messages WHERE thread_id = $1")
                .bind(thread_id)
                .fetch_optional(&self.pool)
                .await?;
        if let Some((Some(id),)) = row {
            return Ok(Some(id));
        }
        let row: Option<(Option<i64>,)> = sqlx::query_as("SELECT MAX(id) FROM messages")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|(id,)| id))
    }

    /// Load enabled hooks for an event from tasks.yml (parsed fresh on every
    /// event so file edits take effect without restart). Channel NAMEs are
    /// resolved to ids; unknown names → None (default channel semantics).
    async fn load_enabled_hooks(&self, event: &str) -> AppResult<Vec<HookRow>> {
        let tasks = crate::tasks_yaml::load_tasks_or_empty(&self.data_dir);
        let mut rows: Vec<HookRow> = Vec::new();
        for (key, def) in &tasks.hooks {
            if !def.enabled || def.event != event {
                continue;
            }
            let channel_id =
                crate::tasks_yaml::resolve_channel_id(&self.pool, def.channel.as_deref()).await;
            rows.push(HookRow::from_yml(key, def, channel_id));
        }
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(rows)
    }

    async fn handle_event(&self, event: &str, thread_id: i64) -> AppResult<()> {
        self.handle_event_with_message(event, thread_id, 0).await
    }

    /// Shared event pipeline for all three event types.
    ///
    /// 1. Infinite-loop protection: hook-caused threads never trigger events.
    /// 2. Load enabled hooks for the event type.
    /// 3. Per-hook scope resolution + counter increment; trigger + reset when
    ///    the counter reaches the threshold. Trigger failures are isolated
    ///    (logged, never propagated, never affect other hooks).
    async fn handle_event_with_message(
        &self,
        event: &str,
        thread_id: i64,
        message_id: i64,
    ) -> AppResult<()> {
        let Some(thread) = self.load_event_thread(thread_id).await? else {
            return Ok(()); // thread deleted: nothing to trigger on
        };
        if thread.hook_caused {
            // Infinite-loop protection: hook-caused threads (and, via them,
            // every message inside them) must not trigger events.
            return Ok(());
        }
        // Guard: threads (or messages from threads) with no channel or no
        // profile never trigger hooks - return early, no hook is evaluated.
        if !thread_has_channel_and_profile(&thread.channel_id, &thread.profile) {
            return Ok(());
        }

        // `current_message`: new_message events carry the inserted message id;
        // thread_started / thread_finished resolve the thread's last message
        // (fallback: the last message id in the DB).
        let current_message: Option<i64> = if event == EVENT_NEW_MESSAGE {
            (message_id > 0).then_some(message_id)
        } else {
            self.resolve_current_message(thread_id).await?
        };

        let hooks = self.load_enabled_hooks(event).await?;
        for hook in hooks {
            let Some(key) = scope_key(
                &hook.scope,
                hook.target.as_deref(),
                &thread.channel_name,
                &thread.profile,
            ) else {
                continue; // out of scope: ignored by this hook
            };
            if let Err(e) = self
                .record_and_maybe_trigger(&hook, &key, &thread, current_message)
                .await
            {
                error!(
                    "[hooks] hook '{}' ({}) event={} failed: {:#}",
                    hook.name, hook.id, hook.event, e
                );
            }
        }
        Ok(())
    }

    /// Atomically increment the hook's counter for `key` (in the
    /// `hook_counters` table); when the new value reaches `count`, reset the
    /// counter and trigger the hook AFTER commit.
    async fn record_and_maybe_trigger(
        &self,
        hook: &HookRow,
        key: &str,
        thread: &EventThreadRow,
        current_message: Option<i64>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        let counter_row: Option<(String,)> = sqlx::query_as(
            "SELECT counter::text AS counter FROM hook_counters WHERE hook_key = $1 FOR UPDATE",
        )
        .bind(&hook.id)
        .fetch_optional(&mut *tx)
        .await?;

        // No row yet → counter starts at the default ({"global": 0}).
        let mut counter: Value = counter_row
            .and_then(|(counter,)| serde_json::from_str(&counter).ok())
            .unwrap_or_else(default_counter);
        // Pre-trigger meta: the ids of the PREVIOUS trigger FOR THIS SCOPE
        // KEY. The event is delivered with these; this trigger's ids are
        // persisted below.
        let (last_thread, last_message) = meta_get(&counter, &hook.scope, key);
        let new_value = counter_increment(&mut counter, &hook.scope, key);
        let should_trigger = new_value >= hook.count as i64;
        if should_trigger {
            counter_reset(&mut counter, &hook.scope, key);
            // Persist this trigger's ids as this scope key's next `meta` -
            // atomically with the counter reset (same tx). `meta` is nested
            // per scope key, so counter increments/resets never clobber it.
            meta_update(
                &mut counter,
                &hook.scope,
                key,
                Some(thread.id),
                current_message,
            );
        }

        sqlx::query(
            "INSERT INTO hook_counters (hook_key, counter) VALUES ($1, $2) \
             ON CONFLICT (hook_key) DO UPDATE SET counter = EXCLUDED.counter",
        )
        .bind(&hook.id)
        .bind(&counter)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        if should_trigger {
            info!(
                "[hooks] hook '{}' ({}) triggered: event={}, scope={}, key='{}'",
                hook.name, hook.id, hook.event, hook.scope, key
            );
            // Build the event from the PRE-trigger meta (previous trigger's
            // ids) + this trigger's ids, and deliver it to the execution
            // target. Trigger AFTER commit so a failing trigger does not roll
            // back the counter reset. Failures are logged, never propagated.
            let event = build_event(
                last_thread,
                last_message,
                Some(thread.id),
                current_message,
                &thread.channel_id,
                &thread.profile,
            );
            if let Err(e) = self.trigger(hook, thread, &event).await {
                error!(
                    "[hooks] hook '{}' ({}) trigger failed: {:#}",
                    hook.name, hook.id, e
                );
            }
        }
        Ok(())
    }

    /// Execute the hook: agentic mode spawns a hook-caused thread; action mode
    /// executes the configured action. Returns the spawned thread id (agentic)
    /// or None (action).
    async fn trigger(
        &self,
        hook: &HookRow,
        thread: &EventThreadRow,
        event: &Value,
    ) -> AppResult<Option<i64>> {
        match hook.mode.as_str() {
            MODE_ACTION => {
                self.run_action(hook, event).await?;
                Ok(None)
            }
            _ => self.run_agentic(hook, thread, event).await,
        }
    }

    /// Agentic mode: create a hook-caused agent thread (mirrors cron agentic
    /// jobs). The spawned thread is marked `hook_caused` so it never
    /// re-triggers hooks (infinite-loop protection).
    async fn run_agentic(
        &self,
        hook: &HookRow,
        thread: &EventThreadRow,
        event: &Value,
    ) -> AppResult<Option<i64>> {
        // Resolution chain: hook's explicit channel -> triggering thread's
        // channel -> default_hook_channel -> '' (empty = the thread is
        // created and then failed with "no channel defined"; the record is
        // kept for audit).
        let channel_id = hook
            .channel_id
            .as_deref()
            .filter(|c| !c.trim().is_empty() && crate::channels_yaml::exists(c.trim()))
            .map(str::to_string)
            .or_else(|| {
                (!thread.channel_id.trim().is_empty()
                    && crate::channels_yaml::exists(&thread.channel_id))
                .then(|| thread.channel_id.clone())
            })
            .or_else(|| crate::channels_yaml::resolve_default_channel(None, "default_hook_channel"))
            .unwrap_or_default();
        let profile = hook
            .profile
            .clone()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| thread.profile.clone());
        let prompt = {
            let p = hook.prompt.clone().unwrap_or_default();
            if p.trim().is_empty() {
                format!("Hook '{}' fired (event: {})", hook.name, hook.event)
            } else {
                p
            }
        };
        // The full event object is embedded as JSON in the spawned thread's
        // prompt so the hook thread can react to what happened since the last
        // trigger ("<hook prompt>\n\nEvent: <json>").
        let event_json = serde_json::to_string(event).unwrap_or_default();
        let prompt = format!("{}\n\nEvent: {}", prompt, event_json);

        let metadata = json!({
            "hook_id": hook.id,
            "hook_name": hook.name,
            "hook_event": hook.event,
        });

        let external_id = format!("hook:{}:{}", hook.id, Utc::now().timestamp_millis());
        let (thread, _msg) = queries::create_thread_with_cause(
            &self.pool,
            &self.data_dir,
            "system",
            &channel_id,
            &profile,
            queries::ThreadCauseParams {
                provider: None,
                model: None,
                task_id: None,
                schedule_task_id: None,
                content: prompt,
                external_id: Some(external_id),
                parent_external_id: None,
                metadata,
                msg_type: "hook".to_string(),
                msg_subtype: Some(hook.name.clone()),
                // plan=true forces planning ON; false/absent = no explicit
                // preference (the prompt plugin / channel decides at runtime).
                task_plan: if hook.plan.unwrap_or(false) {
                    Some(true)
                } else {
                    None
                },
                template: hook.template.clone(),
                workflow_id: None,
                workflow_step: None,
                hook_caused: true,
            },
        )
        .await?;

        info!(
            "[hooks] agentic hook '{}' ({}) created thread #{}",
            hook.name, hook.id, thread.id
        );
        Ok(Some(thread.id))
    }

    /// Action mode: resolve the action from actions.yml and execute it via the
    /// plugin registry (non-agentic, mirrors cron direct-action mode).
    async fn run_action(&self, hook: &HookRow, event: &Value) -> AppResult<()> {
        let action_id = hook.action_id.clone().unwrap_or_default();
        if action_id.trim().is_empty() {
            return Err(Error::Message(format!(
                "hook '{}' ({}) has mode=action but no action_id",
                hook.name, hook.id
            )));
        }
        let mut tool_call: McpToolCall =
            crate::scheduler::resolve_action(&self.data_dir, &action_id)?;
        // The event object is merged into the action's arguments under the
        // well-known `event` key. Merge order: static `params` first, then the
        // event - the event WINS on key collision (trigger-specific data).
        if let Value::Object(args) = &mut tool_call.arguments {
            args.insert("event".to_string(), event.clone());
        } else {
            tool_call.arguments = json!({ "event": event });
        }
        let snapshot = self.plugin_manager.snapshot_registry().await;
        match snapshot.execute(&tool_call, self.app_context.clone()).await {
            Ok(result) => {
                if result.is_error {
                    warn!(
                        "[hooks] action hook '{}' ({}) tool '{}' returned error: {}",
                        hook.name, hook.id, tool_call.name, result.content
                    );
                } else {
                    info!(
                        "[hooks] action hook '{}' ({}) executed tool '{}'",
                        hook.name, hook.id, tool_call.name
                    );
                }
                Ok(())
            }
            Err(e) => Err(Error::Message(format!(
                "action '{}' execution failed: {}",
                action_id, e
            ))),
        }
    }
}

// ── Pure scope/counter logic (unit-testable) ────────────────────────────────

/// The default counter document: a single `global` key at 0.
pub fn default_counter() -> Value {
    json!({ "global": 0 })
}

/// Resolve the counter key for a hook/event pair and apply scope filtering.
///
/// Returns:
/// - `Some("global")` for global scope (no filtering).
/// - `Some(channel_name)` for channel scope: target None = all channels
///   (one counter per channel); target Some(t) = only that named channel -
///   events from other channels return `None` (ignored).
/// - `Some(profile)` for profile scope (same filter logic).
/// - `None` when the event is out of scope for this hook.
pub fn scope_key(
    scope: &str,
    target: Option<&str>,
    channel_name: &str,
    profile: &str,
) -> Option<String> {
    let target = target.map(str::trim).filter(|t| !t.is_empty());
    match scope {
        SCOPE_CHANNEL => match target {
            Some(t) if t == channel_name => Some(channel_name.to_string()),
            Some(_) => None, // single named channel mismatch → ignored
            None => Some(channel_name.to_string()),
        },
        SCOPE_PROFILE => match target {
            Some(t) if t == profile => Some(profile.to_string()),
            Some(_) => None,
            None => Some(profile.to_string()),
        },
        _ => Some(SCOPE_GLOBAL.to_string()),
    }
}

/// Read the current counter value for a scope+key.
pub fn counter_get(counter: &Value, scope: &str, key: &str) -> i64 {
    match scope {
        SCOPE_CHANNEL => counter["channel"][key].as_i64().unwrap_or(0),
        SCOPE_PROFILE => counter["profile"][key].as_i64().unwrap_or(0),
        _ => counter["global"].as_i64().unwrap_or(0),
    }
}

fn counter_set(counter: &mut Value, scope: &str, key: &str, val: i64) {
    match scope {
        SCOPE_CHANNEL | SCOPE_PROFILE => {
            if let Value::Object(root) = counter {
                let section = root.entry(scope.to_string()).or_insert_with(|| json!({}));
                if let Value::Object(inner) = section {
                    inner.insert(key.to_string(), json!(val));
                }
            }
        }
        _ => {
            if let Value::Object(root) = counter {
                root.insert(SCOPE_GLOBAL.to_string(), json!(val));
            }
        }
    }
}

/// Increment the counter for scope+key; returns the NEW value.
pub fn counter_increment(counter: &mut Value, scope: &str, key: &str) -> i64 {
    let val = counter_get(counter, scope, key) + 1;
    counter_set(counter, scope, key, val);
    val
}

/// Reset the counter for scope+key to 0 (all other counters untouched).
pub fn counter_reset(counter: &mut Value, scope: &str, key: &str) {
    counter_set(counter, scope, key, 0);
}

/// Read the per-scope-key `meta` section of a counter document - the ids of
/// the last time the hook was triggered FOR THIS scope key. Returns
/// `(None, None)` before the first trigger of that key.
///
/// `meta` is stored per scope key (`meta[scope][key]` for channel/profile
/// scopes, a single `meta["global"]` entry for the global scope) so every
/// profile/channel keeps its OWN last_thread/last_message - a trigger in one
/// profile/channel must never leak its ids into another scope key's event.
pub fn meta_get(counter: &Value, scope: &str, key: &str) -> (Option<i64>, Option<i64>) {
    let entry = match scope {
        SCOPE_CHANNEL | SCOPE_PROFILE => counter["meta"][scope].get(key),
        _ => counter["meta"].get(SCOPE_GLOBAL),
    };
    let last_thread = entry.and_then(|e| e["last_thread"].as_i64());
    let last_message = entry.and_then(|e| e["last_message"].as_i64());
    (last_thread, last_message)
}

/// Write the per-scope-key `meta` section of a counter document. Both keys
/// are ALWAYS written (null when the id is unknown). `meta` is nested per
/// scope key (`meta[scope][key]` for channel/profile scopes, `meta["global"]`
/// for the global scope), alongside `global` / `channel` / `profile`, so the
/// counter accessors (`counter_get` / `counter_set` / `counter_increment` /
/// `counter_reset`) never touch it.
pub fn meta_update(
    counter: &mut Value,
    scope: &str,
    key: &str,
    last_thread: Option<i64>,
    last_message: Option<i64>,
) {
    let entry = json!({
        "last_thread": last_thread,
        "last_message": last_message,
    });
    if let Value::Object(root) = counter {
        let meta = root.entry("meta").or_insert_with(|| json!({}));
        if let Value::Object(meta) = meta {
            match scope {
                SCOPE_CHANNEL | SCOPE_PROFILE => {
                    let section = meta.entry(scope.to_string()).or_insert_with(|| json!({}));
                    if let Value::Object(section) = section {
                        section.insert(key.to_string(), entry);
                    }
                }
                _ => {
                    meta.insert(SCOPE_GLOBAL.to_string(), entry);
                }
            }
        }
    }
}

/// Build the event object delivered to a hook's execution target.
///
/// All six keys are ALWAYS present; unknown ids are serialized as `null`:
/// on the first trigger `last_thread` / `last_message` are null; on a
/// manual fire `current_thread` / `current_message` are null.
pub fn build_event(
    last_thread: Option<i64>,
    last_message: Option<i64>,
    current_thread: Option<i64>,
    current_message: Option<i64>,
    channel: &str,
    profile: &str,
) -> Value {
    json!({
        "last_thread": last_thread,
        "last_message": last_message,
        "current_thread": current_thread,
        "current_message": current_message,
        "channel": channel,
        "profile": profile,
    })
}

/// Guard: hooks must never be triggered by threads (or messages from
/// threads) that have no channel or no profile.
pub fn thread_has_channel_and_profile(channel_id: &str, profile: &str) -> bool {
    !channel_id.trim().is_empty() && !profile.trim().is_empty()
}

// ── Manual fire (REST API: POST /hooks/{id}/fire) ───────────────────────────

/// Manually trigger a hook by id (no counter increment / reset). Resolves the
/// execution channel: the hook's explicit channel, or the cron channel as a
/// fallback. Profile: the hook's explicit profile, or the channel's profile.
/// Returns the spawned thread id (agentic mode) or None (action mode).
/// Reads the hook definition from tasks.yml.
pub async fn fire_hook_by_id(
    pool: &PgPool,
    data_dir: &str,
    plugin_manager: &Arc<dyn PluginManager>,
    app_context: &AppContext,
    hook_id: &str,
) -> AppResult<Option<i64>> {
    let engine = HooksEngine::new(
        pool.clone(),
        data_dir.to_string(),
        plugin_manager.clone(),
        app_context.clone(),
    );
    let tasks = crate::tasks_yaml::load_tasks(data_dir)?;
    let def = tasks
        .hooks
        .get(hook_id)
        .ok_or_else(|| Error::Message(format!("Hook '{}' not found", hook_id)))?;
    let channel_id = crate::tasks_yaml::resolve_channel_id(pool, def.channel.as_deref()).await;
    let hook = HookRow::from_yml(hook_id, def, channel_id);

    // Resolution chain: explicit channel -> default_hook_channel -> ''
    // (empty = the thread is created and then failed with "no channel
    // defined"; the record is kept for audit).
    let channel_id = crate::channels_yaml::resolve_default_channel(
        def.channel.as_deref(),
        "default_hook_channel",
    )
    .unwrap_or_default();
    let channel = if channel_id.is_empty() {
        None
    } else {
        crate::db::channels::get_channel_by_id(pool, &channel_id).await?
    };
    let profile = hook
        .profile
        .clone()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| channel.as_ref().map(|c| c.current_profile.clone()))
        .unwrap_or_else(crate::profile::default_profile_name);
    let thread_ctx = EventThreadRow {
        id: 0,
        channel_id,
        channel_name: channel.as_ref().map(|c| c.name.clone()).unwrap_or_default(),
        profile,
        hook_caused: false,
    };
    // Manual fire has no triggering thread/message: the event carries the last
    // trigger context from the stored counter meta (if any) and null
    // current_* ids. Manual fire does NOT write meta (no counter path).
    let stored: Option<(String,)> =
        sqlx::query_as("SELECT counter::text AS counter FROM hook_counters WHERE hook_key = $1")
            .bind(&hook.id)
            .fetch_optional(pool)
            .await?;
    // Manual fire reads the meta of the scope key the fired thread belongs
    // to (per-scope-key meta); no key (out of scope) -> no last-trigger ids.
    let fire_key = scope_key(
        &hook.scope,
        hook.target.as_deref(),
        &thread_ctx.channel_name,
        &thread_ctx.profile,
    );
    let (last_thread, last_message) = stored
        .and_then(|(c,)| serde_json::from_str::<Value>(&c).ok())
        .and_then(|c| fire_key.as_deref().map(|k| meta_get(&c, &hook.scope, k)))
        .unwrap_or((None, None));
    let event = build_event(
        last_thread,
        last_message,
        None,
        None,
        &thread_ctx.channel_id,
        &thread_ctx.profile,
    );
    engine.trigger(&hook, &thread_ctx, &event).await
}

// ── Unit tests (pure logic) ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_key_resolution() {
        // global: always the single global key
        assert_eq!(
            scope_key("global", None, "ch1", "omni"),
            Some("global".to_string())
        );
        assert_eq!(
            scope_key("global", Some("whatever"), "ch1", "omni"),
            Some("global".to_string())
        );
        // channel: all channels → per-channel key
        assert_eq!(
            scope_key("channel", None, "ch1", "omni"),
            Some("ch1".to_string())
        );
        // channel: named channel match / mismatch
        assert_eq!(
            scope_key("channel", Some("ch2"), "ch2", "omni"),
            Some("ch2".to_string())
        );
        assert_eq!(scope_key("channel", Some("ch2"), "ch1", "omni"), None);
        // profile: all profiles → per-profile key
        assert_eq!(
            scope_key("profile", None, "ch1", "omni"),
            Some("omni".to_string())
        );
        assert_eq!(
            scope_key("profile", Some("omni"), "ch1", "omni"),
            Some("omni".to_string())
        );
        assert_eq!(scope_key("profile", Some("other"), "ch1", "omni"), None);
        // empty target = all
        assert_eq!(
            scope_key("channel", Some(""), "ch1", "omni"),
            Some("ch1".to_string())
        );
    }

    #[test]
    fn counter_increment_reset_global() {
        let mut c = default_counter();
        assert_eq!(counter_get(&c, "global", "global"), 0);
        assert_eq!(counter_increment(&mut c, "global", "global"), 1);
        assert_eq!(counter_increment(&mut c, "global", "global"), 2);
        assert_eq!(counter_get(&c, "global", "global"), 2);
        counter_reset(&mut c, "global", "global");
        assert_eq!(counter_get(&c, "global", "global"), 0);
    }

    #[test]
    fn counter_isolation_per_channel() {
        let mut c = default_counter();
        assert_eq!(counter_increment(&mut c, "channel", "ch1"), 1);
        assert_eq!(counter_increment(&mut c, "channel", "ch1"), 2);
        assert_eq!(counter_increment(&mut c, "channel", "ch2"), 1);
        // ch1 reset leaves ch2 untouched
        counter_reset(&mut c, "channel", "ch1");
        assert_eq!(counter_get(&c, "channel", "ch1"), 0);
        assert_eq!(counter_get(&c, "channel", "ch2"), 1);
        // global untouched by channel increments
        assert_eq!(counter_get(&c, "global", "global"), 0);
    }

    #[test]
    fn counter_isolation_per_profile() {
        let mut c = default_counter();
        counter_increment(&mut c, "profile", "omni");
        counter_increment(&mut c, "profile", "omni");
        counter_increment(&mut c, "profile", "another");
        assert_eq!(counter_get(&c, "profile", "omni"), 2);
        assert_eq!(counter_get(&c, "profile", "another"), 1);
        counter_reset(&mut c, "profile", "another");
        assert_eq!(counter_get(&c, "profile", "omni"), 2);
        assert_eq!(counter_get(&c, "profile", "another"), 0);
    }

    #[test]
    fn threshold_trigger_reset_semantics() {
        // Simulate record_and_maybe_trigger's decision for count = 3:
        // 1st and 2nd events only increment; 3rd triggers and resets.
        let mut c = default_counter();
        let count = 3;
        assert!(counter_increment(&mut c, "global", "global") < count);
        assert!(counter_increment(&mut c, "global", "global") < count);
        assert_eq!(counter_increment(&mut c, "global", "global"), count);
        counter_reset(&mut c, "global", "global");
        assert_eq!(counter_get(&c, "global", "global"), 0);
        // next event starts the cycle again
        assert_eq!(counter_increment(&mut c, "global", "global"), 1);
    }

    #[test]
    fn counter_shape_matches_spec() {
        let mut c = default_counter();
        counter_increment(&mut c, "global", "global");
        counter_increment(&mut c, "channel", "channel1");
        counter_increment(&mut c, "channel", "channel2");
        counter_increment(&mut c, "profile", "omni");
        // Before any trigger the counter document has NO `meta` section.
        assert_eq!(
            c,
            json!({
                "global": 1,
                "channel": { "channel1": 1, "channel2": 1 },
                "profile": { "omni": 1 },
            })
        );
        // After a trigger, `meta` carries the trigger's ids per scope key,
        // alongside global/channel/profile - untouched by counter
        // increments/resets.
        meta_update(&mut c, "global", "global", Some(123), Some(456));
        assert_eq!(c["meta"]["global"]["last_thread"], json!(123));
        assert_eq!(c["meta"]["global"]["last_message"], json!(456));
        counter_increment(&mut c, "global", "global");
        counter_reset(&mut c, "global", "global");
        assert_eq!(
            c,
            json!({
                "global": 0,
                "channel": { "channel1": 1, "channel2": 1 },
                "profile": { "omni": 1 },
                "meta": { "global": { "last_thread": 123, "last_message": 456 } },
            })
        );
    }

    #[test]
    fn meta_update_roundtrip() {
        // No meta before any trigger.
        let mut c = default_counter();
        assert_eq!(meta_get(&c, "global", "global"), (None, None));
        // A trigger writes its ids; a later trigger overwrites them.
        meta_update(&mut c, "global", "global", Some(11), Some(22));
        assert_eq!(meta_get(&c, "global", "global"), (Some(11), Some(22)));
        meta_update(&mut c, "global", "global", Some(33), Some(44));
        assert_eq!(meta_get(&c, "global", "global"), (Some(33), Some(44)));
        // Unknown message id → null in the persisted doc, None on read.
        meta_update(&mut c, "global", "global", Some(55), None);
        assert_eq!(meta_get(&c, "global", "global"), (Some(55), None));
        assert_eq!(c["meta"]["global"]["last_message"], Value::Null);
        // Counter sections are untouched by meta writes.
        counter_increment(&mut c, "global", "global");
        assert_eq!(counter_get(&c, "global", "global"), 1);
        assert_eq!(meta_get(&c, "global", "global"), (Some(55), None));
    }

    #[test]
    fn meta_isolation_per_scope_key() {
        // Per-profile hook (no target): each profile keeps its OWN meta -
        // profile A's trigger must never leak ids into profile B's event.
        let mut c = default_counter();
        meta_update(&mut c, "profile", "omni", Some(11), Some(22));
        meta_update(&mut c, "profile", "other", Some(33), Some(44));
        assert_eq!(meta_get(&c, "profile", "omni"), (Some(11), Some(22)));
        assert_eq!(meta_get(&c, "profile", "other"), (Some(33), Some(44)));
        // Per-channel hook: same isolation between channels.
        meta_update(&mut c, "channel", "ch1", Some(100), Some(200));
        meta_update(&mut c, "channel", "ch2", Some(300), Some(400));
        assert_eq!(meta_get(&c, "channel", "ch1"), (Some(100), Some(200)));
        assert_eq!(meta_get(&c, "channel", "ch2"), (Some(300), Some(400)));
        // Scopes are fully separate: no cross-scope leakage.
        assert_eq!(meta_get(&c, "profile", "omni"), (Some(11), Some(22)));
        assert_eq!(meta_get(&c, "channel", "ch1"), (Some(100), Some(200)));
        assert_eq!(meta_get(&c, "global", "global"), (None, None));
        // Counter increments/resets leave per-key meta untouched.
        counter_increment(&mut c, "profile", "omni");
        counter_reset(&mut c, "profile", "omni");
        assert_eq!(meta_get(&c, "profile", "omni"), (Some(11), Some(22)));
        assert_eq!(c["meta"]["profile"]["omni"]["last_thread"], json!(11));
    }

    #[test]
    fn build_event_first_trigger() {
        // First trigger: last_* unknown → null; current_* = trigger ids.
        let e = build_event(None, None, Some(123), Some(456), "mattermost-abc", "omni");
        assert_eq!(
            e,
            json!({
                "last_thread": null,
                "last_message": null,
                "current_thread": 123,
                "current_message": 456,
                "channel": "mattermost-abc",
                "profile": "omni",
            })
        );
    }

    #[test]
    fn build_event_subsequent_trigger() {
        // Subsequent trigger: last_* = previous trigger's ids.
        let e = build_event(Some(100), Some(200), Some(123), Some(456), "ch", "omni");
        assert_eq!(e["last_thread"], json!(100));
        assert_eq!(e["last_message"], json!(200));
        assert_eq!(e["current_thread"], json!(123));
        assert_eq!(e["current_message"], json!(456));
        assert_eq!(e["channel"], json!("ch"));
        assert_eq!(e["profile"], json!("omni"));
    }

    #[test]
    fn manual_fire_event_shape() {
        // Manual fire: no triggering thread/message.
        let e = build_event(Some(7), Some(8), None, None, "mattermost-x", "omni");
        assert_eq!(
            e,
            json!({
                "last_thread": 7,
                "last_message": 8,
                "current_thread": null,
                "current_message": null,
                "channel": "mattermost-x",
                "profile": "omni",
            })
        );
    }

    #[test]
    fn no_channel_or_profile_guard() {
        assert!(!thread_has_channel_and_profile("", "omni"));
        assert!(!thread_has_channel_and_profile("   ", "omni"));
        assert!(!thread_has_channel_and_profile("mattermost-abc", ""));
        assert!(!thread_has_channel_and_profile("mattermost-abc", "   "));
        assert!(thread_has_channel_and_profile("mattermost-abc", "omni"));
    }
}
