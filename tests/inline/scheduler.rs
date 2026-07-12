use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::lockorder::RankedMutex;
use crate::oauth::RefreshError;

use crate::profile::DEFAULT_REFRESH_INTERVAL_MS as REFRESH_INTERVAL_MS;

use super::{
    ActivityStore, EpochMs, LastFetchedAt, ProfileActivity, SuppressedGenericStore,
    ThirdPartyEntry, TokenEntry, clear_activity, clear_orphaned_forced, filter_suppressed,
    mark_activity, partition_due, window_lapsed,
};

fn token(name: &str) -> TokenEntry {
    TokenEntry {
        name: name.to_string(),
        access_token: "access".to_string(),
        refresh_token: Some("refresh".to_string()),
        auto_start: false,
        access_expires_at: None,
        auth_broken: false,
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
    let (due, next) = partition_due(
        &snapshot,
        base,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert_eq!(due.len(), 1, "a never-fetched profile is due");
    assert_eq!(next.get("a").copied(), Some(REFRESH_INTERVAL_MS));

    // Just fetched: not due one ms later.
    last_fetched
        .lock()
        .unwrap()
        .insert("a".to_string(), EpochMs::from_millis(base));
    let (due, next) = partition_due(
        &snapshot,
        base + 1,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert!(due.is_empty(), "not due one ms after a fetch");
    assert_eq!(next.get("a").copied(), Some(base + REFRESH_INTERVAL_MS));

    // Exactly one interval later: due again.
    let (due, _) = partition_due(
        &snapshot,
        base + REFRESH_INTERVAL_MS,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert_eq!(due.len(), 1, "due once the fixed interval has elapsed");
}

/// Profiles mid-refresh are excluded from the due set even when their interval
/// has elapsed, but their countdown still publishes so the UI shows when they
/// become eligible again.
#[test]
fn partition_due_excludes_refreshing() {
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let snapshot = vec![token("a")];

    mark_activity(&activity, "a", ProfileActivity::Refreshing);

    let (due, next) = partition_due(
        &snapshot,
        REFRESH_INTERVAL_MS + 1,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert!(due.is_empty(), "refreshing profiles are excluded from due");
    assert!(
        next.contains_key("a"),
        "countdown still publishes for excluded profiles"
    );
}

/// A profile whose switch gate is in flight (`Switching`) is excluded like a
/// `Refreshing` one: a fetch worker would re-mark it `Queued`/`Fetching`,
/// overwriting the pending-switch mark that `switch_gate_in_flight` keys on.
#[test]
fn partition_due_excludes_switching() {
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let snapshot = vec![token("a")];

    mark_activity(&activity, "a", ProfileActivity::Switching);

    let (due, next) = partition_due(
        &snapshot,
        REFRESH_INTERVAL_MS + 1,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert!(due.is_empty(), "mid-switch profiles are excluded from due");
    assert!(
        next.contains_key("a"),
        "countdown still publishes for excluded profiles"
    );
}

/// A quarantined (`auth_broken`) profile's poll spends a guaranteed-dead
/// 401 → refresh → 400 pair against the token endpoint, so partition widens
/// its cadence by `AUTH_BROKEN_BACKOFF_MS` — computed from the live flag,
/// never baked into the `last_fetched` stamp, so any flag lift (login, adopt,
/// carry) snaps the cadence back on the very next tick.
#[test]
fn partition_due_defers_flagged_profiles_until_the_flag_lifts() {
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let base = 1_700_000_000_000u64;
    last_fetched
        .lock()
        .unwrap()
        .insert("a".to_string(), EpochMs::from_millis(base));

    let mut flagged = token("a");
    flagged.auth_broken = true;
    let snapshot = vec![flagged];

    // One interval elapsed: an unflagged profile would be due here.
    let at_interval = base + REFRESH_INTERVAL_MS + 1;
    let (due, next) = partition_due(
        &snapshot,
        at_interval,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert!(due.is_empty(), "flagged profile skips the plain cadence");
    assert_eq!(
        next["a"],
        base + REFRESH_INTERVAL_MS + super::AUTH_BROKEN_BACKOFF_MS,
        "published countdown shows the widened deadline"
    );

    // Past the widened deadline it still polls — the poll's own refresh
    // attempt stays a (slow) recovery path.
    let (due, _) = partition_due(
        &snapshot,
        base + REFRESH_INTERVAL_MS + super::AUTH_BROKEN_BACKOFF_MS,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert_eq!(
        due.len(),
        1,
        "a flagged profile still polls after the backoff"
    );

    // Same stamp, flag lifted: due immediately on the plain cadence.
    let unflagged = vec![token("a")];
    let (due, next) = partition_due(
        &unflagged,
        at_interval,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert_eq!(
        due.len(),
        1,
        "an unflagged profile snaps back to the cadence"
    );
    assert_eq!(next["a"], base + REFRESH_INTERVAL_MS);
}

/// Forced (manual `r`) refetches skip a mid-switch profile for the same
/// reason: scheduling it would overwrite the `Switching` mark and drop the
/// in-flight switch pending state.
#[test]
fn merge_forced_skips_switching() {
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    mark_activity(&activity, "switching", ProfileActivity::Switching);

    let snapshot = vec![token("switching"), token("plain")];
    let forced: HashSet<String> = ["switching", "plain"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut due: Vec<TokenEntry> = Vec::new();
    let mut next: HashMap<String, u64> = HashMap::new();

    super::merge_forced(&snapshot, &forced, &mut due, &mut next, &activity, 1);

    assert_eq!(due.len(), 1, "only the unowned profile is scheduled");
    assert_eq!(due[0].name, "plain");
}

/// Entering the rotation leg through the clock-expired-429 unmask must not
/// cost the endpoint-level backoff when the refresh can't complete: the bail
/// keeps `RateLimited` plus the server hint, while a 401-entered bail stays
/// `Cached`.
#[test]
fn failed_unmask_bail_keeps_the_429_context() {
    use std::time::Duration;

    use super::{FetchStatus, rotation_bail_context};

    // 429-entered with a server hint: both survive the failed refresh.
    let (status, retry_after) = rotation_bail_context(Some(Some(Duration::from_secs(30))));
    assert_eq!(status, FetchStatus::RateLimited);
    assert_eq!(retry_after, Some(Duration::from_secs(30)));

    // 429-entered without a hint: still RateLimited so the no-hint ladder runs.
    let (status, retry_after) = rotation_bail_context(Some(None));
    assert_eq!(status, FetchStatus::RateLimited);
    assert_eq!(retry_after, None);

    // 401-entered: plain cached bail, no phantom rate limit.
    let (status, retry_after) = rotation_bail_context(None);
    assert_eq!(status, FetchStatus::Cached);
    assert_eq!(retry_after, None);
}

/// The unmask-bail outcome drives the same deferral + streak accounting as a
/// plain 429: the next slot lands on `now + retry_after` and the consecutive
/// count survives the failed refresh attempt.
#[test]
fn failed_unmask_outcome_defers_and_streaks_like_a_429() {
    use std::time::Duration;

    use super::{
        FetchOutcome, StatusStore, apply_outcome, now_ms, partition_due, rotation_bail_context,
    };

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let statuses: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::RateLimitStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    let (status, retry_after) = rotation_bail_context(Some(Some(Duration::from_secs(300))));
    let outcome = FetchOutcome {
        name: "u".to_string(),
        info: None,
        status,
        rotated: None,
        from_fetch: false,
        retry_after,
    };

    let before = now_ms();
    apply_outcome(
        outcome,
        &store,
        &statuses,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
    );
    let after = now_ms();

    assert_eq!(
        streaks.lock().unwrap().get("u").copied(),
        Some(1),
        "the failed unmask still counts toward the 429 streak"
    );

    let extra = 300_000 - REFRESH_INTERVAL_MS;
    let stamp = last_fetched
        .lock()
        .unwrap()
        .get("u")
        .copied()
        .expect("stamp present")
        .as_millis();
    assert!(
        (before + extra..=after + extra).contains(&stamp),
        "deferred stamp must sit retry_after - interval ahead of now"
    );

    // partition_due honors the deferral end to end.
    let snapshot = vec![token("u")];
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let (due, _) = partition_due(
        &snapshot,
        stamp + REFRESH_INTERVAL_MS - 1,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert!(due.is_empty(), "not due before the deferred slot");
    let (due, _) = partition_due(
        &snapshot,
        stamp + REFRESH_INTERVAL_MS,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert_eq!(due.len(), 1, "due once the deferred slot arrives");
}

/// A forced refetch marks `Queued`; if no leg schedules that name this tick (its
/// profile vanished from both snapshots), the orphan sweep clears it so the
/// spinner can't freeze — but a name that IS scheduled, and one mid-`Refreshing`,
/// are both left alone.
#[test]
fn orphaned_forced_cleared_but_scheduled_and_refreshing_kept() {
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    mark_activity(&activity, "orphan", ProfileActivity::Queued);
    mark_activity(&activity, "scheduled", ProfileActivity::Queued);
    mark_activity(&activity, "rotating", ProfileActivity::Refreshing);

    let forced: HashSet<String> = ["orphan", "scheduled", "rotating"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let scheduled: HashSet<String> = ["scheduled"].iter().map(|s| s.to_string()).collect();

    clear_orphaned_forced(&activity, &forced, &scheduled);

    let a = activity.lock().unwrap();
    assert!(!a.contains_key("orphan"), "orphaned forced name is cleared");
    assert_eq!(
        a.get("scheduled").copied(),
        Some(ProfileActivity::Queued),
        "a scheduled name keeps its mark"
    );
    assert_eq!(
        a.get("rotating").copied(),
        Some(ProfileActivity::Refreshing),
        "a refreshing name is owned by the rotate worker, left alone"
    );
}

// ── Panic-clear discipline ────────────────────────────────────────────────────

/// The scheduler tick's mark/join/clear discipline must clear the ActivityStore
/// slot even when a fetch worker panics — exercises the `Err(_)` arm of
/// `h.join()` without real HTTP or a full scheduler.
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

/// A disk-cache fallback (`from_fetch: false`) must not clobber a newer store
/// entry: while `/usage` rate-limits, every tick recycles the stale on-disk
/// snapshot, and treating it as fresh froze the UI + auto-start scan on
/// pre-kick windowless data. Regression for the RateLimited-masking bug.
#[test]
fn cached_fallback_does_not_clobber_store() {
    use super::{FetchOutcome, FetchStatus, StatusStore, apply_outcome};
    use crate::usage::{UsageInfo, UsageWindow};

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::RateLimitStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    let live = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 1.0,
            resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
        }),
        ..Default::default()
    };
    store.lock().unwrap().insert("a".to_string(), live);

    let stale_windowless = UsageInfo::default();
    apply_outcome(
        FetchOutcome {
            name: "a".to_string(),
            info: Some(stale_windowless.clone()),
            status: FetchStatus::RateLimited,
            rotated: None,
            from_fetch: false,
            retry_after: None,
        },
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
    );
    assert!(
        store.lock().unwrap().get("a").unwrap().five_hour.is_some(),
        "a cache fallback must not overwrite a newer store entry"
    );
    assert_eq!(
        status.lock().unwrap().get("a").copied(),
        Some(FetchStatus::RateLimited),
        "the RateLimited status still surfaces"
    );

    // Cold start: the same fallback DOES fill an absent entry.
    apply_outcome(
        FetchOutcome {
            name: "b".to_string(),
            info: Some(stale_windowless),
            status: FetchStatus::Cached,
            rotated: None,
            from_fetch: false,
            retry_after: None,
        },
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
    );
    assert!(
        store.lock().unwrap().contains_key("b"),
        "a cache fallback still cold-fills an absent entry"
    );
}

/// `mark_window_open` synthesizes a live 5h window after a successful kick
/// (the kick's 200 IS the window opening; /usage may 429 for minutes), but
/// never touches a window that is already live.
#[test]
fn mark_window_open_synthesizes_only_when_not_live() {
    use super::mark_window_open;
    use crate::usage::{UsageInfo, UsageWindow, iso_to_epoch_secs};

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let now = 1_780_000_000i64;

    // Absent entry → synthetic window resets now + 5h.
    mark_window_open(&store, "a", now);
    let resets = store.lock().unwrap()["a"]
        .five_hour
        .as_ref()
        .and_then(|w| w.resets_at.as_deref())
        .and_then(iso_to_epoch_secs);
    assert_eq!(
        resets,
        Some(now + 5 * 3600),
        "synthetic window opens at +5h"
    );

    // Live window → untouched (kick into a live window must not extend it).
    let live_resets = "2999-01-01T00:00:00+00:00";
    store.lock().unwrap().insert(
        "b".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 42.0,
                resets_at: Some(live_resets.to_string()),
            }),
            ..Default::default()
        },
    );
    mark_window_open(&store, "b", now);
    let kept = store.lock().unwrap()["b"].five_hour.clone().unwrap();
    assert_eq!(kept.resets_at.as_deref(), Some(live_resets));
    assert_eq!(kept.utilization, 42.0);

    // Expired window → replaced by a fresh synthetic one.
    store.lock().unwrap().insert(
        "c".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 88.0,
                resets_at: Some("2020-01-01T00:00:00+00:00".to_string()),
            }),
            ..Default::default()
        },
    );
    mark_window_open(&store, "c", now);
    let replaced = store.lock().unwrap()["c"].five_hour.clone().unwrap();
    assert_eq!(
        replaced.resets_at.as_deref().and_then(iso_to_epoch_secs),
        Some(now + 5 * 3600)
    );
    assert_eq!(replaced.utilization, 0.0, "fresh window starts at zero");
}

/// `window_lapsed` gates the auto-start kick: an absent store entry (never
/// fetched this run) is NOT lapsed — fetch first, kick next tick — while a
/// fetched entry with no 5h window or a past `resets_at` IS lapsed, and a future
/// `resets_at` is live.
#[test]
fn window_lapsed_only_fires_on_a_fetched_expired_window() {
    use super::UsageStore;
    use crate::usage::{UsageInfo, UsageWindow};

    let store: UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let now = 1_780_000_000i64;

    // Never fetched (absent) → not lapsed: fetch first.
    assert!(
        !window_lapsed(&store, "a", now),
        "an absent entry must not kick — fetch first, kick next tick"
    );

    // Fetched, no 5h window present → lapsed.
    store
        .lock()
        .unwrap()
        .insert("a".to_string(), UsageInfo::default());
    assert!(
        window_lapsed(&store, "a", now),
        "a fetched entry with no live window is lapsed"
    );

    // Past resets_at → lapsed.
    store.lock().unwrap().insert(
        "a".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 0.0,
                resets_at: Some("2020-01-01T00:00:00+00:00".to_string()),
            }),
            ..Default::default()
        },
    );
    assert!(
        window_lapsed(&store, "a", now),
        "a past resets_at is lapsed"
    );

    // Future resets_at → live, not lapsed.
    store.lock().unwrap().insert(
        "a".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 0.0,
                resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
            }),
            ..Default::default()
        },
    );
    assert!(
        !window_lapsed(&store, "a", now),
        "a future resets_at is a live window — no kick"
    );
}

