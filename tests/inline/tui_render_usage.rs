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
