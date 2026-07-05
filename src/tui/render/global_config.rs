//! Program-wide Config tab — a single panel of global settings, distinct from
//! the per-account Setup tab. Rows back real persisted state in `AppState`:
//! the theme tier (`[theme]`), the divergence default, the refresh interval, and
//! the chain-wide wrap-off default. ↑↓ walks the rows; space cycles a row's value
//! in place; ⏎ opens the refresh-interval custom-value editor and otherwise
//! mirrors space. No left selector, no popups — these settings are global.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::profile::{DivergenceChoice, MAX_REFRESH_INTERVAL_MS, MIN_REFRESH_INTERVAL_MS};

use super::super::app::{App, GLOBAL_CONFIG_ROWS, GlobalConfigRow, InputState, parse_refresh_secs};
use super::super::theme::{self, Tier};
use super::panes::{head_cols, highlight_row, label_style, section_box};

const KEY_W: usize = 12;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = section_box("settings", true, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let wrap_off = app.config().state.wrap_off;
    let refresh_interval_ms = app
        .refresh_interval
        .load(std::sync::atomic::Ordering::Relaxed);
    let default_divergence = app.config().state.default_divergence;
    let cursor = app
        .global_config_cursor
        .min(GLOBAL_CONFIG_ROWS.len().saturating_sub(1));
    let editing = app.refresh_interval_draft.as_ref();

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut caret: Option<(u16, u16)> = None;
    for (i, row) in GLOBAL_CONFIG_ROWS.iter().enumerate() {
        let selected = i == cursor;
        let row_editing = editing.filter(|_| *row == GlobalConfigRow::RefreshInterval);
        let line = detail_row(
            *row,
            selected,
            wrap_off,
            refresh_interval_ms,
            default_divergence,
            row_editing,
        );
        match row_editing {
            Some(input) => {
                // The native terminal cursor owns the caret; the row renders plain
                // (no highlight) with the edit gutter + sunken field, like the chain
                // threshold editor. x = "✎ " (2) + key block (KEY_W) + pre-caret cols.
                let cx = inner
                    .x
                    .saturating_add((2 + KEY_W + head_cols(input)) as u16);
                let cy = inner.y.saturating_add(lines.len() as u16);
                caret = Some((cx, cy));
                lines.push(line);
                lines.push(refresh_range_tooltip(input));
            }
            None => {
                lines.push(if selected {
                    highlight_row(line, inner.width as usize)
                } else {
                    line
                });
                if selected && let Some(tip) = row_hint(*row, default_divergence) {
                    lines.push(help_tooltip(&tip));
                }
            }
        }
    }

    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
    if let Some((cx, cy)) = caret {
        frame.set_cursor_position((cx, cy));
    }
}

/// Inline help for rows whose value doesn't self-describe.
fn row_hint(row: GlobalConfigRow, default_divergence: Option<DivergenceChoice>) -> Option<String> {
    match row {
        GlobalConfigRow::Theme => None,
        GlobalConfigRow::DivergenceDefault => {
            let tip = match default_divergence {
                None => "show the divergence modal when CC overwrites the symlink",
                Some(DivergenceChoice::Overwrite) => "adopt the new login into the current profile",
                Some(DivergenceChoice::NewProfile) => {
                    "save the new login as a separate profile, leave current profile alone"
                }
                Some(DivergenceChoice::Discard) => {
                    "restore the previous profile, clobbering the new login"
                }
            };
            Some(tip.to_string())
        }
        GlobalConfigRow::RefreshInterval => {
            Some("space cycles presets, wraps at the top; ⏎ types a custom value".to_string())
        }
        GlobalConfigRow::WrapOff => {
            Some("default when every fallback member is over its threshold".to_string())
        }
    }
}

fn detail_row(
    row: GlobalConfigRow,
    selected: bool,
    wrap_off: bool,
    refresh_interval_ms: u64,
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
        GlobalConfigRow::WrapOff => cycle_row(
            arrow,
            "when spent",
            &[("stay on last", !wrap_off), ("switch off all", wrap_off)],
            selected,
        ),
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
        line.push_span(Span::styled(
            format!("   {}s", refresh_interval_ms / 1000),
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
    let pad = KEY_W.saturating_sub("refresh".chars().count()).max(1);
    let mut spans = vec![
        arrow,
        Span::styled(format!("refresh{}", " ".repeat(pad)), label_style(true)),
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

/// A `  └ text` help sub-line: `LINE` leader, reason in `faint`.
fn help_tooltip(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  └ ", Style::default().fg(theme::line_color())),
        Span::styled(text.to_string(), theme::faint()),
    ])
}

/// A `  └ text` Invalid-input sub-line: both the `└ ` leader and the reason in
/// `DANGER`, distinguishing it from a plain help tooltip.
fn invalid_tooltip(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  └ ", theme::danger()),
        Span::styled(text.to_string(), theme::danger()),
    ])
}

/// Sub-line under the refresh field while typing: the valid range, in DANGER
/// when the current buffer parses out of range (or non-numeric), else faint.
fn refresh_range_tooltip(input: &InputState) -> Line<'static> {
    let range = format!(
        "{}–{} s",
        MIN_REFRESH_INTERVAL_MS / 1000,
        MAX_REFRESH_INTERVAL_MS / 1000
    );
    if parse_refresh_secs(input.trimmed()).is_none() {
        invalid_tooltip(&range)
    } else {
        help_tooltip(&range)
    }
}

/// A cloudy-tui cycle row: `key   [active]  other`. The active option is `ACCENT`
/// (bracketed only while the row is the cursor), the rest `TEXT_FAINT`; space
/// cycles the value in place. Reads as the segmented control it is, instead of a
/// single value that silently swaps text on cycle.
fn cycle_row(
    arrow: Span<'static>,
    key: &str,
    options: &[(&str, bool)],
    row_selected: bool,
) -> Line<'static> {
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    let key_style = label_style(row_selected);
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
