//! Program-wide Config tab — a single panel of global settings, distinct from
//! the per-account Setup tab. Rows back real persisted state in `AppState`,
//! grouped by concern: appearance (`theme`), login (`on mismatch`), background
//! timing (`refresh` cadence, `refresh spent` toggle, `rotation`), fallback
//! detection (`weekly limit`, `rotate mode` = burn-aware, plus the burn-aware
//! `burn floor`/`burn horizon` tunables it gates, issue #8 follow-up b),
//! fallback halt (`quota spent`), then the spend block (`allow extra usage` opt-in +
//! its own `extra usage spent` halt default — real money, see `docs/internals.md`).
//! ↑↓ walks the rows; space cycles a row's value in place; ⏎ opens the
//! refresh-interval and weekly-threshold custom-value editors and otherwise
//! mirrors space. No left selector, no popups — settings are global.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::profile::{DivergenceChoice, MAX_REFRESH_INTERVAL_MS, MIN_REFRESH_INTERVAL_MS};

use super::super::app::{
    App, BURN_FLOOR_PRESETS, BURN_HORIZON_PRESETS, GLOBAL_CONFIG_ROWS, GlobalConfigRow, InputState,
    WEEKLY_PRESETS, format_weekly_pct, parse_refresh_secs, parse_weekly_pct,
};
use super::super::theme::{self, Tier};
use super::panes::{
    cycle_option, head_cols, help_tooltip_lines, highlight_row, invalid_tooltip_lines, key_cell,
    label_style, section_box,
};

/// Width of the key column: the longest keys (`allow extra usage` /
/// `extra usage spent`, 17). Keys pad to it, then [`KEY_GUTTER`] separates them
/// from the value — so every row's value starts at the same column (the Config
/// tab is a cloudy-tui tight chip group).
const KEY_W: usize = 17;
/// Fixed gap between the padded key and the value column.
const KEY_GUTTER: usize = 2;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = section_box("settings", true, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let toggles = {
        let state = &app.config().state;
        ToggleState {
            switch_off_when_spent: state.switch_off_when_spent,
            burn_aware: state.burn_aware_switching,
            spend_budget: state.spend_budget_switching,
            switch_off_when_budget_spent: state.switch_off_when_budget_spent,
            preemptive: state.preemptive_rotation,
            refresh_spent: state.refresh_spent_accounts,
        }
    };
    let refresh_interval_ms = app
        .refresh_interval
        .load(std::sync::atomic::Ordering::Relaxed);
    let weekly_pct = app.config().state.weekly_switch_threshold_pct();
    let burn_floor_pct = app.config().state.burn_switch_floor_pct();
    let burn_horizon_ms = app.config().state.burn_horizon_cap_ms();
    let default_divergence = app.config().state.default_divergence;
    let cursor = app
        .global_config_cursor
        .min(GLOBAL_CONFIG_ROWS.len().saturating_sub(1));
    let editing = app.refresh_interval_draft.as_ref();
    let weekly_editing = app.weekly_threshold_draft.as_ref();

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut caret: Option<(u16, u16)> = None;
    for (i, row) in GLOBAL_CONFIG_ROWS.iter().enumerate() {
        let selected = i == cursor;
        let row_editing = match row {
            GlobalConfigRow::RefreshInterval => editing,
            GlobalConfigRow::WeeklyThreshold => weekly_editing,
            _ => None,
        };
        let line = detail_row(
            *row,
            selected,
            toggles,
            refresh_interval_ms,
            weekly_pct,
            burn_floor_pct,
            burn_horizon_ms,
            default_divergence,
            row_editing,
        );
        match row_editing {
            Some(input) => {
                // The native terminal cursor owns the caret; the row renders plain
                // (no highlight) with the edit gutter + sunken field, like the chain
                // threshold editor. x = "✎ " (2) + key block + pre-caret cols.
                let cx = inner
                    .x
                    .saturating_add((2 + KEY_W + KEY_GUTTER + head_cols(input)) as u16);
                let cy = inner.y.saturating_add(lines.len() as u16);
                caret = Some((cx, cy));
                lines.push(line);
                lines.extend(if *row == GlobalConfigRow::WeeklyThreshold {
                    weekly_range_tooltip(input, inner.width as usize)
                } else {
                    refresh_range_tooltip(input, inner.width as usize)
                });
            }
            None => {
                lines.push(if selected {
                    highlight_row(line, inner.width as usize)
                } else {
                    line
                });
                if selected && let Some(tip) = row_hint(*row, default_divergence, toggles) {
                    lines.extend(help_tooltip_lines(&tip, inner.width as usize));
                }
            }
        }
    }

    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
    if let Some((cx, cy)) = caret {
        frame.set_cursor_position((cx, cy));
    }
}

