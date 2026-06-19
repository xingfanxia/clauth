//! Usage tab — account picker on the left, the selected account's full usage
//! breakdown on the right: a header (plan, active marker, per-account refresh
//! status / countdown), then every window, reset timers, and extra credits.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::App;
use super::super::theme;
use super::format::{activity_verb, format_reset, reset_in_secs, spinner_frame, spinner_style};
use super::panes::{
    SELECTOR_WIDTH, active_dot, draw_profile_selector, section_box, section_box_verbatim,
};
use crate::format::plan_label;
use crate::profile::Profile;
use crate::providers::StatRowKind;
use crate::usage::{FetchStatus, ProfileActivity, ideal_pace_pct, now_epoch_secs, now_ms};

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

    let show_estimates = cfg.state.show_estimates;
    let show_pace = cfg.state.show_pace;
    let lines = build_usage_lines(
        profile,
        inner.width,
        &header,
        app,
        show_estimates,
        show_pace,
    );
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

fn build_usage_lines(
    profile: &Profile,
    inner_w: u16,
    header: &HeaderState,
    app: &App,
    show_estimates: bool,
    show_pace: bool,
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
        lines.push(Line::from(Span::styled("  loading", theme::faint())));
        return lines;
    }

    let mut stats = collect_stats(profile);
    if stats.is_empty() {
        lines.push(Line::from(Span::styled("  loading", theme::faint())));
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

    let history = app
        .history_cache
        .get(profile.name.as_str())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    // 5-hour windows get the recency-weighted recent-burn rate (%/h) from
    // history. 7-day windows instead use a window-anchored average pace (%/d)
    // derived from `resets_at` in `collect_stats` — rotation-proof, where a
    // history slope would jump on every account rotation.
    let mut window_rates: HashMap<String, Option<f64>> = HashMap::new();
    if let Some(u) = profile.usage.as_ref() {
        let five_h: Vec<_> = u
            .windows()
            .into_iter()
            .filter(|(l, _)| !l.starts_with("7d"))
            .collect();
        if !five_h.is_empty() {
            window_rates.extend(crate::usage::compute_burn_rates_from_history(
                history,
                &five_h,
                60 * 60 * 1000, // lookback_ms: last 1h of samples for %/h
                3,              // min_samples before a rate is shown
                10 * 60 * 1000, // gap_cut_ms: cut idle gaps for short-horizon windows
            ));
        }
    }

    // Fill the 5h recent-burn rate; the 7d average pace is already set by
    // collect_stats.
    for stat in &mut stats {
        if !stat.label.starts_with("7d") {
            stat.burn_rate = window_rates.get(&stat.label).and_then(|r| *r);
        }
    }

    if !show_estimates {
        for stat in &mut stats {
            stat.burn_rate = None;
        }
    }
    if !show_pace {
        for stat in &mut stats {
            stat.pace_pct = None;
        }
    }

    let max_rate_w = stats
        .iter()
        .map(|s| s.rate_section_width())
        .max()
        .unwrap_or(0);

    for (i, stat) in stats.into_iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(stat.render(bar_width, pct_col, max_rate_w));
    }
    lines
}

struct Stat {
    label: String,
    pct: f64,
    color: Style,
    trailing: String,
    burn_rate: Option<f64>,
    rate_unit: &'static str,
    /// Ideal-pace marker position as a percentage (0..=100), or `None` to draw
    /// no marker. Gated by `AppState.show_pace`; computed in [`collect_stats`].
    pace_pct: Option<f64>,
    /// Seconds until this window resets, for the burn-ETA color cue. `None` when
    /// the window carries no reset stamp (e.g. extra credits).
    reset_secs: Option<i64>,
}

/// Seconds until the window hits 100% at the current burn rate.
/// `rate` may be in %/h or %/d (determined by `rate_unit`).
fn eta_left_secs(rate: f64, pct: f64, rate_unit: &str) -> Option<i64> {
    if rate <= 0.0 || pct >= 100.0 {
        return None;
    }
    let rate_per_h = if rate_unit == "d" { rate / 24.0 } else { rate };
    let hours = (100.0 - pct) / rate_per_h;
    let secs = (hours * 3600.0) as i64;
    (secs > 0).then_some(secs)
}