/// The auto-start kick only fires on a lapsed window when no 429 streak is in
/// flight. Mid-streak the kick is suppressed so it can't re-hit (and prolong) a
/// throttled endpoint on every due slot; a live `/usage` body clears the streak
/// and the next lapsed tick opens cleanly.
#[test]
fn kick_suppressed_during_rate_limit_streak() {
    use super::should_open_window;

    assert!(should_open_window(0, true), "lapsed + no streak → open");
    assert!(
        !should_open_window(1, true),
        "lapsed but 429-streaking → suppress the kick"
    );
    assert!(
        !should_open_window(5, true),
        "deep streak → still suppressed"
    );
    assert!(
        !should_open_window(0, false),
        "a live window never kicks, streak or not"
    );
}

/// Auto-switch and recovery decisions act only on a confirmed-live (`Fresh`)
/// read. A `Cached` window may have rolled over and a `RateLimited` one may be a
/// synthetic just-kicked 0% — both must be treated as undecidable, as must a
/// profile with no read yet.
#[test]
fn only_a_fresh_read_drives_a_switch_decision() {
    use super::{FetchStatus, StatusStore, decision_fresh};

    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    {
        let mut s = status.lock().unwrap();
        s.insert("fresh".to_string(), FetchStatus::Fresh);
        s.insert("cached".to_string(), FetchStatus::Cached);
        s.insert("limited".to_string(), FetchStatus::RateLimited);
        s.insert("failed".to_string(), FetchStatus::Failed);
    }

    assert!(decision_fresh(&status, "fresh"));
    assert!(
        !decision_fresh(&status, "cached"),
        "a possibly rolled-over cached window must not drive a switch"
    );
    assert!(
        !decision_fresh(&status, "limited"),
        "a synthetic rate-limited window must not drive a switch"
    );
    assert!(!decision_fresh(&status, "failed"));
    assert!(
        !decision_fresh(&status, "absent"),
        "no read yet → no decision"
    );
}

