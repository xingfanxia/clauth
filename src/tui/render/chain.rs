//! Fallback tab — master-detail, mirroring the Config layout. Left: the ordered
//! chain (plus a trailing `+ add` row), cursor = `❯`, color = active. Right: the
//! selected member's rotation card (position, a 5h gauge with a threshold tick,
//! headroom, next hop) plus inline rows — a threshold stepper and a remove row —
//! or, on `+ add`, a candidate picker. Editing happens in place: ⏎ on the left
//! drops focus into the right pane, `+` / `-` step the threshold (or ⏎ on it to
//! type a value), ⏎ on remove arms then confirms. No popups.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{
    App, ChainItemKind, FALLBACK_ROWS, FallbackFocus, FallbackRow, InputState, chain_candidates,
    chain_items,
};
use super::super::theme;
use super::format::health_color;
use super::panes::{
    SELECTOR_WIDTH, draw_selector_list, highlight_row, name_color, section_box, select_line,
};
use crate::fallback::{DEFAULT_THRESHOLD, threshold_for};
use crate::profile::AppConfig;

/// Wide enough to read a threshold tick.
const GAUGE_W: usize = 22;
/// Key column width, matching the Setup tab.
const KEY_W: usize = 11;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SELECTOR_WIDTH), Constraint::Min(20)])
        .split(area);

    let chain_focused = app.fallback_focus == FallbackFocus::Chain;
    draw_chain_selector(frame, cols[0], app, chain_focused);
    draw_chain_detail(frame, cols[1], app);
}

fn draw_chain_selector(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let items = chain_items(app);
    let cfg = app.config();
    let sel = app.chain_cursor.min(items.len().saturating_sub(1));
    // Selector is the first (and only) bordered panel in the left column.
    draw_selector_list(frame, area, "chain", focused, sel, |w| {
        items
            .iter()
            .enumerate()
            .map(|(row, item)| {
                let selected = row == sel;
                let line = match item {
                    ChainItemKind::Member(i) => {
                        let name = cfg
                            .state
                            .fallback_chain
                            .get(*i)
                            .map(|n| n.to_string())
                            .unwrap_or_default();
                        let style = name_color(cfg.is_active(&name));
                        // Cursor + ordinal share the leading span so the name
                        // lands at spans[1] — the item `highlight_row` bolds.
                        // Caret only in the focused pane.
                        let rail = if selected && focused {
                            Span::styled(format!("❯ {:>2}  ", i + 1), theme::accent())
                        } else {
                            Span::styled(format!("  {:>2}  ", i + 1), theme::faint())
                        };
                        Line::from(vec![rail, Span::styled(name, style)])
                    }
                    ChainItemKind::Add => {
                        let arrow = if selected && focused {
                            Span::styled("❯ ", theme::accent())
                        } else {
                            Span::raw("  ")
                        };
                        Line::from(vec![arrow, Span::styled("    + add", theme::accent())])
                    }
                };
                select_line(line, selected, focused, w)
            })
            .collect()
    });
}

fn draw_chain_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let detail_focused = app.fallback_focus == FallbackFocus::Detail;
    let inner_w = section_box("", detail_focused, false).inner(area).width as usize;
    let items = chain_items(app);
    let selected = items
        .get(app.chain_cursor.min(items.len().saturating_sub(1)))
        .copied();

    // `Add` arm must NOT hold the `config` guard — `add_detail` re-locks it via
    // `chain_candidates`, and the mutex is non-reentrant (deadlock on `+ add` row).
    let (title, lines): (String, Vec<Line<'static>>) = match selected {
        Some(ChainItemKind::Member(i)) => {
            let cfg = app.config();
            let chain_len = cfg.state.fallback_chain.len();
            let name = cfg
                .state
                .fallback_chain
                .get(i)
                .map(|n| n.to_string())
                .unwrap_or_default();
            let lines = member_detail(
                &cfg,
                &name,
                i,
                chain_len,
                detail_focused,
                app.fallback_detail_cursor,
                app.fallback_armed_remove,
                app.fallback_threshold_draft.as_ref(),
                inner_w,
            );
            (name, lines)
        }
        Some(ChainItemKind::Add) => (
            "add to chain".to_string(),
            add_detail(app, detail_focused, inner_w),
        ),
        None => ("chain".to_string(), empty_detail()),
    };

    // Detail is the second panel on this screen — not first.
    let block = section_box(&title, detail_focused, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);

    // Position the native terminal cursor when the threshold field is being typed.
    // The threshold row is always FALLBACK_ROWS[0]; member_detail pushes exactly
    // 8 fixed lines before the loop (position, blank, label, gauge, figure,
    // blank, next-hop-or-only-member, blank).
    if detail_focused
        && let Some(ChainItemKind::Member(_)) = selected
        && let Some(draft) = &app.fallback_threshold_draft
    {
        // x = "❯ " (2) + "threshold" (9) + pad (KEY_W - 9 + 1 = 3) + cols before caret
        // = 2 + KEY_W + 1 + head_cols  =  14 + head_cols
        let prefix_cols = 2 + KEY_W + 1 + head_cols(draft);
        let cx = inner.x.saturating_add(prefix_cols as u16);
        let cy = inner.y.saturating_add(8);
        frame.set_cursor_position((cx, cy));
    }
}

