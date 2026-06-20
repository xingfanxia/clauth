//! Shared background-polling loop for the feed workers (`status`, `pricing`,
//! `tokens`). Each worker spawns a thread that cold-loads its cache, then
//! repeatedly runs a fetch and blocks on a refresh channel until either the
//! cadence elapses (re-fetch) or a manual signal arrives (coalesce + re-fetch),
//! exiting only when the channel disconnects (TUI shutdown). Only this loop is
//! shared; the cold-load and event-sending differ per worker and stay local.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// Drive a background worker: call `tick` once, then block on `refresh_rx` for
/// up to `interval`. On a manual signal, drain any coalesced extras before the
/// next tick; on timeout, tick again; on disconnect, return (ending the thread).
pub(crate) fn run_polling_loop<F: FnMut()>(
    refresh_rx: &Receiver<()>,
    interval: Duration,
    mut tick: F,
) {
    loop {
        tick();

        match refresh_rx.recv_timeout(interval) {
            Ok(()) => while refresh_rx.try_recv().is_ok() {},
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
