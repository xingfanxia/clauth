//! Overview tab: accounts table + fallback flow, inside one content frame.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use super::super::app::{App, MainItemKind};
use super::super::theme;
use super::format::{
    account_type_label, account_type_style, fixed, fixed_split, name_style, window_summary_parts,
    window_summary_span,
};
use super::panes::section_box;
use crate::fallback::threshold_for;
use crate::profile::{AppConfig, Profile};
use crate::usage::{ProfileActivity, now_ms};

/// Braille spinner frames — same set most CLI tools use. Cycled by
/// `app.tick_count` so every render frame advances one step.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Width of the per-profile timer slot inserted before the 5h bar.
/// Format: `XXXs` (3 digits + 's') + 1 trailing space = 5 chars.
/// When fetching: `● ` padded to this width.
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
    let block = section_box("accounts", false);
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
    // 2 = cursor (`❯ ` / two blanks) before name. Timer slot is rendered in the
    // gap before 5h and does not count as a column. The kind→timer gap is 4 chars
    // narrower than the standard gap (min 1).
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
    // The timer slot sits before the 5h bar; keep the header label left-aligned
    // over the bar by rendering blanks for the slot width.
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
) -> Line<'static> {
    let cfg = app.config();
    let Some(profile) = cfg.profiles.get(idx) else {
        return Line::from("");
    };

    let active = cfg.is_active(&profile.name);
    let name_str = profile.name.clone();
    let cursor = if selected {
        Span::styled("❯ ", theme::accent())
    } else {
        Span::raw("  ")
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

    // Per-profile timer slot: braille spinner during any in-flight activity,
    // seconds countdown when Idle. Right-aligned in TIMER_SLOT-1 chars + 1
    // trailing space so the bar [ always has visible breathing room.
    let timer_span = {
        let inner = TIMER_SLOT - 1;
        let activity = app
            .activity
            .lock()
            .ok()
            .and_then(|g| g.get(&name_str).copied())
            .unwrap_or(ProfileActivity::Idle);
        if !matches!(activity, ProfileActivity::Idle) {
            let frame = SPINNER_FRAMES[(app.tick_count as usize) % SPINNER_FRAMES.len()];
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
    let (five_text, five_style) = window_summary_parts(
        profile.usage.as_ref().and_then(|u| u.five_hour.as_ref()),
        widths.five_hour,
        true,
    );
    let five_pad = widths.five_hour.saturating_sub(five_text.chars().count());
    spans.push(Span::styled(five_text, five_style));
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

/// Maps an in-flight activity to the spinner color. Idle is unreachable here;
/// the caller falls back to the seconds countdown for it.
fn spinner_style(activity: ProfileActivity) -> Style {
    match activity {
        // Sapphire primary — the routine "I'm pulling fresh numbers" state.
        ProfileActivity::Fetching => theme::accent(),
        // Info cyan — distinct from accent so a refresh inside a fetch reads
        // visibly different from a plain fetch.
        ProfileActivity::Refreshing => theme::info(),
        // Claude orange — secondary accent; switching is the rare,
        // user-visible state and earns the warm slot.
        ProfileActivity::Switching => theme::orange(),
        // Warning yellow — a launching session is a transient mid-state.
        ProfileActivity::Starting => theme::warning(),
        // Catppuccin green — a successful auto-start arms the 5h window.
        ProfileActivity::AutoStarting => theme::success(),
        ProfileActivity::Idle => theme::faint(),
    }
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
    let block = section_box("fallback chain", false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = fallback_flow_lines(&app.config(), inner.width, inner.height);
    let para = Paragraph::new(lines)
        .style(theme::base())
        .wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
}

/// Cells in a member's 5h gauge. Enough resolution to read headroom at a
/// glance, narrow enough to stay quiet.
const GAUGE_W: usize = 12;

fn fallback_flow_lines(cfg: &AppConfig, _width: u16, height: u16) -> Vec<Line<'static>> {
    if cfg.state.fallback_chain.is_empty() {
        return vec![
            Line::from(Span::styled("no fallback chain yet", theme::muted())),
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
    // Caption only if it won't push a member off-screen: explains the tick + loop.
    if lines.len() < cap {
        lines.push(Line::from(vec![
            Span::styled("     ↺", theme::orange()),
            Span::styled(" wraps to the top", theme::faint()),
        ]));
    }
    lines
}

/// One chain member on its own line: a faint ordering rail, a padded name, then
/// a slim 5h gauge with a threshold tick and the figure. Color carries the
/// active state (orange name) and lands on the gauge fill + the percentage;
/// everything else stays quiet so the row reads at a glance without shouting.
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
    // No active glyph — color carries the active state (orange name vs dim).
    let name_style = if active {
        Style::default().fg(theme::ACCENT_2)
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
                    Style::default().fg(chain_health_color(v, threshold)),
                ),
                None => ("    —".to_string(), theme::faint()),
            };
            spans.push(Span::styled(figure, figure_style));
            spans.push(Span::styled(format!(" / {threshold:.0}%"), theme::faint()));
        }
    }
    Line::from(spans)
}

/// A `GAUGE_W`-cell bar whose fill tracks usage *relative to the member's own
/// threshold*: a full bar means it has reached the threshold and is about to
/// rotate off. Fill is colored by headroom; the empty track stays faint.
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
        .map(|v| chain_health_color(v, threshold))
        .unwrap_or(theme::TEXT_FAINT);

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

/// 5h headroom against a member's own threshold: green with room, yellow as it
/// nears, pink once it crosses — the point at which clauth rotates off it.
fn chain_health_color(pct: f64, threshold: f64) -> ratatui::style::Color {
    if pct >= threshold {
        theme::DANGER
    } else if pct >= threshold * 0.8 {
        theme::WARNING
    } else {
        theme::SUCCESS
    }
}

/// Style for the Overview table's `route` column. Mirrors the pill health map,
/// red for a missing profile.
fn chain_state_style(profile: Option<&Profile>, pct: f64, threshold: f64) -> Style {
    match profile {
        None => theme::danger(),
        Some(_) => Style::default().fg(chain_health_color(pct, threshold)),
    }
}
