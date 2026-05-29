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
