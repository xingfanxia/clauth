//! Overview tab: accounts table + fallback flow, inside one content frame.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph};

use super::super::app::{App, MainItemKind};
use super::super::theme;
use super::chain::reason_marker;
use super::format::{
    account_type_label, cue_style, fetch_cue_color, fixed, fixed_split, spinner_frame,
    spinner_style, window_summary_spans_bracketed,
};
use super::header::pulse_name_spans;
use super::panes::{
    bold_when, draw_scrollbar, empty_state, name_color, section_box, select_line, wrap_words,
};
use super::usage::{eta_left_secs, window_rate_unit};
use crate::fallback::{
    BlockedReason, SwitchAction, blocked_reason, next_target, soonest_resume, threshold_for,
};
use crate::profile::{AppConfig, Profile};
use crate::usage::{
    LABEL_5H, LABEL_7D, ProfileActivity, UsageWindow, humanize_duration, now_epoch_secs, now_ms,
    switch_grade_kick_lifts,
};

/// `XXXs` + 1 trailing space = 5 chars; spinner padded to same width.
const TIMER_SLOT: usize = 5;
/// Rows the accounts table keeps before the chain panel may claim any space —
/// it is the scrollable, interactive list, so it wins the vertical budget.
const ACCOUNTS_MIN: u16 = 7;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // The chain panel's inner width is independent of the vertical split (both
    // panels span the full body width), so build its lines first and size the
    // panel to fit them — no clipped members, no wasted rows.
    let probe = Rect { height: 3, ..area };
    let chain_inner_w = section_box("", false, false).inner(probe).width as usize;
    let chain_lines = fallback_flow_lines(app, chain_inner_w);
    let chain_height = chain_panel_height(chain_lines.len(), area.height);

    let [accounts_area, chain_area] = Layout::vertical([
        Constraint::Min(ACCOUNTS_MIN),
        Constraint::Length(chain_height),
    ])
    .areas(area);

    draw_overview_accounts(frame, accounts_area, app);
    draw_fallback_overview(frame, chain_area, chain_lines);
}

/// Height for the fallback chain panel: sized to its content (`content_rows`
/// plus the 2 border rows), capped so the accounts table keeps [`ACCOUNTS_MIN`]
/// rows whenever `area_height >= ACCOUNTS_MIN + 3`. Below that the 3-row floor
/// (border + one row) wins instead and accounts gives way — a terminal too
/// short for both shrinks the accounts table, not the chain.
fn chain_panel_height(content_rows: usize, area_height: u16) -> u16 {
    let desired = (content_rows as u16).saturating_add(2);
    let max_chain = area_height.saturating_sub(ACCOUNTS_MIN);
    desired.min(max_chain).max(3)
}

fn draw_overview_accounts(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // Sole interactive content panel on this screen — always focused.
    let focused = true;
    let block = section_box("accounts", focused, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.config().profiles.is_empty() {
        frame.render_widget(empty_state("no accounts yet", "n", "to create one"), inner);
        return;
    }

    let [header_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);

    let widths = OverviewWidths::new(list_area.width, app);
    let header = overview_header(&widths);
    frame.render_widget(Paragraph::new(header).style(theme::base()), header_area);

    let items = app.main_items();
    let sel = app.profile_cursor.min(items.len().saturating_sub(1));
    let width = list_area.width;
    let rows: Vec<ListItem<'_>> = items
        .iter()
        .enumerate()
        .map(|(row, item)| match item {
            MainItemKind::Profile(idx) => {
                let selected = row == sel;
                let line = render_overview_row(app, *idx, &widths, selected, focused);
                ListItem::new(select_line(line, selected, focused, width))
            }
        })
        .collect();

    let total = items.len();
    let list = List::new(rows).style(theme::base());
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(sel));
    frame.render_stateful_widget(list, list_area, &mut state);

    let viewport = list_area.height as usize;
    draw_scrollbar(frame, list_area, total, state.offset(), viewport);
}

#[derive(Debug, Clone, Copy)]
struct OverviewWidths {
    name: usize,
    kind: usize,
    five_hour: usize,
    seven_day: usize,
    gap: usize,
}

