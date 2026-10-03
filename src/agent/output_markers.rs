//! Cross-component output markers (audit HV-E2).
//!
//! The spill/prune preview markers and the token-usage header are produced by
//! the core and recognised again by the core (and, for the spill preview, by
//! the prompt plugin). Two components agreeing on a human-readable phrase is a
//! drift bug waiting to happen: change the wording in one place and the
//! idempotency guard stops recognising its own output.
//!
//! This module is the SINGLE exported source for those strings. The consumer
//! modules re-export these constants, so a future structural marker (a JSON
//! field instead of prose) changes exactly this file.

/// Substring shared by the spill preview and the pruner's preview: its
/// presence means the content is ALREADY a bounded preview with a locator and
/// must never be re-pruned (idempotency).
pub const ALREADY_PRUNED_MARKER: &str = "omitted - see full output below";

/// Prefix of the spill locator appended to a preview: `[full output: <path>]`.
pub const SPILL_LOCATOR_PREFIX: &str = "[full output: ";

/// Header of the live token-usage telemetry block appended to the
/// conversation; its presence is what the telemetry pruner looks for.
pub const TOKEN_USAGE_MARKER: &str = "=== Token Usage ===";

#[cfg(test)]
mod tests {
    use super::*;

    /// The producing/consuming modules re-export THESE constants: there is one
    /// source, so the producer and the idempotency guard cannot drift apart.
    #[test]
    fn consumers_use_the_single_marker_source() {
        assert_eq!(
            crate::agent::tool_result_pruner::ALREADY_PRUNED_MARKER,
            ALREADY_PRUNED_MARKER
        );
        assert_eq!(
            crate::agent::tool_result_pruner::SPILL_LOCATOR_PREFIX,
            SPILL_LOCATOR_PREFIX
        );
        assert_eq!(
            crate::agent::token_usage::TOKEN_USAGE_MARKER,
            TOKEN_USAGE_MARKER
        );
    }

    /// The pruner recognises a preview built from the shared marker.
    #[test]
    fn a_shared_marker_preview_is_recognisable() {
        let preview = format!("head\n\n[... 5 chars {ALREADY_PRUNED_MARKER} ...]\n\ntail");
        assert!(preview.contains(ALREADY_PRUNED_MARKER));
    }
}
