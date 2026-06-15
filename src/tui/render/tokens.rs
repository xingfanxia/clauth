//! Tokens tab — global Claude Code token usage read from `~/.claude`
//! (`stats-cache.json` + recent transcript top-up; see `crate::tokens`).
//!
//! Two views: a full-width **dashboard** (the landing page — totals, daily
//! trend, top models, hour-of-day, composition, cache efficiency, activity) and
//! a per-model **master-detail** breakdown reached with `⏎`. All figures are
//! global across every model/provider Claude Code has run — the on-disk
//! transcript/stats pool is shared across clauth profiles, not per-account.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, TokenView};
use super::super::theme;
use super::format::fixed;
use super::panes::{
    SELECTOR_WIDTH, draw_scrollbar, draw_selector_list, picker_row, section_box,
    section_box_verbatim,
};
use crate::tokens::{ModelTokens, TokenStats, group_models, is_anthropic};

/// Key column width for the dashboard's label:value rows.
const KEY_W: usize = 8;
/// Block-glyph ramp for sparklines, low → high.
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// How many top models the dashboard lists before stopping.
const TOP_MODELS: usize = 8;

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

/// Padded key span in the dim+bold label style (matches the usage tab).
fn key(label: &str) -> Span<'static> {
    let pad = KEY_W.saturating_sub(label.chars().count()).max(1);
    Span::styled(format!("{label}{}", " ".repeat(pad)), theme::label())
}

/// Eyebrow section header (`TEXT_DIM + bold`).
fn eyebrow(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), theme::label()))
}

// ── dashboard view ─────────────────────────────────────────────────────────

fn draw_dashboard(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = section_box("tokens", false, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(stats) = app.token_stats.as_ref() else {
        let hint = Paragraph::new(Line::from(Span::styled(
            "reading ~/.claude…",
            theme::faint(),
        )))
        .style(theme::base());
        frame.render_widget(hint, inner);
        return;
    };

    let lines = dashboard_lines(stats, inner.width);
    let total = lines.len();
    let viewport = inner.height as usize;
    let max_scroll = total.saturating_sub(viewport) as u16;
    // Publish the max so the key handler can clamp over-scroll on the next event.
    app.token_scroll_max.set(max_scroll);
    let scroll = app.token_scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((scroll, 0)),
        inner,
    );
    draw_scrollbar(frame, inner, total, scroll as usize, viewport);
}

