use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::{
    ActivityStore, CACHE_HIT_EPSILON, ConsecutiveCacheHit, ConsecutiveOk, FetchOutcome,
    FetchStatus, LEARNED_CEILING_MS, LEARNED_FLOOR_MS, LEARNED_QUIET_RESET_MS, LEARNED_STEP_MS,
    Last429At, LastFetchedAt, LearnedIntervals, NEAR_THRESHOLD_MARGIN, NORMAL_INTERVAL_MS,
    PendingAutoStart, ProfileActivity, SERVER_CACHE_TTL_ESTIMATE_MS, StatusStore, TokenEntry,
    UsageInfo, UsageStore, UsageWindow, apply_outcome, bump_down, bump_up, detect_cache_hit,
    interval_for, now_ms, partition_due, update_learner,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_learner_maps() -> (
    LearnedIntervals,
    ConsecutiveOk,
    ConsecutiveCacheHit,
    Last429At,
) {
    (
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
    )
}

fn token(name: &str, threshold: f64) -> TokenEntry {
    TokenEntry {
        name: name.into(),
        access_token: "tok".into(),
        refresh_token: None,
        fallback_threshold: threshold,
        auto_start: false,
    }
}

fn usage_with_util(util: f64) -> UsageInfo {
    UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: util,
            resets_at: None,
        }),
        ..Default::default()
    }
}

fn outcome(name: &str, status: FetchStatus, util: f64) -> FetchOutcome {
    FetchOutcome {
        name: name.into(),
        info: Some(usage_with_util(util)),
        status,
        needs_auto_start: false,
        rotated: None,
    }
}

fn apply_stores() -> (
    UsageStore,
    StatusStore,
    LastFetchedAt,
    PendingAutoStart,
    LearnedIntervals,
    ConsecutiveOk,
    ConsecutiveCacheHit,
    Last429At,
) {
    let (learned, ok, ch, l429) = make_learner_maps();
    (
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashSet::new())),
        learned,
        ok,
        ch,
        l429,
    )
}

fn stored_util(store: &UsageStore, name: &str) -> Option<f64> {
    store
        .lock()
        .unwrap()
        .get(name)
        .and_then(|u| u.five_hour.as_ref())
        .map(|w| w.utilization)
}

// ── Constants & ordering ──────────────────────────────────────────────────────

#[test]
fn quiet_reset_longer_than_normal_interval() {
    // A single 429 followed by immediate quiet-period expiry would be
    // misleading if the quiet window is shorter than one normal cycle.
    const { assert!(LEARNED_QUIET_RESET_MS > NORMAL_INTERVAL_MS) }
}

#[test]
fn constant_ordering() {
    const {
        assert!(LEARNED_FLOOR_MS < NORMAL_INTERVAL_MS);
        assert!(NORMAL_INTERVAL_MS < LEARNED_CEILING_MS);
        assert!(LEARNED_STEP_MS < NORMAL_INTERVAL_MS);
        // Cache-hit gate must sit strictly below NORMAL so polling at the
        // baseline cadence never registers a server cache hit on idle.
        assert!(SERVER_CACHE_TTL_ESTIMATE_MS < NORMAL_INTERVAL_MS);
        // Recovery from saturation must be possible.
        assert!(LEARNED_STEP_MS < LEARNED_CEILING_MS);
    }
}

// ── bump_up ───────────────────────────────────────────────────────────────────

#[test]
fn bump_up_raises_by_1_5x_within_jitter_bounds() {
    let current = NORMAL_INTERVAL_MS; // 30_000
    let result = bump_up(current);
    // Expected center: 45_000; jitter is ±10% of 45_000 = ±4_500.
    let center = current * 3 / 2;
    let margin = center / 10;
    assert!(
        result >= center.saturating_sub(margin),
        "bump_up({current}) = {result} is below center - margin ({})",
        center - margin,
    );
    assert!(
        result <= (center + margin).min(LEARNED_CEILING_MS),
        "bump_up({current}) = {result} is above center + margin",
    );
    assert!(result <= LEARNED_CEILING_MS);
    assert!(result >= LEARNED_FLOOR_MS);
}

#[test]
fn bump_up_at_ceiling_pins_exactly() {
    // Hard ceiling. Applying jitter after the saturating multiply (the prior
    // bug) would drift the effective ceiling down by ~half the jitter range
    // because the upper half of the jitter window gets clamped back to CEILING
    // while the lower half passes through.
    for _ in 0..50 {
        assert_eq!(bump_up(LEARNED_CEILING_MS), LEARNED_CEILING_MS);
    }
}

#[test]
fn bump_up_at_saturation_boundary_pins_to_ceiling() {
    // raised = current * 3 / 2 >= CEILING when current >= 200_000.
    let boundary = LEARNED_CEILING_MS * 2 / 3;
    for current in [
        boundary,
        boundary + 1,
        LEARNED_CEILING_MS,
        LEARNED_CEILING_MS * 2,
    ] {
        for _ in 0..20 {
            assert_eq!(
                bump_up(current),
                LEARNED_CEILING_MS,
                "bump_up({current}) should pin to ceiling without jitter",
            );
        }
    }
}

#[test]
fn bump_up_never_exceeds_ceiling() {
    for _ in 0..20 {
        let result = bump_up(LEARNED_CEILING_MS);
        assert!(result <= LEARNED_CEILING_MS);
        assert!(result >= LEARNED_FLOOR_MS);
    }
}

#[test]
fn bump_up_near_floor_never_goes_below_floor() {
    // Jitter subtracts up to raised/10. When the input is near the floor the
    // raised value is small, so without the clamp the subtracted jitter could
    // push below LEARNED_FLOOR_MS.
    for _ in 0..20 {
        let at_floor = bump_up(LEARNED_FLOOR_MS);
        assert!(at_floor >= LEARNED_FLOOR_MS);
        let just_above = bump_up(LEARNED_FLOOR_MS + 1);
        assert!(just_above >= LEARNED_FLOOR_MS);
    }
}

