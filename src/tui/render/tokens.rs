//! Tokens tab — global Claude Code token usage read from `~/.claude`
//! (`stats-cache.json` + recent transcript top-up; see `crate::tokens`).
//!
//! Two views. The **dashboard** (landing page) is a fixed grid of bordered
//! cards — today, lifetime totals, daily trend, top models, token composition,
//! hour-of-day, and activity — so each metric reads on its own rather than as
//! one long scroll. The **Models** master-detail (reached with `⏎`) drills into
//! a single model. All figures are global across every model/provider Claude
//! Code has run — the on-disk pool is shared across clauth profiles, not
//! per-account.
//!
//! Both views obey the [`TokenPeriod`] lens (`t` / action menu): `lifetime`
//! is the untouched all-time dashboard; `daily`/`weekly`/`monthly` scope the
//! cards to the current calendar bucket and re-bucket the trend rows. Where
//! the data can't follow — pre-cutoff days publish no cache/in-out split —
//! cards fall back to lifetime (badged) and costs render as `+`-marked floors.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, TokenFilter, TokenPeriod, TokenView, token_period_models};
use super::super::theme;
use super::format::fixed;
use super::panes::{
    draw_selector_list, picker_row, section_box, section_box_verbatim, selector_width,
};
use crate::pricing::PriceTable;
use crate::tokens::{
    ModelTokens, PeriodModel, TokenStats, bucket_activity, bucket_tokens, current_bucket_bounds,
    effective_cache_basis, is_anthropic, model_display_name, today_date,
};

/// Key column width for label:value rows.
const KEY_W: usize = 8;
/// Wider key column for the spelled-out `cache read`/`cache write` rows
/// (composition card + per-model detail): `cache write` (11) + 1 trailing space,
/// so every label keeps a gap before its bar/value and the columns stay aligned.
const WIDE_KEY_W: usize = 12;
/// Block-glyph ramp for sparklines, low → high.
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Hour-of-day card outer width: 24 hour buckets + 4 (border + 1-col padding
/// each side), so the fixed-width sparkline fills the box exactly.
const HOUR_BOX_W: u16 = 28;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    match app.token_view {
        TokenView::Dashboard => draw_dashboard(frame, area, app),
        TokenView::Models => draw_models(frame, area, app),
    }
}

// ── shared formatters ──────────────────────────────────────────────────────

/// Compact human count: `2.74B`, `186M`, `33.7M`, `12.3K`, `945`.
fn fmt_count(n: u64) -> String {
    let f = n as f64;
    let (v, suffix) = if f >= 1e12 {
        (f / 1e12, "T")
    } else if f >= 1e9 {
        (f / 1e9, "B")
    } else if f >= 1e6 {
        (f / 1e6, "M")
    } else if f >= 1e3 {
        (f / 1e3, "K")
    } else {
        return n.to_string();
    };
    if v >= 100.0 {
        format!("{v:.0}{suffix}")
    } else if v >= 10.0 {
        format!("{v:.1}{suffix}")
    } else {
        format!("{v:.2}{suffix}")
    }
}

