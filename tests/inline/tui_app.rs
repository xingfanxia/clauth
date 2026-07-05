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

/// Seed CC's plugin registry with one clauth install record at `scope`.
fn write_plugin_install(scope: &str) {
    let path = crate::profile::claude_dir()
        .expect("claude dir")
        .join("plugins")
        .join("installed_plugins.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let body = serde_json::json!({
        "plugins": { "clauth@clauth": [{ "scope": scope, "version": "0.1.0" }] }
    });
    std::fs::write(&path, serde_json::to_vec(&body).expect("serialize")).expect("write");
}

fn plugin_check(app: &App) -> &super::Check {
    app.plugin
        .checks
        .iter()
        .find(|c| c.label == "plugin")
        .expect("plugin check present")
}

#[test]
fn plugin_check_ok_when_installed_globally() {
    let _home = crate::testutil::HomeSandbox::new();
    write_plugin_install("user");
    let mut app = bare_app();
    super::recompute_plugin_checks(&mut app, false);
    let check = plugin_check(&app);
    assert_eq!(check.health, super::Health::Ok);
    assert!(
        check
            .detail
            .iter()
            .any(|line| line.starts_with("installed: yes")),
        "global install should read installed, got {:?}",
        check.detail
    );
}

#[test]
fn plugin_check_warns_and_suggests_global_when_project_local() {
    let _home = crate::testutil::HomeSandbox::new();
    write_plugin_install("local");
    let mut app = bare_app();
    super::recompute_plugin_checks(&mut app, false);
    let check = plugin_check(&app);
    assert_eq!(check.health, super::Health::Warn);
    assert!(
        check.detail.iter().any(|line| line.contains("(local)")),
        "the project-local scope should surface in the readout, got {:?}",
        check.detail
    );
    assert!(
        check
            .detail
            .iter()
            .any(|line| line.contains("--scope user")),
        "non-global install should suggest a user-scope install, got {:?}",
        check.detail
    );
}

fn mcp_check(app: &App) -> &super::Check {
    app.plugin
        .checks
        .iter()
        .find(|c| c.label == "mcp servers")
        .expect("mcp servers check present")
}

#[test]
fn mcp_check_ok_when_globally_wired() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::plugin_probe::wire_mcp_server().expect("wire ~/.claude.json");
    let mut app = bare_app();
    super::recompute_plugin_checks(&mut app, false);
    let check = mcp_check(&app);
    assert_eq!(check.health, super::Health::Ok);
    assert!(
        check.detail.iter().any(|line| line == "present: yes"),
        "a globally wired server should read present, got {:?}",
        check.detail
    );
    assert!(check.fix.is_none());
}

#[test]
fn mcp_check_warns_project_only_for_local_plugin() {
    let _home = crate::testutil::HomeSandbox::new();
    // A project-scope plugin advertises the server for one repo only, and no
    // global `~/.claude.json` entry exists in the sandbox to make it global.
    write_plugin_install("local");
    let mut app = bare_app();
    super::recompute_plugin_checks(&mut app, false);
    let check = mcp_check(&app);
    assert_eq!(check.health, super::Health::Warn);
    assert!(
        check
            .detail
            .iter()
            .any(|line| line == "wired for this project only, not global"),
        "project-only wiring should say so in the readout, got {:?}",
        check.detail
    );
    assert!(check.fix.is_some(), "should offer the global write fix");
}

#[test]
fn runtime_check_summarizes_profiles() {
    use crate::profile::{AppConfig, AppState, Profile};
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![Profile::new("acct".to_string(), None, None)],
    });
    super::recompute_plugin_checks(&mut app, false);

    let check = app
        .plugin
        .checks
        .iter()
        .find(|c| c.label == "runtime")
        .expect("runtime check");
    // One idle, non-active, credential-less profile: no active link, no live
    // sessions → a neutral dot (not green) and no fix.
    assert_eq!(check.health, super::Health::Idle);
    assert!(check.fix.is_none());
    assert!(check.detail.iter().any(|l| l == "profiles: 1"));
    assert!(check.detail.iter().any(|l| l == "sessions: 0"));
    assert!(check.detail.iter().any(|l| l == "active: \u{2014}"));
    assert!(check.detail.iter().any(|l| l == "link: \u{2014}"));
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
fn refresh_interval_space_wraps_top_preset_to_min() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_refresh_row(&mut app);
    app.refresh_interval.store(300_000, Ordering::Relaxed); // top preset

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        15_000,
        "space at the top preset wraps to the first preset, never clamps"
    );
}

