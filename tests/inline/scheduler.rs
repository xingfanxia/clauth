use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::lockorder::RankedMutex;
use crate::oauth::RefreshError;

use crate::profile::DEFAULT_REFRESH_INTERVAL_MS as REFRESH_INTERVAL_MS;

use super::{
    ActivityStore, EpochMs, LastFetchedAt, ProfileActivity, SuppressedGenericStore,
    ThirdPartyEntry, TokenEntry, clear_activity, clear_orphaned_forced, collect_oauth_seed_names,
    collect_third_party_entries, collect_tokens, filter_suppressed, mark_activity,
    memoized_identity, partition_due, window_lapsed,
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

/// An OAuth-credentialed profile, optionally disabled, for the
/// `collect_tokens`/`collect_third_party_entries` work-list exclusion tests.
fn oauth_profile_disabled(name: &str, disabled: bool) -> crate::profile::Profile {
    use crate::profile::{ClaudeCredentials, OAuthToken};

    let mut p = crate::profile::Profile::new(name.to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("{name}-access"),
            refresh_token: Some(format!("{name}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    p.disabled = disabled;
    p
}

// A disabled account must not enter the scheduler's per-profile work list at
// all: no polling, no rotation, no auto-start ping, no stuck-429 distrust —
// all downstream of never appearing in the OAuth `TokenEntry` snapshot.
#[test]
fn collect_tokens_excludes_disabled_profiles_includes_enabled_siblings() {
    use crate::profile::{AppConfig, AppState};

    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![
            oauth_profile_disabled("off", true),
            oauth_profile_disabled("on", false),
        ],
    };

    let entries = collect_tokens(&config);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !names.contains(&"off"),
        "a disabled account must never enter the poll/rotate work list"
    );
    assert!(
        names.contains(&"on"),
        "an enabled sibling must still be collected for polling"
    );
}

// The DISPLAY seed is the complement of `collect_tokens`: it INCLUDES a disabled
// OAuth profile (so its cached tier/windows render) — the exact hole behind the
// stale-tier bug — while the work-list above still excludes it. End-to-end: a
// disabled profile's on-disk usage cache lands in the live store via
// `bootstrap_fetch(collect_oauth_seed_names(..))`, and seeding it never widens
// the poll list. A credential-less profile has no oauth cache, so it is not seeded.
#[test]
fn collect_oauth_seed_names_includes_disabled_and_bootstrap_seeds_its_cache() {
    use crate::profile::{AppConfig, AppState};
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::{UsageInfo, UsageWindow};
    let _home = crate::testutil::HomeSandbox::new();

    let mut credless = crate::profile::Profile::new("credless".to_string(), None, None);
    credless.disabled = true;
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![
            oauth_profile_disabled("off", true),
            oauth_profile_disabled("on", false),
            credless,
        ],
    };

    let seed = collect_oauth_seed_names(&config);
    assert!(
        seed.contains(&"off".to_string()),
        "the display seed must include a disabled OAuth profile: {seed:?}"
    );
    assert!(
        seed.contains(&"on".to_string()),
        "and its enabled sibling: {seed:?}"
    );
    assert!(
        !seed.contains(&"credless".to_string()),
        "a credential-less profile has no oauth cache to seed: {seed:?}"
    );

    // End-to-end: the disabled profile's on-disk cache lands in the live store.
    let info = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 42.0,
            resets_at: None,
        }),
        ..UsageInfo::default()
    };
    write_profile_cache("off", USAGE_CACHE_FILE, &info);

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: super::StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    super::bootstrap_fetch(&store, &status, &last_fetched, &seed, REFRESH_INTERVAL_MS);

    let seeded = store.lock().unwrap().get("off").cloned();
    assert_eq!(
        seeded.and_then(|i| i.five_hour.map(|w| w.utilization)),
        Some(42.0),
        "a disabled profile's cached window is seeded for display"
    );

    // Invariant preserved: seeding the store never widens the poll work-list.
    let poll_names: Vec<String> = collect_tokens(&config)
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert!(
        !poll_names.contains(&"off".to_string()),
        "seeding a disabled profile must not make it pollable: {poll_names:?}"
    );
}

// Third-party (api-key) leg's own work list must honor the same exclusion.
#[test]
fn collect_third_party_entries_excludes_disabled_profiles_includes_enabled_siblings() {
    let mut off = crate::profile::Profile::new(
        "off".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    );
    off.disabled = true;
    let on = crate::profile::Profile::new(
        "on".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    );

    let entries = collect_third_party_entries(&[off, on]);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !names.contains(&"off"),
        "a disabled third-party account must never enter the poll work list"
    );
    assert!(
        names.contains(&"on"),
        "an enabled third-party sibling must still be collected"
    );
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
        &HashMap::new(),
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
        &HashMap::new(),
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
        &HashMap::new(),
    );
    assert_eq!(due.len(), 1, "due once the fixed interval has elapsed");
}

/// `spent_skip_set` (the `refresh_spent_accounts` OFF gate): only an unforced,
/// already-fetched, spent account is skipped. A forced (`r`) account, a never-
/// fetched one, a below-cap one, and one whose spent window has lapsed are all
/// absent from the set — the last two are how a reset gets observed.
#[test]
fn spent_skip_set_selects_only_unforced_spent_accounts() {
    use super::spent_skip_set;
    use crate::usage::{UsageInfo, UsageWindow};

    let now = 1_779_027_600i64; // 2026-05-17 UTC
    let capped = |resets: &str| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 100.0,
            resets_at: Some(resets.to_string()),
        }),
        ..Default::default()
    };
    let store: HashMap<String, UsageInfo> = HashMap::from([
        ("spent".to_string(), capped("2999-01-01T00:00:00+00:00")),
        (
            "spent_forced".to_string(),
            capped("2999-01-01T00:00:00+00:00"),
        ),
        ("lapsed".to_string(), capped("2020-01-01T00:00:00+00:00")),
        (
            "busy".to_string(),
            UsageInfo {
                five_hour: Some(UsageWindow {
                    utilization: 40.0,
                    resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
                }),
                ..Default::default()
            },
        ),
        // "fresh" has no store entry → never fetched → always polled.
    ]);
    let forced: HashSet<String> = HashSet::from(["spent_forced".to_string()]);

    let snapshot = vec![
        token("spent"),
        token("spent_forced"),
        token("lapsed"),
        token("busy"),
        token("fresh"),
    ];
    let skip = spent_skip_set(&snapshot, &forced, &store, now);
    assert_eq!(
        skip,
        HashSet::from(["spent".to_string()]),
        "only the unforced spent account is skipped; forced/lapsed/below-cap/never-fetched poll",
    );
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
        &HashMap::new(),
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
        &HashMap::new(),
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
        &HashMap::new(),
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
        &HashMap::new(),
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
        &HashMap::new(),
    );
    assert_eq!(
        due.len(),
        1,
        "an unflagged profile snaps back to the cadence"
    );
    assert_eq!(next["a"], base + REFRESH_INTERVAL_MS);
}

/// The sibling of the `auth_broken` widen above, for the failure it can NEVER
/// cover: a refresh the endpoint rejected without confirming the token is dead
/// (`RefreshError::Transient`) leaves the profile unflagged on purpose, so
/// `auth_broken`'s backoff never applies. Without a ladder of its own, the one
/// failure mode that hits every profile at once — clauth's own request shape
/// drifting — re-hits the token endpoint at the full cadence forever, on every
/// account, with the row saying only `cached`. Same curve and ceiling as the 429
/// ladder, and computed live at partition time so a recovery snaps straight back.
#[test]
fn partition_due_ladders_a_profile_whose_refresh_keeps_failing() {
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let base = 1_700_000_000_000u64;
    last_fetched
        .lock()
        .unwrap()
        .insert("a".to_string(), EpochMs::from_millis(base));

    let snapshot = vec![token("a")];
    let streaks = |refresh_fail: u32| {
        HashMap::from([(
            "a".to_string(),
            super::StreakCounts {
                rate_limit: 0,
                refresh_fail,
            },
        )])
    };
    let next_at = |streaks: &HashMap<String, super::StreakCounts>| {
        partition_due(
            &snapshot,
            base,
            &last_fetched,
            &activity,
            REFRESH_INTERVAL_MS,
            streaks,
        )
        .1["a"]
    };

    // Streak 0 is the plain cadence — `rate_limit_backoff_ms(0)` returns a full
    // base step, so an unguarded call would silently defer a healthy profile.
    assert_eq!(
        next_at(&streaks(0)),
        base + REFRESH_INTERVAL_MS,
        "a profile with no refresh failures must not be deferred at all"
    );

    // The ladder climbs: 10s, 30s, 90s… on top of the fixed cadence.
    assert_eq!(next_at(&streaks(1)), base + REFRESH_INTERVAL_MS + 10_000);
    assert_eq!(next_at(&streaks(2)), base + REFRESH_INTERVAL_MS + 30_000);
    assert_eq!(next_at(&streaks(3)), base + REFRESH_INTERVAL_MS + 90_000);

    // …and stops at the same 15-minute ceiling the 429 ladder honors, rather
    // than running away to hours (`rate_limit_backoff_ms` alone is unbounded).
    assert_eq!(
        next_at(&streaks(50)),
        base + REFRESH_INTERVAL_MS + super::MAX_RETRY_AFTER_MS,
        "a deep refresh-fail streak caps at MAX_RETRY_AFTER_MS",
    );

    // A quarantined profile keeps the wider `auth_broken` deferral: that flag
    // means the token is confirmed dead, which outranks "might be a blip".
    let mut flagged = token("a");
    flagged.auth_broken = true;
    let (_, next) = partition_due(
        &[flagged],
        base,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &streaks(1),
    );
    assert_eq!(
        next["a"],
        base + REFRESH_INTERVAL_MS + super::AUTH_BROKEN_BACKOFF_MS,
        "a confirmed-dead token outranks the refresh-fail ladder"
    );
}

