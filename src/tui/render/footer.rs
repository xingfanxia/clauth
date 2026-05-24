//! Bottom strip: full-width key hints. Status moved to the header status row.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Screen};
use super::super::theme;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let hints: &[(&str, &str)] = match app.screen {
        Screen::Overview => &[
            ("⏎", "switch"),
            ("m", "menu"),
            ("d", "details"),
            ("f", "chain"),
            ("r", "refresh"),
            ("t", "rotate all"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Screen::Chain => &[
            ("⏎", "open"),
            ("r", "refresh"),
            ("⎋", "back"),
            ("?", "help"),
        ],
        Screen::ProfileDetail { .. } => &[
            ("m", "menu"),
            ("r", "refresh"),
            ("⎋", "back"),
            ("?", "help"),
        ],
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
