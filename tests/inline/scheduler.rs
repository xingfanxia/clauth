use std::collections::HashMap;
use std::sync::Arc;

use crate::lockorder::RankedMutex;

use super::{
    ActivityStore, EpochMs, LastFetchedAt, ProfileActivity, REFRESH_INTERVAL_MS, TokenEntry,
    clear_activity, mark_activity, partition_due,
};

fn token(name: &str) -> TokenEntry {
    TokenEntry {
        name: name.to_string(),
        access_token: "access".to_string(),
        refresh_token: Some("refresh".to_string()),
    }
}

/// Every profile uses the same fixed `REFRESH_INTERVAL_MS` cadence: a
/// never-fetched profile is due once `now` reaches the interval, a just-fetched
/// one is not due until exactly one interval has elapsed, and the published
/// next-time is always `last_fetched + REFRESH_INTERVAL_MS`.
#[test]
fn partition_due_uses_fixed_interval() {
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let snapshot = vec![token("a")];
    let base = 1_700_000_000_000u64; // realistic epoch-ms

    // Never fetched: last = 0, next = REFRESH_INTERVAL_MS, due at any real `now`.
    let (due, next) = partition_due(&snapshot, base, &last_fetched, &activity);
    assert_eq!(due.len(), 1, "a never-fetched profile is due");
    assert_eq!(next.get("a").copied(), Some(REFRESH_INTERVAL_MS));

    // Just fetched: not due one ms later.
    last_fetched
        .lock()
        .unwrap()
        .insert("a".to_string(), EpochMs::from_millis(base));
    let (due, next) = partition_due(&snapshot, base + 1, &last_fetched, &activity);
    assert!(due.is_empty(), "not due one ms after a fetch");
    assert_eq!(next.get("a").copied(), Some(base + REFRESH_INTERVAL_MS));

    // Exactly one interval later: due again.
    let (due, _) = partition_due(
        &snapshot,
        base + REFRESH_INTERVAL_MS,
        &last_fetched,
        &activity,
    );
    assert_eq!(due.len(), 1, "due once the fixed interval has elapsed");
}

/// Profiles mid-switch / mid-refresh are excluded from the due set even when
/// their interval has elapsed, but their countdown still publishes so the UI
/// shows when they become eligible again.
#[test]
fn partition_due_excludes_switching_and_refreshing() {
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let snapshot = vec![token("a"), token("b")];

    mark_activity(&activity, "a", ProfileActivity::Switching);
    mark_activity(&activity, "b", ProfileActivity::Refreshing);

    let (due, next) = partition_due(&snapshot, REFRESH_INTERVAL_MS + 1, &last_fetched, &activity);
    assert!(due.is_empty(), "switching/refreshing profiles are excluded");
    assert!(
        next.contains_key("a") && next.contains_key("b"),
        "countdown still publishes for excluded profiles"
    );
}

// ── Panic-clear discipline ────────────────────────────────────────────────────

/// The mark/join/clear discipline in fetch_all_into and spawn_refresher must
/// clear the ActivityStore slot even when the worker panics — exercises the
/// `Err(_)` arm of `h.join()` without real HTTP or a full scheduler.
#[test]
fn activity_cleared_on_worker_panic() {
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let name = "test-profile";

    mark_activity(&activity, name, ProfileActivity::Fetching);
    assert!(
        !activity.lock().unwrap().is_empty(),
        "slot must be set after mark_activity"
    );

    let h = std::thread::spawn(|| -> () { panic!("simulated worker panic") });

    // join loop Err arm: clear slot on panic
    match h.join() {
        Ok(_) => panic!("expected panic in worker"),
        Err(_) => clear_activity(&activity, name),
    }

    assert!(
        activity.lock().unwrap().is_empty(),
        "activity slot must be cleared after worker panic"
    );
}
