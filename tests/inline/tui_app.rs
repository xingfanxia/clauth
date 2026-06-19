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
use super::{GLOBAL_CONFIG_ROWS, GlobalConfigRow, KeyCode, Tab};

use crate::testutil::key;

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

// ── refresh interval custom value ──────────────────────────────────────────

use super::parse_refresh_secs;

/// Park the Config cursor on the refresh-interval row.
fn on_refresh_row(app: &mut App) {
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::RefreshInterval)
        .unwrap();
}

#[test]
fn parse_refresh_secs_accepts_in_range_only() {
    // Whole seconds, scaled to ms, must land in 10s..=3600s.
    assert_eq!(parse_refresh_secs("10"), Some(10_000));
    assert_eq!(parse_refresh_secs("90"), Some(90_000));
    assert_eq!(parse_refresh_secs("3600"), Some(3_600_000));
    assert!(parse_refresh_secs("9").is_none(), "below the 10s floor");
    assert!(parse_refresh_secs("3601").is_none(), "above the 1h cap");
    assert!(parse_refresh_secs("-5").is_none());
    assert!(parse_refresh_secs("1.5").is_none());
    assert!(parse_refresh_secs("abc").is_none());
    assert!(parse_refresh_secs("").is_none());
}

#[test]
fn refresh_interval_enter_opens_editor_seeded_in_seconds() {
    let mut app = bare_app();
    on_refresh_row(&mut app);

    assert!(app.refresh_interval_draft.is_none());
    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    let draft = app
        .refresh_interval_draft
        .as_ref()
        .expect("⏎ opens the custom-value editor");
    assert_eq!(draft.value, "90", "seeded with the default 90s in seconds");
}

#[test]
fn refresh_interval_space_cycles_without_opening_editor() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_refresh_row(&mut app);
    let before = app.refresh_interval.load(Ordering::Relaxed);

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        app.refresh_interval_draft.is_none(),
        "space cycles presets, never opens the editor"
    );
    assert_ne!(
        app.refresh_interval.load(Ordering::Relaxed),
        before,
        "space steps to the next preset"
    );
}

#[test]
fn refresh_interval_custom_value_commits_and_clears() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_refresh_row(&mut app);

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    // Clear the seeded "90", type "45".
    super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Backspace));
    super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Backspace));
    for c in "45".chars() {
        super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Enter));

    assert!(
        app.refresh_interval_draft.is_none(),
        "a valid commit clears the draft"
    );
    assert_eq!(app.refresh_interval.load(Ordering::Relaxed), 45_000);
}

#[test]
fn refresh_interval_out_of_range_keeps_editor_open() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_refresh_row(&mut app);
    let before = app.refresh_interval.load(Ordering::Relaxed);

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    for c in "99999".chars() {
        super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Enter));

    assert!(
        app.refresh_interval_draft.is_some(),
        "an out-of-range value keeps the editor open for correction"
    );
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        before,
        "interval stays put while the typed value is invalid"
    );
}

#[test]
fn refresh_interval_esc_discards_editor() {
    let mut app = bare_app();
    on_refresh_row(&mut app);
    let before = app.refresh_interval.load(Ordering::Relaxed);

    super::handle_global_config_key(&mut app, key(KeyCode::Enter));
    for c in "30".chars() {
        super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Char(c)));
    }
    super::handle_refresh_interval_edit_key(&mut app, key(KeyCode::Esc));

    assert!(
        app.refresh_interval_draft.is_none(),
        "esc discards the editor"
    );
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        before,
        "esc leaves the interval unchanged"
    );
}

// ── Setup tab: per-account custom env editor ───────────────────────────────

mod env_editor {
    use super::super::{App, ConfigFocus, ConfigRow, InputState, Modal, Tab, config_rows};
    use crate::profile::{AppConfig, AppState, Profile};
    use crate::testutil::HomeSandbox;
    use std::collections::BTreeMap;

    fn app_with_env(env: BTreeMap<String, String>) -> App {
        let mut profile = Profile::new("acct".to_string(), None, None);
        profile.env = env;
        App::new(AppConfig {
            state: AppState::default(),
            profiles: vec![profile],
        })
    }

    fn enter_detail(app: &mut App) {
        app.tab = Tab::Setup;
        app.profile_cursor = 0;
        super::super::enter_config_detail(app);
        assert_eq!(app.config_focus, ConfigFocus::Actions);
    }

    #[test]
    fn config_rows_insert_env_entries_then_add_row() {
        let mut env = BTreeMap::new();
        env.insert("ALPHA".to_string(), "1".to_string());
        env.insert("ZED".to_string(), "2".to_string());
        let app = app_with_env(env);

        let rows = config_rows(&app);
        let pos = |row: ConfigRow| rows.iter().position(|r| *r == row);
        let e0 = pos(ConfigRow::EnvEntry(0)).expect("first env row");
        let e1 = pos(ConfigRow::EnvEntry(1)).expect("second env row");
        let add = pos(ConfigRow::EnvAdd).expect("add-env row");
        assert!(e0 < e1 && e1 < add, "sorted entries precede the add row");
        assert_eq!(
            *rows.last().unwrap(),
            ConfigRow::Delete,
            "delete stays last"
        );
    }

