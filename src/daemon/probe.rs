//! Daemon presence + the single-fetcher lease (dual-scheduler dedup, #27).
//!
//! Two `~/.clauth` flock files, peers in the same dir:
//!   * `clauthd.lock` — the daemon singleton + **presence beacon**. Held for
//!     life by the running daemon; a display-only try-lock tells the TUI header
//!     whether a daemon is up (an advisory lock auto-releases on process death,
//!     so a dead daemon reads as absent on the next probe). Paired with
//!     `status.json`'s freshness it drives the `● daemon` health dot
//!     ([`daemon_health`]).
//!   * `usage-fetch.lock` — the **single-fetcher lease** ([`FetchLease`]).
//!     Exactly one instance (daemon OR a TUI) holds it at a time and does all
//!     usage fetching / rotation / switch decisions while it does; every other
//!     instance hydrates from the shared disk cache. First-come, held for life,
//!     released only on process exit — no preemption, so the switch-decider
//!     never thrashes between processes.

use std::fs::{File, OpenOptions};
use std::sync::Mutex;

use crate::profile::clauth_dir;

/// How stale `status.json` may be before the `● daemon` dot flips green→amber.
/// The daemon stamps it every ~1s loop tick, but a single tick can legitimately
/// block up to the keychain shell-out's 20s kill deadline, so the window rides
/// just above that. It also lands at the daemon's tightened
/// [`WATCHDOG_DEADLINE`](super::WATCHDOG_DEADLINE), so amber reads as "wedging,
/// about to be aborted + restarted" rather than a transient slow tick.
const DAEMON_STALE_MS: u64 = 30_000;

/// The `● daemon` header dot's three display states, derived from the daemon
/// singleton flock (presence) + `status.json` mtime (health). Display-only:
/// nothing here gates fetching — that is [`FetchLease`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonHealth {
    /// No daemon: `clauthd.lock` is free (never started, or the holder died).
    /// The dot is hidden; the TUI self-fetches under its own lease.
    Absent,
    /// A daemon holds the lock but its feed is stale/unwritten — wedging,
    /// pre-abort, or just-booted before the first `status.json` write. Amber.
    Stale,
    /// A daemon is up and its feed is fresh. Green.
    Fresh,
}

/// Probe the daemon's presence + health for the header dot. Best-effort: any
/// error that hides whether a daemon is up reads as [`DaemonHealth::Absent`]
/// (the dot simply disappears — never a false "daemon up"). Never CREATES the
/// lock file: a missing file means no daemon has ever started here.
pub(crate) fn daemon_health() -> DaemonHealth {
    let Ok(dir) = clauth_dir() else {
        return DaemonHealth::Absent;
    };
    let Ok(lock_file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(super::LOCK_FILE))
    else {
        return DaemonHealth::Absent;
    };
    match lock_file.try_lock() {
        // We took it → nobody holds it → no live daemon. Dropping releases it.
        Ok(()) => return DaemonHealth::Absent,
        // Held → a daemon is present; fall through to the health read.
        Err(std::fs::TryLockError::WouldBlock) => {}
        // Can't tell (io error): hide the dot rather than assert a daemon.
        Err(std::fs::TryLockError::Error(_)) => return DaemonHealth::Absent,
    }
    // Present. A missing/unreadable feed = booted-but-not-yet-published → amber.
    let Ok(body) = std::fs::read_to_string(dir.join(super::STATUS_FILE)) else {
        return DaemonHealth::Stale;
    };
    if status_is_fresh(&body, crate::usage::now_ms()) {
        DaemonHealth::Fresh
    } else {
        DaemonHealth::Stale
    }
}

/// Pure freshness test: `body`'s `generated_at` stamp is within [`DAEMON_STALE_MS`]
/// of `now_ms`. An unparseable body or a missing/malformed stamp reads as stale —
/// never render a feed we can't read as fresh. A stamp in the FUTURE counts as
/// fresh (clock skew must not flap the dot).
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
    now_ms.saturating_sub(generated_ms) <= DAEMON_STALE_MS
}

/// The single-fetcher lease over `usage-fetch.lock` (#27). Once acquired the
/// [`File`] is retained for the process lifetime, so the flock is held for life
/// and released only on exit — deliberately NOT re-acquired per tick, which
/// would bounce the switch-decider between processes (the #27 switch thrash).
///
/// The inner [`Mutex`] is a pure leaf: locked only inside [`acquire`](Self::acquire)
/// and released before it returns, so it carries no lock rank. The flock itself
/// sits outside the compile-time rank system and must be taken OUTERMOST of the
/// tick's ranked locks (`acquire` runs at the very top of the tick).
#[derive(Default)]
pub(crate) struct FetchLease {
    held: Mutex<Option<File>>,
}

impl FetchLease {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Become — or confirm we already are — the single fetcher. Returns `true`
    /// when THIS instance holds the lease (fetch this tick), `false` when another
    /// instance holds it or the lock is unreadable (stand down and hydrate from
    /// the shared cache). Idempotent once held: a held lease short-circuits to
    /// `true`, so the flock is never re-taken. Both error arms of the try-lock
    /// stand down — an io error is never a licence to dup-fetch.
    pub(crate) fn acquire(&self) -> bool {
        let mut held = match self.held.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if held.is_some() {
            return true;
        }
        let Ok(dir) = clauth_dir() else {
            return false;
        };
        // The lease file lives in `~/.clauth`; ensure the dir exists (best-effort,
        // 0o700) so a first-run TUI with no daemon can still take the lease.
        // `serve()` already creates it for the daemon; a TUI normally reaches here
        // with the dir already present (profiles live in it).
        let _ = crate::profile::mkdir_700(&dir);
        let Ok(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(super::FETCH_LOCK_FILE))
        else {
            return false;
        };
        match file.try_lock() {
            Ok(()) => {
                *held = Some(file);
                true
            }
            Err(std::fs::TryLockError::WouldBlock) => false,
            Err(std::fs::TryLockError::Error(_)) => false,
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_probe.rs"]
mod tests;