/// Group the integer part of non-negative `n` with `,` thousands separators,
/// keeping `decimals` fractional digits: `(12345.6, 2)` → `12,345.60`.
fn group_thousands(n: f64, decimals: usize) -> String {
    let s = format!("{n:.decimals$}");
    let (int, frac) = s.split_once('.').map_or((s.as_str(), ""), |(i, f)| (i, f));
    let digits = int.len();
    let mut out = String::with_capacity(digits + digits / 3 + 1 + frac.len());
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (digits - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// Full USD with 2–3 decimal precision and `,`-grouped thousands: `$12,345.67`,
/// `$340.00`, `$12.50`, `$0.340`, `<$0.001`, `$0`. No K/M/B suffix — the whole
/// figure is shown, two decimals from a dollar up and three below.
fn fmt_money(usd: f64) -> String {
    if usd <= 0.0 {
        return "$0".to_string();
    }
    if usd >= 1.0 {
        format!("${}", group_thousands(usd, 2))
    } else {
        // Sub-dollar: 3 decimals. Format first, then floor — any positive value
        // that rounds to $0.000 (incl. 0.0005 under round-half-to-even) shows
        // `<$0.001` rather than a misleading zero.
        let s = format!("${usd:.3}");
        if s == "$0.000" {
            "<$0.001".to_string()
        } else {
            s
        }
    }
}

/// Cost value style — gives the API-equivalent figures one identity across the
/// tab, distinct from the accent-bold token headline.
fn money_style() -> Style {
    Style::default().fg(theme::accent_2_color())
}

/// A `label  $cost` row summing API-equivalent cost over `models`. Shows `—` when
/// no price table has loaded yet, and a trailing `+` when the figure is a floor
/// rather than a total — some models carry tokens but no matching rate, or the
/// caller knows the token set itself is incomplete (`floor`).
fn cost_line(
    label: &str,
    prices: Option<&PriceTable>,
    models: &[ModelTokens],
    floor: bool,
) -> Line<'static> {
    let value = match prices {
        Some(p) => {
            let (total, unpriced) = p.total_cost(models);
            let mut s = fmt_money(total);
            if unpriced > 0 || floor {
                s.push('+');
            }
            s
        }
        None => "—".to_string(),
    };
    Line::from(vec![key(label), Span::styled(value, money_style())])
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// `2026-01-18[...]` → `jan 18`. Degrades to the raw string when too short.
fn short_date(ymd: &str) -> String {
    if ymd.len() < 10 {
        return ymd.to_string();
    }
    let month: usize = ymd[5..7].parse().unwrap_or(1);
    let day: u32 = ymd[8..10].parse().unwrap_or(0);
    let mon = MONTHS.get(month.saturating_sub(1)).copied().unwrap_or("?");
    format!("{mon} {day}")
}

/// `2026-06[-..]` → `jun`. Degrades to the raw string when too short.
fn month_label(ymd: &str) -> String {
    if ymd.len() < 7 {
        return ymd.to_string();
    }
    let month: usize = ymd[5..7].parse().unwrap_or(1);
    MONTHS
        .get(month.saturating_sub(1))
        .copied()
        .unwrap_or("?")
        .to_string()
}

/// Block-glyph sparkline over `vals`, scaled to the slice's own max.
fn sparkline(vals: &[u64]) -> String {
    let max = vals.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return SPARK[0].to_string().repeat(vals.len());
    }
    vals.iter()
        .map(|&v| {
            let idx = ((v as f64 / max as f64) * 7.0).round() as usize;
            SPARK[idx.min(7)]
        })
        .collect()
}

/// `█`×filled + `░`×rest, `value` scaled against `max`, in `fill`.
fn hbar(value: u64, max: u64, width: usize, fill: Style) -> Vec<Span<'static>> {
    let filled = if max == 0 {
        0
    } else {
        (((value as f64 / max as f64) * width as f64).round() as usize).min(width)
    };
    vec![
        Span::styled("█".repeat(filled), fill),
        Span::styled(
            "░".repeat(width.saturating_sub(filled)),
            theme::line_strong(),
        ),
    ]
}

/// Fixed-width key span in the dim+bold label style — left-justified to `KEY_W`
/// so values/bars in adjacent rows line up regardless of label length.
fn key(label: &str) -> Span<'static> {
    Span::styled(format!("{label:<KEY_W$}"), theme::label())
}

/// Inner content width of a card (`section_box` border + 1-col horizontal padding).
fn inner_w(area: Rect) -> usize {
    (area.width as usize).saturating_sub(4)
}

/// Last `width`-bounded tail of a chronological slice (the recent days).
fn trail<T>(items: &[T], width: usize) -> &[T] {
    let n = items.len().min(width.max(1));
    &items[items.len().saturating_sub(n)..]
}

