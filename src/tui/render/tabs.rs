//! Tab bar — the primary navigation surface. Lowercase labels; the active tab
//! is underlined in sapphire, the rest sit dim. Switching is ← →.
//! An inactive tab with a pending background event takes that event's semantic
//! color until the user visits it (clears in `switch_tab`).
//!
//! # Overflow
//!
//! When `area.width` is too narrow to fit all tab labels (plus 3-space separators),
//! the bar degrades to the overflow form:
//!
//! ```text
//!  ‹   active   ›
//! ```
//!
//! `‹` / `›` are rendered in `TEXT_FAINT` (2 spaces padding each side) and
//! indicate more tabs exist to the left / right. A marker is omitted when the
//! active tab is already at that edge. The active label is always underlined in
//! place (`ACCENT + BOLD + UNDERLINED`).
//!
//! # Dynamic label truncation
//!
//! `truncate_label` is available for dynamic tabs that carry a progress suffix
//! (`(N%)` / `(✓)`) — the suffix is preserved and only the name body is trimmed.
//! The current static tabs always fit so this path is defensive / future-proof.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Tab, ToastKind};
use super::super::theme;

/// Separator between adjacent tabs in normal form.
const SEP: &str = "   ";
const SEP_WIDTH: usize = 3;

/// Chevron affordances (2 spaces padding each side) used in overflow form,
/// matching the visual contract: `‹   active   ›`.
const PREV_MARK: &str = "‹  ";
const NEXT_MARK: &str = "  ›";

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let avail = area.width as usize;
    let spans = if fits_normal(avail) {
        build_normal(app, avail)
    } else {
        build_overflow(app)
    };

    let para = Paragraph::new(Line::from(spans)).style(theme::base());
    frame.render_widget(para, area);
}

/// Total width of the tab strip in normal form (all labels + separators),
/// before any truncation. Uses raw label widths since all current labels are ASCII.
fn full_strip_width() -> usize {
    Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let sep = if i > 0 { SEP_WIDTH } else { 0 };
            sep + t.title().len()
        })
        .sum()
}

/// Whether all tabs fit in `avail` columns without truncation.
fn fits_normal(avail: usize) -> bool {
    full_strip_width() <= avail
}

/// Build spans for the normal (all-tabs-visible) form.
/// Called only when `fits_normal` has already confirmed all labels fit — no truncation needed.
fn build_normal(app: &App, _avail: usize) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(SEP, theme::dim()));
        }
        let label = tab.title().to_lowercase();
        if *tab == app.tab {
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(theme::accent_color())
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
        } else {
            let color = activity_color(app.tab_activity[tab.index()]);
            spans.push(Span::styled(label, Style::default().fg(color)));
        }
    }
    spans
}

/// Build spans for the overflow form: `‹  active  ›` (chevrons in TEXT_FAINT).
/// Edge markers are omitted when there is no tab in that direction.
fn build_overflow(app: &App) -> Vec<Span<'static>> {
    let active_idx = app.tab.index();
    let last_idx = Tab::ALL.len().saturating_sub(1);

    let mut spans: Vec<Span<'static>> = Vec::new();

    // Left chevron — omit when already at the first tab.
    if active_idx > 0 {
        spans.push(Span::styled(PREV_MARK, theme::faint()));
    }

    let label = app.tab.title().to_lowercase();
    spans.push(Span::styled(
        label,
        Style::default()
            .fg(theme::accent_color())
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    ));

    // Right chevron — omit when already at the last tab.
    if active_idx < last_idx {
        spans.push(Span::styled(NEXT_MARK, theme::faint()));
    }

    spans
}

/// Map a background activity kind to its semantic label color. `None` → TEXT_DIM
/// (the standard inactive tab color).
fn activity_color(activity: Option<ToastKind>) -> Color {
    match activity {
        None => theme::text_dim_color(),
        Some(ToastKind::Success) => theme::success_color(),
        Some(ToastKind::Danger) => theme::danger_color(),
        Some(ToastKind::Warning) => theme::warning_color(),
        Some(ToastKind::Info) => theme::text_color(),
    }
}

