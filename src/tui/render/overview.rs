//! Overview tab: accounts table + fallback flow, inside one content frame.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use super::super::app::{App, MainItemKind};
use super::super::theme;
use super::format::{
    account_type_label, account_type_style, fixed, fixed_split, health_color, name_style,
    spinner_frame, spinner_style, window_summary_spans_bracketed,
};
use super::panes::{draw_scrollbar, empty_state, section_box, select_line};
use crate::fallback::threshold_for;
use crate::profile::{AppConfig, Profile};
use crate::usage::{ProfileActivity, now_ms};

/// `XXXs` + 1 trailing space = 5 chars; spinner padded to same width.
const TIMER_SLOT: usize = 5;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let target = if area.height >= 18 { 8 } else { 5 };
    let cap = area.height.saturating_sub(7).max(3);
    let chain_height = target.min(cap);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(7), Constraint::Length(chain_height)])
        .split(area);

    draw_overview_accounts(frame, chunks[0], app);
    draw_fallback_overview(frame, chunks[1], app);
}

fn draw_overview_accounts(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // Sole interactive content panel on this screen — always focused.
    let focused = true;
    let block = section_box("accounts", focused, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.config().profiles.is_empty() {
        frame.render_widget(empty_state("no accounts yet", "n", "to create one"), inner);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let widths = OverviewWidths::new(chunks[1].width, app);
    let header = overview_header(&widths);
    frame.render_widget(Paragraph::new(header).style(theme::base()), chunks[0]);

    let items = app.main_items();
    let sel = app.profile_cursor.min(items.len().saturating_sub(1));
    let width = chunks[1].width;
    let rows: Vec<ListItem<'_>> = items
        .iter()
        .enumerate()
        .map(|(row, item)| match item {
            MainItemKind::Profile(idx) => {
                let selected = row == sel;
                let line = render_overview_row(app, *idx, &widths, selected, focused);
                ListItem::new(select_line(line, selected, focused, width))
            }
        })
        .collect();

    let total = items.len();
    let list = List::new(rows).style(theme::base());
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(sel));
    frame.render_stateful_widget(list, chunks[1], &mut state);

    let viewport = chunks[1].height as usize;
    draw_scrollbar(frame, chunks[1], total, state.offset(), viewport);
}

#[derive(Debug, Clone, Copy)]
struct OverviewWidths {
    name: usize,
    kind: usize,
    five_hour: usize,
    seven_day: usize,
    route: usize,
    gap: usize,
}

impl OverviewWidths {
    fn new(width: u16, app: &App) -> Self {
        let total = width as usize;
        let max_name = app
            .config()
            .profiles
            .iter()
            .map(|p| p.name.chars().count())
            .max()
            .unwrap_or(8);
        let mut name = max_name.clamp(8, if total >= 86 { 22 } else { 16 });
        let mut kind = if total >= 92 {
            16
        } else if total >= 66 {
            12
        } else {
            6
        };
        // 26 = [bar]+pct+reset, 17 = [bar]+pct only.
        let mut five_hour = if total >= 81 {
            26
        } else if total >= 64 {
            17
        } else {
            12
        };
        let mut seven_day = if total >= 102 {
            26
        } else if total >= 93 {
            17
        } else if total >= 58 {
            5
        } else {
            0
        };
        let mut route = if total >= 88 {
            13
        } else if total >= 68 {
            9
        } else {
            0
        };

        let gap_min = 2;
        while fixed_overview_width(name, kind, five_hour, seven_day, route, gap_min) > total {
            if route > 0 {
                route = 0;
            } else if seven_day >= 17 {
                seven_day = 5;
            } else if seven_day > 0 {
                seven_day = 0;
            } else if five_hour > 17 {
                five_hour = 17;
            } else if five_hour > 12 {
                five_hour = 12;
            } else if kind > 6 {
                kind = 6;
            } else if name > 8 {
                name -= 1;
            } else {
                break;
            }
        }

        // Spread leftover width into gaps so columns breathe on wide terminals.
        let base = fixed_overview_width(name, kind, five_hour, seven_day, route, gap_min);
        let column_count = 3 + usize::from(seven_day > 0) + usize::from(route > 0);
        let gap_slots = column_count.saturating_sub(1).max(1);
        let gap = (gap_min + total.saturating_sub(base) / gap_slots).clamp(gap_min, 8);

        Self {
            name,
            kind,
            five_hour,
            seven_day,
            route,
            gap,
        }
    }
}

fn fixed_overview_width(
    name: usize,
    kind: usize,
    five_hour: usize,
    seven_day: usize,
    route: usize,
    gap: usize,
) -> usize {
    let column_count = 3 + usize::from(seven_day > 0) + usize::from(route > 0);
    // 2 = cursor prefix. Timer slot is in the gap before 5h, not a column.
    // kind→timer gap is 4 chars narrower than standard (min 1).
    let narrow = gap.saturating_sub(4).max(1);
    let standard_gaps = column_count.saturating_sub(2);
    2 + name + kind + five_hour + seven_day + route + standard_gaps * gap + narrow
}

fn overview_header(widths: &OverviewWidths) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", theme::label())];
    spans.push(Span::styled(fixed("account", widths.name), theme::label()));
    spans.push(gap(widths));
    spans.push(Span::styled(fixed("type", widths.kind), theme::label()));
    spans.push(narrow_gap(widths));
    // Blank TIMER_SLOT keeps the "5h" header aligned over the bar.
    spans.push(Span::raw(" ".repeat(TIMER_SLOT)));
    spans.push(Span::styled(fixed("5h", widths.five_hour), theme::label()));
    if widths.seven_day > 0 {
        spans.push(gap(widths));
        spans.push(Span::styled(fixed("7d", widths.seven_day), theme::label()));
    }
    if widths.route > 0 {
        spans.push(gap(widths));
        spans.push(Span::styled(fixed("route", widths.route), theme::label()));
    }
    Line::from(spans)
}

