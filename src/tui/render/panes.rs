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

/// Paint a full-width selection bar across `line` — the shared selected-row
/// treatment for the account pickers and the Config / Fallback detail rows.
/// Bolds only the label item (`spans[1]`, after the leading cursor span); the
/// rest just rides the soft bar. `width` stretches the bar across.
pub(super) fn highlight_row(mut line: Line<'static>, width: usize) -> Line<'static> {
    let pad = width.saturating_sub(line.width());
    if let Some(label) = line.spans.get_mut(1) {
        label.style = label.style.add_modifier(Modifier::BOLD);
    }
    let mut line = line.style(theme::selected_row());
    if pad > 0 {
        line.push_span(Span::raw(" ".repeat(pad)));
    }
    line
}

/// Apply the selected-row treatment to a pre-built row: the bar with its first
/// label bolded when the pane holds focus, a dim wash when blurred, untouched
/// otherwise. The single entry point every list / picker funnels its selected
/// row through, so they all read identically.
pub(super) fn select_line(
    line: Line<'static>,
    selected: bool,
    focused: bool,
    width: u16,
) -> Line<'static> {
    if !selected {
        line
    } else if focused {
        highlight_row(line, width as usize)
    } else {
        line.patch_style(Style::default().fg(theme::TEXT_DIM))
    }
}

/// Account name color: Claude orange while it's the active profile, plain text
/// otherwise. Shared by every picker that lists account names.
pub(super) fn name_color(active: bool) -> Style {
    if active {
        Style::default().fg(theme::ACCENT_2)
    } else {
        Style::default().fg(theme::TEXT)
    }
}

/// One account-picker row: a leading cursor span then the styled name, routed
/// through `select_line`. Shared by every "accounts" picker so they match.
pub(super) fn picker_row(
    selected: bool,
    focused: bool,
    name: String,
    name_style: Style,
    width: u16,
) -> Line<'static> {
    let arrow = if selected {
        Span::styled("❯ ", theme::accent())
    } else {
        Span::raw("  ")
    };
    let line = Line::from(vec![arrow, Span::styled(name, name_style)]);
    select_line(line, selected, focused, width)
}

/// Master-detail left selector: the bordered box, an empty-state line, then a
/// scrolling list of `build_rows` (handed the inner width for `select_line`'s
/// bar). Shared by the accounts and chain pickers; `sel` is the cursor index.
pub(super) fn draw_selector_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    focused: bool,
    sel: usize,
    build_rows: impl FnOnce(u16) -> Vec<Line<'static>>,
) {
    let block = section_box(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = build_rows(inner.width);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("no accounts yet", theme::muted())))
                .style(theme::base()),
            inner,
        );
        return;
    }

    let list =
        List::new(rows.into_iter().map(ListItem::new).collect::<Vec<_>>()).style(theme::base());
    let mut state = ListState::default();
    state.select(Some(sel));
    frame.render_stateful_widget(list, inner, &mut state);
}

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
    let cfg = app.config();
    let sel = selected.min(cfg.profiles.len().saturating_sub(1));
    draw_selector_list(frame, area, "accounts", focused, sel, |w| {
        cfg.profiles
            .iter()
            .enumerate()
            .map(|(i, p)| {
                picker_row(
                    i == sel,
                    focused,
                    p.name.clone(),
                    name_color(cfg.is_active(&p.name)),
                    w,
                )
            })
            .collect()
    });
}
