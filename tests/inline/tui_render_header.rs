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

/// Renders only the header block (sized by `header_height`) so these tests
/// are independent of body/footer layout.
fn render_header_rows(app: &App, width: u16) -> Vec<String> {
    let height = header_height(app);
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, app);
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf.content[(y as usize) * (width as usize) + (x as usize)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// A header row's content past the claude-glyph column (0..10) — the glyph
/// itself uses `█`/block glyphs that would otherwise collide with the gauge
/// bar's own fill characters in a naive whole-row substring check.
fn row_content(app: &App, width: u16, row: usize) -> String {
    render_header_rows(app, width)[row]
        .chars()
        .skip(10)
        .collect()
}

// ── `gauge_fit` — the collapse ladder (pure) ────────────────────────────────
//
// Full gauge width = name + 2 + [bar+2 brackets] + 1 + 4 ("100%").

#[test]
fn gauge_fit_shows_full_name_bar_pct_when_roomy() {
    let fit = gauge_fit(100, 8, true);
    assert_eq!(
        fit,
        GaugeFit {
            name_w: 8,
            bar_cells: 8,
            visible: true,
        }
    );
}

#[test]
fn gauge_fit_truncates_name_before_touching_bar() {
    // Full = 8 + 2 + 10 + 1 + 4 = 25; at 23 the name gives up exactly 2 cells.
    let fit = gauge_fit(23, 8, true);
    assert_eq!(fit.name_w, 6);
    assert_eq!(
        fit.bar_cells, 8,
        "bar must stay full while the name still has room to shrink"
    );
    assert!(fit.visible);
}

#[test]
fn gauge_fit_shrinks_bar_only_after_name_hits_its_floor() {
    // Name floored at 3: 3 + 2 + (bar+2) + 1 + 4 = bar + 12 → bar 7 at 19.
    let fit = gauge_fit(19, 8, true);
    assert_eq!(
        fit.name_w, GAUGE_NAME_MIN,
        "name must already be at its floor"
    );
    assert_eq!(fit.bar_cells, 7);
    assert!(fit.visible);
}

#[test]
fn gauge_fit_drops_bar_before_name() {
    // Below the bar floor (3 + 2 + 5 + 1 + 4 = 15) the bar drops entirely:
    // name(3) + 2 + pct(4) = 9 still fits at 14.
    let fit = gauge_fit(14, 8, true);
    assert_eq!(fit.bar_cells, 0, "bar drops entirely before the name does");
    assert_eq!(fit.name_w, GAUGE_NAME_MIN);
    assert!(fit.visible);
}

#[test]
fn gauge_fit_drops_name_only_after_bar_is_already_gone() {
    let fit = gauge_fit(7, 8, true);
    assert_eq!(fit.bar_cells, 0);
    assert_eq!(fit.name_w, 0);
    assert!(fit.visible, "the percent figure alone should still render");
}

#[test]
fn gauge_fit_hides_entirely_below_the_percent_width() {
    let fit = gauge_fit(3, 8, true);
    assert_eq!(fit, GaugeFit::HIDDEN);
}

#[test]
fn gauge_fit_provider_profile_never_shows_a_bar() {
    // `has_pct = false` (api-key/provider profile, no OAuth 5h window): only
    // the name collapses on the way down to the bare `—` placeholder.
    let roomy = gauge_fit(100, 10, false);
    assert_eq!(
        roomy,
        GaugeFit {
            name_w: 10,
            bar_cells: 0,
            visible: true
        }
    );

    // name + 2 + dash(1): at 9 the name shrinks to 6.
    let tight = gauge_fit(9, 10, false);
    assert_eq!(tight.bar_cells, 0);
    assert_eq!(tight.name_w, 6);

    let dash_only = gauge_fit(1, 10, false);
    assert_eq!(dash_only.name_w, 0);
    assert!(
        dash_only.visible,
        "the dash alone still renders at its 1-cell floor"
    );
}

// ── `header_height` — the gauge row is claimed only when it renders ─────────

#[test]
fn header_height_is_four_only_with_an_active_gauge() {
    let with_active = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    assert_eq!(header_height(&with_active), 4);

    let no_active = app_with(vec![oauth_profile("uwuclxdy", 42.0)], None);
    assert_eq!(header_height(&no_active), 3);

    let mut compact = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    compact.compact = true;
    assert_eq!(header_height(&compact), 3);
}

// ── Full render — row placement and gauge form ──────────────────────────────

#[test]
fn header_renders_gauge_on_its_own_row_below_the_feed_dot() {
    let app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    let row1 = row_content(&app, 90, 1);
    assert!(row1.contains("1 account"));
    assert!(row1.contains("● status.claude.ai"));
    assert!(
        !row1.contains("uwuclxdy"),
        "gauge must not share the feed-dot row"
    );

    let row2 = row_content(&app, 90, 2);
    assert!(row2.contains("uwuclxdy  ["));
    assert!(
        row2.contains("] 42%"),
        "bracketed bar with trailing percent"
    );
    assert!(row2.contains('█'), "bar renders fill cells");
    assert!(
        row2.trim_end().ends_with("42%"),
        "gauge right-aligns under the feed dot"
    );
}

#[test]
fn header_keeps_three_rows_in_compact_mode() {
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.compact = true;
    let rows = render_header_rows(&app, 90);
    assert_eq!(rows.len(), 3);
    assert!(rows[1].contains("1 account"));
    assert!(
        !rows.iter().any(|r| r.contains("uwuclxdy")),
        "gauge must not render in compact mode"
    );
    assert!(rows[1].contains("● status.claude.ai"));
}

#[test]
fn header_hides_gauge_when_no_active_profile() {
    let app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], None);
    let rows = render_header_rows(&app, 90);
    assert_eq!(rows.len(), 3, "no gauge row is reserved without a gauge");
    assert!(rows[1].contains("1 account"));
    assert!(!rows.iter().any(|r| r.contains("uwuclxdy")));
    assert!(rows[1].contains("● status.claude.ai"));
}

#[test]
fn header_renders_dash_for_provider_active_profile() {
    let app = app_with(vec![provider_profile("z.ai")], Some("z.ai"));
    let row2 = row_content(&app, 90, 2);
    assert!(row2.contains("z.ai"));
    assert!(
        row2.contains('—'),
        "provider profile shows a bare dash, no bar/percent"
    );
    assert!(
        !row2.contains('█'),
        "provider profile must not render a bar"
    );
}