/// The two streak axes must move independently, because every other reader
/// means only one of them: `rate_limit` feeds `is_stuck_rate_limited`, the
/// auto-switch freshness bypass and `status.json`'s `stale` — none of which a
/// refresh failure may ever claim. A live body clears both.
#[test]
fn streak_axes_move_independently_and_a_live_body_clears_both() {
    use super::FetchStatus;

    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
    let update = |status: FetchStatus, refresh_failed: bool| {
        super::update_streaks(&streaks, "a", status, refresh_failed)
    };

    // A transient refresh failure bails to `Cached` — it must NOT touch the 429
    // axis, or a client-side bug would report a stuck throttle and let the
    // auto-switch rotate the chain away on it.
    let counts = update(FetchStatus::Cached, true);
    assert_eq!((counts.rate_limit, counts.refresh_fail), (0, 1));
    let counts = update(FetchStatus::Cached, true);
    assert_eq!((counts.rate_limit, counts.refresh_fail), (0, 2));

    // A 429 bumps only its own axis and leaves the refresh count standing.
    let counts = update(FetchStatus::RateLimited, false);
    assert_eq!((counts.rate_limit, counts.refresh_fail), (1, 2));

    // A status that says nothing about either axis holds both — and must not
    // conjure an entry for a profile that has none.
    let counts = update(FetchStatus::Failed, false);
    assert_eq!((counts.rate_limit, counts.refresh_fail), (1, 2));
    assert_eq!(
        super::update_streaks(&streaks, "never-seen", FetchStatus::Failed, false),
        super::StreakCounts::default(),
    );
    assert!(
        !streaks.lock().unwrap().contains_key("never-seen"),
        "a no-op update must not insert an empty entry"
    );

    // A live body clears both: whatever went wrong, the profile is serving. This
    // is also the preemptive-rotation case — a refresh can fail while the still
    // valid access token fetches fine, and nothing is degraded yet.
    let counts = update(FetchStatus::Fresh, true);
    assert_eq!((counts.rate_limit, counts.refresh_fail), (0, 0));
    assert!(!streaks.lock().unwrap().contains_key("a"));
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
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    let (status, retry_after) = rotation_bail_context(Some(Some(Duration::from_secs(300))));
    let outcome = FetchOutcome {
        name: "u".to_string(),
        info: None,
        status,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
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
        false,
    );
    let after = now_ms();

    assert_eq!(
        streaks.lock().unwrap().get("u").map(|c| c.rate_limit),
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
        &HashMap::new(),
    );
    assert!(due.is_empty(), "not due before the deferred slot");
    let (due, _) = partition_due(
        &snapshot,
        stamp + REFRESH_INTERVAL_MS,
        &last_fetched,
        &activity,
        REFRESH_INTERVAL_MS,
        &HashMap::new(),
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
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

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
            refresh_failed: false,
            plan_override: None,
            retry_after: None,
        },
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
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
            refresh_failed: false,
            plan_override: None,
            retry_after: None,
        },
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    assert!(
        store.lock().unwrap().contains_key("b"),
        "a cache fallback still cold-fills an absent entry"
    );
}

/// The scheduler half of the /usage-429 decouple: a `/profile` plan fetched
/// despite the 429 rides the cached bail and advances the STORED tier (Pro →
/// Free/canceled) while the cached 5h window is preserved, and the overlay
/// reaches disk so CLI/MCP readers see it too. The 429 status still surfaces.
#[test]
fn cached_bail_overlays_a_fresh_plan_onto_store_and_disk() {
    use super::{FetchOutcome, FetchStatus, StatusStore, apply_outcome};
    use crate::usage::{PlanInfo, PlanTier, UsageInfo, UsageWindow};

    let _home = crate::testutil::HomeSandbox::new();
    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    // Prior state: a live 5h window under a (now stale) Pro tier, in both the
    // store and the disk cache the bail loads from.
    let prior = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 5.0,
            resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
        }),
        plan: Some(PlanInfo {
            tier: PlanTier::Pro,
            subscription_status: None,
        }),
        ..Default::default()
    };
    store.lock().unwrap().insert("a".to_string(), prior.clone());
    super::write_profile_cache("a", super::USAGE_CACHE_FILE, &prior);

    let canceled = PlanInfo {
        tier: PlanTier::Free,
        subscription_status: Some("canceled".to_string()),
    };
    apply_outcome(
        FetchOutcome::cached("a", FetchStatus::RateLimited, None, None).with_plan(Some(canceled)),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );

    let got = store.lock().unwrap().get("a").cloned().unwrap();
    let plan = got.plan.as_ref().unwrap();
    assert_eq!(plan.tier, PlanTier::Free, "the stored tier flips to Free");
    assert!(
        plan.is_canceled(),
        "the canceled state persists to the store"
    );
    assert!(
        got.five_hour.is_some(),
        "the cached 5h window is preserved — only the tier advanced"
    );
    assert_eq!(
        status.lock().unwrap().get("a").copied(),
        Some(FetchStatus::RateLimited),
        "the account stays visibly rate-limited"
    );

    let disk = super::load_profile_cache::<UsageInfo>("a", super::USAGE_CACHE_FILE).unwrap();
    assert!(
        disk.plan.unwrap().is_canceled(),
        "the flip persists to usage_cache.json for CLI/MCP readers"
    );
}

