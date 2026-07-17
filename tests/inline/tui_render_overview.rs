//! `fallback_flow_lines`'s all-exhausted "resumes: <name> in ~<eta>" caption
//! (issue #10 follow-up) — the sibling of the "switching to <name> in ~<eta>"
//! projection line, driven by `crate::fallback::soonest_resume`.
//! Plus the overview-row state cues: marker precedence + countdown fetch cue.

use super::*;
use ratatui::style::Modifier;

use crate::profile::{AppState, ClaudeCredentials, OAuthToken, ProfileName};
use crate::usage::{FetchStatus, UsageInfo, epoch_secs_to_iso, now_epoch_secs};
use std::collections::BTreeMap;

/// ISO reset `secs` in the future.
fn reset_in(secs: i64) -> String {
    epoch_secs_to_iso(now_epoch_secs() + secs)
}

/// A slow burner whose reset lands well before it would top out reads as a
/// low-drain suffix: the plain `util_color` of the rate (dim), never the
/// runs-dry WARNING escalation.
#[test]
fn drain_reset_style_low_drain_is_dim_hue() {
    let w = crate::usage::UsageWindow {
        utilization: 50.0,
        resets_at: Some(reset_in(3_600)), // resets in 1h
    };
    // 1 %/h from 50% → ~50h to 100%, far past the 1h reset → not runs-dry.
    let style = drain_reset_style(Some(1.0), "h", &w).expect("a positive rate yields a style");
    assert_eq!(
        style.fg,
        Some(theme::util_color(1.0)),
        "a slow drain colors by util_color (dim), not warning",
    );
    assert_ne!(
        style.fg,
        theme::warning().fg,
        "a slow drain must not read as the runs-dry warning",
    );
}

/// A fast burner that will hit 100% before its reset flips the suffix to the
/// flat WARNING tint regardless of the rate's own `util_color` band.
#[test]
fn drain_reset_style_runs_dry_first_is_warning() {
    let w = crate::usage::UsageWindow {
        utilization: 50.0,
        resets_at: Some(reset_in(360_000)), // resets in 100h
    };
    // 50 %/h from 50% → ~1h to 100%, well before the 100h reset → runs dry.
    let style = drain_reset_style(Some(50.0), "h", &w).expect("a positive rate yields a style");
    assert_eq!(
        style.fg,
        theme::warning().fg,
        "running dry before the reset escalates to warning",
    );
    // Proves the escalation overrides the rate's own band (util_color(50) = dim).
    assert_ne!(style.fg, Some(theme::util_color(50.0)));
}

/// No positive burn rate (too little history, or a window too young for an avg
/// pace) yields no style, so the caller keeps the faint default.
#[test]
fn drain_reset_style_none_without_a_positive_rate() {
    let w = crate::usage::UsageWindow {
        utilization: 50.0,
        resets_at: Some(reset_in(3_600)),
    };
    assert!(
        drain_reset_style(None, "h", &w).is_none(),
        "no rate → no style"
    );
    assert!(
        drain_reset_style(Some(0.0), "h", &w).is_none(),
        "a flat rate → no style",
    );
}

/// A 7d rate arrives in %/d, so the runs-dry projection must divide by 24
/// rather than reading %/d as %/h — which would over-project the drain by 24x
/// and paint an idle weekly window amber.
#[test]
fn drain_reset_style_reads_a_7d_rate_as_per_day() {
    let w = crate::usage::UsageWindow {
        utilization: 50.0,
        resets_at: Some(reset_in(2 * 86_400)), // resets in 2d
    };
    // 10 %/d from 50% → 5d to 100%, past the 2d reset → not runs-dry.
    let style = drain_reset_style(Some(10.0), "d", &w).expect("a positive rate yields a style");
    assert_eq!(
        style.fg,
        Some(theme::util_color(10.0)),
        "a weekly window that outlasts its drain hues by util_color, not warning",
    );
    // The same figure misread as %/h → ~5h to 100% → runs dry → warning.
    assert_eq!(
        drain_reset_style(Some(10.0), "h", &w).map(|s| s.fg),
        Some(theme::warning().fg),
        "guards the unit: %/h from the same number does escalate",
    );
}

