//! Top bar: claude glyph on the left; brand, account count, and the tab bar
//! stacked in the text column to the right. Three rows, no dead space.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::App;
use super::super::theme;
use super::format::{bar_string_with_cells, fixed_split, name_style};
use super::tabs;

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Account gauge (issue #16) ────────────────────────────────────────────────
//
// Bare (bracket-less) mini usage bar for the active profile's 5h window,
// row 1, left of the feed dot. Collapse ladder, in sacrifice order:
//   0. row gap FULL(2) -> MIN(1)                     [`resolve_gauge_row`]
//   1. account-count text drops entirely              [`resolve_gauge_row`]
//   2. profile name truncates                         [`gauge_fit`]
//   3. bar cells shrink                                [`gauge_fit`]
//   4. bar drops entirely                              [`gauge_fit`]
//   5. name drops entirely                             [`gauge_fit`]
//   6. whole gauge drops                               [`gauge_fit`]
// The feed dot keeps its pre-existing, independent ">= 3-cell gap" rule.

const GAUGE_GAP_FULL: usize = 2;
const GAUGE_GAP_MIN: usize = 1;
const GAUGE_BAR_FULL: usize = 8;
const GAUGE_BAR_MIN: usize = 3;
const GAUGE_NAME_MAX: usize = 16;
const GAUGE_NAME_MIN: usize = 3;
const GAUGE_PCT_W: usize = 4; // "100%" / " 42%"
const GAUGE_DASH_W: usize = 1; // "—" — api-key/provider profiles have no 5h OAuth window

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GaugeFit {
    name_w: usize,
    bar_cells: usize,
    gap: usize,
    visible: bool,
}

impl GaugeFit {
    const HIDDEN: GaugeFit = GaugeFit {
        name_w: 0,
        bar_cells: 0,
        gap: 0,
        visible: false,
    };
}

/// Active profile's gauge content: full (untruncated) name, 5h utilization
/// (`None` for api-key/provider profiles — no OAuth window, renders `—`),
/// and the name's stale-cache style (same convention as the overview list).
struct ActiveGauge {
    name: String,
    pct: Option<f64>,
    style: Style,
}

fn active_gauge(app: &App) -> Option<ActiveGauge> {
    let cfg = app.config();
    let name = cfg.state.active_profile.as_deref()?;
    let profile = cfg.find(name)?;
    let pct = if profile.is_oauth() {
        profile
            .usage
            .as_ref()
            .and_then(|u| u.five_hour.as_ref())
            // Clamped at this boundary like every other utilization display:
            // the ladder reserves exactly 4 tail cells ("100%"), so an
            // out-of-range wire value must never widen the printed number.
            .map(|w| w.utilization.clamp(0.0, 100.0))
    } else {
        None
    };
    Some(ActiveGauge {
        name: name.to_string(),
        pct,
        style: name_style(profile),
    })
}

fn gauge_tail_w(has_pct: bool) -> usize {
    if has_pct { GAUGE_PCT_W } else { GAUGE_DASH_W }
}

fn gauge_total_w(name_w: usize, bar_cells: usize, gap: usize, tail_w: usize) -> usize {
    let mut w = tail_w;
    if bar_cells > 0 {
        w += bar_cells + gap;
    }
    if name_w > 0 {
        w += name_w + gap;
    }
    w
}

/// Steps 2-6 of the ladder: given the cells available to the gauge alone
/// (`avail`), the untruncated name length, whether it has a percent (bar)
/// tail at all, and the already-decided inter-piece `gap`, degrade name →
/// bar → drop-bar → drop-name → hide entirely until it fits.
fn gauge_fit(avail: usize, name_len: usize, has_pct: bool, gap: usize) -> GaugeFit {
    let tail_w = gauge_tail_w(has_pct);
    if avail < tail_w {
        return GaugeFit::HIDDEN;
    }
    let mut name_w = name_len.min(GAUGE_NAME_MAX);
    let mut bar_cells = if has_pct { GAUGE_BAR_FULL } else { 0 };
    let name_floor = GAUGE_NAME_MIN.min(name_len);

    while gauge_total_w(name_w, bar_cells, gap, tail_w) > avail && name_w > name_floor {
        name_w -= 1;
    }
    while gauge_total_w(name_w, bar_cells, gap, tail_w) > avail && bar_cells > GAUGE_BAR_MIN {
        bar_cells -= 1;
    }
    if bar_cells > 0 && gauge_total_w(name_w, bar_cells, gap, tail_w) > avail {
        bar_cells = 0;
    }
    if name_w > 0 && gauge_total_w(name_w, bar_cells, gap, tail_w) > avail {
        name_w = 0;
    }
    if gauge_total_w(name_w, bar_cells, gap, tail_w) > avail {
        return GaugeFit::HIDDEN;
    }
    GaugeFit {
        name_w,
        bar_cells,
        gap,
        visible: true,
    }
}

/// Steps 0-1 of the ladder: shrink the row gap (FULL -> MIN) before dropping
/// the account-count text, before ever asking the gauge itself to sacrifice
/// anything (`gauge_fit`). Returns whether the count text survives and the
/// gauge's resolved fit.
fn resolve_gauge_row(
    info_width: usize,
    count_w: usize,
    gauge: &Option<ActiveGauge>,
) -> (bool, GaugeFit) {
    let Some(g) = gauge else {
        return (true, GaugeFit::HIDDEN);
    };
    let name_len = g.name.chars().count();
    let has_pct = g.pct.is_some();
    let bar_full = if has_pct { GAUGE_BAR_FULL } else { 0 };
    let name_full = name_len.min(GAUGE_NAME_MAX);
    let tail_w = gauge_tail_w(has_pct);
    let full_w = |gap: usize| gauge_total_w(name_full, bar_full, gap, tail_w);
    let full_fit = |gap: usize| GaugeFit {
        name_w: name_full,
        bar_cells: bar_full,
        gap,
        visible: true,
    };

    for gap in [GAUGE_GAP_FULL, GAUGE_GAP_MIN] {
        if count_w + gap + full_w(gap) <= info_width {
            return (true, full_fit(gap));
        }
    }

    // Account count gives way before the gauge sacrifices anything of its own.
    let gap = GAUGE_GAP_MIN;
    if full_w(gap) <= info_width {
        return (false, full_fit(gap));
    }

    (false, gauge_fit(info_width, name_len, has_pct, gap))
}

fn gauge_spans(fit: GaugeFit, name: &str, style: Style, pct: Option<f64>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if fit.name_w > 0 {
        let (nt, _pad) = fixed_split(name, fit.name_w);
        spans.push(Span::styled(nt, style));
        spans.push(Span::raw(" ".repeat(fit.gap)));
    }
    match pct {
        Some(pct) if fit.bar_cells > 0 => {
            let style = Style::default().fg(theme::util_color(pct));
            spans.push(Span::styled(
                bar_string_with_cells(pct, fit.bar_cells),
                style,
            ));
            spans.push(Span::raw(" ".repeat(fit.gap)));
            spans.push(Span::styled(format!("{pct:>3.0}%"), style));
        }
        Some(pct) => {
            spans.push(Span::styled(
                format!("{pct:>3.0}%"),
                Style::default().fg(theme::util_color(pct)),
            ));
        }
        None => spans.push(Span::styled("—", theme::faint())),
    }
    spans
}

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(10), Constraint::Min(20)])
        .split(area);

    draw_logo(frame, cols[0], app);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(cols[1]);

    let n = app.config().profiles.len();

    // Row 0: brand (TEXT + bold; house deviation from ACCENT_2) left, version (TEXT_DIM) right.
    let info_width = rows[0].width as usize;
    let brand = "clauth";
    let ver = format!("v{VERSION}");
    let gap = info_width.saturating_sub(brand.len() + ver.len());
    let title = Line::from(vec![
        Span::styled(brand, Style::default().fg(theme::text_color()).bold()),
        Span::styled(" ".repeat(gap), theme::base()),
        Span::styled(ver, theme::dim()),
    ]);

    // Row 1: count left; then (space permitting) the active profile's 5h
    // gauge (issue #16); `● status.claude.ai` right-aligned. The gauge
    // claims space before the account count (`resolve_gauge_row`); the feed
    // dot keeps its original, independent "only if a >= 3-cell gap fits"
    // rule, now measured against whatever actually rendered before it.
    // Suppressed outright in compact mode.
    let count_txt = format!("{n} account{}", plural(n));
    let count_w = count_txt.chars().count();
    let feed = "status.claude.ai"; // display label per user choice; feed itself is status.claude.com
    let ind_width = 2 + feed.len(); // `● ` + label

    let gauge = if app.compact { None } else { active_gauge(app) };
    let (show_count, fit) = resolve_gauge_row(info_width, count_w, &gauge);

    let mut row1_spans: Vec<Span<'static>> = Vec::new();
    let mut prefix_w = 0usize;
    if show_count {
        row1_spans.push(Span::styled(count_txt, theme::faint()));
        prefix_w += count_w;
    }
    if let Some(g) = &gauge
        && fit.visible
    {
        if prefix_w > 0 {
            row1_spans.push(Span::raw(" ".repeat(fit.gap)));
            prefix_w += fit.gap;
        }
        let gspans = gauge_spans(fit, &g.name, g.style, g.pct);
        let gw: usize = gspans.iter().map(|s| s.content.chars().count()).sum();
        row1_spans.extend(gspans);
        prefix_w += gw;
    }
    if info_width >= prefix_w + ind_width + 3 {
        let dot_gap = info_width - prefix_w - ind_width;
        row1_spans.push(Span::raw(" ".repeat(dot_gap)));
        row1_spans.push(Span::styled(
            "●",
            Style::default().fg(status_dot_color(app)),
        ));
        row1_spans.push(Span::styled(format!(" {feed}"), theme::dim()));
    }

    frame.render_widget(Paragraph::new(title).style(theme::base()), rows[0]);
    frame.render_widget(
        Paragraph::new(Line::from(row1_spans)).style(theme::base()),
        rows[1],
    );
    tabs::draw(frame, rows[2], app);
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Feed-health color for the header status dot — when active incidents exist,
/// uses the worst impact's semantic color (critical/major → DANGER, minor →
/// WARNING, maintenance → TEXT_DIM). Stale cache or no data → SUCCESS.
fn status_dot_color(app: &App) -> ratatui::style::Color {
    use crate::status::Impact;
    match app.status.worst_active_impact() {
        Impact::Critical | Impact::Major => theme::danger_color(),
        Impact::Minor => theme::warning_color(),
        Impact::Maintenance => theme::text_dim_color(),
        Impact::None | Impact::Other(_) => theme::success_color(),
    }
}

/// Claude glyph in the top-left. Eyes blank for ~200ms every ~6s as a subtle sign of life.
fn draw_logo(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let elapsed = app.started_at.elapsed().as_millis() as u64;
    let blink = (elapsed % 6000) < 200;

    let style = Style::default().fg(theme::accent_2_color());

    let logo_top = if blink {
        " ▐█████▌ "
    } else {
        " ▐▛███▜▌ "
    };
    let logo_mid = "▝▜█████▛▘";
    let logo_eyes = "  ▘▘ ▝▝  ";

    let lines = vec![
        Line::from(Span::styled(logo_top, style)).alignment(Alignment::Left),
        Line::from(Span::styled(logo_mid, style)).alignment(Alignment::Left),
        Line::from(Span::styled(logo_eyes, style)).alignment(Alignment::Left),
    ];

    let para = Paragraph::new(lines).style(theme::base());
    frame.render_widget(para, area);
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_header.rs"]
mod gauge_tests;