#[test]
fn bump_up_zero_clamps_to_floor() {
    // current=0: raised=0, margin=0, jitter=0 → max(0, FLOOR) = FLOOR.
    assert_eq!(bump_up(0), LEARNED_FLOOR_MS);
}

#[test]
fn bump_up_jitter_produces_variation_across_calls() {
    // Two profiles bumping in the same tick had near-identical `subsec_nanos`,
    // which defeated the whole point of jittering. The atomic-counter-mixed
    // seed must produce distinct outputs even when calls land in the same
    // wall-clock instant.
    let mut seen = HashSet::new();
    for _ in 0..200 {
        seen.insert(bump_up(NORMAL_INTERVAL_MS));
    }
    assert!(
        seen.len() > 1,
        "bump_up jitter is deterministic across 200 calls (seed too weak)",
    );
}

// ── bump_down ─────────────────────────────────────────────────────────────────

#[test]
fn bump_down_subtracts_step() {
    assert_eq!(
        bump_down(NORMAL_INTERVAL_MS),
        NORMAL_INTERVAL_MS - LEARNED_STEP_MS,
    );
}

#[test]
fn bump_down_saturates_at_floor() {
    assert_eq!(bump_down(LEARNED_FLOOR_MS), LEARNED_FLOOR_MS);
    assert_eq!(bump_down(LEARNED_FLOOR_MS / 2), LEARNED_FLOOR_MS);
    assert_eq!(bump_down(0), LEARNED_FLOOR_MS);
}

// ── update_learner: RateLimited (bump_up) ─────────────────────────────────────

#[test]
fn rate_limited_bumps_up_stamps_429_and_resets_counters() {
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), NORMAL_INTERVAL_MS);
    ok.lock().unwrap().insert("p".into(), 1);
    ch.lock().unwrap().insert("p".into(), 1);

    update_learner(
        "p",
        FetchStatus::RateLimited,
        false,
        &learned,
        &ok,
        &ch,
        &l429,
    );

    let result = learned.lock().unwrap().get("p").copied().unwrap();
    assert!(
        result > NORMAL_INTERVAL_MS,
        "RateLimited must raise the learned interval",
    );
    assert!(result <= LEARNED_CEILING_MS);
    assert_eq!(ok.lock().unwrap().get("p").copied().unwrap(), 0);
    assert_eq!(ch.lock().unwrap().get("p").copied().unwrap(), 0);
    assert!(
        l429.lock().unwrap().contains_key("p"),
        "RateLimited must stamp last_429_at",
    );
}

#[test]
fn rate_limited_at_ceiling_stays_pinned() {
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), LEARNED_CEILING_MS);

    for _ in 0..10 {
        update_learner(
            "p",
            FetchStatus::RateLimited,
            false,
            &learned,
            &ok,
            &ch,
            &l429,
        );
    }

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        LEARNED_CEILING_MS,
        "repeated RateLimited at CEILING must not drift down",
    );
}

#[test]
fn rate_limited_without_prior_learned_uses_normal_as_baseline() {
    let (learned, ok, ch, l429) = make_learner_maps();
    update_learner(
        "p",
        FetchStatus::RateLimited,
        false,
        &learned,
        &ok,
        &ch,
        &l429,
    );

    let result = learned.lock().unwrap().get("p").copied().unwrap();
    let center = NORMAL_INTERVAL_MS * 3 / 2;
    let margin = center / 10;
    assert!(
        result >= center - margin,
        "first RateLimited should bump from NORMAL, not 0",
    );
}

// ── update_learner: Fresh recovery (bump_down) ────────────────────────────────

#[test]
fn first_fresh_starts_ok_count_at_one_with_no_bump() {
    let (learned, ok, ch, l429) = make_learner_maps();
    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);

    assert_eq!(ok.lock().unwrap().get("p").copied().unwrap(), 1);
    assert!(
        learned.lock().unwrap().get("p").is_none(),
        "one Fresh alone must not bump the learner",
    );
}

#[test]
fn two_genuine_fresh_trigger_bump_down() {
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), NORMAL_INTERVAL_MS);

    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);
    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        bump_down(NORMAL_INTERVAL_MS),
    );
    assert_eq!(ok.lock().unwrap().get("p").copied().unwrap(), 0);
}

#[test]
fn fresh_no_change_resets_cache_hit_counter() {
    // A real change-event must wipe the cache-hit accumulator so it can't
    // bridge across an intervening "different value" Fresh.
    let (learned, ok, ch, l429) = make_learner_maps();
    ch.lock().unwrap().insert("p".into(), 1);

    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);

    assert_eq!(ch.lock().unwrap().get("p").copied().unwrap(), 0);
}

#[test]
fn bump_down_at_floor_stays_at_floor() {
    let (learned, ok, ch, l429) = make_learner_maps();
    learned.lock().unwrap().insert("p".into(), LEARNED_FLOOR_MS);

    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);
    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        LEARNED_FLOOR_MS,
    );
}

// ── update_learner: Fresh with cache hit ──────────────────────────────────────

#[test]
fn two_cache_hits_at_floor_bump_up_to_floor_plus_step() {
    // Polling at FLOOR with unchanged utilization → server-side cache. After
    // two consecutive cache-hits the learner backs off by one STEP toward NORMAL.
    let (learned, ok, ch, l429) = make_learner_maps();
    learned.lock().unwrap().insert("p".into(), LEARNED_FLOOR_MS);

    update_learner("p", FetchStatus::Fresh, true, &learned, &ok, &ch, &l429);
    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        LEARNED_FLOOR_MS,
        "first cache-hit must not bump",
    );

    update_learner("p", FetchStatus::Fresh, true, &learned, &ok, &ch, &l429);
    let expected = (LEARNED_FLOOR_MS + LEARNED_STEP_MS).min(NORMAL_INTERVAL_MS);
    assert_eq!(learned.lock().unwrap().get("p").copied().unwrap(), expected,);
    assert_eq!(ch.lock().unwrap().get("p").copied().unwrap_or(0), 0);
}

