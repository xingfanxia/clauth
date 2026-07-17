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
        harness: Default::default(),
        name: "tp".into(),
        base_url: Some("https://api.example.com".into()),
        api_key: Some("k".into()),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
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
        harness: Default::default(),
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(threshold),
        last_resort: false,
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

// ── account-email column (CAP-3) ─────────────────────────────────────────────
//
// The `email` column is carved purely from the width LEFT OVER once every
// other column is at full size — layouts without it must be pixel-identical,
// and it must never shrink an upstream column. The em-dash placeholder means
// "OAuth, anchor not seeded yet"; api-key/provider rows render a blank cell.

/// Two-profile roster used by every test in this section: an OAuth profile
/// and an api-key profile (index 1), so placeholder gating is exercised on
/// both row kinds.
fn email_fixture() -> App {
    let oauth = profile("ax-main", 95.0, 10.0, 3600);
    let mut api = profile("relay", 95.0, 0.0, 3600);
    api.base_url = Some("https://api.example.com".into());
    api.api_key = Some("sk-test".into());
    api.usage = None;
    let config = config_with(vec![oauth, api], Some("ax-main"), vec![]);
    App::new(config)
}

/// Layout invariants across widths. The 5-tuple equality is a regression
/// tripwire (today's implementation computes the upstream columns before it
/// reads `has_email`, so it cannot fail; it exists to catch a refactor that
/// lets the carve feed back into column sizing). The live protections are the
/// no-clip bound — REAL row content including the TIMER_SLOT that
/// `fixed_overview_width` omits — checked against BOTH the width model and
/// the actually-rendered line, and gap parity on ungranted layouts.
#[test]
fn email_column_never_disturbs_the_upstream_columns() {
    let app = email_fixture();
    let long = "a-very-long-account-email@example-domain.com";
    for width in [30u16, 48, 53, 58, 64, 81, 93, 102, 110, 124, 140, 200] {
        let plain = OverviewWidths::new(width, &app, false);
        let with = OverviewWidths::new(width, &app, true);
        assert_eq!(plain.account, 0, "no emails → no column at {width}");
        assert_eq!(
            (
                plain.name,
                plain.kind,
                plain.five_hour,
                plain.seven_day,
                plain.route
            ),
            (
                with.name,
                with.kind,
                with.five_hour,
                with.seven_day,
                with.route
            ),
            "regression tripwire: carve fed back into column sizing at {width}"
        );
        if with.account > 0 {
            let used = fixed_overview_width(
                with.name,
                with.kind,
                with.five_hour,
                with.seven_day,
                with.route,
                with.gap,
            ) + TIMER_SLOT
                + ACCOUNT_GAP
                + with.account;
            assert!(
                used <= width as usize,
                "granted layout clips the 5h column at width {width}: used {used}"
            );
            // Model ↔ renderer: the actually-rendered row must fit too, with
            // the widest possible email in the cell.
            let rendered = line_text(&render_overview_row(
                &app,
                0,
                &with,
                false,
                true,
                Some(long),
            ));
            assert!(
                rendered.chars().count() <= width as usize,
                "rendered row overflows at width {width}: {rendered:?}"
            );
        } else {
            // No column → the gap must not depend on has_email at all.
            assert_eq!(plain.gap, with.gap, "gap drifted with no column at {width}");
        }
        // Gap widening works from real spare in BOTH branches: whenever the
        // columns fit at all (upstream deliberately overflows-and-clips below
        // ~33 cols), the widened layout must still fit — the upstream bug was
        // gap widening from the TIMER_SLOT-undercounted figure clipping the
        // 5h column's `%` at narrow widths.
        let plain_min = fixed_overview_width(
            plain.name,
            plain.kind,
            plain.five_hour,
            plain.seven_day,
            plain.route,
            2,
        ) + TIMER_SLOT;
        if plain_min <= width as usize {
            let plain_used = fixed_overview_width(
                plain.name,
                plain.kind,
                plain.five_hour,
                plain.seven_day,
                plain.route,
                plain.gap,
            ) + TIMER_SLOT;
            assert!(
                plain_used <= width as usize,
                "gap widening overflows the plain layout at width {width}: used {plain_used}"
            );
        }
    }
}

/// The exact grant boundary and the cap. For this roster (max name 7 →
/// clamped to the 8 floor; narrow bands: kind 6 / 5h 12 / no 7d / no route)
/// the real row costs `base 33 + TIMER_SLOT 5`, so the column needs
/// `38 + ACCOUNT_GAP + ACCOUNT_MIN = 52` columns: one short of that gets
/// nothing, 52 gets exactly ACCOUNT_MIN. A very wide terminal caps the column
/// at ACCOUNT_MAX and flows the excess into the elastic gaps (clamped at 8).
#[test]
fn email_column_grant_boundary_and_cap() {
    let app = email_fixture();
    assert_eq!(OverviewWidths::new(51, &app, true).account, 0);
    assert_eq!(OverviewWidths::new(52, &app, true).account, ACCOUNT_MIN);
    let wide = OverviewWidths::new(300, &app, true);
    assert_eq!(wide.account, ACCOUNT_MAX);
    assert_eq!(wide.gap, 8, "excess spare beyond the cap widens gaps");
}

