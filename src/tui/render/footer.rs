//! Bottom strip: full-width key hints. Status moved to the header status row.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Screen};
use super::super::theme;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // Footer stays minimal. The per-profile menu (m) is the canonical place
    // for the rest; the help modal (?) lists every binding.
    let hints: &[(&str, &str)] = match app.screen {
        Screen::Overview => &[
            ("⏎", "switch"),
            ("d", "details"),
            ("m", "menu"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Screen::Chain => &[("⏎", "open"), ("⎋", "back"), ("?", "help")],
        Screen::ProfileDetail { .. } => &[("⏎", "menu"), ("⎋", "back"), ("?", "help")],
    };

    let mut spans: Vec<Span<'_>> = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", theme::faint()));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default().fg(theme::ACCENT).bold(),
        ));
        spans.push(Span::styled(format!(" {label}"), theme::dim()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}
