//! Top bar: claude glyph + title block (brand, screen eyebrow, status).

use std::sync::atomic::Ordering;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Screen};
use super::super::theme;
use crate::format::plan_label;
use crate::usage::now_ms;

const VERSION_SUFFIX: &str = concat!("  v", env!("CARGO_PKG_VERSION"));

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(10), Constraint::Min(20)])
        .split(area);

    draw_logo(frame, cols[0], app);
    draw_title(frame, cols[1], app);
}

fn draw_title(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let active = app.config.state.active_profile.as_deref();
    let active_span = match active {
        Some(name) => Span::styled(format!("active: {name}"), theme::accent()),
        None => Span::styled("no active profile", theme::warning()),
    };

    let title = Line::from(vec![
        Span::styled("clauth", Style::default().fg(theme::TEXT).bold()),
        Span::styled(VERSION_SUFFIX, theme::faint()),
    ]);
    let eyebrow = match app.screen {
        Screen::Overview => {
            let n = app.config.profiles.len();
            Line::from(vec![
                Span::styled("OVERVIEW", theme::label()),
                Span::raw("  "),
                active_span,
                Span::styled(format!("  ·  {n} account{}", plural(n)), theme::faint()),
            ])
        }
        Screen::Chain => {
            let n = app.config.state.fallback_chain.len();
            Line::from(vec![
                Span::styled("FALLBACK CHAIN", theme::label()),
                Span::raw("  "),
                Span::styled(format!("{n} profile{}", plural(n)), theme::muted()),
            ])
        }
        Screen::ProfileDetail { profile_index } => {
            let profile = app.config.profiles.get(profile_index);
            let name = profile.map(|p| p.name.as_str()).unwrap_or("—");
            let kind = profile
                .map(|p| {
                    if !p.is_oauth() {
                        "endpoint".to_string()
                    } else {
                        p.usage
                            .as_ref()
                            .and_then(|u| u.plan.as_ref())
                            .map(plan_label)
                            .unwrap_or_else(|| "oauth".to_string())
                    }
                })
                .unwrap_or_else(|| "—".to_string());
            let active = profile.is_some_and(|p| app.config.is_active(&p.name));
            Line::from(vec![
                Span::styled(name.to_string(), Style::default().fg(theme::TEXT).bold()),
                Span::styled("  ·  ", theme::faint()),
                Span::styled(kind, theme::faint()),
                Span::styled("  ·  ", theme::faint()),
                if active {
                    Span::styled("active", theme::accent())
                } else {
                    Span::styled("inactive", theme::faint())
                },
            ])
        }
    };

    let para = Paragraph::new(vec![title, eyebrow, status_line(app)]).style(theme::base());
    frame.render_widget(para, area);
}

/// Live refresh state — busy pip plus countdown to the next background
/// poll. Sits on the header's third row so the footer is free for hints.
fn status_line(app: &App) -> Line<'static> {
    let busy = app.activity.load(Ordering::Relaxed);
    let pip = if busy {
        Span::styled("●", theme::accent())
    } else {
        Span::styled("●", theme::faint())
    };
    let countdown = next_refresh_secs(app);
    Line::from(vec![
        pip,
        Span::styled(format!(" next refresh in {countdown}s"), theme::faint()),
    ])
}

fn next_refresh_secs(app: &App) -> i64 {
    let target = app.next_refresh_at.load(Ordering::Relaxed) as i64;
    let now = now_ms() as i64;
    ((target - now) / 1000).max(0)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Claude glyph in the top-left. Static orange — the status pip on the third
/// title row is the one busy indicator, so the logo stays a calm anchor.
/// Eyes blank for ~200ms every ~6s as a subtle sign of life.
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
