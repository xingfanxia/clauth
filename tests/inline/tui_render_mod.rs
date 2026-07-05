use super::*;
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::status::{Impact, Incident, IncidentUpdate, UpdatePhase};
use crate::tui::app::{App, ConfigFocus, StatusFocus};
use crate::usage::{UsageInfo, UsageWindow};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::collections::BTreeMap;

fn oauth(name: &str, five: f64, seven: f64, auto: bool) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: auto,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(80.0),
        bell_threshold: None,
        credentials: None,
        usage: Some(UsageInfo {
            plan: None,
            five_hour: Some(UsageWindow {
                utilization: five,
                resets_at: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: seven,
                resets_at: None,
            }),
            ..UsageInfo::default()
        }),
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn demo_incidents() -> Vec<Incident> {
    vec![
        Incident {
            id: "6ptd5skgmy3v".into(),
            title: "opus 4.8 degraded service".into(),
            link: "https://stspg.io/abc".into(),
            phase: UpdatePhase::Monitoring,
            impact: Impact::Minor,
            started_ms: 1_780_740_881_000,
            resolved_ms: None,
            components: vec![
                ("claude.ai".into(), "operational".into()),
                ("Claude Code".into(), "degraded_performance".into()),
                ("Claude API".into(), "major_outage".into()),
            ],
            updates: vec![
                IncidentUpdate {
                    phase: UpdatePhase::Monitoring,
                    at_ms: 1_780_741_000_000,
                    text: "a fix has been implemented and we are monitoring".into(),
                    transitions: vec![
                        (
                            "claude.ai".into(),
                            "degraded_performance".into(),
                            "operational".into(),
                        ),
                        (
                            "Claude Code".into(),
                            "major_outage".into(),
                            "degraded_performance".into(),
                        ),
                    ],
                },
                IncidentUpdate {
                    phase: UpdatePhase::Investigating,
                    at_ms: 1_780_740_881_000,
                    text: "we are currently investigating this issue.".into(),
                    transitions: Vec::new(),
                },
            ],
        },
        Incident {
            id: "fprlnsvdnr2k".into(),
            title: "elevated errors on many claude models".into(),
            link: "https://stspg.io/def".into(),
            phase: UpdatePhase::Resolved,
            impact: Impact::None,
            started_ms: 1_780_684_084_000,
            resolved_ms: Some(1_780_686_904_000),
            components: Vec::new(),
            updates: vec![IncidentUpdate {
                phase: UpdatePhase::Resolved,
                at_ms: 1_780_686_904_000,
                text: "this incident has been resolved.".into(),
                transitions: Vec::new(),
            }],
        },
    ]
}

fn dump(app: &App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, app)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        for x in 0..w {
            out.push_str(buf.content[(y as usize) * (w as usize) + (x as usize)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn all_tabs_render() {
    let profiles = vec![
        oauth("uwuclxdy", 42.0, 18.0, true),
        oauth("work", 12.0, 3.0, false),
        oauth("spare", 0.0, 0.0, false),
    ];
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let config = AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: names.clone(),
            fallback_chain: vec!["uwuclxdy".into(), "work".into()],
            ..AppState::default()
        },
        profiles,
    };
    let mut app = App::new(config);
    app.status.incidents = demo_incidents();
    for (tab, focus) in [
        (Tab::Overview, ConfigFocus::Profiles),
        (Tab::Usage, ConfigFocus::Profiles),
        (Tab::Setup, ConfigFocus::Profiles),
        (Tab::Setup, ConfigFocus::Actions),
        (Tab::Fallback, ConfigFocus::Profiles),
        (Tab::Config, ConfigFocus::Profiles),
        (Tab::Status, ConfigFocus::Profiles),
    ] {
        app.tab = tab;
        app.config_focus = focus;
        assert!(dump(&app, 90, 20).contains("clauth"));
    }

    app.tab = Tab::Status;
    app.status.focus = StatusFocus::Detail;
    let detail = dump(&app, 90, 20);
    assert!(detail.contains("clauth"));
    assert!(
        detail.contains("TIMELINE"),
        "detail timeline eyebrow renders"
    );
    assert!(detail.contains("components"), "components row renders");
    assert!(detail.contains('→'), "a transition arrow renders");
    app.status.focus = StatusFocus::List;
}

#[test]
fn config_refresh_interval_custom_editor_renders() {
    use crate::tui::app::{GLOBAL_CONFIG_ROWS, GlobalConfigRow, InputState, Tab};
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::RefreshInterval)
        .unwrap();

    app.refresh_interval_draft = Some(InputState::new("45"));
    let valid = dump(&app, 90, 20);
    assert!(valid.contains("refresh"), "refresh row label renders");
    assert!(valid.contains("45"), "typed seconds render in the field");
    assert!(valid.contains("10–3600 s"), "valid-range tooltip renders");

    // An out-of-range buffer still renders (DANGER value) without panicking.
    app.refresh_interval_draft = Some(InputState::new("99999"));
    let invalid = dump(&app, 90, 20);
    assert!(invalid.contains("99999"));
    assert!(invalid.contains("10–3600 s"));

    // At rest, a custom (non-preset) interval shows the real value rather than
    // mis-bracketing the nearest preset chip.
    app.refresh_interval_draft = None;
    app.refresh_interval
        .store(200_000, std::sync::atomic::Ordering::Relaxed);
    let custom = dump(&app, 90, 20);
    assert!(
        custom.contains("200s"),
        "custom interval shows its real value"
    );
}

