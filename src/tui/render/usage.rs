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
use super::format::{activity_verb, format_reset, spinner_frame, spinner_style};
use super::panes::{
    active_pill, draw_profile_selector, section_box, section_box_verbatim, selector_width,
};
use crate::format::plan_label;
use crate::profile::Profile;
use crate::providers::StatRowKind;
use crate::usage::{
    FetchStatus, ProfileActivity, UsageWindow, ideal_pace_pct, now_epoch_secs, now_ms,
};

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
        .constraints([
            Constraint::Length(selector_width(area.width)),
            Constraint::Min(20),
        ])
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

    // Api-key/provider accounts (recognised or generic) render via the third-party
    // rows/bars path; OAuth accounts — including OAuth run against a custom
    // base_url — fall through to their live window bars.
    if profile.api_key.is_some() || profile.is_third_party() {
        lines.extend(build_tp_rows(profile, inner_w, show_estimates, show_pace));
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

    lines.extend(render_stat_block(&stats, inner_w));
    lines
}

/// Render a list of [`Stat`]s as the shared two-line bar blocks (eyebrow + bar),
/// computing the column widths (`max_trailing`/`max_rate_w`/`max_amount_w`) once
/// so the `%` column lines up across rows. A blank line separates rows. Used by
/// both the OAuth window path and the third-party bars path.
fn render_stat_block(stats: &[Stat], inner_w: u16) -> Vec<Line<'static>> {
    let max_trailing = stats
        .iter()
        .map(|s| s.trailing.chars().count())
        .max()
        .unwrap_or(0);
    let bar_width = bar_width_for(inner_w, max_trailing);
    // Right-align % to far content edge so figures stack above the reset text.
    let pct_col = (bar_width + max_trailing).min(inner_w as usize);
    let max_rate_w = stats
        .iter()
        .map(|s| s.rate_section_width())
        .max()
        .unwrap_or(0);
    let max_amount_w = stats
        .iter()
        .map(|s| s.amount.chars().count())
        .max()
        .unwrap_or(0);

    let mut lines = Vec::new();
    for (i, stat) in stats.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(stat.render(bar_width, pct_col, max_rate_w, max_amount_w));
    }
    lines
}

struct Stat {
    label: String,
    pct: f64,
    color: Style,
    trailing: String,
    /// Absolute `used / total` shown on the eyebrow line immediately before the
    /// `%` (right-aligned in its own column). Empty when the window carries no
    /// absolute amounts. Distinct from `trailing`, which sits on the bar line.
    amount: String,
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

    /// Eyebrow (label · rate · `used / total` · %) then bar with trailing reset
    /// suffix. `bar_width` shared across rows; `pct_col` = far content edge for %
    /// alignment. `max_rate_w` / `max_amount_w` keep the rate + amount columns
    /// fixed across all rows so the `%` column never shifts.
    fn render(
        &self,
        bar_width: usize,
        pct_col: usize,
        max_rate_w: usize,
        max_amount_w: usize,
    ) -> Vec<Line<'static>> {
        // Natural width — `header_pad` right-aligns the whole block to `pct_col`,
        // so the `%` lands in the same column every row without padding the
        // number. (A fixed `{:>3.0}` width added stray spaces after the amount.)
        let pct_str = format!("{:.0}%", self.pct);

        // The amount sits in its own right-aligned column just left of the %,
        // with a 2-space gap when present.
        let amount_section_w = if max_amount_w > 0 {
            max_amount_w + 2
        } else {
            0
        };
        let header_pad = pct_col
            .saturating_sub(self.label.chars().count())
            .saturating_sub(max_rate_w)
            .saturating_sub(amount_section_w)
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
                if max_amount_w > 0 {
                    // Right-align the amount in its fixed column, then a 2-space gap.
                    let pad = max_amount_w.saturating_sub(self.amount.chars().count());
                    spans.push(Span::raw(" ".repeat(pad)));
                    if !self.amount.is_empty() {
                        spans.push(Span::styled(self.amount.clone(), theme::faint()));
                    }
                    spans.push(Span::raw("  "));
                }
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

/// Classify a window label into its rate unit. OAuth labels carry the unit at
/// the front (`"7d sonnet"`, `"7d opus"`, `"5h"` → `starts_with("7d")`); the
/// third-party bar labels carry it at the end (`"30d"`, `"7d"` → `ends_with('d')`
/// with a leading ascii digit). Anything else is treated as an hour window.
fn window_rate_unit(label: &str) -> &'static str {
    if label.starts_with("7d") {
        return "d";
    }
    if label.ends_with('d') && label.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
        return "d";
    }
    "h"
}

