//! Shared widgets: the bordered section box every pane uses, and the account
//! picker shared by the Usage and Config tabs.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph};

use super::super::app::App;
use super::super::theme;

/// Width of the account picker column on the master-detail tabs.
pub(super) const SELECTOR_WIDTH: u16 = 24;

/// Bordered section box: rounded hairline, lowercase dim title, a one-column
/// left/right margin (no vertical padding). The border turns sapphire when the
/// pane holds focus — the "active pane" indicator.
pub(super) fn section_box(title: &str, focused: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(theme::ACCENT)
    } else {
        Style::default().fg(theme::LINE)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(border_style)
        .title(Line::from(Span::styled(format!(" {title} "), theme::dim())))
        .padding(Padding::horizontal(1))
}

/// Account picker box. `focused` accents the border and the selection bar so
/// it's obvious which pane ↑↓ drives.
pub(super) fn draw_profile_selector(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    selected: usize,
    focused: bool,
) {
    let block = section_box("accounts", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let cfg = app.config();
    if cfg.profiles.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("no accounts yet", theme::muted())))
                .style(theme::base()),
            inner,
        );
        return;
    }

    let items: Vec<ListItem<'_>> = cfg
        .profiles
        .iter()
        .map(|p| {
            let name_style = if cfg.is_active(&p.name) {
                Style::default().fg(theme::ACCENT_2)
            } else {
                Style::default().fg(theme::TEXT)
            };
            ListItem::new(Line::from(Span::styled(p.name.clone(), name_style)))
        })
        .collect();

    let highlight = if focused {
        theme::selected_row().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::TEXT_DIM)
    };
    let list = List::new(items)
        .style(theme::base())
        .highlight_style(highlight)
        .highlight_symbol("❯ ");
    let mut state = ListState::default();
    state.select(Some(selected.min(cfg.profiles.len().saturating_sub(1))));
    frame.render_stateful_widget(list, inner, &mut state);
}