    fn app_with_profile(profile: Profile) -> App {
        App::new(AppConfig {
            state: AppState::default(),
            profiles: vec![profile],
        })
    }

    #[test]
    fn oauth_account_hides_api_key_keeps_auto_start() {
        let app = app_with_env(BTreeMap::new()); // no base url → OAuth
        let rows = config_rows(&app);
        assert!(
            !rows.contains(&ConfigRow::ApiKey),
            "api key is meaningless without a base url"
        );
        assert!(
            rows.contains(&ConfigRow::AutoStart),
            "auto-start is the OAuth-only row"
        );
    }

    #[test]
    fn api_account_shows_api_key_drops_auto_start() {
        let app = app_with_profile(Profile::new(
            "acct".to_string(),
            Some("https://api.test".to_string()),
            Some("sk-test".to_string()),
        ));
        let rows = config_rows(&app);
        assert!(
            rows.contains(&ConfigRow::ApiKey),
            "api key shows in API mode"
        );
        assert!(
            !rows.contains(&ConfigRow::AutoStart),
            "auto-start does not apply to API accounts"
        );
    }

    #[test]
    fn unset_overrides_collapse_behind_reveal_chip() {
        let app = app_with_env(BTreeMap::new());
        let rows = config_rows(&app);
        assert!(
            rows.contains(&ConfigRow::ModelOverrideAdd),
            "the reveal chip stands in for the unset overrides"
        );
        for row in [
            ConfigRow::OpusModel,
            ConfigRow::SonnetModel,
            ConfigRow::HaikuModel,
            ConfigRow::SubagentModel,
        ] {
            assert!(
                !rows.contains(&row),
                "unset override is hidden while collapsed"
            );
        }
    }

    #[test]
    fn set_override_renders_others_stay_collapsed() {
        let mut profile = Profile::new("acct".to_string(), None, None);
        profile.models.opus = Some("claude-opus-4-8".to_string());
        let rows = config_rows(&app_with_profile(profile));
        assert!(
            rows.contains(&ConfigRow::OpusModel),
            "a set override always renders"
        );
        assert!(
            !rows.contains(&ConfigRow::SonnetModel),
            "an unset sibling stays hidden"
        );
        assert!(
            rows.contains(&ConfigRow::ModelOverrideAdd),
            "the chip remains while any override is still unset"
        );
    }

    #[test]
    fn reveal_chip_expands_all_overrides() {
        let mut app = app_with_env(BTreeMap::new());
        enter_detail(&mut app);
        let chip = config_rows(&app)
            .iter()
            .position(|r| *r == ConfigRow::ModelOverrideAdd)
            .expect("reveal chip present while collapsed");
        app.config_action_cursor = chip;
        super::super::run_config_row(&mut app, ConfigRow::ModelOverrideAdd);
        assert!(
            app.config_draft
                .as_ref()
                .is_some_and(|d| d.overrides_expanded),
            "⏎ on the chip expands the override block"
        );
        let rows = config_rows(&app);
        for row in [
            ConfigRow::OpusModel,
            ConfigRow::SonnetModel,
            ConfigRow::HaikuModel,
            ConfigRow::SubagentModel,
        ] {
            assert!(rows.contains(&row), "every override shows once expanded");
        }
        assert!(
            !rows.contains(&ConfigRow::ModelOverrideAdd),
            "the chip is gone once expanded"
        );
    }

    #[test]
    fn add_field_with_managed_key_prompts_collision() {
        let _home = HomeSandbox::new();
        let mut app = app_with_env(BTreeMap::new());
        enter_detail(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.env_new_key = InputState::new("ANTHROPIC_BASE_URL");
            d.active = Some(ConfigRow::EnvAdd);
        }
        super::super::commit_env_new_key(&mut app);
        assert!(
            matches!(app.modals.last(), Some(Modal::EnvCollision(_))),
            "a clauth-managed key clash raises the collision prompt"
        );
    }

    #[test]
    fn add_field_with_fresh_key_inserts_and_edits_value() {
        let _home = HomeSandbox::new();
        let mut app = app_with_env(BTreeMap::new());
        enter_detail(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.env_new_key = InputState::new("CLAUDE_CODE_MAX_OUTPUT_TOKENS");
            d.active = Some(ConfigRow::EnvAdd);
        }
        super::super::commit_env_new_key(&mut app);

        assert!(app.modals.is_empty(), "a fresh key adds without prompting");
        assert_eq!(
            app.config()
                .find("acct")
                .and_then(|p| p.env.get("CLAUDE_CODE_MAX_OUTPUT_TOKENS").cloned()),
            Some(String::new()),
            "the key is added with an empty value"
        );
        assert!(
            matches!(
                app.config_draft.as_ref().and_then(|d| d.active),
                Some(ConfigRow::EnvEntry(_))
            ),
            "focus drops into the new entry's value editor"
        );
    }
}