/// Total display columns of a span run.
fn span_w(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// A row with `left` flush to the start and `right` flush to `width`, the gap
/// filled with spaces. cloudy-tui leans on alignment + color to separate facts,
/// not a `·` middot (that's reserved for banner/toast prose).
fn lr(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let gap = width.saturating_sub(span_w(&left) + span_w(&right)).max(1);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    Line::from(spans)
}

/// A row with `spans` centered within `width`.
fn center(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let pad = width.saturating_sub(span_w(&spans)) / 2;
    let mut out = vec![Span::raw(" ".repeat(pad))];
    out.extend(spans);
    Line::from(out)
}

fn busiest_hour(hours: &[u64; 24]) -> Option<usize> {
    let (hour, &count) = hours.iter().enumerate().max_by_key(|&(_, c)| *c)?;
    (count > 0).then_some(hour)
}

// ── dashboard view ─────────────────────────────────────────────────────────

fn draw_dashboard(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(stats) = app.token_stats.as_ref() else {
        let block = section_box("tokens", false, true);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let (msg, style) = if app.tokens_failed {
            ("~/.claude/stats-cache.json unreadable", theme::danger())
        } else {
            ("reading ~/.claude", theme::faint())
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, style))).style(theme::base()),
            inner,
        );
        return;
    };

    let count_cache = app.config().state.count_cache;
    let prices = app.price_table.as_ref();
    let period = app.token_period;

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // today/this week/this month · total (incl. cost row)
            Constraint::Length(4), // daily/weekly/monthly trend
            Constraint::Length(7), // top models · composition
            Constraint::Min(4),    // hour · activity
        ])
        .split(area);

    let top = halves(rows[0], 42);
    if let Some(bucket) = period.bucket() {
        // Scoped first card: the current calendar bucket, meta = its start day.
        let (from, to) = current_bucket_bounds(&today_date(), bucket);
        let meta = format!("{}+", short_date(&from));
        card(
            frame,
            top[0],
            period.badge().unwrap_or("period"),
            Some(&meta),
            true,
            period_lines(stats, inner_w(top[0]), count_cache, prices, &from, &to),
        );
    } else {
        // Today's date → the today card's title-right meta badge.
        let today_meta = stats.today.as_ref().map(|t| short_date(&t.date));
        card(
            frame,
            top[0],
            "today",
            today_meta.as_deref(),
            true,
            today_lines(stats, inner_w(top[0]), count_cache, prices),
        );
    }
    // Total stays the lifetime anchor in every period — the scoped window
    // already owns the first card.
    card(
        frame,
        top[1],
        "total",
        None,
        false,
        total_lines(stats, inner_w(top[1]), count_cache, prices),
    );

    // Freshness badge → the trend card's title-right meta slot.
    let fresh = stats
        .topped_up_through
        .as_deref()
        .map(|d| format!("live thru {}", short_date(d)));
    let trend_title = match period {
        TokenPeriod::Weekly => "weekly",
        TokenPeriod::Monthly => "monthly",
        TokenPeriod::Lifetime | TokenPeriod::Daily => "daily",
    };
    card(
        frame,
        rows[1],
        trend_title,
        fresh.as_deref(),
        false,
        trend_lines(stats, inner_w(rows[1]), period),
    );

    let mid = halves(rows[2], 55);
    // Filter + period lenses both show as the card's title-right meta badge.
    let model_rows = token_period_models(app);
    let models_meta = join_badges(app.token_filter.badge(), period.badge());
    card(
        frame,
        mid[0],
        "top models",
        models_meta.as_deref(),
        false,
        model_lines(
            &model_rows,
            inner_w(mid[0]),
            5,
            effective_cache_basis(&model_rows, count_cache),
            prices,
            empty_models_msg(app.token_filter),
        ),
    );
    // Composition can scope honestly only to today (transcript-derived split);
    // weekly/monthly fall back to lifetime, badged as such.
    let (comp_meta, comp) = match period {
        TokenPeriod::Daily => (Some("today"), today_comp_lines(stats, inner_w(mid[1]))),
        TokenPeriod::Weekly | TokenPeriod::Monthly => {
            (Some("lifetime"), comp_lines(stats, inner_w(mid[1])))
        }
        TokenPeriod::Lifetime => (None, comp_lines(stats, inner_w(mid[1]))),
    };
    card(frame, mid[1], "composition", comp_meta, false, comp);

    // Hour graph is a fixed 24-bucket sparkline (one cell/hour). Pin its box to
    // 24 + 4 (border + 1-col padding each side) so the graph fills it with no
    // gap; activity takes the rest and shows more history on wide terminals.
    let bot = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(HOUR_BOX_W), Constraint::Min(0)])
        .split(rows[3]);
    // Same fallback shape as composition: per-day hours exist only for today.
    let (hour_meta, hours) = match period {
        TokenPeriod::Daily => (
            Some("today"),
            stats.today.as_ref().map(|t| t.hours).unwrap_or([0; 24]),
        ),
        TokenPeriod::Weekly | TokenPeriod::Monthly => (Some("lifetime"), stats.hour_counts),
        TokenPeriod::Lifetime => (None, stats.hour_counts),
    };
    card(
        frame,
        bot[0],
        "hour of day",
        hour_meta,
        false,
        hour_lines(&hours, inner_w(bot[0])),
    );
    let act_meta = match period {
        TokenPeriod::Weekly => Some("weekly"),
        TokenPeriod::Monthly => Some("monthly"),
        TokenPeriod::Lifetime | TokenPeriod::Daily => None,
    };
    card(
        frame,
        bot[1],
        "activity",
        act_meta,
        false,
        activity_lines(stats, inner_w(bot[1]), period),
    );
}

