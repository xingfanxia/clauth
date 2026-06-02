//! Usage tab — account picker on the left, the selected account's full usage
//! breakdown (every window, reset timers, extra credits) on the right.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::App;
use super::super::theme;
use super::format::format_reset;
use super::panes::{SELECTOR_WIDTH, draw_profile_selector, section_box};
use crate::format::plan_label;
use crate::profile::Profile;
use crate::usage::UsageWindow;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SELECTOR_WIDTH), Constraint::Min(20)])
        .split(area);

    draw_profile_selector(frame, cols[0], app, app.usage_cursor, true);
    draw_usage_detail(frame, cols[1], app);
}

fn draw_usage_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cfg = app.config();
    let profile = cfg
        .profiles
        .get(app.usage_cursor.min(cfg.profiles.len().saturating_sub(1)));

    let title = profile.map(|p| p.name.as_str()).unwrap_or("usage");
    let block = section_box(title, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(profile) = profile else {
        let hint = Paragraph::new(Line::from(Span::styled(
            "no accounts yet — press n to create one",
            theme::muted(),
        )))
        .style(theme::base());
        frame.render_widget(hint, inner);
        return;
    };

    let lines = build_usage_lines(profile, inner.width);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

fn build_usage_lines(profile: &Profile, inner_w: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(meta_line(profile));
    lines.push(Line::from(""));

    if !profile.is_oauth() {
        lines.push(Line::from(Span::styled(
            "API endpoint profile — no usage windows.",
            theme::faint(),
        )));
        return lines;
    }

    match profile.usage.as_ref() {
        None => lines.push(Line::from(Span::styled("  loading…", theme::faint()))),
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
                    blocks.push(bar_block(label, w, inner_w));
                }
            }
            if let Some(extra) = &usage.extra_usage
                && extra.is_enabled
            {
                blocks.push(extra_block(extra, inner_w));
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

/// One-line summary above the bars: the account's plan.
fn meta_line(profile: &Profile) -> Line<'static> {
    let plan = profile
        .usage
        .as_ref()
        .and_then(|u| u.plan.as_ref())
        .map(plan_label)
        .unwrap_or_else(|| {
            if profile.is_oauth() {
                "oauth".into()
            } else {
                "api".into()
            }
        });
    Line::from(vec![
        Span::styled("plan  ", theme::faint()),
        Span::styled(plan, theme::muted()),
    ])
}

/// Two-line stat block: eyebrow + right-aligned %, then a full-width bar with a
/// trailing reset/credit suffix.
fn bar_block(label: &str, window: &UsageWindow, inner_w: u16) -> Vec<Line<'static>> {
    let pct = window.utilization.clamp(0.0, 100.0);
    let color = theme::util_color(pct);
    let reset_suffix = format_reset(window)
        .map(|r| format!("  resets in {r}"))
        .unwrap_or_default();
    stat_block(label, pct, color, &reset_suffix, inner_w)
}

fn extra_block(extra: &crate::usage::ExtraUsage, inner_w: u16) -> Vec<Line<'static>> {
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
    let label_span = label.to_string();
    let pct_str = format!("{pct:>3.0}%");
    let header_pad = (inner_w as usize)
        .saturating_sub(label_span.chars().count())
        .saturating_sub(pct_str.chars().count());

    let trailing_len = trailing.chars().count();
    let bar_width = (inner_w as usize).saturating_sub(trailing_len).max(10);
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
            Span::styled("█".repeat(filled), Style::default().fg(color)),
            Span::styled("░".repeat(empty), Style::default().fg(theme::LINE_STRONG)),
            Span::styled(trailing.to_string(), theme::faint()),
        ]),
    ]
}