/// AUTH-4: `scan_auto_switch` bypasses the freshness gate for an auth-broken
/// active — its reads can never be `Fresh` again (the login is dead), so
/// requiring one froze the scan forever and wedged the daemon on the dead
/// account while a viable sibling idled (observed live 2026-07-09). A healthy
/// active keeps the gate: the same frozen store state must NOT drive a switch
/// when the account is merely stale, only when it is confirmed dead.
#[test]
fn scan_auto_switch_walks_off_a_broken_active_without_a_fresh_read() {
    use super::{FetchStatus, PendingSwitch, PendingSwitchOff, StatusStore, scan_auto_switch};
    use crate::profile::{AppConfig, AppState, Profile};
    use crate::usage::{UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso, now_epoch_secs};

    let frozen_state = || {
        // The wedge's exact shape: the active's last-ever read is maxed on a
        // window that has since lapsed (reads as idle headroom), status stuck
        // on RateLimited; the sibling is genuinely viable and Fresh.
        let store: UsageStore = Arc::new(RankedMutex::new(HashMap::from([
            (
                "a".to_string(),
                UsageInfo {
                    five_hour: Some(UsageWindow {
                        utilization: 100.0,
                        resets_at: Some(epoch_secs_to_iso(now_epoch_secs() - 3600)),
                    }),
                    ..Default::default()
                },
            ),
            (
                "b".to_string(),
                UsageInfo {
                    five_hour: Some(UsageWindow {
                        utilization: 10.0,
                        resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
                    }),
                    ..Default::default()
                },
            ),
        ])));
        let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([
            ("a".to_string(), FetchStatus::RateLimited),
            ("b".to_string(), FetchStatus::Fresh),
        ])));
        let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
        let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        let pending_off: PendingSwitchOff = Arc::new(RankedMutex::new(false));
        (store, status, activity, pending, pending_off)
    };
    let config_handle = |broken: bool| -> crate::profile::ConfigHandle {
        let mut cfg = AppConfig {
            state: AppState {
                active_profile: Some("a".into()),
                profiles: vec!["a".into(), "b".into()],
                fallback_chain: vec!["a".into(), "b".into()],
                ..AppState::default()
            },
            profiles: vec![
                Profile::new("a".to_string(), None, None),
                Profile::new("b".to_string(), None, None),
            ],
        };
        cfg.set_auth_broken("a", broken);
        Arc::new(RankedMutex::new(cfg))
    };

    // Broken active → the gate is bypassed and the walk queues the sibling.
    let (store, status, activity, pending, pending_off) = frozen_state();
    scan_auto_switch(
        &config_handle(true),
        &store,
        &status,
        &activity,
        &pending,
        &pending_off,
    );
    assert!(
        pending.lock().unwrap().contains("b"),
        "a dead active must be walked away from without waiting for a Fresh read"
    );

    // Healthy active, identical frozen stores → the freshness gate holds.
    let (store, status, activity, pending, pending_off) = frozen_state();
    scan_auto_switch(
        &config_handle(false),
        &store,
        &status,
        &activity,
        &pending,
        &pending_off,
    );
    assert!(
        pending.lock().unwrap().is_empty(),
        "a merely-stale healthy active must still not drive a switch"
    );
}

