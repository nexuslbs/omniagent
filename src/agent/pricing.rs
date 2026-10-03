//! Service-side price table for the omniagent's own provider calls, loaded from
//! an EXTERNAL, updatable definition file: `{data_dir}/config/model_prices.yml`.
//!
//! WHY (operator, telegram 2026-10-02)
//! -----------------------------------
//! The prices used to be a COMPILED CONSTANT (`PRICE_TABLE`): changing a rate
//! needed a code change and a release. The operator rejected that ("It must not
//! be hardcoded! A version release is out of question. Maybe allow the code read
//! from an external file, with the file updatable."). This module now reads the
//! rates from `config/model_prices.yml` - exactly like `config/models.yml`
//! (`crate::models_yaml`) - so an operator updates a price by editing one file.
//!
//! ABSENT / EMPTY / INVALID FILE -> COST 0, NEVER AN ERROR (operator contract)
//! --------------------------------------------------------------------------
//! A missing, empty, unparseable or malformed `model_prices.yml` MUST NOT fail
//! startup, panic, or produce a 5xx: every computed cost is the plain number
//! `0` (usable directly by the `threads.cost` sum). A warning is logged once per
//! distinct file content. When the file IS present and valid, a route it does
//! not contain is UNPRICED -> `cost: null` (an unknown price is never
//! fabricated as 0).
//!
//! SHARED SCHEMA (core + dsh)
//! --------------------------
//! `{OMNI_DIR}/config/model_prices.yml` is the SINGLE source of truth for this
//! module AND the dsh/workstation cost accounting
//! (`workstation-plugins/shared/usage.ts`). ONE provider -> model hierarchy and
//! ONE set of field names, USD per 1,000,000 tokens:
//!
//! ```yaml
//! version: price_table_v1
//! providers:
//!   deepseek:
//!     deepseek-v4-flash:
//!       input: 0.30          # uncached input tokens
//!       cached_input: 0.006  # cache-READ (cache-hit) tokens
//!       output: 1.20
//!       cache_write: 0.30
//!       reasoning: 1.20      # OPTIONAL; absent = billed inside `output`
//! aliases:
//!   deepseek-official: deepseek
//!   opencode-go: deepseek
//! ```
//!
//! `cached_input` is the CANONICAL key (matching the `cached_input_tokens`
//! field both sides already emit). `cache_read` was the dsh-side INTERNAL name
//! and is never a key in `model_prices.yml`. Unknown keys are IGNORED at every
//! level (forward compatible: the dependent peak/off-peak task adds a per-route
//! `off_peak_factor` plus a file-level UTC off-peak window to this same file
//! with no schema break).
//!
//! RELOAD SEMANTICS
//! ----------------
//! The file is re-read on every cost computation; the PARSED table is cached by
//! CONTENT HASH (canonical path + FNV-1a of the bytes), so editing a rate takes
//! effect on the next call with no recompile and no restart, while an unchanged
//! file costs one small read + one hash. `source` / `pricing_ref` name the
//! exact definition that priced the call (`model_prices.yml@<version>#<hash>`).
//!
//! PROVIDER ALIASES
//! ----------------
//! Resolution order for `<provider>/<model>`:
//!   1. exact `providers.<provider>.<model>`;
//!   2. the provider's `aliases:` entry (`deepseek-official` -> `deepseek`,
//!      `opencode-go` -> `deepseek`) - one hop, never a chain;
//!   3. a model-only match across all providers (last resort for an unlisted
//!      gateway name; never a wrong-model guess).
//!
//! FORMULA (unchanged)
//! -------------------
//!   uncached_input = max(input_tokens - cached_input_tokens, 0)
//!   amount_usd = ( uncached_input * input
//!                + cached_input   * cached_input
//!                + output_tokens  * output
//!                + cache_write    * cache_write ) / 1_000_000
//! rounded to 9 decimals. `prompt_tokens` reported by these providers INCLUDES
//! the cache-hit tokens, so the cached bucket is subtracted from the input
//! bucket instead of being billed twice. Reasoning is billed inside
//! `output_tokens` by these providers, so no separate reasoning term is added
//! (the optional `reasoning` rate is carried for the dsh side and future
//! callers). `cache_write_tokens` is not reported by the core provider parsing
//! today (always 0 here).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Name of the external pricing definition inside `{data_dir}/config/`.
pub const PRICES_FILE: &str = "model_prices.yml";