/// Inline help for rows whose value doesn't self-describe. Phrased for the
/// value currently selected, so cycling a row re-explains what it now does.
/// The boolean toggles the Config tab renders, bundled so `detail_row` /
/// `row_hint` stay within clippy's argument budget as rows accumulate.
#[derive(Clone, Copy)]
struct ToggleState {
    switch_off_when_spent: bool,
    burn_aware: bool,
    spend_budget: bool,
    switch_off_when_budget_spent: bool,
    preemptive: bool,
    refresh_spent: bool,
}

fn row_hint(
    row: GlobalConfigRow,
    default_divergence: Option<DivergenceChoice>,
    toggles: ToggleState,
) -> Option<String> {
    let tip = match row {
        GlobalConfigRow::Theme => return None,
        GlobalConfigRow::DivergenceDefault => match default_divergence {
            None => "ask what to do when claude code signs in over the active account",
            Some(DivergenceChoice::Overwrite) => {
                "fold a new login into the active account, replacing its credentials"
            }
            Some(DivergenceChoice::NewProfile) => {
                "pick which account to save a new login into, keeping the current one"
            }
            Some(DivergenceChoice::Discard) => {
                "restore the previous credentials and drop the new login"
            }
        },
        GlobalConfigRow::RefreshInterval => {
            "how often usage is refreshed for every account (default 90s)"
        }
        GlobalConfigRow::WeeklyThreshold => {
            "soft switch-early line on the 7d window (default 98%): a member past it is handed off \
             but still serves; 100% = only at the hard cap"
        }
        GlobalConfigRow::SwitchOffWhenSpent => {
            if toggles.switch_off_when_spent {
                "once every account is spent, switch everything off until one recovers"
            } else {
                "once every account is spent, stay on the last one until one recovers"
            }
        }
        GlobalConfigRow::BurnAware => {
            if toggles.burn_aware {
                "switch the active account away once its burn rate projects 100% before the next \
                 refresh"
            } else {
                "switch the active account away once its usage crosses its threshold"
            }
        }
        GlobalConfigRow::BurnFloor => {
            if !toggles.burn_aware {
                "inert until rotate mode is burn-aware: the lowest usage % a projected switch may \
                 fire at"
            } else {
                "burn-aware never switches below this %, so it can't waste more than 100 minus it \
                 (default 98)"
            }
        }
        GlobalConfigRow::BurnHorizon => {
            if !toggles.burn_aware {
                "inert until rotate mode is burn-aware: how far ahead the burn projection looks"
            } else {
                "project burn at most this far ahead (also capped by refresh); shorter switches \
                 nearer 100 (default 60s)"
            }
        }
        GlobalConfigRow::SpendBudget => {
            if toggles.spend_budget {
                "spent accounts may fall back to pay-as-you-go, up to each max auto-spend"
            } else {
                "never allow extra usage automatically; a spent chain parks or switches off"
            }
        }
        GlobalConfigRow::SwitchOffWhenBudgetSpent => {
            if !toggles.spend_budget {
                "inert until extra usage is allowed; decides the halt once an account's extra usage \
                 runs out"
            } else if toggles.switch_off_when_budget_spent {
                "once an account's extra usage runs out, switch everything off"
            } else {
                "once an account's extra usage runs out, stay on it and keep billing"
            }
        }
        GlobalConfigRow::PreemptiveRotation => {
            if toggles.preemptive {
                "rotate the active account's login ahead of expiry (macos keychain)"
            } else {
                "rotate the active account's login only when a request rejects it"
            }
        }
        GlobalConfigRow::RefreshSpentAccounts => {
            if toggles.refresh_spent {
                "keep refreshing usage for spent (100%) accounts every interval"
            } else {
                "skip refreshing a spent account until its window resets"
            }
        }
    };
    Some(tip.to_string())
}

