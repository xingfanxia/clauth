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

/// `retry-after` parsing accepts only the delta-seconds form; the HTTP-date
/// form and garbage are no-hint (`None`).
#[test]
fn retry_after_parses_delta_seconds_only() {
    assert_eq!(parse_retry_after("5"), Some(Duration::from_secs(5)));
    assert_eq!(parse_retry_after(" 30 "), Some(Duration::from_secs(30)));
    assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
    assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    assert_eq!(parse_retry_after(""), None);
    assert_eq!(parse_retry_after("-5"), None);
    assert_eq!(parse_retry_after("1.5"), None);
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
