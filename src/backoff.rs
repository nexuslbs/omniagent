//! ONE exponential-backoff policy for external plugin/bridge respawns.
//!
//! Replaces the hand-written `2^failures`-capped-at-60 pair that lived twice
//! in `platform/external/client.rs` plus a third `60` cap in the agent loop
//! (audit HV-D2 / HV-B13): changing the retry policy used to need three edits,
//! and a change applied to only one of them failed silently.

use std::time::Duration;

/// `base_secs ^ failures`, capped at `cap_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffPolicy {
    /// Backoff base, in seconds (raised to the failure count).
    pub base_secs: u64,
    /// Upper bound, in seconds.
    pub cap_secs: u64,
}

impl BackoffPolicy {
    /// The shipped policy: 2 s base, 60 s cap.
    pub const DEFAULT: Self = Self {
        base_secs: 2,
        cap_secs: 60,
    };

    /// Delay before the attempt following `failures` consecutive failures.
    pub fn delay(self, failures: u32) -> Duration {
        Duration::from_secs(self.base_secs.saturating_pow(failures).min(self.cap_secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_pins_base_and_cap() {
        assert_eq!(BackoffPolicy::DEFAULT.base_secs, 2);
        assert_eq!(BackoffPolicy::DEFAULT.cap_secs, 60);
    }

    #[test]
    fn delay_is_exponential_and_capped() {
        let p = BackoffPolicy::DEFAULT;
        assert_eq!(p.delay(0), Duration::from_secs(1));
        assert_eq!(p.delay(1), Duration::from_secs(2));
        assert_eq!(p.delay(2), Duration::from_secs(4));
        assert_eq!(p.delay(5), Duration::from_secs(32));
        assert_eq!(p.delay(6), Duration::from_secs(60));
        assert_eq!(p.delay(50), Duration::from_secs(60));
    }
}
