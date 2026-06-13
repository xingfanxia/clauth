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

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(10), Constraint::Min(20)])
        .split(area);

    draw_logo(frame, cols[0], app);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(cols[1]);

    let n = app.config().profiles.len();

    // Row 0: brand (TEXT + bold; house deviation from ACCENT_2) left, version (TEXT_DIM) right.
    let info_width = rows[0].width as usize;
    let brand = "clauth";
    let ver = format!("v{VERSION}");
    let gap = info_width.saturating_sub(brand.len() + ver.len());
    let title = Line::from(vec![
        Span::styled(brand, Style::default().fg(theme::text_color()).bold()),
        Span::styled(" ".repeat(gap), theme::base()),
        Span::styled(ver, theme::dim()),
    ]);

    // Row 1: count left; `● status.claude.ai` right-aligned. Dot color mirrors the
    // status tab (incidents → WARNING/DANGER, none → SUCCESS). Dropped if < 3-cell gap.
    let count_txt = format!("{n} account{}", plural(n));
    let mut count_spans = vec![Span::styled(count_txt.clone(), theme::faint())];
    let feed = "status.claude.ai"; // display label per user choice; feed itself is status.claude.com
    let ind_width = 2 + feed.len(); // `● ` + label
    if info_width >= count_txt.len() + ind_width + 3 {
        let gap = info_width - count_txt.len() - ind_width;
        count_spans.push(Span::styled(" ".repeat(gap), theme::base()));
        count_spans.push(Span::styled(
            "●",
            Style::default().fg(status_dot_color(app)),
        ));
        count_spans.push(Span::styled(format!(" {feed}"), theme::dim()));
    }

    frame.render_widget(Paragraph::new(title).style(theme::base()), rows[0]);
    frame.render_widget(
        Paragraph::new(Line::from(count_spans)).style(theme::base()),
        rows[1],
    );
    tabs::draw(frame, rows[2], app);
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Feed-health color for the header status dot — when active incidents exist,
/// uses the worst impact's semantic color (critical/major → DANGER, minor →
/// WARNING, maintenance → TEXT_DIM). Stale cache or no data → SUCCESS.
fn status_dot_color(app: &App) -> ratatui::style::Color {
    use crate::status::Impact;
    match app.status.worst_active_impact() {
        Impact::Critical | Impact::Major => theme::danger_color(),
        Impact::Minor => theme::warning_color(),
        Impact::Maintenance => theme::text_dim_color(),
        Impact::None | Impact::Other(_) => theme::success_color(),
    }
}

/// Claude glyph in the top-left. Eyes blank for ~200ms every ~6s as a subtle sign of life.
fn draw_logo(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let elapsed = app.started_at.elapsed().as_millis() as u64;
    let blink = (elapsed % 6000) < 200;

    let style = Style::default().fg(theme::accent_2_color());

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
