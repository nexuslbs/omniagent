//! Generic PUBLISHED-EVENT bus (channel/agent agnostic).
//!
//! This is the core mechanism behind the human-intervention hand-off: a
//! producer (e.g. the browser/captcha solver) PUBLISHES a named event with a
//! bounded JSON payload and a correlation id. It never talks to a delivery
//! channel (no telegram, no email): delivery is done by LISTENERS.
//!
//! A LISTENER is a normal hook definition in `{data_dir}/config/tasks.yml`
//! whose `event` is the published event name and whose `mode` is `action`
//! (the action is resolved from `actions.yml`, exactly like hook actions).
//! Several hooks may listen to the same event: every one of them runs
//! independently (fan-out), each failure is isolated (recorded per listener,
//! never propagated to the producer or to the other listeners).
//!
//! Lifecycle of one interaction:
//!   publish("solve-captcha", correlation_id, payload)  -> listeners run
//!   publish("<event>-resolved"|"-aborted"|"-timeout", same correlation_id,
//!           payload)                                   -> outcome recorded
//!   wait(correlation_id, max_seconds)                  -> solved|aborted|timeout
//!
//! `wait` enforces the interaction deadline (payload `timeout_s`, default
//! 900 s): when it passes with no outcome, the bus itself publishes the
//! `-timeout` terminal event, so a waiter NEVER hangs.
//!
//! Authoritative second signal: any inbound operator/platform message observed
//! after the request was published counts as "the human acted" (an abort-ish
//! text yields `-aborted`, anything else `-solved`). It is delivered to the
//! producer as the terminal EVENT, never through a channel-specific call.
//!
//! State is in-process: the bus never writes to the DB (no thread, no message,
//! no new table), so a hand-off creates no thread/cause row anywhere.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use tracing::{info, warn};

use crate::agent::plugin_manager::PluginManager;
use crate::error::{AppResult, Error};
use crate::mcp::{AppContext, McpToolCall};

// ── Canonical event names ───────────────────────────────────────────────────

/// Base event: a browser session the agent drives needs a human (challenge it
/// must not solve itself).
pub const EVENT_SOLVE_CAPTCHA: &str = "solve-captcha";

pub const OUTCOME_SOLVED: &str = "solved";
pub const OUTCOME_ABORTED: &str = "aborted";
pub const OUTCOME_TIMEOUT: &str = "timeout";

/// Default interaction deadline when the payload carries no `timeout_s`.
pub const DEFAULT_TIMEOUT_S: i64 = 900;
/// How often a waiter re-checks the interaction state.
const WAIT_POLL_MS: u64 = 250;
/// Hard bound for ONE listener action (a hanging listener must never hang the
/// producer).
const LISTENER_TIMEOUT_S: u64 = 30;

/// The terminal event name for a base event + outcome
/// (`solve-captcha` + `solved` -> `solve-captcha-resolved`).
pub fn terminal_event(base: &str, outcome: &str) -> String {
    // The solved outcome is canonically spelled `-resolved`
    // (`solve-captcha-resolved`), the other two use the outcome verbatim.
    let suffix = if outcome == OUTCOME_SOLVED {
        "resolved"
    } else {
        outcome
    };
    format!("{}-{}", base, suffix)
}

/// The outcome carried by a terminal event name (`<base>-<outcome>`), `None`
/// when the event is not a terminal event.
pub fn outcome_for_event(event: &str) -> Option<&'static str> {
    if event.ends_with("-resolved") {
        Some(OUTCOME_SOLVED)
    } else if event.ends_with("-aborted") {
        Some(OUTCOME_ABORTED)
    } else if event.ends_with("-timeout") {
        Some(OUTCOME_TIMEOUT)
    } else {
        None
    }
}

/// Operator text that means "stop the wait" (operator-intent classification of
/// the OPERATOR'S OWN message - never page text).
fn is_abort_text(content: &str) -> bool {
    let lower = content.trim().to_lowercase();
    let lower = lower.trim_start_matches("/");
    ["abort", "cancel", "stop", "quit", "nevermind", "never mind"]
        .iter()
        .any(|kw| lower == *kw || lower.starts_with(&format!("{} ", kw)) || lower.starts_with(kw) && lower.len() <= kw.len() + 2)
}

// ── Data shapes ─────────────────────────────────────────────────────────────