/// A Fresh `/usage` body fetched in the same tick as a kick can lag the
/// just-opened window and still report it closed; `preserve_live_window` keeps
/// the live window we already hold so it can't re-lapse and re-fire the kick.
/// A body that already carries a live window, or has no live predecessor, is
/// passed through untouched.
#[test]
fn fresh_body_lagging_a_kick_keeps_the_live_window() {
    use super::{five_hour_live, preserve_live_window};
    use crate::usage::{UsageInfo, UsageWindow};

    let now = 1_600_000_000i64; // 2020 — between the two reset stamps below
    let win = |util: f64, resets: &str| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: util,
            resets_at: Some(resets.to_string()),
        }),
        ..Default::default()
    };
    let live = |u| win(u, "2999-01-01T00:00:00+00:00");
    let closed = |u| win(u, "2000-01-01T00:00:00+00:00");

    // Lagging fresh body (closed window) over a just-opened live one → keep live.
    let merged = preserve_live_window(closed(80.0), Some(&live(0.0)), now);
    assert!(
        five_hour_live(&merged, now),
        "a lagging fresh body must not re-close a just-opened window"
    );
    assert_eq!(
        merged.five_hour.unwrap().utilization,
        0.0,
        "keeps the live window verbatim"
    );

    // Fresh body already carries a live window → take it as-is.
    let merged = preserve_live_window(live(12.0), Some(&live(0.0)), now);
    assert_eq!(merged.five_hour.unwrap().utilization, 12.0);

    // Prior window also closed → nothing live to preserve; the fresh body stands.
    let merged = preserve_live_window(closed(80.0), Some(&closed(50.0)), now);
    assert!(!five_hour_live(&merged, now));

    // No prior entry at all → fresh body stands.
    let merged = preserve_live_window(closed(80.0), None, now);
    assert_eq!(merged.five_hour.unwrap().utilization, 80.0);
}

