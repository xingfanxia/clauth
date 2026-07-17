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
        last_resort: false,
        max_auto_spend: None,
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

fn hybrid_creds() -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
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
    crate::testutil::buffer_rows(term.backend().buffer())
        .into_iter()
        .map(|r| r + "\n")
        .collect()
}

#[test]
fn a_two_line_toast_bolds_the_head_and_dims_the_detail() {
    use crate::tui::app::ToastKind;
    use ratatui::style::Modifier;

    // Centralized diagnostics render a `head\ndetail` body: head bold, detail
    // dim below it. No toast carried a detail line before, so this render path
    // (`toasts::draw`'s detail loop) was never exercised until now.
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.toast(ToastKind::Danger, "QHEAD\nQNOTE");

    let (w, h) = (80u16, 24u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let out = dump(&app, w, h);

    let modifier_at = |needle: &str| -> Modifier {
        let (row, line) = out
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` missing from toast:\n{out}"));
        let col = line.find(needle).unwrap();
        buf.content[row * (w as usize) + col].modifier
    };

    assert!(
        modifier_at("QHEAD").contains(Modifier::BOLD),
        "toast head is bold:\n{out}"
    );
    assert!(
        !modifier_at("QNOTE").contains(Modifier::BOLD),
        "toast detail line is dim, not bold:\n{out}"
    );
}

#[test]
fn login_modal_drops_the_url_and_offers_a_retry() {
    use crate::tui::app::{LoginSession, LoginStage, Modal, Tab};
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Setup;
    app.login = Some(LoginSession {
        name: "fresh".to_string(),
        is_new: true,
        generation: 1,
        url: Some("https://claude.com/cai/oauth/authorize?client_id=redacted".to_string()),
        stage: LoginStage::WaitingBrowser,
    });
    app.modals.push(Modal::Login);

    let out = dump(&app, 80, 24);
    assert!(
        out.contains("open the browser again"),
        "the login modal offers a browser retry:\n{out}",
    );
    assert!(
        !out.contains("oauth/authorize"),
        "the wrapped authorize URL is no longer rendered inline:\n{out}",
    );
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
    assert!(valid.contains("10-3600 s"), "valid-range tooltip renders");

    // An out-of-range buffer still renders (DANGER value) without panicking.
    app.refresh_interval_draft = Some(InputState::new("99999"));
    let invalid = dump(&app, 90, 20);
    assert!(invalid.contains("99999"));
    assert!(invalid.contains("10-3600 s"));

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
    assert!(valid.contains("0-100 %"), "valid-range tooltip renders");

    // An out-of-range buffer still renders (DANGER) with the same range tooltip.
    app.fallback_threshold_draft = Some(InputState::new("150"));
    let invalid = dump(&app, 90, 20);
    assert!(invalid.contains("150"));
    assert!(invalid.contains("0-100 %"));
}

#[test]
fn setup_delete_row_hint_names_usage_history() {
    use crate::tui::app::{ConfigRow, Tab, config_rows};
    let profiles = vec![oauth("uwuclxdy", 42.0, 18.0, true)];
    let config = AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: vec!["uwuclxdy".into()],
            ..AppState::default()
        },
        profiles,
    };
    let mut app = App::new(config);
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::Delete)
        .unwrap();

    // Delete removes the whole profile dir, usage_history.jsonl included, so the
    // hint must not read as if history survives (unlike delete-credentials).
    let out = dump(&app, 120, 30);
    assert!(
        out.contains("usage history included"),
        "delete-row hint names usage history:\n{out}",
    );
}

#[test]
fn setup_api_account_shows_relogin_and_logout_rows() {
    use crate::tui::app::{ConfigRow, Tab, config_rows};
    let mut api = oauth("acme", 0.0, 0.0, false);
    api.base_url = Some("https://api.example.com".to_string());
    api.api_key = Some("sk-secret".to_string());
    api.usage = None;
    let config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            ..AppState::default()
        },
        profiles: vec![api],
    };
    let mut app = App::new(config);
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    // Park the cursor on the login row so its API hint renders too.
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::Login)
        .expect("api account shows a login row");

    let out = dump(&app, 120, 30);
    assert!(
        out.contains("re-login"),
        "api account with a key shows a re-login row:\n{out}",
    );
    assert!(
        out.contains("log out"),
        "api account with a key shows a log-out row:\n{out}",
    );
    assert!(
        out.contains("re-enter the base url"),
        "the login row's hint describes the API re-entry:\n{out}",
    );
}

/// A hybrid (stored OAuth pair + base url) with no api key: the login/log-out
/// rows must read off the credential that exists, or the tab reads "logged out"
/// over a live token.
#[test]
fn setup_hybrid_account_reads_logged_in_on_its_oauth_pair() {
    use crate::tui::app::{ConfigRow, Tab, config_rows};
    let mut hybrid = oauth("acme", 0.0, 0.0, false);
    hybrid.base_url = Some("http://127.0.0.1:1234".to_string());
    hybrid.credentials = Some(hybrid_creds());
    hybrid.usage = None;
    let config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            ..AppState::default()
        },
        profiles: vec![hybrid],
    };
    let mut app = App::new(config);
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    // Park on the login row (always present) so its hint renders too.
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::Login)
        .expect("every account shows a login row");

    let out = dump(&app, 120, 30);
    assert!(
        out.contains("re-login"),
        "a stored OAuth pair reads as logged in:\n{out}",
    );
    assert!(
        out.contains("log out"),
        "a stored OAuth pair keeps the log-out row:\n{out}",
    );
    assert!(
        out.contains("browser OAuth login"),
        "the login row's hint describes the OAuth mint:\n{out}",
    );
}

/// Same typing rule for the log-out row's own hint: a hybrid holding both an api
/// key and an OAuth pair logs out of the pair, and the copy must say so.
#[test]
fn setup_hybrid_logout_hint_names_the_oauth_login() {
    use crate::tui::app::{ConfigRow, Tab, config_rows};
    let mut hybrid = oauth("acme", 0.0, 0.0, false);
    hybrid.base_url = Some("https://api.example.com".to_string());
    hybrid.api_key = Some("sk-secret".to_string());
    hybrid.credentials = Some(hybrid_creds());
    hybrid.usage = None;
    let config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            ..AppState::default()
        },
        profiles: vec![hybrid],
    };
    let mut app = App::new(config);
    app.tab = Tab::Setup;
    app.config_focus = ConfigFocus::Actions;
    app.config_action_cursor = config_rows(&app)
        .iter()
        .position(|r| *r == ConfigRow::DeleteCreds)
        .expect("an api key alone already shows the log-out row");

    let out = dump(&app, 120, 30);
    assert!(
        out.contains("clears the stored OAuth"),
        "the log-out hint names the OAuth login it clears:\n{out}",
    );
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
            account_uuid: None,
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

// A filter that empties the grouped model list (set from the menu while on the
// Models view) must explain itself — not fall back to the shared account
// empty-state ("no accounts yet · n to create one").
#[test]
fn tokens_models_view_empty_filter_names_the_filter() {
    use crate::tokens::{ModelTokens, TokenStats};
    use crate::tui::app::{TokenFilter, TokenView};

    let profiles = vec![oauth("uwuclxdy", 42.0, 18.0, true)];
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    let config = AppConfig {
        state: AppState {
            active_profile: Some("uwuclxdy".into()),
            profiles: names,
            ..AppState::default()
        },
        profiles,
    };
    let mut app = App::new(config);
    app.tab = Tab::Tokens;
    app.token_view = TokenView::Models;
    app.token_filter = TokenFilter::Others;
    app.token_stats = Some(TokenStats {
        models: vec![ModelTokens {
            model: "claude-opus-4-8".to_string(),
            input: 10_000_000,
            ..Default::default()
        }],
        ..Default::default()
    });

    let out = dump(&app, 100, 30);
    assert!(
        out.contains("no models match the filter"),
        "filtered-empty models view must name the filter:\n{out}"
    );
    assert!(
        !out.contains("no accounts yet"),
        "must not fall back to the accounts empty state:\n{out}"
    );
}
