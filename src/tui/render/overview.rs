//! Overview screen: accounts list + fallback flow widget.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Padding, Paragraph, Wrap};

use super::super::app::{App, MainItemKind};
use super::super::theme;
use super::format::{
    account_type_label, account_type_style, fixed, fixed_split, name_style, window_summary_parts,
    window_summary_span,
};
use crate::fallback::threshold_for;
use crate::profile::{AppConfig, Profile};
use crate::usage::now_ms;

/// Width of the per-profile timer slot inserted before the 5h bar.
/// Format: `XXXs` (3 digits + 's') + 1 trailing space = 5 chars.
/// When fetching: `● ` padded to this width.
const TIMER_SLOT: usize = 5;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let target = if area.height >= 18 { 8 } else { 5 };
    let cap = area.height.saturating_sub(6).max(3);
    let chain_height = target.min(cap);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(7), Constraint::Length(chain_height)])
        .split(area);

    draw_overview_accounts(frame, chunks[0], app);
    draw_fallback_overview(frame, chunks[1], app);
}

fn draw_overview_accounts(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = Line::from(vec![
        Span::styled(" ACCOUNTS ", theme::label()),
        Span::styled(
            format!("{} total", app.config().profiles.len()),
            theme::faint(),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::LINE))
        .title(title)
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.config().profiles.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled("no accounts yet", theme::muted())))
            .style(theme::base());
        frame.render_widget(empty, inner);
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
    let rows: Vec<ListItem<'_>> = items
        .iter()
        .enumerate()
        .map(|(row, item)| match item {
            MainItemKind::Profile(idx) => ListItem::new(render_overview_row(
                app,
                *idx,
                &widths,
                row == app.main_cursor,
            )),
            MainItemKind::ActionSeparator => ListItem::new(render_separator_row()),
            MainItemKind::NewProfile => {
                ListItem::new(render_action_row("+ new profile", row == app.main_cursor))
            }
            MainItemKind::CaptureCredentials => ListItem::new(render_action_row(
                "+ new from current profile",
                row == app.main_cursor,
            )),
            MainItemKind::OpenChain => ListItem::new(render_action_row(
                "→ fallback chain",
                row == app.main_cursor,
            )),
        })
        .collect();

    let list = List::new(rows)
        .style(theme::base())
        .highlight_style(theme::selected_row().add_modifier(Modifier::BOLD));
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(app.main_cursor.min(items.len().saturating_sub(1))));
    frame.render_stateful_widget(list, chunks[1], &mut state);
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
        // 26 = "[bar 10] XX% (1h 32m)" fits.  17 = "[bar 10] XX%" without reset.
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

        // Spread leftover width across the inter-column gaps so columns
        // breathe on wide terminals and stay tight on narrow ones.
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
    // 4 = cursor + dot + spacer before name; +2 = spacer + auto-start marker after 5h.
    // Timer slot is rendered in the gap before 5h and does not count as a column.
    // The kind→timer gap is 4 chars narrower than the standard gap (min 1).
    let narrow = gap.saturating_sub(4).max(1);
    let standard_gaps = column_count.saturating_sub(2);
    6 + name + kind + five_hour + seven_day + route + standard_gaps * gap + narrow
}