/// The cold-canceled class: a profile added while ALREADY canceled 429s `/usage`
/// from its first poll and has no `usage_cache.json`, so the cached bail carries
/// a plan but `info=None`. The plan must still be recorded — on a windowless,
/// plan-only entry — in BOTH the store and disk, or the cancellation is dropped
/// every tick and the dead account stays selectable by the fallback walk.
#[test]
fn cold_bail_records_a_plan_only_canceled_entry() {
    use super::{FetchOutcome, FetchStatus, StatusStore, apply_outcome};
    use crate::usage::{PlanInfo, PlanTier, UsageInfo};

    let _home = crate::testutil::HomeSandbox::new();
    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    // No prior store entry and no usage_cache.json: `cached()` yields info=None.
    let canceled = PlanInfo {
        tier: PlanTier::Free,
        subscription_status: Some("canceled".to_string()),
    };
    apply_outcome(
        FetchOutcome::cached("cold", FetchStatus::RateLimited, None, None)
            .with_plan(Some(canceled)),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );

    let got = store.lock().unwrap().get("cold").cloned();
    assert!(
        got.as_ref()
            .and_then(|i| i.plan.as_ref())
            .is_some_and(|p| p.is_canceled()),
        "the store records the canceled plan even with no prior snapshot"
    );
    assert!(
        got.unwrap().five_hour.is_none(),
        "a plan-only entry — no windows to show"
    );

    let disk = super::load_profile_cache::<UsageInfo>("cold", super::USAGE_CACHE_FILE);
    assert!(
        disk.and_then(|i| i.plan).is_some_and(|p| p.is_canceled()),
        "and persists to usage_cache.json so the flip survives and readers see it"
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

/// The auto-start kick's firing rules: never mid-`/usage`-429-streak; a lapsed
/// window opens on the kick's backoff cadence; a live window re-tests a standing
/// block on the poll cadence (recovery may be imminent). Mid-streak the kick is
/// suppressed so it can't re-hit (and prolong) a throttled endpoint every slot; a
/// live `/usage` body clears the streak and the next due tick kicks cleanly.
#[test]
fn kick_suppressed_during_rate_limit_streak() {
    use super::should_open_window;

    // args: (streak, window_lapsed, kick_due, has_block)
    assert!(
        should_open_window(0, true, true, false),
        "lapsed + no streak → open"
    );
    assert!(
        !should_open_window(1, true, true, false),
        "lapsed but 429-streaking → suppress the kick"
    );
    assert!(
        !should_open_window(5, true, true, false),
        "deep streak → still suppressed"
    );
    assert!(
        !should_open_window(0, false, true, false),
        "a live window with no block never kicks"
    );
    assert!(
        should_open_window(0, false, true, true),
        "a live window WITH a standing block re-tests it — the window can be a \
         Claude-web open while Claude Code stays 429'd, so only a landed kick \
         proves the block is gone"
    );
    assert!(
        should_open_window(0, false, false, true),
        "a live-window block re-tests on the POLL cadence, not the deep kick \
         backoff — the window reopened (maybe via web), so recovery may be \
         imminent and we must not wait out the ~15min ladder"
    );
    assert!(
        !should_open_window(1, false, false, true),
        "but a /usage 429-streak still suppresses even the live-window re-test"
    );
    assert!(
        !should_open_window(0, true, false, true),
        "a LAPSED-window kick-429 block whose retry isn't due still waits its \
         backoff — no reopened-window signal, so don't re-hit a dead endpoint"
    );
}

// The `run_fetch` wiring seam: a LIVE 5h window with a standing block must
// re-test (the fix), a healthy live window stays quiet. Guards the
// `block.is_some()` → `has_block` plumbing `should_open_window`'s own test can't
// reach, since `run_fetch` is HTTP-bound.
#[test]
fn auto_start_re_tests_a_live_window_block_but_leaves_a_healthy_one() {
    use super::{KickBlock, KickBlocks, PollStreaks, auto_start_should_kick};
    use crate::usage::{UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso};

    let now = 3_000_000;
    let streaks: PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
    let live_store = || -> UsageStore {
        Arc::new(RankedMutex::new(HashMap::from([(
            "a".to_string(),
            UsageInfo {
                five_hour: Some(UsageWindow {
                    utilization: 5.0,
                    resets_at: Some(epoch_secs_to_iso(now + 3600)),
                }),
                ..Default::default()
            },
        )])))
    };

    let blocked: KickBlocks = Arc::new(RankedMutex::new(HashMap::from([(
        "a".to_string(),
        KickBlock {
            streak: 3,
            rejected: true,
            until: Some(now + 900),
            next_retry: now + 600,
        },
    )])));
    assert!(
        auto_start_should_kick(&streaks, &live_store(), &blocked, "a", now),
        "a live window with a standing block re-tests it — the fix"
    );

    let clean: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    assert!(
        !auto_start_should_kick(&streaks, &live_store(), &clean, "a", now),
        "a healthy live window with no block must not kick"
    );
}

/// The kick-429 block's retry clock: the streak climbs the shared backoff
/// ladder but never schedules past the limiter's advertised ceiling, and a
/// passed ceiling (or no block at all) is always due.
#[test]
fn kick_block_backoff_decays_toward_the_advertised_ceiling() {
    use super::{KickBlock, kick_block_after_429, kick_retry_due};
    use crate::oauth::KickRateLimit;

    let now = 1_000_000;
    let rl = KickRateLimit {
        rejected: true,
        until_epoch_secs: Some(now + 10_000),
    };

    let first = kick_block_after_429(None, &rl, now);
    assert_eq!(first.streak, 1);
    assert!(first.rejected);
    assert_eq!(first.until, Some(now + 10_000));
    assert_eq!(
        first.next_retry,
        now + 10,
        "streak 1 rides the ladder base (10s), far below the ceiling"
    );
    assert!(
        !kick_retry_due(Some(&first), now + 5),
        "before next_retry → not due"
    );
    assert!(
        kick_retry_due(Some(&first), now + 10),
        "at next_retry → due"
    );
    assert!(kick_retry_due(None, now), "no block → always due");

    // Climb the ladder deep enough that it would overshoot a near ceiling.
    let deep = kick_block_after_429(Some(first), &rl, now + 9_990);
    assert_eq!(deep.streak, 2);
    let near_rl = KickRateLimit {
        rejected: true,
        until_epoch_secs: Some(now + 9_995),
    };
    let clamped = kick_block_after_429(Some(deep), &near_rl, now + 9_990);
    assert_eq!(
        clamped.next_retry,
        now + 9_995,
        "ladder overshooting the advertised ceiling clamps to the ceiling — \
         the reset is an upper bound the retry must reach, never sleep past"
    );

    // No headers at all still blocks, on the pure ladder.
    let bare = KickRateLimit {
        rejected: false,
        until_epoch_secs: None,
    };
    let no_hint = kick_block_after_429(None, &bare, now);
    assert!(!no_hint.rejected);
    assert_eq!(no_hint.until, None);
    assert_eq!(no_hint.next_retry, now + 10);

    // With NO ceiling to clamp to, a deep streak must still cap at the shared
    // 15min MAX_RETRY_AFTER_MS — an uncapped ladder (~6h at streak 8) would
    // wedge the window closed for hours after a header-less outage clears.
    let deep_bare = kick_block_after_429(
        Some(KickBlock {
            streak: 8,
            rejected: false,
            until: None,
            next_retry: now,
        }),
        &bare,
        now,
    );
    assert!(
        deep_bare.next_retry <= now + 15 * 60,
        "ladder must cap at 15min, got +{}s",
        deep_bare.next_retry - now
    );
}

/// Only a switch-grade block moves the fallback chain: the limiter's own
/// `rejected` verdict, ≥2 consecutive kicks, ceiling still ahead. Anything
/// weaker gets the pill + backoff but never rotates accounts.
#[test]
fn only_a_switch_grade_kick_block_rotates_the_chain() {
    use super::{KickBlock, KickBlocks, kick_block_switch_grade, kick_rejected_names};

    let now = 3_000_000;
    let grade = KickBlock {
        streak: 2,
        rejected: true,
        until: Some(now + 600),
        next_retry: now + 30,
    };
    assert!(kick_block_switch_grade(&grade, now));
    assert!(
        !kick_block_switch_grade(&KickBlock { streak: 1, ..grade }, now),
        "one 429 must not move the chain — flap guard"
    );
    assert!(
        !kick_block_switch_grade(
            &KickBlock {
                rejected: false,
                ..grade
            },
            now
        ),
        "a burst 429 without the limiter's rejected verdict must not move the chain"
    );
    assert!(
        !kick_block_switch_grade(
            &KickBlock {
                until: None,
                ..grade
            },
            now
        ),
        "no advertised ceiling → no switch-grade claim"
    );
    assert!(
        !kick_block_switch_grade(&grade, now + 601),
        "a passed ceiling ends the claim — the next kick re-proves it or clears"
    );

    let blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::from([
        ("dead".to_string(), grade),
        ("blip".to_string(), KickBlock { streak: 1, ..grade }),
    ])));
    assert_eq!(kick_rejected_names(&blocks, now), vec!["dead".to_string()]);
}

/// `note_kick_outcome` lifecycle: a 429 upserts the block and writes the
/// per-profile cache file; a later successful kick clears both. A no-metadata
/// failure (transport, 401 path) leaves existing state untouched.
#[test]
fn kick_block_persists_and_clears_by_outcome() {
    use super::{kick_block, note_kick_outcome, sync_kick_blocks_from_cache};
    use crate::oauth::KickRateLimit;
    use crate::profile_cache::{KICK_BLOCK_CACHE_FILE, load_profile_cache};

    let _home = crate::testutil::HomeSandbox::new();
    let blocks: super::KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let now = 2_000_000;
    let rl = KickRateLimit {
        rejected: true,
        until_epoch_secs: Some(now + 600),
    };

    note_kick_outcome(&blocks, "kitty", false, Some(rl), now);
    let live = kick_block(&blocks, "kitty").expect("429 outcome must block");
    assert_eq!(live.streak, 1);
    let on_disk: super::KickBlock =
        load_profile_cache("kitty", KICK_BLOCK_CACHE_FILE).expect("block written through");
    assert_eq!(on_disk, live);

    // A failure with no limiter metadata must not disturb the block.
    note_kick_outcome(&blocks, "kitty", false, None, now + 20);
    assert_eq!(kick_block(&blocks, "kitty"), Some(live));

    // A second 429 grows the streak in place.
    note_kick_outcome(&blocks, "kitty", false, Some(rl), now + 30);
    assert_eq!(kick_block(&blocks, "kitty").map(|b| b.streak), Some(2));

    // A fresh map (new process) resumes the persisted block…
    let rehydrated: super::KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    sync_kick_blocks_from_cache(&rehydrated, &["kitty".to_string()]);
    assert_eq!(kick_block(&rehydrated, "kitty").map(|b| b.streak), Some(2));

    // …and a successful kick clears map + file, so the next sync clears mirrors.
    note_kick_outcome(&blocks, "kitty", true, None, now + 40);
    assert_eq!(kick_block(&blocks, "kitty"), None);
    assert!(
        load_profile_cache::<super::KickBlock>("kitty", KICK_BLOCK_CACHE_FILE).is_none(),
        "clearing must remove the cache file"
    );
    sync_kick_blocks_from_cache(&rehydrated, &["kitty".to_string()]);
    assert_eq!(
        kick_block(&rehydrated, "kitty"),
        None,
        "a mirroring instance drops the block once the file is gone"
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
        let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
        let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
        let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        let pending_off: PendingSwitchOff = Arc::new(RankedMutex::new(false));
        (store, status, streaks, activity, pending, pending_off)
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
    let (store, status, streaks, activity, pending, pending_off) = frozen_state();
    scan_auto_switch(
        &config_handle(true),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &streaks,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &activity,
        &pending,
        &pending_off,
    );
    assert!(
        pending.lock().unwrap().contains("b"),
        "a dead active must be walked away from without waiting for a Fresh read"
    );

    // Healthy active, identical frozen stores (lapsed window = headroom, shallow
    // streak) → the freshness gate holds: not broken, not stuck-RL, not Fresh.
    let (store, status, streaks, activity, pending, pending_off) = frozen_state();
    scan_auto_switch(
        &config_handle(false),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &streaks,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &activity,
        &pending,
        &pending_off,
    );
    assert!(
        pending.lock().unwrap().is_empty(),
        "a merely-stale healthy active must still not drive a switch"
    );
}

/// The scan fills `ChainSnapshot::fresh`, which `snapshot_chain` cannot: config
/// carries no freshness and `Profile.fetch_status` is written by the UI thread
/// only, so the daemon reads it stale. Without this fill the store twin's
/// fresh-preference pass matches nothing and silently degrades to walk order.
#[test]
fn scan_auto_switch_prefers_a_fresh_member_over_an_earlier_stale_one() {
    use super::{FetchStatus, PendingSwitch, StatusStore, scan_auto_switch};
    use crate::profile::{AppConfig, AppState, Profile};
    use crate::usage::{UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso, now_epoch_secs};

    let live = |utilization: f64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
        }),
        ..Default::default()
    };
    // Spent active; both siblings read as headroom, but only c's read is live.
    let store: UsageStore = Arc::new(RankedMutex::new(HashMap::from([
        ("a".to_string(), live(100.0)),
        ("b".to_string(), live(10.0)),
        ("c".to_string(), live(20.0)),
    ])));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([
        ("a".to_string(), FetchStatus::Fresh),
        ("b".to_string(), FetchStatus::Cached),
        ("c".to_string(), FetchStatus::Fresh),
    ])));
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            active_profile: Some("a".into()),
            profiles: vec!["a".into(), "b".into(), "c".into()],
            fallback_chain: vec!["a".into(), "b".into(), "c".into()],
            ..AppState::default()
        },
        profiles: vec![
            Profile::new("a".to_string(), None, None),
            Profile::new("b".to_string(), None, None),
            Profile::new("c".to_string(), None, None),
        ],
    }));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
    scan_auto_switch(
        &config,
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &Arc::new(RankedMutex::new(HashMap::new())),
        &Arc::new(RankedMutex::new(HashMap::new())),
        &Arc::new(RankedMutex::new(HashMap::new())),
        &pending,
        &Arc::new(RankedMutex::new(false)),
    );
    let queued = pending.lock().unwrap();
    assert!(
        queued.contains("c"),
        "the scan must fill `fresh` so the walk prefers c's trusted read; queued: {:?}",
        *queued
    );
    assert!(
        !queued.contains("b"),
        "b is reached first but its read is Cached — walk order must not win"
    );
}

