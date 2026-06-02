//! Fallback tab — master-detail, mirroring the Usage / Config layout. Left: the
//! ordered chain (plus a trailing `+ add` row), cursor = `❯`, color = active.
//! Right: the selected member's rotation detail — position, threshold, a 5h
//! gauge with a threshold tick, headroom, and where rotation flows next.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use super::super::app::{App, ChainItemKind, chain_items};
use super::super::theme;
use super::panes::{SELECTOR_WIDTH, section_box};
use crate::fallback::threshold_for;
use crate::profile::AppConfig;

/// Cells in the detail-pane gauges. Wide enough to read a threshold tick.
const GAUGE_W: usize = 22;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SELECTOR_WIDTH), Constraint::Min(20)])
        .split(area);

    draw_chain_selector(frame, cols[0], app);
    draw_chain_detail(frame, cols[1], app);
}

/// Left pane: the ordered chain members plus a trailing `+ add` row. Color
/// marks the active account; the cursor rides on `❯`.
fn draw_chain_selector(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = section_box("chain", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items = chain_items(app);
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("no accounts yet", theme::muted())))
                .style(theme::base()),
            inner,
        );
        return;
    }

    let cfg = app.config();
    let list_items: Vec<ListItem<'_>> = items
        .iter()
        .map(|item| match item {
            ChainItemKind::Member(i) => {
                let name = cfg
                    .state
                    .fallback_chain
                    .get(*i)
                    .cloned()
                    .unwrap_or_default();
                let style = if cfg.is_active(&name) {
                    Style::default().fg(theme::ACCENT_2)
                } else {
                    Style::default().fg(theme::TEXT)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:>2}  ", i + 1), theme::faint()),
                    Span::styled(name, style),
                ]))
            }
            ChainItemKind::Add => {
                ListItem::new(Line::from(Span::styled("    + add", theme::accent())))
            }
        })
        .collect();

    let list = List::new(list_items)
        .style(theme::base())
        .highlight_style(theme::selected_row().add_modifier(Modifier::BOLD))
        .highlight_symbol("❯ ");
    let mut state = ListState::default();
    state.select(Some(app.chain_cursor.min(items.len().saturating_sub(1))));
    frame.render_stateful_widget(list, inner, &mut state);
}

/// Right pane: rotation detail for the member under the cursor, the add hint on
/// the `+ add` row, or an empty-chain explainer.
fn draw_chain_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items = chain_items(app);
    let cfg = app.config();
    let chain_len = cfg.state.fallback_chain.len();

    let selected = items.get(app.chain_cursor.min(items.len().saturating_sub(1)));

    let (title, lines): (String, Vec<Line<'static>>) = match selected {
        Some(ChainItemKind::Member(i)) => {
            let name = cfg
                .state
                .fallback_chain
                .get(*i)
                .cloned()
                .unwrap_or_default();
            (name.clone(), member_detail(&cfg, &name, *i, chain_len))
        }
        Some(ChainItemKind::Add) => ("add to chain".to_string(), add_detail()),
        None => ("chain".to_string(), empty_detail()),
    };

    let block = section_box(&title, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

/// The member rotation card: position, threshold, a 5h gauge with a threshold
/// tick, the numeric headroom, and the next hop in the rotation.
fn member_detail(
    cfg: &AppConfig,
    name: &str,
    index: usize,
    chain_len: usize,
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

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Position + active state.
    let pos = Line::from(vec![
        Span::styled(kv_key("position"), theme::faint()),
        Span::styled(format!("#{} of {chain_len}", index + 1), theme::muted()),
        if active {
            Span::styled("   ● active", theme::orange())
        } else {
            Span::raw("")
        },
    ]);
    lines.push(pos);

    lines.push(Line::from(vec![
        Span::styled(kv_key("threshold"), theme::faint()),
        Span::styled(format!("{threshold:.0}%"), theme::muted()),
        Span::styled("  rotate off at this 5h utilization", theme::faint()),
    ]));
    lines.push(Line::from(""));

    // 5h gauge with a threshold tick.
    lines.push(Line::from(Span::styled("5h utilization", theme::label())));
    lines.push(Line::from(gauge_with_tick(pct, Some(threshold))));
    let (figure, figure_style) = match pct {
        Some(v) => {
            let headroom = (threshold - v).max(0.0);
            (
                format!("{v:.0}% used · {headroom:.0}% headroom to rotate"),
                Style::default().fg(health_color(v, threshold)),
            )
        }
        None => ("no usage data yet".to_string(), theme::faint()),
    };
    lines.push(Line::from(Span::styled(figure, figure_style)));
    lines.push(Line::from(""));

    // Where rotation flows when this member crosses its threshold.
    if chain_len > 1 {
        let next = (index + 1) % chain_len;
        let next_name = cfg
            .state
            .fallback_chain
            .get(next)
            .cloned()
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
    lines.push(Line::from(Span::styled(
        "⏎ reorder · set threshold · remove",
        theme::faint(),
    )));
    lines
}

fn add_detail() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "add an account to the rotation",
            theme::muted(),
        )),
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
        Line::from(Span::styled("⏎ to pick an account", theme::faint())),
    ]
}

fn empty_detail() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled("chain is empty", theme::muted())),
        Line::from(""),
        Line::from(Span::styled(
            "create an account first, then add it to the chain.",
            theme::dim(),
        )),
    ]
}

/// `GAUGE_W`-cell bar over 0..100 with the fill colored by headroom against the
/// threshold and a `┊` tick drawn at the threshold position.
fn gauge_with_tick(pct: Option<f64>, threshold: Option<f64>) -> Vec<Span<'static>> {
    let value = pct.unwrap_or(0.0).clamp(0.0, 100.0);
    let fill = ((value / 100.0) * GAUGE_W as f64).round() as usize;
    let fill = fill.min(GAUGE_W);
    let tick = threshold.map(|t| {
        (((t.clamp(0.0, 100.0) / 100.0) * GAUGE_W as f64).round() as usize).min(GAUGE_W - 1)
    });
    let fill_color = match (pct, threshold) {
        (Some(v), Some(t)) => health_color(v, t),
        (Some(_), None) => theme::ACCENT,
        _ => theme::TEXT_FAINT,
    };

    let mut spans = vec![Span::raw("[")];
    for i in 0..GAUGE_W {
        if Some(i) == tick {
            spans.push(Span::styled("┊", Style::default().fg(theme::TEXT)));
        } else if i < fill {
            spans.push(Span::styled("█", Style::default().fg(fill_color)));
        } else {
            spans.push(Span::styled("░", Style::default().fg(theme::LINE_STRONG)));
        }
    }
    spans.push(Span::raw("]"));
    spans
}

/// 5h headroom against the member's own threshold: green with room, yellow as
/// it nears, pink once it crosses — the point clauth rotates off it.
fn health_color(pct: f64, threshold: f64) -> Color {
    if pct >= threshold {
        theme::DANGER
    } else if pct >= threshold * 0.8 {
        theme::WARNING
    } else {
        theme::SUCCESS
    }
}

/// Pad a detail key to a fixed gutter so values line up.
fn kv_key(key: &str) -> String {
    const KEY_W: usize = 11;
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    format!("{key}{}", " ".repeat(pad))
}
