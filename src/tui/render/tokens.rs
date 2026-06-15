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

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, TokenView};
use super::super::theme;
use super::format::fixed;
use super::panes::{
    SELECTOR_WIDTH, draw_selector_list, picker_row, section_box, section_box_verbatim,
};
use crate::pricing::PriceTable;
use crate::tokens::{ModelTokens, TokenStats, group_models, is_anthropic, model_display_name};

/// Key column width for label:value rows.
const KEY_W: usize = 8;
/// Block-glyph ramp for sparklines, low → high.
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

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

/// Compact USD: `$1.2K`, `$340`, `$12.50`, `$0.34`, `<$0.01`, `$0`.
fn fmt_money(usd: f64) -> String {
    if usd <= 0.0 {
        return "$0".to_string();
    }
    if usd >= 1e3 {
        let (v, suffix) = if usd >= 1e9 {
            (usd / 1e9, "B")
        } else if usd >= 1e6 {
            (usd / 1e6, "M")
        } else {
            (usd / 1e3, "K")
        };
        if v >= 100.0 {
            format!("${v:.0}{suffix}")
        } else if v >= 10.0 {
            format!("${v:.1}{suffix}")
        } else {
            format!("${v:.2}{suffix}")
        }
    } else if usd >= 100.0 {
        format!("${usd:.0}")
    } else {
        // Format first, then check — any positive value that rounds to $0.00
        // (incl. exactly 0.005 under round-half-to-even) shows `<$0.01`.
        let s = format!("${usd:.2}");
        if s == "$0.00" {
            "<$0.01".to_string()
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
/// no price table has loaded yet, and a trailing `+` when some models carry
/// tokens but no matching rate (so the figure is a floor, not a total).
fn cost_line(label: &str, prices: Option<&PriceTable>, models: &[ModelTokens]) -> Line<'static> {
    let value = match prices {
        Some(p) => {
            let (total, unpriced) = p.total_cost(models);
            let mut s = fmt_money(total);
            if unpriced > 0 {
                s.push('+');
            }
            s
        }
        None => "—".to_string(),
    };
    Line::from(vec![key(label), Span::styled(value, money_style())])
}

/// `2026-01-18[...]` → `jan 18`. Degrades to the raw string when too short.
fn short_date(ymd: &str) -> String {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    if ymd.len() < 10 {
        return ymd.to_string();
    }
    let month: usize = ymd[5..7].parse().unwrap_or(1);
    let day: u32 = ymd[8..10].parse().unwrap_or(0);
    let mon = MONTHS.get(month.saturating_sub(1)).copied().unwrap_or("?");
    format!("{mon} {day}")
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

/// A row with `left` flush left, `right` flush right, and `mid` centered between
/// them (an axis with a centered marker).
fn lcr(
    left: Vec<Span<'static>>,
    mid: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let (lw, mw, rw) = (span_w(&left), span_w(&mid), span_w(&right));
    let mid_start = width.saturating_sub(mw) / 2;
    let gap1 = mid_start.saturating_sub(lw).max(1);
    let gap2 = width.saturating_sub(lw + gap1 + mw + rw).max(1);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap1)));
    spans.extend(mid);
    spans.push(Span::raw(" ".repeat(gap2)));
    spans.extend(right);
    Line::from(spans)
}

fn busiest_hour(hours: &[u64; 24]) -> Option<usize> {
    let (hour, &count) = hours.iter().enumerate().max_by_key(|&(_, c)| *c)?;
    (count > 0).then_some(hour)
}

/// A model's token count on the active basis: in+out, or +cache when `count_cache`.
fn model_metric(m: &ModelTokens, count_cache: bool) -> u64 {
    if count_cache { m.total() } else { m.in_out() }
}

/// Grouped models ranked DESC by the active basis (so the bars descend by the
/// value actually shown).
fn ranked_models(stats: &TokenStats, count_cache: bool) -> Vec<ModelTokens> {
    let mut g = group_models(&stats.models);
    g.sort_unstable_by_key(|m| std::cmp::Reverse(model_metric(m, count_cache)));
    g
}

// ── dashboard view ─────────────────────────────────────────────────────────

