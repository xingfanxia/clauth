//! Top bar: claude glyph on the left; brand, account count, active-account
//! gauge, and the tab bar stacked in the text column to the right. Four rows
//! with an active account (`brand / count + feed dot / gauge / tabs`), three
//! without one or in compact mode — [`header_height`] keeps `render::draw`'s
//! layout in step so no dead row is ever reserved.

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
// `name  [███░░░░░] 38%` for the active profile's 5h window, on its own row
// right-aligned under the feed dot (brackets dim, house deviation shared with
// the overview bars). Collapse ladder, in sacrifice order:
//   1. profile name truncates                         [`gauge_fit`]
//   2. bar cells shrink                                [`gauge_fit`]
//   3. bar drops entirely                              [`gauge_fit`]
//   4. name drops entirely                             [`gauge_fit`]
//   5. whole gauge drops                               [`gauge_fit`]

const GAUGE_NAME_GAP: usize = 2;
const GAUGE_BAR_FULL: usize = 8;
const GAUGE_BAR_MIN: usize = 3;
const GAUGE_NAME_MAX: usize = 16;
const GAUGE_NAME_MIN: usize = 3;
const GAUGE_PCT_W: usize = 4; // "100%" / " 42%"
const GAUGE_DASH_W: usize = 1; // "—" — api-key/provider profiles have no 5h OAuth window

// ── Account-name pulse ───────────────────────────────────────────────────────
//
// cloudy-tui attention-shimmer, periodic-pulse mode: a pale-orange crest sweeps
// the name left→right, then rests flat — tint, never saturate. Full tier only
// (the per-cell blend needs truecolor). Feel lives in these constants.

const PULSE_SWEEP_MS: u64 = 900;
const PULSE_REST_MS: u64 = 1700;
/// Cap on how far a char leans toward the crest color (0 = off, 1 = full flip).
const PULSE_DEPTH: f32 = 0.45;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GaugeFit {
    name_w: usize,
    bar_cells: usize,
    visible: bool,
}