/// One listener delivery attempt.
#[derive(Clone, Debug, Serialize)]
pub struct Delivery {
    pub listener: String,
    pub action: String,
    pub tool: String,
    pub status: String,
    pub detail: String,
    pub duration_ms: u64,
}

/// One published interaction (correlation-id keyed).
#[derive(Clone, Debug, Serialize)]
pub struct Interaction {
    pub correlation_id: String,
    pub event: String,
    pub session_id: Option<String>,
    pub published_at: String,
    pub published_ms: i64,
    pub deadline_at: String,
    pub timeout_s: i64,
    pub payload: Value,
    pub deliveries: Vec<Delivery>,
    pub outcome: Option<String>,
    pub outcome_event: Option<String>,
    pub outcome_payload: Option<Value>,
    pub outcome_at: Option<String>,
}

/// Result of `publish`.
#[derive(Clone, Debug, Serialize)]
pub struct PublishResult {
    pub event: String,
    pub correlation_id: String,
    /// Outcome carried by a terminal event (`None` for a base event).
    pub outcome: Option<String>,
    pub listeners: Vec<Delivery>,
    /// Number of listeners that delivered successfully.
    pub delivered: usize,
}

/// Result of `wait`.
#[derive(Clone, Debug, Serialize)]
pub struct WaitOutcome {
    /// `solved` | `aborted` | `timeout` | `pending`.
    pub state: String,
    pub correlation_id: String,
    pub event: Option<String>,
    pub payload: Option<Value>,
    pub elapsed_ms: i64,
}

// ── Listener execution backend (abstracted: unit-testable) ──────────────────

/// Executes ONE listener action. The real implementation runs the action's
/// tool through the plugin registry; tests inject a fake.
pub trait ListenerExec: Send + Sync + 'static {
    fn execute<'a>(
        &'a self,
        tool_name: String,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = AppResult<(String, bool)>> + Send + 'a>>;
}

/// Real backend: resolve the action's tool through the plugin registry and
/// execute it with the live `AppContext` (identical to hook/cron actions).
pub struct PluginListenerExec {
    plugin_manager: Arc<dyn PluginManager>,
    app_context: AppContext,
}

impl PluginListenerExec {
    pub fn new(plugin_manager: Arc<dyn PluginManager>, app_context: AppContext) -> Self {
        Self {
            plugin_manager,
            app_context,
        }
    }
}

impl ListenerExec for PluginListenerExec {
    fn execute<'a>(
        &'a self,
        tool_name: String,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = AppResult<(String, bool)>> + Send + 'a>> {
        Box::pin(async move {
            let call = McpToolCall {
                id: format!("event-listener-{}", tool_name),
                name: tool_name,
                arguments: args,
            };
            let snapshot = self.plugin_manager.snapshot_registry().await;
            let result = snapshot.execute(&call, self.app_context.clone()).await?;
            Ok((result.content, result.is_error))
        })
    }
}

// ── Engine ──────────────────────────────────────────────────────────────────

pub struct EventsEngine {
    data_dir: String,
    exec: Arc<dyn ListenerExec>,
    /// Only used for the inbound-message signal (optional: unit tests omit it).
    pool: Option<PgPool>,
    interactions: Arc<parking_lot::RwLock<HashMap<String, Interaction>>>,
    seq: AtomicU64,
}

static ENGINE: OnceLock<EventsEngine> = OnceLock::new();

/// Initialize the global published-event bus (called once at startup).
pub fn init(engine: EventsEngine) {
    let _ = ENGINE.set(engine);
}

fn engine() -> Option<&'static EventsEngine> {
    ENGINE.get()
}

impl EventsEngine {
    pub fn new(data_dir: String, exec: Arc<dyn ListenerExec>, pool: Option<PgPool>) -> Self {
        Self {
            data_dir,
            exec,
            pool,
            interactions: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            seq: AtomicU64::new(0),
        }
    }

