use super::*;

// 2026-05-17T14:20:00 UTC == 1779027600 epoch seconds.
const BASE_UTC: i64 = 1_779_027_600;

#[test]
fn parses_z_suffix() {
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00Z"), Some(BASE_UTC));
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00.121699Z"),
        Some(BASE_UTC)
    );
}

#[test]
fn parses_colon_offset() {
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+00:00"),
        Some(BASE_UTC)
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00.121699+00:00"),
        Some(BASE_UTC)
    );
    // +05:30 is 5h30m ahead, so the UTC instant is earlier.
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+05:30"),
        Some(BASE_UTC - (5 * 3600 + 30 * 60))
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00-05:30"),
        Some(BASE_UTC + (5 * 3600 + 30 * 60))
    );
}

#[test]
fn parses_colonless_offset() {
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+0000"),
        Some(BASE_UTC)
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00.121699+0000"),
        Some(BASE_UTC)
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+0530"),
        Some(BASE_UTC - (5 * 3600 + 30 * 60))
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00-0530"),
        Some(BASE_UTC + (5 * 3600 + 30 * 60))
    );
}

#[test]
fn parses_bare_hour_offset() {
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00+00"), Some(BASE_UTC));
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+05"),
        Some(BASE_UTC - 5 * 3600)
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00-05"),
        Some(BASE_UTC + 5 * 3600)
    );
}

#[test]
fn colon_and_colonless_agree() {
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+0530"),
        iso_to_epoch_secs("2026-05-17T14:20:00+05:30")
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00-0800"),
        iso_to_epoch_secs("2026-05-17T14:20:00-08:00")
    );
}

#[test]
fn rejects_malformed() {
    // Too short to hold a date-time.
    assert_eq!(iso_to_epoch_secs("2026-05-17"), None);
    // Bad separators.
    assert_eq!(iso_to_epoch_secs("2026/05/17T14:20:00Z"), None);
    // Non-sign, non-Z trailing char.
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00X"), None);
    // Garbage in the offset.
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00+ab:cd"), None);
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00+5"), None);
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00+12345"), None);
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00+05:3"), None);
}

/// `epoch_secs_to_iso` is the exact inverse of `iso_to_epoch_secs`: a round
/// trip through the formatter lands on the same epoch, and the output uses the
/// `+00:00` shape the parser accepts.
#[test]
fn epoch_to_iso_round_trips() {
    assert_eq!(epoch_secs_to_iso(BASE_UTC), "2026-05-17T14:20:00+00:00");
    for secs in [0, BASE_UTC, 951_867_122, 4_102_444_799] {
        assert_eq!(
            iso_to_epoch_secs(&epoch_secs_to_iso(secs)),
            Some(secs),
            "round trip failed for {secs}"
        );
    }
    // Negative input clamps to epoch 0 instead of underflowing the calendar.
    assert_eq!(epoch_secs_to_iso(-1), "1970-01-01T00:00:00+00:00");
}

/// `retry-after` parsing accepts the delta-seconds form and the IMF-fixdate
/// HTTP-date form; a future date becomes the delay until it, a past date is
/// `Duration::ZERO`, and garbage is no-hint (`None`).
#[test]
fn retry_after_parses_delta_seconds_and_http_date() {
    assert_eq!(parse_retry_after("5"), Some(Duration::from_secs(5)));
    assert_eq!(parse_retry_after(" 30 "), Some(Duration::from_secs(30)));
    assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
    assert_eq!(parse_retry_after(""), None);
    assert_eq!(parse_retry_after("-5"), None);
    assert_eq!(parse_retry_after("1.5"), None);

    // HTTP-date form, evaluated against a fixed instant for determinism.
    // "Wed, 21 Oct 2015 07:28:00 GMT" == 1_445_412_480 epoch seconds.
    let date = "Wed, 21 Oct 2015 07:28:00 GMT";
    assert_eq!(
        parse_retry_after_at(date, 1_445_412_480),
        Some(Duration::ZERO)
    );
    assert_eq!(
        parse_retry_after_at(date, 1_445_412_480 - 90),
        Some(Duration::from_secs(90)),
        "a future HTTP-date yields the delay until it"
    );
    assert_eq!(
        parse_retry_after_at(date, 1_445_412_480 + 120),
        Some(Duration::ZERO),
        "a past HTTP-date saturates to zero"
    );
    // Malformed HTTP-dates are no-hint.
    assert_eq!(
        parse_retry_after_at("Wed, 21 Foo 2015 07:28:00 GMT", 0),
        None
    );
    assert_eq!(parse_retry_after_at("21 Oct 2015 07:28:00 GMT", 0), None);
    assert_eq!(
        parse_retry_after_at("Wed, 21 Oct 2015 07:28:00 PST", 0),
        None
    );
}

/// `reserve_slot` spaces OAuth requests by `OAUTH_REQUEST_SPACING_MS`: a cold
/// slot (now already past it) fires immediately, and a slot still ahead of now
/// waits until it — each reservation advancing the slot by exactly one spacing.
#[test]
fn reserve_slot_spaces_requests() {
    let now = 1_000_000u64;
    // Cold slot in the past → fire now, reserve one spacing out.
    let (next, wait) = reserve_slot(0, now);
    assert_eq!(wait, 0);
    assert_eq!(next, now + OAUTH_REQUEST_SPACING_MS);
    // Slot already ahead of now → wait until it, advance by one more spacing.
    let (next2, wait2) = reserve_slot(next, now);
    assert_eq!(wait2, OAUTH_REQUEST_SPACING_MS);
    assert_eq!(next2, next + OAUTH_REQUEST_SPACING_MS);
}