impl OverviewWidths {
    fn new(width: u16, app: &App) -> Self {
        let total = width as usize;
        let max_name = app
            .config()
            .profiles
            .iter()
            .map(|p| p.name.chars().count())
            .max()
            .unwrap_or(8);
        let mut name = max_name.clamp(8, if total >= 86 { 22 } else { 16 });
        let mut kind = if total >= 92 {
            16
        } else if total >= 66 {
            12
        } else {
            6
        };
        // 26 = [bar]+pct+reset, 17 = [bar]+pct only.
        let mut five_hour = if total >= 81 {
            26
        } else if total >= 64 {
            17
        } else {
            12
        };
        let mut seven_day = if total >= 102 {
            26
        } else if total >= 93 {
            17
        } else if total >= 58 {
            5
        } else {
            0
        };
        let gap_min = 2;
        while fixed_overview_width(name, kind, five_hour, seven_day, gap_min) > total {
            if seven_day >= 17 {
                seven_day = 5;
            } else if seven_day > 0 {
                seven_day = 0;
            } else if five_hour > 17 {
                five_hour = 17;
            } else if five_hour > 12 {
                five_hour = 12;
            } else if kind > 6 {
                kind = 6;
            } else if name > 8 {
                name -= 1;
            } else {
                break;
            }
        }

        let base = fixed_overview_width(name, kind, five_hour, seven_day, gap_min);
        let column_count = 3 + usize::from(seven_day > 0);
        let gap_slots = column_count.saturating_sub(1).max(1);
        // `fixed_overview_width` omits the TIMER_SLOT the row always renders;
        // widening gaps from that undercounted figure overflows the row at
        // narrow widths and clips the tail of the 5h column. Widen from the
        // real leftover instead.
        let gap = (gap_min + total.saturating_sub(base + TIMER_SLOT) / gap_slots).clamp(gap_min, 8);

        Self {
            name,
            kind,
            five_hour,
            seven_day,
            gap,
        }
    }
}

fn fixed_overview_width(
    name: usize,
    kind: usize,
    five_hour: usize,
    seven_day: usize,
    gap: usize,
) -> usize {
    let column_count = 3 + usize::from(seven_day > 0);
    // 2 = cursor prefix. Timer slot is in the gap before 5h, not a column.
    // kind→timer gap is 4 chars narrower than standard (min 1).
    let narrow = gap.saturating_sub(4).max(1);
    let standard_gaps = column_count.saturating_sub(2);
    4 + name + kind + five_hour + seven_day + standard_gaps * gap + narrow
}

fn overview_header(widths: &OverviewWidths) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", theme::label())];
    spans.push(Span::raw("  ")); // bell slot (blank in header)
    spans.push(Span::styled(fixed("account", widths.name), theme::label()));
    spans.push(gap(widths));
    spans.push(Span::styled(fixed("type", widths.kind), theme::label()));
    spans.push(narrow_gap(widths));
    // Blank TIMER_SLOT keeps the label aligned over the bar.
    spans.push(Span::raw(" ".repeat(TIMER_SLOT)));
    spans.push(Span::styled(
        fixed(LABEL_5H, widths.five_hour),
        theme::label(),
    ));
    if widths.seven_day > 0 {
        spans.push(gap(widths));
        spans.push(Span::styled(
            fixed(LABEL_7D, widths.seven_day),
            theme::label(),
        ));
    }
    Line::from(spans)
}