/// 40 %/d from 50% → ~1.25d to 100%, before the 2d reset → runs dry.
#[test]
fn drain_reset_style_7d_runs_dry_first_is_warning() {
    let w = crate::usage::UsageWindow {
        utilization: 50.0,
        resets_at: Some(reset_in(2 * 86_400)),
    };
    let style = drain_reset_style(Some(40.0), "d", &w).expect("a positive rate yields a style");
    assert_eq!(style.fg, theme::warning().fg);
}

/// `drain_rate` must source a rate for BOTH windows of an api-key/provider
/// profile. These have no `UsageInfo` and no burn history at all, so the
/// `active_burn_rate` path yields nothing — the window's own average pace is
/// the only source, and it needs only the window's utilization + `resets_at`.
#[test]
fn drain_rate_covers_third_party_windows_from_avg_pace() {
    let p = third_party_profile(60.0, 30.0);
    let config = config_with(vec![p], None, vec![]);
    let app = App::new(config);
    let profile = &app.config().profiles[0];
    let (five, seven) = overview_windows(profile);
    let five = five.expect("5h bar synthesizes a window");
    let seven = seven.expect("7d bar synthesizes a window");

    assert!(
        app.active_burn_rate("tp", &UsageInfo::default()).is_none(),
        "no burn history exists for an api-key profile — avg pace is the only source",
    );
    let five_rate = drain_rate(&app, "tp", profile, LABEL_5H, &five)
        .expect("a half-elapsed 5h window yields an avg pace");
    let seven_rate = drain_rate(&app, "tp", profile, LABEL_7D, &seven)
        .expect("a half-elapsed 7d window yields an avg pace");
    assert!(five_rate > 0.0 && seven_rate > 0.0);

    // 60% over the 2.5h elapsed half of a 5h window = 24 %/h.
    assert!(
        (five_rate - 24.0).abs() < 0.5,
        "5h rate in %/h: {five_rate}"
    );
    // 30% over the 3.5d elapsed half of a 7d window ≈ 8.57 %/d.
    assert!(
        (seven_rate - 30.0 / 3.5).abs() < 0.2,
        "7d rate in %/d: {seven_rate}",
    );
    assert!(
        drain_reset_style(Some(five_rate), window_rate_unit(LABEL_5H), &five).is_some(),
        "a third-party 5h countdown must drain-color",
    );
    assert!(
        drain_reset_style(Some(seven_rate), window_rate_unit(LABEL_7D), &seven).is_some(),
        "a third-party 7d countdown must drain-color",
    );
}

/// An OAuth 5h window keeps the recency-weighted recent burn, not the avg pace:
/// with no history recorded, it stays uncolored rather than falling back.
#[test]
fn drain_rate_oauth_five_hour_uses_recent_burn() {
    let a = profile("a", 95.0, 60.0, 9_000);
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    let p = &app.config().profiles[0];
    let w = p.usage.as_ref().unwrap().five_hour.clone().unwrap();
    assert!(
        drain_rate(&app, "a", p, LABEL_5H, &w).is_none(),
        "no recorded history → no rate, rather than an avg-pace fallback",
    );
}

/// An api-key/provider profile: no `UsageInfo`, so its overview 5h/7d windows
/// are synthesized from the provider bars. Both bars sit exactly half-elapsed,
/// which is enough for `window_avg_pace_per_day` to have a pace to report.
fn third_party_profile(five_pct: f64, seven_pct: f64) -> Profile {
    let bar = |label: &str, pct: f64, reset_secs: i64| crate::providers::UsageBar {
        label: label.to_string(),
        pct,
        resets_at: Some(reset_in(reset_secs)),
        used: None,
        total: None,
    };
    Profile {
        name: "tp".into(),
        base_url: Some("https://api.example.com".into()),
        api_key: Some("k".into()),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        bell_threshold: None,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: Some(crate::providers::ThirdPartyStats {
            is_available: true,
            rows: Vec::new(),
            bars: vec![
                bar(LABEL_5H, five_pct, 5 * 3600 / 2),
                bar(LABEL_7D, seven_pct, 7 * 86_400 / 2),
            ],
            plan: None,
            endpoint: None,
            best_effort: false,
        }),
    }
}

