//! Bottom strip: full-width key hints. Status moved to the header status row.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, ConfigFocus, FallbackHint, Tab, fallback_hint};
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
        Tab::Fallback => match fallback_hint(app) {
            FallbackHint::Empty => &[("?", "help"), ("q", "quit")],
            FallbackHint::ChainMember => &[
                ("↑↓", "move"),
                ("⇧↑↓", "reorder"),
                ("⏎", "open"),
                ("?", "help"),
                ("q", "quit"),
            ],
            FallbackHint::ChainAdd => &[("↑↓", "move"), ("⏎", "add"), ("?", "help"), ("q", "quit")],
            FallbackHint::DetailThreshold => &[
                ("↑↓", "row"),
                ("+ -", "adjust"),
                ("⏎", "edit"),
                ("⎋", "back"),
                ("?", "help"),
            ],
            FallbackHint::DetailThresholdEdit => &[("0-9", "type"), ("⏎", "save"), ("⎋", "cancel")],
            FallbackHint::DetailWrapOff => {
                &[("↑↓", "row"), ("⏎", "toggle"), ("⎋", "back"), ("?", "help")]
            }
            FallbackHint::DetailRemove => {
                &[("↑↓", "row"), ("⏎", "remove"), ("⎋", "back"), ("?", "help")]
            }
            FallbackHint::DetailRemoveArmed => {
                &[("⏎", "confirm remove"), ("⎋", "cancel"), ("?", "help")]
            }
            FallbackHint::DetailAdd => {
                &[("↑↓", "pick"), ("⏎", "add"), ("⎋", "back"), ("?", "help")]
            }
        },
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
