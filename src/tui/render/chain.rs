//! Fallback tab — master-detail, mirroring the Config layout. Left: the ordered
//! chain (plus a trailing `+ add` row), cursor = `❯`, active member name in
//! orange. Right: the selected member's rotation card — labeled
//! key:value rows (`priority`, `5h usage` gauge with a threshold tick, `rotate at`
//! threshold stepper, `last resort` toggle, `remove`) — or, on `+ add`, a
//! candidate picker. Order = priority (reorder with ⇧↑↓). The chain-global
//! wrap-off setting lives on the Config tab, not here. Editing happens in
//! place: ⏎ on the left drops focus into the right pane, `+` / `-` step the
//! threshold (or ⏎ on it to type a value), space/⏎ flips `last resort`, ⏎ on
//! remove arms then confirms. No popups.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{
    App, ChainItemKind, FALLBACK_ROWS, FallbackFocus, FallbackRow, InputState, chain_candidates,
    chain_items, parse_threshold,
};
use super::super::theme;
use super::panes::{
    active_pill, bold_when, draw_selector_list, head_cols, help_tooltip_lines, highlight_row,
    invalid_tooltip_lines, key_cell, label_style, name_color, section_box, section_box_verbatim,
    select_line, selector_width, wrap_words,
};
use crate::fallback::{DEFAULT_THRESHOLD, soonest_resume, threshold_for};
use crate::profile::AppConfig;
use crate::usage::humanize_duration;

/// Wide enough to read a threshold tick.
const GAUGE_W: usize = 22;
/// Key column width: the longest key (`last resort`, 11), matching the Config
/// tab's `KEY_W` so the two master-detail panes open their value column at the
/// same place. `KEY_GUTTER` is the separator, so an exactly-fitting key never
/// collides with its value.
const KEY_W: usize = 11;
/// Fixed gap between the padded key and the value column (house standard).
const KEY_GUTTER: usize = 2;
/// Fixed lines `member_detail` pushes before the FALLBACK_ROWS loop: priority,
/// blank, `5h usage` gauge (key row), headroom figure, blank. The native-cursor
/// math and the `rotate at` row index both key off this.
const ROWS_BEFORE: usize = 5;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let [selector_area, detail_area] = Layout::horizontal([
        Constraint::Length(selector_width(area.width)),
        Constraint::Min(20),
    ])
    .areas(area);

    let chain_focused = app.fallback_focus == FallbackFocus::Chain;
    draw_chain_selector(frame, selector_area, app, chain_focused);
    draw_chain_detail(frame, detail_area, app);
}

fn draw_chain_selector(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let items = chain_items(app);
    let cfg = app.config();
    let sel = app.chain_cursor.min(items.len().saturating_sub(1));
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
                        let rail = if selected && focused {
                            Span::styled(
                                format!("❯ {:>2}  ", i + 1),
                                theme::accent().add_modifier(Modifier::BOLD),
                            )
                        } else {
                            Span::styled(format!("  {:>2}  ", i + 1), theme::faint())
                        };
                        let ns = bold_when(name_color(cfg.is_active(&name)), selected && focused);
                        let spans = vec![rail, Span::styled(name.clone(), ns)];
                        Line::from(spans)
                    }
                    ChainItemKind::Add => {
                        let arrow = if selected && focused {
                            Span::styled("❯ ", theme::accent().add_modifier(Modifier::BOLD))
                        } else {
                            Span::raw("  ")
                        };
                        Line::from(vec![
                            arrow,
                            Span::styled(
                                "    + add",
                                bold_when(theme::accent(), selected && focused),
                            ),
                        ])
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
    // `is_name`: member names render in original case; structural titles stay uppercased.
    let (title, is_name, lines): (String, bool, Vec<Line<'static>>) = match selected {
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
            (name, true, lines)
        }
        Some(ChainItemKind::Add) => (
            "add to chain".to_string(),
            false,
            add_detail(app, detail_focused, inner_w),
        ),
        None => ("chain".to_string(), false, empty_detail()),
    };

    let block = if is_name {
        section_box_verbatim(&title, detail_focused, false)
    } else {
        section_box(&title, detail_focused, false)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);

    // Position the native terminal cursor when the threshold field is being typed,
    // matching the post-draw cursor path the other edit screens use. The `rotate at`
    // row is FALLBACK_ROWS[0]; member_detail pushes exactly `ROWS_BEFORE` fixed
    // lines before the loop (priority, blank, gauge, figure, blank).
    if detail_focused
        && let Some(ChainItemKind::Member(_)) = selected
        && let Some(draft) = &app.fallback_threshold_draft
    {
        // x = "❯ " (2) + "rotate at" key block (KEY_W + KEY_GUTTER cols) + cols before caret.
        let prefix_cols = 2 + KEY_W + KEY_GUTTER + head_cols(draft);
        let cx = inner.x.saturating_add(prefix_cols as u16);
        let cy = inner.y.saturating_add(ROWS_BEFORE as u16);
        frame.set_cursor_position((cx, cy));
    }
}