/// Compose the filter + period meta badges into one title-right string.
fn join_badges(filter: Option<&'static str>, period: Option<&'static str>) -> Option<String> {
    match (filter, period) {
        (Some(f), Some(p)) => Some(format!("{f}  {p}")),
        (Some(f), None) => Some(f.to_string()),
        (None, Some(p)) => Some(p.to_string()),
        (None, None) => None,
    }
}

/// Empty-state copy for a model list: name the filter when one is narrowing,
/// else the window simply has no usage.
fn empty_models_msg(filter: TokenFilter) -> &'static str {
    if filter == TokenFilter::All {
        "no model usage yet"
    } else {
        "no models match the filter"
    }
}

/// Split a row into two columns, the left taking `left_pct` percent.
fn halves(area: Rect, left_pct: u16) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(left_pct),
            Constraint::Percentage(100 - left_pct),
        ])
        .split(area)
}

/// Draw one bordered card with its lines. `meta` (if any) renders as a
/// right-aligned title badge (the cloudy-tui title-right meta slot).
fn card(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    meta: Option<&str>,
    first: bool,
    lines: Vec<Line<'static>>,
) {
    let mut block = section_box(title, false, first);
    if let Some(m) = meta {
        block = block.title(
            Line::from(Span::styled(format!(" {m} "), theme::dim())).alignment(Alignment::Right),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

fn today_lines(
    stats: &TokenStats,
    w: usize,
    count_cache: bool,
    prices: Option<&PriceTable>,
) -> Vec<Line<'static>> {
    let Some(t) = stats.today.as_ref() else {
        return vec![Line::from(Span::styled(
            "idle so far today",
            theme::faint(),
        ))];
    };
    let tokens = if count_cache { t.total() } else { t.in_out() };
    vec![
        kv_accent("tokens", fmt_count(tokens)),
        cost_line("cost", prices, &t.models, false),
        Line::from(vec![
            key("msgs"),
            Span::styled(t.messages.to_string(), theme::body()),
        ]),
        lr(
            vec![Span::styled(
                format!("{} in", fmt_count(t.input)),
                theme::dim(),
            )],
            vec![Span::styled(
                format!("{} out", fmt_count(t.output)),
                theme::dim(),
            )],
            w,
        ),
    ]
}

fn total_lines(
    stats: &TokenStats,
    w: usize,
    count_cache: bool,
    prices: Option<&PriceTable>,
) -> Vec<Line<'static>> {
    let last = stats
        .topped_up_through
        .as_deref()
        .or(stats.daily.last().map(|d| d.date.as_str()))
        .or(stats.last_computed_date.as_deref());
    let total = if count_cache {
        stats.total_tokens()
    } else {
        stats.total_in_out()
    };
    let mut lines = vec![
        lr(
            vec![
                key("tokens"),
                Span::styled(
                    fmt_count(total),
                    theme::accent().add_modifier(Modifier::BOLD),
                ),
            ],
            vec![Span::styled(
                format!("{:.0}% cached", stats.cache_hit_ratio() * 100.0),
                Style::default().fg(theme::info_color()),
            )],
            w,
        ),
        cost_line("cost", prices, &stats.models, false),
        lr(
            vec![Span::styled(
                format!("{} sess", stats.total_sessions),
                theme::body(),
            )],
            vec![Span::styled(
                format!("{} msgs", fmt_count(stats.total_messages)),
                theme::body(),
            )],
            w,
        ),
    ];
    if let (Some(first), Some(latest)) = (stats.first_session_date.as_deref(), last) {
        lines.push(lr(
            vec![Span::styled(short_date(first), theme::dim())],
            vec![Span::styled(short_date(latest), theme::dim())],
            w,
        ));
    }
    lines
}

/// The this-week / this-month headline card. Tokens and msgs sum the calendar
/// bucket's days; cost sums the priceable (split-bearing) days and renders as
/// a `+` floor when stats-cache days hide part of the window.
fn period_lines(
    stats: &TokenStats,
    w: usize,
    count_cache: bool,
    prices: Option<&PriceTable>,
    from: &str,
    to: &str,
) -> Vec<Line<'static>> {
    let in_range = |date: &str| date >= from && date <= to;
    let rows = crate::tokens::period_models(&stats.daily_models, from, to);
    let complete = rows.iter().all(|m| m.split_complete);
    let splits: Vec<ModelTokens> = rows.iter().map(|m| m.split.clone()).collect();
    let in_out: u64 = stats
        .daily
        .iter()
        .filter(|d| in_range(&d.date))
        .map(|d| d.tokens)
        .sum();
    if in_out == 0 && rows.is_empty() {
        return vec![Line::from(Span::styled("idle so far", theme::faint()))];
    }
    // The cache-counting basis holds only when the whole window carries splits.
    let tokens = if count_cache && complete {
        splits.iter().map(ModelTokens::total).sum()
    } else {
        in_out
    };
    let msgs: u64 = stats
        .activity
        .iter()
        .filter(|a| in_range(&a.date))
        .map(|a| a.messages)
        .sum();
    vec![
        kv_accent("tokens", fmt_count(tokens)),
        cost_line("cost", prices, &splits, !complete),
        Line::from(vec![
            key("msgs"),
            Span::styled(fmt_count(msgs), theme::body()),
        ]),
        lr(
            vec![Span::styled(short_date(from), theme::dim())],
            vec![Span::styled(short_date(to), theme::dim())],
            w,
        ),
    ]
}

