//! Bottom strip: key hints, or a footer alert in place when one is active.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{
    App, ConfigFocus, FallbackFocus, FallbackHint, FooterAlert, Tab, fallback_hint,
};
use super::super::theme;

const TAB_NAV: (&str, &str) = ("←→", "tabs");

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // A live alert replaces the hint bar in place — one footer row, never stacked.
    if let Some(alert) = &app.footer_alert {
        draw_alert(frame, area, alert);
        return;
    }

    // `q` label: "back" in a sub-focus, "quit" at top level.
    // (While armed the alert row shows instead, so this label stays "quit".)
    let has_sub_focus = (app.tab == Tab::Setup && app.config_focus == ConfigFocus::Actions)
        || (app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail);
    let q_label: &str = if has_sub_focus { "back" } else { "quit" };

    let tail: &[(&str, &str)] = match app.tab {
        Tab::Overview => &[
            ("↑↓", "move"),
            ("⇧↑↓", "reorder"),
            ("⏎", "switch"),
            ("n", "new"),
            ("r", "refresh"),
            ("a", "actions"),
            ("?", "help"),
        ],
        Tab::Usage => &[
            ("↑↓", "account"),
            ("r", "refresh account"),
            ("a", "actions"),
            ("?", "help"),
        ],
        Tab::Setup => match app.config_focus {
            ConfigFocus::Profiles => &[
                ("↑↓", "account"),
                ("⏎", "configure"),
                ("n", "new"),
                ("a", "actions"),
                ("?", "help"),
            ],
            ConfigFocus::Actions => &[
                ("↑↓", "row"),
                ("⏎", "edit / toggle"),
                ("a", "actions"),
                ("⎋", "back"),
                ("?", "help"),
            ],
        },
        Tab::Config => &[
            ("↑↓", "row"),
            ("⏎", "cycle / toggle"),
            ("a", "actions"),
            ("?", "help"),
        ],
        Tab::Fallback => match fallback_hint(app) {
            FallbackHint::Empty => &[("?", "help")],
            FallbackHint::ChainMember => &[
                ("↑↓", "move"),
                ("⇧↑↓", "reorder"),
                ("⏎", "open"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::ChainAdd => &[("↑↓", "move"), ("⏎", "add"), ("?", "help")],
            FallbackHint::DetailThreshold => &[
                ("↑↓", "row"),
                ("+ -", "adjust"),
                ("⏎", "edit"),
                ("a", "actions"),
                ("⎋", "back"),
                ("?", "help"),
            ],
            FallbackHint::DetailThresholdEdit => &[("0-9", "type"), ("⏎", "save"), ("⎋", "cancel")],
            FallbackHint::DetailWrapOff => &[
                ("↑↓", "row"),
                ("⏎", "toggle"),
                ("a", "actions"),
                ("⎋", "back"),
                ("?", "help"),
            ],
            FallbackHint::DetailRemove => &[
                ("↑↓", "row"),
                ("⏎", "remove"),
                ("a", "actions"),
                ("⎋", "back"),
                ("?", "help"),
            ],
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
            Style::default().fg(theme::accent_color()).bold(),
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

/// Render a footer alert in place of the hint bar.
/// `! <message>` — glyph in `WARNING`, message in `TEXT_DIM`.
fn draw_alert(frame: &mut Frame<'_>, area: Rect, alert: &FooterAlert) {
    let FooterAlert::Warn(msg) = alert;
    let spans = vec![
        Span::styled("! ", Style::default().fg(theme::warning_color())),
        Span::styled(msg.as_str(), theme::dim()),
    ];
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}
