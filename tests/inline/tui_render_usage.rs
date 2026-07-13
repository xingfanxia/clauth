use super::*;

/// `bar_spans` overlays the `│` pace marker at its cell without ever changing
/// the bar's total width, whether the marker lands over the filled run (ahead of
/// pace) or the empty run (under pace). An out-of-range column draws no marker.
#[test]
fn bar_spans_places_marker_without_changing_width() {
    let fill = Style::default();
    let total =
        |spans: &[Span<'_>]| -> usize { spans.iter().map(|s| s.content.chars().count()).sum() };
    let marker_col = |spans: &[Span<'_>]| -> Option<usize> {
        let mut col = 0;
        for s in spans {
            if s.content == "│" {
                return Some(col);
            }
            col += s.content.chars().count();
        }
        None
    };

    // No marker requested: plain fill + empty, full width, no glyph.
    let plain = bar_spans(4, 10, fill, None);
    assert_eq!(total(&plain), 10);
    assert_eq!(marker_col(&plain), None);

    // Marker over the empty run (under pace) sits exactly at its column.
    let under = bar_spans(4, 10, fill, Some(7));
    assert_eq!(
        total(&under),
        10,
        "width unchanged with a marker over the empty run"
    );
    assert_eq!(marker_col(&under), Some(7));

    // Marker over the filled run (ahead of pace) — still one glyph, same width.
    let ahead = bar_spans(8, 10, fill, Some(3));
    assert_eq!(
        total(&ahead),
        10,
        "width unchanged with a marker over the filled run"
    );
    assert_eq!(marker_col(&ahead), Some(3));

    // Marker at the fill boundary lands on the first empty cell.
    let boundary = bar_spans(4, 10, fill, Some(4));
    assert_eq!(total(&boundary), 10);
    assert_eq!(marker_col(&boundary), Some(4));

    // Out-of-range column → no marker drawn.
    let oob = bar_spans(4, 10, fill, Some(10));
    assert_eq!(marker_col(&oob), None);
    assert_eq!(total(&oob), 10);
}

/// `stats_from_bars` keeps each bar's API label and source order (no inferred
/// window vocabulary, no reordering), puts absolute `used / total` on the eyebrow
/// `amount` (not the bar-line trailing), and leaves only the reset countdown on
/// the trailing.
#[test]
fn stats_from_bars_keeps_api_labels_and_source_order() {
    let now = crate::usage::now_epoch_secs();
    let bars = vec![
        // Far-future reset + absolute amounts, given first → stays first.
        tp_bar(
            "time limit",
            0.0,
            now + 30 * 86_400,
            Some(0.0),
            Some(1000.0),
        ),
        // Short reset, percentage-only → stays second (no reordering).
        tp_bar("tokens limit", 1.0, now + 4 * 3600, None, None),
    ];
    let stats = stats_from_bars(&bars, true, true);
    assert_eq!(stats[0].label, "time limit", "API label kept verbatim");
    assert_eq!(stats[1].label, "tokens limit");

    // Amounts live on the eyebrow now, not the bar-line trailing.
    assert_eq!(stats[0].amount, "0 / 1000");
    assert!(!stats[0].trailing.contains('/'));
    assert!(stats[0].trailing.contains("resets in"));

    // Percentage-only bar: no amount, countdown only.
    assert!(stats[1].amount.is_empty());
    assert!(stats[1].trailing.contains("resets in"));
}

/// Two bars sharing the same API label are NOT renamed — z.ai's pair of token
/// limits both read "tokens limit", in source order.
#[test]
fn stats_from_bars_does_not_rename_duplicate_labels() {
    let now = crate::usage::now_epoch_secs();
    let bars = vec![
        tp_bar("tokens limit", 0.0, now + 4 * 3600, None, None),
        tp_bar("tokens limit", 12.0, now + 6 * 86_400, None, None),
    ];
    let stats = stats_from_bars(&bars, true, true);
    assert_eq!(stats[0].label, "tokens limit");
    assert_eq!(stats[1].label, "tokens limit");
    assert_eq!(stats[0].pct, 0.0);
    assert_eq!(stats[1].pct, 12.0);
}

/// A bar whose label decodes to a window (`5h`/`7d`/`30d`) gets the OAuth window
/// predictions: a window-anchored average pace (sub-day → %/h, `<n>d` → %/d) and
/// the ideal-pace marker. Toggles gate them; non-window labels stay bare.
#[test]
fn stats_from_bars_fills_pace_for_windowed_labels() {
    let approx = |a: Option<f64>, b: f64| a.is_some_and(|v| (v - b).abs() < 0.1);
    let now = crate::usage::now_epoch_secs();
    // 5h window 4h in (resets in 1h), 20% used → 5 %/h, 80% of the way through.
    // 7d window 3.5d in, 35% used → 10 %/d, half elapsed.
    // 30d window 15d in, 30% used → 2 %/d (proves the new 30d duration arm).
    let bars = vec![
        tp_bar("5h", 20.0, now + 3600, None, None),
        tp_bar("7d", 35.0, now + 3 * 86_400 + 43_200, None, None),
        tp_bar("30d", 30.0, now + 15 * 86_400, None, None),
    ];

    let stats = stats_from_bars(&bars, true, true);
    assert_eq!(stats[0].rate_unit, "h");
    assert!(approx(stats[0].burn_rate, 5.0), "5h shows %/h average pace");
    assert!(approx(stats[0].pace_pct, 80.0), "5h ideal-pace marker");
    assert_eq!(stats[1].rate_unit, "d");
    assert!(
        approx(stats[1].burn_rate, 10.0),
        "7d shows %/d average pace"
    );
    assert!(
        approx(stats[2].burn_rate, 2.0),
        "30d window now resolves a pace"
    );

    // Both toggles off → no rate, no marker (matches the OAuth gating).
    let bare = stats_from_bars(&bars, false, false);
    assert!(bare.iter().all(|s| s.burn_rate.is_none()));
    assert!(bare.iter().all(|s| s.pace_pct.is_none()));

    // A label that isn't a `<n>h`/`<n>d` window carries no prediction.
    let other = stats_from_bars(
        &[tp_bar("balance", 50.0, now + 3600, None, None)],
        true,
        true,
    );
    assert!(other[0].burn_rate.is_none() && other[0].pace_pct.is_none());
}

fn tp_bar(
    label: &str,
    pct: f64,
    reset_secs: i64,
    used: Option<f64>,
    total: Option<f64>,
) -> crate::providers::UsageBar {
    crate::providers::UsageBar {
        label: label.to_string(),
        pct,
        resets_at: Some(crate::usage::epoch_secs_to_iso(reset_secs)),
        used,
        total,
    }
}

// ── oauth empty states ────────────────────────────────────────────────────────
//
// The oauth body must not spin "loading" forever: a credential-less profile is
// never fetched (issue #2's permanent "loading"), and a Failed fetch is
// terminal. Only a still-possible fetch may show "loading".

#[test]
fn empty_msg_credless_profile_is_terminal() {
    let profile = crate::testutil::blank_profile("a");
    assert_eq!(
        oauth_empty_msg(&profile),
        "no credentials, capture or log in"
    );
}

#[test]
fn empty_msg_failed_fetch_is_terminal() {
    let mut profile = crate::testutil::blank_profile("a");
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    profile.fetch_status = Some(FetchStatus::Failed);
    assert_eq!(oauth_empty_msg(&profile), "no usage available");
}

#[test]
fn empty_msg_pending_fetch_loads() {
    let mut profile = crate::testutil::blank_profile("a");
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    assert_eq!(oauth_empty_msg(&profile), "loading");
}

/// The `[ rate limited ]` suffix names which retry the countdown leads to
/// (`HeaderState.streak`) so a deep slot reads as stuck from the count alone;
/// a zero streak keeps the bare `retry in` suffix.
#[test]
fn rate_limited_suffix_counts_the_retry() {
    let mut profile = crate::testutil::blank_profile("a");
    profile.fetch_status = Some(FetchStatus::RateLimited);
    let header = |streak: u32| HeaderState {
        is_active: false,
        activity: ProfileActivity::Idle,
        next_refresh_ms: Some(now_ms() + 90_000),
        tick: 0,
        streak,
    };
    let text = |l: Line<'_>| -> String { l.spans.iter().map(|s| s.content.clone()).collect() };

    assert!(text(status_line(&profile, &header(7))).contains("7th retry in"));
    let bare = text(status_line(&profile, &header(0)));
    assert!(bare.contains("· retry in"));
    assert!(
        !bare.contains("th retry"),
        "a zero streak must not invent a retry count"
    );
}

/// The retry ordinal survives English's teen exceptions (`11th`–`13th` beat
/// the `1`/`2`/`3` last-digit rules).
#[test]
fn ordinal_covers_teens_and_edge_digits() {
    for (n, want) in [
        (1, "1st"),
        (2, "2nd"),
        (3, "3rd"),
        (4, "4th"),
        (11, "11th"),
        (12, "12th"),
        (13, "13th"),
        (21, "21st"),
        (22, "22nd"),
        (23, "23rd"),
        (111, "111th"),
    ] {
        assert_eq!(ordinal(n), want);
    }
}

// ── extra / spend credit bar ──────────────────────────────────────────────────
//
// `extra_usage` (legacy) and `spend` (newer) are the same credit cap on a real
// account. `spend` carries dollars; `extra_usage` reports bare minor units.

/// When both blocks are present the `extra` bar is suppressed (spend owns it),
/// and the legacy fallback scales its cents to dollars instead of showing 100×.
#[test]
fn extra_bar_dedups_against_spend_and_scales_cents() {
    let with = |extra: Option<crate::usage::ExtraUsage>, spend: Option<crate::usage::SpendInfo>| {
        let mut profile = crate::testutil::blank_profile("a");
        profile.usage = Some(crate::usage::UsageInfo {
            plan: None,
            five_hour: None,
            seven_day: None,
            weekly_scoped: Vec::new(),
            window_dollars: Vec::new(),
            extra_usage: extra,
            spend,
        });
        collect_stats(&profile)
    };
    let extra = crate::usage::ExtraUsage {
        is_enabled: true,
        monthly_limit: Some(5000.0),
        used_credits: Some(487.0),
        utilization: Some(9.74),
        currency: Some("USD".to_string()),
        ..Default::default()
    };
    let spend = crate::usage::SpendInfo {
        enabled: true,
        used: Some(4.87),
        limit: Some(50.0),
        percent: Some(10.0),
        currency: Some("USD".to_string()),
    };

    // Real account (both blocks): only `spend` renders, no duplicate `extra`.
    let both = with(Some(extra.clone()), Some(spend));
    assert!(both.iter().any(|s| s.label == "spend"));
    assert!(
        !both.iter().any(|s| s.label == "extra"),
        "extra suppressed while spend is visible"
    );

    // Legacy-only account: `extra` falls back, cents scaled to dollars, and the
    // figure rides the trailing line (where window bars show `resets in`).
    let legacy = with(Some(extra), None);
    let bar = legacy.iter().find(|s| s.label == "extra").unwrap();
    assert_eq!(bar.trailing, "$4.87 / $50.00");
    assert!(bar.amount.is_empty());
}