/// The trend sparkline — per-day rows, or calendar buckets under the weekly /
/// monthly lens (peak caption names the bucket).
fn trend_lines(stats: &TokenStats, w: usize, period: TokenPeriod) -> Vec<Line<'static>> {
    let series = match period.bucket() {
        Some(b) => bucket_tokens(&stats.daily, b),
        None => stats.daily.clone(),
    };
    let vals: Vec<u64> = trail(&series, w).iter().map(|d| d.tokens).collect();
    if vals.is_empty() {
        return vec![Line::from(Span::styled("no daily data", theme::faint()))];
    }
    let (peak_v, peak_d) = series
        .iter()
        .max_by_key(|d| d.tokens)
        .map(|d| (d.tokens, d.date.clone()))
        .unwrap_or((0, String::new()));
    let peak = match period {
        TokenPeriod::Weekly => {
            format!("peak {} wk of {}", fmt_count(peak_v), short_date(&peak_d))
        }
        TokenPeriod::Monthly => format!("peak {} {}", fmt_count(peak_v), month_label(&peak_d)),
        TokenPeriod::Lifetime | TokenPeriod::Daily => {
            format!("peak {} {}", fmt_count(peak_v), short_date(&peak_d))
        }
    };
    vec![
        center(vec![Span::styled(sparkline(&vals), theme::accent())], w),
        center(vec![Span::styled(peak, theme::faint())], w),
    ]
}