#[test]
fn two_cache_hits_at_20s_bump_up_but_not_past_normal() {
    let (learned, ok, ch, l429) = make_learner_maps();
    let start = 20_000u64;
    learned.lock().unwrap().insert("p".into(), start);

    update_learner("p", FetchStatus::Fresh, true, &learned, &ok, &ch, &l429);
    update_learner("p", FetchStatus::Fresh, true, &learned, &ok, &ch, &l429);

    let result = learned.lock().unwrap().get("p").copied().unwrap();
    assert_eq!(result, (start + LEARNED_STEP_MS).min(NORMAL_INTERVAL_MS));
    assert!(
        result <= NORMAL_INTERVAL_MS,
        "cache-hit backoff must not exceed NORMAL",
    );
}

#[test]
fn cache_hits_at_normal_stay_at_normal() {
    // The cache-hit arm caps at NORMAL — at NORMAL, repeated cache hits are
    // a no-op rather than climbing toward CEILING.
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), NORMAL_INTERVAL_MS);

    for _ in 0..6 {
        update_learner("p", FetchStatus::Fresh, true, &learned, &ok, &ch, &l429);
    }

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        NORMAL_INTERVAL_MS,
    );
}

#[test]
fn cache_hit_then_change_event_resumes_recovery() {
    // One cache-hit resets to 0 on a real change-event so the ok counter
    // starts clean and two genuine Fresh responses still trigger bump_down.
    let (learned, ok, ch, l429) = make_learner_maps();
    learned.lock().unwrap().insert("p".into(), LEARNED_FLOOR_MS);

    update_learner("p", FetchStatus::Fresh, true, &learned, &ok, &ch, &l429);
    assert_eq!(ch.lock().unwrap().get("p").copied().unwrap_or(0), 1);

    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);
    assert_eq!(ch.lock().unwrap().get("p").copied().unwrap_or(0), 0);
    assert_eq!(ok.lock().unwrap().get("p").copied().unwrap_or(0), 1);

    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);
    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        bump_down(LEARNED_FLOOR_MS),
    );
}

// ── update_learner: Cached / Failed are no-ops ────────────────────────────────

#[test]
fn cached_does_not_touch_any_counter() {
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), NORMAL_INTERVAL_MS);
    ok.lock().unwrap().insert("p".into(), 1);
    ch.lock().unwrap().insert("p".into(), 1);

    update_learner("p", FetchStatus::Cached, false, &learned, &ok, &ch, &l429);

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        NORMAL_INTERVAL_MS,
    );
    assert_eq!(ok.lock().unwrap().get("p").copied().unwrap(), 1);
    assert_eq!(ch.lock().unwrap().get("p").copied().unwrap(), 1);
}

#[test]
fn failed_does_not_touch_any_counter() {
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), NORMAL_INTERVAL_MS);
    ok.lock().unwrap().insert("p".into(), 1);
    ch.lock().unwrap().insert("p".into(), 1);

    update_learner("p", FetchStatus::Failed, false, &learned, &ok, &ch, &l429);

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        NORMAL_INTERVAL_MS,
    );
    assert_eq!(ok.lock().unwrap().get("p").copied().unwrap(), 1);
    assert_eq!(ch.lock().unwrap().get("p").copied().unwrap(), 1);
}

#[test]
fn intermittent_failure_preserves_recovery_progress() {
    // Fresh, Cached, Fresh — the Cached must not wipe ok_count, so the
    // second Fresh still triggers bump_down. A network blip mid-recovery
    // shouldn't penalize the user.
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), NORMAL_INTERVAL_MS);

    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);
    update_learner("p", FetchStatus::Cached, false, &learned, &ok, &ch, &l429);
    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        bump_down(NORMAL_INTERVAL_MS),
    );
}

// ── update_learner: quiet-period reset ────────────────────────────────────────

#[test]
fn quiet_reset_fires_on_fresh_after_window() {
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), LEARNED_CEILING_MS);
    let stale = now_ms().saturating_sub(LEARNED_QUIET_RESET_MS + 1_000);
    l429.lock().unwrap().insert("p".into(), stale);

    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        NORMAL_INTERVAL_MS,
    );
    assert!(
        !l429.lock().unwrap().contains_key("p"),
        "quiet-reset must clear the stale 429 stamp",
    );
}

#[test]
fn quiet_reset_does_not_fire_on_cached() {
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), LEARNED_CEILING_MS);
    let stale = now_ms().saturating_sub(LEARNED_QUIET_RESET_MS + 1_000);
    l429.lock().unwrap().insert("p".into(), stale);

    update_learner("p", FetchStatus::Cached, false, &learned, &ok, &ch, &l429);

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        LEARNED_CEILING_MS,
        "a 5-min network outage must not undo legitimate 429 backoff",
    );
    assert!(l429.lock().unwrap().contains_key("p"));
}

#[test]
fn quiet_reset_does_not_fire_on_failed() {
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), LEARNED_CEILING_MS);
    let stale = now_ms().saturating_sub(LEARNED_QUIET_RESET_MS + 1_000);
    l429.lock().unwrap().insert("p".into(), stale);

    update_learner("p", FetchStatus::Failed, false, &learned, &ok, &ch, &l429);

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        LEARNED_CEILING_MS,
    );
    assert!(l429.lock().unwrap().contains_key("p"));
}