/// The `source` of every cost block computed here: the external definition
/// file (never a compiled-in module path).
pub const PRICING_SOURCE: &str = PRICES_FILE;

/// Global data dir - set once at startup (main.rs) so the pricing file is
/// reachable from every cost computation. First call wins (same contract as
/// `channels_yaml::set_data_dir`).
static DATA_DIR: OnceLock<String> = OnceLock::new();

/// Set the global data dir (idempotent; first call wins).
pub fn set_data_dir(dir: &str) {
    let _ = DATA_DIR.set(dir.to_string());
}

/// The globally configured data dir (if set).
pub fn data_dir() -> Option<&'static str> {
    DATA_DIR.get().map(|s| s.as_str())
}

/// Canonical path to the pricing file: `{data_dir}/config/model_prices.yml`.
pub fn prices_path(data_dir: &str) -> PathBuf {
    crate::config_path::config_path(data_dir, PRICES_FILE)
}

// ---------------------------------------------------------------------------
// File structs
// ---------------------------------------------------------------------------

/// Top-level `model_prices.yml` content.
///
/// Every field is optional/defaulted and unknown keys are ignored, so a file
/// written for a NEWER binary still loads here (forward compatible).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PricesFile {
    /// Informational version cited in `pricing_ref` (e.g. `price_table_v1`).
    pub version: Option<String>,
    /// provider -> model -> rates.
    pub providers: BTreeMap<String, BTreeMap<String, PriceEntry>>,
    /// alias provider -> canonical provider (`deepseek-official: deepseek`).
    pub aliases: BTreeMap<String, String>,
}

/// The canonical per-model rate set: USD per 1,000,000 tokens.
///
/// A rate that is absent from the file is 0 (the file never errors), and
/// `reasoning` is the only OPTIONAL rate - absent means reasoning is billed
/// inside `output`, exactly as the core does today.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PriceEntry {
    /// Uncached input tokens.
    pub input: f64,
    /// Cache-READ (cache-hit) tokens. Canonical name; `cache_read` is not
    /// accepted (it was the dsh-side internal spelling).
    pub cached_input: f64,
    /// Output tokens.
    pub output: f64,
    /// Cache-write tokens.
    pub cache_write: f64,
    /// Optional reasoning tokens (absent = billed inside `output`).
    pub reasoning: Option<f64>,
}

/// One priced route as used by the cost formula.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub reasoning: Option<f64>,
}

impl From<PriceEntry> for ModelPrice {
    fn from(e: PriceEntry) -> Self {
        ModelPrice {
            input: e.input,
            cached_input: e.cached_input,
            output: e.output,
            cache_write: e.cache_write,
            reasoning: e.reasoning,
        }
    }
}

/// Parse the pricing document. `Err` carries a human-readable reason (the
/// caller turns it into a warning + zero costs, never a failure).
pub fn parse_prices(content: &str) -> Result<PricesFile, String> {
    let file: PricesFile = serde_yaml::from_str(content).map_err(|e| e.to_string())?;
    // Light semantic validation: names must not be empty. Rates are tolerant
    // (a partial entry bills 0 for the absent rates) - this file must NEVER
    // brick cost accounting.
    for (provider, models) in &file.providers {
        if provider.trim().is_empty() {
            return Err("provider name must not be empty".to_string());
        }
        for model in models.keys() {
            if model.trim().is_empty() {
                return Err(format!("provider '{provider}' has an empty model name"));
            }
        }
    }
    for (alias, canonical) in &file.aliases {
        if alias.trim().is_empty() || canonical.trim().is_empty() {
            return Err("alias names must not be empty".to_string());
        }
    }
    Ok(file)
}

// ---------------------------------------------------------------------------
// Loaded table
// ---------------------------------------------------------------------------

/// Why a table has no usable rates: the file is absent, empty or malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricesStatus {
    Loaded,
    Missing,
    Empty,
    Invalid,
}

/// One parsed definition file (or the status that says why there are no rates).
#[derive(Debug, Clone)]
pub struct PriceTable {
    status: PricesStatus,
    version: String,
    hash: String,
    file: PricesFile,
}

impl PriceTable {
    fn new(status: PricesStatus, version: String, hash: String, file: PricesFile) -> Self {
        PriceTable {
            status,
            version,
            hash,
            file,
        }
    }

