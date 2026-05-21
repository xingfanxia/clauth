//! Profile detail screen — usage breakdown + fallback metadata for one profile.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use super::super::app::App;
use super::super::theme;
use super::format::format_reset;
use crate::fallback::threshold_for;
use crate::profile::Profile;
use crate::usage::UsageWindow;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, profile_index: usize) {
    let Some(profile) = app.config.profiles.get(profile_index) else {
        return;
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::LINE))
        .title(Line::from(Span::styled(" PROFILE ", theme::label())))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let inner_w = inner.width;

    let usage_lines = build_usage_lines(profile, inner_w);
    // +1 for the separator below.
    let usage_height = usage_lines.len() as u16 + 2;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(usage_height), Constraint::Min(1)])
        .split(inner);

    let mut top_with_sep = usage_lines;
    top_with_sep.push(Line::from(""));
    top_with_sep.push(detail_separator(inner_w));
    frame.render_widget(Paragraph::new(top_with_sep).style(theme::base()), chunks[0]);

    // Side-by-side: CONFIG on the left, FALLBACK on the right. Narrow
    // terminals will clip the right column — accepted trade-off so the two
    // sections share a row instead of stacking.
    let bottom = chunks[1];
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(bottom);

    frame.render_widget(
        Paragraph::new(build_config_lines(profile)).style(theme::base()),
        cols[0],
    );
    frame.render_widget(
        Paragraph::new(build_fallback_lines(app, profile)).style(theme::base()),
        cols[1],
    );
}

fn build_usage_lines(profile: &Profile, inner_w: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(detail_section_header("USAGE"));
    lines.push(Line::from(""));
    match profile.usage.as_ref() {
        None => {
            lines.push(Line::from(Span::styled("  loading…", theme::faint())));
        }
        Some(usage) => {
            let windows: &[(&str, Option<&UsageWindow>)] = &[
                ("5h", usage.five_hour.as_ref()),
                ("7d all", usage.seven_day.as_ref()),
                ("7d sonnet", usage.seven_day_sonnet.as_ref()),
                ("7d opus", usage.seven_day_opus.as_ref()),
            ];
            let mut blocks: Vec<Vec<Line<'static>>> = Vec::new();
            for (label, w) in windows {
                if let Some(w) = w {
                    blocks.push(detail_bar_block(label, w, inner_w));
                }
            }
            if let Some(extra) = &usage.extra_usage
                && extra.is_enabled
            {
                blocks.push(detail_extra_block(extra, inner_w));
            }
            if blocks.is_empty() {
                lines.push(Line::from(Span::styled("  loading…", theme::faint())));
            } else {
                for (i, block) in blocks.into_iter().enumerate() {
                    if i > 0 {
                        lines.push(Line::from(""));
                    }
                    lines.extend(block);
                }
            }
        }
    }
    lines
}

fn build_config_lines(profile: &Profile) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(detail_section_header("CONFIG"));
    lines.push(Line::from(""));
    if !profile.is_oauth() {
        lines.push(detail_kv("auto-start usage", "n/a", theme::faint()));
        return lines;
    }
    let (auto_state, auto_style) = if profile.auto_start {
        ("on", theme::accent())
    } else {
        ("off", theme::faint())
    };
    lines.push(detail_kv("auto-start usage", auto_state, auto_style));
    lines
}

fn build_fallback_lines(app: &App, profile: &Profile) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(detail_section_header("FALLBACK"));
    lines.push(Line::from(""));
    let chain_pos = app
        .config
        .state
        .fallback_chain
        .iter()
        .position(|n| n == &profile.name);
    match chain_pos {
        None => {
            lines.push(detail_kv("position", "not in chain", theme::faint()));
        }
        Some(pos) => {
            let total = app.config.state.fallback_chain.len();
            lines.push(detail_kv(
                "position",
                &format!("{} of {total} in chain", pos + 1),
                theme::muted(),
            ));
            let threshold = threshold_for(profile);
            lines.push(detail_kv(
                "threshold",
                &format!("{threshold:.0}%"),
                theme::muted(),
            ));
        }
    }
    lines
}

fn detail_section_header(label: &'static str) -> Line<'static> {
    Line::from(Span::styled(label, theme::label()))
}

fn detail_kv(key: &str, value: &str, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {key:<12}", key = key), theme::faint()),
        Span::styled(value.to_string(), value_style),
    ])
}

/// Full-width `─` separator in LINE color. `width` is the inner area width.
fn detail_separator(width: u16) -> Line<'static> {
    let bar = "─".repeat(width as usize);
    Line::from(Span::styled(bar, Style::default().fg(theme::LINE)))
}

/// Two-line stat block: eyebrow label + right-aligned %, then full-width bar
/// with the reset suffix trailing.
fn detail_bar_block(label: &str, window: &UsageWindow, inner_w: u16) -> Vec<Line<'static>> {
    let pct = window.utilization.clamp(0.0, 100.0);
    let color = theme::util_color(pct);
    let reset_suffix = format_reset(window)
        .map(|r| format!("  resets in {r}"))
        .unwrap_or_default();
    stat_block(label, pct, color, &reset_suffix, inner_w)
}

fn detail_extra_block(extra: &crate::usage::ExtraUsage, inner_w: u16) -> Vec<Line<'static>> {
    let pct = extra.utilization.unwrap_or(0.0).clamp(0.0, 100.0);
    let color = theme::util_color(pct);
    let currency_sym = match extra.currency.as_deref() {
        Some("USD") | None => "$",
        Some(other) => other,
    };
    let used = extra.used_credits.unwrap_or(0.0);
    let limit = extra.monthly_limit.unwrap_or(0.0);
    let money = format!("  {currency_sym}{used:.2} / {currency_sym}{limit:.2}");
    stat_block("extra", pct, color, &money, inner_w)
}

fn stat_block(
    label: &str,
    pct: f64,
    color: Color,
    trailing: &str,
    inner_w: u16,
) -> Vec<Line<'static>> {
    let label_upper = label.to_uppercase();
    let label_span = format!("  {label_upper}");
    let pct_str = format!("{pct:>3.0}%");
    let header_pad = (inner_w as usize)
        .saturating_sub(label_span.chars().count())
        .saturating_sub(pct_str.chars().count());

    let trailing_len = trailing.chars().count();
    let bar_width = (inner_w as usize)
        .saturating_sub(2)
        .saturating_sub(trailing_len)
        .max(10);
    let filled = ((pct / 100.0) * bar_width as f64).round() as usize;
    let filled = filled.min(bar_width);
    let empty = bar_width - filled;

    vec![
        Line::from(vec![
            Span::styled(label_span, theme::label()),
            Span::raw(" ".repeat(header_pad)),
            Span::styled(
                pct_str,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("█".repeat(filled), Style::default().fg(color)),
            Span::styled("░".repeat(empty), Style::default().fg(theme::LINE_STRONG)),
            Span::styled(trailing.to_string(), theme::faint()),
        ]),
    ]
}
