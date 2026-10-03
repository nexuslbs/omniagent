//! Thread-wide usage collection: strip `_meta` from tool results, collect
//! `_meta.usage` array items, build the omniagent's own LLM-call usage entries
//! and the thread-end "Usage"-type message.
//!
//! Background (operator request 2026-09-30, follow-up of the workstation
//! `expose_tool_call` task): workstation agent-calling tools return
//! `_meta.usage` as an ARRAY of per-call usage dicts (fields: agent,
//! input_tokens, output_tokens, total_tokens, cached_input_tokens,
//! cache_write_tokens, reasoning_tokens, cost{amount_usd, is_estimate, source,
//! pricing_ref}, provider, model, request_id, details). The omniagent:
//!
//! 1. strips EVERY `_meta` field from every tool result (it must never reach
//!    the agent context nor the stored thread messages) - at ANY nesting
//!    level, including payloads that arrive string-encoded inside a
//!    background-dispatch envelope (`{"status":..,"result":"<json>"}`) or a
//!    multi-tool wrapper (`{"tool":..,"input":..,"output":"<json>"}`): the
//!    workstation's `{"status":"ok","tool":"agent_run","result":{..},
//!    "_meta":{..}}` answer reaches the loop inside those envelopes, so a
//!    top-level-only strip never saw it (operator thread 3887 defect);
//! 2. concatenates all `_meta.usage` items from all tool call results of the
//!    thread into one array, plus the omniagent's own LLM-call entries;
//! 3. inserts a "Usage"-type message at thread end (just before the last
//!    message) carrying ONLY that array (operator UPDATE 2026-09-30 threads
//!    3705/3707/3709: no wrapper object, no "usage" key, no `full_*` keys);
//! 4. the aggregate fields (`full_input_tokens`, `full_cached_tokens`,
//!    `full_output_tokens`, `full_reasoning_tokens`, `cost`) are computed as
//!    sums over the array items and written to the THREADS TABLE at thread end
//!    (operator UPDATE 3702) - they never appear on the Usage message. The
//!    min-clamp against the omniagent's own bare totals applies to the
//!    OMNIAGENT sub-total only, because the operator's semantics are
//!    `full_*` = omniagent + dsh agents (thread 3887): the dsh items are added
//!    on top, `full_input_tokens = min(input_tokens, omniagent_sum) + dsh_sum`.
//!    Only REAL LLM calls are summed: a dsh `details.kind = "agent-aggregate"`
//!    roll-up (the session summary of the same calls) is skipped so its
//!    tokens/cost are not counted a second time (review thread 3890).
//!
//! RAW RESULTS ONLY: none of these fields are ever changed by agents - they
//! are raw provider and tool results. Cost is never estimated by the agent:
//! the omniagent's own entries carry the cost block computed SERVICE-SIDE from
//! the fixed price table in [`crate::agent::pricing`] (operator correction
//! 2026-10-02, telegram thread 3882: the hardcoded `cost: null` left
//! `threads.cost` at 0 for the whole database). A route the table does not
//! price keeps `cost: null` - an unknown price is never fabricated as 0.

use crate::llm::Usage;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Strip EVERY `_meta` key from a tool result payload - at any nesting level,
/// including JSON payloads that arrive string-encoded inside a background
/// dispatch envelope or a multi-tool wrapper - and collect each removed
/// `_meta.usage` array's items (in encounter order) into `collector`.
///
/// The content is only rewritten when a `_meta` key was actually removed:
/// non-JSON content and JSON without any `_meta` pass through byte-identical
/// (a string that carries JSON is re-serialized only when a `_meta` inside it
/// was removed). A `_meta` whose `usage` is not an array collects nothing (the
/// `_meta` key is still stripped).
pub fn strip_meta_and_collect(content: &str, collector: &mut Vec<Value>) -> String {
    let Ok(value) = serde_json::from_str::<Value>(content) else {
        return content.to_string();
    };
    let (stripped, changed) = strip_meta_deep(value, collector);
    if !changed {
        return content.to_string();
    }
    serde_json::to_string(&stripped).unwrap_or_else(|_| content.to_string())
}