fn model_lines(
    rows: &[PeriodModel],
    w: usize,
    n: usize,
    basis: bool,
    prices: Option<&PriceTable>,
    empty_msg: &'static str,
) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return vec![Line::from(Span::styled(empty_msg, theme::faint()))];
    }
    let max = rows.first().map(|m| m.metric(basis)).unwrap_or(0);
    let names: Vec<String> = rows
        .iter()
        .take(n)
        .map(|m| model_display_name(&m.model))
        .collect();
    // Token-count strings, right-aligned to a shared column so the counts (and
    // the cost suffix after them) form clean vertical columns across rows.
    let counts: Vec<String> = rows
        .iter()
        .take(n)
        .map(|m| fmt_count(m.metric(basis)))
        .collect();
    let count_w = counts.iter().map(|s| s.chars().count()).max().unwrap_or(3);
    // Per-model API-equivalent cost suffix (empty when unpriced / not loaded),
    // right-aligned to its own shared column; a partial split reads as a floor.
    let costs: Vec<String> = rows
        .iter()
        .take(n)
        .map(|m| {
            prices
                .and_then(|p| p.cost(&m.split))
                .map(|c| {
                    let mut s = fmt_money(c);
                    if !m.split_complete {
                        s.push('+');
                    }
                    s
                })
                .unwrap_or_default()
        })
        .collect();
    let cost_w = costs.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    // Only carry the cost column when the card is wide enough for label + a
    // minimum bar + the count + cost; otherwise drop it (token bars stay legible
    // on narrow terminals rather than clipping). `cost_col` includes its gap.
    let cost_col = if cost_w > 0 { cost_w + 1 } else { 0 };
    let show_cost = cost_col > 0 && w >= 6 + 2 + 8 + count_w + cost_col;
    let cost_col = if show_cost { cost_col } else { 0 };

    // Expand the label column to the longest name when there's room; only
    // truncate (via `fixed`) when the card is too narrow. Reserve the count
    // column + two 1-cell gaps + a minimum 8-cell bar + the cost column.
    let longest = names.iter().map(|s| s.chars().count()).max().unwrap_or(6);
    let max_label = w.saturating_sub(count_w + 2 + 8 + cost_col);
    let label_w = longest.clamp(6, max_label.max(6));
    let bar_w = w
        .saturating_sub(label_w)
        .saturating_sub(count_w + 2 + cost_col)
        .clamp(4, 30);
    rows.iter()
        .take(n)
        .zip(names.iter())
        .zip(counts.iter())
        .zip(costs.iter())
        .map(|(((m, name), count), cost)| {
            let fill = if is_anthropic(&m.model) {
                theme::accent()
            } else {
                theme::dim()
            };
            let val = m.metric(basis);
            let mut spans = vec![
                Span::styled(fixed(name, label_w), theme::body()),
                Span::raw(" "),
            ];
            spans.extend(hbar(val, max, bar_w, fill));
            spans.push(Span::styled(format!(" {count:>count_w$}"), theme::dim()));
            if show_cost {
                spans.push(Span::styled(format!(" {cost:>cost_w$}"), money_style()));
            }
            Line::from(spans)
        })
        .collect()
}

fn comp_lines(stats: &TokenStats, w: usize) -> Vec<Line<'static>> {
    comp_rows(
        stats.total_input,
        stats.total_output,
        stats.total_cache_create,
        stats.total_cache_read,
        w,
    )
}

/// Today's transcript-derived split — the one window besides lifetime whose
/// composition is fully known.
fn today_comp_lines(stats: &TokenStats, w: usize) -> Vec<Line<'static>> {
    match stats.today.as_ref() {
        Some(t) => comp_rows(t.input, t.output, t.cache_create, t.cache_read, w),
        None => vec![Line::from(Span::styled(
            "idle so far today",
            theme::faint(),
        ))],
    }
}

fn comp_rows(
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
    w: usize,
) -> Vec<Line<'static>> {
    let grand = input
        .saturating_add(output)
        .saturating_add(cache_write)
        .saturating_add(cache_read);
    let bar_w = w.saturating_sub(WIDE_KEY_W).saturating_sub(6).clamp(4, 28);
    [
        ("input", input, theme::accent()),
        ("output", output, theme::success()),
        ("cache write", cache_write, theme::warning()),
        ("cache read", cache_read, theme::info()),
    ]
    .into_iter()
    .map(|(label, value, fill)| {
        let pct = if grand == 0 {
            0.0
        } else {
            value as f64 / grand as f64 * 100.0
        };
        let mut spans = vec![Span::styled(
            format!("{label:<WIDE_KEY_W$}"),
            theme::label(),
        )];
        spans.extend(hbar(value, grand, bar_w, fill));
        spans.push(Span::styled(format!(" {pct:>3.0}%"), theme::dim()));
        Line::from(spans)
    })
    .collect()
}

fn hour_lines(hours: &[u64; 24], w: usize) -> Vec<Line<'static>> {
    // Centered 24-bucket sparkline with the busiest hour centered below it.
    let peak = busiest_hour(hours)
        .map(|h| format!("peak {h:02}:00"))
        .unwrap_or_default();
    vec![
        center(vec![Span::styled(sparkline(hours), theme::accent())], w),
        center(vec![Span::styled(peak, theme::faint())], w),
    ]
}