/// Priority + `[ active ]` pill, 5h gauge with threshold tick, headroom figure, and the
/// inline `rotate at` threshold stepper/editor + `last resort` toggle + `remove` rows.
/// Caret only when focused.
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
            "account no longer exists · remove it from the chain",
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

    // `priority` — position in the chain (order = priority).
    let value = format!("#{} of {chain_len}", index + 1);
    let mut priority_spans = vec![
        Span::styled(key_cell("priority", KEY_W, KEY_GUTTER), theme::label()),
        Span::styled(value.clone(), theme::body()),
    ];
    if active {
        let left_w = KEY_W + KEY_GUTTER + value.chars().count();
        let indicator_w = "[ active ]".chars().count(); // 10
        let pad = width.saturating_sub(left_w).saturating_sub(indicator_w);
        priority_spans.push(Span::raw(" ".repeat(pad)));
        priority_spans.extend(active_pill());
    }
    lines.push(Line::from(priority_spans));
    lines.push(Line::from(""));

    // `5h usage` — gauge lives on the kv key row (matching `priority` / `rotate
    // at` grammar), headroom figure indented beneath it. Two lines, not three:
    // the standalone eyebrow is folded into the key.
    let mut gauge_spans = vec![Span::styled(
        key_cell("5h usage", KEY_W, KEY_GUTTER),
        theme::label(),
    )];
    gauge_spans.extend(gauge_with_tick(pct, Some(threshold)));
    if let Some(v) = pct {
        gauge_spans.push(Span::styled(format!("  {v:.0}% used"), theme::util(v)));
    } else {
        gauge_spans.push(Span::styled("  no data yet", theme::faint()));
    }
    lines.push(Line::from(gauge_spans));

    let figure = match pct {
        Some(v) => format!("{:.0}% until rotate", (threshold - v).max(0.0)),
        None => String::new(),
    };
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(KEY_W + KEY_GUTTER)),
        Span::styled(figure, theme::faint()),
    ]));
    lines.push(Line::from(""));

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
            profile.last_resort,
            armed_remove,
            row_editing,
        );
        lines.push(if selected {
            highlight_row(line, width)
        } else {
            line
        });
        // `rotate at` shows its help hint while the row is selected; while typing,
        // it swaps to an always-on `0–100 %` range tooltip (faint, DANGER when out
        // of range) — mirroring the Config-tab refresh editor.
        if *row == FallbackRow::Threshold {
            match row_editing {
                Some(input) => lines.extend(threshold_range_tooltip(input, width)),
                None if selected => lines.extend(help_tooltip_lines(
                    &format!("switches to the next account once 5h usage hits {threshold:.0}%"),
                    width,
                )),
                None => {}
            }
        }
        if *row == FallbackRow::LastResort && selected {
            lines.extend(help_tooltip_lines(
                &last_resort_hint(cfg, name, profile.last_resort),
                width,
            ));
        }
    }

    // All-exhausted sibling of the Overview projection line: when EVERY chain
    // member is currently maxed, name whichever one resumes first instead of
    // leaving the recovery implicit (issue #10 follow-up). Chain-wide, so it
    // renders under whichever member happens to be selected.
    if let Some((resume_name, eta)) = soonest_resume(cfg) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("resumes: {resume_name} in ~{}", humanize_duration(eta)),
            theme::faint(),
        )));
    }
    lines
}

/// Hint under the `last resort` toggle — phrased for the state flipping it
/// would produce: on → describes the standing behavior; off → what turning it
/// on does, naming the member the (exclusive) mark would move away from.
fn last_resort_hint(cfg: &AppConfig, name: &str, on: bool) -> String {
    if on {
        return "the chain parks here once every account is spent".to_string();
    }
    match cfg
        .profiles
        .iter()
        .find(|p| p.last_resort && p.name != *name)
    {
        Some(marked) => format!("move the chain's parking spot here from '{}'", marked.name),
        None => "park the chain here once every account is spent".to_string(),
    }
}

/// Sub-line under the `rotate at` field while typing: the valid range, in DANGER
/// when the current buffer parses out of range (or non-numeric), else faint —
/// the threshold twin of the Config-tab refresh editor's `refresh_range_tooltip`.
fn threshold_range_tooltip(input: &InputState, width: usize) -> Vec<Line<'static>> {
    let range = "0-100 %";
    if parse_threshold(input.trimmed()).is_none() {
        invalid_tooltip_lines(range, width)
    } else {
        help_tooltip_lines(range, width)
    }
}

