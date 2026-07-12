//! Small, pure daemon state types plus the switch-backoff schedule. Extracted
//! from `mod.rs` to keep the run-loop module focused; re-exported from `super`
//! so callers (tick.rs, the inline tests) reference them unchanged.

/// Retry/backoff state for a persistently-failing switch. Without it, a switch
/// that can't land (keychain denial, diverged active, busy target) re-queued
/// and logged ~1/s forever, ballooning the daemon log and burying the signal.
/// This dedups the log (emit only when the reason changes) and spaces retries
/// with exponential backoff once a target has failed repeatedly.
#[derive(Debug, Clone)]
pub(crate) struct SwitchBackoff {
    pub(crate) target: String,
    /// Consecutive failed attempts for this target.
    pub(crate) attempts: u32,
    /// Epoch-ms before which no further attempt is made (backoff window).
    pub(crate) not_before: u64,
    /// Last failure reason for this target — the dedup key for logging.
    pub(crate) reason: String,
    /// Epoch-ms after which the daemon gives up on this target entirely
    /// (stamped at the FIRST failure). The scheduler re-queues a still-correct
    /// target fresh on a later tick.
    pub(crate) retry_until: u64,
}

/// Base backoff after a switch has failed enough times to warrant spacing.
const SWITCH_BACKOFF_BASE_MS: u64 = 2_000;
/// Ceiling so a permanently-stuck target still gets an occasional retry.
const SWITCH_BACKOFF_MAX_MS: u64 = 60_000;

/// Backoff (ms) before the next attempt after `attempts` consecutive failures.
/// The first two failures retry immediately (`0`) so the common brief-fetch case
/// still lands the instant the target goes idle (preserves the re-queue
/// behavior); only a persistently-failing switch backs off, exponentially, capped.
/// Pure so the schedule is unit-testable.
pub(crate) fn switch_backoff_ms(attempts: u32) -> u64 {
    match attempts {
        0..=2 => 0,
        n => SWITCH_BACKOFF_BASE_MS
            .saturating_mul(1_u64 << (n - 3).min(20))
            .min(SWITCH_BACKOFF_MAX_MS),
    }
}
