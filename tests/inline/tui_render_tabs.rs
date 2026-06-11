use super::*;
use crate::profile::{AppConfig, AppState};
use crate::tui::app::App;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn truncate_label(label: String, max_chars: usize) -> String {
    let char_count = label.chars().count();
    if char_count <= max_chars {
        return label;
    }
    if max_chars == 0 {
        return String::new();
    }
    if let Some(suffix_start) = label.rfind('(') {
        let suffix = &label[suffix_start..];
        let body = &label[..suffix_start];
        let suffix_chars = suffix.chars().count();
        let body_budget = max_chars.saturating_sub(suffix_chars + 1);
        if body_budget > 0 {
            let trimmed_body: String = body.chars().take(body_budget).collect();
            return format!("{trimmed_body}…{suffix}");
        }
    }
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
    let app = empty_app(Tab::Overview);
    let s = render_tabs(&app, 70);
    assert!(s.contains("overview"), "active tab missing");
    assert!(s.contains("usage"), "inactive tab missing");
    assert!(s.contains("setup"), "inactive tab missing");
    assert!(s.contains("fallback"), "inactive tab missing");
    assert!(s.contains("config"), "inactive tab missing");
    assert!(s.contains("status"), "inactive tab missing");
}

#[test]
fn normal_form_labels_untruncated_at_tight_boundary() {
    let app = empty_app(Tab::Overview);
    let s = render_tabs(&app, 53);
    assert!(
        s.contains("overview"),
        "overview must not be truncated at tight boundary"
    );
    assert!(
        s.contains("status"),
        "status must not be truncated at tight boundary"
    );
}

#[test]
fn overflow_form_shows_only_active() {
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
    let app = empty_app(Tab::Usage);
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
    let app = empty_app(Tab::Overview);
    let s = render_tabs(&app, 12);
    assert!(!s.contains('‹'), "no left chevron at first tab");
    assert!(
        s.contains('›'),
        "right chevron must appear when more tabs follow"
    );
}

#[test]
fn overflow_no_right_chevron_at_last_tab() {
    let app = empty_app(Tab::Status);
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
    assert_eq!(truncate_label("my-long-name(45%)".into(), 10), "my-l…(45%)");
    assert_eq!(truncate_label("done(✓)".into(), 6), "do…(✓)");
}
