//! Transient toast stack — bottom-right above the footer.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::super::app::{App, Toast, ToastKind};
use super::super::theme;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.toasts.is_empty() {
        return;
    }
    let toasts: Vec<&Toast> = app.toasts.iter().collect();
    let count = toasts.len() as u16;
    let max_width = toasts
        .iter()
        .map(|t| t.body.chars().count() as u16 + 4)
        .max()
        .unwrap_or(20)
        .min(area.width.saturating_sub(4));

    let x = area.x + area.width.saturating_sub(max_width + 2);
    let y = area.y.saturating_add(area.height.saturating_sub(count + 4));

    for (i, toast) in toasts.iter().enumerate() {
        let rect = Rect {
            x,
            y: y + i as u16,
            width: max_width,
            height: 1,
        };
        if !fits_in(area, rect) {
            continue;
        }
        let color = match toast.kind {
            ToastKind::Info => theme::INFO,
            ToastKind::Success => theme::SUCCESS,
            ToastKind::Warning => theme::WARNING,
            ToastKind::Danger => theme::DANGER,
        };
        let para = Paragraph::new(Line::from(vec![
            Span::styled("▍ ", Style::default().fg(color)),
            Span::styled(
                toast.body.clone(),
                Style::default().fg(theme::TEXT).bg(theme::BG_RAISED),
            ),
        ]))
        .style(Style::default().bg(theme::BG_RAISED));
        frame.render_widget(Clear, rect);
        frame.render_widget(para, rect);
    }
}

fn fits_in(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}