/// A 429's `retry-after` hint defers the profile's next fetch slot: the
/// `last_fetched` stamp lands `retry_after - interval` in the future so
/// `partition_due` marks the profile due (and publishes its countdown) exactly
/// at `now + retry_after`. A 429 with no hint adds a flat 10s beyond the
/// cadence; a zero or sub-interval hint keeps the cadence; an absurd hint clamps
/// to the ceiling.
#[test]
fn retry_after_defers_next_fetch_slot() {
    use std::time::Duration;

    use super::{
        FetchOutcome, FetchStatus, MAX_RETRY_AFTER_MS, RATE_LIMIT_MIN_BACKOFF_MS, StatusStore,
        apply_outcome, now_ms,
    };

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::RateLimitStreaks = Arc::new(RankedMutex::new(HashMap::new()));
    let outcome = |name: &str, retry_after: Option<Duration>| FetchOutcome {
        name: name.to_string(),
        info: None,
        status: FetchStatus::RateLimited,
        rotated: None,
        from_fetch: false,
        retry_after,
    };
    let stamp = |name: &str| {
        last_fetched
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .expect("stamp present")
            .as_millis()
    };

    // retry-after 300s → stamp ≈ now + (300s - interval).
    let before = now_ms();
    apply_outcome(
        outcome("a", Some(Duration::from_secs(300))),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
    );
    let after = now_ms();
    let extra = 300_000 - REFRESH_INTERVAL_MS;
    let a = stamp("a");
    assert!(
        (before + extra..=after + extra).contains(&a),
        "deferred stamp must sit retry_after - interval ahead of now"
    );
    // partition_due: not due just before now + retry_after, due at it.
    let snapshot = vec![token("a")];
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let (due, next) = partition_due(
        &snapshot,
        a + REFRESH_INTERVAL_MS - 1,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert!(due.is_empty(), "not due before the deferred slot");
    assert_eq!(
        next.get("a").copied(),
        Some(a + REFRESH_INTERVAL_MS),
        "countdown publishes the deferred slot"
    );
    let (due, _) = partition_due(
        &snapshot,
        a + REFRESH_INTERVAL_MS,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
    );
    assert_eq!(due.len(), 1, "due once the deferred slot arrives");

    // No hint → flat 10s backoff beyond the cadence.
    let before = now_ms();
    apply_outcome(
        outcome("b", None),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
    );
    let after = now_ms();
    let floor = RATE_LIMIT_MIN_BACKOFF_MS;
    assert!(
        (before + floor..=after + floor).contains(&stamp("b")),
        "a 429 with no retry-after defers a flat 10s past now"
    );

    // Hint shorter than the ladder → the ladder wins (max, never suppressed).
    let before = now_ms();
    apply_outcome(
        outcome("c", Some(Duration::from_secs(5))),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
    );
    let after = now_ms();
    assert!(
        (before + floor..=after + floor).contains(&stamp("c")),
        "a sub-cadence hint cannot undercut the streak ladder"
    );

    // Absurd hint → clamped to the ceiling.
    let before = now_ms();
    apply_outcome(
        outcome("d", Some(Duration::from_secs(86_400))),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
    );
    let after = now_ms();
    let capped = MAX_RETRY_AFTER_MS - REFRESH_INTERVAL_MS;
    assert!(
        (before + capped..=after + capped).contains(&stamp("d")),
        "huge retry-after clamps to MAX_RETRY_AFTER_MS"
    );

    // Explicit `retry-after: 0` rides the SAME ladder as a missing header.
    // The usage endpoint answers every 429 with `retry-after: 0` while its
    // sliding window counts the rejected requests too — honoring the "retry
    // now" verbatim re-polls at cadence and pins the window full forever
    // (observed 2026-07-11: hours of uninterrupted per-account 429s).
    let before = now_ms();
    apply_outcome(
        outcome("e", Some(Duration::ZERO)),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
    );
    let after = now_ms();
    assert!(
        (before + floor..=after + floor).contains(&stamp("e")),
        "a zero retry-after must not suppress the backoff ladder"
    );
}

/// Consecutive 429s with no `retry-after` back off exponentially (10s → 30s →
/// 90s past now), and a live fetch resets the streak so the next 429 starts at
/// the base again.
#[test]
fn consecutive_rate_limits_back_off_exponentially() {
    use super::{
        FetchOutcome, FetchStatus, RATE_LIMIT_BACKOFF_FACTOR, RATE_LIMIT_MIN_BACKOFF_MS,
        StatusStore, apply_outcome, now_ms,
    };

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::RateLimitStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    let rate_limited = |from_fetch: bool, status: FetchStatus| FetchOutcome {
        name: "a".to_string(),
        info: None,
        status,
        rotated: None,
        from_fetch,
        retry_after: None,
    };
    let stamp = || {
        last_fetched
            .lock()
            .unwrap()
            .get("a")
            .copied()
            .expect("stamp present")
            .as_millis()
    };

    // No retry-after: each consecutive 429 lands the slot one interval + a
    // growing backoff out, i.e. the stamp sits `base * factor^(n-1)` past now.
    // Derived from the constants so retuning the factor can't leave it stale.
    let base = RATE_LIMIT_MIN_BACKOFF_MS;
    let f = RATE_LIMIT_BACKOFF_FACTOR;
    for expect in [base, base * f, base * f * f] {
        let before = now_ms();
        apply_outcome(
            rate_limited(false, FetchStatus::RateLimited),
            &store,
            &status,
            &last_fetched,
            &streaks,
            REFRESH_INTERVAL_MS,
        );
        let after = now_ms();
        assert!(
            (before + expect..=after + expect).contains(&stamp()),
            "consecutive 429 backs off to {expect}ms past now"
        );
    }

    // A live fetch resets the streak (info `None` so no disk write); the next
    // 429 starts at the base backoff again.
    apply_outcome(
        rate_limited(true, FetchStatus::Fresh),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
    );
    let before = now_ms();
    apply_outcome(
        rate_limited(false, FetchStatus::RateLimited),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
    );
    let after = now_ms();
    assert!(
        (before + RATE_LIMIT_MIN_BACKOFF_MS..=after + RATE_LIMIT_MIN_BACKOFF_MS).contains(&stamp()),
        "a live fetch resets the backoff streak"
    );
}

/// A transient `Cached`/`Failed` outcome between two 429s must NOT reset the
/// consecutive-429 streak — a network blip mid-storm should leave the ramp
/// climbing (base → base*factor), not drop it back to the base.
#[test]
fn transient_errors_preserve_rate_limit_streak() {
    use super::{
        FetchOutcome, FetchStatus, RATE_LIMIT_BACKOFF_FACTOR, RATE_LIMIT_MIN_BACKOFF_MS,
        StatusStore, apply_outcome, now_ms,
    };

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::RateLimitStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    let outcome = |kind: FetchStatus| FetchOutcome {
        name: "a".to_string(),
        info: None,
        status: kind,
        rotated: None,
        from_fetch: false,
        retry_after: None,
    };
    let apply = |kind: FetchStatus| {
        apply_outcome(
            outcome(kind),
            &store,
            &status,
            &last_fetched,
            &streaks,
            REFRESH_INTERVAL_MS,
        );
    };
    let stamp = || {
        last_fetched
            .lock()
            .unwrap()
            .get("a")
            .copied()
            .expect("stamp present")
            .as_millis()
    };

    // 429 (streak 1), then transient errors that must leave the streak at 1.
    apply(FetchStatus::RateLimited);
    apply(FetchStatus::Cached);
    apply(FetchStatus::Failed);

    // Next 429 → streak 2 (not reset to 1) → base * factor.
    let before = now_ms();
    apply(FetchStatus::RateLimited);
    let after = now_ms();
    let expect = RATE_LIMIT_MIN_BACKOFF_MS * RATE_LIMIT_BACKOFF_FACTOR;
    assert!(
        (before + expect..=after + expect).contains(&stamp()),
        "a Cached/Failed blip must not reset the 429 streak"
    );
}

/// Any on-disk cache seeds at startup as a starting point (store + status +
/// `last_fetched` stamped at the cache mtime so the cadence resumes), regardless of
/// 5h window state. Freshness only picks the status: younger than one interval →
/// `Fresh` (left be), older → `Cached` (refreshed in the background). A missing
/// cache is left for the scheduler.
#[test]
fn try_seed_cache_seeds_any_cache_and_resumes_timer() {
    use std::time::{Duration, SystemTime};

    use super::{FetchStatus, StatusStore, now_ms, try_seed_cache};
    use crate::profile::profile_subpath;
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::testutil::{HomeSandbox, set_mtime};
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};

    let _home = HomeSandbox::new();
    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));

    let now_secs = now_epoch_secs();
    let with_reset = |reset_secs: i64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 12.0,
            resets_at: Some(epoch_secs_to_iso(reset_secs)),
        }),
        ..Default::default()
    };

    // Fresh cache (mtime ~30s ago) whose 5h window already reset (resets_at in the
    // past) — an idle account. Younger than one interval, so seeded `Fresh`.
    write_profile_cache("idle", USAGE_CACHE_FILE, &with_reset(now_secs - 600));
    let idle_path = profile_subpath("idle", "usage_cache.json").expect("idle path");
    set_mtime(&idle_path, SystemTime::now() - Duration::from_secs(30));

    // Stale cache (written 2h ago) whose window is still open — seeded as a starting
    // point with `Cached` status; the scheduler refreshes it in the background.
    write_profile_cache("stale", USAGE_CACHE_FILE, &with_reset(now_secs + 3600));
    let stale_path = profile_subpath("stale", "usage_cache.json").expect("stale path");
    set_mtime(
        &stale_path,
        SystemTime::now() - Duration::from_secs(2 * 3600),
    );

    let now = now_ms();
    assert!(
        try_seed_cache(
            &store,
            &status,
            &last_fetched,
            "idle",
            now,
            REFRESH_INTERVAL_MS
        ),
        "a fresh cache seeds even when its 5h window has reset (idle account)"
    );
    assert!(
        try_seed_cache(
            &store,
            &status,
            &last_fetched,
            "stale",
            now,
            REFRESH_INTERVAL_MS
        ),
        "a cache older than one interval is still seeded as a Cached starting point"
    );
    assert!(
        !try_seed_cache(
            &store,
            &status,
            &last_fetched,
            "missing",
            now,
            REFRESH_INTERVAL_MS
        ),
        "a missing cache is left for the background fetch"
    );

    assert!(store.lock().unwrap().contains_key("idle"));
    assert!(store.lock().unwrap().contains_key("stale"));
    assert!(!store.lock().unwrap().contains_key("missing"));
    assert_eq!(
        status.lock().unwrap().get("idle").copied(),
        Some(FetchStatus::Fresh),
        "a cache younger than one interval is Fresh",
    );
    assert_eq!(
        status.lock().unwrap().get("stale").copied(),
        Some(FetchStatus::Cached),
        "a cache older than one interval is Cached",
    );

    // Stamped at the ~30s-old cache mtime, not `now` — so `partition_due` resumes
    // the cadence (next ≈ mtime + interval, ~30s short of full) instead of
    // resetting the countdown.
    let stamp = last_fetched
        .lock()
        .unwrap()
        .get("idle")
        .copied()
        .unwrap()
        .as_millis();
    assert!(
        stamp <= now.saturating_sub(20_000) && stamp >= now.saturating_sub(40_000),
        "stamped at the ~30s-old cache mtime (resume), not now"
    );
}

