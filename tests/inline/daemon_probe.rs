//! Probe contract (#27, #57):
//!   * `claim_singleton` caps the daemon tree at one active instance plus one
//!     standby: a third arrival is `Redundant` and exits instead of parking.
//!   * `daemon_health` drives the `● daemon` header dot from two signals —
//!     the `clauthd.lock` flock (presence) and `status.json` freshness (health):
//!     no lock → Absent (hidden), held + fresh → Fresh (green), held + stale →
//!     Stale (amber).
//!   * `singleton_held` asks the same presence question as a DECISION rather
//!     than a display: where the dot hides an unreadable lock, `--status` fails
//!     on it instead of telling a supervisor to spawn.
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

// ── claim_singleton (one active + one standby, #57) ───────────────────────────

/// `~/.clauth` inside the sandbox, created so the lock files have a home.
fn sandbox_dir() -> std::path::PathBuf {
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// One claim attempt, no retry — the role decision on its own. The production
/// [`claim_singleton`] retries past a transient probe hold, which
/// `a_transient_probe_hold_never_forces_an_exit` covers separately.
fn claim_now(dir: &std::path::Path, standby: bool) -> Claim {
    claim_singleton_with(dir, standby, 1, std::time::Duration::ZERO).expect("claim")
}

/// How long the fake probe below sits on both flocks. A literal, deliberately
/// NOT derived from [`CLAIM_RETRY`]: a hold that scales with the constant under
/// test shrinks with it, so the fixture could never catch the retry window
/// collapsing. It has to outlast the first attempt, so a one-shot claim loses
/// the race, and the full window has to clear it, so the retry wins — the first
/// holds by construction (`probe_holding_both` returns while still holding), the
/// second is pinned below.
const PROBE_HOLD: std::time::Duration = std::time::Duration::from_millis(150);
// The shipped schedule must outlive PROBE_HOLD, or the recovery this test proves
// cannot happen and it reds for a fixture reason instead of a real one.
// `as_millis` because Duration's comparisons are not const — it also floors a
// sub-millisecond CLAIM_RETRY to 0, where production's `!is_zero()` would pass.
const _: () = assert!(
    PROBE_HOLD.as_millis() < CLAIM_RETRY.as_millis() * (CLAIM_ATTEMPTS as u128 - 1),
    "the retry window no longer outlives PROBE_HOLD: a starting daemon would spend every \
     attempt inside a probe's hold and exit for good. Retune PROBE_HOLD with the schedule, \
     never silence this."
);

/// Take both singleton flocks the way a presence probe does, hold them for
/// `hold`, then let go. Returns once both are held, so the caller's clock starts
/// inside the hold: a claim that gets no second attempt is still inside it.
///
/// Stronger than either real probe on purpose — `daemon_health` releases the
/// singleton lock before `standby_waiting` opens the slot file, so nothing in
/// production holds both at once. Read it as a worst case, not as a model of the
/// probes.
fn probe_holding_both(
    dir: &std::path::Path,
    hold: std::time::Duration,
) -> std::thread::JoinHandle<()> {
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let dir = dir.to_path_buf();
    let handle = std::thread::spawn(move || {
        let a = crate::profile::open_state_file(&dir.join(super::super::LOCK_FILE)).expect("open");
        a.try_lock().expect("probe takes the free singleton lock");
        let s = crate::profile::open_state_file(&dir.join(super::super::STANDBY_LOCK_FILE))
            .expect("open");
        s.try_lock().expect("probe takes the free standby lock");
        held_tx.send(()).expect("signal held");
        std::thread::sleep(hold);
        drop(s);
        drop(a);
    });
    held_rx.recv().expect("probe reports both flocks held");
    handle
}

#[test]
fn third_instance_is_redundant_and_the_promoted_standby_frees_the_slot() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();

    let Claim::Active(active) = claim_now(&dir, true) else {
        panic!("the first instance takes the singleton lock");
    };
    assert_eq!(
        holder_pid(),
        Some(std::process::id()),
        "the holder stamps its pid so a `ps` dump names the live one"
    );

    let Claim::Standby(slot) = claim_now(&dir, true) else {
        panic!("the second instance takes the one standby slot");
    };
    assert!(
        standby_waiting(),
        "the parked instance is visible to a probe"
    );
    assert!(
        matches!(claim_now(&dir, true), Claim::Redundant),
        "a third instance exits instead of parking — this is the #57 pile-up"
    );

    // A decoy pid in the sidecar, planted while the holder is still up: this
    // process would stamp its own pid either way, so without it a promotion that
    // never re-stamps reads as correct. Written to PID_FILE, not LOCK_FILE —
    // the active handle holds LOCK_FILE's mandatory flock on Windows, so a
    // foreign write there would fault, and the pid no longer lives there anyway.
    std::fs::write(dir.join(super::super::PID_FILE), b"999\n").expect("plant a decoy pid");

    // The daemon exits: the standby takes over, off a thread so a promotion
    // that never unblocks fails the test instead of wedging the suite.
    drop(active);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(slot.promote());
    });
    let _promoted = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the standby promotes once the holder exits")
        .expect("promote");

    assert_eq!(
        holder_pid(),
        Some(std::process::id()),
        "the takeover re-stamps the sidecar, so the decoy pid can't outlive the handover"
    );
    assert!(
        !standby_waiting(),
        "promotion releases the slot: the takeover must not leave it held"
    );
    assert!(
        matches!(claim_now(&dir, true), Claim::Standby(_)),
        "the freed slot is available to the next arrival"
    );
}

