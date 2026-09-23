//! Engine-level efficiency guard: the generic duplicate-invocation ledger.
//!
//! Root cause (incident 2874 - 87 min, 247 LLM calls, 14.7M prompt tokens for a
//! ~3-edit change): the agent replayed invocations whose result it ALREADY had.
//! The replay is a symptom of context/compaction thrash - once the context hits
//! the hard budget the older tool results are summarised away, the model
//! "forgets" it already asked, and asks again. A fix in the prompt alone cannot
//! work, because the model no longer remembers the earlier call. The fix has to
//! live in the TOOL EXECUTOR, so it is context-independent: if the thread has
//! already executed an invocation, the executor answers the replay with a
//! compact stub instead of running it again and re-injecting its bytes.
//!
//! [`CallLedger`] is that ledger. An invocation is identified by the tool id
//! the executor received from the registry (an OPAQUE string: this module never
//! parses it, never matches it against a list and never knows a plugin or tool
//! name) plus the CANONICAL form of its arguments. The mechanism is therefore
//! generic by construction: a tool the core has never seen participates on
//! exactly the same terms, and an unknown invocation always EXECUTES.
//!
//! A record is invalidated (the invocation executes normally again) by:
//! - a result that flags a state change (`{"state_changed": true}`), the
//!   structured "the tool reports it changed something" signal;
//! - an ERROR result - a failed invocation is always retryable;
//! - a new instruction merged into the live thread (new intent, new world);
//! - the reserved argument [`FORCE_REPEAT_ARG`], consumed by the core and never
//!   dispatched, which forces one fresh execution on demand.
//!
//! Suppression therefore never changes what the agent CAN learn - only whether
//! bytes that are already in its context are injected a second time.

use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::error::Error;

/// Reserved, core-consumed argument key: the caller explicitly asks for one
/// fresh execution of an invocation the ledger already holds. The key is
/// stripped before dispatch, so no tool ever receives it.
pub const FORCE_REPEAT_ARG: &str = "force_repeat";

/// Structured marker a tool may put at the top level of its result payload to
/// declare that it changed state. The ledger is invalidated when it is seen, so
/// the next identical invocation executes normally (genuine repeats stay
/// possible; the tool - not a name list in the core - reports the change).
pub const STATE_CHANGED_KEY: &str = "state_changed";

/// What the executor should do with an invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallVerdict {
    /// Run it (unknown tool, first time, or an invalidated record).
    Execute,
    /// The thread already executed this exact invocation at `first_iter`.
    Duplicate { first_iter: u32 },
}

/// Per-thread-run ledger of the invocations this thread already executed.
#[derive(Debug, Default)]
pub struct CallLedger {
    /// invocation identity -> iteration of its first execution
    seen: HashMap<String, u32>,
    duplicates: u32,
    executions: u32,
    invalidations: u32,
}

/// Invocation identity: the opaque tool id plus the canonical arguments.
/// `\u{1f}` (unit separator) cannot occur in a tool id, so the two parts cannot
/// be confused by concatenation.
fn identity(tool: &str, canonical_args: &str) -> String {
    format!("{tool}\u{1f}{canonical_args}")
}

fn canonicalize(value: &Value, top: bool) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                if top && k == FORCE_REPEAT_ARG {
                    continue;
                }
                out.insert(k.clone(), canonicalize(&map[k], false));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| canonicalize(i, false)).collect()),
        other => other.clone(),
    }
}

/// Canonical form of an invocation's arguments: JSON with object keys sorted
/// recursively and the reserved core key removed, so key order or whitespace
/// cannot disguise a replay. Falls back to the trimmed raw string when the
/// arguments are not valid JSON.
pub fn canonical_args(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => serde_json::to_string(&canonicalize(&v, true))
            .unwrap_or_else(|_| raw.trim().to_string()),
        Err(_) => raw.trim().to_string(),
    }
}

