//! Probe contract: `daemon_is_live` = flock HELD **and** `status.json`
//! fresh. Either signal alone must read as "no daemon" — an unheld lock is a
//! dead process (advisory locks auto-release), a stale stamp is a wedged one.

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

// ── daemon_is_live (both signals) ────────────────────────────────────────────

#[test]
fn no_lock_file_reads_as_no_daemon() {
    let _home = HomeSandbox::new();
    write_status(&epoch_secs_to_iso(now_epoch_secs()));
    assert!(!daemon_is_live(), "fresh status but no lock file ever");
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
fn unheld_lock_reads_as_no_daemon() {
    let _home = HomeSandbox::new();
    write_status(&epoch_secs_to_iso(now_epoch_secs()));
    // Lock file exists (a daemon ran once) but nobody holds it — died/exited.
    let f = hold_daemon_lock();
    drop(f);
    assert!(!daemon_is_live(), "a released flock means the daemon died");
}

#[test]
fn held_lock_with_fresh_status_is_live() {
    let _home = HomeSandbox::new();
    write_status(&epoch_secs_to_iso(now_epoch_secs()));
    let _held = hold_daemon_lock();
    assert!(daemon_is_live());
}

#[test]
fn held_lock_with_stale_or_missing_status_is_not_live() {
    let _home = HomeSandbox::new();
    let _held = hold_daemon_lock();
    assert!(!daemon_is_live(), "lock held but no status.json yet");
    write_status(&epoch_secs_to_iso(now_epoch_secs() - 300));
    assert!(
        !daemon_is_live(),
        "a wedged holder (stale stamp) must re-arm the TUI"
    );
}
