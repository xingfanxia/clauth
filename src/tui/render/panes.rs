//! Shared widgets: the bordered section box every pane uses, and the account
//! picker shared by the Usage and Setup tabs.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap};

use super::super::app::{App, InputState};
use super::super::theme;

/// Account-picker column width for a master-detail tab: ~30% of the body,
/// clamped 20-40 cells (cloudy-tui master-detail contract).
pub(super) fn selector_width(body_w: u16) -> u16 {
    (body_w.saturating_mul(3) / 10).clamp(20, 40)
}

/// Display columns occupied by the text before the caret in `input`.
/// `InputState::cursor` is a byte offset; every edited field is ASCII-only in
/// practice, so the char count of the pre-caret slice equals display columns.
pub(super) fn head_cols(input: &InputState) -> usize {
    input.value[..input.cursor.min(input.value.len())]
        .chars()
        .count()
}

/// Bolds `style` when `cond` is true.
pub(super) fn bold_when(style: Style, cond: bool) -> Style {
    if cond {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// Full-width selection bar: bg tint and stretch. Callers handle per-row bold.
pub(super) fn highlight_row(line: Line<'static>, width: usize) -> Line<'static> {
    let pad = width.saturating_sub(line.width());
    let mut line = line.style(theme::selected_row());
    if pad > 0 {
        line.push_span(Span::raw(" ".repeat(pad)));
    }
    line
}

/// Selected-row treatment: bold+bar+caret when focused, hover-tint-only when blurred.
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
        // Keep BG_HOVER tint so the user sees where they were; drop caret + bold.
        // Filler must carry the tint too — bare Span::raw paints Color::Reset holes.
        let pad = (width as usize).saturating_sub(line.width());
        let mut line = line.style(theme::selected_row());
        if pad > 0 {
            line.push_span(Span::styled(" ".repeat(pad), theme::selected_row()));
        }
        line
    }
}

/// Orange for the active profile, plain text otherwise.
pub(super) fn name_color(active: bool) -> Style {
    if active {
        Style::default().fg(theme::accent_2_color())
    } else {
        Style::default().fg(theme::text_color())
    }
}

/// `[ active ]` status pill: the active-account marker shared by usage, setup,
/// and fallback detail panes (cloudy-tui status pill — brackets dim, label
/// accent + bold).
pub(super) fn active_pill() -> Vec<Span<'static>> {
    vec![
        Span::styled("[ ", theme::dim()),
        Span::styled("active", theme::accent().add_modifier(Modifier::BOLD)),
        Span::styled(" ]", theme::dim()),
    ]
}

pub(super) fn picker_row(
    selected: bool,
    focused: bool,
    name: String,
    name_style: Style,
    width: u16,
) -> Line<'static> {
    // Caret only in the focused pane; blurred rows keep BG_HOVER via select_line.
    let arrow = if selected && focused {
        Span::styled("❯ ", theme::accent().add_modifier(Modifier::BOLD))
    } else {
        Span::raw("  ")
    };
    let line = Line::from(vec![
        arrow,
        Span::styled(name, bold_when(name_style, selected && focused)),
    ]);
    select_line(line, selected, focused, width)
}

