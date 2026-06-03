//! Usage tab — account picker on the left, the selected account's full usage
//! breakdown on the right: a header (plan, active marker, per-account refresh
//! status / countdown), then every window, reset timers, and extra credits.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::App;
use super::super::theme;
use super::format::{activity_verb, format_reset, spinner_frame, spinner_style};
use super::panes::{SELECTOR_WIDTH, draw_profile_selector, section_box};
use crate::format::plan_label;
use crate::profile::Profile;
use crate::usage::{FetchStatus, ProfileActivity, UsageWindow, now_ms};

/// Key column width for the header rows (`plan`, `status`).
const KEY_W: usize = 8;

/// Per-account runtime state shown in the usage detail header — gathered once
/// under the locks in `draw_usage_detail` so the line builders stay lock-free.
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

    // Per-account runtime state, read under the activity / refresh-timer locks.
    // `config` (held via `cfg`) is outer of both in the global lock order, so
    // taking them here is safe.
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

    let lines = build_usage_lines(profile, inner.width, &header);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

fn build_usage_lines(profile: &Profile, inner_w: u16, header: &HeaderState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.extend(header_lines(profile, header));
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

    let max_trailing = stats
        .iter()
        .map(|s| s.trailing.chars().count())
        .max()
        .unwrap_or(0);
    let bar_width = bar_width_for(inner_w, max_trailing);
    // Right-align every % to the far edge of the content so the figures stack
    // in one column above the trailing "resets in …" text rather than hugging
    // the bar's right end.
    let pct_col = (bar_width + max_trailing).min(inner_w as usize);
    for (i, stat) in stats.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(stat.render(bar_width, pct_col));
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
    /// rows so every bar lines up at the same length; `pct_col` is the column
    /// the % right-aligns to (the content's far edge, above the reset text).
    fn render(&self, bar_width: usize, pct_col: usize) -> Vec<Line<'static>> {
        let pct_str = format!("{:>3.0}%", self.pct);
        let header_pad = pct_col
            .saturating_sub(self.label.chars().count())
            .saturating_sub(pct_str.chars().count());

        let filled = ((self.pct / 100.0) * bar_width as f64).round() as usize;
        let filled = filled.min(bar_width);
        let empty = bar_width - filled;

        let mut bar_line = vec![
            Span::styled("█".repeat(filled), Style::default().fg(self.color)),
            Span::styled("░".repeat(empty), Style::default().fg(theme::LINE_STRONG)),
        ];
        // Right-align the reset/credit text to the same far column as the %, so
        // the bar line ends in a clean right column under it.
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
                Span::styled(
                    pct_str,
                    Style::default().fg(self.color).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(bar_line),
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
fn bar_width_for(inner_w: u16, max_trailing: usize) -> usize {
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

/// Header above the bars: the account's plan (with an `● active` marker when
/// this is the live profile), then a per-account status line carrying the live
/// activity spinner, fetch health, and the countdown to the next auto-refresh.
fn header_lines(profile: &Profile, header: &HeaderState) -> Vec<Line<'static>> {
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
    let mut plan_spans = vec![key_span("plan"), Span::styled(plan, theme::muted())];
    if header.is_active {
        plan_spans.push(Span::raw("   "));
        plan_spans.push(Span::styled("● active", theme::orange()));
    }

    let mut lines = vec![Line::from(plan_spans)];
    // Usage windows (and thus refresh scheduling) only exist for oauth profiles.
    if profile.is_oauth() {
        lines.push(status_line(profile, header));
    }
    lines
}

/// The `status` row: a live spinner while an op is in flight, otherwise the
/// fetch health plus the seconds remaining until the next scheduled refresh.
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
            spans.push(Span::styled("✖ fetch failed", theme::danger()));
            if let Some(c) = countdown {
                spans.push(Span::styled(format!("  · retry in {c}"), theme::faint()));
            }
        }
        Some(FetchStatus::Cached) => {
            spans.push(Span::styled("⚠ cached", theme::warning()));
            if let Some(c) = countdown {
                spans.push(Span::styled(format!("  · refresh in {c}"), theme::faint()));
            }
        }
        Some(FetchStatus::RateLimited) => {
            spans.push(Span::styled("⚠ rate limited", theme::danger()));
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

/// A padded eyebrow key cell for the header rows — same `label` treatment as
/// the usage bars' eyebrows just below, so the whole pane's label column reads
/// in one style.
fn key_span(key: &str) -> Span<'static> {
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    Span::styled(format!("{key}{}", " ".repeat(pad)), theme::label())
}
