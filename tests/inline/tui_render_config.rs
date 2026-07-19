//! The Setup-tab `model` row is a segmented alias cycle sharing the Config-tab
//! contract: bare labels when blurred, the active option bracketed only on focus
//! (the row widens by 2 on focus — the bracket pair is the only width change).
//! Plus the pane's action rows, which take the `+ new`-row focus promotion.

use super::*;
use ratatui::style::Modifier;

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

/// The pane's color-identity action rows take the `+ new`-row promotion: bold
/// when the cursor is on it, and the accent (or success) color held throughout,
/// since the color is the row's identity and never promotes to `TEXT`. Before
/// this they were bare labels that looked identical focused and blurred, leaving
/// only the row tint to carry focus. `delete account` / `log out` are NOT in
/// this set: their always-bold `DANGER` is a fixed destructive cue that must
/// persist whether or not the row is focused.
#[test]
fn action_rows_bold_on_select_and_keep_their_color() {
    let mut snap = Snap::blank("+ new account");
    let input = InputState::new("");
    // (row, the style the label holds in both states)
    let cases: [(ConfigRow, Style); 4] = [
        (ConfigRow::EnvAdd, theme::accent()),
        (ConfigRow::ModelOverrideAdd, theme::accent()),
        (ConfigRow::Create, theme::accent()),
        (ConfigRow::Login, theme::accent()),
    ];
    for (row, want) in cases {
        let blurred = detail_row(row, false, false, false, &snap, &input);
        let focused = detail_row(row, true, false, false, &snap, &input);
        let (b, f) = (&blurred.spans[1], &focused.spans[1]);
        assert_eq!(
            b.content, f.content,
            "{row:?}: focus must not change the label text"
        );
        assert!(
            !b.style.add_modifier.contains(Modifier::BOLD),
            "{row:?}: a blurred action row must not be bold"
        );
        assert!(
            f.style.add_modifier.contains(Modifier::BOLD),
            "{row:?}: a selected action row promotes to bold"
        );
        assert_eq!(b.style.fg, want.fg, "{row:?}: blurred keeps its color");
        assert_eq!(
            f.style.fg, want.fg,
            "{row:?}: the color is the row's identity, so focus never recolors it"
        );
    }

    // The `✓ logged in` state is the same row in SUCCESS — same promotion rule.
    snap.captured = true;
    let blurred = detail_row(ConfigRow::Login, false, false, false, &snap, &input);
    let focused = detail_row(ConfigRow::Login, true, false, false, &snap, &input);
    assert!(line_text(&focused).contains("✓ logged in"));
    assert!(!blurred.spans[1].style.add_modifier.contains(Modifier::BOLD));
    assert!(focused.spans[1].style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(focused.spans[1].style.fg, theme::success().fg);
}

/// Setup hints follow their row's current value — auto-start flips on/off, the
/// base-url hint flips between the claude.ai and custom-endpoint phrasings.
#[test]
fn setup_hints_follow_the_row_value() {
    let mut snap = Snap::blank("a");

    snap.auto_start = true;
    let on = row_hint(ConfigRow::AutoStart, &snap).unwrap();
    assert!(on.contains("throwaway session"), "{on}");
    snap.auto_start = false;
    let off = row_hint(ConfigRow::AutoStart, &snap).unwrap();
    assert!(off.contains("never starts"), "{off}");

    snap.base_url = String::new();
    let empty = row_hint(ConfigRow::BaseUrl, &snap).unwrap();
    assert!(empty.contains("claude.ai account"), "{empty}");
    snap.base_url = "https://api.example.com".into();
    let set = row_hint(ConfigRow::BaseUrl, &snap).unwrap();
    assert!(set.contains("calls instead"), "{set}");
}

// ── CLA-SPLIT: the `token` long-lived-login status row ──────────────────────

// The row states the horizon in days and escalates: accent while comfortable,
// WARNING inside 30 days, DANGER + the re-mint hint once expired; a sidecar
// without a stamp says so; a mis-filled rotating pair reads as disengaged in
// DANGER (the operator thinks the split is armed and it isn't).
#[test]
fn long_lived_token_row_counts_down_and_escalates() {
    use crate::claude::SessionTokenStatus as S;
    let day = 86_400_000_i64;
    let now = 1_700_000_000_000_i64;

    let comfy = line_text(&session_token_line(
        &S::LongLived(Some(now + 340 * day)),
        now,
    ));
    assert!(comfy.contains("token"), "{comfy}");
    assert!(comfy.contains("long-lived · expires in ~340d"), "{comfy}");

    let soon = session_token_line(&S::LongLived(Some(now + 12 * day)), now);
    assert!(line_text(&soon).contains("expires in ~12d"));
    assert!(
        soon.spans.iter().any(|s| s.style == theme::warning()),
        "last 30 days warn"
    );

    let dead = session_token_line(&S::LongLived(Some(now - day)), now);
    let dead_text = line_text(&dead);
    assert!(
        dead_text.contains("re-mint: claude setup-token"),
        "{dead_text}"
    );
    assert!(
        dead.spans.iter().any(|s| s.style == theme::danger()),
        "expired is DANGER"
    );

    // Expired within the last 24h: truncating division gives 0 days, so the old
    // `days < 0` check mislabeled it "~0d / warning". It must read as expired.
    let just_dead = session_token_line(&S::LongLived(Some(now - day / 2)), now);
    let just_dead_text = line_text(&just_dead);
    assert!(
        just_dead_text.contains("expired"),
        "a token expired <24h ago is expired, not ~0d: {just_dead_text}"
    );
    assert!(
        just_dead.spans.iter().any(|s| s.style == theme::danger()),
        "sub-day-expired is DANGER"
    );

    let unstamped = line_text(&session_token_line(&S::LongLived(None), now));
    assert!(unstamped.contains("no recorded expiry"), "{unstamped}");

    let misfilled = session_token_line(&S::NotLongLived, now);
    let misfilled_text = line_text(&misfilled);
    assert!(
        misfilled_text.contains("not long-lived") && misfilled_text.contains("ignored"),
        "{misfilled_text}"
    );
    assert!(
        misfilled.spans.iter().any(|s| s.style == theme::danger()),
        "a disengaged sidecar is DANGER"
    );
}
