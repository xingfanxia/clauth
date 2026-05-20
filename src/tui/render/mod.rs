//! Rendering — single top-level `draw` fn dispatches the whole frame.
//!
//! Every widget here pulls its colors from `super::theme` and its border set
//! from `symbols::border::ROUNDED`. cloudy-ui visual rules in play:
//!   - hairline rounded corners (no sharp 0)
//!   - elevation via background tone, no drop shadows
//!   - sapphire active state, claude-orange used sparingly (logo, eyebrow)
//!   - 11px tracked-uppercase labels → bold + dim in the terminal
//!   - sentence case throughout
//!
//! Where new things go:
//!   - new screen → its own submodule + dispatch in `draw`
//!   - new modal variant → `modals.rs`
//!   - shared profile/usage formatters → `format.rs`

mod chain;
mod detail;
mod footer;
mod format;
mod header;
mod modals;
mod overview;
mod toasts;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::Block;

use super::app::{App, Screen};
use super::theme;

pub(super) const LOGO_HEIGHT: u16 = 3;

pub(crate) fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let background = Block::default().style(theme::base());
    frame.render_widget(background, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(LOGO_HEIGHT),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);

    header::draw(frame, chunks[0], app);
    match app.screen {
        Screen::Overview => overview::draw(frame, chunks[1], app),
        Screen::Chain => chain::draw(frame, chunks[1], app),
        Screen::ProfileDetail { profile_index } => {
            detail::draw(frame, chunks[1], app, profile_index)
        }
    }
    footer::draw(frame, chunks[2], app);

    toasts::draw(frame, area, app);

    if let Some(modal) = app.modals.last() {
        modals::draw(frame, area, app, modal);
    }
}
