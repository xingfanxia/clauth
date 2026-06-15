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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, TokenView};
use super::super::theme;
use super::format::fixed;
use super::panes::{
    SELECTOR_WIDTH, draw_selector_list, picker_row, section_box, section_box_verbatim,
};
use crate::tokens::{ModelTokens, TokenStats, group_models, is_anthropic};

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

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // today · total
            Constraint::Length(4), // daily
            Constraint::Length(7), // top models · composition
            Constraint::Min(4),    // hour · activity
        ])
        .split(area);

    let top = halves(rows[0], 42);
    card(frame, top[0], &today_title(stats), true, today_lines(stats));
    card(frame, top[1], "total", false, total_lines(stats));

    card(
        frame,
        rows[1],
        "daily",
        false,
        daily_lines(stats, inner_w(rows[1])),
    );

    let mid = halves(rows[2], 55);
    card(
        frame,
        mid[0],
        "top models",
        false,
        model_lines(stats, inner_w(mid[0]), 5),
    );
    card(
        frame,
        mid[1],
        "composition",
        false,
        comp_lines(stats, inner_w(mid[1])),
    );

    let bot = halves(rows[3], 50);
    card(
        frame,
        bot[0],
        "hour of day",
        false,
        hour_lines(stats, inner_w(bot[0])),
    );
    card(
        frame,
        bot[1],
        "activity",
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

/// Draw one bordered card with its lines.
fn card(frame: &mut Frame<'_>, area: Rect, title: &str, first: bool, lines: Vec<Line<'static>>) {
    let block = section_box(title, false, first);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

fn today_title(stats: &TokenStats) -> String {
    match &stats.today {
        Some(t) => format!("today · {}", short_date(&t.date)),
        None => "today".to_string(),
    }
}

fn today_lines(stats: &TokenStats) -> Vec<Line<'static>> {
    let Some(t) = stats.today.as_ref() else {
        return vec![Line::from(Span::styled(
            "idle so far today",
            theme::faint(),
        ))];
    };
    vec![
        kv_accent("tokens", fmt_count(t.total())),
        Line::from(vec![
            key("msgs"),
            Span::styled(t.messages.to_string(), theme::body()),
        ]),
        Line::from(vec![
            key("io"),
            Span::styled(
                format!("{} in · {} out", fmt_count(t.input), fmt_count(t.output)),
                theme::dim(),
            ),
        ]),
    ]
}

fn total_lines(stats: &TokenStats) -> Vec<Line<'static>> {
    let last = stats
        .topped_up_through
        .as_deref()
        .or(stats.daily.last().map(|d| d.date.as_str()))
        .or(stats.last_computed_date.as_deref());
    let span = match (stats.first_session_date.as_deref(), last) {
        (Some(f), Some(l)) => format!(" · {} – {}", short_date(f), short_date(l)),
        _ => String::new(),
    };
    vec![
        kv_accent("tokens", fmt_count(stats.total_tokens())),
        Line::from(vec![
            key("usage"),
            Span::styled(
                format!(
                    "{} sess · {} msgs",
                    stats.total_sessions,
                    fmt_count(stats.total_messages)
                ),
                theme::body(),
            ),
        ]),
        Line::from(vec![
            key("cache"),
            Span::styled(
                format!("{:.0}% hit", stats.cache_hit_ratio() * 100.0),
                Style::default().fg(theme::accent_2_color()),
            ),
            Span::styled(span, theme::faint()),
        ]),
    ]
}

fn daily_lines(stats: &TokenStats, w: usize) -> Vec<Line<'static>> {
    let vals: Vec<u64> = trail(&stats.daily, w).iter().map(|d| d.tokens).collect();
    if vals.is_empty() {
        return vec![Line::from(Span::styled("no daily data", theme::faint()))];
    }
    let last = stats.daily.last().map(|d| d.tokens).unwrap_or(0);
    let (peak_v, peak_d) = stats
        .daily
        .iter()
        .max_by_key(|d| d.tokens)
        .map(|d| (d.tokens, d.date.clone()))
        .unwrap_or((0, String::new()));
    let fresh = stats
        .topped_up_through
        .as_deref()
        .map(|d| format!(" · live thru {}", short_date(d)))
        .unwrap_or_default();
    vec![
        Line::from(Span::styled(sparkline(&vals), theme::accent())),
        Line::from(Span::styled(
            format!(
                "{}d · last {} · peak {} ({}){fresh}",
                vals.len(),
                fmt_count(last),
                fmt_count(peak_v),
                short_date(&peak_d)
            ),
            theme::faint(),
        )),
    ]
}

fn model_lines(stats: &TokenStats, w: usize, n: usize) -> Vec<Line<'static>> {
    let grouped = group_models(&stats.models);
    let max = grouped.first().map(ModelTokens::total).unwrap_or(0);
    let label_w = (w / 3).clamp(6, 14);
    let bar_w = w.saturating_sub(label_w).saturating_sub(9).clamp(4, 28);
    grouped
        .iter()
        .take(n)
        .map(|m| {
            let fill = if is_anthropic(&m.model) {
                theme::accent()
            } else {
                theme::dim()
            };
            let mut spans = vec![
                Span::styled(fixed(&m.model, label_w), theme::body()),
                Span::raw(" "),
            ];
            spans.extend(hbar(m.total(), max, bar_w, fill));
            spans.push(Span::styled(
                format!(" {}", fmt_count(m.total())),
                theme::dim(),
            ));
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

fn hour_lines(stats: &TokenStats, _w: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        sparkline(&stats.hour_counts),
        theme::accent(),
    ))];
    let caption = match busiest_hour(&stats.hour_counts) {
        Some(h) => format!("0h ─── 23h · busiest {h:02}:00"),
        None => "no activity recorded".to_string(),
    };
    lines.push(Line::from(Span::styled(caption, theme::faint())));
    lines
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
        Line::from(Span::styled(
            format!(
                "msgs/day · peak {} · {} sess · {} tools",
                fmt_count(peak_msgs),
                peak_sess,
                fmt_count(tools)
            ),
            theme::faint(),
        )),
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

    let grouped = app
        .token_stats
        .as_ref()
        .map(|s| group_models(&s.models))
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
                picker_row(i == sel, true, m.model.clone(), style, w)
            })
            .collect()
    });

    let grand = app
        .token_stats
        .as_ref()
        .map(TokenStats::total_tokens)
        .unwrap_or(0);
    draw_model_detail(frame, cols[1], grouped.get(sel), grand);
}

fn draw_model_detail(frame: &mut Frame<'_>, area: Rect, model: Option<&ModelTokens>, grand: u64) {
    let title = model.map(|m| m.model.as_str()).unwrap_or("model");
    let block = section_box_verbatim(title, false, false);
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