fn detail_row(
    row: FallbackRow,
    selected: bool,
    threshold: f64,
    last_resort: bool,
    armed_remove: bool,
    editing: Option<&InputState>,
) -> Line<'static> {
    let arrow = if editing.is_some() {
        Span::styled(format!("{} ", theme::edit_glyph()), theme::accent())
    } else if selected {
        Span::styled("❯ ", theme::accent().add_modifier(Modifier::BOLD))
    } else {
        Span::raw("  ")
    };
    match row {
        FallbackRow::Threshold => {
            let mut spans = vec![
                arrow,
                Span::styled(
                    key_cell("rotate at", KEY_W, KEY_GUTTER),
                    label_style(selected),
                ),
            ];
            match editing {
                Some(input) => {
                    // Invalid typed value renders in DANGER (the gutter `└ invalid input`
                    // tooltip carries the reason); valid keeps body styling.
                    let invalid = parse_threshold(input.trimmed()).is_none();
                    spans.extend(value_caret(input, invalid));
                    let pct_style = if invalid {
                        theme::danger()
                    } else {
                        theme::faint()
                    };
                    // Leading space so the native caret (parked at the buffer end)
                    // sits in a blank cell and `%` renders after it — matching the
                    // refresh editor's ` s` unit.
                    spans.push(Span::styled(" %", pct_style));
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
        FallbackRow::LastResort => {
            let (value, style) = if last_resort {
                (theme::toggle_on().to_string(), theme::accent())
            } else {
                (theme::toggle_off().to_string(), theme::faint())
            };
            Line::from(vec![
                arrow,
                Span::styled(
                    key_cell("last resort", KEY_W, KEY_GUTTER),
                    label_style(selected),
                ),
                Span::styled(value, style),
            ])
        }
        FallbackRow::Remove => {
            let label = if armed_remove {
                "press again to remove".to_string()
            } else {
                "remove from chain".to_string()
            };
            Line::from(vec![
                arrow,
                Span::styled(key_cell("remove", KEY_W, KEY_GUTTER), label_style(selected)),
                Span::styled(label, theme::danger()),
            ])
        }
    }
}

fn value_caret(input: &InputState, invalid: bool) -> Vec<Span<'static>> {
    // The terminal cursor (set via frame.set_cursor_position) owns the caret
    // glyph — render the whole buffer with uniform styling.
    let body = if invalid {
        theme::danger()
    } else {
        theme::body()
    }
    .bg(theme::bg_sunken());
    vec![Span::styled(input.value.clone(), body)]
}

fn add_detail(app: &App, focused: bool, width: usize) -> Vec<Line<'static>> {
    let candidates = chain_candidates(app);
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("add an account to the rotation", theme::dim())),
        Line::from(""),
    ];
    lines.extend(
        wrap_words(
            "when an account's 5h usage crosses its threshold, clauth hands the \
             session to the next account in the chain.",
            width,
        )
        .into_iter()
        .map(|seg| Line::from(Span::styled(seg, theme::dim()))),
    );
    lines.push(Line::from(""));

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
            Span::styled("❯ ", theme::accent().add_modifier(Modifier::BOLD))
        } else {
            Span::raw("  ")
        };
        let ns = bold_when(theme::body(), selected);
        let line = Line::from(vec![arrow, Span::styled(name.clone(), ns)]);
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

/// `GAUGE_W`-cell usage bar: fill colored by the usage thresholds (via
/// `util_color`), with a `│` tick at the rotate threshold. Once the fill reaches
/// or passes the tick column, the tick is drawn `│` in `DANGER` over the fill so
/// the "over limit" marker is never occluded.
fn gauge_with_tick(pct: Option<f64>, threshold: Option<f64>) -> Vec<Span<'static>> {
    let value = pct.unwrap_or(0.0).clamp(0.0, 100.0);
    let fill = ((value / 100.0) * GAUGE_W as f64).round() as usize;
    let fill = fill.min(GAUGE_W);
    let tick = threshold.map(|t| {
        (((t.clamp(0.0, 100.0) / 100.0) * GAUGE_W as f64).round() as usize).min(GAUGE_W - 1)
    });
    let fill_style = match pct {
        Some(v) => theme::util(v),
        None => theme::faint(),
    };

    let mut spans = vec![];
    for i in 0..GAUGE_W {
        if Some(i) == tick {
            // Below the fill the tick is a neutral marker; once fill reaches it,
            // promote to DANGER so it stays visible over the blocks.
            let style = if i < fill {
                theme::danger()
            } else {
                theme::dim()
            };
            spans.push(Span::styled("│", style));
        } else if i < fill {
            spans.push(Span::styled("█", fill_style));
        } else {
            spans.push(Span::styled("░", theme::line_strong()));
        }
    }
    spans
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_chain.rs"]
mod tests;