fn overview_header(widths: &OverviewWidths) -> Line<'static> {
    let mut spans = vec![Span::styled("    ", theme::label())];
    spans.push(Span::styled(fixed("account", widths.name), theme::label()));
    spans.push(gap(widths));
    spans.push(Span::styled(fixed("type", widths.kind), theme::label()));
    spans.push(narrow_gap(widths));
    // The timer slot sits before the 5h bar; keep the header label left-aligned
    // over the bar by rendering blanks for the slot width.
    spans.push(Span::raw(" ".repeat(TIMER_SLOT)));
    spans.push(Span::styled(fixed("5h", widths.five_hour), theme::label()));
    spans.push(Span::raw("  "));
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
) -> Line<'static> {
    let cfg = app.config();
    let Some(profile) = cfg.profiles.get(idx) else {
        return Line::from("");
    };

    let active = cfg.is_active(&profile.name);
    let name_str = profile.name.clone();
    let cursor = if selected {
        Span::styled("▸ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    let dot = if active {
        Span::styled("◆", theme::orange())
    } else {
        Span::styled("◇", theme::faint())
    };
    let (name_text, name_pad) = fixed_split(&profile.name, widths.name);
    let name = Span::styled(
        name_text,
        if active {
            name_style(profile).fg(theme::ACCENT_2)
        } else {
            name_style(profile)
        },
    );
    let name_pad = Span::raw(name_pad);

    // Auto-start indicator sits after the 5h column so it reads alongside the
    // usage that triggers the rotation, with a fixed 1-char slot kept in all
    // rows so subsequent columns stay aligned.
    let auto_marker = if profile.auto_start && profile.is_oauth() {
        Span::styled("↻", theme::accent())
    } else {
        Span::raw(" ")
    };

    // Per-profile timer slot: busy pip while fetching, seconds countdown otherwise.
    // Right-aligned in TIMER_SLOT-1 chars + 1 trailing space so the bar [ always
    // has visible breathing room and the dot doesn't crowd the kind column.
    let timer_span = {
        let inner = TIMER_SLOT - 1;
        let is_fetching = app
            .fetching_now
            .lock()
            .ok()
            .is_some_and(|s| s.contains(&name_str));
        if is_fetching {
            Span::styled(format!("{:>inner$} ", "●", inner = inner), theme::accent())
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

    let mut spans = vec![cursor, dot, Span::raw(" "), name, name_pad, gap(widths)];
    spans.push(Span::styled(
        fixed(&account_type_label(profile), widths.kind),
        account_type_style(profile),
    ));
    spans.push(narrow_gap(widths));
    spans.push(timer_span);
    let (five_text, five_style) = window_summary_parts(
        profile.usage.as_ref().and_then(|u| u.five_hour.as_ref()),
        widths.five_hour,
        true,
    );
    let five_pad = widths.five_hour.saturating_sub(five_text.chars().count());
    spans.push(Span::styled(five_text, five_style));
    spans.push(Span::raw(" "));
    spans.push(auto_marker);
    spans.push(Span::raw(" ".repeat(five_pad)));
    if widths.seven_day > 0 {
        spans.push(gap(widths));
        spans.push(window_summary_span(
            profile.usage.as_ref().and_then(|u| u.weekly_window()),
            widths.seven_day,
            widths.seven_day >= 18,
        ));
    }
    if widths.route > 0 {
        spans.push(gap(widths));
        let (chain, chain_style) = chain_summary(&cfg, profile);
        spans.push(Span::styled(fixed(&chain, widths.route), chain_style));
    }

    Line::from(spans)
}

fn render_action_row(label: &'static str, selected: bool) -> Line<'static> {
    let cursor = if selected {
        Span::styled("▸ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    Line::from(vec![cursor, Span::styled(label, theme::dim())])
}

/// Section break above the action rows. Cursor skips this line.
fn render_separator_row() -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled("ACTIONS", theme::label()),
    ])
}

fn gap(widths: &OverviewWidths) -> Span<'static> {
    Span::raw(" ".repeat(widths.gap))
}

/// Narrower gap used only between the `type` column and the timer slot.
/// 4 chars less than the standard gap; never drops below 1.
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
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::LINE))
        .title(Line::from(vec![
            Span::styled(" FALLBACK ", Style::default().fg(theme::ACCENT_2).bold()),
            Span::styled("flow", theme::faint()),
        ]))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = fallback_flow_lines(&app.config(), inner.width, inner.height);
    let para = Paragraph::new(lines)
        .style(theme::base())
        .wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

fn fallback_flow_lines(cfg: &AppConfig, width: u16, height: u16) -> Vec<Line<'static>> {
    if cfg.state.fallback_chain.is_empty() {
        return vec![
            Line::from(Span::styled(
                "No fallback chain configured.",
                theme::muted(),
            )),
            Line::from(vec![
                Span::styled("f", theme::accent()),
                Span::styled(
                    " opens the chain editor. Add accounts to rotate automatically when a 5h window crosses its threshold.",
                    theme::dim(),
                ),
            ]),
        ];
    }

    if width >= 92 {
        let mut spans = Vec::new();
        for (i, name) in cfg.state.fallback_chain.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" ─▶ ", theme::orange()));
            }
            let (label, style) = chain_node(cfg, name, i);
            spans.push(Span::styled(
                format!(" {label} "),
                style.bg(theme::BG_RAISED).bold(),
            ));
        }
        spans.push(Span::styled("  ↺", theme::orange()));
        return vec![Line::from(spans)];
    }

    let mut lines = Vec::new();
    for (i, name) in cfg.state.fallback_chain.iter().enumerate() {
        let (label, style) = chain_node(cfg, name, i);
        lines.push(Line::from(Span::styled(label.to_string(), style.bold())));
        if i + 1 < cfg.state.fallback_chain.len() && lines.len() + 1 < height as usize {
            lines.push(Line::from(Span::styled("  ↓", theme::faint())));
        }
        if lines.len() >= height as usize {
            break;
        }
    }
    if lines.len() < height as usize {
        lines.push(Line::from(Span::styled("  ↺ wraps to top", theme::faint())));
    }
    lines
}

fn chain_node(cfg: &AppConfig, name: &str, index: usize) -> (String, Style) {
    let active = cfg.is_active(name);
    let marker = if active { "◆" } else { "◇" };
    let Some(profile) = cfg.find(name) else {
        return (
            format!("{} {marker} {name} · missing", index + 1),
            theme::danger(),
        );
    };
    let threshold = threshold_for(profile);
    let pct = profile
        .usage
        .as_ref()
        .and_then(|u| u.five_hour.as_ref())
        .map(|w| w.utilization);
    let status = pct
        .map(|p| format!("{p:.0}/{threshold:.0}%"))
        .unwrap_or_else(|| format!("—/{threshold:.0}%"));
    let style = chain_state_style(
        profile
            .usage
            .as_ref()
            .and_then(|u| u.five_hour.as_ref())
            .map(|_| profile),
        pct.unwrap_or(0.0),
        threshold,
    );
    (
        format!("{} {marker} {} · {status}", index + 1, profile.name),
        style,
    )
}

fn chain_state_style(profile: Option<&Profile>, pct: f64, threshold: f64) -> Style {
    if profile.is_none() {
        return theme::danger();
    }
    if pct >= threshold {
        theme::danger()
    } else if pct >= threshold * 0.8 {
        theme::warning()
    } else {
        Style::default().fg(theme::ACCENT_2)
    }
}