fn dashboard_lines(stats: &TokenStats, inner_w: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let width = inner_w as usize;
    let cache_total = stats.total_cache_read + stats.total_cache_create;

    // ── headline ──
    lines.push(Line::from(Span::styled(
        "global across all models · from ~/.claude",
        theme::faint(),
    )));
    lines.push(Line::from(vec![
        key("total"),
        Span::styled(
            fmt_count(stats.total_tokens()),
            theme::accent().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  ·  {} sessions · {} msgs",
                stats.total_sessions,
                fmt_count(stats.total_messages)
            ),
            theme::dim(),
        ),
    ]));
    lines.push(Line::from(vec![
        key("tokens"),
        Span::styled(
            format!(
                "{} in · {} out · {} cache",
                fmt_count(stats.total_input),
                fmt_count(stats.total_output),
                fmt_count(cache_total)
            ),
            theme::body(),
        ),
    ]));
    lines.push(Line::from({
        let mut spans = vec![
            key("cache"),
            Span::styled(
                format!("{:.0}% hit", stats.cache_hit_ratio() * 100.0),
                Style::default().fg(theme::accent_2_color()),
            ),
        ];
        let freshness = match (&stats.topped_up_through, &stats.last_computed_date) {
            (Some(d), _) => format!("  ·  live through {}", short_date(d)),
            (None, Some(d)) => format!("  ·  cache {}", short_date(d)),
            (None, None) => String::new(),
        };
        if !freshness.is_empty() {
            spans.push(Span::styled(freshness, theme::faint()));
        }
        spans
    }));
    if let (Some(first), Some(last)) = (
        stats.first_session_date.as_deref(),
        stats.daily.last().map(|d| d.date.as_str()),
    ) {
        lines.push(Line::from(vec![
            key("span"),
            Span::styled(
                format!("{} – {}", short_date(first), short_date(last)),
                theme::dim(),
            ),
        ]));
    }

    // ── daily trend ──
    push_blank(&mut lines);
    let daily: Vec<u64> = trail(&stats.daily, width)
        .iter()
        .map(|d| d.tokens)
        .collect();
    if !daily.is_empty() {
        lines.push(eyebrow(&format!("DAILY · {} days", daily.len())));
        lines.push(Line::from(Span::styled(sparkline(&daily), theme::accent())));
        if let Some((peak_v, peak_d)) = stats
            .daily
            .iter()
            .max_by_key(|d| d.tokens)
            .map(|d| (d.tokens, d.date.clone()))
        {
            let last = stats.daily.last().map(|d| d.tokens).unwrap_or(0);
            lines.push(Line::from(Span::styled(
                format!(
                    "last {} · peak {} ({})",
                    fmt_count(last),
                    fmt_count(peak_v),
                    short_date(&peak_d)
                ),
                theme::faint(),
            )));
        }
    }

    // ── top models ──
    push_blank(&mut lines);
    lines.push(eyebrow("TOP MODELS"));
    let grouped = group_models(&stats.models);
    let max = grouped.first().map(ModelTokens::total).unwrap_or(0);
    let label_w = 16.min(width.saturating_sub(12)).max(6);
    let val_w = 8;
    let bar_w = width
        .saturating_sub(label_w)
        .saturating_sub(val_w)
        .saturating_sub(2)
        .clamp(6, 40);
    for m in grouped.iter().take(TOP_MODELS) {
        let fill = if is_anthropic(&m.model) {
            theme::accent()
        } else {
            theme::dim()
        };
        let mut spans = vec![Span::styled(fixed(&m.model, label_w), theme::body())];
        spans.push(Span::raw(" "));
        spans.extend(hbar(m.total(), max, bar_w, fill));
        spans.push(Span::styled(
            format!(" {}", fmt_count(m.total())),
            theme::dim(),
        ));
        lines.push(Line::from(spans));
    }

    // ── hour of day ──
    push_blank(&mut lines);
    lines.push(eyebrow("HOUR OF DAY"));
    lines.push(Line::from(Span::styled(
        sparkline(&stats.hour_counts),
        theme::accent(),
    )));
    if let Some(busiest) = busiest_hour(&stats.hour_counts) {
        lines.push(Line::from(Span::styled(
            format!("0h ───────── 23h · busiest {busiest:02}:00"),
            theme::faint(),
        )));
    }

    // ── composition ──
    push_blank(&mut lines);
    lines.push(eyebrow("COMPOSITION"));
    let grand = stats.total_tokens();
    let comp_bar = width
        .saturating_sub(KEY_W)
        .saturating_sub(val_w + 6)
        .clamp(6, 40);
    for (label, value, fill) in [
        ("input", stats.total_input, theme::accent()),
        ("output", stats.total_output, theme::success()),
        ("c.write", stats.total_cache_create, theme::warning()),
        ("c.read", stats.total_cache_read, theme::info()),
    ] {
        let pct = if grand == 0 {
            0.0
        } else {
            value as f64 / grand as f64 * 100.0
        };
        let mut spans = vec![key(label)];
        spans.extend(hbar(value, grand, comp_bar, fill));
        spans.push(Span::styled(
            format!(" {} {:>3.0}%", fmt_count(value), pct),
            theme::dim(),
        ));
        lines.push(Line::from(spans));
    }

    // ── activity ──
    push_blank(&mut lines);
    let msgs: Vec<u64> = trail(&stats.activity, width)
        .iter()
        .map(|a| a.messages)
        .collect();
    if !msgs.is_empty() {
        lines.push(eyebrow(&format!("ACTIVITY · {} days", msgs.len())));
        lines.push(Line::from(Span::styled(sparkline(&msgs), theme::accent())));
        let peak_msgs = stats.activity.iter().map(|a| a.messages).max().unwrap_or(0);
        let peak_sess = stats.activity.iter().map(|a| a.sessions).max().unwrap_or(0);
        let tool_calls: u64 = stats.activity.iter().map(|a| a.tool_calls).sum();
        lines.push(Line::from(Span::styled(
            format!(
                "messages/day · peak {} msgs, {} sessions · {} tool calls total",
                fmt_count(peak_msgs),
                peak_sess,
                fmt_count(tool_calls)
            ),
            theme::faint(),
        )));
    }

    lines
}

/// Last `width`-bounded window of a chronological slice (keeps the recent tail).
fn trail<T>(items: &[T], width: usize) -> &[T] {
    let n = items.len().min(width.saturating_sub(2).max(1));
    &items[items.len().saturating_sub(n)..]
}

fn busiest_hour(hours: &[u64; 24]) -> Option<usize> {
    let (hour, &count) = hours.iter().enumerate().max_by_key(|&(_, c)| *c)?;
    (count > 0).then_some(hour)
}

fn push_blank(lines: &mut Vec<Line<'static>>) {
    lines.push(Line::from(""));
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
        let hint = Paragraph::new(Line::from(Span::styled("no model data", theme::faint())))
            .style(theme::base());
        frame.render_widget(hint, inner);
        return;
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    let kv = |label: &str, value: String| {
        Line::from(vec![key(label), Span::styled(value, theme::body())])
    };
    lines.push(kv("input", fmt_count(m.input)));
    lines.push(kv("output", fmt_count(m.output)));
    lines.push(kv("c.read", fmt_count(m.cache_read)));
    lines.push(kv("c.write", fmt_count(m.cache_create)));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        key("total"),
        Span::styled(
            fmt_count(m.total()),
            theme::body().add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(kv("io", fmt_count(m.in_out())));

    let bar_w = (inner.width as usize).saturating_sub(14).clamp(6, 36);

    lines.push(Line::from(""));
    lines.push(eyebrow("SHARE OF ALL TOKENS"));
    let share = if grand == 0 {
        0.0
    } else {
        m.total() as f64 / grand as f64 * 100.0
    };
    let mut share_line = hbar(m.total(), grand, bar_w, theme::accent());
    share_line.push(Span::styled(format!(" {share:>4.1}%"), theme::dim()));
    lines.push(Line::from(share_line));

    lines.push(Line::from(""));
    lines.push(eyebrow("CACHE HIT"));
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