/// A chain-eligible OAuth profile with a live 5h window at `util`%, resetting
/// in `reset_secs`.
fn profile(name: &str, threshold: f64, util: f64, reset_secs: i64) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(threshold),
        last_resort: false,
        max_auto_spend: None,
        bell_threshold: None,
        credentials: None,
        usage: Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: util,
                resets_at: Some(reset_in(reset_secs)),
            }),
            ..UsageInfo::default()
        }),
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn config_with(profiles: Vec<Profile>, active: Option<&str>, chain: Vec<&str>) -> AppConfig {
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            profiles: names,
            fallback_chain: chain.into_iter().map(Into::into).collect(),
            ..AppState::default()
        },
        profiles,
    }
}

/// Flattens a line's spans to plain text for substring assertions.
fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn resumes_line(lines: &[Line<'static>]) -> Option<String> {
    lines.iter().map(line_text).find(|t| t.contains("resumes:"))
}

// Wrap mode: the active profile itself is exhausted and stays put (no sink,
// `next_target` returns `None`) — previously silent. b resets sooner than a.
#[test]
fn all_exhausted_wrap_mode_shows_resumes_hint() {
    let a = profile("a", 95.0, 100.0, 3600);
    let b = profile("b", 95.0, 100.0, 1800);
    let config = config_with(vec![a, b], Some("a"), vec!["a", "b"]);
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60, 20);
    let hint =
        resumes_line(&lines).expect("resumes hint must render when the whole chain is exhausted");
    assert!(
        hint.contains("resumes: b in ~"),
        "names the soonest-resuming member: {hint}"
    );
}

// Wrap-off: switch-off-all already cleared the active profile. The hint must
// not depend on an active profile being set at all.
#[test]
fn all_exhausted_wrap_off_active_cleared_shows_resumes_hint() {
    let a = profile("a", 95.0, 100.0, 900);
    let b = profile("b", 95.0, 100.0, 3600);
    let mut config = config_with(vec![a, b], None, vec!["a", "b"]);
    config.state.wrap_off = true;
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60, 20);
    let hint = resumes_line(&lines)
        .expect("resumes hint must render even with no active profile (wrap-off cleared it)");
    assert!(hint.contains("resumes: a in ~"), "{hint}");
}

// b still has headroom — the chain is not all-exhausted, so the caption must
// stay hidden (recovery would relink b on the next tick regardless).
#[test]
fn partially_exhausted_chain_hides_resumes_hint() {
    let a = profile("a", 95.0, 100.0, 3600);
    let b = profile("b", 95.0, 20.0, 3600);
    let config = config_with(vec![a, b], Some("a"), vec!["a", "b"]);
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60, 20);
    assert!(
        resumes_line(&lines).is_none(),
        "must not show when the chain isn't fully exhausted"
    );
}

// Nobody near their threshold at all — the ordinary healthy-chain case.
#[test]
fn healthy_chain_hides_resumes_hint() {
    let a = profile("a", 95.0, 10.0, 3600);
    let b = profile("b", 95.0, 5.0, 3600);
    let config = config_with(vec![a, b], Some("a"), vec!["a", "b"]);
    let app = App::new(config);
    let lines = fallback_flow_lines(&app, 60, 20);
    assert!(resumes_line(&lines).is_none());
}

// ── overview row state cues ──────────────────────────────────────────────

/// Marker column: a broken login (×) outranks both the bell (!) and the
/// active dot (●) — usage alerts are moot until re-login.
#[test]
fn broken_login_marker_outranks_bell_and_active() {
    let a = profile("a", 95.0, 10.0, 3600);
    let mut config = config_with(vec![a], Some("a"), vec![]);
    config.state.auth_broken.push("a".into());
    let mut app = App::new(config);
    app.bell_fired.insert("a".into(), true);
    let widths = OverviewWidths::new(80, &app);
    let line = render_overview_row(&app, 0, &widths, false, true);
    let text = line_text(&line);
    assert!(text.contains('×'), "broken login renders ×: {text}");
    assert!(!text.contains('!'), "bell yields to ×: {text}");
    assert!(!text.contains('●'), "active dot yields to ×: {text}");
    let marker = line.spans.iter().find(|s| s.content == "×").unwrap();
    assert_eq!(marker.style.fg, theme::danger().fg);
}

