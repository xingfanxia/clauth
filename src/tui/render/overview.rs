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
use super::panes::{bold_when, draw_scrollbar, empty_state, section_box, select_line};
use crate::fallback::{SwitchAction, next_target, threshold_for};
use crate::profile::{AppConfig, Profile};
use crate::usage::{
    LABEL_5H, LABEL_7D, ProfileActivity, UsageInfo, UsageWindow, humanize_duration, now_ms,
};

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
    4 + name + kind + five_hour + seven_day + route + standard_gaps * gap + narrow
}

fn overview_header(widths: &OverviewWidths) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", theme::label())];
    spans.push(Span::raw("  ")); // bell slot (blank in header)
    spans.push(Span::styled(fixed("account", widths.name), theme::label()));
    spans.push(gap(widths));
    spans.push(Span::styled(fixed("type", widths.kind), theme::label()));
    spans.push(narrow_gap(widths));
    // Blank TIMER_SLOT keeps the label aligned over the bar.
    spans.push(Span::raw(" ".repeat(TIMER_SLOT)));
    spans.push(Span::styled(
        fixed(LABEL_5H, widths.five_hour),
        theme::label(),
    ));
    if widths.seven_day > 0 {
        spans.push(gap(widths));
        spans.push(Span::styled(
            fixed(LABEL_7D, widths.seven_day),
            theme::label(),
        ));
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
    let cursor = if selected && focused {
        Span::styled("❯ ", theme::accent().add_modifier(Modifier::BOLD))
    } else {
        Span::raw("  ")
    };
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

    let mut spans = vec![cursor];
    if app.bell_fired.contains_key(&name_str) {
        spans.push(Span::styled("🔔", theme::accent()));
    } else if active {
        spans.push(Span::styled("●", theme::accent_2_color()));
        spans.push(Span::raw(" "));
    } else {
        spans.push(Span::raw("  "));
    }
    let (nt, np) = fixed_split(&profile.name, widths.name);
    let ns = bold_when(
        if active {
            name_style(profile).fg(theme::accent_2_color())
        } else {
            name_style(profile)
        },
        selected && focused,
    );
    spans.push(Span::styled(nt, ns));
    spans.push(Span::raw(np));
    spans.push(gap(widths));
    spans.push(Span::styled(
        fixed(&account_type_label(profile), widths.kind),
        account_type_style(profile),
    ));
    spans.push(narrow_gap(widths));
    spans.push(timer_span);
    // Bracketed bars ([███░░░]) for overview account rows only.
    // Usage-page gauges, chain bars, and fallback thresholds stay bracket-less.
    // OAuth windows come from `usage`; api-key/provider profiles have no `usage`,
    // so the 5h/7d windows are synthesized from the matching third-party bars.
    let (five_window, seven_window) = overview_windows(profile);
    let five_spans = window_summary_spans_bracketed(five_window.as_ref(), widths.five_hour, true);
    let five_len: usize = five_spans.iter().map(|s| s.content.chars().count()).sum();
    let five_pad = widths.five_hour.saturating_sub(five_len);
    spans.extend(five_spans);
    spans.push(Span::raw(" ".repeat(five_pad)));
    if widths.seven_day > 0 {
        spans.push(gap(widths));
        let seven_spans = window_summary_spans_bracketed(
            seven_window.as_ref(),
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

/// The `(5h, 7d)` windows to show in the overview row. OAuth profiles use their
/// live `UsageInfo`; api-key/provider profiles have no `UsageInfo`, so each slot
/// is synthesized from the third-party bar whose label matches (`5h` / `7d`) —
/// the same labels `zai` decodes from its window codes. `None` per slot when no
/// source exists (renders `—`).
fn overview_windows(profile: &Profile) -> (Option<UsageWindow>, Option<UsageWindow>) {
    if let Some(usage) = profile.usage.as_ref() {
        return (usage.five_hour.clone(), usage.weekly_window().cloned());
    }
    let Some(bars) = profile.third_party_usage.as_ref().map(|s| &s.bars) else {
        return (None, None);
    };
    let window_for = |label: &str| {
        bars.iter().find(|b| b.label == label).map(|b| UsageWindow {
            utilization: b.pct,
            resets_at: b.resets_at.clone(),
        })
    };
    (window_for(LABEL_5H), window_for(LABEL_7D))
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

    let lines = fallback_flow_lines(app, inner.width, inner.height);
    let para = Paragraph::new(lines)
        .style(theme::base())
        .wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

const GAUGE_W: usize = 12;

fn fallback_flow_lines(app: &App, _width: u16, height: u16) -> Vec<Line<'static>> {
    let cfg = app.config();
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
        lines.push(chain_row(&cfg, name, i, last, name_w));
    }

    // Caption only if it fits; wrap-off replaces wrap caption.
    if lines.len() < cap {
        let caption = if cfg.state.wrap_off {
            vec![
                Span::raw("  "),
                Span::styled("[ ", theme::dim()),
                Span::styled("stop", theme::danger().add_modifier(Modifier::BOLD)),
                Span::styled(" ]", theme::dim()),
                Span::styled(" when all spent", theme::faint()),
            ]
        } else {
            vec![
                Span::raw("  "),
                Span::styled("[ ", theme::dim()),
                Span::styled("stay", theme::dim().add_modifier(Modifier::BOLD)),
                Span::styled(" ]", theme::dim()),
                Span::styled(" on last when all spent", theme::faint()),
            ]
        };
        lines.push(Line::from(caption));
    }

    if lines.len() < cap
        && chain.len() > 1
        && let Some(active_name) = cfg.state.active_profile.as_deref()
        && let Some(profile) = cfg.find(active_name)
        && let Some(usage_info) = profile.usage.as_ref()
        && let Some(usage) = usage_info.five_hour.as_ref()
    {
        let threshold = threshold_for(profile);
        let eta_secs = burn_rate_eta(app, active_name, usage_info, usage.utilization, threshold);
        let reset_secs = super::format::reset_in_secs(usage);
        // Only project a switch when the account crosses its threshold BEFORE the
        // 5h window resets — past the reset the window refills and no switch fires.
        if let Some(secs) = eta_secs
            && reset_secs.is_none_or(|reset| secs < reset)
        {
            match next_target(&cfg) {
                Some(SwitchAction::To(target)) => {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            format!("switching to {target} in ~{}", humanize_duration(secs)),
                            theme::faint(),
                        ),
                    ]));
                }
                Some(SwitchAction::Off) => {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            format!("stops all in ~{}", humanize_duration(secs)),
                            theme::faint(),
                        ),
                    ]));
                }
                None => {}
            }
        }
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
                    Style::default().fg(theme::util_color(v)),
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
        .map(theme::util_color)
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

/// Project seconds until a profile's utilization crosses the threshold, based on
/// 5h window burn rate. Returns `None` when there aren't enough samples, the
/// rate is flat/negative, or utilization is already at/above the threshold.
fn burn_rate_eta(
    app: &App,
    name: &str,
    current_usage: &UsageInfo,
    current: f64,
    threshold: f64,
) -> Option<i64> {
    if current >= threshold {
        return None;
    }
    let five_h = current_usage.five_hour.as_ref().map(|w| ("5h", w));
    let rate = five_h.and_then(|pair| {
        let mut rates = crate::usage::compute_burn_rates_from_history(
            app.history_cache
                .get(name)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
            std::slice::from_ref(&pair),
            60 * 60 * 1000, // lookback_ms: last 1h of samples, aligned with %/h
            3,              // min_samples before a rate is shown
            10 * 60 * 1000, // gap_cut_ms: cut idle gaps for ETA projection
        );
        rates.remove("5h").flatten()
    })?;
    if rate <= 0.0 {
        return None;
    }
    let hours = (threshold - current) / rate;
    if hours <= 0.0 {
        return None;
    }
    Some((hours * 3600.0) as i64)
}
