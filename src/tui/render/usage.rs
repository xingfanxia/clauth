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

    if profile.usage.is_none() {
        lines.push(Line::from(Span::styled("  loading…", theme::faint())));
        return lines;
    }

    let stats = collect_stats(profile);
    if stats.is_empty() {
        lines.push(Line::from(Span::styled("  loading…", theme::faint())));
        return lines;
    }

    let bar_width = uniform_bar_width(&stats, inner_w);
    for (i, stat) in stats.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(stat.render(bar_width));
    }
    lines
}

/// One renderable usage row: a window or the extra-credit line, reduced to the
/// few values the bar needs.
struct Stat {
    label: String,
    pct: f64,
    color: Color,
    trailing: String,
}

impl Stat {
    /// Two-line block: eyebrow + right-aligned %, then a `bar_width`-cell bar
    /// with the trailing reset/credit suffix. `bar_width` is shared across all
    /// rows so every bar lines up at the same length.
    fn render(&self, bar_width: usize) -> Vec<Line<'static>> {
        let pct_str = format!("{:>3.0}%", self.pct);
        let header_pad = bar_width
            .saturating_sub(self.label.chars().count())
            .saturating_sub(pct_str.chars().count());

        let filled = ((self.pct / 100.0) * bar_width as f64).round() as usize;
        let filled = filled.min(bar_width);
        let empty = bar_width - filled;

        vec![
            Line::from(vec![
                Span::styled(self.label.clone(), theme::label()),
                Span::raw(" ".repeat(header_pad)),
                Span::styled(
                    pct_str,
                    Style::default().fg(self.color).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("█".repeat(filled), Style::default().fg(self.color)),
                Span::styled("░".repeat(empty), Style::default().fg(theme::LINE_STRONG)),
                Span::styled(self.trailing.clone(), theme::faint()),
            ]),
        ]
    }
}

/// Every renderable usage row for the account, in display order.
fn collect_stats(profile: &Profile) -> Vec<Stat> {
    let Some(usage) = profile.usage.as_ref() else {
        return Vec::new();
    };
    let windows: &[(&str, Option<&UsageWindow>)] = &[
        ("5h", usage.five_hour.as_ref()),
        ("7d all", usage.seven_day.as_ref()),
        ("7d sonnet", usage.seven_day_sonnet.as_ref()),
        ("7d opus", usage.seven_day_opus.as_ref()),
    ];
    let mut stats: Vec<Stat> = Vec::new();
    for (label, w) in windows {
        if let Some(w) = w {
            let pct = w.utilization.clamp(0.0, 100.0);
            let trailing = format_reset(w)
                .map(|r| format!("  resets in {r}"))
                .unwrap_or_default();
            stats.push(Stat {
                label: (*label).to_string(),
                pct,
                color: theme::util_color(pct),
                trailing,
            });
        }
    }
    if let Some(extra) = &usage.extra_usage
        && extra.is_enabled
    {
        let pct = extra.utilization.unwrap_or(0.0).clamp(0.0, 100.0);
        let sym = match extra.currency.as_deref() {
            Some("USD") | None => "$",
            Some(other) => other,
        };
        let used = extra.used_credits.unwrap_or(0.0);
        let limit = extra.monthly_limit.unwrap_or(0.0);
        stats.push(Stat {
            label: "extra".to_string(),
            pct,
            color: theme::util_color(pct),
            trailing: format!("  {sym}{used:.2} / {sym}{limit:.2}"),
        });
    }
    stats
}

/// Uniform bar width across all rows: as wide as possible while still leaving
/// room for the longest trailing suffix, so every bar lines up.
fn uniform_bar_width(stats: &[Stat], inner_w: u16) -> usize {
    let max_trailing = stats
        .iter()
        .map(|s| s.trailing.chars().count())
        .max()
        .unwrap_or(0);
    let avail = (inner_w as usize).saturating_sub(max_trailing);
    if avail >= 10 {
        // Comfortable case: a readable bar with room to spare for the suffix.
        avail
    } else {
        // Suffix nearly fills the line — keep whatever room is left rather than
        // forcing a 10-cell bar that would push the suffix off the edge.
        avail.max(1)
    }
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