/// `decision_fresh_any` unions BOTH status stores. Before the fix the scheduler
/// twin read only the OAuth `StatusStore`, so a fresh third-party member looked
/// stale to its fresh-preference/recovery gate while the UI twin (reading
/// `Profile.fetch_status`, filled from both in `apply_usage`) saw it — the twins
/// disagreed on a mixed OAuth+third-party chain (2026-07-17).
#[test]
fn decision_fresh_any_reads_both_the_oauth_and_third_party_stores() {
    use super::{FetchStatus, StatusStore, ThirdPartyStatusStore, decision_fresh_any};

    let oauth: StatusStore = Arc::new(RankedMutex::new(HashMap::from([
        ("a".to_string(), FetchStatus::Fresh),
        ("stale".to_string(), FetchStatus::Cached),
    ])));
    let tp: ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::from([
        ("b".to_string(), FetchStatus::Fresh),
        ("tp-stale".to_string(), FetchStatus::Cached),
    ])));

    assert!(decision_fresh_any(&oauth, &tp, "a"), "OAuth-fresh counts");
    assert!(
        decision_fresh_any(&oauth, &tp, "b"),
        "third-party-fresh must count too — the whole point of the fix"
    );
    assert!(
        !decision_fresh_any(&oauth, &tp, "stale"),
        "OAuth Cached is not fresh"
    );
    assert!(
        !decision_fresh_any(&oauth, &tp, "tp-stale"),
        "third-party Cached is not fresh"
    );
    assert!(
        !decision_fresh_any(&oauth, &tp, "unknown"),
        "absent in both stores is not fresh"
    );
}

