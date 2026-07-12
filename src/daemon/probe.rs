//! The TUI's "is a daemon alive?" probe (dual-scheduler dedup, issue #27).
//!
//! Two signals, both required:
//!   * the daemon singleton flock (`clauthd.lock`) is HELD — a dead daemon's
//!     advisory lock auto-releases, so process death re-arms the TUI on the
//!     very next tick;
//!   * `status.json` is FRESH — a wedged-but-alive holder stops stamping
//!     `generated_at`, so the TUI re-arms instead of rendering a frozen feed.
//!
//! The probe belongs to the TUI ONLY: the daemon must never probe (its own
//! held flock would read as "another daemon" — a self-stand-down). That
//! asymmetry is wired at `spawn_refresher`'s `standdown_probe` flag, not here.

use crate::profile::clauth_dir;

/// How stale `status.json` may be before the holder is presumed wedged. The
/// daemon stamps it every ~1 s loop tick, but a single tick can legitimately
/// block up to the keychain shell-out's 20 s kill deadline — the window rides
/// above that so a slow switch doesn't false re-arm the TUI mid-write.
///
/// Deliberately BELOW the daemon's own 60 s watchdog deadline: a truly wedged
/// daemon leaves a ~30 s window where the TUI has re-armed while the daemon's
/// scheduler thread may still be fetching. That overlap is bounded, merely
/// duplicative, and preferable to the alternative — waiting out the watchdog
/// would leave a refresh coverage gap instead.
const STANDDOWN_STALE_MS: u64 = 30_000;

/// Whether a live (lock-holding + publishing) daemon owns the
/// fetch/rotate/switch loop right now. Best-effort `false` on any error — the
/// TUI then runs its own refresher, which is always safe (merely duplicative).
pub(crate) fn daemon_is_live() -> bool {
    let Ok(dir) = clauth_dir() else { return false };
    // Never CREATE the lock file from the probe: no file means no daemon has
    // ever started here, and manufacturing one would only add probe noise.
    let Ok(lock_file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(super::LOCK_FILE))
    else {
        return false;
    };
    if lock_file.try_lock().is_ok() {
        // We took it → nobody holds it. Dropping the handle releases it.
        return false;
    }
    let Ok(body) = std::fs::read_to_string(dir.join(super::STATUS_FILE)) else {
        return false;
    };
    status_is_fresh(&body, crate::usage::now_ms())
}

/// Pure freshness half of the probe: `body`'s `generated_at` stamp is within
/// [`STANDDOWN_STALE_MS`] of `now_ms`. An unparseable body or a missing or
/// malformed stamp reads as stale — never stand down on a feed we can't read.
/// A stamp in the FUTURE counts as fresh (clock skew must not flap the probe).
pub(crate) fn status_is_fresh(body: &str, now_ms: u64) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let Some(stamp) = v.get("generated_at").and_then(|g| g.as_str()) else {
        return false;
    };
    let Some(secs) = crate::usage::iso_to_epoch_secs(stamp) else {
        return false;
    };
    if secs < 0 {
        return false;
    }
    let generated_ms = (secs as u64).saturating_mul(1000);
    now_ms.saturating_sub(generated_ms) <= STANDDOWN_STALE_MS
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_probe.rs"]
mod tests;