#[cfg(test)]
mod tab_overflow_tests {
    use super::*;
    use crate::profile::{AppConfig, AppState};
    use crate::tui::app::App;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Truncate a label to at most `max_chars` Unicode scalar values, appending `…`
    /// if truncated. Preserves a trailing `(…)` suffix (e.g. `(45%)`, `(✓)`) — only
    /// the name body is trimmed. Kept here for future dynamic-tab support.
    fn truncate_label(label: String, max_chars: usize) -> String {
        let char_count = label.chars().count();
        if char_count <= max_chars {
            return label;
        }
        if max_chars == 0 {
            return String::new();
        }
        // Preserve trailing `(…)` suffix.
        if let Some(suffix_start) = label.rfind('(') {
            let suffix = &label[suffix_start..];
            let body = &label[..suffix_start];
            let suffix_chars = suffix.chars().count();
            let body_budget = max_chars.saturating_sub(suffix_chars + 1); // +1 for `…`
            if body_budget > 0 {
                let trimmed_body: String = body.chars().take(body_budget).collect();
                return format!("{trimmed_body}…{suffix}");
            }
        }
        // Plain truncation.
        let trimmed: String = label.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{trimmed}…")
    }

    fn empty_app(active: Tab) -> App {
        let mut app = App::new(AppConfig {
            state: AppState::default(),
            profiles: Vec::new(),
        });
        app.tab = active;
        app
    }

    fn render_tabs(app: &App, width: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(width, 1)).unwrap();
        term.draw(|f| {
            let area = f.area();
            super::draw(f, area, app);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        (0..width)
            .map(|x| buf.content[x as usize].symbol().to_owned())
            .collect()
    }

    #[test]
    fn normal_form_shows_all_tabs() {
        // full strip: "overview   usage   setup   fallback   config"
        // = 8+3+5+3+5+3+8+3+6 = 44
        let app = empty_app(Tab::Overview);
        let s = render_tabs(&app, 60);
        assert!(s.contains("overview"), "active tab missing");
        assert!(s.contains("usage"), "inactive tab missing");
        assert!(s.contains("setup"), "inactive tab missing");
        assert!(s.contains("fallback"), "inactive tab missing");
        assert!(s.contains("config"), "inactive tab missing");
    }

    #[test]
    fn normal_form_labels_untruncated_at_tight_boundary() {
        // width=44 is the minimum that passes fits_normal; labels must be full, not "overv…"
        let app = empty_app(Tab::Overview);
        let s = render_tabs(&app, 44);
        assert!(
            s.contains("overview"),
            "overview must not be truncated at tight boundary"
        );
        assert!(
            s.contains("config"),
            "config must not be truncated at tight boundary"
        );
    }

    #[test]
    fn overflow_form_shows_only_active() {
        // width=10 — too narrow for all tabs (44 cols needed)
        let app = empty_app(Tab::Usage);
        let s = render_tabs(&app, 10);
        assert!(
            s.contains("usage"),
            "active label must appear in overflow form"
        );
        assert!(
            !s.contains("overview"),
            "non-active tab must not appear in overflow"
        );
        assert!(
            !s.contains("config"),
            "non-active tab must not appear in overflow"
        );
    }

    #[test]
    fn overflow_chevrons_at_middle_tab() {
        // Middle tab — both ‹ and › present.
        let app = empty_app(Tab::Usage); // index 1, last=4
        let s = render_tabs(&app, 15);
        assert!(
            s.contains('‹'),
            "left chevron must appear for non-first tab"
        );
        assert!(
            s.contains('›'),
            "right chevron must appear for non-last tab"
        );
    }

    #[test]
    fn overflow_no_left_chevron_at_first_tab() {
        let app = empty_app(Tab::Overview); // index 0
        let s = render_tabs(&app, 12);
        assert!(!s.contains('‹'), "no left chevron at first tab");
        assert!(
            s.contains('›'),
            "right chevron must appear when more tabs follow"
        );
    }

    #[test]
    fn overflow_no_right_chevron_at_last_tab() {
        let app = empty_app(Tab::Config); // index 4, last=4
        let s = render_tabs(&app, 12);
        assert!(
            s.contains('‹'),
            "left chevron must appear for non-first tab"
        );
        assert!(!s.contains('›'), "no right chevron at last tab");
    }

    #[test]
    fn truncate_label_plain() {
        assert_eq!(truncate_label("overview".into(), 5), "over…");
        assert_eq!(truncate_label("overview".into(), 8), "overview");
        assert_eq!(truncate_label("overview".into(), 0), "");
    }

    #[test]
    fn truncate_label_preserves_suffix() {
        // max_chars=10: suffix "(45%)" = 5, "…" = 1 → body_budget = 4 → "my-l"
        assert_eq!(truncate_label("my-long-name(45%)".into(), 10), "my-l…(45%)");
        // max_chars=6: suffix "(✓)" = 3, "…" = 1 → body_budget = 2 → "do"
        assert_eq!(truncate_label("done(✓)".into(), 6), "do…(✓)");
    }
}