    fn next_correlation_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        format!("evt-{}-{}", Utc::now().timestamp_millis(), n)
    }

    /// Publish one event and run every listener bound to it.
    pub async fn publish(
        &self,
        event: &str,
        correlation_id: Option<String>,
        payload: Value,
    ) -> AppResult<PublishResult> {
        let event = event.trim();
        if event.is_empty() {
            return Err(Error::Message("event name is required".to_string()));
        }
        let correlation_id = correlation_id
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| self.next_correlation_id());
        let now = Utc::now().timestamp_millis();
        let outcome = outcome_for_event(event);

        // 1. Record the interaction state (create on any event: a terminal
        //    event may arrive before a waiter, and a base event may be seen
        //    only through its terminal event).
        {
            let mut map = self.interactions.write();
            let entry = map
                .entry(correlation_id.clone())
                .or_insert_with(|| new_interaction(event, &correlation_id, &payload, now));
            entry.event = event.to_string();
            entry.payload = payload.clone();
            if let Some(session) = payload.get("session_id").and_then(Value::as_str) {
                entry.session_id = Some(session.to_string());
            }
            if let Some(value) = payload.get("timeout_s").and_then(Value::as_i64) {
                if value > 0 {
                    entry.timeout_s = value;
                    entry.deadline_at = rfc3339(entry.published_ms + value * 1000);
                }
            }
            if let Some(state) = outcome {
                if entry.outcome.is_none() {
                    entry.outcome = Some(state.to_string());
                    entry.outcome_event = Some(event.to_string());
                    entry.outcome_payload = Some(payload.clone());
                    entry.outcome_at = Some(rfc3339(now));
                }
            }
        }

        // 2. Fan-out: every enabled hook bound to this event is a listener.
        let event_json = json!({
            "event": event,
            "correlation_id": correlation_id,
            "payload": payload,
        });
        let listeners = self.resolve_listeners(event, &event_json);
        let mut deliveries: Vec<Delivery> = Vec::new();
        for (key, action_id, tool_name, spec) in listeners {
            let started = std::time::Instant::now();
            let (status, detail) = match spec {
                Err(reason) => ("error".to_string(), reason),
                Ok(args) => {
                    let exec = self.exec.clone();
                    let tool = tool_name.clone();
                    match tokio::time::timeout(
                        Duration::from_secs(LISTENER_TIMEOUT_S),
                        exec.execute(tool.clone(), args),
                    )
                    .await
                    {
                        Ok(Ok((content, is_error))) => {
                            if is_error {
                                ("error".to_string(), content)
                            } else {
                                ("ok".to_string(), content)
                            }
                        }
                        Ok(Err(e)) => ("error".to_string(), format!("{:#}", e)),
                        Err(_) => (
                            "error".to_string(),
                            format!("listener timed out after {}s", LISTENER_TIMEOUT_S),
                        ),
                    }
                }
            };
            let duration_ms = started.elapsed().as_millis() as u64;
            if status == "ok" {
                info!(
                    "[events] listener '{}' delivered event '{}' (correlation {}) in {}ms",
                    key, event, correlation_id, duration_ms
                );
            } else {
                warn!(
                    "[events] listener '{}' FAILED event '{}' (correlation {}): {}",
                    key, event, correlation_id, detail
                );
            }
            deliveries.push(Delivery {
                listener: key,
                action: action_id,
                tool: tool_name,
                status,
                detail,
                duration_ms,
            });
        }

        let delivered = deliveries.iter().filter(|d| d.status == "ok").count();
        {
            let mut map = self.interactions.write();
            if let Some(entry) = map.get_mut(&correlation_id) {
                entry.deliveries.extend(deliveries.clone());
                if outcome.is_some() {
                    entry.outcome_event = Some(event.to_string());
                }
            }
        }

        Ok(PublishResult {
            event: event.to_string(),
            correlation_id,
            outcome: outcome.map(str::to_string),
            listeners: deliveries,
            delivered,
        })
    }

    /// Resolve every enabled hook bound to `event` into a listener spec.
    /// Returned tuple: (listener key, action id, tool name, args-or-error).
    #[allow(clippy::type_complexity)]
    fn resolve_listeners(
        &self,
        event: &str,
        event_json: &Value,
    ) -> Vec<(String, String, String, Result<Value, String>)> {
        let tasks = crate::tasks_yaml::load_tasks_or_empty(&self.data_dir);
        let mut hooks: Vec<(&String, &crate::tasks_yaml::HookDef)> = tasks
            .hooks
            .iter()
            .filter(|(_, def)| def.enabled && def.event.trim() == event)
            .collect();
        hooks.sort_by(|a, b| a.0.cmp(b.0));

        let mut out = Vec::new();
        for (key, def) in hooks {
            let action_id = def.action.clone().unwrap_or_default();
            if def.mode() != crate::hooks::MODE_ACTION {
                out.push((
                    key.clone(),
                    action_id.clone(),
                    String::new(),
                    Err(format!(
                        "published events only run 'action' listeners (hook mode is '{}')",
                        def.mode()
                    )),
                ));
                continue;
            }
            if def.scope != crate::hooks::SCOPE_GLOBAL {
                out.push((
                    key.clone(),
                    action_id.clone(),
                    String::new(),
                    Err(format!(
                        "published events have no thread scope: use scope 'global' (hook scope is '{}')",
                        def.scope
                    )),
                ));
                continue;
            }
            if def.count != 1 {
                warn!(
                    "[events] listener '{}' declares count={}: published events always deliver per event",
                    key, def.count
                );
            }
            if action_id.trim().is_empty() {
                out.push((
                    key.clone(),
                    action_id,
                    String::new(),
                    Err("hook has mode=action but no action id".to_string()),
                ));
                continue;
            }
            match crate::scheduler::resolve_action(&self.data_dir, &action_id) {
                Ok(call) => {
                    // Same convention as hook actions: the event object is
                    // merged into the action arguments under the `event` key
                    // and WINS on collision.
                    let mut args = call.arguments;
                    if let Value::Object(map) = &mut args {
                        map.insert("event".to_string(), event_json.clone());
                    } else {
                        args = json!({ "event": event_json });
                    }
                    out.push((key.clone(), action_id, call.name, Ok(args)));
                }
                Err(e) => out.push((
                    key.clone(),
                    action_id,
                    String::new(),
                    Err(format!("action resolve failed: {:#}", e)),
                )),
            }
        }
        out
    }

    /// Block (bounded) until the interaction reaches a terminal state.
    ///
    /// `max_seconds` bounds THIS call only: when it expires while the
    /// interaction is still pending the state is `pending` (the caller may
    /// wait again). The interaction's own deadline (`timeout_s`) publishes the
    /// `-timeout` terminal event, so the wait can never hang.
    pub async fn wait(&self, correlation_id: &str, max_seconds: Option<i64>) -> WaitOutcome {
        let started = std::time::Instant::now();
        let caller_deadline = max_seconds
            .filter(|s| *s > 0)
            .map(|s| Utc::now().timestamp_millis() + s * 1000);
        loop {
            let snapshot = self.interactions.read().get(correlation_id).cloned();
            match snapshot {
                Some(entry) if entry.outcome.is_some() => {
                    return WaitOutcome {
                        state: entry.outcome.unwrap_or_default(),
                        correlation_id: correlation_id.to_string(),
                        event: entry.outcome_event.clone(),
                        payload: entry.outcome_payload.clone(),
                        elapsed_ms: started.elapsed().as_millis() as i64,
                    };
                }
                Some(entry) => {
                    if Utc::now().timestamp_millis() >= entry.published_ms + entry.timeout_s * 1000 {
                        // The interaction deadline passed: the bus closes the
                        // interaction so the waiter does not hang.
                        let payload = json!({
                            "request_event": entry.event,
                            "session_id": entry.session_id,
                            "timed_out": true,
                            "timeout_s": entry.timeout_s,
                        });
                        let terminal = terminal_event(&entry.event, OUTCOME_TIMEOUT);
                        let _ = self
                            .publish(&terminal, Some(correlation_id.to_string()), payload)
                            .await;
                        // Fall through: the loop re-reads the recorded outcome.
                        continue;
                    }
                }
                None => {
                    return WaitOutcome {
                        state: "pending".to_string(),
                        correlation_id: correlation_id.to_string(),
                        event: None,
                        payload: None,
                        elapsed_ms: started.elapsed().as_millis() as i64,
                    };
                }
            }
            if let Some(deadline) = caller_deadline {
                if Utc::now().timestamp_millis() >= deadline {
                    return WaitOutcome {
                        state: "pending".to_string(),
                        correlation_id: correlation_id.to_string(),
                        event: None,
                        payload: None,
                        elapsed_ms: started.elapsed().as_millis() as i64,
                    };
                }
            }
            tokio::time::sleep(Duration::from_millis(WAIT_POLL_MS)).await;
        }
    }

    /// Describe one interaction (status/debug).
    pub fn describe(&self, correlation_id: &str) -> Option<Interaction> {
        self.interactions.read().get(correlation_id).cloned()
    }

    /// All known interactions, newest first (status/debug).
    pub fn list(&self) -> Vec<Interaction> {
        let mut all: Vec<Interaction> = self.interactions.read().values().cloned().collect();
        all.sort_by(|a, b| b.published_ms.cmp(&a.published_ms));
        all
    }

    /// Authoritative second signal: an inbound operator/platform message
    /// observed AFTER a request was published means "the human acted". An
    /// abort-ish text ends the interaction as `aborted`, anything else as
    /// `solved`. Never blocks the message path.
    pub async fn handle_inbound_message(&self, message_id: i64) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        let pending: Vec<(String, String, i64)> = {
            let map = self.interactions.read();
            map.values()
                .filter(|i| i.outcome.is_none())
                .map(|i| (i.correlation_id.clone(), i.event.clone(), i.published_ms))
                .collect()
        };
        if pending.is_empty() {
            return;
        }
        #[derive(sqlx::FromRow)]
        struct InboundRow {
            role: String,
            msg_type: String,
            content: String,
            created_ms: Option<f64>,
        }
        let row: Option<InboundRow> = sqlx::query_as(
            "SELECT role, msg_type, content, \
             (EXTRACT(EPOCH FROM created_at) * 1000)::float8 AS created_ms \
             FROM messages WHERE id = $1",
        )
        .bind(message_id)
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten();
        let Some(row) = row else { return };
        // Platform-inbound causes carry role='cause' + msg_type='Cause'
        // (kanban/cron/hook causes use their own lowercase msg_type).
        if row.role != "cause" || row.msg_type != "Cause" {
            return;
        }
        let created_ms = row.created_ms.map(|f| f as i64).unwrap_or_else(now_ms);
        let outcome = if is_abort_text(&row.content) {
            OUTCOME_ABORTED
        } else {
            OUTCOME_SOLVED
        };
        for (correlation_id, event, published_ms) in pending {
            if created_ms < published_ms {
                continue; // message predates the request: not a response
            }
            let terminal = terminal_event(&event, outcome);
            let payload = json!({
                "request_event": event,
                "signal": "inbound_operator_message",
                "message_id": message_id,
                "operator_text_length": row.content.chars().count(),
            });
            match self
                .publish(&terminal, Some(correlation_id.clone()), payload)
                .await
            {
                Ok(_) => info!(
                    "[events] inbound operator message #{} resolved '{}' as {}",
                    message_id, correlation_id, outcome
                ),
                Err(e) => warn!(
                    "[events] failed to resolve '{}' from inbound message #{}: {:#}",
                    correlation_id, message_id, e
                ),
            }
        }
    }
}