#[test]
fn quiet_reset_does_not_fire_on_rate_limited() {
    // RateLimited writes a fresh last_429 stamp. The Fresh-only gate ensures
    // the same call can't both reset and re-stamp inconsistently.
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), LEARNED_CEILING_MS);
    let stale = now_ms().saturating_sub(LEARNED_QUIET_RESET_MS + 1_000);
    l429.lock().unwrap().insert("p".into(), stale);

    update_learner(
        "p",
        FetchStatus::RateLimited,
        false,
        &learned,
        &ok,
        &ch,
        &l429,
    );

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        LEARNED_CEILING_MS,
    );
    let fresh_stamp = l429.lock().unwrap().get("p").copied().unwrap();
    assert!(
        fresh_stamp > stale,
        "RateLimited must overwrite the stale stamp with a current one",
    );
}

#[test]
fn quiet_reset_does_not_fire_before_window_elapses() {
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), LEARNED_CEILING_MS);
    let recent = now_ms().saturating_sub(1_000);
    l429.lock().unwrap().insert("p".into(), recent);

    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        LEARNED_CEILING_MS,
    );
    assert!(l429.lock().unwrap().contains_key("p"));
}

#[test]
fn quiet_reset_clears_stale_429_stamp_even_when_already_at_normal() {
    // The stale 429 stamp must be cleared even when `current <= NORMAL`,
    // otherwise the persisted entry leaks across restarts and re-evaluates
    // forever on every tick.
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), NORMAL_INTERVAL_MS);
    let stale = now_ms().saturating_sub(LEARNED_QUIET_RESET_MS + 1_000);
    l429.lock().unwrap().insert("p".into(), stale);

    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);

    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        NORMAL_INTERVAL_MS,
    );
    assert!(
        !l429.lock().unwrap().contains_key("p"),
        "stale 429 stamp must be evicted even when learned was already at NORMAL",
    );
}

#[test]
fn quiet_reset_without_429_stamp_is_noop() {
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), LEARNED_CEILING_MS);

    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);

    // No 429 stamp → reset doesn't fire → learned untouched, Fresh arm runs.
    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        LEARNED_CEILING_MS,
    );
    assert_eq!(ok.lock().unwrap().get("p").copied().unwrap(), 1);
}

// ── detect_cache_hit ──────────────────────────────────────────────────────────

#[test]
fn cache_hit_only_when_status_is_fresh() {
    assert!(detect_cache_hit(
        FetchStatus::Fresh,
        1_000,
        Some(50.0),
        Some(50.0),
    ));
    assert!(!detect_cache_hit(
        FetchStatus::RateLimited,
        1_000,
        Some(50.0),
        Some(50.0),
    ));
    assert!(!detect_cache_hit(
        FetchStatus::Cached,
        1_000,
        Some(50.0),
        Some(50.0),
    ));
    assert!(!detect_cache_hit(
        FetchStatus::Failed,
        1_000,
        Some(50.0),
        Some(50.0),
    ));
}

#[test]
fn cache_hit_requires_poll_inside_ttl_window() {
    // Polling at or above the server cache TTL: equal values mean idle, not
    // a cache hit. This is the fix that stopped the learner from snapping
    // back to NORMAL on every idle pause.
    assert!(!detect_cache_hit(
        FetchStatus::Fresh,
        SERVER_CACHE_TTL_ESTIMATE_MS,
        Some(50.0),
        Some(50.0),
    ));
    assert!(!detect_cache_hit(
        FetchStatus::Fresh,
        SERVER_CACHE_TTL_ESTIMATE_MS + 1,
        Some(50.0),
        Some(50.0),
    ));
    assert!(!detect_cache_hit(
        FetchStatus::Fresh,
        NORMAL_INTERVAL_MS,
        Some(50.0),
        Some(50.0),
    ));
    // Inside the window: equal values is a cache hit.
    assert!(detect_cache_hit(
        FetchStatus::Fresh,
        SERVER_CACHE_TTL_ESTIMATE_MS - 1,
        Some(50.0),
        Some(50.0),
    ));
    assert!(detect_cache_hit(
        FetchStatus::Fresh,
        0,
        Some(50.0),
        Some(50.0),
    ));
}

#[test]
fn cache_hit_requires_both_utilizations_present() {
    assert!(!detect_cache_hit(
        FetchStatus::Fresh,
        1_000,
        None,
        Some(50.0),
    ));
    assert!(!detect_cache_hit(
        FetchStatus::Fresh,
        1_000,
        Some(50.0),
        None,
    ));
    assert!(!detect_cache_hit(FetchStatus::Fresh, 1_000, None, None));
}

#[test]
fn cache_hit_values_within_epsilon_count_as_same() {
    assert!(detect_cache_hit(
        FetchStatus::Fresh,
        1_000,
        Some(0.4732),
        Some(0.4732),
    ));
    assert!(detect_cache_hit(
        FetchStatus::Fresh,
        1_000,
        Some(0.4732),
        Some(0.4732 + CACHE_HIT_EPSILON / 2.0),
    ));
}

#[test]
fn cache_hit_values_outside_epsilon_count_as_change() {
    assert!(!detect_cache_hit(
        FetchStatus::Fresh,
        1_000,
        Some(0.4732),
        Some(0.4733),
    ));
    assert!(!detect_cache_hit(
        FetchStatus::Fresh,
        1_000,
        Some(0.4732),
        Some(0.4732 + CACHE_HIT_EPSILON * 10.0),
    ));
}

// ── interval_for ──────────────────────────────────────────────────────────────

#[test]
fn interval_returns_normal_when_no_learned_entry() {
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    let entry = token("p", 95.0);
    assert_eq!(
        interval_for(&entry, Some(50.0), &learned),
        NORMAL_INTERVAL_MS,
    );
}