    /// No file at all -> cost 0.
    pub fn missing() -> Self {
        PriceTable::new(
            PricesStatus::Missing,
            String::new(),
            String::new(),
            PricesFile::default(),
        )
    }

    /// Present but empty -> cost 0.
    pub fn empty(hash: String) -> Self {
        PriceTable::new(
            PricesStatus::Empty,
            String::new(),
            hash,
            PricesFile::default(),
        )
    }

    /// Present but malformed -> cost 0.
    pub fn invalid(hash: String) -> Self {
        PriceTable::new(
            PricesStatus::Invalid,
            String::new(),
            hash,
            PricesFile::default(),
        )
    }

    /// A valid definition file.
    pub fn loaded(file: PricesFile, version: String, hash: String) -> Self {
        PriceTable::new(PricesStatus::Loaded, version, hash, file)
    }

    /// True when the file exists and parsed: only then can a route be priced.
    pub fn is_loaded(&self) -> bool {
        self.status == PricesStatus::Loaded
    }

    /// Why there are no rates (or `Loaded`).
    pub fn status(&self) -> PricesStatus {
        self.status
    }

    /// File version (`version:` key) - empty when absent.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// `config/model_prices.yml@<version>#<content-hash>` for a loaded file, or
    /// `config/model_prices.yml#missing|#empty|#invalid` when there are no rates.
    pub fn pricing_ref(&self) -> String {
        match self.status {
            PricesStatus::Loaded => {
                let version = if self.version.is_empty() {
                    "unversioned"
                } else {
                    self.version.as_str()
                };
                format!("config/{}@{}#{}", PRICES_FILE, version, self.hash)
            }
            PricesStatus::Missing => format!("config/{}#missing", PRICES_FILE),
            PricesStatus::Empty => format!("config/{}#empty", PRICES_FILE),
            PricesStatus::Invalid => format!("config/{}#invalid", PRICES_FILE),
        }
    }

    /// The provider -> model -> rates map (empty unless loaded).
    pub fn providers(&self) -> &BTreeMap<String, BTreeMap<String, PriceEntry>> {
        &self.file.providers
    }

    /// The USD price of one `<provider>/<model>` route, or `None` when unpriced
    /// (file absent/empty/invalid, unknown provider/model, or empty names).
    pub fn price_of(&self, provider: &str, model: &str) -> Option<ModelPrice> {
        if !self.is_loaded() || provider.trim().is_empty() || model.trim().is_empty() {
            return None;
        }
        // 1. exact provider/model.
        if let Some(entry) = self
            .file
            .providers
            .get(provider)
            .and_then(|models| models.get(model))
        {
            return Some((*entry).into());
        }
        // 2. explicit provider alias (one hop).
        if let Some(canonical) = self.file.aliases.get(provider) {
            if let Some(entry) = self
                .file
                .providers
                .get(canonical)
                .and_then(|models| models.get(model))
            {
                return Some((*entry).into());
            }
        }
        // 3. last resort: the model name exists under some provider (an
        //    unlisted gateway spelling). Never a wrong-model guess.
        self.file
            .providers
            .values()
            .find_map(|models| models.get(model).copied())
            .map(ModelPrice::from)
    }

    /// The `cost` block for a caller that cannot be priced because there is no
    /// usable definition file: a plain NUMERIC 0 (operator contract - an absent
    /// or invalid file must never error and never produce a null cost).
    pub fn zero_cost_block(&self) -> Value {
        json!({
            "amount_usd": 0.0,
            "is_estimate": true,
            "source": PRICING_SOURCE,
            "pricing_ref": self.pricing_ref(),
        })
    }

    /// The priced `cost` block for one call, or `null` when the (valid) file
    /// does not price the route.
    fn priced_cost_block(
        &self,
        price: ModelPrice,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        cache_write_tokens: u64,
    ) -> Value {
        let uncached_input = input_tokens.saturating_sub(cached_input_tokens) as f64;
        let amount = (uncached_input * price.input
            + cached_input_tokens as f64 * price.cached_input
            + output_tokens as f64 * price.output
            + cache_write_tokens as f64 * price.cache_write)
            / 1_000_000.0;
        let rounded = (amount * 1e9).round() / 1e9;
        json!({
            "amount_usd": rounded,
            "is_estimate": true,
            "source": PRICING_SOURCE,
            "pricing_ref": self.pricing_ref(),
        })
    }
}