fn eta_left(rate: f64, pct: f64, rate_unit: &str) -> Option<String> {
    eta_left_secs(rate, pct, rate_unit).map(crate::usage::humanize_duration)
}

impl Stat {
    /// Width of the ` · rate` + optional ` · X left` section, for alignment.
    fn rate_section_width(&self) -> usize {
        let Some(rate) = self.burn_rate.filter(|r| *r > 0.0) else {
            return 0;
        };
        let mut w =
            " · ".chars().count() + format!("{:.1} %/{}", rate, self.rate_unit).chars().count();
        if let Some(dur) = eta_left(rate, self.pct, self.rate_unit) {
            w += " · ".chars().count() + dur.chars().count() + " left".chars().count();
        }
        w
    }

    /// Eyebrow + right-aligned %, then bar with trailing reset/credit suffix.
    /// `bar_width` shared across rows; `pct_col` = far content edge for % alignment.
    /// `max_rate_w` keeps the % column fixed across all rows regardless of per-row rate.
    fn render(&self, bar_width: usize, pct_col: usize, max_rate_w: usize) -> Vec<Line<'static>> {
        let pct_str = format!("{:>3.0}%", self.pct);

        let header_pad = pct_col
            .saturating_sub(self.label.chars().count())
            .saturating_sub(max_rate_w)
            .saturating_sub(pct_str.chars().count());

        let filled = (((self.pct / 100.0) * bar_width as f64).round() as usize).min(bar_width);
        let marker = self.pace_pct.filter(|_| bar_width > 0).map(|p| {
            (((p.clamp(0.0, 100.0) / 100.0) * bar_width as f64).round() as usize).min(bar_width - 1)
        });

        let mut bar_line = bar_spans(filled, bar_width, self.color, marker);
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

        let mut label_spans = vec![Span::styled(self.label.clone(), theme::label())];
        if let Some(rate) = self.burn_rate
            && rate > 0.0
        {
            let rate_color = Style::default().fg(theme::util_color(rate.clamp(0.0, 100.0)));
            label_spans.push(Span::styled(" · ", theme::dim()));
            let rate_str = format!("{:.1} %/{}", rate, self.rate_unit);
            label_spans.push(Span::styled(rate_str.clone(), rate_color));

            if let Some(eta_secs) = eta_left_secs(rate, self.pct, self.rate_unit) {
                let dur = crate::usage::humanize_duration(eta_secs);
                // Warn when the window hits 100% before it resets: you run dry
                // before the limit refreshes. Faint when the reset lands first.
                let runs_dry_first = self.reset_secs.is_some_and(|r| eta_secs < r);
                let style = if runs_dry_first {
                    theme::warning()
                } else {
                    theme::faint()
                };
                label_spans.push(Span::styled(" · ", theme::faint()));
                label_spans.push(Span::styled(format!("{dur} left"), style));
            }

            let my_rate_w = self.rate_section_width();
            let extra = max_rate_w.saturating_sub(my_rate_w);
            if extra > 0 {
                label_spans.push(Span::raw(" ".repeat(extra)));
            }
        } else {
            label_spans.push(Span::raw(" ".repeat(max_rate_w)));
        }

        vec![
            Line::from({
                let mut spans = label_spans;
                spans.push(Span::raw(" ".repeat(header_pad)));
                spans.push(Span::styled(
                    pct_str,
                    self.color.add_modifier(Modifier::BOLD),
                ));
                spans
            }),
            Line::from(bar_line),
        ]
    }
}

