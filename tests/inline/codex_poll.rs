//! CDX-6: `wham/usage` response parsing — all offline (fixture bodies only;
//! the HTTP call itself is a thin wrapper over the shared agent and is never
//! exercised against the real backend from tests).

use super::*;

/// The shape the sibling projects document: `rate_limit` with
/// `primary_window`/`secondary_window`, weekly identified by its own
/// duration (10080 min), epoch-seconds resets.
#[test]
fn parses_the_documented_backend_shape() {
    let now = 1_700_000_000i64;
    let body = format!(
        r#"{{
            "rate_limit": {{
                "primary_window": {{
                    "used_percent": 37.5,
                    "resets_at": {reset},
                    "window_minutes": 10080
                }}
            }},
            "some_future_field": {{"ignored": true}}
        }}"#,
        reset = now + 3 * 86_400
    );
    let info = parse_wham_usage(body.as_bytes(), now).expect("parse");
    let seven_day = info.seven_day.expect("weekly window routed by duration");
    assert!((seven_day.utilization - 37.5).abs() < f64::EPSILON);
    assert!(
        seven_day
            .resets_at
            .as_deref()
            .is_some_and(|iso| iso.starts_with("20")),
        "epoch reset published as ISO"
    );
    assert!(info.five_hour.is_none(), "no session window in this shape");
}

/// The JSONL-side spellings (`rate_limits`/`primary`/`secondary`) parse too —
/// the aliases keep either backend spelling working.
#[test]
fn parses_the_jsonl_side_spellings() {
    let now = 1_700_000_000i64;
    let body = format!(
        r#"{{
            "rate_limits": {{
                "primary": {{"used_percent": 12.0, "resets_at": {r1}, "window_minutes": 300}},
                "secondary": {{"used_percent": 88.0, "resets_at": {r2}, "window_minutes": 10080}},
                "rate_limit_reached_type": "secondary"
            }}
        }}"#,
        r1 = now + 3600,
        r2 = now + 5 * 86_400
    );
    let info = parse_wham_usage(body.as_bytes(), now).expect("parse");
    assert!((info.five_hour.expect("short slot").utilization - 12.0).abs() < f64::EPSILON);
    assert!((info.seven_day.expect("weekly slot").utilization - 88.0).abs() < f64::EPSILON);
    assert_eq!(info.codex_rate_limit_reached.as_deref(), Some("secondary"));
}

/// A relative `resets_in_seconds` (no absolute stamp) normalizes to
/// now + delta.
#[test]
fn relative_reset_normalizes_to_absolute() {
    let now = 1_700_000_000i64;
    let body = r#"{
        "rate_limit": {
            "primary_window": {"used_percent": 5.0, "resets_in_seconds": 86400, "window_minutes": 10080}
        }
    }"#;
    let info = parse_wham_usage(body.as_bytes(), now).expect("parse");
    let iso = info
        .seven_day
        .expect("weekly")
        .resets_at
        .expect("normalized reset");
    let secs = crate::usage::iso_to_epoch_secs(&iso).expect("parse iso");
    assert_eq!(secs, now + 86_400);
}

/// Shape drift fails LOUD (an error the scheduler paces), never a silent
/// empty snapshot that would overwrite good cached data with zeros.
#[test]
fn unrecognized_shapes_error_instead_of_publishing_zeros() {
    let now = 1_700_000_000i64;
    assert!(matches!(
        parse_wham_usage(b"not json", now),
        Err(PollError::Other(_))
    ));
    assert!(matches!(
        parse_wham_usage(br#"{"entirely": "different"}"#, now),
        Err(PollError::Other(_))
    ));
    assert!(matches!(
        parse_wham_usage(br#"{"rate_limit": {}}"#, now),
        Err(PollError::Other(_))
    ));
}
