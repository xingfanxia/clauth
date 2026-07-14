//! The Setup-tab `model` row is a segmented alias cycle sharing the Config-tab
//! contract: bare labels when blurred, the active option bracketed only on focus
//! (the row widens by 2 on focus — the bracket pair is the only width change).

use super::*;

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

// Blurred: bare labels, no brackets anywhere. Focused: the active preset wraps
// in `[]` and that bracket pair is the only width change (the old shape padded
// the blurred active option to ` label `, so blurred==focused in width — this
// delta is the contract).
#[test]
fn model_cycle_brackets_the_active_option_only_on_focus() {
    let arrow = Span::raw("  ");
    let blurred = line_text(&model_cycle_line(arrow.clone(), "sonnet", false));
    let focused = line_text(&model_cycle_line(arrow, "sonnet", true));

    assert!(
        blurred.contains("sonnet"),
        "active preset renders when blurred: {blurred}"
    );
    assert!(!blurred.contains('['), "blurred stays bare: {blurred}");
    assert!(
        focused.contains("[sonnet]"),
        "focused brackets the active preset: {focused}"
    );
    assert_eq!(
        focused.chars().count(),
        blurred.chars().count() + 2,
        "the bracket pair is the only width change on focus"
    );
}

// A custom id (no preset match) appends in ACCENT instead of mis-bracketing the
// nearest alias — and stays bracket-free when blurred.
#[test]
fn model_cycle_appends_a_custom_id_without_brackets() {
    let arrow = Span::raw("  ");
    let blurred = line_text(&model_cycle_line(arrow.clone(), "claude-fable-5", false));
    let focused = line_text(&model_cycle_line(arrow, "claude-fable-5", true));

    assert!(
        blurred.contains("claude-fable-5"),
        "custom id renders: {blurred}"
    );
    assert!(
        !blurred.contains('['),
        "no brackets on a custom id when blurred: {blurred}"
    );
    assert!(
        !focused.contains("[claude-fable-5]"),
        "a custom id is appended, not bracketed: {focused}"
    );
}