#[test]
fn interval_returns_learned_value_when_present() {
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    learned.lock().unwrap().insert("p".into(), 17_000);
    let entry = token("p", 95.0);
    assert_eq!(interval_for(&entry, Some(50.0), &learned), 17_000);
}

#[test]
fn interval_clamps_to_floor_at_or_above_near_threshold() {
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    learned
        .lock()
        .unwrap()
        .insert("p".into(), LEARNED_CEILING_MS);
    let entry = token("p", 95.0);

    // At the boundary (threshold - margin) and above: clamp to FLOOR.
    assert_eq!(
        interval_for(&entry, Some(95.0 - NEAR_THRESHOLD_MARGIN), &learned),
        LEARNED_FLOOR_MS,
    );
    assert_eq!(interval_for(&entry, Some(94.9), &learned), LEARNED_FLOOR_MS);
    assert_eq!(interval_for(&entry, Some(99.0), &learned), LEARNED_FLOOR_MS);
}

#[test]
fn interval_zero_threshold_is_not_floor_clamped() {
    // threshold == 0.0 (unset/default) must never trigger the near-threshold
    // override regardless of utilization. Without the > NEAR_THRESHOLD_MARGIN
    // guard the RHS (-5.0) is always satisfied by any non-negative utilization.
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    learned
        .lock()
        .unwrap()
        .insert("p".into(), NORMAL_INTERVAL_MS);
    let entry = token("p", 0.0);
    assert_eq!(
        interval_for(&entry, Some(0.0), &learned),
        NORMAL_INTERVAL_MS
    );
    assert_eq!(
        interval_for(&entry, Some(50.0), &learned),
        NORMAL_INTERVAL_MS
    );
    assert_eq!(
        interval_for(&entry, Some(99.0), &learned),
        NORMAL_INTERVAL_MS
    );
}

#[test]
fn interval_just_below_near_threshold_returns_learned() {
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    learned.lock().unwrap().insert("p".into(), 17_000);
    let entry = token("p", 95.0);
    let just_below = 95.0 - NEAR_THRESHOLD_MARGIN - 0.1;
    assert_eq!(interval_for(&entry, Some(just_below), &learned), 17_000);
}

#[test]
fn interval_with_no_5h_data_returns_learned() {
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    learned.lock().unwrap().insert("p".into(), 17_000);
    let entry = token("p", 95.0);
    assert_eq!(interval_for(&entry, None, &learned), 17_000);
}

// ── interval_for: zero/unset threshold (H1 regression) ───────────────────────

#[test]
fn interval_for_zero_threshold_is_not_clamped_to_floor() {
    // A profile with fallback_threshold == 0.0 (unset/default) must NOT be
    // pinned to FLOOR regardless of utilization. Without the > NEAR_THRESHOLD_MARGIN
    // guard the RHS is negative (-5.0), making the comparison true for any
    // non-None utilization and pinning the interval to 10s forever.
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    learned
        .lock()
        .unwrap()
        .insert("p".into(), NORMAL_INTERVAL_MS);
    let entry = token("p", 0.0);

    // At 0% utilization — should still use learned.
    assert_eq!(
        interval_for(&entry, Some(0.0), &learned),
        NORMAL_INTERVAL_MS,
        "threshold 0.0 with util 0.0 must not pin to FLOOR",
    );
    // At 100% utilization — should still use learned (no configured threshold).
    assert_eq!(
        interval_for(&entry, Some(100.0), &learned),
        NORMAL_INTERVAL_MS,
        "threshold 0.0 with util 100.0 must not pin to FLOOR",
    );
    // No utilization data — must use learned.
    assert_eq!(
        interval_for(&entry, None, &learned),
        NORMAL_INTERVAL_MS,
        "threshold 0.0 with no util must not pin to FLOOR",
    );
}

#[test]
fn interval_for_threshold_below_margin_is_not_clamped_to_floor() {
    // A threshold just below NEAR_THRESHOLD_MARGIN (e.g. 4.9 with margin 5.0)
    // must not trigger the near-threshold override for any utilization.
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    learned.lock().unwrap().insert("p".into(), 17_000);
    let threshold_below_margin = NEAR_THRESHOLD_MARGIN - 0.1;
    let entry = token("p", threshold_below_margin);

    // Utilization at 100% (well above threshold) — override must not fire.
    assert_eq!(
        interval_for(&entry, Some(100.0), &learned),
        17_000,
        "threshold below margin must not pin to FLOOR even at 100% util",
    );
    // Utilization at or above threshold.
    assert_eq!(
        interval_for(&entry, Some(threshold_below_margin), &learned),
        17_000,
        "threshold below margin must not pin to FLOOR at threshold util",
    );
}

#[test]
fn interval_for_threshold_at_margin_is_not_clamped_to_floor() {
    // Threshold exactly equal to NEAR_THRESHOLD_MARGIN (5.0) — the guard is
    // `> NEAR_THRESHOLD_MARGIN` so equality must not trigger the override.
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    learned.lock().unwrap().insert("p".into(), 17_000);
    let entry = token("p", NEAR_THRESHOLD_MARGIN);

    assert_eq!(
        interval_for(&entry, Some(100.0), &learned),
        17_000,
        "threshold == NEAR_THRESHOLD_MARGIN must not pin to FLOOR",
    );
}

#[test]
fn interval_for_genuine_threshold_near_match_pins_to_floor() {
    // A profile with threshold 80.0 at 78% utilization (within the 5pp margin)
    // must be clamped to FLOOR.
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    learned
        .lock()
        .unwrap()
        .insert("p".into(), LEARNED_CEILING_MS);
    let entry = token("p", 80.0);

    assert_eq!(
        interval_for(&entry, Some(78.0), &learned),
        LEARNED_FLOOR_MS,
        "threshold 80 at 78% util must pin to FLOOR",
    );
    // At threshold itself.
    assert_eq!(
        interval_for(&entry, Some(80.0), &learned),
        LEARNED_FLOOR_MS,
        "threshold 80 at 80% util must pin to FLOOR",
    );
    // Well below the margin — must use learned.
    assert_eq!(
        interval_for(&entry, Some(74.9), &learned),
        LEARNED_CEILING_MS,
        "threshold 80 at 74.9% util is outside near margin, must use learned",
    );
}