fn render_overview_row(
    app: &App,
    idx: usize,
    widths: &OverviewWidths,
    selected: bool,
    focused: bool,
) -> Line<'static> {
    let cfg = app.config();
    let Some(profile) = cfg.profiles.get(idx) else {
        return Line::from("");
    };

    let active = cfg.is_active(&profile.name);
    let name_str = profile.name.to_string();
    // Overview rows only: the refresh countdown carries the profile's
    // fetch-state cue (amber = last-known numbers, red = failed) so staleness
    // reads off the timer instead of the bar brackets.
    let cue = fetch_cue_color(profile);
    let cursor = if selected && focused {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::raw("  ")
    };
    let timer_span = {
        let inner = TIMER_SLOT - 1;
        let activity = app
            .activity
            .lock()
            .ok()
            .and_then(|g| g.get(&name_str).copied())
            .unwrap_or(ProfileActivity::Idle);
        if !matches!(activity, ProfileActivity::Idle) {
            let frame = spinner_frame(app.tick_count);
            let style = spinner_style(activity);
            Span::styled(format!("{frame:>inner$} ", inner = inner), style)
        } else {
            let secs_str = app
                .next_refresh_per_profile
                .lock()
                .ok()
                .and_then(|m| m.get(&name_str).copied())
                .map(|next_ms| {
                    let now = now_ms();
                    let secs = ((next_ms as i64 - now as i64) / 1000).max(0);
                    format!("{secs}s")
                });
            match secs_str {
                Some(s) => Span::styled(
                    format!("{:>inner$} ", s, inner = inner),
                    cue_style(cue, theme::faint()),
                ),
                None => Span::raw(" ".repeat(TIMER_SLOT)),
            }
        }
    };

    let mut spans = vec![cursor];
    // Marker precedence: broken login (×) > bell (!) > active (●) — a dead
    // login makes usage alerts moot until re-login.
    if cfg.is_auth_broken(&profile.name) {
        spans.push(Span::styled("×", theme::danger()));
        spans.push(Span::raw(" "));
    } else if app.bell_fired.contains_key(&name_str) {
        spans.push(Span::styled("!", theme::danger()));
        spans.push(Span::raw(" "));
    } else if active {
        spans.push(Span::styled("●", theme::accent_2_color()));
        spans.push(Span::raw(" "));
    } else {
        spans.push(Span::raw("  "));
    }
    let (nt, np) = fixed_split(&profile.name, widths.name);
    let ns = bold_when(name_color(active), selected && focused);
    spans.push(Span::styled(nt, ns));
    spans.push(Span::raw(np));
    spans.push(gap(widths));
    let label = account_type_label(profile);
    if profile.credentials.is_some() {
        let (clamped, pad) = fixed_split(&label, widths.kind);
        let elapsed = app.started_at.elapsed().as_millis() as u64;
        let mut pulse = pulse_name_spans(&clamped, theme::dim(), elapsed);
        pulse.push(Span::raw(pad));
        spans.extend(pulse);
    } else {
        spans.push(Span::styled(fixed(&label, widths.kind), theme::dim()));
    }
    spans.push(narrow_gap(widths));
    spans.push(timer_span);
    // Bracketed bars ([███░░░]) for overview account rows only; brackets stay
    // dim — the fetch-state cue lives on the countdown above instead.
    // Usage-page gauges, chain bars, and fallback thresholds stay bracket-less.
    // OAuth windows come from `usage`; api-key/provider profiles have no `usage`,
    // so the 5h/7d windows are synthesized from the matching third-party bars.
    let (five_window, seven_window) = overview_windows(profile);
    // Drain-color each reset countdown by the window's burn rate — see
    // `drain_rate` for where that rate comes from per window.
    let reset_style = |label, window: Option<&UsageWindow>| {
        let window = window?;
        drain_reset_style(
            drain_rate(app, &name_str, profile, label, window),
            window_rate_unit(label),
            window,
        )
    };
    let five_spans = window_summary_spans_bracketed(
        five_window.as_ref(),
        widths.five_hour,
        true,
        reset_style(LABEL_5H, five_window.as_ref()),
    );
    let five_len: usize = five_spans.iter().map(|s| s.content.chars().count()).sum();
    let five_pad = widths.five_hour.saturating_sub(five_len);
    spans.extend(five_spans);
    spans.push(Span::raw(" ".repeat(five_pad)));
    if widths.seven_day > 0 {
        spans.push(gap(widths));
        let seven_spans = window_summary_spans_bracketed(
            seven_window.as_ref(),
            widths.seven_day,
            widths.seven_day >= 18,
            reset_style(LABEL_7D, seven_window.as_ref()),
        );
        let seven_len: usize = seven_spans.iter().map(|s| s.content.chars().count()).sum();
        let seven_pad = widths.seven_day.saturating_sub(seven_len);
        spans.extend(seven_spans);
        spans.push(Span::raw(" ".repeat(seven_pad)));
    }

    Line::from(spans)
}

