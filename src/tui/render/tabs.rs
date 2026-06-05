//! Tab bar — the primary navigation surface. Lowercase labels; the active tab
//! is underlined in sapphire, the rest sit dim. Switching is ← →.
//! An inactive tab with a pending background event takes that event's semantic
//! color until the user visits it (clears in `switch_tab`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Tab, ToastKind};
use super::super::theme;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", theme::dim()));
        }
        let label = tab.title().to_lowercase();
        if *tab == app.tab {
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
        } else {
            let color = activity_color(app.tab_activity[tab.index()]);
            spans.push(Span::styled(label, Style::default().fg(color)));
        }
    }

    let para = Paragraph::new(Line::from(spans)).style(theme::base());
    frame.render_widget(para, area);
}

/// Map a background activity kind to its semantic label color. `None` → TEXT_DIM
/// (the standard inactive tab color).
fn activity_color(activity: Option<ToastKind>) -> Color {
    match activity {
        None => theme::TEXT_DIM,
        Some(ToastKind::Success) => theme::SUCCESS,
        Some(ToastKind::Danger) => theme::DANGER,
        Some(ToastKind::Warning) => theme::WARNING,
        Some(ToastKind::Info) => theme::TEXT,
    }
}
