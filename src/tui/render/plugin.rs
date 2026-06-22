//! Plugin tab — Claude Code integration health on the left, the selected row's
//! readout on the right. Master-detail, mirroring the Status tab's two-pane
//! machinery (counts as 2 of the 3-panel budget; no third panel).
//!
//! The left panel is one cursor-driven selector over two groups: global
//! integration checks (clauth on PATH, mcpServers wiring, plugin install, CC
//! version) then a `profiles` group, one row per profile. Each row is a status
//! dot + label + terse value. Enter descends into the detail pane; `f` applies
//! the selected row's fix (when one applies). All data is recomputed
//! synchronously on tab focus and `r` — there is no background thread, so the
//! title spinner only flickers while the cached `claude --version` is probed.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::super::app::{App, Health, PluginFocus};
use super::super::theme;
use super::format::spinner_frame;
use super::panes::{draw_scrollbar, empty_state, name_color, section_box, section_box_verbatim};
use crate::format::truncate;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // Selector width: 2/5 of the body, clamped 24–40 (same as Status, per spec).
    let sel_w = (area.width.saturating_mul(2) / 5).clamp(24, 40);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(sel_w), Constraint::Min(20)])
        .split(area);

    draw_selector(frame, cols[0], app);
    draw_detail(frame, cols[1], app);
}

// ── Left panel: checks + profiles selector ──────────────────────────────────────

fn draw_selector(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.plugin.focus == PluginFocus::List;
    let block = list_block(app, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.plugin.row_count() == 0 {
        let widget = if app.plugin.error.is_some() {
            empty_state("check failed", "r", "to retry")
        } else {
            empty_state("no checks yet", "r", "to run")
        };
        frame.render_widget(widget, inner);
        return;
    }

    let content_w = inner.width as usize;
    let mut rows: Vec<Line<'static>> = Vec::new();
    // Display-line index of the cursor (the separator row shifts profile rows).
    let mut cursor_line = 0usize;

    for (idx, check) in app.plugin.checks.iter().enumerate() {
        if idx == app.plugin.cursor {
            cursor_line = rows.len();
        }
        rows.push(selector_row(
            check.health,
            check.label,
            theme::body(),
            &check.value,
            idx == app.plugin.cursor,
            focused,
            content_w,
        ));
    }

    if !app.plugin.profiles.is_empty() {
        rows.push(group_eyebrow("profiles"));
        for (idx, profile) in app.plugin.profiles.iter().enumerate() {
            let cursor = app.plugin.checks.len() + idx;
            if cursor == app.plugin.cursor {
                cursor_line = rows.len();
            }
            rows.push(selector_row(
                profile.health,
                &profile.name,
                name_color(profile.active),
                &profile.summary,
                cursor == app.plugin.cursor,
                focused,
                content_w,
            ));
        }
    }

    let viewport = inner.height as usize;
    let start = window_start(cursor_line, viewport, rows.len());
    let shown = rows.len().saturating_sub(start).min(viewport.max(1));
    let window: Vec<Line<'static>> = rows.iter().skip(start).take(shown).cloned().collect();

    frame.render_widget(Paragraph::new(window).style(theme::base()), inner);
    draw_scrollbar(frame, inner, rows.len(), start, viewport);
}

/// Keep `focus` near the center of a `viewport`-tall window over `total` rows.
fn window_start(focus: usize, viewport: usize, total: usize) -> usize {
    if total <= viewport || viewport == 0 {
        return 0;
    }
    let half = viewport / 2;
    if focus < half {
        0
    } else {
        focus.saturating_sub(half).min(total - viewport)
    }
}

/// One selector row: `❯ ● label             value`. The hover tint spans the
/// full content width when selected (the ratatui filler-tint gotcha); the caret
/// shows only in the focused pane.
fn selector_row(
    health: Health,
    label: &str,
    label_style: Style,
    value: &str,
    selected: bool,
    focused: bool,
    content_w: usize,
) -> Line<'static> {
    let tint = selected.then(theme::bg_hover);
    let with_bg = |style: Style| match tint {
        Some(color) => style.bg(color),
        None => style,
    };

    let caret = if selected && focused {
        Span::styled(
            "❯ ",
            with_bg(
                Style::default()
                    .fg(theme::accent_color())
                    .add_modifier(Modifier::BOLD),
            ),
        )
    } else {
        Span::styled("  ", with_bg(Style::default()))
    };
    let dot = Span::styled("● ", with_bg(Style::default().fg(health_color(health))));
    let label_style = if selected && focused {
        with_bg(label_style.add_modifier(Modifier::BOLD))
    } else {
        with_bg(label_style)
    };

    // 2 (caret) + 2 (dot + space) + label, then a right-aligned value.
    let head_w = 4 + label.chars().count();
    let mut spans = vec![caret, dot, Span::styled(label.to_string(), label_style)];

    let value_room = content_w.saturating_sub(head_w + 1);
    if value_room > 0 && !value.is_empty() {
        let value = truncate(value, value_room);
        let used = head_w + value.chars().count();
        if used < content_w {
            spans.push(Span::styled(
                " ".repeat(content_w - used),
                with_bg(Style::default()),
            ));
        }
        spans.push(Span::styled(value, with_bg(theme::faint())));
    }
    pad_to(&mut spans, content_w, tint);
    Line::from(spans)
}