/// RLS-1 (the RateLimited analogue of AUTH-4): a **deep-slot stuck RateLimited**
/// active bypasses the freshness gate so the daemon stops wedging on a
/// rate-limited account — but, unlike auth-broken, the switch still faces the
/// walk's last-known exhaustion gate. Four cases share one frozen shape:
///   * deep streak (> cap) + genuinely-spent LIVE window → switches away;
///   * deep streak + LIVE headroom → stays (throttle artifact, no false switch);
///   * deep streak + stale-HIGH but LAPSED window → stays — the load-bearing
///     RLS-1↔AUTH-4 asymmetry: this is the exact frozen shape a real 429 storm
///     holds (the last Fresh window is preserved; after ~5h it lapses to
///     `resets_at` in the past). An auth-broken active WALKS AWAY on this same
///     store (it bypasses the exhaustion gate too); a stuck-RL active must NOT,
///     since `five_hour_live` reads the lapsed window as regained headroom. A
///     false switch here would log out every running claude over a reset account;
///   * shallow streak (≤ cap) + spent window → stays (give the active cap's
///     frequent retries a chance to return a Fresh read first).
#[test]
fn scan_auto_switch_distrusts_a_deep_slot_stuck_rate_limited_active() {
    use super::{
        ACTIVE_CAP_MAX_STREAK, FetchStatus, PendingSwitch, PendingSwitchOff, PollStreaks,
        StatusStore, scan_auto_switch,
    };
    use crate::profile::{AppConfig, AppState, Profile};
    use crate::usage::{UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso, now_epoch_secs};

    // `a` is active and RateLimited; `active_util` on a 5h window whose reset is
    // `resets_offset` seconds from now (negative = a LAPSED window, which
    // `five_hour_live` reads as regained headroom regardless of `active_util`);
    // `streak` sets slot depth. `b` is a viable Fresh sibling.
    let frozen_state = |active_util: f64, resets_offset: i64, streak: u32| {
        let store: UsageStore = Arc::new(RankedMutex::new(HashMap::from([
            (
                "a".to_string(),
                UsageInfo {
                    five_hour: Some(UsageWindow {
                        utilization: active_util,
                        resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + resets_offset)),
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
        let streaks: PollStreaks = Arc::new(RankedMutex::new(HashMap::from([(
            "a".to_string(),
            super::StreakCounts {
                rate_limit: streak,
                refresh_fail: 0,
            },
        )])));
        let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
        let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        let pending_off: PendingSwitchOff = Arc::new(RankedMutex::new(false));
        (store, status, streaks, activity, pending, pending_off)
    };
    let config_handle = || -> crate::profile::ConfigHandle {
        Arc::new(RankedMutex::new(AppConfig {
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
        }))
    };
    let deep = ACTIVE_CAP_MAX_STREAK + 1;
    // The set of profiles the scan queued a switch to (sorted for determinism).
    let run = |util: f64, resets_offset: i64, streak: u32| -> Vec<String> {
        let (store, status, streaks, activity, pending, pending_off) =
            frozen_state(util, resets_offset, streak);
        scan_auto_switch(
            &config_handle(),
            &store,
            &status,
            &Arc::new(RankedMutex::new(HashMap::new())),
            &streaks,
            &Arc::new(RankedMutex::new(HashMap::new())),
            &activity,
            &pending,
            &pending_off,
        );
        let mut queued: Vec<String> = pending.lock().unwrap().iter().cloned().collect();
        queued.sort();
        queued
    };

    // Deep slot + genuinely spent (LIVE window ≥ threshold) → the wedge breaks.
    assert_eq!(
        run(100.0, 3600, deep),
        vec!["b".to_string()],
        "a deep-slot stuck RateLimited active that is genuinely spent must be walked away from"
    );
    // Deep slot but real LIVE headroom → no false switch (the walk's exhaustion
    // gate still holds; distrusting the STATUS never means trusting spent NUMBERS).
    assert!(
        run(10.0, 3600, deep).is_empty(),
        "a stuck RateLimited active with last-known headroom must stay put"
    );
    // Deep slot + stale-HIGH but LAPSED window (the real post-storm shape) → STAY.
    // This is where RLS-1 diverges from AUTH-4: a broken active walks away on this
    // identical store, a stuck-RL one must not — `five_hour_live` reads the lapsed
    // window as regained headroom, so the account is NOT exhausted.
    assert!(
        run(100.0, -3600, deep).is_empty(),
        "a stuck RateLimited active whose maxed window has since LAPSED must stay put \
         (regained headroom), never false-switch off a reset account"
    );
    // Shallow slot + spent → still gated on Fresh; the active cap's frequent
    // retries get a chance to return a live read before we distrust.
    assert!(
        run(100.0, 3600, ACTIVE_CAP_MAX_STREAK).is_empty(),
        "a shallow RateLimited active must still wait for a Fresh read"
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
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
    let outcome = |name: &str, retry_after: Option<Duration>| FetchOutcome {
        name: name.to_string(),
        info: None,
        status: FetchStatus::RateLimited,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
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
        false,
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
        &HashMap::new(),
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
        &HashMap::new(),
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
        false,
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
        false,
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
        false,
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
        false,
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
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    let rate_limited = |from_fetch: bool, status: FetchStatus| FetchOutcome {
        refresh_failed: false,
        plan_override: None,
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
            false,
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
        false,
    );
    let before = now_ms();
    apply_outcome(
        rate_limited(false, FetchStatus::RateLimited),
        &store,
        &status,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    let after = now_ms();
    assert!(
        (before + RATE_LIMIT_MIN_BACKOFF_MS..=after + RATE_LIMIT_MIN_BACKOFF_MS).contains(&stamp()),
        "a live fetch resets the backoff streak"
    );
}

/// PR #30 guard: the streak ladder escalates a repeated 429 even while the server
/// hint stays PRESENT. A constant sub-cadence `retry-after` is overridden at every
/// streak by `max(hint, interval + backoff(streak))`, so the same account 429ing
/// three times in a row backs off base → base·f → base·f² just like the no-hint
/// path. Without the `max`, an always-present hint (the real endpoint answers
/// `retry-after: 0`) would freeze the streak counter and pin the account forever.
#[test]
fn hint_present_429s_still_ride_the_streak_ladder() {
    use std::time::Duration;

    use super::{
        FetchOutcome, FetchStatus, RATE_LIMIT_BACKOFF_FACTOR, RATE_LIMIT_MIN_BACKOFF_MS,
        StatusStore, apply_outcome, now_ms,
    };

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    // A constant hint well under the ladder floor — present on every 429, so the
    // escalation below can only come from the streak ladder overriding it.
    let hinted = || FetchOutcome {
        name: "a".to_string(),
        info: None,
        status: FetchStatus::RateLimited,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
        retry_after: Some(Duration::from_secs(5)),
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

    let base = RATE_LIMIT_MIN_BACKOFF_MS;
    let f = RATE_LIMIT_BACKOFF_FACTOR;
    for expect in [base, base * f, base * f * f] {
        let before = now_ms();
        apply_outcome(
            hinted(),
            &store,
            &status,
            &last_fetched,
            &streaks,
            REFRESH_INTERVAL_MS,
            false,
        );
        let after = now_ms();
        assert!(
            (before + expect..=after + expect).contains(&stamp()),
            "a hinted 429 backs off to {expect}ms past now — the ladder, not the 5s hint"
        );
    }
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
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));

    let outcome = |kind: FetchStatus| FetchOutcome {
        name: "a".to_string(),
        info: None,
        status: kind,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
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
            false,
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

// `classify_pre_rotation` is the pure classifier `fetch_with_rotation` extracts
// its branch selection into — no I/O, no clock read, so the truth table below
// exercises it without live HTTP. `token_clock_expired` is passed in as an
// already-computed bool (never re-derived inside the classifier).

#[test]
fn pre_rotation_serves_a_live_body() {
    use super::{PreRotationDecision, classify_pre_rotation};
    use crate::usage::{PlanInfo, PlanTier, UsageInfo};

    let info = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Pro,
            subscription_status: None,
        }),
        ..UsageInfo::default()
    };
    match classify_pre_rotation(Ok(info), false) {
        PreRotationDecision::Serve(served) => {
            assert_eq!(served.plan.expect("plan").tier, PlanTier::Pro);
        }
        other => panic!("expected Serve, got {other:?}"),
    }
}

#[test]
fn pre_rotation_429_on_a_valid_token_bails_rate_limited_with_plan() {
    use std::time::Duration;

    use super::{FetchError, PreRotationDecision, classify_pre_rotation};
    use crate::usage::{PlanInfo, PlanTier};

    let err = FetchError::RateLimited {
        retry_after: Some(Duration::from_secs(30)),
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
        }),
    };
    // token_clock_expired == false: a still-valid token's 429 is a pure
    // endpoint rate limit — bail to cache, plan and retry_after both intact.
    match classify_pre_rotation(Err(err), false) {
        PreRotationDecision::BailRateLimited { retry_after, plan } => {
            assert_eq!(retry_after, Some(Duration::from_secs(30)));
            let plan = plan.expect("plan rides along on a live-token 429");
            assert_eq!(plan.tier, PlanTier::Free);
            assert_eq!(plan.subscription_status.as_deref(), Some("canceled"));
        }
        other => panic!("expected BailRateLimited, got {other:?}"),
    }
}

#[test]
fn pre_rotation_401_rotates_without_an_unmask_hint() {
    use super::{FetchError, PreRotationDecision, classify_pre_rotation};

    match classify_pre_rotation(Err(FetchError::Status(401)), false) {
        PreRotationDecision::Rotate { unmask_429 } => assert_eq!(unmask_429, None),
        other => panic!("expected Rotate, got {other:?}"),
    }
}

#[test]
fn pre_rotation_429_on_an_expired_token_rotates_and_drops_the_plan() {
    use std::time::Duration;

    use super::{FetchError, PreRotationDecision, classify_pre_rotation};
    use crate::usage::{PlanInfo, PlanTier};

    let with_hint = FetchError::RateLimited {
        retry_after: Some(Duration::from_secs(5)),
        plan: Some(PlanInfo {
            tier: PlanTier::Pro,
            subscription_status: None,
        }),
    };
    // token_clock_expired == true: falls through to rotation. The `Rotate`
    // variant has no plan field at all — the dead-token plan is dropped by
    // construction, not just by convention.
    match classify_pre_rotation(Err(with_hint), true) {
        PreRotationDecision::Rotate { unmask_429 } => {
            assert_eq!(unmask_429, Some(Some(Duration::from_secs(5))));
        }
        other => panic!("expected Rotate, got {other:?}"),
    }

    let no_hint = FetchError::RateLimited {
        retry_after: None,
        plan: None,
    };
    match classify_pre_rotation(Err(no_hint), true) {
        PreRotationDecision::Rotate { unmask_429 } => assert_eq!(unmask_429, Some(None)),
        other => panic!("expected Rotate, got {other:?}"),
    }
}

#[test]
fn pre_rotation_other_errors_bail_to_cache() {
    use super::{FetchError, PreRotationDecision, classify_pre_rotation};

    assert!(matches!(
        classify_pre_rotation(Err(FetchError::Network), false),
        PreRotationDecision::BailCached
    ));
    assert!(matches!(
        classify_pre_rotation(Err(FetchError::Parse), false),
        PreRotationDecision::BailCached
    ));
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

// ── stand-down hydrate (a live daemon owns the loop) ─────────────────────────
//
// While `standdown_tick` runs, this side never fetches or rotates — it only
// re-seeds the stores from the disk caches the daemon keeps fresh. These pin
// the hydrate contract: cache → store with a freshness-derived status and
// `last_fetched` stamped AT the cache mtime (so the published countdowns track
// the daemon's real cadence, not this process's clock).

#[test]
fn standdown_hydrate_seeds_the_store_from_the_daemon_cache() {
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::{UsageInfo, UsageWindow};
    let _home = crate::testutil::HomeSandbox::new();

    let info = UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 42.0,
            resets_at: None,
        }),
        ..UsageInfo::default()
    };
    write_profile_cache("kitty", USAGE_CACHE_FILE, &info);

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: super::StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let tp_store: super::ThirdPartyUsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let tp_status: super::ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));

    super::hydrate_from_daemon_caches(
        &store,
        &status,
        &tp_store,
        &tp_status,
        &last_fetched,
        &["kitty".to_string(), "cacheless".to_string()],
        &[],
        REFRESH_INTERVAL_MS,
    );

    let seeded = store.lock().unwrap().get("kitty").cloned();
    assert_eq!(
        seeded.and_then(|i| i.five_hour.map(|w| w.utilization)),
        Some(42.0),
        "the daemon-written cache lands in the live store"
    );
    // A just-written cache (mtime ≈ now) is inside the fetch window → Fresh,
    // and its stamp anchors the countdown to the daemon's write time.
    assert_eq!(
        status.lock().unwrap().get("kitty").copied(),
        Some(super::FetchStatus::Fresh),
    );
    let stamp = last_fetched.lock().unwrap().get("kitty").copied();
    let now = super::now_ms();
    assert!(
        stamp.is_some_and(|s| now.saturating_sub(s.as_millis()) < 30_000),
        "last_fetched stamped at the cache mtime: {stamp:?} vs now {now}"
    );

    // No cache → left untouched (the daemon publishes it shortly); never a
    // synthetic entry that would render as data.
    assert!(store.lock().unwrap().get("cacheless").is_none());
    assert!(status.lock().unwrap().get("cacheless").is_none());
    assert!(last_fetched.lock().unwrap().get("cacheless").is_none());
}

/// Re-hydrating every tick must track the daemon's writes: a NEWER cache body
/// replaces the seeded one (same profile, later mtime), never the reverse.
#[test]
fn standdown_hydrate_follows_the_daemon_cache_forward() {
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::{UsageInfo, UsageWindow};
    let _home = crate::testutil::HomeSandbox::new();

    let at = |util: f64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: util,
            resets_at: None,
        }),
        ..UsageInfo::default()
    };
    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let status: super::StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let tp_store: super::ThirdPartyUsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let tp_status: super::ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let hydrate = |seed_names: &[String]| {
        super::hydrate_from_daemon_caches(
            &store,
            &status,
            &tp_store,
            &tp_status,
            &last_fetched,
            seed_names,
            &[],
            REFRESH_INTERVAL_MS,
        )
    };

    write_profile_cache("kitty", USAGE_CACHE_FILE, &at(10.0));
    hydrate(&["kitty".to_string()]);
    write_profile_cache("kitty", USAGE_CACHE_FILE, &at(55.0));
    hydrate(&["kitty".to_string()]);

    let seeded = store.lock().unwrap().get("kitty").cloned();
    assert_eq!(
        seeded.and_then(|i| i.five_hour.map(|w| w.utilization)),
        Some(55.0),
        "the daemon's newer write wins on the next hydrate"
    );
}