/// The `(5h, 7d)` windows to show in the overview row. OAuth profiles use their
/// live `UsageInfo`; api-key/provider profiles have no `UsageInfo`, so each slot
/// is synthesized from the third-party bar whose label matches (`5h` / `7d`) —
/// the same labels `zai` decodes from its window codes. `None` per slot when no
/// source exists (renders `—`).
fn overview_windows(profile: &Profile) -> (Option<UsageWindow>, Option<UsageWindow>) {
    if let Some(usage) = profile.usage.as_ref() {
        return (usage.five_hour.clone(), usage.weekly_window().cloned());
    }
    let Some(bars) = profile.third_party_usage.as_ref().map(|s| &s.bars) else {
        return (None, None);
    };
    let window_for = |label: &str| {
        bars.iter().find(|b| b.label == label).map(|b| UsageWindow {
            utilization: b.pct,
            resets_at: b.resets_at.clone(),
        })
    };
    (window_for(LABEL_5H), window_for(LABEL_7D))
}

fn gap(widths: &OverviewWidths) -> Span<'static> {
    Span::raw(" ".repeat(widths.gap))
}

/// 4 chars less than standard gap; min 1. Used between `type` and timer slot.
fn narrow_gap(widths: &OverviewWidths) -> Span<'static> {
    Span::raw(" ".repeat(widths.gap.saturating_sub(4).max(1)))
}

fn draw_fallback_overview(frame: &mut Frame<'_>, area: Rect, lines: Vec<Line<'static>>) {
    // Read-only detail pane — focus never descends here from the overview screen.
    let block = section_box("fallback chain", false, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // No `Wrap`: the chain rows are tabular (they pad to `inner.width`, so they
    // never wrap), and the empty-state prose is pre-wrapped into its own lines.
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

const GAUGE_W: usize = 12;

fn fallback_flow_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    // Switch-grade kick blocks feed the blocked-reason markers; read BEFORE the
    // Config lock (rank order: KickBlockState 230 < Config 400).
    let kick_lifts = switch_grade_kick_lifts(&app.kick_blocks);
    let cfg = app.config();
    if cfg.state.fallback_chain.is_empty() {
        let mut lines = vec![Line::from(Span::styled(
            "no fallback chain yet",
            theme::dim(),
        ))];
        lines.extend(
            wrap_words(
                "fallback tab adds accounts that rotate automatically when a 5h \
                 window crosses its threshold.",
                width,
            )
            .into_iter()
            .map(|seg| Line::from(Span::styled(seg, theme::dim()))),
        );
        return lines;
    }

    let chain = &cfg.state.fallback_chain;
    let name_w = chain
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(8)
        .clamp(6, 18);
    let last = chain.len() - 1;

    // Project the active profile's next switch once, up front: a `To(target)`
    // renders inline on the target member's row (right side); `Off` has no
    // single target row, so it stays a caption below.
    let projection = projected_switch(app, &cfg);
    let switch_to = match &projection {
        Some((SwitchAction::To(target), secs)) => Some((target.clone(), *secs)),
        _ => None,
    };

    let mut lines = Vec::new();
    for (i, name) in chain.iter().enumerate() {
        let reason = cfg
            .find(name)
            .and_then(|p| blocked_reason(&cfg, p, kick_lifts.get(name.as_str()).copied()));
        let switch_eta = switch_to
            .as_ref()
            .filter(|(target, _)| target.as_str() == name.as_str())
            .map(|(_, secs)| *secs);
        lines.push(chain_row(
            &cfg, name, i, last, name_w, width, reason, switch_eta,
        ));
    }

    // All-spent caption: wrap-off `stop` vs wrap-mode `stay`.
    let caption = if cfg.state.switch_off_when_spent {
        vec![
            Span::raw("  "),
            Span::styled("[ ", theme::dim()),
            Span::styled("stop", theme::danger().bold()),
            Span::styled(" ]", theme::dim()),
            Span::styled(" when all spent", theme::faint()),
        ]
    } else {
        vec![
            Span::raw("  "),
            Span::styled("[ ", theme::dim()),
            Span::styled("stay", theme::dim().bold()),
            Span::styled(" ]", theme::dim()),
            Span::styled(" on last when all spent", theme::faint()),
        ]
    };
    lines.push(Line::from(caption));

    // `Off` projection: chain-wide, no target row to sit on — keep it a caption.
    if let Some((SwitchAction::Off, secs)) = &projection {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("stops all in ~{}", humanize_duration(*secs)),
                theme::faint(),
            ),
        ]));
    }

    // All-exhausted sibling of the projection: when EVERY chain member is maxed
    // (wrap-off's active-cleared state, or wrap mode's stalled-active
    // equivalent), name whichever one resumes first. Mutually exclusive with the
    // projection — `burn_rate_eta` returns `None` once the active crosses its
    // own threshold, which is a precondition for `soonest_resume` to return.
    if let Some((name, eta)) = soonest_resume(&cfg) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("resumes: {name} in ~{}", humanize_duration(eta)),
                theme::faint(),
            ),
        ]));
    }
    lines
}

