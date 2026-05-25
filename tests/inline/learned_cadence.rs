use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::{
    ConsecutiveCacheHit, ConsecutiveOk, FetchStatus, LEARNED_CEILING_MS, LEARNED_FLOOR_MS,
    LEARNED_QUIET_RESET_MS, LEARNED_STEP_MS, Last429At, LearnedIntervals, NORMAL_INTERVAL_MS,
    bump_down, bump_up, update_learner,
};

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
fn bump_up_never_exceeds_ceiling() {
    // Jitter is applied after the multiplicative step, so even when the input
    // is already at the ceiling the result must not escape it.
    for _ in 0..20 {
        let result = bump_up(LEARNED_CEILING_MS);
        assert!(
            result <= LEARNED_CEILING_MS,
            "bump_up exceeded ceiling: {result}"
        );
        assert!(result >= LEARNED_FLOOR_MS);
    }
}

#[test]
fn bump_up_near_floor_never_goes_below_floor() {
    // Jitter subtracts up to raised/10 from the raised value. When the input
    // is near the floor, the raised value is small, so the subtracted jitter
    // could push below LEARNED_FLOOR_MS without the clamp.
    for _ in 0..20 {
        let at_floor = bump_up(LEARNED_FLOOR_MS);
        assert!(
            at_floor >= LEARNED_FLOOR_MS,
            "bump_up(FLOOR) = {at_floor} went below floor"
        );
        let just_above = bump_up(LEARNED_FLOOR_MS + 1);
        assert!(
            just_above >= LEARNED_FLOOR_MS,
            "bump_up(FLOOR+1) = {just_above} went below floor"
        );
    }
}

#[test]
fn bump_down_subtracts_step() {
    let result = bump_down(NORMAL_INTERVAL_MS);
    assert_eq!(result, NORMAL_INTERVAL_MS - LEARNED_STEP_MS);
}

#[test]
fn bump_down_saturates_at_floor() {
    assert_eq!(bump_down(LEARNED_FLOOR_MS), LEARNED_FLOOR_MS);
    // Values already below floor are also clamped up.
    assert_eq!(bump_down(LEARNED_FLOOR_MS / 2), LEARNED_FLOOR_MS);
}

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
    }
}

#[test]
fn two_cache_hits_at_floor_bump_up_to_floor_plus_step() {
    // Polling at FLOOR with unchanged utilization → server-side cache. After
    // two consecutive cache-hits the learner backs off by one STEP toward NORMAL.
    let (learned, ok, ch, l429) = make_learner_maps();
    learned.lock().unwrap().insert("p".into(), LEARNED_FLOOR_MS);

    update_learner("p", FetchStatus::Fresh, true, &learned, &ok, &ch, &l429);
    // first hit — no bump yet
    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        LEARNED_FLOOR_MS
    );

    update_learner("p", FetchStatus::Fresh, true, &learned, &ok, &ch, &l429);
    // second hit — bump to FLOOR + STEP, capped at NORMAL
    let expected = (LEARNED_FLOOR_MS + LEARNED_STEP_MS).min(NORMAL_INTERVAL_MS);
    assert_eq!(
        learned.lock().unwrap().get("p").copied().unwrap(),
        expected,
        "two cache-hits at FLOOR should bump interval up by LEARNED_STEP_MS"
    );
    // cache-hit counter resets so a third hit starts fresh
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
        "cache-hit backoff must not exceed NORMAL"
    );
}

#[test]
fn cache_hit_then_change_event_resumes_recovery() {
    // One cache-hit resets to 0 on a real change-event so the ok counter
    // starts clean and two genuine Fresh responses still trigger bump_down.
    let (learned, ok, ch, l429) = make_learner_maps();
    learned.lock().unwrap().insert("p".into(), LEARNED_FLOOR_MS);

    // first cache-hit — counter increments
    update_learner("p", FetchStatus::Fresh, true, &learned, &ok, &ch, &l429);
    assert_eq!(ch.lock().unwrap().get("p").copied().unwrap_or(0), 1);

    // real change-event — cache-hit counter must reset, ok counter starts
    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);
    assert_eq!(
        ch.lock().unwrap().get("p").copied().unwrap_or(0),
        0,
        "cache-hit counter should reset on a real Fresh"
    );
    let ok_after = ok.lock().unwrap().get("p").copied().unwrap_or(0);
    assert_eq!(ok_after, 1, "ok counter should start accumulating");

    // second real Fresh → bump_down fires
    update_learner("p", FetchStatus::Fresh, false, &learned, &ok, &ch, &l429);
    let interval = learned.lock().unwrap().get("p").copied().unwrap();
    assert_eq!(
        interval,
        bump_down(LEARNED_FLOOR_MS),
        "two real Freshs should trigger additive decrease"
    );
}