#[test]
fn refresh_interval_space_from_custom_lands_on_next_preset() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    on_refresh_row(&mut app);
    app.refresh_interval.store(45_000, Ordering::Relaxed); // custom, between 30s and 60s

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        60_000,
        "space from an off-ladder custom value steps to the next preset above it, not past it"
    );
}

#[test]
fn refresh_interval_plus_minus_are_unbound() {
    let _home = crate::testutil::HomeSandbox::new();

    // Checked in isolation: pressing `+` then `-` would cancel back to the
    // starting preset even if both still worked, which would hide a
    // regression. Each key is asserted alone against a fresh app.
    let mut app = bare_app();
    on_refresh_row(&mut app);
    let before = app.refresh_interval.load(Ordering::Relaxed);
    super::handle_global_config_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        before,
        "+ no longer steps the refresh preset; removed in favor of space-only cycling"
    );
    assert!(
        app.refresh_interval_draft.is_none(),
        "+ must not open the custom-value editor either"
    );

    let mut app = bare_app();
    on_refresh_row(&mut app);
    let before = app.refresh_interval.load(Ordering::Relaxed);
    super::handle_global_config_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.refresh_interval.load(Ordering::Relaxed),
        before,
        "- no longer steps the refresh preset either"
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

// ── banner wording ────────────────────────────────────────────────────────────
//
// "all accounts spent" needs evidence: a profile with a live spent window.
// A no-active state without one (e.g. a credential-less sole profile) gets
// the accurate "no active profile" wording instead (issue #2).

fn app_with_unlinked_profiles(profiles: Vec<crate::profile::Profile>) -> App {
    use crate::profile::{AppConfig, AppState};
    let names: Vec<_> = profiles.iter().map(|p| p.name.clone()).collect();
    App::new(AppConfig {
        state: AppState {
            profiles: names.clone(),
            fallback_chain: names,
            ..AppState::default()
        },
        profiles,
    })
}

#[test]
fn no_active_banner_without_spent_evidence() {
    use super::update_banner;
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("a")]);
    update_banner(&mut app);
    assert_eq!(
        app.banner.as_ref().expect("banner").message,
        "no active profile · select one to resume"
    );
}

#[test]
fn all_spent_banner_needs_live_spent_window() {
    use super::update_banner;
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let mut spent = crate::testutil::blank_profile("a");
    spent.usage = Some(UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 100.0,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
        }),
        ..UsageInfo::default()
    });
    let mut app = app_with_unlinked_profiles(vec![spent]);
    update_banner(&mut app);
    assert_eq!(
        app.banner.as_ref().expect("banner").message,
        "all accounts spent · switch to a profile to resume"
    );
}

// ── fallback threshold: continuous row, unchanged grammar ────────────────────
//
// The `rotate at` threshold is the one CONTINUOUS row: unlike the enumerated
// Config-tab rows, it keeps `+`/`-` for ±5 nudges alongside the `⏎` typed
// editor. This must survive the enumerated-row grammar unification untouched.

#[test]
fn fallback_threshold_plus_minus_still_nudge_both_ways() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile("a");
    profile.fallback_threshold = Some(50.0);
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = 0; // FALLBACK_ROWS[0] == Threshold

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('+')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.fallback_threshold),
        Some(55.0),
        "+ still raises the threshold by 5"
    );

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char('-')));
    assert_eq!(
        app.config().find("a").and_then(|p| p.fallback_threshold),
        Some(45.0),
        "- still lowers the threshold by 5"
    );
}

// ── fallback last-resort toggle (issue #8 follow-up) ─────────────────────────
//
// Space/⏎ on the `last resort` row flips `Profile::last_resort` and persists
// it, then kicks `refresh_tokens()` the same way `toggle_auto_start` does — a
// per-profile config.toml write doesn't bump `profiles.toml`'s mtime, so
// without the explicit kick the scheduler's cached token snapshot would lag
// until the next unrelated reload.

