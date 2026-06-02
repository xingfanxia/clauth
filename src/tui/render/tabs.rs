//! Tab bar — the primary navigation surface. Lowercase labels; the active tab
//! is underlined in sapphire, the rest sit faint. Switching is ⇥ / ⇧⇥ / ← →.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Tab};
use super::super::theme;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", theme::faint()));
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
            spans.push(Span::styled(label, theme::faint()));
        }
    }

    let para = Paragraph::new(Line::from(spans)).style(theme::base());
    frame.render_widget(para, area);
}