/// `standdown_tick` end to end (minus the probe): forced names from a manual
/// `r` are drained and their Queued marks cleared (the daemon can't be asked
/// to fetch early — a stranded mark freezes the row spinner), the store is
/// hydrated, and countdowns are published off the cache stamp. Nothing here
/// performs HTTP: every assertion is served by disk state alone.
#[test]
fn standdown_tick_drains_forced_and_publishes_countdowns() {
    use crate::profile::{AppConfig, AppState};
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::UsageInfo;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    let _home = crate::testutil::HomeSandbox::new();

    write_profile_cache("kitty", USAGE_CACHE_FILE, &UsageInfo::default());

    // The standby seed sources names from config (the display superset), so the
    // profile whose cache is hydrated must live there — as it does in production.
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile_disabled("kitty", false)],
    }));
    let state = super::SchedulerState {
        config,
        tokens: Arc::new(RankedMutex::new(vec![token("kitty")])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashSet::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(true),
    };

    // A manual `r` landed just before this tick: forced name + Queued mark.
    state.refetch_queue.lock().unwrap().insert("kitty".into());
    mark_activity(&state.activity, "kitty", ProfileActivity::Queued);

    super::standdown_tick(&state, REFRESH_INTERVAL_MS);

    assert!(
        state.refetch_queue.lock().unwrap().is_empty(),
        "forced names are consumed, not left to pile up"
    );
    assert!(
        state.activity.lock().unwrap().get("kitty").is_none(),
        "the Queued mark is cleared — no frozen spinner"
    );
    assert!(
        state.store.lock().unwrap().contains_key("kitty"),
        "the store is hydrated from the daemon cache"
    );
    let next = state
        .next_refresh_per_profile
        .lock()
        .unwrap()
        .get("kitty")
        .copied();
    let stamp = state
        .last_fetched
        .lock()
        .unwrap()
        .get("kitty")
        .map(|e| e.as_millis());
    assert_eq!(
        next,
        stamp.map(|s| s + REFRESH_INTERVAL_MS),
        "the countdown tracks the cache stamp + one interval"
    );
}

/// The bootstrap pre-marks cache-due profiles `Queued` for first paint,
/// expecting a fetch worker to take over — standing down, no worker exists, so
/// the tick must sweep EVERY Queued mark (not only forced ones) or the row
/// spins forever where the daemon-fed countdown belongs. In-flight kinds stay.
#[test]
fn standdown_sweeps_bootstrap_queued_marks() {
    use crate::profile::{AppConfig, AppState};
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::UsageInfo;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    let _home = crate::testutil::HomeSandbox::new();

    write_profile_cache("kitty", USAGE_CACHE_FILE, &UsageInfo::default());

    // The standby seed sources names from config (the display superset), so the
    // profile whose cache is hydrated must live there — as it does in production.
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile_disabled("kitty", false)],
    }));
    let state = super::SchedulerState {
        config,
        tokens: Arc::new(RankedMutex::new(vec![token("kitty"), token("stale")])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashSet::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(true),
    };

    // Bootstrap pre-marked a cache-due profile; a rotate worker from the last
    // armed tick is still in flight on another.
    mark_activity(&state.activity, "stale", ProfileActivity::Queued);
    mark_activity(&state.activity, "kitty", ProfileActivity::Refreshing);

    super::standdown_tick(&state, REFRESH_INTERVAL_MS);

    let a = state.activity.lock().unwrap();
    assert!(
        a.get("stale").is_none(),
        "an un-owned Queued mark is swept — no frozen spinner"
    );
    assert!(
        matches!(a.get("kitty"), Some(ProfileActivity::Refreshing)),
        "an in-flight worker's mark survives (it clears itself on landing)"
    );
}

/// Single-fetcher lease (#27): when another instance already holds
/// `usage-fetch.lock`, `tick` must take the stand-down path. Driven through the
/// real `tick` (not `standdown_tick`) so the lease branch itself is pinned: an
/// external holder forces `fetch_lease.acquire()` to return `false`.
///
/// `kitty` is stamped NOT due on purpose, which is what makes every assertion
/// below discriminate between the two branches — and keeps a regression cheap:
///   * armed + nothing due never calls `fetch_oauth_due`, so a broken lease
///     fails these asserts instead of firing a live request at the real endpoint
///     (`tick` hardcodes the real fetcher; there is no seam to inject through it);
///   * the `Queued` mark survives an armed tick (`clear_orphaned_forced` returns
///     early on an empty `forced` set, and no worker runs to clear it), while
///     `standdown_tick` sweeps EVERY `Queued` mark — so the sweep proves the
///     branch. Were `kitty` due, the armed path would mark-then-clear it too and
///     the assert would pass either way.
///   * the store is only seeded by the stand-down hydrate here; an armed tick
///     with nothing due never reaches `apply_outcome`, so it stays empty.
#[test]
fn tick_stands_down_when_another_instance_holds_the_fetch_lease() {
    use crate::profile::{AppConfig, AppState};
    use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
    use crate::usage::UsageInfo;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let _home = crate::testutil::HomeSandbox::new();

    // Another instance wins the lease first; its `File` must stay alive so the
    // flock stays held for the rest of the test.
    let other = crate::daemon::FetchLease::new();
    assert!(other.acquire(), "the first instance wins the lease");

    write_profile_cache("kitty", USAGE_CACHE_FILE, &UsageInfo::default());
    // The standby seed sources names from config (the display superset), so the
    // profile whose cache is hydrated must live there — as it does in production.
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile_disabled("kitty", false)],
    }));
    let state = super::SchedulerState {
        config,
        tokens: Arc::new(RankedMutex::new(vec![token("kitty")])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashSet::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        // A DIFFERENT lease over the same file → its acquire() is denied while
        // `other` holds the flock.
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(false),
    };

    // Stamp `kitty` as just-fetched so it is NOT due this tick: an armed tick
    // would then fetch nothing (no live request on a regression) and leave the
    // marks/store below untouched, which is what makes each assert discriminate.
    state
        .last_fetched
        .lock()
        .unwrap()
        .insert("kitty".to_string(), EpochMs::from_millis(super::now_ms()));

    // A bootstrap-only `Queued` mark: `standdown_tick` sweeps every Queued mark,
    // while an armed tick with nothing due leaves it in place.
    mark_activity(&state.activity, "kitty", ProfileActivity::Queued);

    super::tick(&state);

    assert!(
        state.activity.lock().unwrap().get("kitty").is_none(),
        "stood down: the Queued mark is swept, never driven into a fetch"
    );
    assert!(
        state.store.lock().unwrap().contains_key("kitty"),
        "stood down: the store is hydrated from the shared cache"
    );
    assert!(
        state.standdown_active.load(Ordering::Relaxed),
        "the stand-down edge is recorded"
    );
    drop(other);
}

// ── active-profile 429 ladder cap ────────────────────────────────────────────
//
// A deep back-off slot on the active row mostly buys staleness on the exact
// row the user watches (2026-07-12: the endpoint recovered while the active
// account sat out a 14-minute slot as `RateLimited`), so shallow streaks cap
// at 2× cadence. The cap RELEASES past `ACTIVE_CAP_MAX_STREAK`: the `/usage`
// window counts rejected polls and only clauth's own polls fill it (#30), so
// a sustained storm must climb the same drain ladder as idle profiles or the
// capped re-polls keep the window pinned. Idle profiles always keep the full
// ladder.

#[test]
fn active_profile_rate_limit_ladder_caps_at_one_extra_interval() {
    use super::{IntervalMs, MAX_RETRY_AFTER_MS, next_slot_deferral};
    let interval = 90_000u64;
    // Deep streak: the idle ladder pushes the slot to the 15-min ceiling.
    assert_eq!(
        next_slot_deferral(true, None, 6, interval, false),
        IntervalMs::from_millis(MAX_RETRY_AFTER_MS - interval),
        "idle keeps the full drain ladder"
    );
    // Active: the slot lands at most one extra interval out (2x cadence).
    assert_eq!(
        next_slot_deferral(true, None, 6, interval, true),
        IntervalMs::from_millis(interval),
        "active caps at 2x cadence"
    );
}

#[test]
fn active_profile_cap_still_honors_a_real_server_hint() {
    use super::{IntervalMs, next_slot_deferral};
    let interval = 90_000u64;
    // A genuine long retry-after is a server directive, not ladder guesswork —
    // the active cap must not shorten it.
    assert_eq!(
        next_slot_deferral(
            true,
            Some(std::time::Duration::from_secs(600)),
            6,
            interval,
            true
        ),
        IntervalMs::from_millis(600_000 - interval),
        "a real retry-after wins over the active cap"
    );
}

#[test]
fn active_profile_cap_leaves_shallow_streaks_alone() {
    use super::next_slot_deferral;
    let interval = 90_000u64;
    // streak 1 ladder (interval + 10s) sits under the cap: identical either way.
    assert_eq!(
        next_slot_deferral(true, None, 1, interval, true),
        next_slot_deferral(true, None, 1, interval, false),
    );
}

/// Pins where the cap first bites and where it releases, so a drift in either
/// boundary fails loudly. At 90s cadence: streak 3's ladder (90s + 90s) equals
/// the 2× cap exactly (a no-op), streak 4 (90s + 270s) is the first capped
/// step, streak 6 the last, and streak 7 releases to the idle drain ladder.
#[test]
fn active_profile_cap_bites_at_streak_4_and_releases_past_6() {
    use super::{IntervalMs, MAX_RETRY_AFTER_MS, next_slot_deferral};
    let interval = 90_000u64;
    // streak 3: ladder == cap, active and idle agree.
    assert_eq!(
        next_slot_deferral(true, None, 3, interval, true),
        next_slot_deferral(true, None, 3, interval, false),
        "streak 3 sits exactly on the cap"
    );
    // streak 4: first bite — active holds 2x cadence, idle walks away.
    assert_eq!(
        next_slot_deferral(true, None, 4, interval, true),
        IntervalMs::from_millis(interval),
        "streak 4 is the first capped step"
    );
    assert_ne!(
        next_slot_deferral(true, None, 4, interval, false),
        IntervalMs::from_millis(interval),
        "idle streak 4 must not be capped"
    );
    // streak 7: the cap releases — the active row climbs the same drain
    // ladder as an idle profile (the sustained-storm concession).
    assert_eq!(
        next_slot_deferral(true, None, 7, interval, true),
        next_slot_deferral(true, None, 7, interval, false),
        "past the bound the active row must drain like an idle one"
    );
    assert_eq!(
        next_slot_deferral(true, None, 7, interval, true),
        IntervalMs::from_millis(MAX_RETRY_AFTER_MS - interval),
        "a released deep streak sits at the 15-min ceiling"
    );
}

