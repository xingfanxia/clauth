//! Bottom strip: key hints, or a footer alert in place when one is active.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{
    App, ConfigFocus, ConfigRow, FallbackHint, FooterAlert, GLOBAL_CONFIG_ROWS, GlobalConfigRow,
    PluginFocus, StatusFocus, Tab, TokenView, config_rows, fallback_hint, has_sub_focus,
};
use super::super::theme;

const TAB_NAV: (&str, &str) = ("←→", "tabs");

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // 1-col breathing room on each side; the alert row (which replaces this in
    // place) shares the same inset so the left margin never jumps.
    let area = inset_x(area, 1);

    // A live alert replaces the hint bar in place — one footer row, never stacked.
    if let Some(alert) = &app.footer_alert {
        draw_alert(frame, area, alert);
        return;
    }

    // `q` label: "back" in a sub-focus, "quit" at top level.
    // (While armed the alert row shows instead, so this label stays "quit".)
    let q_label: &str = if has_sub_focus(app) { "back" } else { "quit" };

    let tail: &[(&str, &str)] = match app.tab {
        Tab::Overview => &[("⇧↑↓", "reorder"), ("a", "actions"), ("?", "help")],
        Tab::Usage => &[
            ("↑↓", "account"),
            ("r", "refresh account"),
            ("a", "actions"),
            ("?", "help"),
        ],
        Tab::Tokens => match app.token_view {
            TokenView::Dashboard => &[
                ("↵", "models"),
                ("r", "reload"),
                ("c", "count cache"),
                ("?", "help"),
            ],
            TokenView::Models => &[("↑↓", "model"), ("c", "count cache"), ("?", "help")],
        },
        Tab::Setup => match app.config_focus {
            ConfigFocus::Profiles => &[
                ("↑↓", "account"),
                ("↵", "configure"),
                ("n", "new"),
                ("a", "actions"),
                ("?", "help"),
            ],
            ConfigFocus::Actions => {
                // Row-aware: the `model` row cycles on space; env rows edit a value
                // or open the add-env key editor.
                match config_rows(app).get(app.config_action_cursor) {
                    Some(ConfigRow::Model) => &[
                        ("↑↓", "row"),
                        ("space", "cycle"),
                        ("↵", "custom"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                    Some(ConfigRow::EnvEntry(_)) => &[
                        ("↑↓", "row"),
                        ("↵", "edit value"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                    Some(ConfigRow::EnvAdd) => &[
                        ("↑↓", "row"),
                        ("↵", "add env"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                    // The reveal chip has no `a` actions, so it isn't advertised.
                    Some(ConfigRow::ModelOverrideAdd) => {
                        &[("↑↓", "row"), ("↵", "add override"), ("?", "help")]
                    }
                    _ => &[
                        ("↑↓", "row"),
                        ("↵", "edit / toggle"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                }
            }
        },
        Tab::Config => {
            if app.refresh_interval_draft.is_some() {
                &[("↵", "save"), ("←→", "caret"), ("esc", "cancel")]
            } else if GLOBAL_CONFIG_ROWS
                .get(app.global_config_cursor)
                .is_some_and(|r| *r == GlobalConfigRow::RefreshInterval)
            {
                &[
                    ("↑↓", "row"),
                    ("space", "cycle"),
                    ("↵", "custom"),
                    ("?", "help"),
                ]
            } else {
                &[("↑↓", "row"), ("space/↵", "cycle / toggle"), ("?", "help")]
            }
        }
        Tab::Status => match app.status.focus {
            StatusFocus::List => &[
                ("↑↓", "incident"),
                ("↵", "open"),
                ("r", "refresh"),
                ("a", "actions"),
                ("?", "help"),
            ],
            StatusFocus::Detail => &[("↑↓", "scroll"), ("a", "actions"), ("?", "help")],
        },
        Tab::Plugin => {
            // `f` only fixes a row that actually offers one — don't advertise it as a
            // hint on rows where pressing it is a no-op.
            match (app.plugin.focus, app.plugin.selected_fix().is_some()) {
                (PluginFocus::List, true) => &[
                    ("↑↓", "row"),
                    ("↵", "detail"),
                    ("r", "refresh"),
                    ("f", "fix"),
                    ("?", "help"),
                ],
                (PluginFocus::List, false) => &[
                    ("↑↓", "row"),
                    ("↵", "detail"),
                    ("r", "refresh"),
                    ("?", "help"),
                ],
                (PluginFocus::Detail, true) => &[
                    ("↑↓", "scroll"),
                    ("r", "refresh"),
                    ("f", "fix"),
                    ("?", "help"),
                ],
                (PluginFocus::Detail, false) => {
                    &[("↑↓", "scroll"), ("r", "refresh"), ("?", "help")]
                }
            }
        }
        Tab::Fallback => match fallback_hint(app) {
            FallbackHint::Empty => &[("?", "help")],
            FallbackHint::ChainMember => &[
                ("↑↓", "move"),
                ("⇧↑↓", "reorder = priority"),
                ("↵", "open"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::ChainAdd => &[("↑↓", "move"), ("↵", "add"), ("?", "help")],
            FallbackHint::DetailThreshold => &[
                ("↑↓", "row"),
                ("+", "raise"),
                ("-", "lower"),
                ("↵", "type"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailThresholdEdit => {
                &[("↵", "save"), ("←→", "caret"), ("esc", "cancel")]
            }
            FallbackHint::DetailLastResort => &[
                ("↑↓", "row"),
                ("space/↵", "toggle"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailRemove => &[
                ("↑↓", "row"),
                ("↵", "remove"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailRemoveArmed => {
                &[("↵", "confirm remove"), ("esc", "cancel"), ("?", "help")]
            }
            FallbackHint::DetailAdd => &[("↑↓", "pick"), ("↵", "add"), ("?", "help")],
        },
    };

    // Suppress the trailing `q` hint only where `q` is fully captured by the
    // screen (threshold edit / armed-remove / refresh-interval edit own the
    // keyboard entirely). Every other sub-focus shows `q back` via `q_label`
    // per the cloudy-tui contract.
    let show_q = !((app.tab == Tab::Fallback
        && matches!(
            fallback_hint(app),
            FallbackHint::DetailThresholdEdit | FallbackHint::DetailRemoveArmed
        ))
        || (app.tab == Tab::Config && app.refresh_interval_draft.is_some()));

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
            theme::accent().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*label, theme::dim()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}

/// Shrink a rect by `pad` columns on each side (clamped), leaving the row intact.
fn inset_x(area: Rect, pad: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(pad),
        width: area.width.saturating_sub(pad.saturating_mul(2)),
        ..area
    }
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
