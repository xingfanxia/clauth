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

// ── CLA-SPLIT: the `session` static-token status row ─────────────────────────

// The row states the horizon in days and escalates: accent while comfortable,
// WARNING inside 30 days, DANGER + the re-mint hint once expired; a sidecar
// without a stamp says so instead of inventing a countdown.
#[test]
fn session_token_row_counts_down_and_escalates() {
    let day = 86_400_000_i64;
    let now = 1_700_000_000_000_i64;

    let comfy = line_text(&session_token_line(Some(now + 340 * day), now));
    assert!(comfy.contains("session"), "{comfy}");
    assert!(comfy.contains("expires in ~340d"), "{comfy}");

    let soon = session_token_line(Some(now + 12 * day), now);
    assert!(line_text(&soon).contains("expires in ~12d"));
    assert!(
        soon.spans.iter().any(|s| s.style == theme::warning()),
        "last 30 days warn"
    );

    let dead = session_token_line(Some(now - day), now);
    let dead_text = line_text(&dead);
    assert!(
        dead_text.contains("re-mint: claude setup-token"),
        "{dead_text}"
    );
    assert!(
        dead.spans.iter().any(|s| s.style == theme::danger()),
        "expired is DANGER"
    );

    let unstamped = line_text(&session_token_line(None, now));
    assert!(unstamped.contains("no recorded expiry"), "{unstamped}");
}