/// Cell semantics, pinned via em-dash DELTAS against the no-column layout of
/// the same row (the route and 7d columns legitimately render their own
/// em-dashes, so a bare `contains('—')` would be tautological):
/// - OAuth + cached email → the address renders (truncated to the column).
/// - OAuth + no email → exactly ONE extra em-dash (the pending placeholder).
/// - api-key profile → blank cell, ZERO extra em-dashes (not applicable is
///   not the same as pending — every other surface omits the field).
/// - column not granted → no cell at all.
#[test]
fn email_cell_semantics_by_profile_kind() {
    let app = email_fixture();
    let granted = OverviewWidths::new(160, &app, true);
    let plain = OverviewWidths::new(160, &app, false);
    assert!(granted.account >= ACCOUNT_MIN);

    let header = line_text(&overview_header(&granted));
    assert!(
        header.contains("email"),
        "header names the column: {header}"
    );

    let row = |widths: &OverviewWidths, idx: usize, email: Option<&str>| {
        line_text(&render_overview_row(&app, idx, widths, false, true, email))
    };
    let dashes = |s: &str| s.matches('—').count();

    // OAuth with an email: the address renders truncated, no added em-dash.
    let long = "a-very-long-account-email@example-domain.com";
    let with = row(&granted, 0, Some(long));
    let shown: String = long.chars().take(granted.account - 1).collect();
    assert!(
        with.contains(&format!("{shown}…")),
        "long email truncates with an ellipsis: {with}"
    );
    assert_eq!(dashes(&with), dashes(&row(&plain, 0, None)));

    // OAuth, anchor not seeded: exactly one extra em-dash — the placeholder.
    assert_eq!(
        dashes(&row(&granted, 0, None)),
        dashes(&row(&plain, 0, None)) + 1,
        "unseeded OAuth row must carry the pending placeholder"
    );

    // Api-key profile: blank cell — no placeholder for a profile kind that
    // categorically has no account email.
    assert_eq!(
        dashes(&row(&granted, 1, None)),
        dashes(&row(&plain, 1, None)),
        "api-key row must not render a placeholder"
    );

    // Column not granted (too narrow for even ACCOUNT_MIN) → no cell at all.
    let narrow = OverviewWidths::new(40, &app, true);
    assert_eq!(narrow.account, 0);
    assert!(
        !row(&narrow, 0, Some(long)).contains('@'),
        "no email cell without the column"
    );
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
    let widths = OverviewWidths::new(80, &app, false);
    let line = render_overview_row(&app, 0, &widths, false, true, None);
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
    let widths = OverviewWidths::new(80, &app, false);
    let text = line_text(&render_overview_row(&app, 0, &widths, false, true, None));
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
    let widths = OverviewWidths::new(80, &app, false);
    let line = render_overview_row(&app, 0, &widths, false, true, None);
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
    let widths = OverviewWidths::new(80, &app, false);
    let line = render_overview_row(&app, 0, &widths, false, true, None);
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
    let widths = OverviewWidths::new(200, &app, false);
    assert!(
        widths.five_hour >= 26 && widths.seven_day >= 26,
        "test needs both columns wide enough to render a (reset) suffix",
    );
    let suffixes = reset_suffixes(&render_overview_row(&app, 0, &widths, false, true, None));
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
    let widths = OverviewWidths::new(200, &app, false);
    let suffixes = reset_suffixes(&render_overview_row(&app, 0, &widths, false, true, None));
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
/// gap-widened layout must still fit. (Upstream's sweep, on the ungranted
/// no-email layout; the email tests above pin the granted-layout bound.)
#[test]
fn gap_widening_never_clips_the_row() {
    let a = profile("ax-main", 95.0, 10.0, 3600);
    let b = profile("ax-backup", 95.0, 20.0, 3600);
    let config = config_with(vec![a, b], Some("ax-main"), vec![]);
    let app = App::new(config);
    for width in 34u16..=200 {
        let w = OverviewWidths::new(width, &app, false);
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
        harness: Default::default(),
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
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
    let widths = OverviewWidths::new(60, &app, false);
    assert_eq!(
        widths.kind, 6,
        "test assumes a 6-wide kind column at this pane width"
    );

    let line = render_overview_row(&app, 0, &widths, false, true, None);
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

// CDX-2 acceptance: a codex profile with published passive usage renders the
// harness tag, the codex-slot active dot, and real usage bars — asserted on
// the rendered line, not eyeballed.
#[test]
fn codex_row_renders_harness_tag_and_usage_bars() {
    let mut cdx = profile("cdx-a", 95.0, 62.0, 3600);
    cdx.harness = crate::profile::Harness::Codex;
    let mut config = config_with(vec![cdx], None, vec![]);
    config.state.active_codex_profile = Some("cdx-a".into());
    let app = App::new(config);
    let widths = OverviewWidths::new(100, &app, false);
    let line = render_overview_row(&app, 0, &widths, false, true, None);
    let text = line_text(&line);
    assert!(text.contains("Codex"), "harness tag renders: {text}");
    assert!(text.contains('█'), "usage bar renders: {text}");
    assert!(text.contains('●'), "codex-slot active dot renders: {text}");
    assert!(text.contains("62"), "utilization figure renders: {text}");
}
