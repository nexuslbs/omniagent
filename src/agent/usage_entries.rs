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
//! 1. strips the `_meta` field from every tool result (it must never reach the
//!    agent context nor the stored thread messages);
//! 2. concatenates all `_meta.usage` items from all tool call results of the
//!    thread into one array, plus the omniagent's own LLM-call entries;
//! 3. inserts a "Usage"-type message at thread end (just before the last
//!    message) carrying ONLY that array (operator UPDATE 2026-09-30 threads
//!    3705/3707/3709: no wrapper object, no "usage" key, no `full_*` keys);
//! 4. the aggregate fields (`full_input_tokens`, `full_cached_tokens`,
//!    `full_output_tokens`, `full_reasoning_tokens`, `cost`) are computed as
//!    sums over the array items, min-clamped against the omniagent's own bare
//!    totals (e.g. `full_input_tokens = min(input_tokens, sum_over_usage)`),
//!    and written to the THREADS TABLE at thread end (operator UPDATE 3702) -
//!    they never appear on the Usage message.
//!
//! RAW RESULTS ONLY: none of these fields are ever changed by agents - they
//! are raw provider and tool results. Cost is never estimated by the agent
//! (the omniagent's own entries carry `cost: null`; only a service-side price
//! table could fill it, which is out of scope here).

use crate::llm::Usage;
use serde_json::{json, Value};

/// Strip a top-level `_meta` key from a tool result payload and collect its
/// `_meta.usage` array items (in order) into `collector`.
///
/// The content is only rewritten when it parses as a JSON object carrying a
/// top-level `_meta` key: non-JSON content and JSON without `_meta` pass
/// through byte-identical. A `_meta.usage` that is not an array collects
/// nothing (the `_meta` key is still stripped).
pub fn strip_meta_and_collect(content: &str, collector: &mut Vec<Value>) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(content) else {
        return content.to_string();
    };
    let Some(obj) = v.as_object_mut() else {
        return content.to_string();
    };
    let Some(meta) = obj.remove("_meta") else {
        return content.to_string();
    };
    if let Some(usage) = meta.get("usage").and_then(|u| u.as_array()) {
        collector.extend(usage.iter().cloned());
    }
    serde_json::to_string(&v).unwrap_or_else(|_| content.to_string())
}

/// Build the omniagent's own usage entry for one LLM call, mirroring the
/// dsh-agent field set.
///
/// `omniagent: true` and `agent` are filled by the MAIN LOOP (never by the
/// provider); token counts come from the provider usage result; `cost` is
/// `null` because the agent never estimates cost (a service-side price table
/// is out of scope). `cache_write_tokens` is not tracked by the core provider
/// parsing today, so it stays `null`.
pub fn omniagent_usage_entry(usage: &Usage, provider: &str, model: &str, agent: &str) -> Value {
    json!({
        "omniagent": true,
        "agent": agent,
        "input_tokens": usage.prompt_tokens,
        "output_tokens": usage.completion_tokens,
        "total_tokens": usage.prompt_tokens.saturating_add(usage.completion_tokens),
        "cached_input_tokens": usage.cached_tokens,
        "cache_write_tokens": Value::Null,
        "reasoning_tokens": usage.reasoning_tokens,
        "cost": Value::Null,
        "provider": provider,
        "model": model,
    })
}

/// Sums over the usage array items (missing fields count as 0).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageAggregates {
    pub full_input_tokens: u64,
    pub full_cached_tokens: u64,
    pub full_output_tokens: u64,
    pub full_reasoning_tokens: u64,
    pub cost: f64,
}

fn item_u64(item: &Value, key: &str) -> u64 {
    item.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Sum the respective fields over the usage array items. `full_*` fields
/// default to 0 when the array is empty or the items carry no such field;
/// `cost` sums `cost.amount_usd` (missing = 0).
pub fn sum_usage_items(entries: &[Value]) -> UsageAggregates {
    let mut agg = UsageAggregates::default();
    for item in entries {
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

/// Aggregate fields for the THREADS TABLE (operator UPDATE 2026-09-30 thread
/// 3702: the new fields are columns on the threads table, populated at thread
/// end exactly like `input_tokens` / `cached_tokens` / `output_tokens` are
/// today), min-clamped against the omniagent's own bare totals (requirement
/// 8): `full_input_tokens = min(input_tokens, sum_over_usage)` - the
/// omniagent's recorded totals (threads.input_tokens etc., fed from
/// `cumulative_usage`) are the authoritative billed numbers, and the array
/// sum is never allowed to exceed them for the omniagent-only calculated
/// fields. `cost` has no bare counterpart (the agent never estimates cost),
/// so it is the plain sum.
pub fn aggregate_fields(entries: &[Value], cumulative: Option<&Usage>) -> UsageAggregates {
    let sum = sum_usage_items(entries);
    let Some(cum) = cumulative else {
        return sum;
    };
    UsageAggregates {
        full_input_tokens: sum.full_input_tokens.min(cum.prompt_tokens as u64),
        full_cached_tokens: sum
            .full_cached_tokens
            .min(cum.cached_tokens.unwrap_or(0) as u64),
        full_output_tokens: sum.full_output_tokens.min(cum.completion_tokens as u64),
        full_reasoning_tokens: sum
            .full_reasoning_tokens
            .min(cum.reasoning_tokens.unwrap_or(0) as u64),
        cost: sum.cost,
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
        assert!(entry["cost"].is_null(), "agent never estimates cost");
        assert_eq!(entry["provider"], "deepseek");
        assert_eq!(entry["model"], "deepseek-v4.1");
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
    fn full_fields_are_min_clamped_against_bare_omniagent_values() {
        // Array sum (tool + omniagent entries) exceeds the omniagent's bare
        // totals: the aggregate must be clamped to the bare values.
        let entries = json!([
            {"agent": "dsh", "input_tokens": 500, "output_tokens": 100, "cached_input_tokens": 400, "reasoning_tokens": 50},
            {"omniagent": true, "input_tokens": 100, "output_tokens": 20, "cached_input_tokens": 60, "reasoning_tokens": 5}
        ]);
        let entries: Vec<Value> = serde_json::from_value(entries).unwrap();
        let cum = usage(100, 20, Some(60), Some(5));
        let agg = aggregate_fields(&entries, Some(&cum));
        assert_eq!(agg.full_input_tokens, 100, "min(100, 600)");
        assert_eq!(agg.full_output_tokens, 20, "min(20, 120)");
        assert_eq!(agg.full_cached_tokens, 60, "min(60, 460)");
        assert_eq!(agg.full_reasoning_tokens, 5, "min(5, 55)");
    }

    #[test]
    fn full_fields_without_cumulative_are_plain_sums() {
        let entries = json!([
            {"input_tokens": 10, "output_tokens": 2, "cached_input_tokens": 8, "reasoning_tokens": 1}
        ]);
        let entries: Vec<Value> = serde_json::from_value(entries).unwrap();
        let agg = aggregate_fields(&entries, None);
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
