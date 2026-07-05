use super::*;
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::tui::app::App;
use crate::usage::{UsageInfo, UsageWindow};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::collections::BTreeMap;

fn oauth_profile(name: &str, five_hour_pct: f64) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        bell_threshold: None,
        credentials: None,
        usage: Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: five_hour_pct,
                resets_at: None,
            }),
            ..UsageInfo::default()
        }),
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn provider_profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: Some("https://api.example.com".to_string()),
        api_key: Some("key".to_string()),
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
        third_party_usage: None,
    }
}

fn app_with(profiles: Vec<Profile>, active: Option<&str>) -> App {
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let config = AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            profiles: names,
            ..AppState::default()
        },
        profiles,
    };
    App::new(config)
}

/// Renders only the header block (not the full frame) so these tests are
/// independent of body/footer layout.
fn render_header_rows(app: &App, width: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(width, 3)).unwrap();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, app);
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    (0..3u16)
        .map(|y| {
            (0..width)
                .map(|x| buf.content[(y as usize) * (width as usize) + (x as usize)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// Row 1 (account count / gauge / feed dot), columns past the claude-glyph
/// column (0..10) — the glyph itself uses `█`/block glyphs that would
/// otherwise collide with the gauge bar's own fill characters in a naive
/// whole-row substring check.
fn row1_content(app: &App, width: u16) -> String {
    render_header_rows(app, width)[1].chars().skip(10).collect()
}

fn gauge(name: &str, pct: Option<f64>) -> Option<ActiveGauge> {
    Some(ActiveGauge {
        name: name.to_string(),
        pct,
        style: Style::default(),
    })
}

// ── `gauge_fit` — steps 2-6 of the ladder (pure) ────────────────────────────

#[test]
fn gauge_fit_shows_full_name_bar_pct_when_roomy() {
    let fit = gauge_fit(100, 8, true, 2);
    assert_eq!(
        fit,
        GaugeFit {
            name_w: 8,
            bar_cells: 8,
            gap: 2,
            visible: true,
        }
    );
}

#[test]
fn gauge_fit_truncates_name_before_touching_bar() {
    let fit = gauge_fit(22, 8, true, 2);
    assert_eq!(fit.name_w, 6);
    assert_eq!(
        fit.bar_cells, 8,
        "bar must stay full while the name still has room to shrink"
    );
    assert!(fit.visible);
}

#[test]
fn gauge_fit_shrinks_bar_only_after_name_hits_its_floor() {
    let fit = gauge_fit(16, 8, true, 1);
    assert_eq!(
        fit.name_w, GAUGE_NAME_MIN,
        "name must already be at its floor"
    );
    assert_eq!(fit.bar_cells, 7);
    assert!(fit.visible);
}

#[test]
fn gauge_fit_drops_bar_before_name() {
    let fit = gauge_fit(11, 8, true, 1);
    assert_eq!(fit.bar_cells, 0, "bar drops entirely before the name does");
    assert_eq!(fit.name_w, GAUGE_NAME_MIN);
    assert!(fit.visible);
}

#[test]
fn gauge_fit_drops_name_only_after_bar_is_already_gone() {
    let fit = gauge_fit(7, 8, true, 1);
    assert_eq!(fit.bar_cells, 0);
    assert_eq!(fit.name_w, 0);
    assert!(fit.visible, "the percent figure alone should still render");
}

#[test]
fn gauge_fit_hides_entirely_below_the_percent_width() {
    let fit = gauge_fit(3, 8, true, 1);
    assert_eq!(fit, GaugeFit::HIDDEN);
}

#[test]
fn gauge_fit_provider_profile_never_shows_a_bar() {
    // `has_pct = false` (api-key/provider profile, no OAuth 5h window): only
    // the name collapses on the way down to the bare `—` placeholder.
    let roomy = gauge_fit(100, 10, false, 1);
    assert_eq!(
        roomy,
        GaugeFit {
            name_w: 10,
            bar_cells: 0,
            gap: 1,
            visible: true
        }
    );

    let tight = gauge_fit(8, 10, false, 1);
    assert_eq!(tight.bar_cells, 0);
    assert_eq!(tight.name_w, 6);

    let dash_only = gauge_fit(1, 10, false, 1);
    assert_eq!(dash_only.name_w, 0);
    assert!(
        dash_only.visible,
        "the dash alone still renders at its 1-cell floor"
    );
}

// ── `resolve_gauge_row` — steps 0-1 (row gap, account-count priority) ───────

#[test]
fn resolve_gauge_row_keeps_count_at_comfortable_gap_when_roomy() {
    let g = gauge("uwuclxdy", Some(42.0));
    let (show_count, fit) = resolve_gauge_row(60, 9, &g);
    assert!(show_count);
    assert_eq!(fit.gap, GAUGE_GAP_FULL);
    assert_eq!(fit.name_w, 8);
    assert_eq!(fit.bar_cells, GAUGE_BAR_FULL);
}

#[test]
fn resolve_gauge_row_shrinks_gap_before_dropping_count() {
    let g = gauge("uwuclxdy", Some(42.0));
    // 9 (count) + 2 (full gap) + 24 (full gauge) = 35 is the comfortable-gap
    // boundary: exactly at it nothing is sacrificed; one below it the gap
    // alone shrinks to 1 and everything else stays intact.
    let (show_count, fit) = resolve_gauge_row(35, 9, &g);
    assert!(show_count);
    assert_eq!(fit.gap, GAUGE_GAP_FULL, "exact fit keeps the full gap");
    let (show_count, fit) = resolve_gauge_row(34, 9, &g);
    assert!(show_count);
    assert_eq!(fit.gap, GAUGE_GAP_MIN);
    assert_eq!(fit.name_w, 8);
    assert_eq!(fit.bar_cells, GAUGE_BAR_FULL);
}

#[test]
fn resolve_gauge_row_drops_count_before_the_gauge_sacrifices_anything() {
    let g = gauge("uwuclxdy", Some(42.0));
    // 9 + 1 (min gap) + 22 (full gauge) = 32 is the min-gap boundary: exactly
    // at it the count still renders; one below it the count gives way while
    // the gauge stays completely intact.
    let (show_count, fit) = resolve_gauge_row(32, 9, &g);
    assert!(show_count, "exact min-gap fit keeps the count");
    assert_eq!(fit.gap, GAUGE_GAP_MIN);
    let (show_count, fit) = resolve_gauge_row(31, 9, &g);
    assert!(
        !show_count,
        "account count must give way before the gauge shrinks"
    );
    assert_eq!(
        fit.name_w, 8,
        "gauge itself is untouched once the count is gone"
    );
    assert_eq!(fit.bar_cells, GAUGE_BAR_FULL);
    assert_eq!(fit.gap, GAUGE_GAP_MIN);
}

#[test]
fn resolve_gauge_row_finally_hands_off_to_the_gauges_own_ladder() {
    let g = gauge("uwuclxdy", Some(42.0));
    // Below 22 not even the count-free full gauge fits — `gauge_fit` degrades it.
    let (show_count, fit) = resolve_gauge_row(16, 9, &g);
    assert!(!show_count);
    assert_eq!(fit.name_w, GAUGE_NAME_MIN);
    assert_eq!(fit.bar_cells, 7);
}

#[test]
fn resolve_gauge_row_is_absent_without_an_active_profile() {
    let (show_count, fit) = resolve_gauge_row(80, 9, &None);
    assert!(show_count);
    assert!(!fit.visible);
}

#[test]
fn resolve_gauge_row_provider_profile_uses_the_dash_tail() {
    let g = gauge("z.ai", None);
    let (show_count, fit) = resolve_gauge_row(60, 9, &g);
    assert!(show_count);
    assert_eq!(fit.bar_cells, 0);
    assert_eq!(fit.name_w, 4);
}

// ── Full render — integration-level sacrifice order ─────────────────────────

#[test]
fn header_renders_full_gauge_alongside_count_and_dot() {
    let app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    let row1 = row1_content(&app, 90);
    assert!(row1.contains("1 account"));
    assert!(row1.contains("uwuclxdy"));
    assert!(row1.contains("42%"));
    assert!(
        row1.contains('█'),
        "bar must render bare fill cells (no brackets)"
    );
    assert!(
        !row1.contains('['),
        "header gauge bar must stay bracket-less"
    );
    assert!(row1.contains("● status.claude.ai"));
}

#[test]
fn header_suppresses_gauge_in_compact_mode() {
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.compact = true;
    let row1 = row1_content(&app, 90);
    assert!(row1.contains("1 account"));
    assert!(
        !row1.contains("uwuclxdy"),
        "gauge must not render in compact mode"
    );
    assert!(row1.contains("● status.claude.ai"));
}

#[test]
fn header_hides_gauge_when_no_active_profile() {
    let app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], None);
    let row1 = row1_content(&app, 90);
    assert!(row1.contains("1 account"));
    assert!(!row1.contains("uwuclxdy"));
    assert!(row1.contains("● status.claude.ai"));
}

#[test]
fn header_renders_dash_for_provider_active_profile() {
    let app = app_with(vec![provider_profile("z.ai")], Some("z.ai"));
    let row1 = row1_content(&app, 90);
    assert!(row1.contains("z.ai"));
    assert!(
        row1.contains('—'),
        "provider profile shows a bare dash, no bar/percent"
    );
    assert!(
        !row1.contains('█'),
        "provider profile must not render a bar"
    );
}