fn draw_dashboard(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(stats) = app.token_stats.as_ref() else {
        let block = section_box("tokens", false, true);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "reading ~/.claude…",
                theme::faint(),
            )))
            .style(theme::base()),
            inner,
        );
        return;
    };

    let count_cache = app.config().state.count_cache;
    let prices = app.price_table.as_ref();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // today · total (incl. cost row)
            Constraint::Length(4), // daily
            Constraint::Length(7), // top models · composition
            Constraint::Min(4),    // hour · activity
        ])
        .split(area);

    let top = halves(rows[0], 42);
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
    card(
        frame,
        top[1],
        "total",
        None,
        false,
        total_lines(stats, inner_w(top[1]), count_cache, prices),
    );

    // Freshness badge → the daily card's title-right meta slot.
    let fresh = stats
        .topped_up_through
        .as_deref()
        .map(|d| format!("live thru {}", short_date(d)));
    card(
        frame,
        rows[1],
        "daily",
        fresh.as_deref(),
        false,
        daily_lines(stats, inner_w(rows[1])),
    );

    let mid = halves(rows[2], 55);
    card(
        frame,
        mid[0],
        "top models",
        None,
        false,
        model_lines(stats, inner_w(mid[0]), 5, count_cache, prices),
    );
    card(
        frame,
        mid[1],
        "composition",
        None,
        false,
        comp_lines(stats, inner_w(mid[1])),
    );

    let bot = halves(rows[3], 50);
    card(
        frame,
        bot[0],
        "hour of day",
        None,
        false,
        hour_lines(stats, inner_w(bot[0])),
    );
    card(
        frame,
        bot[1],
        "activity",
        None,
        false,
        activity_lines(stats, inner_w(bot[1])),
    );
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
        cost_line("cost", prices, &t.models),
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
                Style::default().fg(theme::accent_2_color()),
            )],
            w,
        ),
        cost_line("cost", prices, &stats.models),
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
    // Active span — earliest date flush left, latest flush right.
    if let (Some(first), Some(latest)) = (stats.first_session_date.as_deref(), last) {
        lines.push(lr(
            vec![Span::styled(short_date(first), theme::dim())],
            vec![Span::styled(short_date(latest), theme::dim())],
            w,
        ));
    }
    lines
}

fn daily_lines(stats: &TokenStats, w: usize) -> Vec<Line<'static>> {
    let vals: Vec<u64> = trail(&stats.daily, w).iter().map(|d| d.tokens).collect();
    if vals.is_empty() {
        return vec![Line::from(Span::styled("no daily data", theme::faint()))];
    }
    let (peak_v, peak_d) = stats
        .daily
        .iter()
        .max_by_key(|d| d.tokens)
        .map(|d| (d.tokens, d.date.clone()))
        .unwrap_or((0, String::new()));
    vec![
        Line::from(Span::styled(sparkline(&vals), theme::accent())),
        center(
            vec![Span::styled(
                format!("peak {} {}", fmt_count(peak_v), short_date(&peak_d)),
                theme::faint(),
            )],
            w,
        ),
    ]
}

fn model_lines(
    stats: &TokenStats,
    w: usize,
    n: usize,
    count_cache: bool,
    prices: Option<&PriceTable>,
) -> Vec<Line<'static>> {
    let grouped = ranked_models(stats, count_cache);
    let max = grouped
        .first()
        .map(|m| model_metric(m, count_cache))
        .unwrap_or(0);
    let names: Vec<String> = grouped
        .iter()
        .take(n)
        .map(|m| model_display_name(&m.model))
        .collect();
    // Token-count strings, right-aligned to a shared column so the counts (and
    // the cost suffix after them) form clean vertical columns across rows.
    let counts: Vec<String> = grouped
        .iter()
        .take(n)
        .map(|m| fmt_count(model_metric(m, count_cache)))
        .collect();
    let count_w = counts.iter().map(|s| s.chars().count()).max().unwrap_or(3);
    // Per-model API-equivalent cost suffix (empty when unpriced / not loaded),
    // right-aligned to its own shared column.
    let costs: Vec<String> = grouped
        .iter()
        .take(n)
        .map(|m| {
            prices
                .and_then(|p| p.cost(m))
                .map(fmt_money)
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
    grouped
        .iter()
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
            let val = model_metric(m, count_cache);
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
    let grand = stats.total_tokens();
    let bar_w = w.saturating_sub(KEY_W).saturating_sub(6).clamp(4, 28);
    [
        ("input", stats.total_input, theme::accent()),
        ("output", stats.total_output, theme::success()),
        ("c.write", stats.total_cache_create, theme::warning()),
        ("c.read", stats.total_cache_read, theme::info()),
    ]
    .into_iter()
    .map(|(label, value, fill)| {
        let pct = if grand == 0 {
            0.0
        } else {
            value as f64 / grand as f64 * 100.0
        };
        let mut spans = vec![key(label)];
        spans.extend(hbar(value, grand, bar_w, fill));
        spans.push(Span::styled(format!(" {pct:>3.0}%"), theme::dim()));
        Line::from(spans)
    })
    .collect()
}

fn hour_lines(stats: &TokenStats, w: usize) -> Vec<Line<'static>> {
    // Caption is the time axis (0h..23h) with the busiest hour centered between.
    let peak = busiest_hour(&stats.hour_counts)
        .map(|h| format!("peak {h:02}:00"))
        .unwrap_or_default();
    vec![
        Line::from(Span::styled(sparkline(&stats.hour_counts), theme::accent())),
        lcr(
            vec![Span::styled("0h", theme::faint())],
            vec![Span::styled(peak, theme::faint())],
            vec![Span::styled("23h", theme::faint())],
            w,
        ),
    ]
}

