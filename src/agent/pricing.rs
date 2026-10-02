//! Service-side price table for the omniagent's own provider calls.
//!
//! WHY
//! ---
//! Operator correction (telegram thread 3882, 2026-10-02): the thread-end
//! `usage` message carried `cost: null` on every omniagent entry and
//! `threads.cost` was therefore 0 for the whole database. Requirement 7 of
//! the `strip_meta` task asks the omniagent's own provider calls to return the
//! SAME field set as the dsh agents, including
//! `cost{amount_usd,is_estimate,source,pricing_ref}`.
//!
//! This module is the SOURCE OF TRUTH for that cost, exactly like the dsh side
//! (`workstation-plugins/shared/usage.ts` `PRICE_TABLE`): a COMPILED CONSTANT
//! that a code change (with provenance) updates, never a runtime guess and
//! never an agent-supplied value ("No cost estimation by the agent" - this is
//! service-side pricing, not agent estimation).
//!
//! A route the table does not price answers `cost: null` - an unknown price is
//! NEVER fabricated as 0.
//!
//! RATES
//! -----
//! USD per 1,000,000 tokens, mirroring `shared/usage.ts` `PRICE_TABLE`
//! (`price_table_v1`, read 2026-09-30) so both sides agree:
//!   - DeepSeek flash family: $0.30 input / $1.20 output / $0.006 cache-hit.
//!   - DeepSeek v4-pro:       $1.32 input / $3.96 output / $0.044 cache-hit.
//!   - Gemini 2.5 flash:      $0.30 input / $2.50 output / $0.03 cache-hit.
//!
//! FORMULA (documented so it can be recomputed):
//!   uncached_input = max(input_tokens - cached_input_tokens, 0)
//!   amount_usd = ( uncached_input * input
//!                + cached_input   * cache_read
//!                + output_tokens  * output
//!                + cache_write    * cache_write ) / 1_000_000
//! rounded to 9 decimals. `prompt_tokens` reported by these providers INCLUDES
//! the cache-hit tokens, so the cached bucket is subtracted from the input
//! bucket instead of being billed twice. Reasoning tokens are billed inside
//! `output_tokens` by these providers (they are part of `completion_tokens`),
//! so no separate reasoning term is added; `cache_write_tokens` is not
//! reported by the core provider parsing today (always 0 here).

use serde_json::{json, Value};

/// Version tag of the fixed price table; every computed cost cites it.
pub const PRICE_TABLE_VERSION: &str = "price_table_v1";

/// The `pricing_ref` every computed cost cites: the table and its version.
pub const PRICE_TABLE_REF: &str = "src/agent/pricing.rs#PRICE_TABLE@price_table_v1";

/// One priced route: USD per 1,000,000 tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

const fn price(input: f64, output: f64, cache_read: f64, cache_write: f64) -> ModelPrice {
    ModelPrice {
        input,
        output,
        cache_read,
        cache_write,
    }
}

/// FIXED deployment price table, keyed by `<provider>/<model>`, USD per
/// 1,000,000 tokens. The provider keys cover the spelling each side uses:
/// `deepseek` (the code-less provider in `config/models.yml`), the
/// `deepseek-official` route the dsh side records, and the `opencode-go`
/// gateway that serves the same DeepSeek flash model. Lookup falls back to a
/// model-only match for a provider alias that is not listed here, so an alias
/// never loses a price the model itself has.
pub const PRICE_TABLE: &[(&str, ModelPrice)] = &[
    ("deepseek/deepseek-flash", price(0.3, 1.2, 0.006, 0.3)),
    ("deepseek/deepseek-v4-flash", price(0.3, 1.2, 0.006, 0.3)),
    ("deepseek/deepseek-v4.1-flash", price(0.3, 1.2, 0.006, 0.3)),
    ("deepseek/deepseek-v4-pro", price(1.32, 3.96, 0.044, 1.32)),
    (
        "deepseek-official/deepseek-flash",
        price(0.3, 1.2, 0.006, 0.3),
    ),
    (
        "deepseek-official/deepseek-v4-flash",
        price(0.3, 1.2, 0.006, 0.3),
    ),
    (
        "deepseek-official/deepseek-v4.1-flash",
        price(0.3, 1.2, 0.006, 0.3),
    ),
    (
        "deepseek-official/deepseek-v4-pro",
        price(1.32, 3.96, 0.044, 1.32),
    ),
    ("opencode-go/deepseek-v4-flash", price(0.3, 1.2, 0.006, 0.3)),
    ("google/gemini-2.5-flash", price(0.3, 2.5, 0.03, 0.3)),
];