#[allow(clippy::too_many_arguments)]
fn detail_row(
    row: GlobalConfigRow,
    selected: bool,
    toggles: ToggleState,
    refresh_interval_ms: u64,
    weekly_pct: f64,
    burn_floor_pct: f64,
    burn_horizon_ms: u64,
    default_divergence: Option<DivergenceChoice>,
    editing: Option<&InputState>,
) -> Line<'static> {
    let arrow = if editing.is_some() {
        Span::styled(format!("{} ", theme::edit_glyph()), theme::accent())
    } else if selected {
        Span::styled("❯ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    let tier = theme::tier();
    match row {
        GlobalConfigRow::Theme => cycle_row(
            arrow,
            "theme",
            &[
                ("full", tier == Tier::Full),
                ("compatible", tier == Tier::Compatible),
            ],
            selected,
        ),
        GlobalConfigRow::RefreshInterval => match editing {
            Some(input) => refresh_edit_line(arrow, input),
            None => refresh_cycle_line(arrow, refresh_interval_ms, selected),
        },
        GlobalConfigRow::WeeklyThreshold => match editing {
            Some(input) => weekly_edit_line(arrow, input),
            None => weekly_cycle_line(arrow, weekly_pct, selected),
        },
        GlobalConfigRow::DivergenceDefault => cycle_row(
            arrow,
            "on mismatch",
            &[
                ("ask", default_divergence.is_none()),
                (
                    "overwrite",
                    default_divergence == Some(DivergenceChoice::Overwrite),
                ),
                (
                    "new",
                    default_divergence == Some(DivergenceChoice::NewProfile),
                ),
                (
                    "discard",
                    default_divergence == Some(DivergenceChoice::Discard),
                ),
            ],
            selected,
        ),
        GlobalConfigRow::SwitchOffWhenSpent => cycle_row(
            arrow,
            "quota spent",
            &[
                ("stay on last", !toggles.switch_off_when_spent),
                ("switch off all", toggles.switch_off_when_spent),
            ],
            selected,
        ),
        GlobalConfigRow::BurnAware => cycle_row(
            arrow,
            "rotate mode",
            &[
                ("static", !toggles.burn_aware),
                ("burn-aware", toggles.burn_aware),
            ],
            selected,
        ),
        GlobalConfigRow::BurnFloor => {
            burn_floor_line(arrow, burn_floor_pct, selected, toggles.burn_aware)
        }
        GlobalConfigRow::BurnHorizon => {
            burn_horizon_line(arrow, burn_horizon_ms, selected, toggles.burn_aware)
        }
        GlobalConfigRow::SpendBudget => cycle_row(
            arrow,
            "allow extra usage",
            &[
                ("off", !toggles.spend_budget),
                ("pay-as-you-go", toggles.spend_budget),
            ],
            selected,
        ),
        // Same two values as `quota spent` on purpose: the pairing is the point.
        // Only the default differs — staying is free there and costs money here.
        // Inert until `allow extra usage` is on: nothing spends, so nothing halts on a
        // spent budget. Rendered dimmed AND the key no-ops (a true disabled row),
        // so `faint` never decouples from "not editable".
        GlobalConfigRow::SwitchOffWhenBudgetSpent => {
            let options = [
                ("stay on last", !toggles.switch_off_when_budget_spent),
                ("switch off all", toggles.switch_off_when_budget_spent),
            ];
            if toggles.spend_budget {
                cycle_row(arrow, "extra usage spent", &options, selected)
            } else {
                dimmed_cycle_row("extra usage spent", &options, selected)
            }
        }
        GlobalConfigRow::PreemptiveRotation => cycle_row(
            arrow,
            "rotation",
            &[
                ("lazy", !toggles.preemptive),
                ("preemptive", toggles.preemptive),
            ],
            selected,
        ),
        GlobalConfigRow::RefreshSpentAccounts => {
            toggle_row(arrow, "refresh spent", toggles.refresh_spent, selected)
        }
    }
}

/// The presets the refresh row steps through, paired with their `ms` value.
/// Mirrors the `step_refresh_interval` ladder in `app.rs`.
const REFRESH_PRESETS: [(&str, u64); 6] = [
    ("15s", 15_000),
    ("30s", 30_000),
    ("60s", 60_000),
    ("90s", 90_000),
    ("120s", 120_000),
    ("300s", 300_000),
];

/// The `refresh` row at rest: a segmented control over [`REFRESH_PRESETS`]. A
/// chip is bracketed only when the interval **exactly** equals that preset; a
/// custom value (set via ⏎) matches none, so the real `<n>s` is appended in
/// `ACCENT` instead of mis-highlighting the nearest preset.
fn refresh_cycle_line(
    arrow: Span<'static>,
    refresh_interval_ms: u64,
    selected: bool,
) -> Line<'static> {
    let options: Vec<(&str, bool)> = REFRESH_PRESETS
        .iter()
        .map(|(label, ms)| (*label, *ms == refresh_interval_ms))
        .collect();
    let mut line = cycle_row(arrow, "refresh", &options, selected);
    if !REFRESH_PRESETS
        .iter()
        .any(|(_, ms)| *ms == refresh_interval_ms)
    {
        // 2 spaces + the last option's reserved trailing cell = the same 3-cell
        // gap the options keep between themselves.
        line.push_span(Span::styled(
            format!("  {}s", refresh_interval_ms / 1000),
            theme::accent(),
        ));
    }
    line
}