/// Position + active state, 5h gauge with threshold tick, headroom, next hop,
/// inline threshold stepper/editor and remove rows. Caret only when focused.
#[allow(clippy::too_many_arguments)]
fn member_detail(
    cfg: &AppConfig,
    name: &str,
    index: usize,
    chain_len: usize,
    focused: bool,
    row_cursor: usize,
    armed_remove: bool,
    editing: Option<&InputState>,
    width: usize,
) -> Vec<Line<'static>> {
    let Some(profile) = cfg.find(name) else {
        return vec![Line::from(Span::styled(
            "account no longer exists — remove it from the chain",
            theme::danger(),
        ))];
    };

    let threshold = threshold_for(profile);
    let pct = profile
        .usage
        .as_ref()
        .and_then(|u| u.five_hour.as_ref())
        .map(|w| w.utilization);
    let active = cfg.is_active(name);
    let cursor = row_cursor.min(FALLBACK_ROWS.len() - 1);

    let mut lines: Vec<Line<'static>> = Vec::new();

    let mut position_spans = vec![
        Span::styled(kv_key("position"), theme::faint()),
        Span::styled(format!("#{} of {chain_len}", index + 1), theme::dim()),
    ];
    if active {
        position_spans.extend([
            Span::raw("   "),
            Span::styled("●", theme::success()),
            Span::styled(" active", theme::dim()),
        ]);
    }
    lines.push(Line::from(position_spans));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled("5h utilization", theme::label())));
    lines.push(Line::from(gauge_with_tick(pct, Some(threshold))));
    let (figure, figure_style) = match pct {
        Some(v) => {
            let headroom = (threshold - v).max(0.0);
            (
                format!("{v:.0}% used · {headroom:.0}% until rotate"),
                Style::default().fg(health_color(v, threshold)),
            )
        }
        None => ("no usage data yet".to_string(), theme::faint()),
    };
    lines.push(Line::from(Span::styled(figure, figure_style)));
    lines.push(Line::from(""));

    if chain_len > 1 {
        let next = (index + 1) % chain_len;
        let next_name = cfg
            .state
            .fallback_chain
            .get(next)
            .map(|n| n.to_string())
            .unwrap_or_default();
        let arrow = if next == 0 {
            Span::styled("↺ wraps to ", theme::orange())
        } else {
            Span::styled("→ next ", theme::accent())
        };
        lines.push(Line::from(vec![
            arrow,
            Span::styled(next_name, theme::dim()),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "only member — rotation has nowhere to go",
            theme::faint(),
        )));
    }
    lines.push(Line::from(""));

    let wrap_off = cfg.state.wrap_off;
    for (i, row) in FALLBACK_ROWS.iter().enumerate() {
        let selected = focused && i == cursor;
        let row_editing = if *row == FallbackRow::Threshold {
            editing
        } else {
            None
        };
        let line = detail_row(
            *row,
            selected,
            threshold,
            armed_remove,
            wrap_off,
            row_editing,
        );
        lines.push(if selected {
            highlight_row(line, width)
        } else {
            line
        });
        if selected && row_editing.is_none() {
            let tip = match row {
                FallbackRow::Threshold => Some("rotate to next account when 5h usage reaches this"),
                FallbackRow::WrapOff => Some("what to do once every member is over its threshold"),
                FallbackRow::Remove => None,
            };
            if let Some(tip) = tip {
                lines.push(Line::from(vec![
                    Span::styled("  └ ", theme::faint()),
                    Span::styled(tip, theme::faint()),
                ]));
            }
        }
    }
    lines
}