fn new_interaction(event: &str, correlation_id: &str, payload: &Value, now: i64) -> Interaction {
    let timeout_s = payload
        .get("timeout_s")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_TIMEOUT_S);
    Interaction {
        correlation_id: correlation_id.to_string(),
        event: event.to_string(),
        session_id: payload
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        published_at: rfc3339(now),
        published_ms: now,
        deadline_at: rfc3339(now + timeout_s * 1000),
        timeout_s,
        payload: payload.clone(),
        deliveries: Vec::new(),
        outcome: None,
        outcome_event: None,
        outcome_payload: None,
        outcome_at: None,
    }
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn rfc3339(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

// ── Free functions (global engine) ──────────────────────────────────────────

/// Publish an event on the global bus. See [`EventsEngine::publish`].
pub async fn publish(
    event: &str,
    correlation_id: Option<String>,
    payload: Value,
) -> AppResult<PublishResult> {
    let engine = engine().ok_or_else(|| Error::Message("events bus not initialized".to_string()))?;
    engine.publish(event, correlation_id, payload).await
}

/// Wait (bounded) for the outcome of one interaction.
pub async fn wait(correlation_id: &str, max_seconds: Option<i64>) -> WaitOutcome {
    match engine() {
        Some(engine) => engine.wait(correlation_id, max_seconds).await,
        None => WaitOutcome {
            state: "pending".to_string(),
            correlation_id: correlation_id.to_string(),
            event: None,
            payload: None,
            elapsed_ms: 0,
        },
    }
}

/// Describe one interaction.
pub fn describe(correlation_id: &str) -> Option<Interaction> {
    engine().and_then(|e| e.describe(correlation_id))
}

/// List every known interaction, newest first.
pub fn list() -> Vec<Interaction> {
    engine().map(|e| e.list()).unwrap_or_default()
}

/// True when the bus is initialized and at least one interaction is pending
/// (cheap pre-check used by the message path).
pub fn has_pending() -> bool {
    engine()
        .map(|e| e.interactions.read().values().any(|i| i.outcome.is_none()))
        .unwrap_or(false)
}

/// Notify the bus that a message row was inserted. Fire-and-forget: never
/// blocks or fails the message path.
pub fn note_new_message(thread_id: i64, message_id: i64) {
    if !has_pending() {
        return; // nothing awaiting a human: skip the DB lookup entirely
    }
    let Some(engine) = engine() else { return };
    tokio::spawn(async move {
        let _ = thread_id;
        engine.handle_inbound_message(message_id).await;
    });
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Fake listener backend: records calls, returns ok/error per tool name.
    struct FakeExec {
        calls: Arc<Mutex<Vec<(String, Value)>>>,
        fail: Mutex<Vec<String>>,
    }

    impl ListenerExec for FakeExec {
        fn execute<'a>(
            &'a self,
            tool_name: String,
            args: Value,
        ) -> Pin<Box<dyn Future<Output = AppResult<(String, bool)>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push((tool_name.clone(), args));
                if self.fail.lock().unwrap().contains(&tool_name) {
                    return Err(Error::Message("listener exploded".to_string()));
                }
                Ok((format!("delivered by {}", tool_name), false))
            })
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "omni-events-test-{}-{}-{}",
            tag,
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_config(dir: &std::path::Path, hooks_yaml: &str) {
        // Config files live in `{data_dir}/config/` (see crate::config_path).
        let config_dir = dir.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("actions.yml"),
            r#"
actions:
  listener_a:
    enabled: true
    tool_name: fake__a
    params: {}
  listener_b:
    enabled: true
    tool_name: fake__b
    params: {}
  listener_broken:
    enabled: true
    tool_name: fake__broken
    params: {}
"#,
        )
        .unwrap();
        std::fs::write(
            config_dir.join("tasks.yml"),
            format!("hooks:\n{hooks_yaml}"),
        )
        .unwrap();
    }

    fn engine_with(dir: &std::path::Path, fail: Vec<&str>) -> (EventsEngine, Arc<FakeExec>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let exec = Arc::new(FakeExec {
            calls: calls.clone(),
            fail: Mutex::new(fail.into_iter().map(str::to_string).collect()),
        });
        let engine = EventsEngine::new(
            dir.to_string_lossy().to_string(),
            exec.clone(),
            None,
        );
        (engine, exec)
    }

    const TWO_LISTENERS: &str = r#"
  telegram-listener:
    enabled: true
    event: solve-captcha
    scope: global
    count: 1
    mode: action
    action: listener_a
  echo-listener:
    enabled: true
    event: solve-captcha
    scope: global
    count: 1
    mode: action
    action: listener_b