fn activity_lines(stats: &TokenStats, w: usize, period: TokenPeriod) -> Vec<Line<'static>> {
    let series = match period.bucket() {
        Some(b) => bucket_activity(&stats.activity, b),
        None => stats.activity.clone(),
    };
    let msgs: Vec<u64> = trail(&series, w).iter().map(|a| a.messages).collect();
    if msgs.is_empty() {
        return vec![Line::from(Span::styled("no activity data", theme::faint()))];
    }
    let peak_msgs = series.iter().map(|a| a.messages).max().unwrap_or(0);
    let peak_sess = series.iter().map(|a| a.sessions).max().unwrap_or(0);
    let tools: u64 = series.iter().map(|a| a.tool_calls).sum();
    vec![
        center(vec![Span::styled(sparkline(&msgs), theme::accent())], w),
        center(
            vec![Span::styled(
                format!(
                    "peak {} msgs   {peak_sess} sess   {} tools",
                    fmt_count(peak_msgs),
                    fmt_count(tools),
                ),
                theme::faint(),
            )],
            w,
        ),
    ]
}

/// `key:value` line whose value is the accent-bold headline number.
fn kv_accent(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        key(label),
        Span::styled(value, theme::accent().add_modifier(Modifier::BOLD)),
    ])
}

// ── models master-detail view ──────────────────────────────────────────────

fn draw_models(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(selector_width(area.width)),
            Constraint::Min(20),
        ])
        .split(area);

    let grouped = token_period_models(app);
    let sel = app.token_model_cursor.min(grouped.len().saturating_sub(1));

    // The selector title carries the active lenses so the narrowed list reads
    // as such.
    let title = match join_badges(app.token_filter.badge(), app.token_period.badge()) {
        Some(badge) => format!("models  {badge}"),
        None => "models".to_string(),
    };
    // A lens can empty the list mid-view (menu on the Models view) —
    // `draw_selector_list`'s shared empty state talks about accounts, so
    // render the lens-specific message instead.
    if grouped.is_empty() {
        let block = section_box(&title, true, true);
        let inner = block.inner(cols[0]);
        frame.render_widget(block, cols[0]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                empty_models_msg(app.token_filter),
                theme::faint(),
            )))
            .style(theme::base()),
            inner,
        );
        draw_model_detail(
            frame,
            cols[1],
            None,
            0,
            app.price_table.as_ref(),
            app.token_period,
        );
        return;
    }
    draw_selector_list(frame, cols[0], &title, true, sel, |w| {
        grouped
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let style = if is_anthropic(&m.model) {
                    Style::default().fg(theme::text_color())
                } else {
                    theme::dim()
                };
                picker_row(i == sel, true, model_display_name(&m.model), style, w)
            })
            .collect()
    });

    draw_model_detail(
        frame,
        cols[1],
        grouped.get(sel),
        period_grand(app),
        app.price_table.as_ref(),
        app.token_period,
    );
}

/// Share-of-window denominator for the detail card, on the same basis as its
/// numerator per mode: full throughput for lifetime/today, in+out for scoped
/// weeks/months (the only figure every day in range carries).
fn period_grand(app: &App) -> u64 {
    let Some(stats) = app.token_stats.as_ref() else {
        return 0;
    };
    if let Some(bucket) = app.token_period.bucket() {
        let (from, to) = current_bucket_bounds(&today_date(), bucket);
        stats
            .daily
            .iter()
            .filter(|d| d.date >= from && d.date <= to)
            .map(|d| d.tokens)
            .sum()
    } else if app.token_period == TokenPeriod::Daily {
        stats.today.as_ref().map(|t| t.total()).unwrap_or(0)
    } else {
        stats.total_tokens()
    }
}