/// The active profile's projected next switch (action + eta secs), or `None`
/// when none is imminent. Guards the projection the same way the old inline line
/// did: only when the active crosses its threshold BEFORE its 5h window resets
/// (past the reset the window refills and no switch fires). Shared by the inline
/// `To` hint (on the target's row) and the `Off` caption.
fn projected_switch(app: &App, cfg: &AppConfig) -> Option<(SwitchAction, i64)> {
    if cfg.state.fallback_chain.len() <= 1 {
        return None;
    }
    let active_name = cfg.state.active_profile.as_deref()?;
    let profile = cfg.find(active_name)?;
    let usage_info = profile.usage.as_ref()?;
    let usage = usage_info.five_hour.as_ref()?;
    let threshold = threshold_for(profile);
    // In-memory rate (`app.history_cache`) — no disk read while `cfg` is held.
    let active_rate = app.active_burn_rate(active_name, usage_info);
    let eta_secs = burn_rate_eta(active_rate, usage.utilization, threshold)?;
    let reset_secs = super::format::reset_in_secs(usage);
    if reset_secs.is_some_and(|reset| eta_secs >= reset) {
        return None;
    }
    next_target(cfg, active_rate).map(|action| (action, eta_secs))
}

#[allow(clippy::too_many_arguments)]
fn chain_row(
    cfg: &AppConfig,
    name: &str,
    index: usize,
    last: usize,
    name_w: usize,
    width: usize,
    reason: Option<BlockedReason>,
    switch_eta: Option<i64>,
) -> Line<'static> {
    let active = cfg.is_active(name);
    let rail = if index == 0 && last == 0 {
        "╶"
    } else if index == 0 {
        "╭"
    } else if index == last {
        "╰"
    } else {
        "│"
    };
    // Color carries active state — no glyph needed.
    let name_style = if active {
        Style::default().fg(theme::accent_2_color())
    } else {
        theme::dim()
    };
    let name_pad = name_w.saturating_sub(name.chars().count());

    let mut spans = vec![
        Span::styled(format!(" {rail} "), theme::faint()),
        Span::styled(format!("{} ", index + 1), theme::faint()),
        Span::styled(format!("{name}{}  ", " ".repeat(name_pad)), name_style),
    ];

    match cfg.find(name) {
        None => spans.push(Span::styled("missing", theme::danger())),
        Some(profile) => {
            let threshold = threshold_for(profile);
            let pct = profile
                .usage
                .as_ref()
                .and_then(|u| u.five_hour.as_ref())
                .map(|w| w.utilization);
            spans.extend(gauge_spans(pct, threshold));
            let (figure, figure_style) = match pct {
                Some(v) => (
                    format!("  {v:>3.0}"),
                    Style::default().fg(theme::util_color(v)),
                ),
                None => ("    —".to_string(), theme::faint()),
            };
            spans.push(Span::styled(figure, figure_style));
            spans.push(Span::styled(format!(" / {threshold:.0}%"), theme::faint()));
        }
    }

    // Right-aligned trailer. A projected-switch target carries the `↩ ~eta`
    // hint; a blocked member carries its 1-cell reason marker. BOTH can apply to
    // one row: `next_target`'s headroom walk only prefers a fresh member and
    // falls through to a stale-but-unexhausted one (`is_exhausted` ignores
    // `fetch_status`), so a `To` target can also be `Stale`. Render both then
    // (hint, then marker outermost) instead of dropping the imminent-switch
    // projection. Too narrow for the pair → keep the marker (the persistent
    // signal) and drop the hint; too narrow for even that → drop both. Each
    // guard's strict `<` leaves >= 1 pad cell so the group never abuts the
    // figure.
    let base_used: usize = spans.iter().map(|s| s.width()).sum();
    let hint = switch_eta
        .map(|secs| Span::styled(format!("↩ ~{}", humanize_duration(secs)), theme::faint()));
    let marker = reason.as_ref().map(reason_marker);
    let trailer: Vec<Span<'static>> = match (hint, marker) {
        (Some(h), Some(m)) if base_used + h.width() + 1 + m.width() < width => {
            vec![h, Span::raw(" "), m]
        }
        (Some(_), Some(m)) if base_used + m.width() < width => vec![m],
        (Some(h), None) if base_used + h.width() < width => vec![h],
        (None, Some(m)) if base_used + m.width() < width => vec![m],
        _ => Vec::new(),
    };
    if !trailer.is_empty() {
        let tw: usize = trailer.iter().map(|s| s.width()).sum();
        spans.push(Span::raw(" ".repeat(width.saturating_sub(base_used + tw))));
        spans.extend(trailer);
    }
    Line::from(spans)
}