#[test]
fn fallback_last_resort_toggle_persists_and_refreshes_tokens() {
    use crate::profile::{ClaudeCredentials, OAuthToken};

    let _home = crate::testutil::HomeSandbox::new();
    let mut profile = crate::testutil::blank_profile("a");
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-a".to_string(),
            refresh_token: Some("rt-a".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    let mut app = app_with_unlinked_profiles(vec![profile]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0;
    app.fallback_detail_cursor = 1; // FALLBACK_ROWS[1] == LastResort

    assert!(
        !app.config().find("a").is_some_and(|p| p.last_resort),
        "precondition: last_resort starts false"
    );

    // Simulate a stale token cache (the observable proof `refresh_tokens` ran):
    // App::new already populated it from `collect_tokens`, so clear it first.
    app.usage_tokens.lock().unwrap().clear();

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    assert_eq!(
        app.config().find("a").map(|p| p.last_resort),
        Some(true),
        "space toggles last_resort on and persists it"
    );
    assert!(
        app.usage_tokens
            .lock()
            .unwrap()
            .iter()
            .any(|t| t.name == "a"),
        "toggling last_resort must call refresh_tokens() to rebuild the token cache"
    );

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.config().find("a").map(|p| p.last_resort),
        Some(false),
        "⏎ toggles last_resort back off"
    );
}

// ── capture guard ─────────────────────────────────────────────────────────────

// An empty snapshot (no creds file, no endpoint config — the macOS-keychain
// state from issue #1) must refuse loudly instead of opening the name prompt
// and persisting a credential-less profile behind a success toast.
#[test]
fn capture_refuses_empty_snapshot() {
    use super::ToastKind;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    super::begin_capture(&mut app, false);
    assert!(app.modals.is_empty(), "no name prompt on an empty snapshot");
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Danger && t.body.contains("nothing to capture")),
        "danger toast names the problem"
    );
}

// ── capture-name collision (issue #7) ──────────────────────────────────────

/// Typing an EXISTING profile's name in the capture-name prompt must open the
/// confirm-overwrite modal instead of dead-ending with an "already exists"
/// error toast.
#[test]
fn capture_name_collision_opens_overwrite_confirm_instead_of_erroring() {
    use super::ToastKind;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("acme")]);

    let snapshot = crate::actions::CaptureSnapshot {
        credentials: None,
        base_url: Some("https://new.example.com".to_string()),
        api_key: Some("new-key".to_string()),
    };
    app.modals
        .push(super::Modal::CaptureName(super::CaptureNameForm {
            snapshot: Box::new(snapshot),
            input: super::InputState::new("acme"),
            from_divergence: false,
        }));

    super::handle_capture_name_key(&mut app, key(KeyCode::Enter));

    assert!(
        app.toasts.iter().all(|t| t.kind != ToastKind::Danger),
        "typing an existing name must not dead-end with an error toast"
    );
    match app.modals.last() {
        Some(super::Modal::Confirm(state)) => {
            assert!(
                matches!(
                    &state.on_confirm,
                    super::ConfirmAction::CaptureOverwrite(_, name, false) if name.as_str() == "acme"
                ),
                "collision must route to CaptureOverwrite targeting the existing profile"
            );
        }
        other => panic!("expected a Confirm(CaptureOverwrite) modal, got {other:?}"),
    }
}

/// Cancelling the overwrite confirm must leave everything untouched: the
/// captured snapshot is dropped, config.toml/profiles.toml are byte-identical,
/// and the previously active profile stays active.
#[test]
fn capture_overwrite_cancel_changes_nothing() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut existing = crate::testutil::blank_profile("acme");
    existing.env.insert("FOO".to_string(), "bar".to_string());
    crate::profile::save_profile(&existing).expect("save existing");

    let mut app = app_with_unlinked_profiles(vec![existing]);
    app.config().state.active_profile = Some("acme".into());
    crate::profile::save_app_state(&app.config().state).expect("persist active profile");

    let config_toml = crate::profile::profile_dir("acme")
        .unwrap()
        .join("config.toml");
    let profiles_toml = crate::profile::clauth_dir().unwrap().join("profiles.toml");
    let before_config = std::fs::read(&config_toml).expect("read config.toml");
    let before_state = std::fs::read(&profiles_toml).expect("read profiles.toml");

    let snapshot = crate::actions::CaptureSnapshot {
        credentials: None,
        base_url: Some("https://new.example.com".to_string()),
        api_key: Some("new-key".to_string()),
    };
    app.modals.push(super::Modal::Confirm(super::ConfirmState {
        message: "Profile 'acme' already exists.".to_string(),
        detail: None,
        choice: false, // cancel is the default-focused, safe choice
        on_confirm: super::ConfirmAction::CaptureOverwrite(
            Box::new(snapshot),
            "acme".to_string(),
            false,
        ),
    }));

    super::handle_confirm_key(&mut app, key(KeyCode::Enter));

    assert!(app.modals.is_empty(), "cancel dismisses the modal");
    assert_eq!(
        app.config().state.active_profile.as_deref(),
        Some("acme"),
        "active profile unchanged"
    );
    assert_eq!(
        std::fs::read(&config_toml).unwrap(),
        before_config,
        "config.toml byte-identical after cancel"
    );
    assert_eq!(
        std::fs::read(&profiles_toml).unwrap(),
        before_state,
        "profiles.toml byte-identical after cancel"
    );
}