impl GaugeFit {
    const HIDDEN: GaugeFit = GaugeFit {
        name_w: 0,
        bar_cells: 0,
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

/// Cells the gauge occupies: `name  [bar] pct` — the bar carries its 2 bracket
/// cells and a 1-cell gap before the percent; the name a 2-cell gap after it.
fn gauge_total_w(name_w: usize, bar_cells: usize, has_pct: bool) -> usize {
    let mut w = gauge_tail_w(has_pct);
    if bar_cells > 0 {
        w += bar_cells + 2 + 1;
    }
    if name_w > 0 {
        w += name_w + GAUGE_NAME_GAP;
    }
    w
}

/// The collapse ladder: given the cells available to the gauge (`avail`), the
/// untruncated name length, and whether it has a percent (bar) tail at all,
/// degrade name → bar → drop-bar → drop-name → hide entirely until it fits.
fn gauge_fit(avail: usize, name_len: usize, has_pct: bool) -> GaugeFit {
    if avail < gauge_tail_w(has_pct) {
        return GaugeFit::HIDDEN;
    }
    let mut name_w = name_len.min(GAUGE_NAME_MAX);
    let mut bar_cells = if has_pct { GAUGE_BAR_FULL } else { 0 };
    let name_floor = GAUGE_NAME_MIN.min(name_len);

    while gauge_total_w(name_w, bar_cells, has_pct) > avail && name_w > name_floor {
        name_w -= 1;
    }
    while gauge_total_w(name_w, bar_cells, has_pct) > avail && bar_cells > GAUGE_BAR_MIN {
        bar_cells -= 1;
    }
    if bar_cells > 0 && gauge_total_w(name_w, bar_cells, has_pct) > avail {
        bar_cells = 0;
    }
    if name_w > 0 && gauge_total_w(name_w, bar_cells, has_pct) > avail {
        name_w = 0;
    }
    if gauge_total_w(name_w, bar_cells, has_pct) > avail {
        return GaugeFit::HIDDEN;
    }
    GaugeFit {
        name_w,
        bar_cells,
        visible: true,
    }
}

fn gauge_spans(
    fit: GaugeFit,
    name: &str,
    style: Style,
    pct: Option<f64>,
    elapsed_ms: u64,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if fit.name_w > 0 {
        let (nt, _pad) = fixed_split(name, fit.name_w);
        spans.extend(pulse_name_spans(&nt, style, elapsed_ms));
        spans.push(Span::raw(" ".repeat(GAUGE_NAME_GAP)));
    }
    match pct {
        Some(pct) if fit.bar_cells > 0 => {
            let style = Style::default().fg(theme::util_color(pct));
            spans.push(Span::styled("[", theme::dim()));
            spans.push(Span::styled(
                bar_string_with_cells(pct, fit.bar_cells),
                style,
            ));
            spans.push(Span::styled("]", theme::dim()));
            spans.push(Span::styled(format!(" {pct:.0}%"), style));
        }
        Some(pct) => {
            spans.push(Span::styled(
                format!("{pct:.0}%"),
                Style::default().fg(theme::util_color(pct)),
            ));
        }
        None => spans.push(Span::styled("—", theme::faint())),
    }
    spans
}

/// Per-char spans for the account name with the periodic pale-orange pulse.
/// Compatible tier (or mid-rest) renders the name plain — the crest lean is a
/// truecolor blend, and the rest gap is what keeps the motion a nudge.
fn pulse_name_spans(name: &str, style: Style, elapsed_ms: u64) -> Vec<Span<'static>> {
    use std::f32::consts::{PI, TAU};
    let plain = || vec![Span::styled(name.to_string(), style)];
    if theme::tier() != theme::Tier::Full {
        return plain();
    }
    let t = (elapsed_ms % (PULSE_SWEEP_MS + PULSE_REST_MS)) as f32;
    let sweep = PULSE_SWEEP_MS as f32;
    if t >= sweep {
        return plain();
    }
    let progress = t / sweep;
    // Rising/falling envelope so the sweep fades in and out at the row edges.
    let envelope = (PI * progress).sin();
    let head = progress * TAU;
    let len = name.chars().count().max(1) as f32;
    let base = style.fg.unwrap_or_else(theme::text_color);
    name.chars()
        .enumerate()
        .map(|(i, ch)| {
            let col = (i as f32 / len) * TAU;
            let crest = ((col - head).cos() * 0.5 + 0.5).powi(2);
            let lean = f64::from(crest * envelope * PULSE_DEPTH);
            let fg = theme::blend_over(base, theme::accent_2_pale_color(), lean);
            Span::styled(ch.to_string(), style.fg(fg))
        })
        .collect()
}

/// Rows the header needs this frame: 4 with an active-account gauge row, 3
/// otherwise (compact mode or no active profile). `render::draw` sizes the
/// top chunk with this so the gauge row is never reserved empty.
pub(super) fn header_height(app: &App) -> u16 {
    if !app.compact && active_gauge(app).is_some() {
        4
    } else {
        3
    }
}

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(10), Constraint::Min(20)])
        .split(area);

    draw_logo(frame, cols[0], app);

    let gauge = if app.compact { None } else { active_gauge(app) };
    let row_count = if gauge.is_some() { 4 } else { 3 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Length(1); row_count])
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

    // Row 1: account count left; `● status.claude.ai` right-aligned under the
    // version, kept only while a >= 3-cell gap separates it from the count.
    let count_txt = format!("{n} account{}", plural(n));
    let count_w = count_txt.chars().count();
    let feed = "status.claude.ai"; // display label per user choice; feed itself is status.claude.com
    let ind_width = 2 + feed.len(); // `● ` + label

    let mut row1_spans: Vec<Span<'static>> = vec![Span::styled(count_txt, theme::faint())];
    if info_width >= count_w + ind_width + 3 {
        let dot_gap = info_width - count_w - ind_width;
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

    // Row 2 (active profile only): the 5h gauge on its own row, right-aligned
    // under the feed dot. `gauge_fit` degrades it on narrow terminals.
    if let Some(g) = &gauge {
        let fit = gauge_fit(info_width, g.name.chars().count(), g.pct.is_some());
        let mut spans: Vec<Span<'static>> = Vec::new();
        if fit.visible {
            let elapsed = app.started_at.elapsed().as_millis() as u64;
            let gspans = gauge_spans(fit, &g.name, g.style, g.pct, elapsed);
            let gw: usize = gspans.iter().map(|s| s.content.chars().count()).sum();
            spans.push(Span::raw(" ".repeat(info_width.saturating_sub(gw))));
            spans.extend(gspans);
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(theme::base()),
            rows[2],
        );
    }

    tabs::draw(frame, rows[row_count - 1], app);
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
