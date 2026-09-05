//! Log-hygiene guard tests (flood prevention, 2026-09-05).
//!
//! RULE (log hygiene): events that can recur per message / per iteration /
//! per thread (lifecycle anomalies, config discovery/refresh, polling loops)
//! may ONLY log at debug/trace level, or be rate-limited. An info/error log
//! emitted once per repeated event turns a benign condition into a journal
//! flood (incident 2026-09-05: 28,708 ERRORs "Thread N has no cause message,
//! skipping" and ~285,889 INFO lines from MCP config::external discovery in a
//! single 21h window).
//!
//! These tests scan the source and fail loudly if a known per-event log site
//! is ever promoted back to a flooding level, so the floods cannot be
//! reintroduced by accident.

use std::path::PathBuf;

fn read_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e))
}

#[test]
fn mcp_external_config_discovery_logs_debug_not_info() {
    // config::external discovery/refresh runs repeatedly (per server init and
    // per config refresh), so it must never log at INFO or above: an INFO log
    // per discovery flooded the journal with ~285,889 lines in 21h (2026-09-05).
    let src = read_src("mcp/external/config.rs");
    assert!(
        !src.contains("tracing::info!("),
        "mcp/external/config.rs contains tracing::info!() calls: config discovery \
         runs repeatedly and INFO discovery logs flood the journal (2026-09-05: \
         ~285,889 lines in 21h). Use tracing::debug!."
    );
    // The discovery sites must still exist (behavior unchanged), now at debug.
    for needle in [
        "discover_plugin_servers: load_raw",
        "discover: tool",
        "Scanning for MCP plugin configs in:",
        "Resolved command for",
        "Loaded {} MCP server(s) from plugins/tools/ directories",
    ] {
        assert!(
            src.contains(needle),
            "expected discovery site {:?} to still exist in mcp/external/config.rs",
            needle
        );
    }
    // Sanity: discovery logging was migrated to debug (not silently deleted).
    assert!(
        src.matches("tracing::debug!(").count() >= 5,
        "expected the discovery log sites to be present at tracing::debug! level"
    );
}

#[test]
fn no_cause_thread_skip_logs_debug_not_error() {
    // A pending thread without a seq-0 cause message is a per-thread lifecycle
    // event (handled by writing a user-visible error message and finalizing the
    // thread as failed). It must log at DEBUG, never ERROR per occurrence.
    let src = read_src("agent/mod.rs");
    let line = src
        .lines()
        .find(|l| l.contains("has no cause message, skipping"))
        .unwrap_or_else(|| panic!("no source line contains 'has no cause message, skipping'"));
    assert!(
        line.contains("debug!") && !line.contains("error!"),
        "the no-cause thread skip must log at DEBUG level (per-thread ERROR logging \
         flooded the journal with 28,708 lines in 21h, 2026-09-05). Found: {}",
        line.trim()
    );
}