fn render_overview_row(
    app: &App,
    idx: usize,
    widths: &OverviewWidths,
    selected: bool,
    focused: bool,
) -> Line<'static> {
    let cfg = app.config();
    let Some(profile) = cfg.profiles.get(idx) else {
        return Line::from("");
    };

    let active = cfg.is_active(&profile.name);
    let name_str = profile.name.to_string();
    // Caret only in the focused pane.
    let cursor = if selected && focused {
        Span::styled("❯ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    let (name_text, name_pad) = fixed_split(&profile.name, widths.name);
    let name = Span::styled(
        name_text,
        if active {
            name_style(profile).fg(theme::accent_2_color())
        } else {
            name_style(profile)
        },
    );
    let name_pad = Span::raw(name_pad);

    // Spinner while in-flight, seconds countdown when Idle.
    // Right-aligned in TIMER_SLOT-1 chars + 1 trailing space.
    let timer_span = {
        let inner = TIMER_SLOT - 1;
        let activity = app
            .activity
            .lock()
            .ok()
            .and_then(|g| g.get(&name_str).copied())
            .unwrap_or(ProfileActivity::Idle);
        if !matches!(activity, ProfileActivity::Idle) {
            let frame = spinner_frame(app.tick_count);
            let style = spinner_style(activity);
            Span::styled(format!("{frame:>inner$} ", inner = inner), style)
        } else {
            let secs_str = app
                .next_refresh_per_profile
                .lock()
                .ok()
                .and_then(|m| m.get(&name_str).copied())
                .map(|next_ms| {
                    let now = now_ms();
                    let secs = ((next_ms as i64 - now as i64) / 1000).max(0);
                    format!("{secs}s")
                });
            match secs_str {
                Some(s) => Span::styled(format!("{:>inner$} ", s, inner = inner), theme::faint()),
                None => Span::raw(" ".repeat(TIMER_SLOT)),
            }
        }
    };

    let mut spans = vec![cursor, name, name_pad, gap(widths)];
    spans.push(Span::styled(
        fixed(&account_type_label(profile), widths.kind),
        account_type_style(profile),
    ));
    spans.push(narrow_gap(widths));
    spans.push(timer_span);
    // Bracketed bars ([███░░░]) for overview account rows only.
    // Usage-page gauges, chain bars, and fallback thresholds stay bracket-less.
    let five_spans = window_summary_spans_bracketed(
        profile.usage.as_ref().and_then(|u| u.five_hour.as_ref()),
        widths.five_hour,
        true,
    );
    let five_len: usize = five_spans.iter().map(|s| s.content.chars().count()).sum();
    let five_pad = widths.five_hour.saturating_sub(five_len);
    spans.extend(five_spans);
    spans.push(Span::raw(" ".repeat(five_pad)));
    if widths.seven_day > 0 {
        spans.push(gap(widths));
        let seven_spans = window_summary_spans_bracketed(
            profile.usage.as_ref().and_then(|u| u.weekly_window()),
            widths.seven_day,
            widths.seven_day >= 18,
        );
        let seven_len: usize = seven_spans.iter().map(|s| s.content.chars().count()).sum();
        let seven_pad = widths.seven_day.saturating_sub(seven_len);
        spans.extend(seven_spans);
        spans.push(Span::raw(" ".repeat(seven_pad)));
    }
    if widths.route > 0 {
        spans.push(gap(widths));
        let (chain, chain_style) = chain_summary(&cfg, profile);
        spans.push(Span::styled(fixed(&chain, widths.route), chain_style));
    }

    Line::from(spans)
}

fn gap(widths: &OverviewWidths) -> Span<'static> {
    Span::raw(" ".repeat(widths.gap))
}

/// 4 chars less than standard gap; min 1. Used between `type` and timer slot.
fn narrow_gap(widths: &OverviewWidths) -> Span<'static> {
    Span::raw(" ".repeat(widths.gap.saturating_sub(4).max(1)))
}

