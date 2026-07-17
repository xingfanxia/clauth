//! CDX-5 P2: account selection + cooldown table tests.

use super::*;

fn member(name: &str) -> PoolMember {
    PoolMember {
        name: name.to_string(),
        cooldown_until_ms: 0,
        unavailable: false,
    }
}

#[test]
fn selection_is_sticky_to_an_available_active() {
    let pool = vec![member("a"), member("b"), member("c")];
    assert_eq!(
        select_account(&pool, Some("b"), 1000),
        Selection::Use("b".to_string()),
        "stays on the active account (prompt-cache affinity)"
    );
}

#[test]
fn selection_rotates_from_after_the_active_when_it_is_unavailable() {
    let mut pool = vec![member("a"), member("b"), member("c")];
    pool[1].unavailable = true; // active b is spent
    assert_eq!(
        select_account(&pool, Some("b"), 1000),
        Selection::Use("c".to_string()),
        "walks to the next member in chain order, wrapping"
    );
}

#[test]
fn selection_wraps_past_the_end() {
    let mut pool = vec![member("a"), member("b"), member("c")];
    pool[2].unavailable = true; // active c spent
    assert_eq!(
        select_account(&pool, Some("c"), 1000),
        Selection::Use("a".to_string())
    );
}

#[test]
fn cooldown_gates_availability_and_clears_at_the_deadline() {
    let mut pool = vec![member("a"), member("b")];
    pool[0].cooldown_until_ms = 5000; // a in cooldown until t=5000
    assert_eq!(
        select_account(&pool, Some("a"), 1000),
        Selection::Use("b".to_string()),
        "a is cooling; route to b"
    );
    assert_eq!(
        select_account(&pool, Some("a"), 5000),
        Selection::Use("a".to_string()),
        "at the deadline a is available again (sticky)"
    );
}

#[test]
fn selection_exhausted_when_all_unavailable_or_cooling() {
    let mut pool = vec![member("a"), member("b")];
    pool[0].unavailable = true;
    pool[1].cooldown_until_ms = 9000;
    assert_eq!(select_account(&pool, Some("a"), 1000), Selection::Exhausted);
}

#[test]
fn next_after_failure_skips_current_and_already_tried() {
    let pool = vec![member("a"), member("b"), member("c")];
    // a failed; b was already tried this request → c.
    assert_eq!(
        next_after_failure(&pool, "a", &["a".to_string(), "b".to_string()], 1000),
        Selection::Use("c".to_string())
    );
    // a failed, everything else tried → exhausted (no infinite walk).
    assert_eq!(
        next_after_failure(
            &pool,
            "a",
            &["a".to_string(), "b".to_string(), "c".to_string()],
            1000
        ),
        Selection::Exhausted
    );
}

#[test]
fn next_after_failure_skips_cooling_and_unavailable() {
    let mut pool = vec![member("a"), member("b"), member("c")];
    pool[1].cooldown_until_ms = 9000; // b cooling
    pool[2].unavailable = true; // c broken
    assert_eq!(
        next_after_failure(&pool, "a", &["a".to_string()], 1000),
        Selection::Exhausted
    );
}

#[test]
fn cooldown_stamp_honors_the_floor_and_an_advertised_reset() {
    let mut cd = Cooldowns::default();
    // No advertised reset → the 60s floor.
    cd.stamp("a", 1000, None);
    assert_eq!(cd.get("a"), 1000 + COOLDOWN_FLOOR_MS);
    // A farther advertised reset wins over the floor.
    let far = 1000 + COOLDOWN_FLOOR_MS + 500_000;
    cd.stamp("b", 1000, Some(far));
    assert_eq!(cd.get("b"), far);
    // A nearer advertised reset is clamped up to the floor (the ≥60s rule).
    cd.stamp("c", 1000, Some(1000 + 5_000));
    assert_eq!(cd.get("c"), 1000 + COOLDOWN_FLOOR_MS);
}