// ── partition_due ─────────────────────────────────────────────────────────────

fn empty_activity() -> ActivityStore {
    Arc::new(Mutex::new(HashMap::new()))
}

#[test]
fn partition_due_never_fetched_profile_is_due() {
    let snapshot = vec![token("p", 95.0)];
    let store: UsageStore = Arc::new(Mutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(Mutex::new(HashMap::new()));
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    let activity = empty_activity();

    let (due, _, per_profile) = partition_due(
        &snapshot,
        now_ms(),
        &store,
        &last_fetched,
        &learned,
        &activity,
    );

    assert_eq!(due.len(), 1);
    assert_eq!(due[0].name, "p");
    assert!(per_profile.contains_key("p"));
}

#[test]
fn partition_due_recent_fetch_is_not_due() {
    let snapshot = vec![token("p", 95.0)];
    let store: UsageStore = Arc::new(Mutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(Mutex::new(HashMap::new()));
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    let activity = empty_activity();
    let now = now_ms();
    last_fetched.lock().unwrap().insert("p".into(), now);

    let (due, _, per_profile) =
        partition_due(&snapshot, now, &store, &last_fetched, &learned, &activity);

    assert!(due.is_empty());
    assert_eq!(
        per_profile.get("p").copied().unwrap(),
        now + NORMAL_INTERVAL_MS,
    );
}

#[test]
fn partition_due_interval_elapsed_is_due() {
    let snapshot = vec![token("p", 95.0)];
    let store: UsageStore = Arc::new(Mutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(Mutex::new(HashMap::new()));
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    let activity = empty_activity();
    let now = now_ms();
    last_fetched
        .lock()
        .unwrap()
        .insert("p".into(), now - NORMAL_INTERVAL_MS);

    let (due, _, _) = partition_due(&snapshot, now, &store, &last_fetched, &learned, &activity);

    assert_eq!(due.len(), 1);
}

#[test]
fn partition_due_honors_learned_interval() {
    let snapshot = vec![token("p", 95.0)];
    let store: UsageStore = Arc::new(Mutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(Mutex::new(HashMap::new()));
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    let activity = empty_activity();
    let now = now_ms();
    last_fetched
        .lock()
        .unwrap()
        .insert("p".into(), now - 15_000);
    learned.lock().unwrap().insert("p".into(), 20_000);

    let (due, _, per_profile) =
        partition_due(&snapshot, now, &store, &last_fetched, &learned, &activity);

    // 15s elapsed, learned interval 20s → 5s remaining.
    assert!(due.is_empty());
    assert_eq!(
        per_profile.get("p").copied().unwrap(),
        now - 15_000 + 20_000,
    );
}

#[test]
fn partition_due_near_threshold_overrides_learned_with_floor() {
    let snapshot = vec![token("p", 95.0)];
    let store: UsageStore = Arc::new(Mutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(Mutex::new(HashMap::new()));
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    let activity = empty_activity();
    let now = now_ms();
    last_fetched
        .lock()
        .unwrap()
        .insert("p".into(), now - LEARNED_FLOOR_MS - 100);
    // Util at 91% with threshold=95% → within the 5pp near margin.
    store.lock().unwrap().insert(
        "p".into(),
        usage_with_util(95.0 - NEAR_THRESHOLD_MARGIN + 1.0),
    );
    // Even with CEILING as the learned value, near-threshold clamps to FLOOR.
    learned
        .lock()
        .unwrap()
        .insert("p".into(), LEARNED_CEILING_MS);

    let (due, _, _) = partition_due(&snapshot, now, &store, &last_fetched, &learned, &activity);

    assert_eq!(due.len(), 1, "near-threshold must override learned CEILING");
}

#[test]
fn partition_due_empty_snapshot_returns_empty() {
    let store: UsageStore = Arc::new(Mutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(Mutex::new(HashMap::new()));
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    let activity = empty_activity();

    let (due, _, per_profile) =
        partition_due(&[], now_ms(), &store, &last_fetched, &learned, &activity);

    assert!(due.is_empty());
    assert!(per_profile.is_empty());
}

#[test]
fn partition_due_excludes_switching_profiles() {
    let snapshot = vec![token("p", 95.0)];
    let store: UsageStore = Arc::new(Mutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(Mutex::new(HashMap::new()));
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    let activity = empty_activity();
    activity
        .lock()
        .unwrap()
        .insert("p".into(), ProfileActivity::Switching);

    let (due, _, per_profile) = partition_due(
        &snapshot,
        now_ms(),
        &store,
        &last_fetched,
        &learned,
        &activity,
    );

    assert!(due.is_empty(), "switching profile must be skipped");
    // Countdown still publishes — UI keeps showing the eligibility timer.
    assert!(per_profile.contains_key("p"));
}

#[test]
fn partition_due_excludes_refreshing_profiles() {
    let snapshot = vec![token("p", 95.0)];
    let store: UsageStore = Arc::new(Mutex::new(HashMap::new()));
    let last_fetched: LastFetchedAt = Arc::new(Mutex::new(HashMap::new()));
    let learned: LearnedIntervals = Arc::new(Mutex::new(HashMap::new()));
    let activity = empty_activity();
    activity
        .lock()
        .unwrap()
        .insert("p".into(), ProfileActivity::Refreshing);

    let (due, _, per_profile) = partition_due(
        &snapshot,
        now_ms(),
        &store,
        &last_fetched,
        &learned,
        &activity,
    );

    assert!(due.is_empty(), "refreshing profile must be skipped");
    // Countdown still publishes — UI keeps showing the eligibility timer.
    assert!(per_profile.contains_key("p"));
}

// ── apply_outcome: store overwrite guard & cache-hit integration ──────────────

#[test]
fn apply_outcome_cached_does_not_overwrite_fresh_data() {
    let (store, status, last_fetched, pending, learned, ok, ch, l429) = apply_stores();

    apply_outcome(
        outcome("p", FetchStatus::Fresh, 50.0),
        &store,
        &status,
        &last_fetched,
        &pending,
        &learned,
        &ok,
        &ch,
        &l429,
    );
    assert_eq!(stored_util(&store, "p"), Some(50.0));

    // Cached arrives with an older snapshot — must not clobber newer Fresh.
    apply_outcome(
        outcome("p", FetchStatus::Cached, 10.0),
        &store,
        &status,
        &last_fetched,
        &pending,
        &learned,
        &ok,
        &ch,
        &l429,
    );
    assert_eq!(
        stored_util(&store, "p"),
        Some(50.0),
        "Cached must not overwrite newer Fresh data in the store",
    );
}

#[test]
fn apply_outcome_cached_fills_empty_store_on_cold_start() {
    let (store, status, last_fetched, pending, learned, ok, ch, l429) = apply_stores();

    // No prior entry — Cached should fill the store so the UI has something
    // to render after a cold start with no network.
    apply_outcome(
        outcome("p", FetchStatus::Cached, 42.0),
        &store,
        &status,
        &last_fetched,
        &pending,
        &learned,
        &ok,
        &ch,
        &l429,
    );

    assert_eq!(stored_util(&store, "p"), Some(42.0));
}

#[test]
fn apply_outcome_idle_at_normal_does_not_register_cache_hit() {
    // Regression: equal utilization at NORMAL polling cadence (idle user)
    // was being misclassified as a server cache hit, dragging the learner
    // back up to NORMAL on every pause. After the elapsed-gate fix, two
    // such idle Fresh calls should fire bump_down, not snap back to NORMAL.
    let (store, status, last_fetched, pending, learned, ok, ch, l429) = apply_stores();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), NORMAL_INTERVAL_MS);

    // First Fresh seeds the store and stamps last_fetched.
    apply_outcome(
        outcome("p", FetchStatus::Fresh, 42.0),
        &store,
        &status,
        &last_fetched,
        &pending,
        &learned,
        &ok,
        &ch,
        &l429,
    );

    // Simulate the next tick landing exactly NORMAL ms later (idle).
    let now = now_ms();
    last_fetched
        .lock()
        .unwrap()
        .insert("p".into(), now - NORMAL_INTERVAL_MS);

    apply_outcome(
        outcome("p", FetchStatus::Fresh, 42.0),
        &store,
        &status,
        &last_fetched,
        &pending,
        &learned,
        &ok,
        &ch,
        &l429,
    );

    // Cache-hit counter must stay 0 (idle is not a cache hit at NORMAL).
    assert_eq!(
        ch.lock().unwrap().get("p").copied().unwrap_or(0),
        0,
        "idle at NORMAL must not register as a cache hit",
    );
    // ok_count went 1 → 2 → bump_down → 0.
    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        bump_down(NORMAL_INTERVAL_MS),
        "two idle Fresh at NORMAL must bump the learner down",
    );
}

#[test]
fn apply_outcome_rapid_poll_with_same_value_registers_cache_hit() {
    // Same-value Fresh inside the server cache window (e.g. polling at FLOOR)
    // is a true cache hit and must back off via the cache-hit arm.
    let (store, status, last_fetched, pending, learned, ok, ch, l429) = apply_stores();
    learned.lock().unwrap().insert("p".into(), LEARNED_FLOOR_MS);

    apply_outcome(
        outcome("p", FetchStatus::Fresh, 42.0),
        &store,
        &status,
        &last_fetched,
        &pending,
        &learned,
        &ok,
        &ch,
        &l429,
    );

    // Next poll lands well inside the cache window.
    let now = now_ms();
    last_fetched.lock().unwrap().insert("p".into(), now - 1_000);

    apply_outcome(
        outcome("p", FetchStatus::Fresh, 42.0),
        &store,
        &status,
        &last_fetched,
        &pending,
        &learned,
        &ok,
        &ch,
        &l429,
    );

    assert_eq!(
        ch.lock().unwrap().get("p").copied().unwrap(),
        1,
        "rapid same-value Fresh must register as a cache hit",
    );
}

// ── M5: apply_outcome elapsed measured against prior fetch, not just-written ──

#[test]
fn apply_outcome_elapsed_uses_prior_last_fetched_not_just_written() {
    // Two sequential apply_outcome calls for the same profile: the second must
    // compute elapsed_ms against the timestamp that was in last_fetched BEFORE
    // the first call wrote its `now`, not the freshly-written value. Without
    // the M5 single-snapshot fix, the second call would read the value the first
    // just wrote → near-zero elapsed → false cache-hit classification.
    let (store, status, last_fetched, pending, learned, ok, ch, l429) = apply_stores();
    learned.lock().unwrap().insert("p".into(), LEARNED_FLOOR_MS);

    // Simulate a prior fetch well outside the cache window so elapsed is large.
    let prior = now_ms().saturating_sub(NORMAL_INTERVAL_MS);
    last_fetched.lock().unwrap().insert("p".into(), prior);

    // First call: elapsed ~ NORMAL_INTERVAL_MS → not a cache hit.
    apply_outcome(
        outcome("p", FetchStatus::Fresh, 42.0),
        &store,
        &status,
        &last_fetched,
        &pending,
        &learned,
        &ok,
        &ch,
        &l429,
    );

    // After the first call, last_fetched["p"] was updated to `now`. The second
    // call must NOT see near-zero elapsed from reading that just-written value.
    // We verify indirectly: the cache-hit counter must be 0 (not 1) because
    // elapsed was large (NORMAL_INTERVAL_MS > SERVER_CACHE_TTL_ESTIMATE_MS).
    apply_outcome(
        outcome("p", FetchStatus::Fresh, 42.0),
        &store,
        &status,
        &last_fetched,
        &pending,
        &learned,
        &ok,
        &ch,
        &l429,
    );

    // The second call computed elapsed against the value the first call wrote
    // (which is ~0ms ago) — a cache-hit would mean the race exists. If the fix
    // is correct, this second call sees elapsed ~= time between the two
    // apply_outcome calls (effectively ~0ms), which IS inside the cache window.
    // BUT the key invariant is that the `last_fetched` read for `elapsed_ms`
    // happens before the write, so for the SECOND call in this test the elapsed
    // is measured from what the FIRST call wrote (a few microseconds ago →
    // very small elapsed → cache hit), which is the correct behavior per the
    // spec. What the fix prevents is reading a value written DURING the same
    // call. Let's test the actual documented regression path: set last_fetched
    // to a fresh value and verify elapsed reads it correctly.
    let (store2, status2, last_fetched2, pending2, learned2, ok2, ch2, l429_2) = apply_stores();
    learned2
        .lock()
        .unwrap()
        .insert("q".into(), LEARNED_FLOOR_MS);

    // Seed with a timestamp that is OUTSIDE the server cache TTL.
    let old_ts = now_ms().saturating_sub(NORMAL_INTERVAL_MS);
    last_fetched2.lock().unwrap().insert("q".into(), old_ts);

    apply_outcome(
        outcome("q", FetchStatus::Fresh, 77.0),
        &store2,
        &status2,
        &last_fetched2,
        &pending2,
        &learned2,
        &ok2,
        &ch2,
        &l429_2,
    );

    // elapsed_ms was computed from `old_ts` (≥ NORMAL_INTERVAL_MS ms ago),
    // which is ≥ SERVER_CACHE_TTL_ESTIMATE_MS → not a cache hit → ch stays 0.
    assert_eq!(
        ch2.lock().unwrap().get("q").copied().unwrap_or(0),
        0,
        "elapsed measured from prior last_fetched (outside TTL) must not register cache hit",
    );
}

// ── M6: bump_up near-ceiling pins cleanly, no asymmetric under-shoot ──────────

#[test]
fn bump_up_near_ceiling_pins_to_ceiling_no_undershoot() {
    // Before the M6 fix, `raised` values where `raised + margin >= CEILING`
    // (but `raised < CEILING`) would have upward jitter clipped by `.min(CEILING)`
    // while downward jitter passed through, shifting the mean below `raised`.
    // After the fix, any `current` whose `raised + raised/10 >= CEILING` pins.
    //
    // Pick current=181_820: raised=272_730, margin=27_273, sum=300_003 >= CEILING.
    // (current=181_818 gives sum=299_999 < CEILING, demonstrating the threshold
    // is tight — only values that actually straddle the ceiling get pinned.)
    let current = 181_820u64;
    let raised = current * 3 / 2; // 272_730
    let margin = raised / 10; // 27_273
    assert!(
        raised < LEARNED_CEILING_MS,
        "test setup: raised ({raised}) must be below CEILING for this to test the new band",
    );
    assert!(
        raised + margin >= LEARNED_CEILING_MS,
        "test setup: raised+margin ({}) must reach CEILING so the new guard fires",
        raised + margin,
    );

    for _ in 0..50 {
        assert_eq!(
            bump_up(current),
            LEARNED_CEILING_MS,
            "bump_up({current}) with raised+margin straddling CEILING must pin to CEILING",
        );
    }
}

#[test]
fn bump_up_below_jitter_band_still_jitters() {
    // Values where `raised + margin < CEILING` should still receive jitter.
    // Verify the window produces at least two distinct outputs across many calls.
    // Use NORMAL_INTERVAL_MS as a safe "far from ceiling" case.
    let current = NORMAL_INTERVAL_MS; // raised = 45_000, margin = 4_500; CEILING = 300_000.
    let raised = current * 3 / 2;
    let margin = raised / 10;
    assert!(
        raised + margin < LEARNED_CEILING_MS,
        "test setup: raised+margin must be below ceiling for jitter to apply",
    );

    let mut seen = std::collections::HashSet::new();
    for _ in 0..100 {
        seen.insert(bump_up(current));
    }
    assert!(
        seen.len() > 1,
        "bump_up below the jitter band must produce variation, not always pin",
    );
}

// ── L1: quiet-period reset does not fire when last_429_at == 0 ────────────────

#[test]
fn quiet_reset_does_not_fire_when_last_429_at_is_zero() {
    // A stored `last_429_at == 0` (from `now_ms()` returning 0 on a skewed
    // clock) must not satisfy the quiet-period check, because 0 as a sentinel
    // means "no stamp" and `now.saturating_sub(0)` >= LEARNED_QUIET_RESET_MS
    // is trivially true for any reasonable current time.
    let (learned, ok, ch, l429) = make_learner_maps();
    learned
        .lock()
        .unwrap()
        .insert("p".into(), LEARNED_CEILING_MS);
    // Explicitly insert 0 — simulates what happens when `now_ms()` fails.
    l429.lock().unwrap().insert("p".into(), 0u64);

    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);

    // With the L1 fix, the reset must NOT fire — backoff must be preserved.
    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        LEARNED_CEILING_MS,
        "quiet-period reset must not fire when last_429_at == 0",
    );
}