/// `deadline_spread` separates profiles' fetch deadlines so they don't fall due
/// on the same tick: bounded to `[0, interval/4)`, deterministic for a fixed
/// `(name, now)`, varied across profiles and across cycles, and zero on a
/// degenerate interval (no modulo-by-zero).
#[test]
fn deadline_spread_is_bounded_per_profile_and_per_cycle() {
    use super::deadline_spread;

    let interval = REFRESH_INTERVAL_MS;
    let span = interval / 4;
    let now = EpochMs::from_millis(1_700_000_000_000);
    let sp = |name: &str, t: EpochMs| deadline_spread(name, t, interval).0;

    // Bounded and deterministic.
    assert!(sp("alpha", now) < span, "spread stays under interval/4");
    assert_eq!(
        sp("alpha", now),
        sp("alpha", now),
        "deterministic per (name, now)"
    );

    // Varies across profiles (8 distinct names can't all collide).
    let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
    let by_name: Vec<u64> = names.iter().map(|n| sp(n, now)).collect();
    assert!(
        by_name.iter().any(|&s| s != by_name[0]),
        "distinct profiles get distinct phase offsets"
    );

    // Re-rolls per cycle (different `now` for the same name).
    let by_cycle: Vec<u64> = (0..8)
        .map(|i| sp("alpha", EpochMs::from_millis(1_700_000_000_000 + i * 7_000)))
        .collect();
    assert!(
        by_cycle.iter().any(|&s| s != by_cycle[0]),
        "the jitter re-rolls as the cycle advances"
    );

    // Degenerate interval → no spread.
    assert_eq!(deadline_spread("alpha", now, 0).0, 0);
}

/// `filter_suppressed` drops third-party entries whose name is in the session
/// suppressed set and passes the rest through in order; an empty set (the steady
/// state for healthy profiles) is a no-op fast path.
#[test]
fn filter_suppressed_drops_only_named_entries() {
    let suppressed: SuppressedGenericStore = Arc::new(RankedMutex::new(HashSet::new()));
    suppressed.lock().unwrap().insert("no-data".to_string());

    let snap = vec![tp_entry("ok"), tp_entry("no-data"), tp_entry("also-ok")];
    let out = filter_suppressed(&suppressed, snap);
    let names: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["ok", "also-ok"]);

    // Empty set → identity (the fast path).
    let empty: SuppressedGenericStore = Arc::new(RankedMutex::new(HashSet::new()));
    let snap2 = vec![tp_entry("ok"), tp_entry("no-data")];
    assert_eq!(filter_suppressed(&empty, snap2).len(), 2);
}

