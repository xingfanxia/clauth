//! Usage tab — account picker on the left, the selected account's full usage
//! breakdown on the right: a header (plan, active marker, per-account refresh
//! status / countdown), then every window, reset timers, and extra credits.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::App;
use super::super::theme;
use super::format::{activity_verb, format_reset, spinner_frame, spinner_style};
use super::panes::{
    SELECTOR_WIDTH, active_dot, draw_profile_selector, section_box, section_box_verbatim,
};
use crate::format::plan_label;
use crate::profile::Profile;
use crate::providers::StatRowKind;
use crate::usage::{FetchStatus, ProfileActivity, UsageWindow, now_ms};

const KEY_W: usize = 8;

/// Runtime state gathered once under locks; keeps line builders lock-free.
struct HeaderState {
    is_active: bool,
    activity: ProfileActivity,
    next_refresh_ms: Option<u64>,
    tick: u64,
}

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SELECTOR_WIDTH), Constraint::Min(20)])
        .split(area);

    draw_profile_selector(frame, cols[0], app, app.profile_cursor, true);
    draw_usage_detail(frame, cols[1], app);
}

fn draw_usage_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cfg = app.config();
    let profile = cfg
        .profiles
        .get(app.profile_cursor.min(cfg.profiles.len().saturating_sub(1)));

    let title = profile.map(|p| p.name.as_str()).unwrap_or("usage");
    // Detail pane: read-only, focus never descends into it; second panel on screen.
    // Profile names preserve original case; the "usage" fallback stays uppercased.
    let block = if profile.is_some() {
        section_box_verbatim(title, false, false)
    } else {
        section_box(title, false, false)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(profile) = profile else {
        let hint = Paragraph::new(Line::from(Span::styled(
            "no accounts yet — press n to create one",
            theme::dim(),
        )))
        .style(theme::base());
        frame.render_widget(hint, inner);
        return;
    };

    // `config` (via `cfg`) is outer of activity/refresh-timer in lock order.
    let header = HeaderState {
        is_active: cfg.is_active(&profile.name),
        activity: app
            .activity
            .lock()
            .ok()
            .and_then(|g| g.get(profile.name.as_str()).copied())
            .unwrap_or(ProfileActivity::Idle),
        next_refresh_ms: app
            .next_refresh_per_profile
            .lock()
            .ok()
            .and_then(|m| m.get(profile.name.as_str()).copied()),
        tick: app.tick_count,
    };

    let lines = build_usage_lines(profile, inner.width, &header, app);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

fn build_usage_lines(
    profile: &Profile,
    inner_w: u16,
    header: &HeaderState,
    app: &App,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.extend(header_lines(profile, inner_w, header));
    lines.push(Line::from(""));

    if profile.is_third_party() {
        lines.extend(build_tp_rows(profile, header));
        return lines;
    }

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

    let max_trailing = stats
        .iter()
        .map(|s| s.trailing.chars().count())
        .max()
        .unwrap_or(0);
    let bar_width = bar_width_for(inner_w, max_trailing);
    // Right-align % to far content edge so figures stack above the reset text.
    let pct_col = (bar_width + max_trailing).min(inner_w as usize);
    for (i, stat) in stats.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(stat.render(bar_width, pct_col));
        // Insert burn rate after the 5h window's bar (first stat, 2 lines: label + bar).
        if i == 0
            && let Some(samples) = app.burn_samples.get(profile.name.as_str())
            && samples.len() >= 2
        {
            let first = samples.front().unwrap();
            let last = samples.back().unwrap();
            let dt_hours = last.0.duration_since(first.0).as_secs_f64() / 3600.0;
            if dt_hours > 0.0 {
                let dpct = last.1 - first.1;
                let rate = dpct / dt_hours;
                let color = Style::default().fg(theme::util_color(rate.clamp(0.0, 100.0)));
                let line = Line::from(vec![
                    Span::styled("  burn ", theme::label()),
                    Span::styled(format!("{:>5.1} %/h", rate), color),
                ]);
                lines.push(line);
            }
        }
    }
    lines
}

struct Stat {
    label: String,
    pct: f64,
    color: Style,
    trailing: String,
}

impl Stat {
    /// Eyebrow + right-aligned %, then bar with trailing reset/credit suffix.
    /// `bar_width` shared across rows; `pct_col` = far content edge for % alignment.
    fn render(&self, bar_width: usize, pct_col: usize) -> Vec<Line<'static>> {
        let pct_str = format!("{:>3.0}%", self.pct);
        let header_pad = pct_col
            .saturating_sub(self.label.chars().count())
            .saturating_sub(pct_str.chars().count());

        let filled = ((self.pct / 100.0) * bar_width as f64).round() as usize;
        let filled = filled.min(bar_width);
        let empty = bar_width - filled;

        let mut bar_line = vec![
            Span::styled("█".repeat(filled), self.color),
            Span::styled("░".repeat(empty), theme::line_strong()),
        ];
        // Right-align trailing text to the same far column as %.
        if !self.trailing.is_empty() {
            let pad = pct_col
                .saturating_sub(bar_width)
                .saturating_sub(self.trailing.chars().count());
            if pad > 0 {
                bar_line.push(Span::raw(" ".repeat(pad)));
            }
            bar_line.push(Span::styled(self.trailing.clone(), theme::faint()));
        }

        vec![
            Line::from(vec![
                Span::styled(self.label.clone(), theme::label()),
                Span::raw(" ".repeat(header_pad)),
                Span::styled(pct_str, self.color.add_modifier(Modifier::BOLD)),
            ]),
            Line::from(bar_line),
        ]
    }
}

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
                color: Style::default().fg(theme::util_color(pct)),
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
            color: Style::default().fg(theme::util_color(pct)),
            trailing: format!("  {sym}{used:.2} / {sym}{limit:.2}"),
        });
    }
    stats
}

