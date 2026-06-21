use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::lockorder::RankedMutex;

use crate::profile::DEFAULT_REFRESH_INTERVAL_MS as REFRESH_INTERVAL_MS;

use super::{
    ActivityStore, EpochMs, LastFetchedAt, ProfileActivity, SuppressedGenericStore,
    ThirdPartyEntry, TokenEntry, clear_activity, filter_suppressed, mark_activity, partition_due,
    window_lapsed,
};

fn token(name: &str) -> TokenEntry {
    TokenEntry {
        name: name.to_string(),
        access_token: "access".to_string(),
        refresh_token: Some("refresh".to_string()),
        auto_start: false,
        access_expires_at: None,
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

    // Hint shorter than the interval → no extra deferral.
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
    assert!((before..=after).contains(&stamp("c")));

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