fn detail_row(
    row: FallbackRow,
    selected: bool,
    threshold: f64,
    armed_remove: bool,
    wrap_off: bool,
    editing: Option<&InputState>,
) -> Line<'static> {
    let arrow = if selected {
        Span::styled("❯ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    match row {
        FallbackRow::WrapOff => {
            let pad = KEY_W.saturating_sub("when spent".len()).max(1);
            // Spell out the action so "off" is never shown as a bare value.
            let (value, style) = if wrap_off {
                ("switch off all accounts", theme::orange())
            } else {
                ("stay on last account", theme::accent())
            };
            Line::from(vec![
                arrow,
                Span::styled(format!("when spent{}", " ".repeat(pad)), theme::body()),
                Span::styled(value.to_string(), style),
            ])
        }
        FallbackRow::Threshold => {
            let pad = KEY_W.saturating_sub("threshold".len()).max(1);
            let mut spans = vec![
                arrow,
                Span::styled(format!("threshold{}", " ".repeat(pad)), theme::body()),
            ];
            match editing {
                Some(input) => {
                    spans.extend(value_caret(input));
                    spans.push(Span::styled("%", theme::faint()));
                }
                None => {
                    spans.push(Span::styled(format!("{threshold:.0}%"), theme::accent()));
                    if (threshold - DEFAULT_THRESHOLD).abs() > f64::EPSILON {
                        spans.push(Span::styled(
                            format!("   default: {DEFAULT_THRESHOLD:.0}%"),
                            theme::faint(),
                        ));
                    }
                }
            }
            Line::from(spans)
        }
        FallbackRow::Remove => {
            let label = if armed_remove {
                "remove from chain — ⏎ again to confirm".to_string()
            } else {
                "remove from chain".to_string()
            };
            Line::from(vec![arrow, Span::styled(label, theme::danger())])
        }
    }
}

fn value_caret(input: &InputState) -> Vec<Span<'static>> {
    // The terminal cursor (set via frame.set_cursor_position) owns the caret
    // glyph — render the whole buffer with uniform body styling.
    let body = Style::default()
        .fg(theme::text_color())
        .bg(theme::bg_sunken());
    vec![Span::styled(input.value.clone(), body)]
}

/// Display columns occupied by the text before the caret in `input`.
fn head_cols(input: &InputState) -> usize {
    input.value[..input.cursor.min(input.value.len())]
        .chars()
        .count()
}

fn add_detail(app: &App, focused: bool, width: usize) -> Vec<Line<'static>> {
    let candidates = chain_candidates(app);
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("add an account to the rotation", theme::dim())),
        Line::from(""),
        Line::from(Span::styled(
            "clauth auto-switches off a member when its 5h window crosses the",
            theme::dim(),
        )),
        Line::from(Span::styled(
            "member's threshold, moving to the next account in the chain.",
            theme::dim(),
        )),
        Line::from(""),
    ];

    if candidates.is_empty() {
        lines.push(Line::from(Span::styled(
            "every account is already in the chain",
            theme::faint(),
        )));
        return lines;
    }

    if !focused {
        return lines;
    }

    let cursor = app
        .fallback_detail_cursor
        .min(candidates.len().saturating_sub(1));
    for (i, name) in candidates.iter().enumerate() {
        let selected = i == cursor;
        let arrow = if selected {
            Span::styled("❯ ", theme::accent())
        } else {
            Span::raw("  ")
        };
        let line = Line::from(vec![arrow, Span::styled(name.clone(), theme::body())]);
        lines.push(if selected {
            highlight_row(line, width)
        } else {
            line
        });
    }
    lines
}

fn empty_detail() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled("chain is empty", theme::dim())),
        Line::from(""),
        Line::from(Span::styled(
            "create an account first, then add it to the chain.",
            theme::dim(),
        )),
    ]
}

/// `GAUGE_W`-cell bar with fill colored by headroom and a `┊` tick at the threshold.
fn gauge_with_tick(pct: Option<f64>, threshold: Option<f64>) -> Vec<Span<'static>> {
    let value = pct.unwrap_or(0.0).clamp(0.0, 100.0);
    let fill = ((value / 100.0) * GAUGE_W as f64).round() as usize;
    let fill = fill.min(GAUGE_W);
    let tick = threshold.map(|t| {
        (((t.clamp(0.0, 100.0) / 100.0) * GAUGE_W as f64).round() as usize).min(GAUGE_W - 1)
    });
    let fill_color = match (pct, threshold) {
        (Some(v), Some(t)) => health_color(v, t),
        (Some(_), None) => theme::accent_color(),
        _ => theme::text_faint_color(),
    };

    let mut spans = vec![];
    for i in 0..GAUGE_W {
        if Some(i) == tick {
            spans.push(Span::styled("┊", theme::body()));
        } else if i < fill {
            spans.push(Span::styled("█", Style::default().fg(fill_color)));
        } else {
            spans.push(Span::styled("░", theme::line_strong()));
        }
    }
    spans
}

fn kv_key(key: &str) -> String {
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    format!("{key}{}", " ".repeat(pad))
}