/// Pace-prediction toggles that always travel together (both originate from the
/// same `AppState.show_*` flags). Grouped so [`make_window_stat`] stays under
/// clippy's argument limit without an ad-hoc `#[allow]`.
#[derive(Clone, Copy)]
struct WindowGates {
    show_estimates: bool,
    show_pace: bool,
}

/// Shared core for both Stat-building paths (OAuth usage windows and third-party
/// provider bars). Computes the clamped pct, theme color, rate unit, and the
/// window-anchored burn pace / ideal-pace marker / reset countdown — all derived
/// purely from `pct` + `resets_at`, so the two paths render identically. The
/// caller supplies its own `trailing` (bar-line reset suffix) and `amount`
/// (eyebrow `used / total`); `gates` controls the pace fields.
fn make_window_stat(
    label: &str,
    pct: f64,
    resets_at: Option<&str>,
    now: i64,
    amount: String,
    trailing: String,
    gates: WindowGates,
) -> Stat {
    let pct = pct.clamp(0.0, 100.0);
    let rate_unit = window_rate_unit(label);
    let window = UsageWindow {
        utilization: pct,
        resets_at: resets_at.map(str::to_string),
    };
    let burn_rate = gates
        .show_estimates
        .then(|| {
            crate::usage::window_avg_pace_per_day(label, &window, now, 3600).map(|per_day| {
                if rate_unit == "d" {
                    per_day
                } else {
                    per_day / 24.0
                }
            })
        })
        .flatten();
    let pace_pct = gates
        .show_pace
        .then(|| ideal_pace_pct(label, &window, now))
        .flatten();
    let reset_secs = resets_at
        .and_then(crate::usage::iso_to_epoch_secs)
        .map(|r| r - now);
    Stat {
        label: label.to_string(),
        pct,
        color: Style::default().fg(theme::util_color(pct)),
        trailing,
        amount,
        burn_rate,
        rate_unit,
        pace_pct,
        reset_secs,
    }
}