/// The `refresh` row mid-edit: edit gutter + `refresh` key block + the typed
/// buffer (DANGER when out of range) + ` s` unit. The terminal cursor owns the
/// caret, so the buffer renders with uniform styling — no simulated block cursor.
fn refresh_edit_line(arrow: Span<'static>, input: &InputState) -> Line<'static> {
    let invalid = parse_refresh_secs(input.trimmed()).is_none();
    let mut spans = vec![
        arrow,
        Span::styled(key_cell("refresh", KEY_W, KEY_GUTTER), label_style(true)),
    ];
    spans.extend(value_caret(input, invalid));
    let unit_style = if invalid {
        theme::danger()
    } else {
        theme::faint()
    };
    spans.push(Span::styled(" s", unit_style));
    Line::from(spans)
}

/// Render the typed buffer with uniform `BG_SUNKEN` styling (DANGER fg when
/// invalid). The terminal cursor — set via `frame.set_cursor_position` — owns
/// the caret glyph, matching the chain threshold editor.
fn value_caret(input: &InputState, invalid: bool) -> Vec<Span<'static>> {
    let body = if invalid {
        theme::danger()
    } else {
        theme::body()
    }
    .bg(theme::bg_sunken());
    vec![Span::styled(input.value.clone(), body)]
}

/// Sub-line under the refresh field while typing: the valid range, in DANGER
/// when the current buffer parses out of range (or non-numeric), else faint.
fn refresh_range_tooltip(input: &InputState, width: usize) -> Vec<Line<'static>> {
    let range = format!(
        "{}-{} s",
        MIN_REFRESH_INTERVAL_MS / 1000,
        MAX_REFRESH_INTERVAL_MS / 1000
    );
    if parse_refresh_secs(input.trimmed()).is_none() {
        invalid_tooltip_lines(&range, width)
    } else {
        help_tooltip_lines(&range, width)
    }
}

/// The `weekly limit` row at rest: a segmented control over
/// [`WEEKLY_PRESETS`], with a custom value (set via ⏎) appended in `ACCENT`
/// when it matches no preset — same grammar as the refresh row.
fn weekly_cycle_line(arrow: Span<'static>, weekly_pct: f64, selected: bool) -> Line<'static> {
    let labels: Vec<String> = WEEKLY_PRESETS
        .iter()
        .map(|p| format!("{}%", format_weekly_pct(*p)))
        .collect();
    let options: Vec<(&str, bool)> = labels
        .iter()
        .zip(WEEKLY_PRESETS.iter())
        .map(|(label, p)| (label.as_str(), *p == weekly_pct))
        .collect();
    let mut line = cycle_row(arrow, "weekly limit", &options, selected);
    if !WEEKLY_PRESETS.contains(&weekly_pct) {
        line.push_span(Span::styled(
            format!("  {}%", format_weekly_pct(weekly_pct)),
            theme::accent(),
        ));
    }
    line
}

/// The `weekly limit` row mid-edit: edit gutter + key block + typed buffer
/// (DANGER when out of range) + ` %` unit. Mirrors `refresh_edit_line`.
fn weekly_edit_line(arrow: Span<'static>, input: &InputState) -> Line<'static> {
    let invalid = parse_weekly_pct(input.trimmed()).is_none();
    let mut spans = vec![
        arrow,
        Span::styled(
            key_cell("weekly limit", KEY_W, KEY_GUTTER),
            label_style(true),
        ),
    ];
    spans.extend(value_caret(input, invalid));
    let unit_style = if invalid {
        theme::danger()
    } else {
        theme::faint()
    };
    spans.push(Span::styled(" %", unit_style));
    Line::from(spans)
}

/// Sub-line under the weekly field while typing: the valid range, in DANGER
/// when the buffer parses out of range, else faint.
fn weekly_range_tooltip(input: &InputState, width: usize) -> Vec<Line<'static>> {
    let range = "50-100 %";
    if parse_weekly_pct(input.trimmed()).is_none() {
        invalid_tooltip_lines(range, width)
    } else {
        help_tooltip_lines(range, width)
    }
}

/// The `burn floor` row: burn-aware early-switch floor as a segmented control
/// over [`BURN_FLOOR_PRESETS`]. Dimmed + inert when burn-aware is off (the
/// projection it gates never runs), mirroring the `extra usage spent` row. A
/// hand-edited in-band value matching no preset is appended in `ACCENT`, same
/// grammar as the weekly row.
fn burn_floor_line(
    arrow: Span<'static>,
    floor_pct: f64,
    selected: bool,
    burn_aware: bool,
) -> Line<'static> {
    let labels: Vec<String> = BURN_FLOOR_PRESETS
        .iter()
        .map(|p| format!("{}%", format_weekly_pct(*p)))
        .collect();
    let options: Vec<(&str, bool)> = labels
        .iter()
        .zip(BURN_FLOOR_PRESETS.iter())
        .map(|(label, p)| (label.as_str(), *p == floor_pct))
        .collect();
    if !burn_aware {
        return dimmed_cycle_row("burn floor", &options, selected);
    }
    let mut line = cycle_row(arrow, "burn floor", &options, selected);
    if !BURN_FLOOR_PRESETS.contains(&floor_pct) {
        line.push_span(Span::styled(
            format!("  {}%", format_weekly_pct(floor_pct)),
            theme::accent(),
        ));
    }
    line
}