/// `GAUGE_W`-cell bar relative to the member's threshold (full = rotate off).
fn gauge_spans(pct: Option<f64>, threshold: f64) -> Vec<Span<'static>> {
    let fill = pct
        .map(|v| {
            let frac = if threshold > 0.0 {
                (v / threshold).clamp(0.0, 1.0)
            } else {
                1.0
            };
            (frac * GAUGE_W as f64).round() as usize
        })
        .unwrap_or(0)
        .min(GAUGE_W);
    let fill_color = pct
        .map(theme::util_color)
        .unwrap_or(theme::text_faint_color());

    (0..GAUGE_W)
        .map(|i| {
            if i < fill {
                Span::styled("▰", Style::default().fg(fill_color))
            } else {
                Span::styled("▱", theme::faint())
            }
        })
        .collect()
}

/// Seconds until `current` crosses `threshold` at the given 5h-window burn
/// `rate` (%/h, from [`App::active_burn_rate`]). Returns `None` when there's no
/// rate yet, the rate is flat/negative, or utilization is already at/above the
/// threshold.
fn burn_rate_eta(rate: Option<f64>, current: f64, threshold: f64) -> Option<i64> {
    if current >= threshold {
        return None;
    }
    let rate = rate?;
    if rate <= 0.0 {
        return None;
    }
    let hours = (threshold - current) / rate;
    if hours <= 0.0 {
        return None;
    }
    Some((hours * 3600.0) as i64)
}

/// Drain color for an overview reset-countdown suffix (wide layout only).
/// `util_color` of the burn `rate` (slow drain dim, fast warning/danger —
/// mirrors the usage page), escalated to a flat WARNING when the window
/// projects to hit 100% BEFORE it resets ("runs dry first" — you top out ahead
/// of the refill). `rate` is in `rate_unit` (`%/h` or `%/d`) — the window's own
/// unit, the same one the usage page hues by, so both surfaces agree.
/// `None` (caller keeps the faint default) when there's no positive rate yet.
fn drain_reset_style(rate: Option<f64>, rate_unit: &str, window: &UsageWindow) -> Option<Style> {
    let rate = rate.filter(|r| *r > 0.0)?;
    let eta = eta_left_secs(rate, window.utilization, rate_unit);
    let reset = super::format::reset_in_secs(window);
    let runs_dry_first = matches!((eta, reset), (Some(e), Some(r)) if e < r);
    Some(if runs_dry_first {
        theme::warning()
    } else {
        Style::default().fg(theme::util_color(rate.clamp(0.0, 100.0)))
    })
}

/// The rate to drain-color `window`'s countdown by, in the window's native unit
/// (see [`drain_reset_style`]).
///
/// An OAuth 5h window uses the recency-weighted recent burn — in-memory
/// `history_cache`, so no disk read happens under the config guard. Every other
/// window falls back to the window's own average pace, which needs no burn
/// history at all: 7d moves too slowly for the recency weighting to say much,
/// and a synthesized third-party window has no history to weigh.
fn drain_rate(
    app: &App,
    name: &str,
    profile: &Profile,
    label: &str,
    window: &UsageWindow,
) -> Option<f64> {
    if label == LABEL_5H
        && let Some(usage) = profile.usage.as_ref()
    {
        return app.active_burn_rate(name, usage);
    }
    let per_day = crate::usage::window_avg_pace_per_day(label, window, now_epoch_secs(), 3600)?;
    Some(if window_rate_unit(label) == "d" {
        per_day
    } else {
        per_day / 24.0
    })
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_overview.rs"]
mod tests;