/// Empty-state widget: rounded frame in `LINE`, hint on first line `TEXT_DIM`,
/// hotkey `ACCENT` + action on second line.
pub(super) fn empty_state(hint: &str, hotkey: &str, action: &str) -> Paragraph<'static> {
    Paragraph::new(vec![
        Line::from(Span::styled(hint.to_string(), theme::dim())),
        Line::from(vec![
            Span::styled(hotkey.to_string(), theme::accent()),
            Span::styled(format!(" {action}"), theme::dim()),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(Style::default().fg(theme::line_color())),
    )
    .style(theme::base())
    .wrap(Wrap { trim: false })
}

/// Renders a scrollbar into the 1-cell right-padding column of a panel.
///
/// Track: `┊` in `LINE`. Thumb: `┃` in `TEXT_DIM`.
/// Only renders when `total > viewport` (content overflows). The column sits
/// flush against the content area's right edge — it reuses the padding cell
/// `section_box` already reserves, so content width is unchanged.
pub(super) fn draw_scrollbar(
    frame: &mut Frame<'_>,
    inner: Rect,
    total: usize,
    offset: usize,
    viewport: usize,
) {
    if total <= viewport || viewport == 0 || inner.height == 0 {
        return;
    }
    // Right-padding column: one cell to the right of the content rect.
    let col_x = inner.x + inner.width;
    let col_y = inner.y;
    let col_h = inner.height as usize;

    let thumb_len = ((col_h * viewport) / total).max(1).min(col_h);
    let max_offset = total.saturating_sub(viewport);
    let thumb_top = ((col_h - thumb_len) * offset)
        .checked_div(max_offset)
        .unwrap_or(0);
    let thumb_end = thumb_top + thumb_len;

    let buf = frame.buffer_mut();
    for row in 0..col_h {
        let cell = buf.cell_mut((col_x, col_y + row as u16));
        if let Some(cell) = cell {
            if row >= thumb_top && row < thumb_end {
                cell.set_symbol("┃");
                cell.set_style(Style::default().fg(theme::text_dim_color()));
            } else {
                cell.set_symbol("┊");
                cell.set_style(Style::default().fg(theme::line_color()));
            }
        }
    }
}

/// Bordered selector list; `build_rows` receives the inner width for the selection bar.
pub(super) fn draw_selector_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    focused: bool,
    sel: usize,
    build_rows: impl FnOnce(u16) -> Vec<Line<'static>>,
) {
    let block = section_box(title, focused, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = build_rows(inner.width);
    if rows.is_empty() {
        frame.render_widget(empty_state("no accounts yet", "n", "to create one"), inner);
        return;
    }

    let total = rows.len();
    let list =
        List::new(rows.into_iter().map(ListItem::new).collect::<Vec<_>>()).style(theme::base());
    let mut state = ListState::default();
    state.select(Some(sel));
    frame.render_stateful_widget(list, inner, &mut state);

    let viewport = inner.height as usize;
    draw_scrollbar(frame, inner, total, state.offset(), viewport);
}

/// Form-row label style: `TEXT + bold` when focused, `TEXT_DIM` when blurred.
pub(super) fn label_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(theme::text_color())
            .add_modifier(Modifier::BOLD)
    } else {
        theme::dim()
    }
}

/// Rounded box with contract-compliant chrome.
///
/// Border: `LINE_STRONG` when focused, `LINE` when blurred.
/// Title: always italic, always UPPERCASE; bold added only when focused.
/// Color: `ACCENT_2` for the first bordered panel on the screen body, `TEXT_DIM` for the rest.
pub(super) fn section_box(title: &str, focused: bool, first: bool) -> Block<'static> {
    section_box_impl(title, focused, first, true)
}

/// Like [`section_box`] but preserves the title's original case — use only when
/// the title is a profile/account name, not a structural label.
pub(super) fn section_box_verbatim(title: &str, focused: bool, first: bool) -> Block<'static> {
    section_box_impl(title, focused, first, false)
}

fn section_box_impl(title: &str, focused: bool, first: bool, uppercase: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(theme::line_strong_color())
    } else {
        Style::default().fg(theme::line_color())
    };
    let title_color = if first {
        theme::accent_2_color()
    } else {
        theme::text_dim_color()
    };
    let title_style = {
        let base = Style::default()
            .fg(title_color)
            .add_modifier(Modifier::ITALIC);
        if focused {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        }
    };
    let label = if uppercase {
        format!(" {} ", title.to_uppercase())
    } else {
        format!(" {} ", title)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(border_style)
        .title(Line::from(Span::styled(label, title_style)))
        .padding(Padding::horizontal(1))
}

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
                    p.name.to_string(),
                    name_color(cfg.is_active(&p.name)),
                    w,
                )
            })
            .collect()
    });
}
