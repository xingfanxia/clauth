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
mod plugin;
mod status;
mod tabs;
mod toasts;
mod tokens;
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
        Tab::Tokens => tokens::draw(frame, body_area, app),
        Tab::Setup => config::draw(frame, body_area, app),
        Tab::Fallback => chain::draw(frame, body_area, app),
        Tab::Config => global_config::draw(frame, body_area, app),
        Tab::Status => status::draw(frame, body_area, app),
        Tab::Plugin => plugin::draw(frame, body_area, app),
    }
    footer::draw(frame, chunks[2], app);

    toasts::draw(frame, area, app);

    if let Some(modal) = app.modals.last() {
        modals::draw(frame, area, app, modal);
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_mod.rs"]
mod render_smoke;
