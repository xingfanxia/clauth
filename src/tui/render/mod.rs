//! Single top-level `draw` fn dispatches the whole frame.
//!
//! Visual language: one content frame per tab, no nested boxes; chrome is dim
//! lowercase; sapphire = active element; orange = logo + active-account marker.
//!
//! Where new things go:
//!   - new tab → submodule + dispatch in `draw` + entry in `tabs.rs`
//!   - new modal → `modals.rs`
//!   - shared formatters → `format.rs`; shared widgets → `panes.rs`

mod banner;
mod chain;
mod config;
mod footer;
mod format;
mod global_config;
mod header;
mod modals;
mod overview;
mod panes;
mod status;
mod tabs;
mod toasts;
mod usage;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::Block;

use super::app::{App, Tab};
use super::theme;

/// Content frame's top border doubles as the header/content separator.
pub(super) const HEADER_HEIGHT: u16 = 3;

pub(crate) fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let background = Block::default().style(theme::base());
    frame.render_widget(background, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(HEADER_HEIGHT),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);

    header::draw(frame, chunks[0], app);

    // When a banner is active, carve one row off the top of the body area.
    let body_area = if let Some(b) = &app.banner {
        let body_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(chunks[1]);
        banner::draw(frame, body_chunks[0], b);
        body_chunks[1]
    } else {
        chunks[1]
    };

    match app.tab {
        Tab::Overview => overview::draw(frame, body_area, app),
        Tab::Usage => usage::draw(frame, body_area, app),
        Tab::Setup => config::draw(frame, body_area, app),
        Tab::Fallback => chain::draw(frame, body_area, app),
        Tab::Config => global_config::draw(frame, body_area, app),
        Tab::Status => status::draw(frame, body_area, app),
    }
    footer::draw(frame, chunks[2], app);

    toasts::draw(frame, area, app);

    if let Some(modal) = app.modals.last() {
        modals::draw(frame, area, app, modal);
    }
}

#[cfg(test)]
mod render_smoke {
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

    /// Two fixture incidents so both Status panels exercise content paths: one
    /// active (monitoring) with impact + mixed-status components + a multi-
    /// transition update, one resolved with no impact.
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
                // Mixed snapshot statuses; names already paren-stripped at parse.
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
                        // Multi-transition update with varied target statuses.
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

    /// Every tab (and both Config focus states) must render without panic.
    /// Also guards against nested-lock hangs in any render path.
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
        // Seed the Status feed so both panels hit content paths.
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

        // Status detail focus must also render (descended pane), with the new
        // detail rows: timeline eyebrow, components row, transition arrow.
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

    /// The selected incident's `BG_HOVER` tint must cover BOTH of its rows across
    /// the full content width (incl. trailing filler) — the ratatui filler-tint
    /// gotcha. Settles the auditor's `Color::Reset` concern with a buffer probe.
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

        // Find the row carrying the selection caret in the left panel.
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

        // Probe the content span on the caret row and the row below: every cell
        // from the caret rightward must carry BG_HOVER until the padding/border
        // edge, including trailing filler.
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

    /// Banner row renders without panic and its message text is visible.
    /// Also confirms the banner is absent when no condition holds.
    #[test]
    fn banner_renders() {
        use crate::tui::app::{Banner, BannerSeverity};

        let profiles = vec![oauth("alpha", 99.0, 50.0, true)];
        let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
        let config = AppConfig {
            state: AppState {
                // active_profile = None + profiles present → all-spent condition
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

        // No banner → message absent.
        app.banner = None;
        let screen_no_banner = dump(&app, 90, 20);
        assert!(
            !screen_no_banner.contains("all accounts spent"),
            "banner text present when banner is None"
        );
    }
}
