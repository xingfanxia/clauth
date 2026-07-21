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

// A comfortable horizon is a plain accent value; the last 30 days warn as a
// pill; expired and mis-filled escalate to a DANGER pill plus a `└` fix line
// (the operator thinks the split is armed and it isn't). Unstamped says so.
#[test]
fn long_lived_token_row_counts_down_and_escalates() {
    use crate::claude::SessionTokenStatus as S;
    let day = 86_400_000_i64;
    let now = 1_700_000_000_000_i64;
    let w = 60usize;
    let text = |ls: &[Line<'static>]| -> String {
        ls.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect()
    };
    // Match on fg only, so the pill label's added BOLD doesn't defeat the check.
    let has_color = |ls: &[Line<'static>], st: Style| {
        ls.iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.style.fg == st.fg)
    };

    // Comfortable horizon: a plain accent value, no pill, one line.
    let comfy = session_token_lines(&S::LongLived(Some(now + 340 * day)), false, now, w);
    assert_eq!(comfy.len(), 1);
    let comfy_t = text(&comfy);
    assert!(comfy_t.contains("token"), "{comfy_t}");
    assert!(comfy_t.contains("long-lived · ~340d left"), "{comfy_t}");
    assert!(
        !comfy_t.contains('['),
        "comfortable is a value, not a pill: {comfy_t}"
    );
    assert!(has_color(&comfy, theme::accent()));

    // Last 30 days: a WARNING pill, still one line, no fix.
    let soon = session_token_lines(&S::LongLived(Some(now + 12 * day)), false, now, w);
    assert_eq!(soon.len(), 1);
    assert!(
        text(&soon).contains("[ expires in ~12d ]"),
        "{}",
        text(&soon)
    );
    assert!(has_color(&soon, theme::warning()), "last 30 days warn");

    // Expired: DANGER pill + a `└` re-mint fix line.
    let dead = session_token_lines(&S::LongLived(Some(now - day)), false, now, w);
    assert_eq!(dead.len(), 2, "expired = pill + fix line");
    let dead_t = text(&dead);
    assert!(dead_t.contains("[ expired ]"), "{dead_t}");
    assert!(
        dead_t.contains("re-mint with claude setup-token"),
        "{dead_t}"
    );
    assert!(has_color(&dead, theme::danger()), "expired is DANGER");

    // Expired within the last 24h: truncating division gives 0 days; it must
    // read as expired, not "~0d / warning".
    let just_dead = session_token_lines(&S::LongLived(Some(now - day / 2)), false, now, w);
    let just_dead_t = text(&just_dead);
    assert!(
        just_dead_t.contains("[ expired ]"),
        "a token expired <24h ago is expired, not ~0d: {just_dead_t}"
    );
    assert!(
        has_color(&just_dead, theme::danger()),
        "sub-day-expired is DANGER"
    );

    // Unstamped long-lived: a plain accent value.
    let unstamped = session_token_lines(&S::LongLived(None), false, now, w);
    assert_eq!(unstamped.len(), 1);
    assert!(
        text(&unstamped).contains("no recorded expiry"),
        "{}",
        text(&unstamped)
    );

    // Mis-filled (rotating pair): DANGER pill + fix, split disengaged.
    let misfilled = session_token_lines(&S::NotLongLived, false, now, w);
    assert_eq!(misfilled.len(), 2, "mis-filled = pill + fix line");
    let mis_t = text(&misfilled);
    assert!(mis_t.contains("[ mis-filled ]"), "{mis_t}");
    assert!(
        mis_t.contains("sidecar has a refresh token, split is off"),
        "{mis_t}"
    );
    assert!(
        has_color(&misfilled, theme::danger()),
        "a disengaged sidecar is DANGER"
    );
}
