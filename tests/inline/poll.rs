//! Inline tests for `crate::poll` — the cache-age → first-delay math and the
//! loop's wait/tick/coalesce/disconnect shape. The loop tests drive a real
//! channel from a worker thread and use an interval far longer than the test can
//! run, so any observed tick came from the delay or the signal under test, never
//! from the cadence.

use super::*;

use std::sync::mpsc;
use std::time::Duration;

/// Longer than any test's patience window, so a tick can only come from
/// `first_delay` elapsing or a manual signal.
const NEVER: Duration = Duration::from_secs(3600);
/// Generous upper bound for a tick that should be immediate.
const SOON: Duration = Duration::from_secs(5);
/// Window used to assert a tick did *not* happen.
const QUIET: Duration = Duration::from_millis(50);

/// Run the loop on a worker thread, returning the signal sender and a receiver
/// that yields one `()` per tick.
fn spawn_loop(first: Duration, interval: Duration) -> (mpsc::Sender<()>, mpsc::Receiver<()>) {
    let (signal_tx, signal_rx) = mpsc::channel::<()>();
    let (tick_tx, tick_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        run_polling_loop(&signal_rx, first, interval, || {
            let _ = tick_tx.send(());
        });
    });
    (signal_tx, tick_rx)
}

// ── first_delay ─────────────────────────────────────────────────────────────

#[test]
fn no_cache_fetches_now() {
    assert_eq!(
        first_delay(None, 60_000, Duration::from_secs(300)),
        Duration::ZERO,
        "an absent or unreadable cache has nothing to wait on"
    );
}

#[test]
fn a_fresh_stamp_waits_out_the_rest_of_the_interval() {
    assert_eq!(
        first_delay(Some(60_000), 70_000, Duration::from_secs(300)),
        Duration::from_secs(290),
        "10s into a 300s cadence leaves 290s of cached data"
    );
}

#[test]
fn a_stamp_exactly_at_the_interval_fetches_now() {
    assert_eq!(
        first_delay(Some(60_000), 360_000, Duration::from_secs(300)),
        Duration::ZERO,
        "the boundary belongs to the fetch side — the cache is spent"
    );
}

#[test]
fn an_expired_stamp_fetches_now() {
    assert_eq!(
        first_delay(
            Some(60_000),
            60_000 + 24 * 60 * 60 * 1000,
            Duration::from_secs(300)
        ),
        Duration::ZERO,
        "an age past the cadence must not underflow into a wait"
    );
}

#[test]
fn a_future_stamp_fetches_now() {
    // Clock skew, or a cache written by a machine ahead of ours. Reading this as
    // "age 0" would park the pricing feed for a full 24h on a bogus stamp.
    assert_eq!(
        first_delay(Some(360_000), 60_000, Duration::from_secs(300)),
        Duration::ZERO,
        "a stamp in the future is not trustworthy freshness"
    );
}

// ── run_polling_loop ────────────────────────────────────────────────────────

#[test]
fn a_zero_first_delay_ticks_immediately() {
    let (signal_tx, tick_rx) = spawn_loop(Duration::ZERO, NEVER);

    tick_rx
        .recv_timeout(SOON)
        .expect("zero delay must tick without a signal and without waiting the cadence");
    assert_eq!(
        tick_rx.recv_timeout(QUIET),
        Err(mpsc::RecvTimeoutError::Timeout),
        "the second tick waits the full interval"
    );

    drop(signal_tx);
}

#[test]
fn a_pending_first_delay_holds_the_tick_until_a_signal() {
    let (signal_tx, tick_rx) = spawn_loop(NEVER, NEVER);

    assert_eq!(
        tick_rx.recv_timeout(QUIET),
        Err(mpsc::RecvTimeoutError::Timeout),
        "a still-fresh cache must not be re-fetched at startup"
    );

    signal_tx.send(()).expect("loop is listening");
    tick_rx
        .recv_timeout(SOON)
        .expect("a manual refresh during the first delay must tick at once");

    drop(signal_tx);
}

#[test]
fn signals_queued_during_the_first_delay_coalesce_into_one_tick() {
    let (signal_tx, signal_rx) = mpsc::channel::<()>();
    let (tick_tx, tick_rx) = mpsc::channel::<()>();
    // Queued before the loop starts, so the drain is exercised deterministically.
    for _ in 0..3 {
        signal_tx.send(()).expect("buffered channel");
    }
    std::thread::spawn(move || {
        run_polling_loop(&signal_rx, NEVER, NEVER, || {
            let _ = tick_tx.send(());
        });
    });

    tick_rx.recv_timeout(SOON).expect("first tick");
    assert_eq!(
        tick_rx.recv_timeout(QUIET),
        Err(mpsc::RecvTimeoutError::Timeout),
        "three queued signals must collapse into a single fetch"
    );

    drop(signal_tx);
}

#[test]
fn a_disconnect_during_the_first_delay_ends_the_loop_without_ticking() {
    let (signal_tx, signal_rx) = mpsc::channel::<()>();
    drop(signal_tx);
    let (done_tx, done_rx) = mpsc::channel::<usize>();

    // On a worker thread so a loop that stops returning fails red instead of
    // hanging the run: `done` only ever arrives if the loop exited.
    std::thread::spawn(move || {
        let mut ticks = 0usize;
        run_polling_loop(&signal_rx, NEVER, NEVER, || ticks += 1);
        let _ = done_tx.send(ticks);
    });

    let ticks = done_rx
        .recv_timeout(SOON)
        .expect("a disconnected channel must end the loop, not wait out the first delay");
    assert_eq!(ticks, 0, "a disconnected channel must not trigger a fetch");
}