fn tp_entry(name: &str) -> ThirdPartyEntry {
    ThirdPartyEntry {
        name: name.to_string(),
        target: crate::providers::ThirdPartyTarget::Generic {
            base_url: "https://example.com".to_string(),
        },
        api_key: "key".to_string(),
    }
}

/// Third-party startup seed mirrors the OAuth one: any cached profile is seeded
/// with `last_fetched` stamped at the cache mtime (cadence resumes) — `Fresh` when
/// younger than one interval, `Cached` when older (refreshed in the background). A
/// missing cache is left for the scheduler.
#[test]
fn bootstrap_third_party_seeds_any_cache() {
    use std::time::{Duration, SystemTime};

    use super::{
        FetchStatus, ThirdPartyStatusStore, ThirdPartyUsageStore, bootstrap_third_party, now_ms,
    };
    use crate::profile::profile_subpath;
    use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, write_profile_cache};
    use crate::providers::{ThirdPartyStats, UsageBar};
    use crate::testutil::{HomeSandbox, set_mtime};

    let _home = HomeSandbox::new();
    let store: ThirdPartyUsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));

    let stats = |pct: f64| ThirdPartyStats {
        is_available: true,
        rows: Vec::new(),
        bars: vec![UsageBar {
            label: "5h".to_string(),
            pct,
            resets_at: None,
            used: None,
            total: None,
        }],
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    // Fresh cache (just written) seeds `Fresh`; a 2h-old cache seeds `Cached`.
    write_profile_cache("cached", THIRD_PARTY_CACHE_FILE, &stats(12.0));
    write_profile_cache("stale", THIRD_PARTY_CACHE_FILE, &stats(20.0));
    let stale_path = profile_subpath("stale", "third_party_cache.json").expect("stale path");
    set_mtime(
        &stale_path,
        SystemTime::now() - Duration::from_secs(2 * 3600),
    );

    let entries = vec![tp_entry("cached"), tp_entry("stale"), tp_entry("missing")];
    bootstrap_third_party(
        &store,
        &status,
        &last_fetched,
        &entries,
        REFRESH_INTERVAL_MS,
    );

    assert!(
        store.lock().unwrap().contains_key("cached"),
        "a fresh third-party cache is seeded from disk"
    );
    assert!(
        store.lock().unwrap().contains_key("stale"),
        "a stale third-party cache is still seeded as a Cached starting point"
    );
    assert!(
        !store.lock().unwrap().contains_key("missing"),
        "a profile with no cache is left for the scheduler"
    );
    assert_eq!(
        status.lock().unwrap().get("cached").copied(),
        Some(FetchStatus::Fresh),
        "a third-party cache younger than one interval surfaces as Fresh"
    );
    assert_eq!(
        status.lock().unwrap().get("stale").copied(),
        Some(FetchStatus::Cached),
        "a third-party cache older than one interval surfaces as Cached"
    );
    assert!(
        !last_fetched.lock().unwrap().contains_key("missing"),
        "a no-cache profile is left unstamped so it fetches on the first tick"
    );
    // Stamped at the cache mtime (~now, just written), so the cadence resumes.
    let now = now_ms();
    let stamp = last_fetched
        .lock()
        .unwrap()
        .get("cached")
        .copied()
        .unwrap()
        .as_millis();
    assert!(
        stamp <= now && stamp >= now.saturating_sub(5_000),
        "the seeded third-party profile stamps last_fetched at the cache mtime"
    );
}

// ── AUTH-1: proactive auth-health during the usage poll ──────────────────────
// `refresh_failure_is_terminal` decides whether a poll-time refresh failure means
// the OAuth login DROPPED (quarantine the account now) or is a transient blip
// (leave the flag, retry). This is the classification behind the account surfacing
// "needs reauth" on the tick the drop is detected, not only on the next switch.

#[test]
fn dead_refresh_token_is_terminal() {
    // A 4xx from the token endpoint (revoked / expired refresh token) → the login
    // is gone; quarantine so the UI surfaces reauth immediately.
    let err = RefreshError::Invalid("HTTP 400: invalid_grant".to_string());
    assert!(super::refresh_failure_is_terminal(&err));
}

#[test]
fn transient_refresh_failure_is_not_terminal() {
    // A network / 5xx / parse blip must NOT quarantine — the token may be fine; the
    // fixed cadence retries next tick.
    let err = RefreshError::Transient(anyhow::anyhow!("connection reset by peer"));
    assert!(!super::refresh_failure_is_terminal(&err));
}

// `fresher_disk_pair` is the double-spend guard in front of the quarantine: a
// terminal 400 is also what a benign single-use double-spend returns (Claude
// Code refreshing the active profile's symlinked credentials mid-poll, or a
// refresher that completed before this tick's guard was acquired). Only an
// UNCHANGED on-disk pair proves a real revocation.

#[test]
fn a_disk_pair_that_moved_past_the_spent_token_is_returned_not_quarantined() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = "double-spend-benign";
    let mut p = crate::profile::Profile::new(name.to_string(), None, None);
    p.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-new".into(),
            refresh_token: Some("rt-new".into()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&p).expect("save profile");

    // We spent "rt-old"; the store moved to "rt-new" — someone else rotated.
    assert_eq!(
        super::fresher_disk_pair(name, "rt-old"),
        Some(("at-new".to_string(), Some("rt-new".to_string())))
    );
    // We spent "rt-new" itself and it 400d — a real revocation, quarantine.
    assert_eq!(super::fresher_disk_pair(name, "rt-new"), None);
}

