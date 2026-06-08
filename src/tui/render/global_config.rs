//! Program-wide Config tab — a single panel of global settings, distinct from
//! the per-account Setup tab. Rows back real persisted state in `AppState`:
//! the theme tier (`[theme]`) and the chain-wide wrap-off default. ↑↓ walks the
//! rows; ⏎/space cycles the theme or wrap-off value in place. No left selector,
//! no popups — these settings are global, not per-account.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, GLOBAL_CONFIG_ROWS, GlobalConfigRow};
use super::super::theme::{self, Tier};
use super::panes::{highlight_row, section_box};

const KEY_W: usize = 12;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = section_box("settings", true, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let wrap_off = app.config().state.wrap_off;
    let refresh_interval_ms = app
        .refresh_interval
        .load(std::sync::atomic::Ordering::Relaxed);
    let cursor = app
        .global_config_cursor
        .min(GLOBAL_CONFIG_ROWS.len().saturating_sub(1));

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, row) in GLOBAL_CONFIG_ROWS.iter().enumerate() {
        let selected = i == cursor;
        let line = detail_row(*row, selected, wrap_off, refresh_interval_ms);
        lines.push(if selected {
            highlight_row(line, inner.width as usize)
        } else {
            line
        });
        if selected && let Some(tip) = row_hint(*row) {
            lines.push(Line::from(vec![
                Span::styled("  └ ", Style::default().fg(theme::line_color())),
                Span::styled(tip, theme::faint()),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

/// Inline help for rows whose value doesn't self-describe.
fn row_hint(row: GlobalConfigRow) -> Option<&'static str> {
    match row {
        GlobalConfigRow::Theme => None,
        GlobalConfigRow::RefreshInterval => Some("+/- or ⏎ to step through presets"),
        GlobalConfigRow::WrapOff => {
            Some("default when every fallback member is over its threshold")
        }
    }
}

fn detail_row(
    row: GlobalConfigRow,
    selected: bool,
    wrap_off: bool,
    refresh_interval_ms: u64,
) -> Line<'static> {
    let arrow = if selected {
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
        GlobalConfigRow::RefreshInterval => cycle_row(
            arrow,
            "refresh",
            &[
                ("15s", refresh_interval_ms <= 22_500),
                (
                    "30s",
                    refresh_interval_ms > 22_500 && refresh_interval_ms <= 45_000,
                ),
                (
                    "60s",
                    refresh_interval_ms > 45_000 && refresh_interval_ms <= 75_000,
                ),
                (
                    "90s",
                    refresh_interval_ms > 75_000 && refresh_interval_ms <= 105_000,
                ),
                (
                    "120s",
                    refresh_interval_ms > 105_000 && refresh_interval_ms <= 210_000,
                ),
                ("300s", refresh_interval_ms > 210_000),
            ],
            selected,
        ),
        GlobalConfigRow::WrapOff => cycle_row(
            arrow,
            "when spent",
            &[("stay on last", !wrap_off), ("switch off all", wrap_off)],
            selected,
        ),
    }
}

/// A cloudy-tui cycle row: `key   [active]  other`. The active option is `ACCENT`
/// (bracketed only while the row is the cursor), the rest `TEXT_FAINT`; ⏎/space
/// cycles the value in place. Reads as the segmented control it is, instead of a
/// single value that silently swaps text on cycle.
fn cycle_row(
    arrow: Span<'static>,
    key: &str,
    options: &[(&str, bool)],
    row_selected: bool,
) -> Line<'static> {
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    let key_style = if row_selected {
        theme::body().add_modifier(Modifier::BOLD)
    } else {
        theme::dim()
    };
    let mut spans = vec![
        arrow,
        Span::styled(format!("{key}{}", " ".repeat(pad)), key_style),
    ];
    for (i, (label, active)) in options.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(cycle_option(label, *active, row_selected));
    }
    Line::from(spans)
}

/// One segment of a cycle row. Active → `ACCENT`: `[label]` when the row is the
/// cursor, ` label ` (bracket cells reserved as spaces) when blurred so focus
/// never reflows the row. Inactive → bare `TEXT_FAINT`.
fn cycle_option(label: &str, active: bool, row_selected: bool) -> Span<'static> {
    if active {
        let text = if row_selected {
            format!("[{label}]")
        } else {
            format!(" {label} ")
        };
        Span::styled(text, theme::accent())
    } else {
        Span::styled(label.to_string(), theme::faint())
    }
}