#[test]
fn bell_marker_shows_when_login_is_fine() {
    let a = profile("a", 95.0, 10.0, 3600);
    let config = config_with(vec![a], None, vec![]);
    let mut app = App::new(config);
    app.bell_fired.insert("a".into(), true);
    let widths = OverviewWidths::new(80, &app);
    let text = line_text(&render_overview_row(&app, 0, &widths, false, true));
    assert!(text.contains('!'), "{text}");
    assert!(!text.contains('×'), "{text}");
}

/// The stale-data cue lives on the refresh countdown now — an underlined name
/// would double-signal, and the bar brackets stay plain dim.
#[test]
fn cached_row_colors_countdown_amber_and_underlines_nothing() {
    let mut a = profile("a", 95.0, 10.0, 3600);
    a.fetch_status = Some(FetchStatus::Cached);
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    app.next_refresh_per_profile
        .lock()
        .unwrap()
        .insert("a".to_string(), now_ms() + 30_000);
    let widths = OverviewWidths::new(80, &app);
    let line = render_overview_row(&app, 0, &widths, false, true);
    assert!(
        line.spans
            .iter()
            .all(|s| !s.style.add_modifier.contains(Modifier::UNDERLINED)),
        "underline cue is retired"
    );
    let bracket = line
        .spans
        .iter()
        .find(|s| s.content == "[")
        .expect("bracketed 5h bar");
    assert_eq!(bracket.style.fg, theme::dim().fg, "brackets stay plain dim");
    let countdown = line
        .spans
        .iter()
        .find(|s| s.content.ends_with("s "))
        .expect("refresh countdown");
    assert_eq!(countdown.style.fg, Some(theme::warning_color()));
}

#[test]
fn failed_row_colors_countdown_red() {
    let mut a = profile("a", 95.0, 10.0, 3600);
    a.fetch_status = Some(FetchStatus::Failed);
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    app.next_refresh_per_profile
        .lock()
        .unwrap()
        .insert("a".to_string(), now_ms() + 30_000);
    let widths = OverviewWidths::new(80, &app);
    let line = render_overview_row(&app, 0, &widths, false, true);
    let bracket = line
        .spans
        .iter()
        .find(|s| s.content == "[")
        .expect("bracketed 5h bar");
    assert_eq!(bracket.style.fg, theme::dim().fg, "brackets stay plain dim");
    let countdown = line
        .spans
        .iter()
        .find(|s| s.content.ends_with("s "))
        .expect("refresh countdown");
    assert_eq!(countdown.style.fg, Some(theme::danger_color()));
}

