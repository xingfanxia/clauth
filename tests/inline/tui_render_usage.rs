use super::*;

/// `bar_spans` overlays the `│` pace marker at its cell without ever changing
/// the bar's total width, whether the marker lands over the filled run (ahead of
/// pace) or the empty run (under pace). An out-of-range column draws no marker.
#[test]
fn bar_spans_places_marker_without_changing_width() {
    let fill = Style::default();
    let total =
        |spans: &[Span<'_>]| -> usize { spans.iter().map(|s| s.content.chars().count()).sum() };
    let marker_col = |spans: &[Span<'_>]| -> Option<usize> {
        let mut col = 0;
        for s in spans {
            if s.content == "│" {
                return Some(col);
            }
            col += s.content.chars().count();
        }
        None
    };

    // No marker requested: plain fill + empty, full width, no glyph.
    let plain = bar_spans(4, 10, fill, None);
    assert_eq!(total(&plain), 10);
    assert_eq!(marker_col(&plain), None);

    // Marker over the empty run (under pace) sits exactly at its column.
    let under = bar_spans(4, 10, fill, Some(7));
    assert_eq!(
        total(&under),
        10,
        "width unchanged with a marker over the empty run"
    );
    assert_eq!(marker_col(&under), Some(7));

    // Marker over the filled run (ahead of pace) — still one glyph, same width.
    let ahead = bar_spans(8, 10, fill, Some(3));
    assert_eq!(
        total(&ahead),
        10,
        "width unchanged with a marker over the filled run"
    );
    assert_eq!(marker_col(&ahead), Some(3));

    // Marker at the fill boundary lands on the first empty cell.
    let boundary = bar_spans(4, 10, fill, Some(4));
    assert_eq!(total(&boundary), 10);
    assert_eq!(marker_col(&boundary), Some(4));

    // Out-of-range column → no marker drawn.
    let oob = bar_spans(4, 10, fill, Some(10));
    assert_eq!(marker_col(&oob), None);
    assert_eq!(total(&oob), 10);
}

/// `stats_from_bars` keeps each bar's API label and source order (no inferred
/// window vocabulary, no reordering), puts absolute `used / total` on the eyebrow
/// `amount` (not the bar-line trailing), and leaves only the reset countdown on
/// the trailing.
#[test]
fn stats_from_bars_keeps_api_labels_and_source_order() {
    let now = crate::usage::now_epoch_secs();
    let bars = vec![
        // Far-future reset + absolute amounts, given first → stays first.
        tp_bar(
            "time limit",
            0.0,
            now + 30 * 86_400,
            Some(0.0),
            Some(1000.0),
        ),
        // Short reset, percentage-only → stays second (no reordering).
        tp_bar("tokens limit", 1.0, now + 4 * 3600, None, None),
    ];
    let stats = stats_from_bars(&bars);
    assert_eq!(stats[0].label, "time limit", "API label kept verbatim");
    assert_eq!(stats[1].label, "tokens limit");

    // Amounts live on the eyebrow now, not the bar-line trailing.
    assert_eq!(stats[0].amount, "0 / 1000");
    assert!(!stats[0].trailing.contains('/'));
    assert!(stats[0].trailing.contains("resets in"));

    // Percentage-only bar: no amount, countdown only.
    assert!(stats[1].amount.is_empty());
    assert!(stats[1].trailing.contains("resets in"));
}

/// Two bars sharing the same API label are NOT renamed — z.ai's pair of token
/// limits both read "tokens limit", in source order.
#[test]
fn stats_from_bars_does_not_rename_duplicate_labels() {
    let now = crate::usage::now_epoch_secs();
    let bars = vec![
        tp_bar("tokens limit", 0.0, now + 4 * 3600, None, None),
        tp_bar("tokens limit", 12.0, now + 6 * 86_400, None, None),
    ];
    let stats = stats_from_bars(&bars);
    assert_eq!(stats[0].label, "tokens limit");
    assert_eq!(stats[1].label, "tokens limit");
    assert_eq!(stats[0].pct, 0.0);
    assert_eq!(stats[1].pct, 12.0);
}

fn tp_bar(
    label: &str,
    pct: f64,
    reset_secs: i64,
    used: Option<f64>,
    total: Option<f64>,
) -> crate::providers::UsageBar {
    crate::providers::UsageBar {
        label: label.to_string(),
        pct,
        resets_at: Some(crate::usage::epoch_secs_to_iso(reset_secs)),
        used,
        total,
    }
}
