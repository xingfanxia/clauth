//! Top bar: claude glyph on the left; brand, account count, and the tab bar
//! stacked in the text column to the right. Three rows, no dead space.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::App;
use super::super::theme;
use super::tabs;

const VERSION_SUFFIX: &str = concat!("  v", env!("CARGO_PKG_VERSION"));

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(10), Constraint::Min(20)])
        .split(area);

    draw_logo(frame, cols[0], app);

    // Right column: brand, account count, tab bar — one per row, aligned.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(cols[1]);

    let n = app.config().profiles.len();
    let title = Line::from(vec![
        Span::styled("clauth", Style::default().fg(theme::TEXT).bold()),
        Span::styled(VERSION_SUFFIX, theme::faint()),
    ]);
    let count = Line::from(Span::styled(
        format!("{n} account{}", plural(n)),
        theme::faint(),
    ));
    frame.render_widget(Paragraph::new(title).style(theme::base()), rows[0]);
    frame.render_widget(Paragraph::new(count).style(theme::base()), rows[1]);
    tabs::draw(frame, rows[2], app);
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Claude glyph in the top-left. Eyes blank for ~200ms every ~6s as a subtle sign of life.
fn draw_logo(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let elapsed = app.started_at.elapsed().as_millis() as u64;
    let blink = (elapsed % 6000) < 200;

    let style = Style::default().fg(theme::ACCENT_2);

    let logo_top = if blink {
        " ▐█████▌ "
    } else {
        " ▐▛███▜▌ "
    };
    let logo_mid = "▝▜█████▛▘";
    let logo_eyes = "  ▘▘ ▝▝  ";

    let lines = vec![
        Line::from(Span::styled(logo_top, style)).alignment(Alignment::Left),
        Line::from(Span::styled(logo_mid, style)).alignment(Alignment::Left),
        Line::from(Span::styled(logo_eyes, style)).alignment(Alignment::Left),
    ];

    let para = Paragraph::new(lines).style(theme::base());
    frame.render_widget(para, area);
}