// ── Setup tab: default-model row on the `+ new` create form (issue #12) ──────
//
// The row is the same hybrid alias-cycle field an existing account's model row
// is; the create form otherwise stays minimal (no alias overrides, no env).

mod new_account_model_row {
    use super::super::{
        App, ConfigFocus, ConfigRow, InputState, Tab, commit_new_account, config_rows, cycle_model,
        enter_config_detail,
    };
    use crate::profile::{AppConfig, AppState};
    use crate::testutil::HomeSandbox;

    fn empty_app() -> App {
        App::new(AppConfig {
            state: AppState::default(),
            profiles: Vec::new(),
        })
    }

    fn enter_new_account_form(app: &mut App) {
        app.tab = Tab::Setup;
        app.profile_cursor = app.profile_count(); // trailing "+ new" row
        enter_config_detail(app);
        assert_eq!(app.config_focus, ConfigFocus::Actions);
        assert_eq!(
            app.config_draft
                .as_ref()
                .and_then(|d| d.editing_name.clone()),
            None,
            "a fresh draft has no profile yet to persist into"
        );
    }

    #[test]
    fn create_form_carries_the_model_row_before_create() {
        let mut app = empty_app();
        enter_new_account_form(&mut app);
        let rows = config_rows(&app);
        let model_pos = rows
            .iter()
            .position(|r| *r == ConfigRow::Model)
            .expect("create form carries the base model row");
        let create_pos = rows
            .iter()
            .position(|r| *r == ConfigRow::Create)
            .expect("create row present");
        assert!(model_pos < create_pos, "model row precedes create");
        assert!(
            !rows.contains(&ConfigRow::OpusModel) && !rows.contains(&ConfigRow::ModelOverrideAdd),
            "the create form stays minimal: no alias overrides"
        );
    }

    #[test]
    fn space_cycles_the_draft_model_buffer_with_no_profile_to_persist_into() {
        let mut app = empty_app();
        enter_new_account_form(&mut app);

        for expected in ["opus", "sonnet", "haiku", "opusplan"] {
            cycle_model(&mut app);
            assert_eq!(app.config_draft.as_ref().unwrap().model.value, expected);
        }
        cycle_model(&mut app);
        assert_eq!(
            app.config_draft.as_ref().unwrap().model.value,
            "",
            "cycling past the last preset collapses back to unset `default`"
        );
    }

    #[test]
    fn create_persists_the_picked_model_to_the_new_profile() {
        let _home = HomeSandbox::new();
        let mut app = empty_app();
        enter_new_account_form(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.name = InputState::new("fresh");
        }
        cycle_model(&mut app); // "" -> "opus"

        commit_new_account(&mut app);

        assert_eq!(
            app.config()
                .find("fresh")
                .and_then(|p| p.models.default.clone()),
            Some("opus".to_string()),
            "the model picked on the create form persists to the new profile"
        );
    }

    #[test]
    fn create_persists_a_custom_model_id_too() {
        let _home = HomeSandbox::new();
        let mut app = empty_app();
        enter_new_account_form(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.name = InputState::new("fresh");
            // The ⏎ custom-id editor edits this same draft buffer in place.
            d.model = InputState::new("claude-fable-5");
        }

        commit_new_account(&mut app);

        assert_eq!(
            app.config()
                .find("fresh")
                .and_then(|p| p.models.default.clone()),
            Some("claude-fable-5".to_string()),
            "a typed custom id persists through create, not just presets"
        );
    }

    #[test]
    fn create_without_touching_model_leaves_it_unset() {
        let _home = HomeSandbox::new();
        let mut app = empty_app();
        enter_new_account_form(&mut app);
        if let Some(d) = app.config_draft.as_mut() {
            d.name = InputState::new("bare");
        }

        commit_new_account(&mut app);

        assert_eq!(
            app.config()
                .find("bare")
                .and_then(|p| p.models.default.clone()),
            None,
            "default stays unset on purpose, matching default claude code behaviour"
        );
    }
}