// ---------------------------------------------------------------------------
// Loading + cache
// ---------------------------------------------------------------------------

/// Content hash (FNV-1a, 64-bit hex) used both as the cache key and as the
/// `pricing_ref` digest: it changes exactly when a rate in the file changes.
fn content_hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

struct CachedTable {
    path: PathBuf,
    hash: String,
    table: Arc<PriceTable>,
}

/// The parsed table of the last distinct file content (one file in practice).
static CACHE: Mutex<Option<CachedTable>> = Mutex::new(None);

/// Read, parse and cache the definition file at `path`.
///
/// The file is read on EVERY call (cheap: one small file) and the parsed table
/// is reused while the content hash is unchanged, so editing a rate is picked
/// up on the next cost computation with no rebuild and no restart.
pub fn load_table(path: &Path) -> Arc<PriceTable> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    "{}: cannot read {}: {} (LLM costs are 0)",
                    PRICES_FILE,
                    path.display(),
                    e
                );
            }
            return Arc::new(PriceTable::missing());
        }
    };
    let hash = content_hash(content.as_bytes());
    if let Ok(guard) = CACHE.lock() {
        if let Some(cached) = guard.as_ref() {
            if cached.path == path && cached.hash == hash {
                return cached.table.clone();
            }
        }
    }
    let table = Arc::new(build_table(&content, hash, path));
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some(CachedTable {
            path: path.to_path_buf(),
            hash: table.hash.clone(),
            table: table.clone(),
        });
    }
    table
}

/// Load the table for a data dir: `{dir}/config/model_prices.yml`.
pub fn load_table_for(data_dir: &str) -> Arc<PriceTable> {
    load_table(&prices_path(data_dir))
}

fn build_table(content: &str, hash: String, path: &Path) -> PriceTable {
    if content.trim().is_empty() {
        tracing::warn!(
            "{}: {} is empty - LLM costs are 0 (add a providers: map to price calls)",
            PRICES_FILE,
            path.display()
        );
        return PriceTable::empty(hash);
    }
    match parse_prices(content) {
        Ok(file) => {
            // A document with only comments (`Ok(default)`), an explicit
            // `providers: {}` or providers without models defines NO rate at
            // all: treat it exactly like an empty file (numeric 0 costs), not
            // like a valid table where every route is "unpriced".
            if file.providers.values().all(|models| models.is_empty()) {
                tracing::warn!(
                    "{}: {} defines no model rates - LLM costs are 0",
                    PRICES_FILE,
                    path.display()
                );
                return PriceTable::empty(hash);
            }
            tracing::info!(
                "{}: loaded {} provider(s) from {} (version {})",
                PRICES_FILE,
                file.providers.len(),
                path.display(),
                file.version.as_deref().unwrap_or("unversioned")
            );
            let version = file.version.clone().unwrap_or_default();
            PriceTable::loaded(file, version, hash)
        }
        Err(e) => {
            tracing::warn!(
                "{}: ignoring invalid {}: {} (LLM costs are 0)",
                PRICES_FILE,
                path.display(),
                e
            );
            PriceTable::invalid(hash)
        }
    }
}

// ---------------------------------------------------------------------------
// Cost blocks
// ---------------------------------------------------------------------------

/// The cost block for one omniagent provider call, using the GLOBAL data dir
/// (set once at startup by `main`).
///
/// * a usable `config/model_prices.yml` prices the route (or `null` when the
///   valid file does not contain it - never a fabricated price);
/// * a missing/empty/invalid file (or an unset data dir) -> numeric 0, with
///   the reason recorded in `pricing_ref`, never an error.
pub fn cost_block(
    provider: &str,
    model: &str,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    cache_write_tokens: u64,
) -> Value {
    match data_dir() {
        Some(dir) => cost_block_in(
            dir,
            provider,
            model,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            cache_write_tokens,
        ),
        None => json!({
            "amount_usd": 0.0,
            "is_estimate": true,
            "source": PRICING_SOURCE,
            "pricing_ref": format!("config/{}#no-data-dir", PRICES_FILE),
        }),
    }
}

/// [`cost_block`] against an explicit data dir (tests, and callers that hold
/// the dir themselves).
pub fn cost_block_in(
    data_dir: &str,
    provider: &str,
    model: &str,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    cache_write_tokens: u64,
) -> Value {
    let table = load_table_for(data_dir);
    if !table.is_loaded() {
        return table.zero_cost_block();
    }
    match table.price_of(provider, model) {
        Some(price) => table.priced_cost_block(
            price,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            cache_write_tokens,
        ),
        None => Value::Null,
    }
}