/// Recursive worker for [`strip_meta_and_collect`]: removes `_meta` from every
/// JSON object it can reach (object fields, array items and string-encoded
/// JSON) and reports whether anything changed.
fn strip_meta_deep(value: Value, collector: &mut Vec<Value>) -> (Value, bool) {
    match value {
        Value::Object(mut obj) => {
            let mut changed = false;
            if let Some(meta) = obj.remove("_meta") {
                changed = true;
                if let Some(usage) = meta.get("usage").and_then(|u| u.as_array()) {
                    collector.extend(usage.iter().cloned());
                }
            }
            for child in obj.values_mut() {
                let taken = std::mem::take(child);
                let (stripped, child_changed) = strip_meta_deep(taken, collector);
                *child = stripped;
                changed |= child_changed;
            }
            (Value::Object(obj), changed)
        }
        Value::Array(mut items) => {
            let mut changed = false;
            for child in items.iter_mut() {
                let taken = std::mem::take(child);
                let (stripped, child_changed) = strip_meta_deep(taken, collector);
                *child = stripped;
                changed |= child_changed;
            }
            (Value::Array(items), changed)
        }
        Value::String(text) => {
            let trimmed = text.trim_start();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                if let Ok(inner) = serde_json::from_str::<Value>(&text) {
                    let (stripped, changed) = strip_meta_deep(inner, collector);
                    if changed {
                        if let Ok(re_encoded) = serde_json::to_string(&stripped) {
                            return (Value::String(re_encoded), true);
                        }
                    }
                }
            }
            (Value::String(text), false)
        }
        other => (other, false),
    }
}

/// Build the omniagent's own usage entry for one LLM call, mirroring the
/// dsh-agent field set.
///
/// `omniagent: true` and `agent` are filled by the MAIN LOOP (never by the
/// provider); token counts come from the provider usage result; `cost` is the
/// SERVICE-SIDE price-table block ([`crate::agent::pricing::cost_block`]) -
/// never agent estimation - and stays `null` for a route the table does not
/// price. `cache_write_tokens` is not tracked by the core provider parsing
/// today, so it stays `null` (and is priced as 0).
pub fn omniagent_usage_entry(usage: &Usage, provider: &str, model: &str, agent: &str) -> Value {
    omniagent_usage_entry_at(usage, provider, model, agent, chrono::Utc::now())
}

/// [`omniagent_usage_entry`] with an EXPLICIT call time (UTC).
///
/// The rate class (peak / off-peak) of the recorded cost is chosen from `at`
/// against the `off_peak:` calendar of `{OMNI_DIR}/config/model_prices.yml`
/// (`crate::agent::pricing::cost_block_at`). The core ledger records no
/// per-call timestamp, so the plain [`omniagent_usage_entry`] uses the time the
/// entry is built - the entry is built immediately after the provider call
/// returns.
pub fn omniagent_usage_entry_at(
    usage: &Usage,
    provider: &str,
    model: &str,
    agent: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> Value {
    let cost = crate::agent::pricing::cost_block_at(
        provider,
        model,
        u64::from(usage.prompt_tokens),
        u64::from(usage.cached_tokens.unwrap_or(0)),
        u64::from(usage.completion_tokens),
        0,
        at,
    );
    json!({
        "omniagent": true,
        "agent": agent,
        "input_tokens": usage.prompt_tokens,
        "output_tokens": usage.completion_tokens,
        "total_tokens": usage.prompt_tokens.saturating_add(usage.completion_tokens),
        "cached_input_tokens": usage.cached_tokens,
        "cache_write_tokens": Value::Null,
        "reasoning_tokens": usage.reasoning_tokens,
        "cost": cost,
        "provider": provider,
        "model": model,
    })
}

/// Sums over the usage array items (missing fields count as 0).
///
/// FIELD SEMANTICS (operator UPDATE 2026-10-02, telegram threads 3915/3916/
/// 3917): every `input` figure is CACHE-MISS (fresh) input only, never
/// cache-hit + miss. `full_*` = the omniagent's own numbers + the
/// sub-agent/dsh numbers for that same field.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageAggregates {
    /// omniagent cache-miss input + dsh cache-miss input.
    pub full_input_tokens: u64,
    pub full_cached_tokens: u64,
    pub full_output_tokens: u64,
    pub full_reasoning_tokens: u64,
    /// FULL cost (USD): omniagent + dsh/sub-agent LLM calls.
    pub cost: f64,
    /// omniagent-only cost (USD) - the `threads.cost` column value.
    pub omniagent_cost: f64,
    /// omniagent cache-MISS input only - the `threads.input_tokens` value.
    pub omniagent_input_tokens: u64,
}