/// The `burn horizon` row: burn-aware projection look-ahead cap as a segmented
/// control over [`BURN_HORIZON_PRESETS`] (labelled in seconds). Dimmed + inert
/// when burn-aware is off. Custom in-band value appended in `ACCENT`.
fn burn_horizon_line(
    arrow: Span<'static>,
    horizon_ms: u64,
    selected: bool,
    burn_aware: bool,
) -> Line<'static> {
    let labels: Vec<String> = BURN_HORIZON_PRESETS
        .iter()
        .map(|ms| format!("{}s", ms / 1000))
        .collect();
    let options: Vec<(&str, bool)> = labels
        .iter()
        .zip(BURN_HORIZON_PRESETS.iter())
        .map(|(label, ms)| (label.as_str(), *ms == horizon_ms))
        .collect();
    if !burn_aware {
        return dimmed_cycle_row("burn horizon", &options, selected);
    }
    let mut line = cycle_row(arrow, "burn horizon", &options, selected);
    if !BURN_HORIZON_PRESETS.contains(&horizon_ms) {
        line.push_span(Span::styled(
            format!("  {}s", horizon_ms / 1000),
            theme::accent(),
        ));
    }
    line
}

/// A cloudy-tui cycle row: `key  label  [active]  other`. Options are bare
/// labels separated by 2-space gaps; the active option is `ACCENT` and wraps in
/// `[]` only while the row holds the cursor, the rest stay `TEXT_FAINT`. `space`
/// cycles the value in place. Reads as the segmented control it is, instead of
/// a single value that silently swaps text on cycle.
fn cycle_row(
    arrow: Span<'static>,
    key: &str,
    options: &[(&str, bool)],
    row_selected: bool,
) -> Line<'static> {
    let mut spans = vec![
        arrow,
        Span::styled(key_cell(key, KEY_W, KEY_GUTTER), label_style(row_selected)),
    ];
    for (i, (label, active)) in options.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(cycle_option(label, *active, row_selected));
    }
    Line::from(spans)
}

/// A cloudy-tui Disabled row for a cycle setting another toggle makes inert: the
/// whole row (caret, key, current value) renders `TEXT_FAINT`, no bracket
/// highlight — just the current value. Focusable but inert (the key handler
/// no-ops it), so `TEXT_FAINT` keeps meaning "can't touch this". The `draw` loop
/// still tints + shows the `└` reason on focus.
fn dimmed_cycle_row(key: &str, options: &[(&str, bool)], selected: bool) -> Line<'static> {
    let arrow = if selected {
        Span::styled("❯ ", theme::faint())
    } else {
        Span::raw("  ")
    };
    let value = options
        .iter()
        .find(|(_, active)| *active)
        .map(|(label, _)| *label)
        .unwrap_or("");
    Line::from(vec![
        arrow,
        Span::styled(key_cell(key, KEY_W, KEY_GUTTER), theme::faint()),
        Span::styled(value.to_string(), theme::faint()),
    ])
}

/// A cloudy-tui toggle row: `key  ─●` / `key  ○─`. A pure on/off boolean is a
/// toggle, not a 2-option cycle — `on`/`off` labels in brackets read as a cycle,
/// not the switch the contract draws. Knob `ACCENT` when on, `TEXT_FAINT` off.
fn toggle_row(arrow: Span<'static>, key: &str, on: bool, row_selected: bool) -> Line<'static> {
    let (glyph, style) = if on {
        (theme::toggle_on(), theme::accent())
    } else {
        (theme::toggle_off(), theme::faint())
    };
    Line::from(vec![
        arrow,
        Span::styled(key_cell(key, KEY_W, KEY_GUTTER), label_style(row_selected)),
        Span::styled(glyph, style),
    ])
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_global_config.rs"]
mod tests;
