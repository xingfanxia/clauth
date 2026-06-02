//! Rendering — single top-level `draw` fn dispatches the whole frame.
//!
//! Visual language (kept deliberately lean):
//!   - one content frame per tab, never nested boxes; structure comes from
//!     hairline rules (`format::rule` / `labeled_rule`) and whitespace
//!   - all chrome is lowercase and quiet (dim, not bold-uppercase); the active
//!     element is the only thing that wears sapphire
//!   - hairline rounded corners, elevation via background tone, no shadows
//!   - claude-orange reserved for the logo and the active-account marker
//!
//! Where new things go:
//!   - new tab → its own submodule + dispatch in `draw` + entry in `tabs.rs`
//!   - new modal variant → `modals.rs`
//!   - shared profile/usage formatters → `format.rs`
//!   - shared multi-tab widgets (profile selector) → `panes.rs`

mod chain;
mod config;
mod footer;
mod format;
mod header;
mod modals;
mod overview;
mod panes;
mod tabs;
mod toasts;
mod usage;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::Block;

use super::app::{App, Tab};
use super::theme;

/// Logo height; the header (logo + brand + count + tab bar) is this tall. The
/// content frame's top border doubles as the header/content separator, so no
/// extra rule row is needed.
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
    match app.tab {
        Tab::Overview => overview::draw(frame, chunks[1], app),
        Tab::Usage => usage::draw(frame, chunks[1], app),
        Tab::Config => config::draw(frame, chunks[1], app),
        Tab::Fallback => chain::draw(frame, chunks[1], app),
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
    use crate::profile::{AppConfig, AppState, Profile};
    use crate::tui::app::{App, ConfigFocus};
    use crate::usage::{UsageInfo, UsageWindow};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    fn oauth(name: &str, five: f64, seven: f64, auto: bool) -> Profile {
        Profile {
            name: name.to_string(),
            base_url: None,
            api_key: None,
            auto_start: auto,
            env: BTreeMap::new(),
            fallback_threshold: Some(80.0),
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
        }
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

    /// Every tab (and both Config focus states) must render without panicking
    /// and must paint the header. Guards against layout regressions and, more
    /// importantly, against a nested-lock hang in any render path.
    #[test]
    fn all_tabs_render() {
        let profiles = vec![
            oauth("uwuclxdy", 42.0, 18.0, true),
            oauth("work", 12.0, 3.0, false),
            oauth("spare", 0.0, 0.0, false),
        ];
        let names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();
        let config = AppConfig {
            state: AppState {
                active_profile: Some("uwuclxdy".to_string()),
                profiles: names.clone(),
                fallback_chain: vec!["uwuclxdy".to_string(), "work".to_string()],
                ..AppState::default()
            },
            profiles,
        };
        let mut app = App::new(config);
        for (tab, focus) in [
            (Tab::Overview, ConfigFocus::Profiles),
            (Tab::Usage, ConfigFocus::Profiles),
            (Tab::Config, ConfigFocus::Profiles),
            (Tab::Config, ConfigFocus::Actions),
            (Tab::Fallback, ConfigFocus::Profiles),
        ] {
            app.tab = tab;
            app.config_focus = focus;
            assert!(dump(&app, 90, 20).contains("clauth"));
        }
    }

    /// Empty-account state must also render on every tab without panicking.
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
}
