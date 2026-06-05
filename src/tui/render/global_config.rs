//! Program-wide Config tab — a single panel of global settings, distinct from
//! the per-account Setup tab. Rows back real persisted state in `AppState`:
//! the theme tier (`[theme]`) and the chain-wide wrap-off default. ↑↓ walks the
//! rows; ⏎/space cycles the theme or flips wrap-off in place. No left selector,
//! no popups — these settings are global, not per-account.

use ratatui::Frame;
use ratatui::layout::Rect;
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
    let cursor = app
        .global_config_cursor
        .min(GLOBAL_CONFIG_ROWS.len().saturating_sub(1));

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, row) in GLOBAL_CONFIG_ROWS.iter().enumerate() {
        let selected = i == cursor;
        let line = detail_row(*row, selected, wrap_off);
        lines.push(if selected {
            highlight_row(line, inner.width as usize)
        } else {
            line
        });
        if selected && let Some(tip) = row_hint(*row) {
            lines.push(Line::from(vec![
                Span::styled("  └ ", theme::faint()),
                Span::styled(tip, theme::faint()),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

/// Inline help for rows whose value doesn't self-describe.
fn row_hint(row: GlobalConfigRow) -> Option<&'static str> {
    match row {
        GlobalConfigRow::Theme => Some("color depth · ⏎ cycles · applies immediately"),
        GlobalConfigRow::WrapOff => {
            Some("default when every fallback member is over its threshold")
        }
    }
}

fn detail_row(row: GlobalConfigRow, selected: bool, wrap_off: bool) -> Line<'static> {
    let arrow = if selected {
        Span::styled("❯ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    match row {
        GlobalConfigRow::Theme => {
            let (value, style) = match theme::tier() {
                Tier::Full => ("full · 24-bit truecolor", theme::accent()),
                Tier::Compatible => ("compatible · xterm-256", theme::orange()),
            };
            kv(arrow, "theme", value.to_string(), style)
        }
        GlobalConfigRow::WrapOff => {
            // Spell out the action so "off" is never shown as a bare value.
            let (value, style) = if wrap_off {
                ("switch off all accounts", theme::orange())
            } else {
                ("stay on last account", theme::accent())
            };
            kv(arrow, "when spent", value.to_string(), style)
        }
    }
}

fn kv(
    arrow: Span<'static>,
    key: &str,
    value: String,
    value_style: ratatui::style::Style,
) -> Line<'static> {
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    Line::from(vec![
        arrow,
        Span::styled(format!("{key}{}", " ".repeat(pad)), theme::body()),
        Span::styled(value, value_style),
    ])
}