"#;

    #[test]
    fn terminal_event_names_and_outcomes() {
        assert_eq!(terminal_event("solve-captcha", OUTCOME_SOLVED), "solve-captcha-resolved");
        assert_eq!(outcome_for_event("solve-captcha-resolved"), Some("solved"));
        assert_eq!(outcome_for_event("solve-captcha-aborted"), Some("aborted"));
        assert_eq!(outcome_for_event("solve-captcha-timeout"), Some("timeout"));
        assert_eq!(outcome_for_event("solve-captcha"), None);
        assert!(is_abort_text("Abort"));
        assert!(is_abort_text("/cancel"));
        assert!(!is_abort_text("done, go ahead"));
    }

    #[tokio::test]
    async fn published_event_fans_out_to_every_listener() {
        let dir = temp_dir("fanout");
        write_config(&dir, TWO_LISTENERS);
        let (engine, exec) = engine_with(&dir, vec![]);
        let result = engine
            .publish(
                EVENT_SOLVE_CAPTCHA,
                Some("corr-1".to_string()),
                json!({"session_id": "s1", "url": "http://x/y", "timeout_s": 60}),
            )
            .await
            .unwrap();
        assert_eq!(result.listeners.len(), 2);
        assert_eq!(result.delivered, 2);
        // Both listeners received the event payload under the `event` key.
        let seen = exec.calls.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        for (_, args) in seen {
            assert_eq!(args["event"]["event"], "solve-captcha");
            assert_eq!(args["event"]["correlation_id"], "corr-1");
            assert_eq!(args["event"]["payload"]["session_id"], "s1");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn one_failing_listener_does_not_break_the_others() {
        let dir = temp_dir("isolation");
        write_config(
            &dir,
            r#"
  telegram-listener:
    enabled: true
    event: solve-captcha
    scope: global
    count: 1
    mode: action
    action: listener_broken
  echo-listener:
    enabled: true
    event: solve-captcha
    scope: global
    count: 1
    mode: action
    action: listener_b
"#,
        );
        let (engine, _) = engine_with(&dir, vec!["fake__broken"]);
        let result = engine
            .publish(EVENT_SOLVE_CAPTCHA, Some("corr-2".to_string()), json!({}))
            .await
            .unwrap();
        assert_eq!(result.listeners.len(), 2);
        assert_eq!(result.delivered, 1);
        let broken = result
            .listeners
            .iter()
            .find(|d| d.tool == "fake__broken")
            .unwrap();
        assert_eq!(broken.status, "error");
        let ok = result
            .listeners
            .iter()
            .find(|d| d.tool == "fake__b")
            .unwrap();
        assert_eq!(ok.status, "ok");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn disabled_listener_is_not_delivered_to() {
        let dir = temp_dir("disabled");
        write_config(
            &dir,
            r#"
  telegram-listener:
    enabled: false
    event: solve-captcha
    scope: global
    count: 1
    mode: action
    action: listener_a
  echo-listener:
    enabled: true
    event: solve-captcha
    scope: global
    count: 1
    mode: action
    action: listener_b
"#,
        );
        let (engine, exec) = engine_with(&dir, vec![]);
        let result = engine
            .publish(EVENT_SOLVE_CAPTCHA, Some("corr-3".to_string()), json!({}))
            .await
            .unwrap();
        assert_eq!(result.listeners.len(), 1);
        assert_eq!(result.listeners[0].tool, "fake__b");
        assert_eq!(exec.calls.lock().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn wait_resolves_on_terminal_event() {
        let dir = temp_dir("wait");
        write_config(&dir, TWO_LISTENERS);
        let (engine, _) = engine_with(&dir, vec![]);
        engine
            .publish(
                EVENT_SOLVE_CAPTCHA,
                Some("corr-4".to_string()),
                json!({"timeout_s": 30}),
            )
            .await
            .unwrap();
        let pending = engine.wait("corr-4", Some(1)).await;
        assert_eq!(pending.state, "pending");
        engine
            .publish(
                "solve-captcha-resolved",
                Some("corr-4".to_string()),
                json!({"how": "page_state"}),
            )
            .await
            .unwrap();
        let done = engine.wait("corr-4", Some(5)).await;
        assert_eq!(done.state, "solved");
        assert_eq!(done.event.as_deref(), Some("solve-captcha-resolved"));
        assert_eq!(done.payload.unwrap()["how"], "page_state");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn wait_times_out_at_the_interaction_deadline_without_hanging() {
        let dir = temp_dir("timeout");
        write_config(&dir, TWO_LISTENERS);
        let (engine, _) = engine_with(&dir, vec![]);
        engine
            .publish(
                EVENT_SOLVE_CAPTCHA,
                Some("corr-5".to_string()),
                json!({"timeout_s": 1}),
            )
            .await
            .unwrap();
        let outcome = engine.wait("corr-5", Some(10)).await;
        assert_eq!(outcome.state, "timeout");
        assert_eq!(outcome.event.as_deref(), Some("solve-captcha-timeout"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn unknown_correlation_id_is_pending_not_an_error() {
        let dir = temp_dir("unknown");
        write_config(&dir, TWO_LISTENERS);
        let (engine, _) = engine_with(&dir, vec![]);
        let outcome = engine.wait("nope", Some(1)).await;
        assert_eq!(outcome.state, "pending");
        assert!(engine.describe("nope").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