fn collect_stats(profile: &Profile) -> Vec<Stat> {
    let Some(usage) = profile.usage.as_ref() else {
        return Vec::new();
    };
    let now_secs = now_epoch_secs();
    let mut stats: Vec<Stat> = Vec::new();
    for (label, w) in usage.windows() {
        let pct = w.utilization.clamp(0.0, 100.0);
        let trailing = format_reset(w)
            .map(|r| format!("  resets in {r}"))
            .unwrap_or_default();
        let rate_unit = if label.starts_with("7d") { "d" } else { "h" };
        // 7d windows show a window-anchored average pace (%/d) — util spread
        // over the time elapsed since the weekly reset, immune to rotation.
        // The 5h recent-burn rate is filled later from history.
        let burn_rate = if label.starts_with("7d") {
            crate::usage::window_avg_pace_per_day(label, w, now_secs, 60 * 60)
        } else {
            None
        };
        stats.push(Stat {
            label: label.to_string(),
            pct,
            color: Style::default().fg(theme::util_color(pct)),
            trailing,
            burn_rate,
            rate_unit,
            pace_pct: ideal_pace_pct(label, w, now_secs),
            reset_secs: reset_in_secs(w),
        });
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
            burn_rate: None,
            rate_unit: "h",
            pace_pct: None,
            reset_secs: None,
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

/// The usage bar: `filled` █ cells in `fill`, the rest ░, with an optional `│`
/// ideal-pace marker at `marker_col`. The marker reads WARNING once the fill has
/// passed it (usage running ahead of an even spread) and faint while the fill is
/// still behind it. Drawn over the bar so a wide fill never hides it — the
/// horizontal twin of `chain::gauge_with_tick`.
fn bar_spans(
    filled: usize,
    bar_width: usize,
    fill: Style,
    marker_col: Option<usize>,
) -> Vec<Span<'static>> {
    let empty = bar_width - filled;
    let Some(m) = marker_col.filter(|&m| m < bar_width) else {
        return vec![
            Span::styled("█".repeat(filled), fill),
            Span::styled("░".repeat(empty), theme::line_strong()),
        ];
    };

    // Emit each run only when non-empty so the marker splits the bar cleanly.
    let run =
        |glyph: &str, n: usize, style: Style| (n > 0).then(|| Span::styled(glyph.repeat(n), style));
    let mut spans = Vec::with_capacity(4);
    if m < filled {
        spans.extend(run("█", m, fill));
        spans.push(Span::styled("│".to_string(), theme::warning()));
        spans.extend(run("█", filled - m - 1, fill));
        spans.extend(run("░", empty, theme::line_strong()));
    } else {
        spans.extend(run("█", filled, fill));
        spans.extend(run("░", m - filled, theme::line_strong()));
        spans.push(Span::styled("│".to_string(), theme::dim()));
        spans.extend(run("░", bar_width - m - 1, theme::line_strong()));
    }
    spans
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
            Span::styled(format!("{frame} {verb}"), spinner_style(header.activity)),
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
        lines.push(Line::from(Span::styled("loading", theme::faint())));
        return lines;
    };

    if stats.rows.is_empty() {
        let (msg, style) = if stats.is_available {
            ("no stats reported", theme::faint())
        } else {
            ("usage unavailable", theme::danger())
        };
        lines.push(Line::from(Span::styled(msg, style)));
        return lines;
    }

    for row in &stats.rows {
        if row.label.is_empty() {
            let style = match row.kind {
                StatRowKind::Danger => theme::danger(),
                _ => theme::faint(),
            };
            lines.push(Line::from(Span::styled(row.value.to_string(), style)));
        } else if row.kind == StatRowKind::Heading {
            lines.push(Line::from(Span::styled(
                row.label.to_string(),
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
    let mut spans = vec![Span::styled(format!("  {key}"), theme::faint())];
    let pad = TP_KEY_W.saturating_sub(key.chars().count()).max(1);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(value.to_string(), value_style));
    spans
}

fn key_span(key: &str) -> Span<'static> {
    let pad = KEY_W.saturating_sub(key.chars().count()).max(1);
    Span::styled(format!("{key}{}", " ".repeat(pad)), theme::label())
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_usage.rs"]
mod tests;
