//! Config-tab row geometry. Every blurred row's value starts at the same
//! column (the Config tab is a cloudy-tui tight chip group); cycle options are
//! bare labels on 2-space gaps with the active option bracketed only on focus;
//! an on/off boolean renders as a toggle, not a 2-option cycle.

use super::*;

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn row(key: &str, options: &[(&str, bool)], selected: bool) -> String {
    line_text(&cycle_row(Span::raw("  "), key, options, selected))
}

/// Column `key`'s value starts at: the first non-space cell past the key text.
fn value_col(key: &str, rendered: &str) -> usize {
    let after_key = rendered.find(key).expect("row renders its key") + key.chars().count();
    after_key
        + rendered[after_key..]
            .find(|c: char| c != ' ')
            .expect("row renders a value")
}

fn toggles() -> ToggleState {
    ToggleState {
        switch_off_when_spent: false,
        burn_aware: false,
        spend_budget: false,
        switch_off_when_budget_spent: true,
        preemptive: false,
        refresh_spent: true,
    }
}

#[test]
fn key_cell_is_uniform_width() {
    for key in ["theme", "weekly limit", "on mismatch", "refresh spent"] {
        assert_eq!(
            key_cell(key, KEY_W, KEY_GUTTER).chars().count(),
            KEY_W + KEY_GUTTER,
            "{key} key block must be exactly KEY_W + KEY_GUTTER wide"
        );
    }
}

/// Rot-proof against a new row with a longer key: every blurred row opens its
/// value at the shared column. Reads the real rows, so no key list to sync.
#[test]
fn every_blurred_row_starts_its_value_at_the_shared_column() {
    let value_col = 2 + KEY_W + KEY_GUTTER;
    for r in GLOBAL_CONFIG_ROWS {
        let line = line_text(&detail_row(r, false, toggles(), 60_000, 95.0, None, None));
        let before: String = line.chars().take(value_col).collect();
        assert!(
            before.ends_with(&" ".repeat(KEY_GUTTER)),
            "{r:?} key overruns KEY_W or drops the gutter: {before:?}"
        );
        assert_ne!(
            line.chars().nth(value_col),
            Some(' '),
            "{r:?} value column must open on a label/glyph, not a space"
        );
    }
}

/// The regression the screenshot caught: `weekly limit` is exactly `KEY_W`
/// chars, so a `saturating_sub(..).max(1)` pad made its block 1 cell wider.
#[test]
fn longest_key_aligns_with_shortest() {
    let theme = row("theme", &[("full", true), ("compatible", false)], false);
    let weekly = row("weekly limit", &[("90%", true), ("95%", false)], false);
    assert_eq!(value_col("theme", &theme), 2 + KEY_W + KEY_GUTTER);
    assert_eq!(
        value_col("theme", &theme),
        value_col("weekly limit", &weekly),
        "`weekly limit` (== KEY_W chars) must not push its value column right"
    );
}

/// Bare labels: an inactive first option and an active first option open at the
/// same column (no bracket-cell reservation either way).
#[test]
fn active_first_option_aligns_with_inactive_first_option() {
    let theme = row("theme", &[("full", true), ("compatible", false)], false);
    let mismatch = row(
        "on mismatch",
        &[("ask", false), ("overwrite", true), ("new", false)],
        false,
    );
    assert_eq!(
        value_col("theme", &theme),
        value_col("on mismatch", &mismatch),
    );
}

/// Focus wraps the active option in `[]` — the bracket pair is the only width
/// change; labels ahead of the active option hold their columns.
#[test]
fn focus_wraps_the_active_option_in_brackets() {
    let options = [("ask", false), ("overwrite", true), ("new", false)];
    let blurred = row("on mismatch", &options, false);
    let focused = row("on mismatch", &options, true);
    assert!(!blurred.contains('['), "{blurred:?}");
    assert!(focused.contains("[overwrite]"), "{focused:?}");
    assert_eq!(
        focused.chars().count(),
        blurred.chars().count() + 2,
        "the bracket pair is the only width change"
    );
    assert_eq!(
        blurred.find("ask"),
        focused.find("ask"),
        "a label ahead of the active option holds its column"
    );
}

