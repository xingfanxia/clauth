use super::*;

// --- gap_boundary ---

#[test]
fn gap_boundary_returns_0_when_no_gap() {
    let entries = vec![(1000, 10.0), (2000, 20.0), (3000, 30.0)];
    assert_eq!(gap_boundary(&entries, 5000), 0);
}

#[test]
fn gap_boundary_returns_0_when_util_changed() {
    let entries = vec![(1000, 10.0), (3_700_000, 20.0)];
    assert_eq!(gap_boundary(&entries, 3_600_000), 0);
}

#[test]
fn gap_boundary_returns_0_when_gap_too_small() {
    let entries = vec![(1000, 10.0), (2000, 10.0)];
    assert_eq!(gap_boundary(&entries, 5000), 0);
}

#[test]
fn gap_boundary_cuts_at_idle_gap() {
    let entries = vec![
        (1000, 30.0),
        (3_700_000, 30.0), // ~1h later, same util
    ];
    assert_eq!(gap_boundary(&entries, 600_000), 1);
}

#[test]
fn gap_boundary_finds_most_recent_gap() {
    let entries = vec![
        (1000, 30.0),
        (3_700_000, 30.0), // gap 1: ~1h, same util
        (4_000_000, 31.0),
        (8_000_000, 31.0), // gap 2: ~1.1h, same util
    ];
    assert_eq!(gap_boundary(&entries, 600_000), 3);
}

#[test]
fn gap_boundary_clock_step_backwards_no_panic() {
    let now = 1_200_000_000u64;
    let entries = vec![
        (now, 30.0),
        (now - 3_600_000, 30.0), // 1h in the past (clock rewind)
    ];
    // saturating_sub gives 0, not > max_gap_ms
    assert_eq!(gap_boundary(&entries, 600_000), 0);
}

// --- compute_burn_rates_from_history ---
// Timestamps are relative to `crate::usage::now_ms()` so the real clock
// inside the function produces plausible dt values.

fn make_win(util: f64) -> UsageWindow {
    UsageWindow {
        utilization: util,
        resets_at: None,
    }
}

fn make_info(five_h: Option<f64>, seven_d: Option<f64>) -> UsageInfo {
    UsageInfo {
        plan: None,
        five_hour: five_h.map(make_win),
        seven_day: seven_d.map(make_win),
        seven_day_opus: None,
        seven_day_sonnet: None,
        extra_usage: None,
    }
}

#[test]
fn five_h_idle_gap_yields_burst_rate() {
    let now = crate::usage::now_ms();
    // util flat at 30% for 1h, then burst 30%→33% over 5 min
    let history = vec![
        ((now - 3_900_000), make_info(Some(30.0), None)), // pre-idle
        ((now - 300_000), make_info(Some(30.0), None)),   // bridge
        ((now - 240_000), make_info(Some(31.0), None)),
        ((now - 180_000), make_info(Some(32.0), None)),
        ((now - 120_000), make_info(Some(32.5), None)),
        ((now - 60_000), make_info(Some(33.0), None)),
    ];
    let five_h = make_win(33.0);

    let rates = compute_burn_rates_from_history(
        &history,
        &[("5h", &five_h)],
        5,
        30 * 60 * 1000,
        10 * 60 * 1000,
    );
    let rate = rates.get("5h").copied().flatten().unwrap();
    // 33-30=3% over 300s=0.083h → 36 %/h
    assert!((rate - 36.0).abs() < 3.0, "rate={rate}");
}

#[test]
fn seven_d_gap_cut_disabled_preserves_idle() {
    let now = crate::usage::now_ms();
    // ≈3 days of entries with gradually increasing util, no flat plateaus
    let history = vec![
        ((now - 70 * 3_600_000), make_info(None, Some(5.0))),
        ((now - 58 * 3_600_000), make_info(None, Some(8.0))),
        ((now - 46 * 3_600_000), make_info(None, Some(11.0))),
        ((now - 34 * 3_600_000), make_info(None, Some(14.0))),
        ((now - 22 * 3_600_000), make_info(None, Some(17.0))),
        ((now - 10 * 3_600_000), make_info(None, Some(19.0))),
        ((now - 4 * 3_600_000), make_info(None, Some(20.0))),
    ];
    let seven_d = make_win(21.0);

    let rates =
        compute_burn_rates_from_history(&history, &[("7d", &seven_d)], 50, 24 * 60 * 60 * 1000, 0);
    let rate = rates.get("7d").copied().flatten().unwrap();
    // full span ~70h, delta ~16%, rate ≈ 0.23 %/h
    assert!(
        rate < 5.0,
        "rate={rate} should be diluted by full history span"
    );
}

#[test]
fn seven_d_gap_cut_enabled_overstates_during_activity() {
    let now = crate::usage::now_ms();
    // Same 3d span but with an overnight plateau of same util entries
    // that triggers a gap cut, removing everything before the plateau.
    let history = vec![
        ((now - 70 * 3_600_000), make_info(None, Some(5.0))),
        ((now - 58 * 3_600_000), make_info(None, Some(8.0))),
        ((now - 46 * 3_600_000), make_info(None, Some(11.0))),
        ((now - 34 * 3_600_000), make_info(None, Some(14.0))), // plateau start
        ((now - 22 * 3_600_000), make_info(None, Some(14.0))), // 12h gap, same util
        ((now - 10 * 3_600_000), make_info(None, Some(17.0))),
        ((now - 4 * 3_600_000), make_info(None, Some(19.0))),
    ];
    let seven_d = make_win(20.0);

    let rates = compute_burn_rates_from_history(
        &history,
        &[("7d", &seven_d)],
        50,
        24 * 60 * 60 * 1000,
        10 * 60 * 1000,
    );
    let rate = rates.get("7d").copied().flatten().unwrap();
    // cut at (now-22h,14), span from there ≈ 22h, delta 6% → ~0.27 %/h
    // still not dramatic (7d windows are sluggish by nature), but must be
    // higher than the gap_cut=0 variant of the same data
    assert!(
        rate > 0.2,
        "rate={rate} should be higher after cutting idle span"
    );
}

#[test]
fn steady_drain_no_gap_no_cut() {
    let now = crate::usage::now_ms();
    let history = vec![
        ((now - 600_000), make_info(Some(10.0), None)),
        ((now - 480_000), make_info(Some(12.0), None)),
        ((now - 360_000), make_info(Some(14.0), None)),
        ((now - 240_000), make_info(Some(16.0), None)),
        ((now - 120_000), make_info(Some(18.0), None)),
    ];
    let five_h = make_win(20.0);

    let rates = compute_burn_rates_from_history(
        &history,
        &[("5h", &five_h)],
        5,
        30 * 60 * 1000,
        10 * 60 * 1000,
    );
    let rate = rates.get("5h").copied().flatten().unwrap();
    // 20-10=10% over 600s=0.167h → ~60 %/h
    assert!((rate - 60.0).abs() < 5.0, "rate={rate}");
}
