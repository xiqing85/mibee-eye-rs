//! SIP-Date drift observation (GB/T 28181-2022 §9.10.2).
//!
//! The platform's clock arrives on the REGISTER response's SIP `Date`
//! header; the gb28181-rs `ServerHandle` exposes the last parsed value
//! (`platform_date_unix`). The supervision loop polls it every
//! [`DATE_OBSERVER_INTERVAL`] and WARNs on significant drift — pure
//! observation: disciplining the clock stays a host/NTP decision, the
//! system clock is never touched here (same posture as the notebook
//! product; library seam contract).

use std::time::Duration;

/// Poll cadence; one REGISTER per lease keeps the value fresh enough.
pub const DATE_OBSERVER_INTERVAL: Duration = Duration::from_secs(60);

/// Beyond this the drift is worth an operator's attention.
const DATE_DRIFT_WARN_SECS: u64 = 5;

/// Outcome of evaluating one platform-clock sample against the latch.
#[derive(Debug, PartialEq, Eq)]
pub enum DateDriftOutcome {
    /// Drift back within the threshold — clear the latch (log once).
    Recovered,
    /// Beyond threshold but not moved another threshold since the last
    /// WARN — stay quiet (keep the latch).
    Stable,
    /// Beyond threshold and moved since the last WARN — WARN now with
    /// the signed drift, latch its magnitude.
    Warn(i64),
}

/// Signed drift is `local - platform` (how far local runs ahead).
/// A naive Option-as-latch misjudges a stable drift (7s → 7s) as
/// recovery and alternates warn/clear every poll — the three states are
/// explicit for exactly that reason.
#[must_use]
pub fn evaluate_date_drift(
    platform_unix: i64,
    local_unix: i64,
    last_warned: Option<u64>,
) -> DateDriftOutcome {
    let drift = local_unix - platform_unix;
    let abs = drift.unsigned_abs();
    if abs <= DATE_DRIFT_WARN_SECS {
        return if last_warned.is_some() {
            DateDriftOutcome::Recovered
        } else {
            DateDriftOutcome::Stable
        };
    }
    match last_warned {
        Some(prev) if abs.abs_diff(prev) < DATE_DRIFT_WARN_SECS => DateDriftOutcome::Stable,
        _ => DateDriftOutcome::Warn(drift),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_threshold_is_quiet() {
        assert_eq!(
            evaluate_date_drift(1_000, 1_000, None),
            DateDriftOutcome::Stable
        );
        assert_eq!(
            evaluate_date_drift(1_000, 1_005, None),
            DateDriftOutcome::Stable
        );
    }

    #[test]
    fn first_excursion_warns() {
        assert_eq!(
            evaluate_date_drift(1_000, 1_007, None),
            DateDriftOutcome::Warn(7)
        );
        // Sign preserved: platform ahead of local.
        assert_eq!(
            evaluate_date_drift(1_012, 1_000, None),
            DateDriftOutcome::Warn(-12)
        );
    }

    #[test]
    fn stable_drift_does_not_rewarn() {
        // Already warned at 7s; the same drift (or a 1s wiggle) stays quiet.
        assert_eq!(
            evaluate_date_drift(1_000, 1_007, Some(7)),
            DateDriftOutcome::Stable
        );
        assert_eq!(
            evaluate_date_drift(1_000, 1_009, Some(7)),
            DateDriftOutcome::Stable
        );
        // Moved another threshold → warn again.
        assert_eq!(
            evaluate_date_drift(1_000, 1_013, Some(7)),
            DateDriftOutcome::Warn(13)
        );
    }

    #[test]
    fn recovery_clears_the_latch() {
        assert_eq!(
            evaluate_date_drift(1_000, 1_003, Some(7)),
            DateDriftOutcome::Recovered
        );
        assert_eq!(
            evaluate_date_drift(1_000, 1_000, None),
            DateDriftOutcome::Stable
        );
        assert_eq!(
            evaluate_date_drift(1_000, 1_006, None),
            DateDriftOutcome::Warn(6)
        );
    }
}