fn activity_lines(stats: &TokenStats, w: usize) -> Vec<Line<'static>> {
    let msgs: Vec<u64> = trail(&stats.activity, w)
        .iter()
        .map(|a| a.messages)
        .collect();
    if msgs.is_empty() {
        return vec![Line::from(Span::styled("no activity data", theme::faint()))];
    }
    let peak_msgs = stats.activity.iter().map(|a| a.messages).max().unwrap_or(0);
    let peak_sess = stats.activity.iter().map(|a| a.sessions).max().unwrap_or(0);
    let tools: u64 = stats.activity.iter().map(|a| a.tool_calls).sum();
    vec![
        Line::from(Span::styled(sparkline(&msgs), theme::accent())),
        lr(
            vec![Span::styled(
                format!("peak {} msgs", fmt_count(peak_msgs)),
                theme::faint(),
            )],
            vec![Span::styled(
                format!("{peak_sess} sess  {} tools", fmt_count(tools)),
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
        .constraints([Constraint::Length(SELECTOR_WIDTH), Constraint::Min(20)])
        .split(area);

    let count_cache = app.config().state.count_cache;
    let grouped = app
        .token_stats
        .as_ref()
        .map(|s| ranked_models(s, count_cache))
        .unwrap_or_default();
    let sel = app.token_model_cursor.min(grouped.len().saturating_sub(1));

    draw_selector_list(frame, cols[0], "models", true, sel, |w| {
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

    let grand = app
        .token_stats
        .as_ref()
        .map(TokenStats::total_tokens)
        .unwrap_or(0);
    draw_model_detail(
        frame,
        cols[1],
        grouped.get(sel),
        grand,
        app.price_table.as_ref(),
    );
}

fn draw_model_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    model: Option<&ModelTokens>,
    grand: u64,
    prices: Option<&PriceTable>,
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

    let kv = |label: &str, value: String| {
        Line::from(vec![key(label), Span::styled(value, theme::body())])
    };
    let mut lines = vec![
        kv("input", fmt_count(m.input)),
        kv("output", fmt_count(m.output)),
        kv("c.read", fmt_count(m.cache_read)),
        kv("c.write", fmt_count(m.cache_create)),
        Line::from(""),
        kv_accent("total", fmt_count(m.total())),
        kv("io", fmt_count(m.in_out())),
    ];

    // API-equivalent cost, split by token bucket (rates differ per bucket).
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "COST · API-EQUIVALENT",
        theme::label(),
    )));
    match prices {
        None => lines.push(Line::from(Span::styled("rates loading…", theme::faint()))),
        Some(p) => match p.rate(&m.model) {
            None => lines.push(Line::from(Span::styled(
                "no rate for this model",
                theme::faint(),
            ))),
            Some(r) => {
                let c_in = m.input as f64 * r.input;
                let c_out = m.output as f64 * r.output;
                let c_cache =
                    m.cache_read as f64 * r.cache_read + m.cache_create as f64 * r.cache_write;
                // Cost values share the `money_style` identity (vs the body-styled
                // token counts above).
                let cost_kv = |label: &str, value: String| {
                    Line::from(vec![key(label), Span::styled(value, money_style())])
                };
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
        },
    }

    let bar_w = (inner.width as usize).saturating_sub(14).clamp(6, 36);

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "SHARE OF ALL TOKENS",
        theme::label(),
    )));
    let share = if grand == 0 {
        0.0
    } else {
        m.total() as f64 / grand as f64 * 100.0
    };
    let mut share_line = hbar(m.total(), grand, bar_w, theme::accent());
    share_line.push(Span::styled(format!(" {share:>4.1}%"), theme::dim()));
    lines.push(Line::from(share_line));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("CACHE HIT", theme::label())));
    let denom = m.cache_read + m.cache_create + m.input;
    let hit = if denom == 0 {
        0.0
    } else {
        m.cache_read as f64 / denom as f64
    };
    let mut hit_line = hbar(
        (hit * 100.0) as u64,
        100,
        bar_w,
        Style::default().fg(theme::accent_2_color()),
    );
    hit_line.push(Span::styled(
        format!(" {:>3.0}%", hit * 100.0),
        theme::dim(),
    ));
    lines.push(Line::from(hit_line));

    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}