/// The ideal-pace marker tracks the fraction of the window already elapsed:
/// half-elapsed → 50%, just-opened → 0%, lapsed → clamped to 100%. Windows with
/// no reset time or no fixed duration return `None` (no marker).
#[test]
fn ideal_pace_tracks_elapsed_window_fraction() {
    let win = |reset_secs: i64| UsageWindow {
        utilization: 0.0,
        resets_at: Some(epoch_secs_to_iso(reset_secs)),
    };

    // 5h window with 2.5h left → half elapsed → 50%.
    let p = ideal_pace_pct(LABEL_5H, &win(BASE_UTC + 9_000), BASE_UTC).unwrap();
    assert!(
        (p - 50.0).abs() < 1e-6,
        "half-elapsed 5h window paces at 50%, got {p}"
    );

    // 7d window with its full duration left → just opened → 0%.
    let p = ideal_pace_pct(LABEL_7D, &win(BASE_UTC + 7 * 86_400), BASE_UTC).unwrap();
    assert!(
        p.abs() < 1e-6,
        "a freshly-opened window paces at 0%, got {p}"
    );

    // Reset already in the past → clamped to 100%.
    let p = ideal_pace_pct(LABEL_5H, &win(BASE_UTC - 1), BASE_UTC).unwrap();
    assert!(
        (p - 100.0).abs() < 1e-6,
        "a lapsed window paces at 100%, got {p}"
    );

    // No reset time, or a window with no fixed duration → no marker.
    let no_reset = UsageWindow {
        utilization: 0.0,
        resets_at: None,
    };
    assert_eq!(ideal_pace_pct(LABEL_5H, &no_reset, BASE_UTC), None);
    assert_eq!(
        ideal_pace_pct("extra", &win(BASE_UTC + 100), BASE_UTC),
        None
    );
}

/// Window-anchored average pace = utilization spread over the time elapsed since
/// the window opened, in %/day — rotation-proof because it reads only `resets_at`
/// and the current utilization (no history). Gated below `min_elapsed_secs`, and
/// `None` without a reset time or fixed duration.
#[test]
fn window_avg_pace_is_util_over_elapsed_days() {
    let win = |util: f64, reset_secs: i64| UsageWindow {
        utilization: util,
        resets_at: Some(epoch_secs_to_iso(reset_secs)),
    };
    let duration = 7 * 86_400;

    // 7d window 12h into the week at 21% → 21 / 0.5d = 42 %/d.
    let reset = BASE_UTC + duration - 12 * 3600;
    let pace = window_avg_pace_per_day(LABEL_7D, &win(21.0, reset), BASE_UTC, 3600).unwrap();
    assert!((pace - 42.0).abs() < 1e-6, "12h/21% → 42 %/d, got {pace}");

    // Freshly opened (30 min elapsed) is below the 1h floor → None, no divide-by-~0.
    let reset = BASE_UTC + duration - 1800;
    assert_eq!(
        window_avg_pace_per_day(LABEL_7D, &win(5.0, reset), BASE_UTC, 3600),
        None
    );

    // No reset time, or a label with no fixed window → None.
    let no_reset = UsageWindow {
        utilization: 21.0,
        resets_at: None,
    };
    assert_eq!(
        window_avg_pace_per_day(LABEL_7D, &no_reset, BASE_UTC, 3600),
        None
    );
    assert_eq!(
        window_avg_pace_per_day("extra", &win(21.0, BASE_UTC + 100), BASE_UTC, 3600),
        None
    );
}

/// `/profile` re-fetch policy: fetches on first load (no stamp yet) and on a
/// `force` (401 retry), reuses the plan within the hourly TTL, re-pulls once it
/// lapses, and `expire_profile_ttl` (manual single refresh) re-arms it. A
/// persistently failing endpoint is still capped at one attempt per hour.
#[test]
fn take_profile_fetch_honors_ttl_force_and_expiry() {
    let t0 = 1_000_000_000_000u64;

    // First load (no stamp) → fetch; then the same name within the hour → reuse,
    // even though the prior attempt may have failed to yield a plan.
    assert!(
        take_profile_fetch("ttl-first", false, t0),
        "first load pulls /profile"
    );
    assert!(
        !take_profile_fetch("ttl-first", false, t0 + 60_000),
        "within the TTL the cached plan is reused — no per-tick re-pull"
    );

    // `force` overrides a fresh TTL (separate name to avoid cross-talk).
    assert!(take_profile_fetch("ttl-force", false, t0));
    assert!(
        take_profile_fetch("ttl-force", true, t0 + 60_000),
        "force (401 retry) re-pulls /profile despite a fresh TTL"
    );

    // Past the TTL → re-pull.
    assert!(take_profile_fetch("ttl-stale", false, t0));
    assert!(
        take_profile_fetch("ttl-stale", false, t0 + PROFILE_TTL_MS + 1),
        "a plan past the TTL is re-pulled"
    );

    // Manual single refresh expires the clock → re-pull even within the hour.
    assert!(take_profile_fetch("ttl-expire", false, t0));
    assert!(!take_profile_fetch("ttl-expire", false, t0 + 60_000));
    expire_profile_ttl("ttl-expire");
    assert!(
        take_profile_fetch("ttl-expire", false, t0 + 120_000),
        "expiring the TTL forces a re-pull"
    );
}
