//! Shared background-polling loop for the feed workers (`status`, `pricing`).
//! Each worker spawns a thread that cold-loads its cache, waits out whatever the
//! cache has left of the cadence ([`first_delay`]) — so a relaunch minutes apart
//! does not re-download a feed that refreshes daily — then repeatedly fetches and
//! blocks on a refresh channel until either the cadence elapses (re-fetch) or a
//! manual signal arrives (coalesce + re-fetch), exiting only when the channel
//! disconnects (TUI shutdown). Only this loop is shared; the cold-load and
//! event-sending differ per worker and stay local.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// How long a worker may hold off its first fetch: whatever is left of `interval`
/// since the cache was stamped at `fetched_at_ms`. Fails toward fetching — no
/// cache, an age past `interval`, or a stamp in the *future* (clock skew, a cache
/// written by a machine ahead of ours) all yield [`Duration::ZERO`]; reading a
/// future stamp as "age 0" would instead stall the feed for a whole interval.
pub(crate) fn first_delay(fetched_at_ms: Option<u64>, now_ms: u64, interval: Duration) -> Duration {
    let Some(age_ms) = fetched_at_ms.and_then(|at| now_ms.checked_sub(at)) else {
        return Duration::ZERO;
    };
    interval.saturating_sub(Duration::from_millis(age_ms))
}

/// Drive a background worker: block on `refresh_rx` for up to `first_delay`, tick,
/// then block for up to `interval` before each further tick. On a manual signal,
/// drain any coalesced extras before ticking; on timeout, tick; on disconnect,
/// return (ending the thread). A `first_delay` of [`Duration::ZERO`] ticks at once.
pub(crate) fn run_polling_loop<F: FnMut()>(
    refresh_rx: &Receiver<()>,
    first_delay: Duration,
    interval: Duration,
    mut tick: F,
) {
    let mut wait = first_delay;
    loop {
        match refresh_rx.recv_timeout(wait) {
            Ok(()) => while refresh_rx.try_recv().is_ok() {},
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        tick();
        wait = interval;
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "../tests/inline/poll.rs"]
mod tests;