/// Exact bytes: 2-space gaps everywhere, bare labels, active option bracketed
/// only on focus.
#[test]
fn cycle_row_renders_the_contract_shape() {
    let options = [("off", true), ("basic", false), ("strict", false)];
    // Key column is `arrow (2) + KEY_W + KEY_GUTTER` wide; derive the pad so the
    // shape assertion tracks KEY_W instead of rebreaking on a width change.
    let pad = " ".repeat(KEY_W + KEY_GUTTER - "verify".len());
    assert_eq!(
        row("verify", &options, false),
        format!("  verify{pad}off  basic  strict"),
    );
    assert_eq!(
        row("verify", &options, true),
        format!("  verify{pad}[off]  basic  strict"),
    );
}

/// The caret math in `draw` assumes the typed buffer starts at the value column.
#[test]
fn edit_line_buffer_starts_at_the_value_column() {
    let input = InputState {
        value: "45".to_string(),
        cursor: 2,
    };
    for rendered in [
        line_text(&weekly_edit_line(Span::raw("  "), &input)),
        line_text(&refresh_edit_line(Span::raw("  "), &input)),
    ] {
        assert_eq!(
            rendered.find("45"),
            Some(2 + KEY_W + KEY_GUTTER),
            "typed buffer must start at the shared value column: {rendered:?}"
        );
    }
}

/// `refresh spent` is a pure on/off boolean — a cloudy-tui toggle (`─●` / `○─`),
/// not a 2-option cycle row (`[on]  off`).
#[test]
fn refresh_spent_renders_as_a_toggle_not_a_cycle() {
    let on = line_text(&detail_row(
        GlobalConfigRow::RefreshSpentAccounts,
        false,
        toggles(),
        60_000,
        95.0,
        None,
        None,
    ));
    assert!(on.contains(theme::toggle_on()), "on state glyph: {on}");
    assert!(
        !on.contains("off"),
        "must not render the cycle off-option: {on}"
    );

    let mut off = toggles();
    off.refresh_spent = false;
    let off_line = line_text(&detail_row(
        GlobalConfigRow::RefreshSpentAccounts,
        false,
        off,
        60_000,
        95.0,
        None,
        None,
    ));
    assert!(
        off_line.contains(theme::toggle_off()),
        "off state glyph: {off_line}"
    );
    assert!(
        !off_line.contains("  on"),
        "must not render the cycle on-option: {off_line}"
    );
}

// ── `money spent` dims while inert (spend budget off) ────────────────────────

/// With `spend budget` off nothing spends, so `money spent` decides no halt.
/// It renders as a cloudy-tui disabled row (whole content faint) so it never
/// reads as an armed setting; flip the toggle on and it becomes a live cycle.
#[test]
fn money_spent_dims_when_spend_budget_is_off() {
    let dimmed = detail_row(
        GlobalConfigRow::SwitchOffWhenBudgetSpent,
        false,
        toggles(), // spend_budget: false
        60_000,
        95.0,
        None,
        None,
    );
    assert!(
        dimmed
            .spans
            .iter()
            .all(|s| s.content.trim().is_empty() || s.style.fg == theme::faint().fg),
        "every content span must be faint while inert: {:?}",
        dimmed.spans,
    );

    let mut on = toggles();
    on.spend_budget = true;
    let live = line_text(&detail_row(
        GlobalConfigRow::SwitchOffWhenBudgetSpent,
        true,
        on,
        60_000,
        95.0,
        None,
        None,
    ));
    assert!(
        live.contains('['),
        "spend budget on: live + focused brackets the active option: {live}"
    );
}

#[test]
fn money_spent_hint_explains_inertness_when_spend_off() {
    let hint = row_hint(GlobalConfigRow::SwitchOffWhenBudgetSpent, None, toggles())
        .expect("an inert money-spent row still carries its reason");
    assert!(hint.contains("inert until spend budget"), "{hint}");
}