/// As wide as possible while leaving room for the longest trailing suffix.
fn bar_width_for(inner_w: u16, max_trailing: usize) -> usize {
    let avail = (inner_w as usize).saturating_sub(max_trailing);
    if avail >= 10 {
        avail
    } else {
        // Suffix nearly fills the line — keep what's left rather than forcing a
        // 10-cell bar that pushes the suffix off the edge.
        avail.max(1)
    }
}

fn header_lines(profile: &Profile, inner_w: u16, header: &HeaderState) -> Vec<Line<'static>> {
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
    let mut plan_spans = vec![key_span("plan"), Span::styled(plan.clone(), theme::body())];
    if header.is_active {
        // "● active" = 8 chars; left side = KEY_W + plan chars; pad the gap.
        let left_w = KEY_W + plan.chars().count();
        let indicator_w = "● active".chars().count(); // 8
        let pad = (inner_w as usize)
            .saturating_sub(left_w)
            .saturating_sub(indicator_w);
        plan_spans.push(Span::raw(" ".repeat(pad)));
        plan_spans.extend(active_dot());
    }

    let mut lines = vec![Line::from(plan_spans)];
    if profile.is_oauth() || profile.is_third_party() {
        lines.push(status_line(profile, header));
    }
    lines
}

fn status_line(profile: &Profile, header: &HeaderState) -> Line<'static> {
    let key = key_span("status");

    if !matches!(header.activity, ProfileActivity::Idle) {
        let frame = spinner_frame(header.tick);
        let verb = activity_verb(header.activity);
        return Line::from(vec![
            key,
            Span::styled(format!("{frame} {verb}…"), spinner_style(header.activity)),
        ]);
    }

    let countdown = header.next_refresh_ms.map(|next| {
        let secs = ((next as i64 - now_ms() as i64) / 1000).max(0);
        format!("{secs}s")
    });

    let mut spans = vec![key];
    match profile.fetch_status {
        Some(FetchStatus::Failed) => {
            spans.extend([
                Span::styled("[ ", theme::dim()),
                Span::styled("failed", theme::danger().add_modifier(Modifier::BOLD)),
                Span::styled(" ]", theme::dim()),
            ]);
            if let Some(c) = countdown {
                spans.push(Span::styled(format!("  · retry in {c}"), theme::faint()));
            }
        }
        Some(FetchStatus::Cached) => {
            spans.extend([
                Span::styled("[ ", theme::dim()),
                Span::styled("cached", theme::warning().add_modifier(Modifier::BOLD)),
                Span::styled(" ]", theme::dim()),
            ]);
            if let Some(c) = countdown {
                spans.push(Span::styled(format!("  · refresh in {c}"), theme::faint()));
            }
        }
        Some(FetchStatus::RateLimited) => {
            spans.extend([
                Span::styled("[ ", theme::dim()),
                Span::styled("rate limited", theme::danger().add_modifier(Modifier::BOLD)),
                Span::styled(" ]", theme::dim()),
            ]);
            if let Some(c) = countdown {
                spans.push(Span::styled(format!("  · retry in {c}"), theme::faint()));
            }
        }
        _ => match countdown {
            Some(c) => spans.push(Span::styled(format!("↻ refresh in {c}"), theme::faint())),
            None => spans.push(Span::styled("↻ up to date", theme::faint())),
        },
    }
    Line::from(spans)
}

/// Render provider-agnostic third-party stats. The header (plan + status) was
/// already pushed by the caller; only the stats body goes here.
fn build_tp_rows(profile: &Profile, _header: &HeaderState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let Some(stats) = profile.third_party_usage.as_ref() else {
        lines.push(Line::from(Span::styled("  loading…", theme::faint())));
        return lines;
    };

    if stats.rows.is_empty() {
        // `unavailable()` always carries a row, so an empty list means an
        // available account whose provider reported no stats.
        let (msg, style) = if stats.is_available {
            ("  no stats reported", theme::faint())
        } else {
            ("  usage unavailable", theme::danger())
        };
        lines.push(Line::from(Span::styled(msg, style)));
        return lines;
    }

    for row in &stats.rows {
        if row.label.is_empty() {
            // Single-line message (e.g. danger / faint).
            let style = match row.kind {
                StatRowKind::Danger => theme::danger(),
                _ => theme::faint(),
            };
            lines.push(Line::from(Span::styled(format!("  {}", row.value), style)));
        } else if row.kind == StatRowKind::Heading {
            lines.push(Line::from(Span::styled(
                format!("  {}", row.label),
                theme::label(),
            )));
        } else {
            let style = match row.kind {
                StatRowKind::Danger => theme::danger(),
                StatRowKind::Faint => theme::faint(),
                _ => theme::body(),
            };
            lines.push(Line::from(key_value_span(&row.label, &row.value, style)));
        }
    }

    lines
}

/// Key column width for third-party stat rows (wider than `KEY_W` to fit
/// labels like "topped up" plus a 1-space gap).
const TP_KEY_W: usize = 10;

fn key_value_span(key: &str, value: &str, value_style: Style) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(format!("    {key}"), theme::faint())];
    let pad = TP_KEY_W.saturating_sub(key.chars().count()).max(1);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(value.to_string(), value_style));
    spans
}

fn key_span(key: &str) -> Span<'static> {
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    Span::styled(format!("{key}{}", " ".repeat(pad)), theme::label())
}
