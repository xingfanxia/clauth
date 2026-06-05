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

    // Measure the widest *natural* line across all toasts, then clamp to the
    // cap.  This drives both the column width and the wrap budget.
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
            ToastKind::Info => theme::info_color(),
            ToastKind::Success => theme::success_color(),
            ToastKind::Warning => theme::warning_color(),
            ToastKind::Danger => theme::danger_color(),
        };
        let bar_style = Style::default().fg(color).bg(theme::bg_sunken());
        let title_style = Style::default()
            .fg(theme::text_color())
            .bg(theme::bg_sunken())
            .add_modifier(Modifier::BOLD);
        let detail_style = Style::default()
            .fg(theme::text_dim_color())
            .bg(theme::bg_sunken());

        let mut lines_iter = toast.body.lines();
        let first = lines_iter.next().unwrap_or("");

        // Build all rendered lines for this toast, wrapping any segment that
        // exceeds max_content_width so nothing is clipped by the rect boundary.
        let mut render_lines: Vec<Line<'_>> = Vec::new();
        for wrapped in word_wrap(first, max_content_width as usize) {
            render_lines.push(Line::from(vec![
                Span::styled("┃ ", bar_style),
                Span::styled(wrapped, title_style),
            ]));
        }
        for detail in lines_iter {
            for wrapped in word_wrap(detail, max_content_width as usize) {
                render_lines.push(Line::from(vec![
                    Span::styled("┃ ", bar_style),
                    Span::styled(wrapped, detail_style),
                ]));
            }
        }
        // Guard: always show at least one line even for an empty body.
        if render_lines.is_empty() {
            render_lines.push(Line::from(vec![Span::styled("┃ ", bar_style)]));
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
                Paragraph::new(render_lines).style(Style::default().bg(theme::bg_sunken())),
                rect,
            );
        }
        row += height;
    }
}

/// Soft-wrap `text` to at most `max_width` chars per visual line.
///
/// Splits on whitespace boundaries where possible; a single word longer than
/// `max_width` is emitted as its own line (hard-break fallback).  Returns at
/// least one element even for an empty input.
fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_owned()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len: usize = 0;

    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if current_len == 0 {
            // First word on a fresh line — hard-break if it alone exceeds the cap.
            if word_len > max_width {
                // Emit in max_width-char chunks.
                let mut chars = word.chars();
                let mut chunk = String::new();
                let mut chunk_len = 0;
                for ch in chars.by_ref() {
                    chunk.push(ch);
                    chunk_len += 1;
                    if chunk_len == max_width {
                        lines.push(chunk.clone());
                        chunk.clear();
                        chunk_len = 0;
                    }
                }
                if !chunk.is_empty() {
                    current = chunk;
                    current_len = chunk_len;
                }
            } else {
                current.push_str(word);
                current_len = word_len;
            }
        } else if current_len + 1 + word_len <= max_width {
            // Word fits on the current line with a space.
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else {
            // Flush and start a new line.
            lines.push(current.clone());
            current.clear();
            current_len = 0;
            // Re-process this word from scratch on the new line.
            if word_len > max_width {
                let mut chars = word.chars();
                let mut chunk = String::new();
                let mut chunk_len = 0;
                for ch in chars.by_ref() {
                    chunk.push(ch);
                    chunk_len += 1;
                    if chunk_len == max_width {
                        lines.push(chunk.clone());
                        chunk.clear();
                        chunk_len = 0;
                    }
                }
                if !chunk.is_empty() {
                    current = chunk;
                    current_len = chunk_len;
                }
            } else {
                current.push_str(word);
                current_len = word_len;
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn fits_in(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

#[cfg(test)]
mod tests {
    use super::word_wrap;

    #[test]
    fn short_line_unchanged() {
        assert_eq!(word_wrap("hello world", 36), vec!["hello world"]);
    }

    #[test]
    fn wraps_at_word_boundary() {
        // "terminal too small · enlarge for full layout" is 44 chars; cap at 36
        let out = word_wrap("terminal too small · enlarge for full layout", 36);
        assert_eq!(out.len(), 2, "expected 2 wrapped lines, got {out:?}");
        for l in &out {
            assert!(
                l.chars().count() <= 36,
                "line exceeds cap: {l:?} ({} chars)",
                l.chars().count()
            );
        }
    }

    #[test]
    fn empty_input_yields_one_empty_line() {
        assert_eq!(word_wrap("", 36), vec![""]);
    }

    #[test]
    fn single_word_exceeding_cap_hard_breaks() {
        let long_word = "a".repeat(80);
        let out = word_wrap(&long_word, 36);
        assert_eq!(out.len(), 3); // 36 + 36 + 8
        for l in &out {
            assert!(l.chars().count() <= 36);
        }
    }

    #[test]
    fn col_width_equals_content_width_plus_bar() {
        // Simulate the geometry: content_cap=36, message fits in 25 chars.
        // col_width must be 27 (25 + 2 for "┃ ").
        let msg = "· enlarge for full layout"; // 25 chars
        let content_cap: u16 = 36;
        let max_content_width = [msg]
            .iter()
            .map(|l| l.chars().count() as u16)
            .max()
            .unwrap()
            .min(content_cap);
        let col_width = max_content_width + 2;
        assert_eq!(max_content_width, 25);
        assert_eq!(col_width, 27);
    }
}
