use crate::lockorder::RankedMutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::usage::{ActivityStore, ProfileActivity, any_busy};

fn make_activity(entries: &[(&str, ProfileActivity)]) -> ActivityStore {
    let mut map = HashMap::new();
    for (name, activity) in entries {
        map.insert(name.to_string(), *activity);
    }
    Arc::new(RankedMutex::new(map))
}

fn bootstrap_busy(flag: &Arc<AtomicBool>, activity: &ActivityStore) -> bool {
    flag.load(Ordering::SeqCst) || any_busy(activity)
}

use super::{InputState, parse_threshold};

#[test]
fn delete_word_removes_run_left_of_caret() {
    let mut input = InputState::new("foo bar");
    input.delete_word();
    assert_eq!(input.value, "foo ");
    input.delete_word();
    assert_eq!(input.value, "");
}

#[test]
fn delete_word_respects_caret_position() {
    let mut input = InputState::new("foo bar");
    input.home();
    input.delete_word();
    assert_eq!(input.value, "foo bar");
}

#[test]
fn parse_threshold_accepts_in_range_only() {
    assert_eq!(parse_threshold("0"), Some(0.0));
    assert_eq!(parse_threshold("100"), Some(100.0));
    assert_eq!(parse_threshold("73.5"), Some(73.5));
    assert!(parse_threshold("150").is_none());
    assert!(parse_threshold("-1").is_none());
    assert!(parse_threshold("abc").is_none());
    assert!(parse_threshold("").is_none());
}

#[test]
fn bootstrap_active_true_reports_busy() {
    let flag = Arc::new(AtomicBool::new(true));
    let activity = make_activity(&[]);
    assert!(bootstrap_busy(&flag, &activity));
}

#[test]
fn bootstrap_active_false_empty_store_reports_idle() {
    let flag = Arc::new(AtomicBool::new(false));
    let activity = make_activity(&[]);
    assert!(!bootstrap_busy(&flag, &activity));
}

#[test]
fn bootstrap_active_true_with_refreshing_slot_reports_busy() {
    let flag = Arc::new(AtomicBool::new(true));
    let activity = make_activity(&[("alice", ProfileActivity::Refreshing)]);
    assert!(bootstrap_busy(&flag, &activity));
}

#[test]
fn bootstrap_active_false_with_refreshing_slot_still_busy() {
    let flag = Arc::new(AtomicBool::new(false));
    let activity = make_activity(&[("alice", ProfileActivity::Refreshing)]);
    assert!(bootstrap_busy(&flag, &activity));
}

// ── compact mode ─────────────────────────────────────────────────────────

use super::App;

fn bare_app() -> App {
    use crate::profile::{AppConfig, AppState};
    App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    })
}

#[test]
fn compact_entry_sets_flag_no_toast() {
    let mut app = bare_app();
    app.update_compact(13);
    assert!(app.compact);
    assert!(app.toasts.is_empty(), "compact must not fire a toast");
}

#[test]
fn compact_yields_warning_banner() {
    use super::{BannerSeverity, update_banner};
    let mut app = bare_app();
    app.update_compact(13);
    update_banner(&mut app);
    let banner = app.banner.as_ref().expect("compact banner present");
    assert_eq!(banner.severity, BannerSeverity::Warning);
    assert_eq!(
        banner.message,
        "terminal too small · enlarge for full layout"
    );
}

#[test]
fn compact_exit_clears_banner() {
    use super::update_banner;
    let mut app = bare_app();
    app.update_compact(13);
    update_banner(&mut app);
    assert!(app.banner.is_some());
    app.update_compact(14);
    update_banner(&mut app);
    assert!(!app.compact);
    assert!(app.banner.is_none(), "banner self-clears on resize");
}

#[test]
fn compact_rearm_after_exit() {
    use super::update_banner;
    let mut app = bare_app();
    app.update_compact(13);
    app.update_compact(14);
    app.update_compact(13);
    update_banner(&mut app);
    assert!(app.compact);
    assert!(app.toasts.is_empty(), "compact must not fire a toast");
    assert!(app.banner.is_some());
}

// ── global config tab ────────────────────────────────────────────────────

use super::theme::{self, Tier};
use super::{GLOBAL_CONFIG_ROWS, GlobalConfigRow, KeyCode, KeyEvent, KeyModifiers, Tab};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn theme_set_tier_round_trips() {
    theme::set_tier(Tier::Full);
    assert_eq!(theme::tier(), Tier::Full);
    theme::set_tier(Tier::Compatible);
    assert_eq!(theme::tier(), Tier::Compatible);
    theme::set_tier(Tier::Full);
    assert_eq!(theme::tier(), Tier::Full);
}

#[test]
fn global_config_cursor_wraps() {
    let mut app = bare_app();
    app.tab = Tab::Config;
    let last = GLOBAL_CONFIG_ROWS.len() - 1;

    assert_eq!(app.global_config_cursor, 0);
    super::handle_global_config_key(&mut app, key(KeyCode::Up));
    assert_eq!(
        app.global_config_cursor, last,
        "Up from first wraps to last"
    );
    super::handle_global_config_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.global_config_cursor, 0, "Down from last wraps to first");
}

// ── divergence default ─────────────────────────────────────────────────────

use crate::profile::DivergenceChoice;

#[test]
fn next_divergence_default_cycles_round_trip() {
    assert_eq!(
        super::next_divergence_default(None),
        Some(DivergenceChoice::Overwrite)
    );
    assert_eq!(
        super::next_divergence_default(Some(DivergenceChoice::Overwrite)),
        Some(DivergenceChoice::NewProfile)
    );
    assert_eq!(
        super::next_divergence_default(Some(DivergenceChoice::NewProfile)),
        Some(DivergenceChoice::Discard)
    );
    assert_eq!(
        super::next_divergence_default(Some(DivergenceChoice::Discard)),
        None
    );
}

#[test]
fn divergence_default_row_is_reachable_by_cursor() {
    let mut app = bare_app();
    app.tab = Tab::Config;
    let pos = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::DivergenceDefault)
        .unwrap();

    app.global_config_cursor = pos;
    let from_up = if pos == 0 {
        GLOBAL_CONFIG_ROWS.len() - 1
    } else {
        pos - 1
    };
    super::handle_global_config_key(&mut app, key(KeyCode::Up));
    assert_eq!(app.global_config_cursor, from_up);
    super::handle_global_config_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.global_config_cursor, pos);
}
