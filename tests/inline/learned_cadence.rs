use super::{
    LEARNED_CEILING_MS, LEARNED_FLOOR_MS, LEARNED_QUIET_RESET_MS, LEARNED_STEP_MS,
    NORMAL_INTERVAL_MS, bump_down, bump_up,
};

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
