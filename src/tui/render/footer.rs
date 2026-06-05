//! Bottom strip: full-width key hints. Status moved to the header status row.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, ConfigFocus, FallbackFocus, FallbackHint, Tab, fallback_hint};
use super::super::theme;

const TAB_NAV: (&str, &str) = ("←→", "tabs");

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // `q` label: "back" in a sub-focus, "press q again" when armed, "quit" at top level.
    let has_sub_focus = (app.tab == Tab::Config && app.config_focus == ConfigFocus::Actions)
        || (app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail);
    let q_label: &str = if has_sub_focus {
        "back"
    } else if app.armed_quit {
        "press q again"
    } else {
        "quit"
    };

    let tail: &[(&str, &str)] = match app.tab {
        Tab::Overview => &[
            ("↑↓", "move"),
            ("⇧↑↓", "reorder"),
            ("⏎", "switch"),
            ("n", "new"),
            ("r", "refresh"),
            ("?", "help"),
        ],
        Tab::Usage => &[("↑↓", "account"), ("r", "refresh account"), ("?", "help")],
        Tab::Config => match app.config_focus {
            ConfigFocus::Profiles => &[
                ("↑↓", "account"),
                ("⏎", "configure"),
                ("n", "new"),
                ("?", "help"),
            ],
            ConfigFocus::Actions => &[
                ("↑↓", "row"),
                ("⏎", "edit / toggle"),
                ("⎋", "back"),
                ("?", "help"),
            ],
        },
        Tab::Fallback => match fallback_hint(app) {
            FallbackHint::Empty => &[("?", "help")],
            FallbackHint::ChainMember => &[
                ("↑↓", "move"),
                ("⇧↑↓", "reorder"),
                ("⏎", "open"),
                ("?", "help"),
            ],
            FallbackHint::ChainAdd => &[("↑↓", "move"), ("⏎", "add"), ("?", "help")],
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

    // Suppress the trailing `q` hint for screens where `q` doesn't apply
    // (threshold edit owns the keyboard entirely).
    let show_q = !matches!(
        fallback_hint(app),
        FallbackHint::DetailThresholdEdit
            | FallbackHint::DetailRemoveArmed
            | FallbackHint::DetailAdd
            | FallbackHint::DetailThreshold
            | FallbackHint::DetailWrapOff
            | FallbackHint::DetailRemove
    ) || app.tab != Tab::Fallback;

    // Suppress `q` on Config Actions too (⎋ backs out).
    let show_q = show_q && app.config_focus != ConfigFocus::Actions;

    let mut hints: Vec<(&str, &str)> = std::iter::once(TAB_NAV)
        .chain(tail.iter().copied())
        .collect();

    if show_q {
        hints.push(("q", q_label));
    }

    let mut spans: Vec<Span<'_>> = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", theme::faint()));
        }
        spans.push(Span::styled(
            *key,
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