/// Every `(reset)` countdown suffix on a row, in column order.
fn reset_suffixes(line: &Line<'static>) -> Vec<Span<'static>> {
    line.spans
        .iter()
        .filter(|s| s.content.starts_with(" (") && s.content.ends_with(')'))
        .cloned()
        .collect()
}

/// The wiring, end to end. Both call sites used to pass a hardcoded `None` for
/// an api-key profile (no `UsageInfo` → no burn history → no rate), so a
/// third-party row's countdowns stayed faint however fast the window drained.
#[test]
fn third_party_row_drain_colors_both_countdowns() {
    let config = config_with(vec![third_party_profile(60.0, 30.0)], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(200, &app);
    assert!(
        widths.five_hour >= 26 && widths.seven_day >= 26,
        "test needs both columns wide enough to render a (reset) suffix",
    );
    let suffixes = reset_suffixes(&render_overview_row(&app, 0, &widths, false, true));
    assert_eq!(suffixes.len(), 2, "both windows render a (reset) suffix");
    for s in suffixes {
        assert_ne!(
            s.style.fg,
            theme::faint().fg,
            "a synthesized third-party window must still drain-color: {:?}",
            s.content,
        );
    }
}

/// The 7d half for an OAuth profile: its countdown drains off the window's own
/// average pace, so it colors even though the 5h burn history is empty.
#[test]
fn oauth_row_drain_colors_the_seven_day_countdown() {
    let mut a = profile("a", 95.0, 60.0, 9_000);
    a.usage.as_mut().unwrap().seven_day = Some(UsageWindow {
        utilization: 30.0,
        resets_at: Some(reset_in(7 * 86_400 / 2)),
    });
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(200, &app);
    let suffixes = reset_suffixes(&render_overview_row(&app, 0, &widths, false, true));
    assert_eq!(suffixes.len(), 2);
    assert_eq!(
        suffixes[0].style.fg,
        theme::faint().fg,
        "5h keeps the recent-burn source, which has no history here",
    );
    assert_ne!(
        suffixes[1].style.fg,
        theme::faint().fg,
        "7d drains off its own avg pace",
    );
}

/// Gap widening must work from the row's REAL width. `fixed_overview_width`
/// omits the TIMER_SLOT the row always renders, and widening gaps from that
/// undercounted figure overflows the row at narrow widths, clipping the tail
/// of the 5h column (observed at a 50-column pane: `[░░░░░]  0` with the `%`
/// pushed off-screen). Whenever the columns fit at all at minimum gaps, the
/// gap-widened layout must still fit.
#[test]
fn gap_widening_never_clips_the_row() {
    let a = profile("ax-main", 95.0, 10.0, 3600);
    let b = profile("ax-backup", 95.0, 20.0, 3600);
    let config = config_with(vec![a, b], Some("ax-main"), vec![]);
    let app = App::new(config);
    for width in 34u16..=200 {
        let w = OverviewWidths::new(width, &app);
        let min =
            fixed_overview_width(w.name, w.kind, w.five_hour, w.seven_day, w.route, 2) + TIMER_SLOT;
        if min > width as usize {
            // Below this the shrink loop has already bottomed out and the row
            // deliberately overflows-and-clips; gap widening isn't the cause.
            continue;
        }
        let used = fixed_overview_width(w.name, w.kind, w.five_hour, w.seven_day, w.route, w.gap)
            + TIMER_SLOT;
        assert!(
            used <= width as usize,
            "row overflows at width {width}: used {used} (gap {})",
            w.gap
        );
    }
}

/// A credentialed OAuth profile with no fetched `usage.plan` yet, so
/// `account_type_label` falls back to the token's `subscription_type` via
/// `PlanTier::from_subscription_type(..).display()`.
fn credentialed_profile(name: &str, subscription_type: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        bell_threshold: None,
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "tok".into(),
                refresh_token: None,
                expires_at: None,
                scopes: None,
                subscription_type: Some(subscription_type.into()),
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

/// The credentialed pulse arm must clamp the type-column label to
/// `widths.kind` exactly like the non-credentialed `fixed` arm does. A
/// long label ("Enterprise", 10 chars) must not overflow a narrow `kind`
/// column (6 chars) and bleed into the following gap/timer columns.
#[test]
fn credentialed_long_label_clamps_to_kind_width() {
    let a = credentialed_profile("acct", "enterprise");
    let config = config_with(vec![a], None, vec![]);
    let app = App::new(config);
    let widths = OverviewWidths::new(60, &app);
    assert_eq!(
        widths.kind, 6,
        "test assumes a 6-wide kind column at this pane width"
    );

    let line = render_overview_row(&app, 0, &widths, false, true);
    let chars: Vec<char> = line_text(&line).chars().collect();

    // 2 = cursor slot, 2 = marker slot (both always exactly 2 chars).
    let start = 2 + 2 + widths.name + widths.gap;
    let kind_field: String = chars[start..start + widths.kind].iter().collect();
    assert_eq!(
        kind_field, "Enter…",
        "type column must truncate+pad to exactly `kind` width"
    );
    assert_eq!(
        chars[start + widths.kind],
        ' ',
        "type column must not bleed into the following gap/timer columns"
    );
}