/// The carry path must also LIFT a stale quarantine: the moved pair proves the
/// chain is alive, and without the clear, an account recovered by an external
/// re-login stays excluded from the fallback walk and refused by every switch
/// gate forever (its own refresh never succeeds — the carry preempts it).
#[test]
fn carrying_an_external_rotation_clears_a_stale_quarantine() {
    use crate::lockorder::RankedMutex;
    use std::sync::Arc;

    let _home = crate::testutil::HomeSandbox::new();
    let name = "double-spend-quarantined";
    let mut p = crate::profile::Profile::new(name.to_string(), None, None);
    p.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-new".into(),
            refresh_token: Some("rt-new".into()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&p).expect("save profile");

    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![p],
    };
    config.state.profiles = vec![name.into()];
    config.set_auth_broken(name, true);
    let handle: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(config));
    let refetch: super::RefetchQueue = Arc::new(RankedMutex::new(Default::default()));

    // Spent "rt-old"; store holds "rt-new" → carry fires and lifts the flag.
    let outcome = super::carry_external_rotation(&handle, name, "rt-old", &refetch);
    assert!(outcome.is_some(), "a moved pair must carry");
    assert!(
        !handle.lock().unwrap().is_auth_broken(name),
        "the carried (alive) chain must lift a stale quarantine"
    );
    assert!(
        refetch.lock().unwrap().contains(name),
        "the carried pair is refetched next tick"
    );

    // Spent the store's own pair → no carry, and the flag is left alone.
    handle.lock().unwrap().set_auth_broken(name, true);
    let outcome = super::carry_external_rotation(&handle, name, "rt-new", &refetch);
    assert!(outcome.is_none(), "an unchanged pair is a real revocation");
    assert!(
        handle.lock().unwrap().is_auth_broken(name),
        "a real revocation keeps the quarantine"
    );
}

#[test]
fn a_missing_or_tokenless_profile_never_reads_as_a_benign_double_spend() {
    let _home = crate::testutil::HomeSandbox::new();
    // No profile on disk at all.
    assert_eq!(
        super::fresher_disk_pair("double-spend-missing", "rt-x"),
        None
    );
    // Profile exists but has no stored credentials.
    let p = crate::profile::Profile::new("double-spend-bare".to_string(), None, None);
    crate::profile::save_profile(&p).expect("save profile");
    assert_eq!(super::fresher_disk_pair("double-spend-bare", "rt-x"), None);
}

// `token_clock_expired` gates whether a 429 on the usage fetch falls through to the
// refresh leg (the AUTH-1 fix so a dead login that 429s surfaces as auth_broken
// instead of being masked as RateLimited forever) vs bails to cache. Only a
// clock-EXPIRED token is worth spending the single-use refresh on.

#[test]
fn rate_limited_expired_token_rotates_so_a_dead_login_surfaces() {
    // 429 + access token expired 1s ago → rotate (a dead refresh token then flags
    // auth_broken; a live one just re-fetches). now=10_000ms, exp=9_000ms.
    assert!(super::token_clock_expired(Some(9_000), 10_000));
}

#[test]
fn rate_limited_valid_token_does_not_rotate() {
    // 429 on a still-valid token is a pure endpoint rate limit — refusing to refresh
    // protects the single-use token from being re-spent every tick. exp in the future.
    assert!(!super::token_clock_expired(Some(20_000), 10_000));
}

#[test]
fn rate_limited_unknown_expiry_does_not_rotate() {
    // No expiry known → conservative: never spend a refresh on a token we can't prove
    // is expired (matches auto_start_kick's `is_some_and` gate).
    assert!(!super::token_clock_expired(None, 10_000));
}

// `proactive_rotation_due` decides whether the ACTIVE Keychain-installed profile
// rotates AHEAD of expiry (rotation coherence, #1) instead of waiting for a
// 401. Opt-in via `AppState.preemptive_rotation` — adoption plus
// mirror-on-rotate carry the correctness; the early rotate is an optimization.

#[test]
fn preemptive_rotation_is_opt_in_and_off_by_default() {
    // Stock clauth stays strictly lazy: with the toggle off, even a token
    // deep inside the lead window (active + Keychain live) never rotates
    // ahead of expiry.
    assert!(!crate::profile::AppState::default().preemptive_rotation);
    assert!(!super::proactive_rotation_due(
        false,
        true,
        true,
        Some(10_000),
        10_000,
        90_000
    ));
}

#[test]
fn proactive_rotation_fires_only_inside_the_lead_window() {
    let interval = 90_000u64;
    let lead = super::active_rotate_lead_ms(interval);
    // At or inside the lead window → rotate now, keeping the Keychain token
    // from ever expiring under the running claude.
    assert!(super::proactive_rotation_due(
        true,
        true,
        true,
        Some(10_000 + lead),
        10_000,
        interval
    ));
    assert!(super::proactive_rotation_due(
        true,
        true,
        true,
        Some(10_000),
        10_000,
        interval
    ));
    // Beyond the lead window → plain poll; nothing at stake yet.
    assert!(!super::proactive_rotation_due(
        true,
        true,
        true,
        Some(10_000 + lead + 1),
        10_000,
        interval
    ));
}

#[test]
fn proactive_lead_scales_with_the_poll_interval_with_a_floor() {
    // The lead is derived from the cadence (3 polls' worth of rotation
    // opportunities before expiry), not a magic race margin — and it never
    // drops below the floor even on an aggressive interval.
    assert_eq!(super::active_rotate_lead_ms(90_000), 270_000);
    assert_eq!(
        super::active_rotate_lead_ms(10_000),
        super::ACTIVE_ROTATE_LEAD_FLOOR_MS
    );
}

#[test]
fn proactive_rotation_requires_active_and_keychain() {
    // Inactive profile: its chain is not the live login — reactive only.
    assert!(!super::proactive_rotation_due(
        true,
        false,
        true,
        Some(0),
        10_000,
        90_000
    ));
    // No Keychain mirror (other OSes / disabled): the symlinked profile file IS
    // the live credential — there is no second chain to race.
    assert!(!super::proactive_rotation_due(
        true,
        true,
        false,
        Some(0),
        10_000,
        90_000
    ));
}

#[test]
fn proactive_rotation_never_fires_on_unknown_expiry() {
    // Never spend a single-use refresh on a token whose expiry we can't prove.
    assert!(!super::proactive_rotation_due(
        true, true, true, None, 10_000, 90_000
    ));
}
