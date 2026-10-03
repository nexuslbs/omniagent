//! Process-wide operator settings for components that run OUTSIDE the
//! `AgentConfig` snapshot.
//!
//! Most operator knobs are read once by `AgentConfig::from_env` /
//! `AgentConfig::from_settings_yaml` and travel with the agent context. A
//! second class of knobs is consumed by code that has no access to that
//! snapshot: the read-only DB guard, the LLM HTTP transport, the board
//! default, the plugin/compose CLI probe and the spawned-child `PATH`.
//!
//! Those readers go through this module: the settings file
//! (`{OMNI_DIR}/config/settings.yml`) is loaded ONCE at startup into a cached
//! snapshot ([`init`]), and every lookup names its documented code default, so
//! the effective configuration is auditable in one place ([`TRACKED_DEFAULTS`]
//! + [`log_code_defaults`]).
//!
//! Defect class A5 (no unrequested narrowing): a value read here is used
//! VERBATIM. An absent or unparseable value falls back to the documented code
//! default; a present value is never clamped to a narrower one.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// The `OMNI_DIR` bootstrap variable: the omni data directory that contains
/// `config/settings.yml`. Returns `None` when unset or empty.
pub fn omni_dir() -> Option<String> {
    std::env::var("OMNI_DIR")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

static SNAPSHOT: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn snapshot() -> &'static RwLock<HashMap<String, String>> {
    SNAPSHOT.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Load `{OMNI_DIR}/config/settings.yml` into the process-wide snapshot.
/// Called once at startup (and again whenever the settings file is reloaded).
pub fn init() {
    let map = match omni_dir() {
        Some(dir) => crate::server::settings::load_settings_file(&dir),
        None => HashMap::new(),
    };
    set(map);
}

/// Replace the snapshot with an explicit map (settings reload / tests).
pub fn set(map: HashMap<String, String>) {
    if let Ok(mut guard) = snapshot().write() {
        *guard = map;
    }
}

/// True when the operator's settings file declared `key`.
pub fn is_configured(key: &str) -> bool {
    snapshot()
        .read()
        .map(|m| m.contains_key(key))
        .unwrap_or(false)
}

/// Raw configured value for `key`, when present and non-empty.
pub fn raw(key: &str) -> Option<String> {
    snapshot()
        .read()
        .ok()
        .and_then(|m| m.get(key).cloned())
        .filter(|v| !v.trim().is_empty())
}

/// Configured string, or the documented code default.
pub fn get_str(key: &str, default: &str) -> String {
    raw(key).unwrap_or_else(|| default.to_string())
}

/// Configured u64, or the documented code default (unparseable = default).
pub fn get_u64(key: &str, default: u64) -> u64 {
    raw(key)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Configured usize, or the documented code default.
pub fn get_usize(key: &str, default: usize) -> usize {
    raw(key)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Configured i64, or the documented code default.
pub fn get_i64(key: &str, default: i64) -> i64 {
    raw(key)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Configured u32, or the documented code default.
pub fn get_u32(key: &str, default: u32) -> u32 {
    raw(key)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Configured bool (`true`/`1`/`on`/`yes`, case-insensitive), or the default.
pub fn get_bool(key: &str, default: bool) -> bool {
    match raw(key).map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) => matches!(v.as_str(), "true" | "1" | "on" | "yes"),
        None => default,
    }
}

/// Every setting this module can fall back to a code default for, with that
/// default rendered as a string. Feeding this list to [`log_code_defaults`]
/// makes the "invisible" default set visible at boot (SRE "know your effective
/// config") without changing any value.
pub const TRACKED_DEFAULTS: &[(&str, &str)] = &[
    // Read-only DB guard (T2.1 / HV-B6 + HV-D1).
    ("db_readonly_max_rows", "1000"),
    ("db_readonly_timeout_ms", "8000"),
    ("db_readonly_slow_query_ms", "2000"),
    // LLM HTTP transport (T6.3 / HV-B4).
    ("llm_total_timeout_secs", "300"),
    ("llm_connect_timeout_secs", "30"),
    ("llm_pool_idle_timeout_secs", "90"),
    ("llm_transport_retry_attempts", "3"),
    ("llm_transport_retry_base_delay_ms", "500"),
    ("llm_max_concurrent", "5"),
    // Context / tool-output shaping (T3.1 / HV-B7, HV-B10).
    ("max_inline_chars", "50000"),
    ("max_tool_output_chars", "50000"),
    ("prune_head_chars", "12000"),
    ("prune_tail_chars", "8000"),
    ("prune_min_chars", "20000"),
    ("profile_prompt_budget", "15000"),
    // Recovery / orphan sweep policy (T3.1 / HV-B11).
    ("orphan_pending_secs", "120"),
    ("orphan_max_requeues", "3"),
    ("orphan_sweep_interval_secs", "30"),
    ("db_recovery_max_retries", "60"),
    ("event_default_timeout_s", "900"),
    // Deployment (T5.2 / T5.3 / T4.2).
    ("default_board", "main"),
    ("compose_cli", "docker"),
    ("child_extra_path", ""),
    (
        "fail_thread_default_reason",
        "The thread was ended as FAILED by the fail-thread tool.",
    ),
];

/// Keys of [`TRACKED_DEFAULTS`] that the operator has NOT configured (the
/// values currently running on a code default).
pub fn code_defaults_in_use() -> Vec<(&'static str, &'static str)> {
    TRACKED_DEFAULTS
        .iter()
        .copied()
        .filter(|(key, _)| !is_configured(key))
        .collect()
}

/// Log (INFO, once at startup) the tracked settings that are running on a code
/// default, so the effective configuration is discoverable without reading the
/// source. Never changes a value.
pub fn log_code_defaults() {
    let missing = code_defaults_in_use();
    if missing.is_empty() {
        tracing::info!(
            "[settings] all {} tracked settings are operator-configured",
            TRACKED_DEFAULTS.len()
        );
    } else {
        let rendered: Vec<String> = missing.iter().map(|(k, v)| format!("{k}={v}")).collect();
        tracing::info!(
            "[settings] {} of {} tracked settings run on a code default (not in settings.yml): {}",
            missing.len(),
            TRACKED_DEFAULTS.len(),
            rendered.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configured value is used verbatim; an absent one falls back to the
    /// documented default (never a narrower one).
    #[test]
    fn configured_values_win_and_defaults_fill_the_gaps() {
        let mut map = HashMap::new();
        map.insert("db_readonly_max_rows".to_string(), "5000".to_string());
        map.insert("default_board".to_string(), "research".to_string());
        set(map);

        assert_eq!(get_usize("db_readonly_max_rows", 1000), 5000);
        assert_eq!(get_str("default_board", "main"), "research");
        assert_eq!(get_usize("db_readonly_timeout_ms", 8000), 8000);
        assert_eq!(get_str("compose_cli", "docker"), "docker");
        assert!(is_configured("default_board"));
        assert!(!is_configured("compose_cli"));
    }

    /// An unparseable numeric setting falls back to the documented default
    /// instead of silently becoming 0.
    #[test]
    fn unparseable_value_falls_back_to_default() {
        let mut map = HashMap::new();
        map.insert("db_readonly_timeout_ms".to_string(), "abc".to_string());
        set(map);
        assert_eq!(get_i64("db_readonly_timeout_ms", 8000), 8000);
    }

    /// The tracked-default list is the audit's documented default set and
    /// `code_defaults_in_use` reports exactly the non-configured keys.
    #[test]
    fn tracked_defaults_and_missing_report() {
        let mut map = HashMap::new();
        map.insert("compose_cli".to_string(), "docker-compose".to_string());
        set(map);
        let missing = code_defaults_in_use();
        assert!(missing.iter().all(|(k, _)| *k != "compose_cli"));
        assert!(missing.iter().any(|(k, _)| *k == "default_board"));
        assert!(TRACKED_DEFAULTS
            .iter()
            .any(|(k, _)| *k == "orphan_max_requeues"));
    }
}