/// The `is_active` flag threads through `apply_outcome` into the deferral —
/// a regression that drops or hardwires the flag stamps both rows the same.
/// Two profiles at the same deep streak, one active one idle: their stamped
/// next slots must differ by exactly the cap-vs-ladder gap.
#[test]
fn apply_outcome_threads_is_active_into_the_deferral() {
    use super::{FetchOutcome, FetchStatus, StatusStore, apply_outcome, now_ms};

    let store: super::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
    let statuses: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
    let streaks: super::PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
    // Both profiles arrive at streak 6 (deep, still capped for the active row).
    let at_five = super::StreakCounts {
        rate_limit: 5,
        refresh_fail: 0,
    };
    streaks.lock().unwrap().insert("act".to_string(), at_five);
    streaks.lock().unwrap().insert("idle".to_string(), at_five);

    let outcome = |name: &str| FetchOutcome {
        name: name.to_string(),
        info: None,
        status: FetchStatus::RateLimited,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
        retry_after: None,
    };

    let before = now_ms();
    apply_outcome(
        outcome("act"),
        &store,
        &statuses,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        true,
    );
    apply_outcome(
        outcome("idle"),
        &store,
        &statuses,
        &last_fetched,
        &streaks,
        REFRESH_INTERVAL_MS,
        false,
    );
    let after = now_ms();

    let stamp = |name: &str| {
        last_fetched
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .expect("stamp present")
            .as_millis()
    };
    // Active at streak 6: capped → deferral = one extra interval.
    assert!(
        (before + REFRESH_INTERVAL_MS..=after + REFRESH_INTERVAL_MS).contains(&stamp("act")),
        "active stamp must carry the 2x-cadence cap"
    );
    // Idle at streak 6: full ladder → the 15-min ceiling.
    let idle_extra = super::MAX_RETRY_AFTER_MS - REFRESH_INTERVAL_MS;
    assert!(
        (before + idle_extra..=after + idle_extra).contains(&stamp("idle")),
        "idle stamp must carry the full drain ladder"
    );
}

// ── OAuth refresh-all: completion-ordered result processing ──────────────────
//
// Each due profile fetches on its own worker; result processing (spinner clear +
// countdown publish) must fire the instant that profile's OWN fetch resolves,
// keyed on completion order — not the `due` list order. The old join-in-list
// loop stalled a fast account's clear behind an earlier slow account, so a fast
// row's spinner stayed lit / its countdown hidden until the slow one ahead
// finished. This is the regression guard.

/// Build a `SchedulerState` whose two OAuth profiles are `slow` (listed first)
/// and `fast` (listed second) — the ordering that trips the join-order stall.
fn completion_order_state() -> super::SchedulerState {
    use crate::profile::{AppConfig, AppState};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    super::SchedulerState {
        config: Arc::new(RankedMutex::new(AppConfig {
            state: AppState::default(),
            profiles: vec![],
        })),
        tokens: Arc::new(RankedMutex::new(vec![token("slow"), token("fast")])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        suppressed_generic: Arc::new(RankedMutex::new(HashSet::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        standdown_active: AtomicBool::new(false),
    }
}

/// A pure, disk-free outcome: `info: None` + `from_fetch: false` keeps
/// `apply_outcome` entirely in-memory (no cache read/write), so this test needs
/// no `HomeSandbox` and stays parallel-safe.
fn cached_outcome(name: &str) -> super::FetchOutcome {
    super::FetchOutcome {
        name: name.to_string(),
        info: None,
        status: super::FetchStatus::Cached,
        rotated: None,
        from_fetch: false,
        refresh_failed: false,
        plan_override: None,
        retry_after: None,
    }
}

#[test]
fn oauth_completions_apply_in_completion_order_not_list_order() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let state = completion_order_state();

    // `slow` (index 0) blocks in its worker until the test releases it; `fast`
    // (index 1) returns at once. The release is sent from a drop-guard at the end
    // of the scope, so even a failing assertion (RED) unblocks `slow` and lets
    // the scope join instead of hanging.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let worker = |entry: TokenEntry| -> super::FetchOutcome {
        if entry.name == "slow" {
            let _ = release_rx.lock().unwrap().recv();
        }
        cached_outcome(&entry.name)
    };

    /// Releases `slow` on drop so the scope always joins — success or panic.
    struct Release<'a>(&'a mpsc::Sender<()>);
    impl Drop for Release<'_> {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    std::thread::scope(|scope| {
        // Dropped at closure end (success or panic), releasing `slow` so the
        // scope's implicit join never hangs.
        let _release = Release(&release_tx);
        scope.spawn(|| {
            super::fetch_oauth_due_with(
                &state,
                vec![token("slow"), token("fast")],
                REFRESH_INTERVAL_MS,
                worker,
            );
        });

        // Wait for `fast` to be fully applied (countdown published). In
        // completion order this happens within microseconds while `slow` is
        // still blocked; a list-order drain is stuck on `slow` and never applies
        // `fast`, so the deadline fires and the test fails RED.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state
            .next_refresh_per_profile
            .lock()
            .unwrap()
            .contains_key("fast")
        {
            assert!(
                Instant::now() < deadline,
                "`fast` was never applied while `slow` held the head of the list — \
                 result-processing is still gated on list order, not completion order"
            );
            std::thread::yield_now();
        }

        // The core guarantee: at the instant `fast` is applied, `slow` is still
        // pending (spinner mark intact, no countdown). So the later-listed fast
        // account resolved strictly ahead of the slow account before it.
        {
            let activity = state.activity.lock().unwrap();
            assert!(
                activity.get("fast").is_none(),
                "`fast` spinner cleared on its own completion"
            );
            assert!(
                matches!(activity.get("slow"), Some(ProfileActivity::Queued)),
                "`slow` is still queued — it did not gate `fast`"
            );
        }
        assert!(
            !state
                .next_refresh_per_profile
                .lock()
                .unwrap()
                .contains_key("slow"),
            "`slow` countdown is not yet published — it lands after `fast`"
        );
    });

    // Both profiles are fully applied once the batch drains. Read `activity`
    // (rank 600) before `next_refresh` (rank 1100) so the two reads honour the
    // global lock order even though they are logically independent here.
    assert!(
        state.activity.lock().unwrap().is_empty(),
        "every spinner cleared by batch end"
    );
    let nrpp = state.next_refresh_per_profile.lock().unwrap();
    assert!(
        nrpp.contains_key("fast") && nrpp.contains_key("slow"),
        "both countdowns published by batch end"
    );
}

// ── identity memo (adopt path) ───────────────────────────────────────────────
//
// A rotation tick can run two adopts, each resolving the stored and the live
// token's account uuid — up to four `/profile` GETs, 5s apart, for the same two
// immutable answers.

/// A resolved uuid is fetched once per token: immutable, so a hit is exact.
/// Distinct tokens still each get their own probe.
#[test]
fn the_identity_memo_resolves_each_token_once() {
    let calls = std::cell::RefCell::new(Vec::<String>::new());
    let probe = |tok: &str| {
        calls.borrow_mut().push(tok.to_string());
        Some(format!("uuid-of-{tok}"))
    };
    let identity = memoized_identity(&probe);

    assert_eq!(identity("stored").as_deref(), Some("uuid-of-stored"));
    assert_eq!(
        identity("stored").as_deref(),
        Some("uuid-of-stored"),
        "the memo answers, and answers identically"
    );
    assert_eq!(identity("live").as_deref(), Some("uuid-of-live"));

    assert_eq!(
        calls.borrow().as_slice(),
        ["stored", "live"],
        "one probe per distinct token, no matter how often it is asked for"
    );
}

/// A failed probe must stay retryable. The adopt after a failed refresh exists
/// because the live mirror may have surfaced a fresh pair since the first
/// attempt — caching the `None` would silently make that second adopt a no-op.
#[test]
fn the_identity_memo_never_caches_a_failed_probe() {
    let calls = std::cell::RefCell::new(0usize);
    // Fails the first time, succeeds after — the mirror catching up mid-tick.
    let probe = |_tok: &str| {
        *calls.borrow_mut() += 1;
        (*calls.borrow() > 1).then(|| "uuid-late".to_string())
    };
    let identity = memoized_identity(&probe);

    assert_eq!(identity("live"), None, "first probe fails");
    assert_eq!(
        identity("live").as_deref(),
        Some("uuid-late"),
        "the retry must reach the probe, not a cached failure"
    );
    assert_eq!(*calls.borrow(), 2, "a None is never cached");

    assert_eq!(identity("live").as_deref(), Some("uuid-late"));
    assert_eq!(
        *calls.borrow(),
        2,
        "once it resolves, the answer is cached like any other"
    );
}

// ── scan_recovery ─────────────────────────────────────────────────────────
//
// The auto-recovery leg: after a switch-off-all, scans the fallback chain for
// a member back under its threshold and queues it. Every fixture below shares
// one chain member, "b", with a live 5h window under its default (95%)
// threshold — the shape that IS recoverable — and each test flips exactly one
// guard so the queue stays empty (or, for the happy path, fires).