fn chain_summary(cfg: &AppConfig, profile: &Profile) -> (String, Style) {
    let Some(position) = cfg
        .state
        .fallback_chain
        .iter()
        .position(|n| n == &profile.name)
    else {
        return ("—".to_string(), theme::faint());
    };
    let threshold = threshold_for(profile);
    let pct = profile
        .usage
        .as_ref()
        .and_then(|u| u.five_hour.as_ref())
        .map(|w| w.utilization)
        .unwrap_or(0.0);
    let color = chain_state_style(Some(profile), pct, threshold);
    (format!("#{} @ {threshold:.0}%", position + 1), color)
}

fn draw_fallback_overview(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // Read-only detail pane — focus never descends here from the overview screen.
    let block = section_box("fallback chain", false, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = fallback_flow_lines(&app.config(), inner.width, inner.height);
    let para = Paragraph::new(lines)
        .style(theme::base())
        .wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

const GAUGE_W: usize = 12;

fn fallback_flow_lines(cfg: &AppConfig, _width: u16, height: u16) -> Vec<Line<'static>> {
    if cfg.state.fallback_chain.is_empty() {
        return vec![
            Line::from(Span::styled("no fallback chain yet", theme::dim())),
            Line::from(vec![
                Span::styled("fallback", theme::accent()),
                Span::styled(
                    " tab adds accounts that rotate automatically when a 5h window crosses its threshold.",
                    theme::dim(),
                ),
            ]),
        ];
    }

    let chain = &cfg.state.fallback_chain;
    let name_w = chain
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(8)
        .clamp(6, 18);
    let last = chain.len() - 1;
    let cap = height as usize;

    let mut lines = Vec::new();
    for (i, name) in chain.iter().enumerate() {
        if lines.len() >= cap {
            break;
        }
        lines.push(chain_row(cfg, name, i, last, name_w));
    }
    // Caption only if it fits; wrap-off replaces wrap caption.
    if lines.len() < cap {
        let caption = if cfg.state.wrap_off {
            vec![
                Span::raw("  "),
                Span::styled("[ ", theme::dim()),
                Span::styled("stop", theme::danger().add_modifier(Modifier::BOLD)),
                Span::styled(" ]", theme::dim()),
                Span::styled(" switches off when all spent", theme::faint()),
            ]
        } else {
            vec![
                Span::styled("     ↺", theme::orange()),
                Span::styled(" wraps to the top", theme::faint()),
            ]
        };
        lines.push(Line::from(caption));
    }
    lines
}

fn chain_row(
    cfg: &AppConfig,
    name: &str,
    index: usize,
    last: usize,
    name_w: usize,
) -> Line<'static> {
    let active = cfg.is_active(name);
    let rail = if index == 0 && last == 0 {
        "╶"
    } else if index == 0 {
        "╭"
    } else if index == last {
        "╰"
    } else {
        "│"
    };
    // Color carries active state — no glyph needed.
    let name_style = if active {
        Style::default().fg(theme::accent_2_color())
    } else {
        theme::dim()
    };
    let name_pad = name_w.saturating_sub(name.chars().count());

    let mut spans = vec![
        Span::styled(format!(" {rail} "), theme::faint()),
        Span::styled(format!("{} ", index + 1), theme::faint()),
        Span::styled(format!("{name}{}  ", " ".repeat(name_pad)), name_style),
    ];

    match cfg.find(name) {
        None => spans.push(Span::styled("missing", theme::danger())),
        Some(profile) => {
            let threshold = threshold_for(profile);
            let pct = profile
                .usage
                .as_ref()
                .and_then(|u| u.five_hour.as_ref())
                .map(|w| w.utilization);
            spans.extend(gauge_spans(pct, threshold));
            let (figure, figure_style) = match pct {
                Some(v) => (
                    format!("  {v:>3.0}"),
                    Style::default().fg(health_color(v, threshold)),
                ),
                None => ("    —".to_string(), theme::faint()),
            };
            spans.push(Span::styled(figure, figure_style));
            spans.push(Span::styled(format!(" / {threshold:.0}%"), theme::faint()));
        }
    }
    Line::from(spans)
}

/// `GAUGE_W`-cell bar relative to the member's threshold (full = rotate off).
fn gauge_spans(pct: Option<f64>, threshold: f64) -> Vec<Span<'static>> {
    let fill = pct
        .map(|v| {
            let frac = if threshold > 0.0 {
                (v / threshold).clamp(0.0, 1.0)
            } else {
                1.0
            };
            (frac * GAUGE_W as f64).round() as usize
        })
        .unwrap_or(0)
        .min(GAUGE_W);
    let fill_color = pct
        .map(|v| health_color(v, threshold))
        .unwrap_or(theme::text_faint_color());

    (0..GAUGE_W)
        .map(|i| {
            if i < fill {
                Span::styled("▰", Style::default().fg(fill_color))
            } else {
                Span::styled("▱", theme::faint())
            }
        })
        .collect()
}

fn chain_state_style(profile: Option<&Profile>, pct: f64, threshold: f64) -> Style {
    match profile {
        None => theme::danger(),
        Some(_) => Style::default().fg(health_color(pct, threshold)),
    }
}