/// Group eyebrow: UPPERCASE label in `TEXT_DIM` (house eyebrow style, same as the
/// Status tab's `TIMELINE`), separating the integration checks from the profiles.
fn group_eyebrow(label: &str) -> Line<'static> {
    Line::from(Span::styled(label.to_uppercase(), theme::dim()))
}

/// Pad a span list with tinted filler so the hover tint spans the full width.
fn pad_to(spans: &mut Vec<Span<'static>>, content_w: usize, tint: Option<ratatui::style::Color>) {
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = content_w.saturating_sub(used);
    if pad > 0 {
        let style = match tint {
            Some(color) => Style::default().bg(color),
            None => Style::default(),
        };
        spans.push(Span::styled(" ".repeat(pad), style));
    }
}

/// The selector panel block. First panel on the screen → ACCENT_2 title; a
/// manual-refresh spinner sits in the trailing title inset (` PLUGIN ⠇ `).
fn list_block(app: &App, focused: bool) -> Block<'static> {
    let border_color = if focused {
        theme::line_strong_color()
    } else {
        theme::line_color()
    };
    let mut title_mods = Modifier::ITALIC;
    if focused {
        title_mods |= Modifier::BOLD;
    }
    let title_style = Style::default()
        .fg(theme::accent_2_color())
        .add_modifier(title_mods);

    let mut title_spans = vec![Span::styled(" PLUGIN ", title_style)];
    if app.plugin.fetching {
        title_spans.push(Span::styled(
            format!("{} ", spinner_frame(app.tick_count)),
            theme::accent(),
        ));
    }

    Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(title_spans))
        .padding(ratatui::widgets::Padding::horizontal(1))
}

// ── Right panel: selected-row detail ────────────────────────────────────────────

fn draw_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.plugin.focus == PluginFocus::Detail;

    // Title + body for whichever group the cursor sits in. Profile names keep
    // their case (`section_box_verbatim`); check labels go through `section_box`.
    let (block, detail): (Block<'static>, &[String]) =
        if let Some(check) = app.plugin.selected_check() {
            (
                section_box(check.label, focused, false),
                check.detail.as_slice(),
            )
        } else if let Some(profile) = app.plugin.selected_profile() {
            (
                section_box_verbatim(&profile.name, focused, false),
                profile.detail.as_slice(),
            )
        } else {
            (section_box("plugin", focused, false), [].as_slice())
        };

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if detail.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled("no row selected", theme::dim())))
            .style(theme::base());
        frame.render_widget(hint, inner);
        return;
    }

    let lines: Vec<Line<'static>> = detail.iter().map(|line| detail_line(line)).collect();
    let total = lines.len();
    let viewport = inner.height as usize;

    let max_scroll = total.saturating_sub(viewport).min(u16::MAX as usize) as u16;
    app.plugin.detail_max_scroll.set(max_scroll);
    let scroll = app.plugin.detail_scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((scroll, 0)),
        inner,
    );
    draw_scrollbar(frame, inner, total, scroll as usize, viewport);
}

/// Detail key column width — keys render left-aligned in this column, the value
/// follows in body text (house static-kv style: spacing + color, no `:` colon).
const DETAIL_KEY_W: usize = 13;

/// Style one detail line: `[f] …` in ACCENT, two-space-indented sub-lines faint,
/// `key: value` source rows as a dim padded key column + body value (colon
/// dropped), everything else body text.
fn detail_line(text: &str) -> Line<'static> {
    if text.is_empty() {
        return Line::from("");
    }
    if let Some(rest) = text.strip_prefix("[f]") {
        return Line::from(Span::styled(format!("[f]{rest}"), theme::accent()));
    }
    if text.starts_with("  ") {
        return Line::from(Span::styled(text.to_string(), theme::faint()));
    }
    if let Some((key, value)) = text.split_once(": ") {
        let pad = DETAIL_KEY_W.saturating_sub(key.chars().count()).max(2);
        return Line::from(vec![
            Span::styled(format!("{key}{}", " ".repeat(pad)), theme::label()),
            Span::styled(value.to_string(), theme::body()),
        ]);
    }
    Line::from(Span::styled(text.to_string(), theme::body()))
}

fn health_color(health: Health) -> ratatui::style::Color {
    match health {
        Health::Ok => theme::success_color(),
        Health::Warn => theme::warning_color(),
        Health::Danger => theme::danger_color(),
    }
}