fn collect_stats(profile: &Profile) -> Vec<Stat> {
    let Some(usage) = profile.usage.as_ref() else {
        return Vec::new();
    };
    let now_secs = now_epoch_secs();
    let mut stats: Vec<Stat> = Vec::new();
    for (label, w) in usage.windows() {
        let trailing = format_reset(w)
            .map(|r| format!("  resets in {r}"))
            .unwrap_or_default();
        // OAuth paths compute ungated here; the 5h recent-burn rate is filled
        // later from history (overwriting whatever `make_window_stat` set), and
        // the show_estimates / show_pace gates are applied by the caller.
        stats.push(make_window_stat(
            label,
            w.utilization,
            w.resets_at.as_deref(),
            now_secs,
            String::new(),
            trailing,
            WindowGates {
                show_estimates: true,
                show_pace: true,
            },
        ));
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
            trailing: String::new(),
            // Credits used/limit sits on the eyebrow before the %, like the bars.
            amount: format!("{sym}{used:.2} / {sym}{limit:.2}"),
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
        .third_party_usage
        .as_ref()
        .and_then(|s| s.plan.clone())
        .or_else(|| {
            profile
                .usage
                .as_ref()
                .and_then(|u| u.plan.as_ref())
                .map(plan_label)
        })
        .unwrap_or_else(|| {
            if profile.is_oauth() {
                "oauth".to_string()
            } else {
                "api".to_string()
            }
        });
    let mut plan_spans = vec![key_span("plan"), Span::styled(plan.clone(), theme::body())];
    if header.is_active {
        // "[ active ]" = 10 chars; left side = KEY_W + plan chars; pad the gap.
        let left_w = KEY_W + plan.chars().count();
        let indicator_w = "[ active ]".chars().count(); // 10
        let pad = (inner_w as usize)
            .saturating_sub(left_w)
            .saturating_sub(indicator_w);
        plan_spans.push(Span::raw(" ".repeat(pad)));
        plan_spans.extend(active_pill());
    }

    let mut lines = vec![Line::from(plan_spans)];
    lines.push(status_line(profile, header));
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
            // A staleness cue, not a failure: the endpoint is throttling us and
            // the shown numbers are last-known — amber like `cached`, not the
            // red `failed` gets, so it doesn't contradict the live-looking bar.
            spans.extend([
                Span::styled("[ ", theme::dim()),
                Span::styled(
                    "rate limited",
                    theme::warning().add_modifier(Modifier::BOLD),
                ),
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
fn build_tp_rows(
    profile: &Profile,
    inner_w: u16,
    show_estimates: bool,
    show_pace: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let Some(stats) = profile.third_party_usage.as_ref() else {
        // No data yet. "loading" only while a fetch is pending or in flight; a
        // terminal Failed status means we tried and the provider has nothing to
        // show, and a RateLimited one means the provider throttled us with
        // nothing cached — never spin on "loading" forever (the original z.ai bug).
        let msg = match profile.fetch_status {
            Some(FetchStatus::Failed) => "no usage available",
            Some(FetchStatus::RateLimited) => "rate limited — retrying",
            _ => "loading",
        };
        lines.push(Line::from(Span::styled(msg, theme::faint())));
        return lines;
    };

    let has_bars = !stats.bars.is_empty();

    // Percentage windows → bars rendered through the same `Stat::render` path as
    // OAuth window bars (near-full-width bar + two-line eyebrow), using each bar's
    // API-provided label in source order and showing absolute `used / total` on
    // the eyebrow just before the %.
    if has_bars {
        let bar_stats = stats_from_bars(&stats.bars, show_estimates, show_pace);
        lines.extend(render_stat_block(&bar_stats, inner_w));
    }

    // Text rows (e.g. z.ai per-model token totals, DeepSeek balances) render
    // below the bars. A provider can carry both.
    if !stats.rows.is_empty() {
        if has_bars {
            lines.push(Line::from(""));
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
    } else if !has_bars {
        let (msg, style) = if stats.is_available {
            ("no stats reported", theme::faint())
        } else {
            ("usage unavailable", theme::danger())
        };
        lines.push(Line::from(Span::styled(msg, style)));
    }

    // Best-effort (unknown-provider) data is mapped heuristically — invite a
    // report so a real integration can be added. Subtle, below everything.
    if stats.best_effort && (has_bars || !stats.rows.is_empty()) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "looks wrong? report at github.com/uwuclxdy/clauth/issues",
            theme::faint(),
        )));
    }

    lines
}

/// Build render [`Stat`]s from third-party percentage bars: each bar keeps its
/// API-provided label and source order (no inferred window vocabulary, no
/// reordering), shows absolute `used / total` on the eyebrow before the %, and a
/// reset countdown on the bar line. Rendered through the same `Stat::render` path
/// as OAuth window bars, so the two are visually identical.
///
/// A bar with a `<n>h`/`<n>d` label and a reset stamp also gets the OAuth window
/// predictions: a window-anchored average pace (`<n>d` → %/d, sub-day → %/h), a
/// burn ETA, and the ideal-pace marker — gated by the same `show_estimates` /
/// `show_pace` toggles. Providers never rotate, so the window-average pace is a
/// stable read (no recency-weighted history is kept for third-party accounts).
fn stats_from_bars(
    bars: &[crate::providers::UsageBar],
    show_estimates: bool,
    show_pace: bool,
) -> Vec<Stat> {
    let now = now_epoch_secs();
    let gates = WindowGates {
        show_estimates,
        show_pace,
    };
    bars.iter()
        .map(|bar| {
            let rem = window_remaining(bar, now);
            make_window_stat(
                &bar.label,
                bar.pct,
                bar.resets_at.as_deref(),
                now,
                bar_amount(bar),
                bar_reset_trailing(rem),
                gates,
            )
        })
        .collect()
}

/// Seconds until `bar` resets (may be negative if overdue). `None` when the bar
/// carries no reset stamp — its window length is then unknown.
fn window_remaining(bar: &crate::providers::UsageBar, now: i64) -> Option<i64> {
    let reset = crate::usage::iso_to_epoch_secs(bar.resets_at.as_deref()?)?;
    Some(reset - now)
}

/// Bar-line trailing: the reset countdown (`  resets in …`), or empty when the
/// bar carries no future reset. The absolute amount now lives on the eyebrow.
fn bar_reset_trailing(rem: Option<i64>) -> String {
    match rem.filter(|&s| s > 0) {
        Some(secs) => format!("  resets in {}", crate::usage::humanize_duration(secs)),
        None => String::new(),
    }
}

/// Eyebrow amount for a bar: `used / total` when both are present, else empty.
fn bar_amount(bar: &crate::providers::UsageBar) -> String {
    match (bar.used, bar.total) {
        (Some(used), Some(total)) => format!("{} / {}", fmt_amount(used), fmt_amount(total)),
        _ => String::new(),
    }
}

fn fmt_amount(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n:.2}")
    }
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