#[test]
fn no_standby_exits_rather_than_taking_the_free_slot() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let Claim::Active(_active) = claim_now(&dir, true) else {
        panic!("the first instance takes the singleton lock");
    };

    assert!(
        matches!(claim_now(&dir, false), Claim::Redundant),
        "--no-standby never queues, even with the slot free"
    );
    assert!(
        !dir.join(super::super::STANDBY_LOCK_FILE).exists(),
        "--no-standby never even creates the slot file"
    );
}

/// Both presence probes take the flock they test and release it microseconds
/// later, so one lost try-lock is not proof of a daemon. A `Redundant` decided
/// on a reader would exit a supervisor's instance for good.
///
/// Runs against the SHIPPED [`claim_singleton`] rather than an injected
/// schedule: `CLAIM_ATTEMPTS`/`CLAIM_RETRY` are what the recovery is made of,
/// and a test that supplies its own numbers stays green with the production
/// window collapsed back to one attempt.
#[test]
fn a_transient_probe_hold_never_forces_an_exit() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let probe = probe_holding_both(&dir, PROBE_HOLD);

    // Positive control: a single attempt reads the probe as a live daemon plus a
    // live standby, which is exactly the wrong answer the retry exists to undo.
    assert!(
        matches!(claim_now(&dir, true), Claim::Redundant),
        "one attempt cannot tell a probe from a holder"
    );

    let claim = claim_singleton(&dir, true).expect("claim");
    assert!(
        matches!(claim, Claim::Active(_)),
        "the shipped retry window outlives a probe's hold instead of exiting for good"
    );
    probe.join().expect("probe thread");
}

/// The retry is for a `Redundant` verdict alone. Dropping a won standby slot to
/// re-take it hands the one seat back for most of the window and delays a
/// takeover that already has its answer.
#[test]
fn a_won_standby_slot_is_kept_rather_than_re_taken() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let Claim::Active(_active) = claim_now(&dir, true) else {
        panic!("the first instance takes the singleton lock");
    };

    let started = std::time::Instant::now();
    let claim = claim_singleton(&dir, true).expect("claim");
    let elapsed = started.elapsed();

    assert!(
        matches!(claim, Claim::Standby(_)),
        "the second instance parks in the standby slot"
    );
    assert!(
        elapsed < CLAIM_RETRY,
        "a won slot must stand: the claim took {elapsed:?}, past the {CLAIM_RETRY:?} gap it \
         would only wait for by re-testing an answer it already had"
    );
    assert!(
        standby_waiting(),
        "the slot reads as taken the moment the claim returns, never re-opened by a retry"
    );
}

/// `clauth daemon --status` decides on `singleton_held`, not on the header dot:
/// a lock it cannot read has to surface as an error there, since a `--status ||
/// spawn` supervisor respawns on the dot's "no daemon". These are the three
/// answers a sandbox can produce — the io-error arm needs a filesystem without
/// working locks.
#[test]
fn singleton_held_separates_a_missing_lock_a_free_one_and_a_held_one() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    assert!(
        !singleton_held().expect("a missing lock file is an answer, not a failure"),
        "no lock file ever → no daemon has started here"
    );
    assert!(
        !dir.join(super::super::LOCK_FILE).exists(),
        "the probe never creates the lock file"
    );

    let Claim::Active(active) = claim_now(&dir, true) else {
        panic!("the first instance takes the singleton lock");
    };
    assert!(
        singleton_held().expect("a held lock"),
        "a held lock is a running daemon"
    );

    drop(active);
    assert!(
        !singleton_held().expect("a released lock"),
        "a released flock means the holder died → no daemon, still not an error"
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
