//! Probe contract (#27):
//!   * `daemon_health` drives the `● daemon` header dot from two signals —
//!     the `clauthd.lock` flock (presence) and `status.json` freshness (health):
//!     no lock → Absent (hidden), held + fresh → Fresh (green), held + stale →
//!     Stale (amber).
//!   * `FetchLease` is the single-fetcher lease over `usage-fetch.lock`: exactly
//!     one holder at a time, held for life, released on drop so a waiter takes
//!     over.

use super::*;
use crate::profile::clauth_dir;
use crate::testutil::HomeSandbox;
use crate::usage::{epoch_secs_to_iso, now_epoch_secs, now_ms};

fn write_status(generated_at: &str) {
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join(super::super::STATUS_FILE),
        format!(r#"{{"schema":1,"generated_at":"{generated_at}","profiles":[]}}"#),
    )
    .expect("write status");
}

/// Opens + exclusively locks the daemon lock file, standing in for a live
/// daemon. The returned handle must stay alive for the duration of the probe.
fn hold_daemon_lock() -> std::fs::File {
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(super::super::LOCK_FILE))
        .expect("open lock");
    f.try_lock().expect("acquire test lock");
    f
}

// ── status_is_fresh (pure half) ──────────────────────────────────────────────

#[test]
fn fresh_stale_and_garbage_stamps() {
    let now = now_ms();
    let iso_now = epoch_secs_to_iso(now_epoch_secs());
    let fresh = format!(r#"{{"schema":1,"generated_at":"{iso_now}"}}"#);
    assert!(
        status_is_fresh(&fresh, now),
        "a just-written stamp is fresh"
    );

    let iso_old = epoch_secs_to_iso(now_epoch_secs() - 120);
    let stale = format!(r#"{{"schema":1,"generated_at":"{iso_old}"}}"#);
    assert!(!status_is_fresh(&stale, now), "a 2-min-old stamp is stale");

    // Clock skew: a stamp slightly in the future must not flap the probe.
    let iso_future = epoch_secs_to_iso(now_epoch_secs() + 60);
    let future = format!(r#"{{"schema":1,"generated_at":"{iso_future}"}}"#);
    assert!(
        status_is_fresh(&future, now),
        "a future stamp reads as fresh"
    );

    assert!(!status_is_fresh("not json", now));
    assert!(!status_is_fresh(r#"{"schema":1}"#, now), "missing stamp");
    assert!(
        !status_is_fresh(r#"{"generated_at":"yesterday-ish"}"#, now),
        "malformed stamp"
    );
    assert!(
        !status_is_fresh(r#"{"generated_at":12345}"#, now),
        "non-string stamp"
    );
}

// ── daemon_health (dot: presence + health) ───────────────────────────────────

#[test]
fn no_lock_file_reads_as_absent() {
    let _home = HomeSandbox::new();
    write_status(&epoch_secs_to_iso(now_epoch_secs()));
    assert_eq!(
        daemon_health(),
        DaemonHealth::Absent,
        "fresh status but no lock file ever → dot hidden"
    );
    // And the probe must not have manufactured the lock file.
    assert!(
        !clauth_dir()
            .expect("dir")
            .join(super::super::LOCK_FILE)
            .exists(),
        "the probe never creates the lock file"
    );
}

#[test]
fn unheld_lock_reads_as_absent() {
    let _home = HomeSandbox::new();
    write_status(&epoch_secs_to_iso(now_epoch_secs()));
    // Lock file exists (a daemon ran once) but nobody holds it — died/exited.
    let f = hold_daemon_lock();
    drop(f);
    assert_eq!(
        daemon_health(),
        DaemonHealth::Absent,
        "a released flock means the daemon died → dot hidden"
    );
}

#[test]
fn held_lock_with_fresh_status_is_fresh() {
    let _home = HomeSandbox::new();
    write_status(&epoch_secs_to_iso(now_epoch_secs()));
    let _held = hold_daemon_lock();
    assert_eq!(daemon_health(), DaemonHealth::Fresh, "up + fresh → green");
}

#[test]
fn held_lock_with_stale_or_missing_status_is_stale() {
    let _home = HomeSandbox::new();
    let _held = hold_daemon_lock();
    assert_eq!(
        daemon_health(),
        DaemonHealth::Stale,
        "up but no status.json yet → amber"
    );
    write_status(&epoch_secs_to_iso(now_epoch_secs() - 300));
    assert_eq!(
        daemon_health(),
        DaemonHealth::Stale,
        "up but a wedged holder's stale stamp → amber"
    );
}

// ── FetchLease (single-fetcher lease over usage-fetch.lock) ───────────────────

#[test]
fn one_holder_at_a_time_and_a_waiter_takes_the_freed_lease() {
    let _home = HomeSandbox::new();

    // First instance wins the lease and holds it for life.
    let a = FetchLease::new();
    assert!(a.acquire(), "first caller becomes the fetcher");
    assert!(
        a.acquire(),
        "a held lease is idempotent — still the fetcher"
    );

    // A second instance over the SAME file is denied → it must stand down.
    let b = FetchLease::new();
    assert!(
        !b.acquire(),
        "two lease holders never both fetch — the second stands down"
    );

    // The holder exits (its File drops → flock released). The waiter's next
    // acquire wins.
    drop(a);
    assert!(
        b.acquire(),
        "on the holder's exit the waiter takes over the freed lease"
    );
}

#[test]
fn an_unreadable_lock_stands_down() {
    let _home = HomeSandbox::new();
    // Make `usage-fetch.lock` a directory so the lease can never open it as a
    // file — the acquire must fail closed (stand down), never dup-fetch.
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(dir.join(super::super::FETCH_LOCK_FILE)).expect("mkdir lockpath");
    let lease = FetchLease::new();
    assert!(
        !lease.acquire(),
        "an unopenable lock file stands down rather than fetching"
    );
}
