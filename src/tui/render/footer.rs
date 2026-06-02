//! Bottom strip: full-width key hints. Status moved to the header status row.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, ConfigFocus, Tab};
use super::super::theme;

/// Shown on every tab; the persistent navigation cue.
const TAB_NAV: (&str, &str) = ("⇥ ←→", "tabs");

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let tail: &[(&str, &str)] = match app.tab {
        Tab::Overview => &[
            ("↑↓", "move"),
            ("⇧↑↓", "reorder"),
            ("⏎", "switch"),
            ("n", "new"),
            ("r", "refresh"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Usage => &[
            ("↑↓", "account"),
            ("⏎", "switch"),
            ("r", "refresh"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Config => match app.config_focus {
            ConfigFocus::Profiles => &[
                ("↑↓", "account"),
                ("⏎", "configure"),
                ("n", "new"),
                ("?", "help"),
                ("q", "quit"),
            ],
            ConfigFocus::Actions => &[
                ("↑↓", "row"),
                ("⏎", "edit / toggle"),
                ("⎋", "back"),
                ("?", "help"),
            ],
        },
        Tab::Fallback => &[
            ("↑↓", "move"),
            ("⏎", "open"),
            ("r", "refresh"),
            ("?", "help"),
            ("q", "quit"),
        ],
    };

    let hints: Vec<(&str, &str)> = std::iter::once(TAB_NAV)
        .chain(tail.iter().copied())
        .collect();

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
