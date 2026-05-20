//! Fallback chain editor screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Padding, Paragraph};

use super::super::app::{App, ChainItemKind, chain_items};
use super::super::theme;
use crate::fallback::{DEFAULT_THRESHOLD, threshold_for};

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items = chain_items(app);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::LINE))
        .title(Line::from(Span::styled(" FALLBACK CHAIN ", theme::label())))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.config.state.fallback_chain.is_empty() {
        let hint = Paragraph::new(vec![
            Line::from(Span::styled("Chain is empty.", theme::muted())),
            Line::from(""),
            Line::from(Span::styled(
                "Add a profile to enable auto-switch when its 5h window crosses",
                theme::dim(),
            )),
            Line::from(Span::styled(
                "the threshold. clauth will rotate to the next chain member.",
                theme::dim(),
            )),
        ])
        .style(theme::base());
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(1)])
            .split(inner);
        frame.render_widget(hint, parts[0]);

        let action_items = items
            .iter()
            .map(|item| ListItem::new(render_chain_row(app, *item)))
            .collect::<Vec<_>>();
        let list = List::new(action_items)
            .style(theme::base())
            .highlight_style(theme::selected_row().add_modifier(Modifier::BOLD))
            .highlight_symbol("▸ ");
        let mut state = ratatui::widgets::ListState::default();
        state.select(Some(app.chain_cursor.min(items.len().saturating_sub(1))));
        frame.render_stateful_widget(list, parts[1], &mut state);
        return;
    }

    let lines: Vec<ListItem<'_>> = items
        .iter()
        .map(|item| ListItem::new(render_chain_row(app, *item)))
        .collect();
    let list = List::new(lines)
        .style(theme::base())
        .highlight_style(theme::selected_row().add_modifier(Modifier::BOLD))
        .highlight_symbol("▸ ");
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(app.chain_cursor.min(items.len().saturating_sub(1))));
    frame.render_stateful_widget(list, inner, &mut state);
}

fn render_chain_row<'a>(app: &'a App, item: ChainItemKind) -> Line<'a> {
    match item {
        ChainItemKind::Member(i) => {
            let Some(name) = app.config.state.fallback_chain.get(i) else {
                return Line::from("");
            };
            let profile = app.config.find(name);
            let threshold = profile.map(threshold_for).unwrap_or(DEFAULT_THRESHOLD);
            let utilization = profile
                .and_then(|p| p.usage.as_ref())
                .and_then(|u| u.five_hour.as_ref())
                .map(|w| w.utilization);

            let active = app.config.is_active(name);
            let active_mark = if active {
                Span::styled("● ", theme::accent())
            } else {
                Span::raw("  ")
            };
            let position = Span::styled(format!("{:>2}.  ", i + 1), theme::faint());
            let name_span = Span::styled(name.clone(), Style::default().fg(theme::TEXT).bold());
            let threshold_span = Span::styled(format!("  @ {threshold:.0}%"), theme::faint());
            let util_span = match utilization {
                Some(pct) => {
                    let color = if pct >= threshold {
                        theme::DANGER
                    } else if pct >= threshold * 0.8 {
                        theme::ACCENT_2
                    } else {
                        theme::TEXT_DIM
                    };
                    Span::styled(format!("  5h {pct:.0}%"), Style::default().fg(color))
                }
                None => Span::styled("  5h —", theme::faint()),
            };
            Line::from(vec![
                position,
                active_mark,
                name_span,
                threshold_span,
                util_span,
            ])
        }
        ChainItemKind::Add => Line::from(vec![
            Span::styled("    + ", theme::orange()),
            Span::styled("Add profile to chain", theme::muted()),
        ]),
        ChainItemKind::Back => Line::from(vec![
            Span::raw("    "),
            Span::styled("← Back", theme::faint()),
        ]),
    }
}
