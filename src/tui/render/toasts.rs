//! Transient toast stack — top-right, floating, no border.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::super::app::{App, Toast, ToastKind};
use super::super::theme;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.toasts.is_empty() {
        return;
    }
    let toasts: Vec<&Toast> = app.toasts.iter().collect();

    // Max content width: min(36, terminal_width − 2); the −2 accounts for the
    // 1-cell inset on each side.
    let content_cap = 36_u16.min(area.width.saturating_sub(2));

    // Each toast can be multi-line (newline-split body). Measure the widest
    // rendered line across all toasts to size the column consistently.
    let max_content_width = toasts
        .iter()
        .flat_map(|t| t.body.lines().map(|l| l.chars().count() as u16))
        .max()
        .unwrap_or(0)
        .min(content_cap);

    // +2: 1 for the bar glyph cell, 1 for the space after it.
    let col_width = max_content_width + 2;

    // Anchor: 1-cell inset from the right edge.
    let x = area.x + area.width.saturating_sub(col_width + 1);

    // Count total rows needed to place the stack top-down from row 1.
    let mut row = area.y + 1;

    for toast in &toasts {
        let color = match toast.kind {
            ToastKind::Info => theme::INFO,
            ToastKind::Success => theme::SUCCESS,
            ToastKind::Warning => theme::WARNING,
            ToastKind::Danger => theme::DANGER,
        };
        let bar_style = Style::default().fg(color).bg(theme::BG_SUNKEN);
        let title_style = Style::default()
            .fg(theme::TEXT)
            .bg(theme::BG_SUNKEN)
            .add_modifier(Modifier::BOLD);
        let detail_style = Style::default().fg(theme::TEXT_DIM).bg(theme::BG_SUNKEN);

        let mut lines_iter = toast.body.lines();
        let first = lines_iter.next().unwrap_or("");

        // Build all rendered lines for this toast.
        let mut render_lines: Vec<Line<'_>> = vec![Line::from(vec![
            Span::styled("┃ ", bar_style),
            Span::styled(first.to_owned(), title_style),
        ])];
        for detail in lines_iter {
            render_lines.push(Line::from(vec![
                Span::styled("┃ ", bar_style),
                Span::styled(detail.to_owned(), detail_style),
            ]));
        }

        let height = render_lines.len() as u16;
        let rect = Rect {
            x,
            y: row,
            width: col_width,
            height,
        };
        if fits_in(area, rect) {
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(render_lines).style(Style::default().bg(theme::BG_SUNKEN)),
                rect,
            );
        }
        row += height;
    }
}

fn fits_in(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}