fn item_u64(item: &Value, key: &str) -> u64 {
    item.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// True when a usage item counts toward the aggregates: a real LLM call, or
/// an item that carries no call-kind marker at all.
///
/// The workstation's dsh layer emits, besides one item per LLM call
/// (`details.kind == "llm-call"`), a per-session ROLL-UP item
/// (`details.kind == "agent-aggregate"`, carrying `llm_calls` / `tool_calls`)
/// whose tokens and cost are the SUM of those same call items. Counting both
/// counted every dsh call twice (review thread 3890 on dev thread 3912:
/// `full_input_tokens` 154381 instead of 138086, `cost` 0.026022 instead of
/// 0.020250), while the operator semantics are "omniagent LLM calls + dsh LLM
/// calls" (telegram 3887) - a roll-up of calls already in the array is not
/// another call. Items without `details.kind` (the omniagent's own entries,
/// and any shape that carries no marker) are counted as before.
fn is_llm_call_item(item: &Value) -> bool {
    match item
        .get("details")
        .and_then(|d| d.get("kind"))
        .and_then(|k| k.as_str())
    {
        Some(kind) => kind == "llm-call",
        None => true,
    }
}

/// Sum the respective fields over the LLM-call usage array items. `full_*`
/// fields default to 0 when the array is empty or the items carry no such
/// field; `cost` sums `cost.amount_usd` (missing = 0). Roll-up items
/// (`details.kind` present and not `"llm-call"`, see [`is_llm_call_item`])
/// are skipped so a dsh session aggregate never double counts the calls it
/// summarizes. The items themselves stay in the Usage message array (they are
/// raw tool output) - only the aggregates skip them.
pub fn sum_usage_items(entries: &[Value]) -> UsageAggregates {
    let mut agg = UsageAggregates::default();
    for item in entries {
        if !is_llm_call_item(item) {
            continue;
        }
        agg.full_input_tokens = agg
            .full_input_tokens
            .saturating_add(item_u64(item, "input_tokens"));
        agg.full_cached_tokens = agg
            .full_cached_tokens
            .saturating_add(item_u64(item, "cached_input_tokens"));
        agg.full_output_tokens = agg
            .full_output_tokens
            .saturating_add(item_u64(item, "output_tokens"));
        agg.full_reasoning_tokens = agg
            .full_reasoning_tokens
            .saturating_add(item_u64(item, "reasoning_tokens"));
        if let Some(amount) = item
            .get("cost")
            .and_then(|c| c.get("amount_usd"))
            .and_then(|v| v.as_f64())
        {
            agg.cost += amount;
        }
    }
    agg
}

/// Sum the items whose `omniagent` flag equals `want` (a missing flag counts
/// as `false`, i.e. as a non-omniagent / dsh item).
fn sum_usage_items_where(entries: &[Value], want: bool) -> UsageAggregates {
    let filtered: Vec<Value> = entries
        .iter()
        .filter(|item| {
            item.get("omniagent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                == want
        })
        .cloned()
        .collect();
    sum_usage_items(&filtered)
}

/// Aggregate fields for the THREADS TABLE (operator UPDATE 2026-09-30 thread
/// 3702: the new fields are columns on the threads table, populated at thread
/// end exactly like `input_tokens` / `cached_tokens` / `output_tokens` are
/// today).
///
/// Semantics (operator, telegram threads 3887 + 3915/3916/3917): `full_*` =
/// omniagent + dsh agents, and every `input` figure is CACHE-MISS only.
///
/// The two item kinds do NOT report `input_tokens` the same way, verified on
/// real dev thread 3899 (2026-10-02):
///   * the OMNIAGENT entries carry the provider's `prompt_tokens`, i.e. TOTAL
///     input with the cache hit INCLUDED (33 items summed to 1,928,385 with
///     1,567,488 cached);
///   * the dsh/workstation entries carry the FRESH (cache-miss) input and the
///     hit in `cached_input_tokens` (112 items summed to 398,032 input +
///     5,025,024 cached, and `total_tokens` = input + cached + output held
///     exactly).
///
/// So the omniagent sub-total is min-clamped against the omniagent's own
/// cumulative totals (still authoritative) and its cache hit is SUBTRACTED to
/// obtain the miss-only figure; the dsh sub-total needs no subtraction (its
/// input is already miss-only). Result:
/// `full_input_tokens = (min(input, cum.prompt) - min(cached, cum.cached)) +
///  dsh_input`.
///
/// `cost` is the FULL cost (omniagent + dsh) and `omniagent_cost` the
/// omniagent-only share (operator threads 3916/3917: `threads.cost` is the
/// omniagent-only cost, the new `threads.full_cost` is the combined one).
pub fn aggregate_fields(entries: &[Value], cumulative: Option<&Usage>) -> UsageAggregates {
    let omniagent_sum = sum_usage_items_where(entries, true);
    let dsh_sum = sum_usage_items_where(entries, false);
    let (omniagent_total_input, omniagent_cached, omniagent_output, omniagent_reasoning) =
        match cumulative {
            Some(cum) => (
                omniagent_sum
                    .full_input_tokens
                    .min(cum.prompt_tokens as u64),
                omniagent_sum
                    .full_cached_tokens
                    .min(cum.cached_tokens.unwrap_or(0) as u64),
                omniagent_sum
                    .full_output_tokens
                    .min(cum.completion_tokens as u64),
                omniagent_sum
                    .full_reasoning_tokens
                    .min(cum.reasoning_tokens.unwrap_or(0) as u64),
            ),
            None => (
                omniagent_sum.full_input_tokens,
                omniagent_sum.full_cached_tokens,
                omniagent_sum.full_output_tokens,
                omniagent_sum.full_reasoning_tokens,
            ),
        };
    let omniagent_miss_input = omniagent_total_input.saturating_sub(omniagent_cached);
    // dsh entries report cache-MISS `input_tokens` already (verified on dev
    // thread 3899: `total_tokens` == input + cached + output for all 112 dsh
    // items), so subtracting their (much larger) cache hit would collapse the
    // dsh share to 0.
    let dsh_miss_input = dsh_sum.full_input_tokens;
    UsageAggregates {
        full_input_tokens: omniagent_miss_input.saturating_add(dsh_miss_input),
        full_cached_tokens: omniagent_cached.saturating_add(dsh_sum.full_cached_tokens),
        full_output_tokens: omniagent_output.saturating_add(dsh_sum.full_output_tokens),
        full_reasoning_tokens: omniagent_reasoning.saturating_add(dsh_sum.full_reasoning_tokens),
        cost: omniagent_sum.cost + dsh_sum.cost,
        omniagent_cost: omniagent_sum.cost,
        omniagent_input_tokens: omniagent_miss_input,
    }
}

/// Content of the thread-end "Usage"-type message: the usage ARRAY itself -
/// all `_meta.usage` items in call order + the omniagent's own LLM-call
/// entries (operator UPDATE 2026-09-30 thread 3709: "the message should be
/// the array, and message type 'Usage'"). No wrapper object, no "usage" key,
/// no `full_*` aggregate fields (those live on the threads table, see
/// [`aggregate_fields`]).
pub fn usage_message_content(entries: &[Value]) -> Value {
    Value::Array(entries.to_vec())
}

/// Per-thread snapshot of the usage entries collected so far by the running
/// loop.
///
/// WHY: the thread-end Usage message must be CREATED before the thread's final
/// message (operator correction 2026-10-02, thread 3883: the Usage message is
/// the 2nd-last message and the summary - or the fail-thread tool result - is
/// last), and the builtin fail-thread tool persists that final message from
/// inside the tool dispatch, where the loop's `usage_entries` vector is not in
/// scope. The loop publishes its entries before every tool round; the fail
/// tool reads the snapshot so a Failed thread's Usage message still carries
/// the real per-call array instead of an empty one.
static THREAD_USAGE: LazyLock<Mutex<HashMap<i64, Vec<Value>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Publish the entries collected so far for `thread_id`. An empty list clears
/// the slot.
pub fn publish_thread_usage(thread_id: i64, entries: &[Value]) {
    let mut map = THREAD_USAGE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if entries.is_empty() {
        map.remove(&thread_id);
    } else {
        map.insert(thread_id, entries.to_vec());
    }
}

/// The entries published for `thread_id` (empty when none were published).
pub fn thread_usage_snapshot(thread_id: i64) -> Vec<Value> {
    THREAD_USAGE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&thread_id)
        .cloned()
        .unwrap_or_default()
}

/// Drop the published entries of a terminal thread (no leak across threads).
pub fn clear_thread_usage(thread_id: i64) {
    THREAD_USAGE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(&thread_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u32, output: u32, cached: Option<u32>, reasoning: Option<u32>) -> Usage {
        Usage {
            prompt_tokens: input,
            completion_tokens: output,
            cached_tokens: cached,
            reasoning_tokens: reasoning,
        }
    }

    // ── strip_meta_and_collect ─────────────────────────────────────────────

    #[test]
    fn strips_top_level_meta_and_collects_usage_array_in_order() {
        let content = r#"{"status":"ok","tool":"agent_run","result":{"exitCode":0},"_meta":{"usage":[{"agent":"dsh","input_tokens":10},{"agent":"dsh","input_tokens":20}]}}"#;
        let mut collector = Vec::new();
        let stripped = strip_meta_and_collect(content, &mut collector);
        let v: Value = serde_json::from_str(&stripped).unwrap();
        assert!(v.get("_meta").is_none(), "_meta must be removed");
        assert_eq!(v["status"], "ok");
        assert_eq!(v["result"]["exitCode"], 0);
        assert_eq!(collector.len(), 2, "both usage items collected");
        assert_eq!(collector[0]["agent"], "dsh");
        assert_eq!(collector[0]["input_tokens"], 10);
        assert_eq!(collector[1]["input_tokens"], 20);
    }

    #[test]
    fn non_json_content_passes_through_unchanged() {
        let content = "plain text result, nothing to strip";
        let mut collector = Vec::new();
        assert_eq!(strip_meta_and_collect(content, &mut collector), content);
        assert!(collector.is_empty());
    }

    #[test]
    fn json_without_meta_passes_through_unchanged() {
        let content = r#"{"status":"ok","result":1}"#;
        let mut collector = Vec::new();
        assert_eq!(strip_meta_and_collect(content, &mut collector), content);
        assert!(collector.is_empty());
    }

    #[test]
    fn meta_without_usage_array_collects_nothing_but_still_strips() {
        let content = r#"{"status":"ok","_meta":{"channel_id":"x"}}"#;
        let mut collector = Vec::new();
        let stripped = strip_meta_and_collect(content, &mut collector);
        let v: Value = serde_json::from_str(&stripped).unwrap();
        assert!(v.get("_meta").is_none());
        assert!(collector.is_empty());
    }

    #[test]
    fn meta_usage_non_array_collects_nothing() {
        let content = r#"{"status":"ok","_meta":{"usage":{"input_tokens":1}}}"#;
        let mut collector = Vec::new();
        let stripped = strip_meta_and_collect(content, &mut collector);
        let v: Value = serde_json::from_str(&stripped).unwrap();
        assert!(v.get("_meta").is_none());
        assert!(collector.is_empty());
    }

    // ── omniagent_usage_entry ──────────────────────────────────────────────

    #[test]
    fn omniagent_entry_has_dsh_field_set_with_main_loop_filled_fields() {
        // A valid `config/model_prices.yml` is present (pricing.rs tests seed
        // one): an unknown ROUTE stays null - it is never fabricated as 0.
        let _ = crate::agent::pricing::test_support::seeded_data_dir();
        let entry = omniagent_usage_entry(
            &usage(100, 25, Some(60), Some(5)),
            "deepseek",
            "deepseek-v4.1",
            "omni",
        );
        assert_eq!(entry["omniagent"], true, "filled by the main loop");
        assert_eq!(entry["agent"], "omni", "agent name from the profile");
        assert_eq!(entry["input_tokens"], 100);
        assert_eq!(entry["output_tokens"], 25);
        assert_eq!(entry["total_tokens"], 125);
        assert_eq!(entry["cached_input_tokens"], 60);
        assert!(entry["cache_write_tokens"].is_null());
        assert_eq!(entry["reasoning_tokens"], 5);
        // `deepseek-v4.1` is not a priced route: an unknown price stays null
        // instead of being fabricated as 0.
        assert!(entry["cost"].is_null(), "unknown route -> null cost");
        assert_eq!(entry["provider"], "deepseek");
        assert_eq!(entry["model"], "deepseek-v4.1");
    }

    #[test]
    fn omniagent_entry_prices_a_known_route_service_side() {
        let _ = crate::agent::pricing::test_support::seeded_data_dir();
        let entry = omniagent_usage_entry_at(
            &usage(1_000_000, 200_000, Some(500_000), Some(5)),
            "deepseek",
            "deepseek-v4-flash",
            "omni",
            peak_time(),
        );
        let cost = &entry["cost"];
        assert!(!cost.is_null(), "known route must carry a cost block");
        assert_eq!(cost["is_estimate"], true);
        assert_eq!(cost["source"], crate::agent::pricing::PRICING_SOURCE);
        let pricing_ref = cost["pricing_ref"]
            .as_str()
            .expect("pricing_ref is a string");
        assert!(
            pricing_ref.starts_with("config/model_prices.yml@"),
            "provenance must name the external file: {pricing_ref}"
        );
        let amount = cost["amount_usd"].as_f64().expect("amount_usd is numeric");
        // 1,000,000 prompt tokens of which 500,000 are cache hits (500,000
        // uncached) + 200,000 output at the DeepSeek flash rate:
        // 0.15 + 0.003 + 0.24 = 0.393 USD.
        assert!((amount - 0.393).abs() < 1e-9, "got {}", amount);
    }

    /// 2026-10-05T02:00:00Z = a Monday inside the configured peak window.
    fn peak_time() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-10-05T02:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    /// 2026-10-05T20:00:00Z = a Monday outside every peak window.
    fn off_peak_time() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-10-05T20:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn omniagent_entry_selects_the_rate_class_from_the_call_time() {
        let _ = crate::agent::pricing::test_support::seeded_data_dir();
        let tokens = usage(1_000_000, 200_000, Some(500_000), Some(5));
        let peak = omniagent_usage_entry_at(
            &tokens,
            "deepseek",
            "deepseek-v4-flash",
            "omni",
            peak_time(),
        );
        let off = omniagent_usage_entry_at(
            &tokens,
            "deepseek",
            "deepseek-v4-flash",
            "omni",
            off_peak_time(),
        );
        assert_eq!(peak["cost"]["rate_class"], "peak");
        assert_eq!(off["cost"]["rate_class"], "off-peak");
        assert_eq!(off["cost"]["off_peak_factor"], 0.5);
        assert_eq!(off["cost"]["call_time"], "2026-10-05T20:00:00Z");
        let peak_amount = peak["cost"]["amount_usd"].as_f64().unwrap();
        let off_amount = off["cost"]["amount_usd"].as_f64().unwrap();
        assert!(
            (off_amount - peak_amount / 2.0).abs() < 1e-9,
            "off-peak must be half: peak={peak_amount} off={off_amount}"
        );
        // The token counts themselves never depend on the rate class.
        assert_eq!(peak["input_tokens"], off["input_tokens"]);
        assert_eq!(peak["total_tokens"], off["total_tokens"]);
    }

    #[test]
    fn thread_usage_registry_round_trips_and_clears() {
        let entries = json!([{"agent": "dsh", "input_tokens": 7}]);
        let entries: Vec<Value> = serde_json::from_value(entries).unwrap();
        assert!(thread_usage_snapshot(9_999_999).is_empty());
        publish_thread_usage(9_999_999, &entries);
        assert_eq!(thread_usage_snapshot(9_999_999).len(), 1);
        publish_thread_usage(9_999_999, &[]);
        assert!(thread_usage_snapshot(9_999_999).is_empty());
        publish_thread_usage(9_999_999, &entries);
        clear_thread_usage(9_999_999);
        assert!(thread_usage_snapshot(9_999_999).is_empty());
    }

    // ── aggregates ─────────────────────────────────────────────────────────

    #[test]
    fn aggregates_sum_over_items_with_missing_fields_as_zero() {
        let entries = json!([
            {"input_tokens": 10, "output_tokens": 2, "cached_input_tokens": 8, "reasoning_tokens": 1, "cost": {"amount_usd": 0.001}},
            {"input_tokens": 20, "output_tokens": 3},
            {"agent": "dsh"} // no numeric fields at all
        ]);
        let entries: Vec<Value> = serde_json::from_value(entries).unwrap();
        let agg = sum_usage_items(&entries);
        assert_eq!(agg.full_input_tokens, 30);
        assert_eq!(agg.full_output_tokens, 5);
        assert_eq!(agg.full_cached_tokens, 8);
        assert_eq!(agg.full_reasoning_tokens, 1);
        assert!((agg.cost - 0.001).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_array_yields_default_zero_aggregates() {
        let agg = sum_usage_items(&[]);
        assert_eq!(agg, UsageAggregates::default());
        assert_eq!(agg.full_input_tokens, 0);
        assert_eq!(agg.cost, 0.0);
    }

    #[test]
    fn agent_aggregate_rollup_is_not_counted_twice() {
        // Shape observed on the omnidev dev stack (dev thread 3912, review
        // 3890): one `llm-call` item plus the `agent-aggregate` roll-up of
        // that very call. Summing both counted the dsh call twice.
        let entries: Vec<Value> = serde_json::from_value(json!([
            {
                "agent": "researcher",
                "input_tokens": 14646,
                "output_tokens": 20,
                "cost": {"amount_usd": 0.0044658},
                "details": {"kind": "llm-call", "message_id": "m1"}
            },
            {
                "agent": "researcher",
                "input_tokens": 14646,
                "output_tokens": 20,
                "cost": {"amount_usd": 0.0044658},
                "details": {"kind": "agent-aggregate", "llm_calls": 1, "tool_calls": 2}
            }
        ]))
        .unwrap();
        let agg = sum_usage_items(&entries);
        assert_eq!(
            agg.full_input_tokens, 14646,
            "the roll-up is not another call"
        );
        assert_eq!(agg.full_output_tokens, 20);
        assert!((agg.cost - 0.0044658).abs() < 1e-12, "got {}", agg.cost);
        // The roll-up itself stays IN the Usage message array (raw tool
        // output); only the aggregates skip it.
        assert_eq!(usage_message_content(&entries).as_array().unwrap().len(), 2);
    }

    #[test]
    fn llm_call_kind_and_markerless_items_are_counted() {
        let entries: Vec<Value> = serde_json::from_value(json!([
            {"agent": "dsh", "input_tokens": 100, "details": {"kind": "llm-call"}},
            {"input_tokens": 5},
            {"agent": "dsh", "input_tokens": 7, "details": {"kind": "tool-call"}}
        ]))
        .unwrap();
        let agg = sum_usage_items(&entries);
        assert_eq!(agg.full_input_tokens, 105, "llm-call + markerless only");
    }

    #[test]
    fn dsh_rollup_is_excluded_from_the_clamped_aggregate_fields() {
        // full_* = omniagent LLM calls + dsh LLM calls, never the dsh roll-up.
        let entries: Vec<Value> = serde_json::from_value(json!([
            {"omniagent": true, "input_tokens": 100, "output_tokens": 20, "cost": {"amount_usd": 0.1}},
            {"agent": "researcher", "input_tokens": 500, "output_tokens": 100, "cost": {"amount_usd": 0.5},
             "details": {"kind": "llm-call"}},
            {"agent": "researcher", "input_tokens": 500, "output_tokens": 100, "cost": {"amount_usd": 0.5},
             "details": {"kind": "agent-aggregate", "llm_calls": 1}}
        ]))
        .unwrap();
        let cum = usage(100, 20, Some(60), Some(5));
        let agg = aggregate_fields(&entries, Some(&cum));
        assert_eq!(agg.full_input_tokens, 600, "min(100,100) + 500");
        assert_eq!(agg.full_output_tokens, 120, "min(20,20) + 100");
        assert!((agg.cost - 0.6).abs() < 1e-9, "got {}", agg.cost);
    }

    #[test]
    fn full_fields_clamp_only_the_omniagent_part_and_add_dsh_items() {
        // Operator thread 3887: full_* = omniagent + dsh agents. The clamp
        // against the bare omniagent totals therefore applies to the
        // omniagent sub-total only; the dsh items are always added on top.
        let entries = json!([
            {"agent": "dsh", "input_tokens": 500, "output_tokens": 100, "cached_input_tokens": 400, "reasoning_tokens": 50, "cost": {"amount_usd": 0.5}},
            {"omniagent": true, "input_tokens": 100, "output_tokens": 20, "cached_input_tokens": 60, "reasoning_tokens": 5, "cost": {"amount_usd": 0.1}}
        ]);
        let entries: Vec<Value> = serde_json::from_value(entries).unwrap();
        let cum = usage(100, 20, Some(60), Some(5));
        let agg = aggregate_fields(&entries, Some(&cum));
        // input is CACHE-MISS only (operator thread 3915): omniagent
        // min(100,100) - min(60,60) = 40; the dsh item's input is already
        // miss-only, so it is added as-is (no cache subtraction).
        assert_eq!(
            agg.full_input_tokens, 540,
            "omniagent miss 40 + dsh miss 500"
        );
        assert_eq!(agg.omniagent_input_tokens, 40);
        assert_eq!(agg.full_output_tokens, 120, "min(20, 20) + 100");
        assert_eq!(agg.full_cached_tokens, 460, "min(60, 60) + 400");
        assert_eq!(agg.full_reasoning_tokens, 55, "min(5, 5) + 50");
        assert!((agg.cost - 0.6).abs() < 1e-9, "cost is the plain sum");
        assert!(
            (agg.omniagent_cost - 0.1).abs() < 1e-9,
            "omniagent-only share"
        );
    }

    #[test]
    fn thread_3899_real_numbers_split_miss_only_input_and_the_two_costs() {
        // Raw data from the omnidev dev DB, thread 3899 (2026-10-02):
        //   33 omniagent items: input 1,928,385 (cached 1,567,488), cost 0.175425228
        //   112 dsh llm-call items: input 398,032 (miss), cached 5,025,024,
        //   output 101,612, cost 0.271494144 (the old COMBINED `cost` column
        //   held 0.446919372 = omniagent + dsh).
        let entries = json!([
            {"omniagent": true, "input_tokens": 1928385, "cached_input_tokens": 1567488, "output_tokens": 48126, "cost": {"amount_usd": 0.175425228}},
            {"agent": "researcher", "input_tokens": 398032, "cached_input_tokens": 5025024, "output_tokens": 101612, "cost": {"amount_usd": 0.271494144}, "details": {"kind": "llm-call"}}
        ]);
        let entries: Vec<Value> = serde_json::from_value(entries).unwrap();
        let cum = usage(1928385, 48126, Some(1567488), Some(0));
        let agg = aggregate_fields(&entries, Some(&cum));
        assert_eq!(agg.omniagent_input_tokens, 360_897, "1,928,385 - 1,567,488");
        assert_eq!(
            agg.full_input_tokens, 758_929,
            "omniagent miss 360,897 + dsh miss 398,032"
        );
        assert_eq!(agg.full_cached_tokens, 6_592_512);
        assert_eq!(agg.full_output_tokens, 149_738);
        assert!(
            (agg.omniagent_cost - 0.175425228).abs() < 1e-9,
            "got {}",
            agg.omniagent_cost
        );
        assert!(
            (agg.cost - 0.446919372).abs() < 1e-9,
            "full cost = omniagent + dsh"
        );
    }

    #[test]
    fn omniagent_only_array_is_still_clamped_against_the_bare_totals() {
        let entries = json!([
            {"omniagent": true, "input_tokens": 400, "output_tokens": 90},
            {"omniagent": true, "input_tokens": 200, "output_tokens": 30}
        ]);
        let entries: Vec<Value> = serde_json::from_value(entries).unwrap();
        let cum = usage(100, 20, Some(60), Some(5));
        let agg = aggregate_fields(&entries, Some(&cum));
        assert_eq!(agg.full_input_tokens, 100, "min(100, 600)");
        assert_eq!(agg.full_output_tokens, 20, "min(20, 120)");
    }

    #[test]
    fn strip_meta_reaches_string_encoded_envelopes_and_multi_tool_wrappers() {
        // Shape observed on the omnidev dev stack (thread 3909): the
        // workstation answer carrying `_meta` arrives string-encoded inside a
        // background-dispatch envelope, itself inside a multi-tool wrapper.
        let dsh_entry = json!({
            "agent": "researcher",
            "input_tokens": 15118,
            "cost": {"amount_usd": 0.0046698, "is_estimate": true, "source": "price_table_v1"}
        });
        let workstation_answer = json!({
            "status": "ok",
            "tool": "agent_run",
            "result": {"role": "researcher", "output": "done"},
            "_meta": {"usage": [dsh_entry]}
        })
        .to_string();
        let envelope = json!({
            "status": "completed",
            "task_id": "task_1_2",
            "tool": "workstation__tool",
            "logs": "tool completed",
            "result": workstation_answer
        })
        .to_string();
        let content = json!({
            "tool": "workstation__tool",
            "input": {"tool": "agent_run"},
            "output": envelope
        })
        .to_string();

        let mut collector: Vec<Value> = Vec::new();
        let stripped = strip_meta_and_collect(&content, &mut collector);

        assert_eq!(
            collector.len(),
            1,
            "the nested _meta.usage item is collected"
        );
        assert_eq!(collector[0]["agent"], "researcher");
        assert_eq!(collector[0]["cost"]["amount_usd"], 0.0046698);
        assert!(!stripped.contains("_meta"), "no _meta survives anywhere");
        // The rest of the payload survives (same leaves, still parseable).
        let parsed: Value = serde_json::from_str(&stripped).unwrap();
        let inner: Value = serde_json::from_str(parsed["output"].as_str().unwrap()).unwrap();
        let answer: Value = serde_json::from_str(inner["result"].as_str().unwrap()).unwrap();
        assert_eq!(answer["status"], "ok");
        assert_eq!(answer["result"]["role"], "researcher");
    }

    #[test]
    fn full_fields_without_cumulative_are_plain_sums() {
        let entries = json!([
            {"input_tokens": 10, "output_tokens": 2, "cached_input_tokens": 8, "reasoning_tokens": 1}
        ]);
        let entries: Vec<Value> = serde_json::from_value(entries).unwrap();
        let agg = aggregate_fields(&entries, None);
        // A marker-less item is treated as a dsh item (missing `omniagent`
        // flag == false), whose `input_tokens` is already miss-only.
        assert_eq!(agg.full_input_tokens, 10);
        assert_eq!(agg.full_output_tokens, 2);
        assert_eq!(agg.full_cached_tokens, 8);
        assert_eq!(agg.full_reasoning_tokens, 1);
        assert_eq!(agg.cost, 0.0);
    }

    #[test]
    fn usage_message_content_is_the_array_itself() {
        let entries = json!([
            {"agent": "dsh", "input_tokens": 10, "output_tokens": 2, "cost": {"amount_usd": 0.0005}},
            {"omniagent": true, "input_tokens": 100, "output_tokens": 25}
        ]);
        let entries: Vec<Value> = serde_json::from_value(entries).unwrap();
        let content = usage_message_content(&entries);
        let arr = content.as_array().expect("content is the array itself");
        assert_eq!(arr.len(), 2, "all items in order");
        assert_eq!(arr[0]["agent"], "dsh");
        assert_eq!(arr[1]["omniagent"], true);
        // No wrapper object: no "usage" key, no full_* aggregates.
        assert!(content.get("usage").is_none());
        assert!(content.get("full_input_tokens").is_none());
        assert!(content.get("full_cached_tokens").is_none());
        assert!(content.get("full_output_tokens").is_none());
        assert!(content.get("full_reasoning_tokens").is_none());
        assert!(content.get("cost").is_none());
    }
}