/// [`price_of`] against an explicit data dir.
pub fn price_of_in(data_dir: &str, provider: &str, model: &str) -> Option<ModelPrice> {
    load_table_for(data_dir).price_of(provider, model)
}

/// The USD price of one `<provider>/<model>` route from the GLOBAL data dir, or
/// `None` when the route (or the whole file) is unpriced.
pub fn price_of(provider: &str, model: &str) -> Option<ModelPrice> {
    let dir = data_dir()?;
    price_of_in(dir, provider, model)
}

/// Test-only helper: one process-wide temp OMNI_DIR seeded with the shipped
/// `config/model_prices.yml`, for tests that exercise the GLOBAL data dir.
#[cfg(test)]
pub mod test_support {
    use super::*;

    /// Path of the seed shipped in the repository (also the test fixture).
    pub const SEED_YAML: &str = include_str!("../../config/model_prices.yml");

    /// Create (once per process) `<tmp>/omniagent-pricing-tests-<pid>/config/
    /// model_prices.yml` from [`SEED_YAML`] and point the global data dir at it.
    pub fn seeded_data_dir() -> &'static str {
        static DIR: OnceLock<String> = OnceLock::new();
        DIR.get_or_init(|| {
            let dir = std::env::temp_dir()
                .join(format!("omniagent-pricing-tests-{}", std::process::id()));
            let config_dir = dir.join("config");
            std::fs::create_dir_all(&config_dir).expect("create test omni dir");
            std::fs::write(config_dir.join(PRICES_FILE), SEED_YAML)
                .expect("write test model_prices.yml");
            let dir = dir.to_string_lossy().to_string();
            set_data_dir(&dir);
            dir
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seed shipped in the repo (`omniagent/config/model_prices.yml`).
    const SEED: &str = test_support::SEED_YAML;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("omniagent-pricing-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).expect("create temp config dir");
        dir
    }

    fn write_prices(dir: &Path, content: &str) -> PathBuf {
        let path = prices_path(dir.to_str().unwrap());
        std::fs::write(&path, content).expect("write model_prices.yml");
        path
    }

    #[test]
    fn shipped_seed_is_valid_and_prices_the_documented_routes() {
        let file = parse_prices(SEED).expect("shipped seed parses");
        assert_eq!(file.version.as_deref(), Some("price_table_v1"));
        let table = PriceTable::loaded(
            file,
            "price_table_v1".to_string(),
            content_hash(SEED.as_bytes()),
        );
        assert!(table.is_loaded());
        let flash = table
            .price_of("deepseek", "deepseek-v4-flash")
            .expect("flash");
        assert_eq!(flash.input, 0.3);
        assert_eq!(flash.cached_input, 0.006);
        assert_eq!(flash.output, 1.2);
        assert_eq!(flash.cache_write, 0.3);
        let pro = table.price_of("deepseek", "deepseek-v4-pro").expect("pro");
        assert_eq!(pro.input, 1.32);
        assert_eq!(pro.cached_input, 0.044);
        assert_eq!(pro.output, 3.96);
        let gemini = table
            .price_of("google", "gemini-2.5-flash")
            .expect("gemini");
        assert_eq!(gemini.output, 2.5);
        assert_eq!(gemini.cached_input, 0.03);
    }

    #[test]
    fn seed_uses_canonical_keys_only() {
        assert!(
            SEED.contains("cached_input:"),
            "cached_input is the canonical key"
        );
        for line in SEED.lines() {
            let code = line.split('#').next().unwrap_or("");
            assert!(
                !code.contains("cache_read"),
                "cache_read is not a canonical key in model_prices.yml: {line}"
            );
        }
    }

    #[test]
    fn valid_file_prices_a_known_route_with_provenance() {
        let dir = tmpdir("valid");
        write_prices(&dir, SEED);
        let cost = cost_block_in(
            dir.to_str().unwrap(),
            "deepseek",
            "deepseek-v4-flash",
            1_000_000,
            500_000,
            200_000,
            0,
        );
        assert!(!cost.is_null(), "known route must carry a cost block");
        assert_eq!(cost["is_estimate"], true);
        assert_eq!(cost["source"], PRICING_SOURCE);
        // 500,000 uncached * 0.30 + 500,000 cached * 0.006 + 200,000 * 1.20
        // = 0.15 + 0.003 + 0.24 = 0.393 USD.
        let amount = cost["amount_usd"].as_f64().expect("amount_usd is numeric");
        assert!((amount - 0.393).abs() < 1e-9, "got {amount}");
        let pricing_ref = cost["pricing_ref"].as_str().expect("pricing_ref");
        assert!(
            pricing_ref.starts_with("config/model_prices.yml@price_table_v1#"),
            "provenance must name the external file: {pricing_ref}"
        );
    }

    #[test]
    fn cached_input_tokens_are_not_billed_twice() {
        let dir = tmpdir("cache");
        write_prices(&dir, SEED);
        // prompt_tokens INCLUDES the cache-hit tokens: 1,000,000 prompt of
        // which 1,000,000 are cache hits = only the cache rate.
        let cost = cost_block_in(
            dir.to_str().unwrap(),
            "deepseek",
            "deepseek-v4-flash",
            1_000_000,
            1_000_000,
            0,
            0,
        );
        let amount = cost["amount_usd"].as_f64().unwrap();
        assert!((amount - 0.006).abs() < 1e-9, "got {amount}");
    }

    #[test]
    fn missing_file_yields_numeric_zero_without_error() {
        let dir = tmpdir("missing");
        let cost = cost_block_in(
            dir.to_str().unwrap(),
            "deepseek",
            "deepseek-v4-flash",
            1_000_000,
            0,
            1_000_000,
            0,
        );
        assert!(!cost.is_null(), "missing file -> numeric 0, never null");
        assert_eq!(cost["amount_usd"].as_f64().unwrap(), 0.0);
        assert_eq!(cost["source"], PRICING_SOURCE);
        assert_eq!(cost["pricing_ref"], "config/model_prices.yml#missing");
    }

    #[test]
    fn empty_file_yields_numeric_zero() {
        let dir = tmpdir("empty");
        write_prices(&dir, "\n# nothing here\n");
        let cost = cost_block_in(
            dir.to_str().unwrap(),
            "deepseek",
            "deepseek-v4-flash",
            10,
            0,
            10,
            0,
        );
        assert_eq!(cost["amount_usd"].as_f64().unwrap(), 0.0);
        assert_eq!(cost["pricing_ref"], "config/model_prices.yml#empty");
    }

    #[test]
    fn invalid_file_yields_numeric_zero_without_panic() {
        let dir = tmpdir("invalid");
        // `providers:` must be a mapping of mappings; this body is malformed.
        write_prices(&dir, "providers: [not a map\n  :::\n  - oops\n");
        let cost = cost_block_in(
            dir.to_str().unwrap(),
            "deepseek",
            "deepseek-v4-flash",
            10,
            0,
            10,
            0,
        );
        assert_eq!(cost["amount_usd"].as_f64().unwrap(), 0.0);
        assert_eq!(cost["pricing_ref"], "config/model_prices.yml#invalid");
    }

    #[test]
    fn invalid_body_with_unsupported_rates_is_never_an_error() {
        let dir = tmpdir("malformed-rates");
        // A model entry that is a scalar instead of a rates mapping: malformed
        // -> zero cost, no panic.
        write_prices(
            &dir,
            "providers:\n  deepseek:\n    deepseek-v4-flash: 0.30\n",
        );
        let cost = cost_block_in(
            dir.to_str().unwrap(),
            "deepseek",
            "deepseek-v4-flash",
            10,
            0,
            10,
            0,
        );
        assert_eq!(cost["amount_usd"].as_f64().unwrap(), 0.0);
        assert_eq!(cost["pricing_ref"], "config/model_prices.yml#invalid");
    }

    #[test]
    fn editing_a_rate_changes_the_cost_with_no_code_change() {
        let dir = tmpdir("edit");
        write_prices(
            &dir,
            "providers:\n  deepseek:\n    m1:\n      input: 1.0\n      output: 1.0\n",
        );
        let before = cost_block_in(dir.to_str().unwrap(), "deepseek", "m1", 1_000_000, 0, 0, 0)
            ["amount_usd"]
            .as_f64()
            .unwrap();
        assert!((before - 1.0).abs() < 1e-9, "got {before}");
        // Same route, different rate, no rebuild / no restart.
        write_prices(
            &dir,
            "providers:\n  deepseek:\n    m1:\n      input: 2.5\n      output: 1.0\n",
        );
        let after = cost_block_in(dir.to_str().unwrap(), "deepseek", "m1", 1_000_000, 0, 0, 0)
            ["amount_usd"]
            .as_f64()
            .unwrap();
        assert!(
            (after - 2.5).abs() < 1e-9,
            "an edited rate must be picked up with no rebuild: got {after}"
        );
    }

    #[test]
    fn unknown_keys_are_tolerated_at_every_level() {
        let dir = tmpdir("unknown-keys");
        write_prices(
            &dir,
            "future_top_level: yes\nversion: v2\nproviders:\n  deepseek:\n    m1:\n      input: 1.0\n      output: 0.0\n      off_peak_factor: 0.5\n      off_peak_window: '22:00-02:00'\n      future_flag: true\naliases:\n  some-gateway: deepseek\n",
        );
        let table = load_table_for(dir.to_str().unwrap());
        assert!(table.is_loaded(), "forward-compatible file must load");
        let cost = cost_block_in(dir.to_str().unwrap(), "deepseek", "m1", 1_000_000, 0, 0, 0);
        assert!((cost["amount_usd"].as_f64().unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn provider_aliases_and_the_model_fallback_keep_the_same_price() {
        let dir = tmpdir("aliases");
        write_prices(&dir, SEED);
        let base = cost_block_in(
            dir.to_str().unwrap(),
            "deepseek",
            "deepseek-v4-pro",
            1_000_000,
            0,
            0,
            0,
        );
        assert!(!base.is_null());
        for alias in ["deepseek-official", "opencode-go", "some-unlisted-gateway"] {
            let cost = cost_block_in(
                dir.to_str().unwrap(),
                alias,
                "deepseek-v4-pro",
                1_000_000,
                0,
                0,
                0,
            );
            assert_eq!(cost["amount_usd"], base["amount_usd"], "alias {alias}");
        }
    }

    #[test]
    fn unpriced_route_in_a_valid_file_stays_null_never_zero() {
        let dir = tmpdir("unpriced");
        write_prices(&dir, SEED);
        assert!(cost_block_in(
            dir.to_str().unwrap(),
            "deepseek",
            "deepseek-v4.1",
            10,
            0,
            10,
            0
        )
        .is_null());
        assert!(price_of_in(dir.to_str().unwrap(), "acme", "mystery-model").is_none());
    }

    #[test]
    fn seed_agrees_with_the_dsh_side_price_table_fixture() {
        // Cross-side schema parity (fixture level): the SAME file + canonical
        // keys must reproduce the rates the dsh/workstation table documents
        // (`workstation-plugins/shared/usage.ts` PRICE_TABLE), including the
        // alias provider spellings that side records.
        let dir = tmpdir("cross-side");
        write_prices(&dir, SEED);
        let dir = dir.to_str().unwrap();

        let flash = price_of_in(dir, "deepseek-official", "deepseek-v4-flash").expect("flash");
        assert_eq!(
            (flash.input, flash.output, flash.cached_input),
            (0.3, 1.2, 0.006)
        );
        let pro = price_of_in(dir, "deepseek-official", "deepseek-v4-pro").expect("pro");
        assert_eq!(
            (pro.input, pro.output, pro.cached_input),
            (1.32, 3.96, 0.044)
        );
        let gemini = price_of_in(dir, "google", "gemini-2.5-flash").expect("gemini");
        assert_eq!(
            (gemini.input, gemini.output, gemini.cached_input),
            (0.3, 2.5, 0.03)
        );
        // Unlisted gateway spelling: the dsh side uses `opencode-go`.
        assert_eq!(
            price_of_in(dir, "opencode-go", "deepseek-v4-flash").map(|p| p.input),
            Some(0.3)
        );
    }

    #[test]
    fn global_cost_block_reads_the_configured_data_dir() {
        let _ = test_support::seeded_data_dir();
        let cost = cost_block(
            "deepseek",
            "deepseek-v4-flash",
            1_000_000,
            500_000,
            200_000,
            0,
        );
        assert!(!cost.is_null());
        assert_eq!(cost["source"], PRICING_SOURCE);
        let amount = cost["amount_usd"].as_f64().unwrap();
        assert!((amount - 0.393).abs() < 1e-9, "got {amount}");
    }
}