/// True when the caller explicitly asked for a fresh execution.
pub fn force_requested(raw: &str) -> bool {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => map
            .get(FORCE_REPEAT_ARG)
            .and_then(Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

/// The arguments actually dispatched to the tool: identical to the raw
/// arguments unless the reserved core key was present (then it is stripped).
pub fn strip_reserved(raw: &str) -> String {
    let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    if map.remove(FORCE_REPEAT_ARG).is_none() {
        return raw.to_string();
    }
    serde_json::to_string(&Value::Object(map)).unwrap_or_else(|_| raw.to_string())
}

/// Structured state-change signal in a tool result payload.
pub fn result_reports_state_change(output: &str) -> bool {
    match serde_json::from_str::<Value>(output) {
        Ok(Value::Object(map)) => map
            .get(STATE_CHANGED_KEY)
            .and_then(Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

impl CallLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask about an invocation BEFORE dispatching it. Pure: it only reports what
    /// this thread has already executed.
    pub fn observe(&self, tool: &str, canonical_args: &str) -> CallVerdict {
        match self.seen.get(&identity(tool, canonical_args)) {
            Some(&first_iter) => CallVerdict::Duplicate { first_iter },
            None => CallVerdict::Execute,
        }
    }

    /// Record that the invocation is being executed; from now on its result is
    /// part of the thread's context.
    pub fn record_executed(&mut self, tool: &str, canonical_args: &str, iteration: u32) {
        self.executions = self.executions.saturating_add(1);
        self.seen.insert(identity(tool, canonical_args), iteration);
    }

    /// The invocation was answered with a stub instead of being executed.
    pub fn record_blocked(&mut self) {
        self.duplicates = self.duplicates.saturating_add(1);
    }

    /// Drop one record: a failed invocation must stay retryable.
    pub fn drop_record(&mut self, tool: &str, canonical_args: &str) {
        self.seen.remove(&identity(tool, canonical_args));
    }

    /// A state change happened (a tool reported one, or a new instruction was
    /// merged into the live thread): every prior record is invalidated, so the
    /// next identical call executes normally. Returns how many records were
    /// dropped (0 = nothing to invalidate).
    pub fn note_state_change(&mut self) -> usize {
        let dropped = self.seen.len();
        if dropped > 0 {
            self.seen.clear();
            self.invalidations = self.invalidations.saturating_add(1);
        }
        dropped
    }

    pub fn duplicates(&self) -> u32 {
        self.duplicates
    }

    pub fn executions(&self) -> u32 {
        self.executions
    }

    pub fn tracked(&self) -> usize {
        self.seen.len()
    }

    /// Per-thread counters for the logs / metrics surface.
    pub fn metrics_json(&self, thread_id: i64) -> Value {
        serde_json::json!({
            "thread": thread_id,
            "executed_calls": self.executions,
            "duplicate_calls": self.duplicates,
            "tracked_invocations": self.seen.len(),
            "invalidations": self.invalidations,
        })
    }

    pub fn metrics_summary(&self, thread_id: i64) -> String {
        format!(
            "thread={} executed_calls={} duplicate_calls={} tracked_invocations={} invalidations={}",
            thread_id,
            self.executions,
            self.duplicates,
            self.seen.len(),
            self.invalidations
        )
    }
}

/// The compact stub the executor returns instead of re-executing a duplicate
/// invocation. It intentionally carries NO payload.
pub fn duplicate_stub(tool: &str, first_iter: u32) -> String {
    format!(
        "[duplicate call - `{}` with these exact arguments was already executed at iteration {} in THIS thread; its result is already in your context, it was NOT executed again and no payload was re-injected. Use your working notes instead of repeating the call. If a state change happened since (an edit, a commit, an apply/checkout, a new instruction) and you really need a fresh result, re-issue the call with the reserved argument \"{}\": true (the engine consumes that key and executes it again), or change the arguments to ask a different question.]",
        tool, first_iter, FORCE_REPEAT_ARG
    )
}

/// Behavioural progress signal, derived ONLY from the duplicate-invocation
/// counter. It is a signal, never a cap: the thread keeps running and the model
/// is told to act on what it has (or to report its blocker) in the next reply.
pub fn progress_signal(duplicates: u32, signalled_at: u32) -> Option<String> {
    const STEP: u32 = 3;
    if duplicates < STEP || duplicates < signalled_at.saturating_add(STEP) {
        return None;
    }
    Some(format!(
        "You have issued {} invocation(s) whose result this thread already had. The engine did not execute them again and did not re-inject their payload. Stop replaying: use your notes and the results already in your context, take the next real step (edit + commit), or - if you are blocked - state the blocker and report what is done and what remains in your next reply.",
        duplicates
    ))
}

/// Fatal provider errors, classified on the STRUCTURED transport status only
/// (never on a vendor's wording): retrying them can only burn the remaining
/// provider balance - incident 2874 ended on "402 Payment Required".
pub fn classify_provider_error(err: &Error) -> Option<String> {
    let status = err.provider_status()?;
    let kind = match status {
        401 => "provider authentication failed (HTTP 401)",
        402 => "provider rejected the request for billing reasons (HTTP 402, insufficient balance/credit)",
        403 => "provider denied the request (HTTP 403)",
        _ => return None,
    };
    Some(format!("{} - a retry cannot succeed", kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool id the core has NEVER seen: no plugin, no real tool name. The
    /// guard must treat it exactly like any other (this is the genericity gate
    /// at unit level).
    const UNSEEN_TOOL: &str = "zorp__frobnicate";

    #[test]
    fn unseen_tool_participates_generically() {
        let mut led = CallLedger::new();
        assert_eq!(led.observe(UNSEEN_TOOL, "{\"a\":1}"), CallVerdict::Execute);
        led.record_executed(UNSEEN_TOOL, "{\"a\":1}", 7);
        assert_eq!(
            led.observe(UNSEEN_TOOL, "{\"a\":1}"),
            CallVerdict::Duplicate { first_iter: 7 }
        );
        assert_eq!(led.executions(), 1);
    }

    #[test]
    fn different_arguments_always_execute() {
        let mut led = CallLedger::new();
        led.record_executed(UNSEEN_TOOL, "{\"a\":1}", 1);
        assert_eq!(led.observe(UNSEEN_TOOL, "{\"a\":2}"), CallVerdict::Execute);
        assert_eq!(
            led.observe("zorp__other", "{\"a\":1}"),
            CallVerdict::Execute
        );
    }

    #[test]
    fn key_order_and_whitespace_cannot_disguise_a_replay() {
        let a = canonical_args("{ \"path\": \"/x\", \"limit\": 20 }");
        let b = canonical_args("{\"limit\":20,\"path\":\"/x\"}");
        assert_eq!(a, b);
        let mut led = CallLedger::new();
        led.record_executed(UNSEEN_TOOL, &a, 3);
        assert_eq!(
            led.observe(UNSEEN_TOOL, &b),
            CallVerdict::Duplicate { first_iter: 3 }
        );
    }

    #[test]
    fn error_result_is_retryable() {
        let mut led = CallLedger::new();
        led.record_executed(UNSEEN_TOOL, "{}", 2);
        led.drop_record(UNSEEN_TOOL, "{}");
        assert_eq!(led.observe(UNSEEN_TOOL, "{}"), CallVerdict::Execute);
    }

    #[test]
    fn flagged_state_change_invalidates_every_record() {
        assert!(result_reports_state_change("{\"state_changed\":true}"));
        assert!(!result_reports_state_change("{\"state_changed\":false}"));
        assert!(!result_reports_state_change("plain tool output"));
        assert!(!result_reports_state_change("[1,2,3]"));

        let mut led = CallLedger::new();
        led.record_executed(UNSEEN_TOOL, "{\"a\":1}", 1);
        led.record_executed("zorp__other", "{}", 2);
        assert_eq!(led.note_state_change(), 2);
        assert_eq!(led.observe(UNSEEN_TOOL, "{\"a\":1}"), CallVerdict::Execute);
        assert_eq!(led.observe("zorp__other", "{}"), CallVerdict::Execute);
    }

    #[test]
    fn force_repeat_is_honoured_and_stripped_from_the_dispatched_arguments() {
        let raw = "{\"path\":\"/x\",\"force_repeat\":true}";
        assert!(force_requested(raw));
        let stripped = strip_reserved(raw);
        assert!(!stripped.contains(FORCE_REPEAT_ARG));
        assert_eq!(canonical_args(raw), canonical_args("{\"path\":\"/x\"}"));

        let plain = "{\"path\":\"/x\"}";
        assert!(!force_requested(plain));
        assert_eq!(strip_reserved(plain), plain);
    }

    #[test]
    fn counters_are_exposed_per_thread() {
        let mut led = CallLedger::new();
        led.record_executed(UNSEEN_TOOL, "{}", 1);
        led.record_blocked();
        led.record_blocked();
        assert_eq!(led.duplicates(), 2);
        let m = led.metrics_json(4242);
        assert_eq!(m["duplicate_calls"], 2);
        assert_eq!(m["executed_calls"], 1);
        assert_eq!(m["thread"], 4242);
        let s = led.metrics_summary(4242);
        assert!(s.contains("duplicate_calls=2"));
        assert!(s.contains("executed_calls=1"));
    }

    #[test]
    fn duplicate_stub_carries_no_payload() {
        let stub = duplicate_stub(UNSEEN_TOOL, 9);
        assert!(stub.contains("duplicate call"));
        assert!(stub.contains(FORCE_REPEAT_ARG));
        assert!(
            stub.len() < 700,
            "stub must stay tiny: {} chars",
            stub.len()
        );
    }

    #[test]
    fn progress_signal_escalates_with_duplicates_only() {
        assert!(progress_signal(2, 0).is_none());
        assert!(progress_signal(3, 0).is_some());
        assert!(progress_signal(4, 3).is_none());
        assert!(progress_signal(6, 3).is_some());
    }

    /// C1 gate: this module carries no vocabulary of invocation classification.
    #[test]
    fn module_has_no_invocation_classification_vocabulary() {
        let src: &str = include_str!("efficiency.rs");
        for needle in [
            concat!("read", "_only"),
            concat!("read", "-only"),
            concat!("read", "only_streak"),
            concat!("read", "_write_ratio"),
            concat!("read", "_scope"),
            concat!("read", "Scope"),
            concat!("Scope", "Kind"),
        ] {
            assert!(
                !src.contains(needle),
                "forbidden classification vocabulary in the efficiency module: {needle}"
            );
        }
    }
}