/// The USD price of one `<provider>/<model>` route, or `None` when unpriced.
///
/// Exact `provider/model` first, then a model-name match for a provider alias
/// absent from the table (never a wrong-model guess).
pub fn price_of(provider: &str, model: &str) -> Option<ModelPrice> {
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    let key = format!("{}/{}", provider, model);
    if let Some((_, p)) = PRICE_TABLE.iter().find(|(k, _)| *k == key) {
        return Some(*p);
    }
    let suffix = format!("/{}", model);
    PRICE_TABLE
        .iter()
        .find(|(k, _)| k.ends_with(&suffix))
        .map(|(_, p)| *p)
}

/// The cost block for one omniagent provider call, or `cost: null` when the
/// route has no price. Shape matches the dsh side exactly.
pub fn cost_block(
    provider: &str,
    model: &str,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    cache_write_tokens: u64,
) -> Value {
    let Some(p) = price_of(provider, model) else {
        return Value::Null;
    };
    let uncached_input = input_tokens.saturating_sub(cached_input_tokens) as f64;
    let amount = (uncached_input * p.input
        + cached_input_tokens as f64 * p.cache_read
        + output_tokens as f64 * p.output
        + cache_write_tokens as f64 * p.cache_write)
        / 1_000_000.0;
    let rounded = (amount * 1e9).round() / 1e9;
    json!({
        "amount_usd": rounded,
        "is_estimate": true,
        "source": PRICE_TABLE_VERSION,
        "pricing_ref": PRICE_TABLE_REF,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_route_is_priced_with_the_documented_formula() {
        // 1,000,000 prompt tokens of which 500,000 are cache hits (500,000
        // uncached) + 200,000 output on the DeepSeek flash rate:
        // 0.15 + 0.003 + 0.24 = 0.393 USD.
        // See the deployment price table comment above: the cached bucket is
        // subtracted from the input bucket instead of being billed twice.
        let cost = cost_block(
            "deepseek",
            "deepseek-v4-flash",
            1_000_000,
            500_000,
            200_000,
            0,
        );
        assert!(!cost.is_null());
        assert_eq!(cost["is_estimate"], true);
        assert_eq!(cost["source"], PRICE_TABLE_VERSION);
        assert_eq!(cost["pricing_ref"], PRICE_TABLE_REF);
        let amount = cost["amount_usd"].as_f64().expect("amount_usd is a number");
        assert!((amount - 0.393).abs() < 1e-9, "got {}", amount);
    }

    #[test]
    fn cached_tokens_are_not_billed_twice() {
        // prompt_tokens (input) INCLUDES the cache-hit tokens: 1,000,000
        // prompt of which 1,000,000 are cache hits = only the cache rate.
        let cost = cost_block("deepseek", "deepseek-v4-flash", 1_000_000, 1_000_000, 0, 0);
        let amount = cost["amount_usd"].as_f64().unwrap();
        assert!((amount - 0.006).abs() < 1e-9, "got {}", amount);
    }

    #[test]
    fn unknown_route_yields_null_never_zero() {
        assert!(cost_block("acme", "mystery-model", 1_000_000, 0, 1_000_000, 0).is_null());
        assert!(cost_block("", "deepseek-v4-flash", 10, 0, 10, 0).is_null());
        assert!(price_of("acme", "mystery-model").is_none());
    }

    #[test]
    fn provider_alias_falls_back_to_the_model_price() {
        assert_eq!(
            price_of("some-gateway", "deepseek-v4-pro"),
            price_of("deepseek", "deepseek-v4-pro")
        );
        assert_eq!(
            price_of("deepseek-official", "deepseek-v4-flash"),
            price_of("opencode-go", "deepseek-v4-flash")
        );
    }
}