fn draw_model_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    model: Option<&PeriodModel>,
    grand: u64,
    prices: Option<&PriceTable>,
    period: TokenPeriod,
) {
    let title = model
        .map(|m| model_display_name(&m.model))
        .unwrap_or_else(|| "model".to_string());
    let block = section_box_verbatim(&title, false, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(m) = model else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("no model data", theme::faint())))
                .style(theme::base()),
            inner,
        );
        return;
    };

    // This block carries the spelled-out `cache read`/`cache write` rows, so it
    // pads to WIDE_KEY_W (not the shared KEY_W) to keep the value column aligned.
    let kv = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label:<WIDE_KEY_W$}"), theme::label()),
            Span::styled(value, theme::body()),
        ])
    };
    let s = &m.split;
    let mut lines = if m.split_complete {
        vec![
            kv("input", fmt_count(s.input)),
            kv("output", fmt_count(s.output)),
            kv("cache read", fmt_count(s.cache_read)),
            kv("cache write", fmt_count(s.cache_create)),
            Line::from(""),
            Line::from(vec![
                Span::styled(format!("{:<WIDE_KEY_W$}", "total"), theme::label()),
                Span::styled(
                    fmt_count(s.total()),
                    theme::accent().add_modifier(Modifier::BOLD),
                ),
            ]),
            kv("io", fmt_count(s.in_out())),
        ]
    } else {
        // Part of the window predates the transcript cutoff, where only the
        // combined in+out per model exists — no split rows, no cache lens.
        vec![
            Line::from(vec![
                Span::styled(format!("{:<WIDE_KEY_W$}", "tokens"), theme::label()),
                Span::styled(
                    fmt_count(m.in_out),
                    theme::accent().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                "in+out only (older days carry no split)",
                theme::faint(),
            )),
        ]
    };

    // API-equivalent cost, split by token bucket (rates differ per bucket).
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "COST · API-EQUIVALENT",
        theme::label(),
    )));
    // Cost values share the `money_style` identity (vs the body-styled
    // token counts above).
    let cost_kv = |label: &str, value: String| {
        Line::from(vec![key(label), Span::styled(value, money_style())])
    };
    match prices {
        None => lines.push(Line::from(Span::styled("rates loading", theme::faint()))),
        Some(p) => match p.rate(&m.model) {
            None => lines.push(Line::from(Span::styled(
                "no rate for this model",
                theme::faint(),
            ))),
            Some(r) if m.split_complete => {
                let c_in = s.input as f64 * r.input;
                let c_out = s.output as f64 * r.output;
                let c_cache =
                    s.cache_read as f64 * r.cache_read + s.cache_create as f64 * r.cache_write;
                lines.push(cost_kv("input", fmt_money(c_in)));
                lines.push(cost_kv("output", fmt_money(c_out)));
                lines.push(cost_kv("cache", fmt_money(c_cache)));
                lines.push(Line::from(vec![
                    key("total"),
                    Span::styled(
                        fmt_money(c_in + c_out + c_cache),
                        money_style().add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            Some(_) => {
                // Floor over the split-bearing days only.
                let floor = p.cost(s).unwrap_or(0.0);
                lines.push(Line::from(vec![
                    key("total"),
                    Span::styled(
                        format!("{}+", fmt_money(floor)),
                        money_style().add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
        },
    }

    let bar_w = (inner.width as usize).saturating_sub(14).clamp(6, 36);

    lines.push(Line::from(""));
    let share_title = match period.badge() {
        Some(b) => format!("SHARE OF {}", b.to_uppercase()),
        None => "SHARE OF ALL TOKENS".to_string(),
    };
    lines.push(Line::from(Span::styled(share_title, theme::label())));
    // Numerator on the denominator's basis (see `period_grand`): in+out under
    // a weekly/monthly bucket, full throughput otherwise.
    let share_val = if period.bucket().is_some() {
        m.in_out
    } else {
        s.total()
    };
    let share = if grand == 0 {
        0.0
    } else {
        share_val as f64 / grand as f64 * 100.0
    };
    let mut share_line = hbar(share_val, grand, bar_w, theme::accent());
    share_line.push(Span::styled(format!(" {share:>4.1}%"), theme::dim()));
    lines.push(Line::from(share_line));

    if m.split_complete {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("CACHE HIT", theme::label())));
        let denom = s.cache_read + s.cache_create + s.input;
        let hit = if denom == 0 {
            0.0
        } else {
            s.cache_read as f64 / denom as f64
        };
        let mut hit_line = hbar(
            (hit * 100.0) as u64,
            100,
            bar_w,
            Style::default().fg(theme::info_color()),
        );
        hit_line.push(Span::styled(
            format!(" {:>3.0}%", hit * 100.0),
            theme::dim(),
        ));
        lines.push(Line::from(hit_line));
    }

    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}