use crate::usage::{UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso, now_epoch_secs};

fn recoverable_store() -> UsageStore {
    Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 10.0,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
            }),
            ..Default::default()
        },
    )])))
}

fn recovery_config(
    active_profile: Option<&str>,
    fallback_chain: &[&str],
) -> crate::profile::ConfigHandle {
    use crate::profile::{AppConfig, AppState, Profile};

    Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            active_profile: active_profile.map(Into::into),
            fallback_chain: fallback_chain.iter().map(|s| (*s).into()).collect(),
            ..AppState::default()
        },
        profiles: vec![Profile::new("b".to_string(), None, None)],
    }))
}

/// A switch already queued means a previous decision (auto-switch or a prior
/// recovery scan) hasn't been dispatched yet; scanning again on top of it
/// could queue a second, contradictory switch.
#[test]
fn scan_recovery_is_a_no_op_while_a_switch_is_pending() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};

    let store = recoverable_store();
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::from([
        "already-queued".to_string()
    ])));

    scan_recovery(
        &recovery_config(None, &["b"]),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert_eq!(
        pending.lock().unwrap().clone(),
        HashSet::from(["already-queued".to_string()]),
        "a pending switch must be left untouched, not joined by a second target"
    );
}

/// Recovery only applies to the switch-off-all state: an active profile means
/// there's nothing to relink.
#[test]
fn scan_recovery_is_a_no_op_with_an_active_profile_set() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};

    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &recovery_config(Some("a"), &["b"]),
        &recoverable_store(),
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert!(
        pending.lock().unwrap().is_empty(),
        "a live active profile must never be relinked over by a recovery scan"
    );
}

/// A `Cached`/`RateLimited`/absent read may be a rollover or a synthetic
/// just-kicked 0% — recovery must not relink to it even though its stored
/// numbers look recovered, matching the auto-switch side's freshness gate.
#[test]
fn scan_recovery_ignores_a_stale_or_synthetic_read() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};

    let store = recoverable_store();
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));

    for stale in [
        FetchStatus::Cached,
        FetchStatus::RateLimited,
        FetchStatus::Failed,
    ] {
        let status: StatusStore =
            Arc::new(RankedMutex::new(HashMap::from([("b".to_string(), stale)])));
        let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        scan_recovery(
            &recovery_config(None, &["b"]),
            &store,
            &status,
            &Arc::new(RankedMutex::new(HashMap::new())),
            &kick_blocks,
            &pending,
        );
        assert!(
            pending.lock().unwrap().is_empty(),
            "a {stale:?} read must not drive a recovery relink"
        );
    }

    // No read at all yet — same undecidable treatment.
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
    scan_recovery(
        &recovery_config(None, &["b"]),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );
    assert!(
        pending.lock().unwrap().is_empty(),
        "an absent read is undecidable, not recovered"
    );
}

/// A switch-grade kick-rejected member is frozen by the messages-limiter, not
/// actually recovered — its idle-looking usage is exactly what the rejection
/// produces. Recovery must walk past it, never relink to it.
#[test]
fn scan_recovery_never_relinks_to_a_switch_grade_kick_rejected_member() {
    use super::{FetchStatus, KickBlock, KickBlocks, PendingSwitch, StatusStore, scan_recovery};

    let store = recoverable_store();
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let now = now_epoch_secs();
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        KickBlock {
            streak: 2,
            rejected: true,
            until: Some(now + 600),
            next_retry: now + 30,
        },
    )])));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &recovery_config(None, &["b"]),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert!(
        pending.lock().unwrap().is_empty(),
        "a switch-grade kick rejection must block recovery, not just auto-switch"
    );
}

/// The happy path: no pending switch, no active profile, a fresh read on a
/// chain member whose 5h window sits back under its threshold — queued.
#[test]
fn scan_recovery_queues_a_recovered_chain_member() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};

    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &recovery_config(None, &["b"]),
        &recoverable_store(),
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert_eq!(
        pending.lock().unwrap().clone(),
        HashSet::from(["b".to_string()]),
        "a recovered chain member must be queued for switch"
    );
}

/// Same recovered-usage shape as the happy path above, but the member is
/// disabled — the scan must never relink to it (mirrors the kick-rejected
/// exclusion just above).
#[test]
fn scan_recovery_never_relinks_to_a_disabled_member() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};
    use crate::profile::{AppConfig, AppState, Profile};

    let mut disabled_b = Profile::new("b".to_string(), None, None);
    disabled_b.disabled = true;
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            active_profile: None,
            fallback_chain: vec!["b".into()],
            ..AppState::default()
        },
        profiles: vec![disabled_b],
    }));

    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &config,
        &recoverable_store(),
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert!(
        pending.lock().unwrap().is_empty(),
        "a disabled member must never be relinked by the recovery scan"
    );
}

/// Same recovered-usage shape as the happy path, but the member's plan reads
/// canceled — `/v1/messages` 403s no matter how idle its cached 5h window
/// looks, so it must never be a relink target (mirrors the disabled/kick-
/// rejected exclusions above; twin of the `fully_clear_target` canceled fix
/// on the target-side walk).
#[test]
fn scan_recovery_never_relinks_to_a_canceled_member() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};
    use crate::usage::{PlanInfo, PlanTier};

    let store: UsageStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 10.0,
                resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
            }),
            plan: Some(PlanInfo {
                tier: PlanTier::Free,
                subscription_status: Some("canceled".to_string()),
            }),
            ..Default::default()
        },
    )])));
    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &recovery_config(None, &["b"]),
        &store,
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert!(
        pending.lock().unwrap().is_empty(),
        "a canceled member must never be relinked by the recovery scan"
    );
}

/// Same recovered-usage shape as the happy path, but the member is flagged
/// auth-broken (AUTH-1 quarantine) — its store entry is frozen at the last
/// successful read while every refresh is permanently rejected, so it must
/// never be a relink target (mirrors the disabled exclusion above).
#[test]
fn scan_recovery_never_relinks_to_an_auth_broken_member() {
    use super::{FetchStatus, KickBlocks, PendingSwitch, StatusStore, scan_recovery};
    use crate::profile::{AppConfig, AppState, Profile};

    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState {
            active_profile: None,
            fallback_chain: vec!["b".into()],
            auth_broken: vec!["b".into()],
            ..AppState::default()
        },
        profiles: vec![Profile::new("b".to_string(), None, None)],
    }));

    let status: StatusStore = Arc::new(RankedMutex::new(HashMap::from([(
        "b".to_string(),
        FetchStatus::Fresh,
    )])));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
    let pending: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));

    scan_recovery(
        &config,
        &recoverable_store(),
        &status,
        &Arc::new(RankedMutex::new(HashMap::new())),
        &kick_blocks,
        &pending,
    );

    assert!(
        pending.lock().unwrap().is_empty(),
        "an auth-broken member must never be relinked by the recovery scan"
    );
}

/// `spawn_refresher`'s kick-block seed must run on the CALLING thread, not
/// inside the spawned tick worker: nothing joins that worker, so a home-
/// derived path resolved on it could outlive a test's `HOME_OVERRIDE` and read
/// the operator's real home — live the moment the seed grows a write leg
/// (docs/internals.md's 2026-06-06 convention). Entering through
/// `spawn_refresher` itself (never `sync_kick_blocks_from_cache` directly) is
/// the only way to pin WHERE the seed runs; asserting immediately after return,
/// with no sleep or yield, is what makes the race decide against a broken
/// version instead of racing it.
#[test]
fn spawn_refresher_seeds_kick_blocks_before_returning() {
    use super::{KickBlock, KickBlocks, spawn_refresher};
    use crate::profile::{AppConfig, AppState};
    use crate::profile_cache::{KICK_BLOCK_CACHE_FILE, write_profile_cache};
    use std::sync::atomic::{AtomicBool, AtomicU64};

    let _home = crate::testutil::HomeSandbox::new();

    let cached = KickBlock {
        streak: 3,
        rejected: true,
        until: Some(1_700_000_600),
        next_retry: 1_700_000_100,
    };
    write_profile_cache("kitty", KICK_BLOCK_CACHE_FILE, &cached);

    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    }));
    let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));

    spawn_refresher(
        config,
        Arc::new(RankedMutex::new(vec![token("kitty")])),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(AtomicU64::new(REFRESH_INTERVAL_MS)),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::clone(&kick_blocks),
        Arc::new(RankedMutex::new(HashSet::new())),
        Arc::new(RankedMutex::new(false)),
        Arc::new(RankedMutex::new(HashSet::new())),
        Arc::new(RankedMutex::new(vec![])),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashSet::new())),
        // Pre-armed shutdown: if the `cfg!(test)` spawn-skip is ever removed
        // while the seed hoist stays, the tick thread this would spawn breaks
        // at its loop-top check instead of looping past this sandbox teardown.
        Arc::new(AtomicBool::new(true)),
        Arc::new(crate::daemon::FetchLease::new()),
    );

    // `cfg!(test)` makes `spawn_refresher` return without ever spawning the
    // tick thread, so the seed above is the ONLY thing that could have
    // populated `kick_blocks` — this is what pins the seed synchronous.
    assert_eq!(
        kick_blocks.lock().unwrap().get("kitty").copied(),
        Some(cached),
        "the on-disk kick block must be seeded before spawn_refresher returns"
    );
}
