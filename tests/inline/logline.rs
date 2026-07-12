use super::*;

// 2026-05-17T14:20:00 UTC == 1779027600 epoch seconds (same fixture instant
// as the fetch ISO tests).
const BASE_UTC: i64 = 1_779_027_600;

#[test]
fn daemon_mode_prefixes_an_iso_utc_stamp() {
    assert_eq!(
        render(true, BASE_UTC, "clauth daemon: switched to 'b'"),
        "2026-05-17T14:20:00+00:00 clauth daemon: switched to 'b'",
        "daemon.log lines must be self-dating — incident forensics depend on it"
    );
}

#[test]
fn interactive_mode_stays_bare() {
    assert_eq!(
        render(false, BASE_UTC, "clauth: 'a' re-authenticated"),
        "clauth: 'a' re-authenticated",
        "CLI/TUI stderr keeps the historical bare format"
    );
}