#[test]
fn fallback_threshold_editor_shows_range_tooltip() {
    use crate::tui::app::{FallbackFocus, InputState, Tab};
    let profiles = vec![oauth("uwuclxdy", 42.0, 18.0, true)];
    let config = AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: vec!["uwuclxdy".into()],
            fallback_chain: vec!["uwuclxdy".into()],
            ..AppState::default()
        },
        profiles,
    };
    let mut app = App::new(config);
    app.tab = Tab::Fallback;
    app.fallback_focus = FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = 0; // FALLBACK_ROWS[0] == `rotate at`

    // A valid in-range buffer shows the range tooltip (mirrors the refresh editor).
    app.fallback_threshold_draft = Some(InputState::new("70"));
    let valid = dump(&app, 90, 20);
    assert!(valid.contains("rotate at"), "threshold row label renders");
    assert!(
        valid.contains("70 %"),
        "typed value renders with the unit after the caret cell"
    );
    assert!(valid.contains("0–100 %"), "valid-range tooltip renders");

    // An out-of-range buffer still renders (DANGER) with the same range tooltip.
    app.fallback_threshold_draft = Some(InputState::new("150"));
    let invalid = dump(&app, 90, 20);
    assert!(invalid.contains("150"));
    assert!(invalid.contains("0–100 %"));
}

#[test]
fn capture_name_caret_follows_edit_position() {
    use crate::actions::CaptureSnapshot;
    use crate::tui::app::{CaptureNameForm, InputState, Modal};

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });

    let mut input = InputState::new("alice");
    input.left();
    input.left(); // caret now sits before "ce", not at the end
    assert_ne!(input.cursor, input.value.len());

    app.modals.push(Modal::CaptureName(CaptureNameForm {
        snapshot: Box::new(CaptureSnapshot {
            credentials: None,
            base_url: None,
            api_key: None,
        }),
        input,
        from_divergence: false,
    }));

    let (w, h) = (90u16, 20u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, &app)).unwrap();
    let mid_caret = term.get_cursor_position().unwrap();

    // Move the caret to the end of the same text and re-render: the terminal
    // caret column must shift right, i.e. it tracks `InputState::cursor`
    // instead of always snapping to the end of the string.
    if let Some(Modal::CaptureName(form)) = app.modals.last_mut() {
        form.input.end();
    }
    term.draw(|f| super::draw(f, &app)).unwrap();
    let end_caret = term.get_cursor_position().unwrap();

    assert_eq!(mid_caret.y, end_caret.y, "caret stays on the input row");
    assert!(
        mid_caret.x < end_caret.x,
        "caret column must follow the edit position (mid={mid_caret:?}, end={end_caret:?})"
    );
}

#[test]
fn status_selected_row_tint_spans_both_lines() {
    let config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    let mut app = App::new(config);
    app.status.incidents = demo_incidents();
    app.tab = Tab::Status;
    app.status.focus = StatusFocus::List;
    app.status.cursor = 0;

    let (w, h) = (90u16, 20u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();

    let hover = crate::tui::theme::bg_hover();
    let cell = |x: u16, y: u16| &buf.content[(y as usize) * (w as usize) + (x as usize)];

    let mut caret_y = None;
    for y in 0..h {
        for x in 0..w {
            if cell(x, y).symbol() == "❯" {
                caret_y = Some(y);
                break;
            }
        }
        if caret_y.is_some() {
            break;
        }
    }
    let caret_y = caret_y.expect("selected incident renders a caret");

    let row_has_full_tint = |y: u16| {
        let caret_x = (0..w).find(|&x| cell(x, caret_y).symbol() == "❯").unwrap();
        let mut saw_filler_tint = false;
        for x in caret_x..w {
            let c = cell(x, y);
            if c.style().bg != Some(hover) {
                return saw_filler_tint && x > caret_x + 4;
            }
            if c.symbol() == " " {
                saw_filler_tint = true;
            }
        }
        saw_filler_tint
    };

    assert!(
        row_has_full_tint(caret_y),
        "title row tint must span the full content width"
    );
    assert!(
        row_has_full_tint(caret_y + 1),
        "pill row tint must span the full content width"
    );
}

#[test]
fn empty_state_renders() {
    let config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    let mut app = App::new(config);
    for tab in Tab::ALL {
        app.tab = tab;
        assert!(dump(&app, 90, 20).contains("clauth"));
    }
}

#[test]
fn banner_renders() {
    use crate::tui::app::{Banner, BannerSeverity};

    let profiles = vec![oauth("alpha", 99.0, 50.0, true)];
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let config = AppConfig {
        state: AppState {
            active_profile: None,
            profiles: names,
            ..AppState::default()
        },
        profiles,
    };
    let mut app = App::new(config);
    app.banner = Some(Banner {
        severity: BannerSeverity::Danger,
        message: "all accounts spent · switch to a profile to resume".to_string(),
    });

    let screen = dump(&app, 90, 20);
    assert!(screen.contains("all accounts spent"), "banner text missing");
    assert!(
        screen.contains("clauth"),
        "header missing with banner active"
    );

    app.banner = None;
    let screen_no_banner = dump(&app, 90, 20);
    assert!(
        !screen_no_banner.contains("all accounts spent"),
        "banner text present when banner is None"
    );
}
