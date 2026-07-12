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
fn config_rows_login_and_delete_creds_visibility() {
    use super::{ConfigRow, config_rows};
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let creds = || ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };

    let mut oauth_with = Profile::new("oauth-with".to_string(), None, None);
    oauth_with.credentials = Some(creds());
    let oauth_without = Profile::new("oauth-without".to_string(), None, None);
    let api = Profile::new(
        "api".to_string(),
        Some("https://api.example.com".to_string()),
        None,
    );

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_with, oauth_without, api],
    });
    app.config_draft = None;

    // OAuth account holding creds → re-login row plus delete-creds row.
    app.profile_cursor = 0;
    let rows = config_rows(&app);
    assert!(rows.contains(&ConfigRow::Login), "oauth+creds shows login");
    assert!(
        rows.contains(&ConfigRow::DeleteCreds),
        "oauth+creds shows delete-creds"
    );

    // OAuth shell with no creds → login only.
    app.profile_cursor = 1;
    let rows = config_rows(&app);
    assert!(rows.contains(&ConfigRow::Login), "oauth blank shows login");
    assert!(
        !rows.contains(&ConfigRow::DeleteCreds),
        "oauth blank hides delete-creds"
    );

    // API account (base_url set) → neither.
    app.profile_cursor = 2;
    let rows = config_rows(&app);
    assert!(!rows.contains(&ConfigRow::Login), "api hides login");
    assert!(
        !rows.contains(&ConfigRow::DeleteCreds),
        "api hides delete-creds"
    );

    // `+ new` form with an empty base_url buffer → login before create.
    app.profile_cursor = 3;
    let rows = config_rows(&app);
    let login_idx = rows
        .iter()
        .position(|r| *r == ConfigRow::Login)
        .expect("new form shows login");
    let create_idx = rows
        .iter()
        .position(|r| *r == ConfigRow::Create)
        .expect("new form shows create");
    assert!(
        login_idx < create_idx,
        "login precedes create on the new form"
    );
}

#[test]
fn config_rows_login_hidden_when_draft_types_a_base_url() {
    use super::{ConfigRow, InputState, build_draft_existing, build_draft_new, config_rows};
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let mut oauth = Profile::new("oauth".to_string(), None, None);
    oauth.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![oauth],
    });

    // Existing OAuth draft that types a base url flips the row to API mode:
    // both OAuth-only login rows disappear even though the saved profile is OAuth.
    app.profile_cursor = 0;
    let mut draft = build_draft_existing(&app, "oauth");
    draft.base_url = InputState::new("https://api.example.com");
    app.config_draft = Some(draft);
    let rows = config_rows(&app);
    assert!(
        !rows.contains(&ConfigRow::Login),
        "typing a base url hides login on an existing account"
    );
    assert!(
        !rows.contains(&ConfigRow::DeleteCreds),
        "typing a base url hides delete-creds on an existing account"
    );

    // `+ new` form with a typed base url is an API create → no login row.
    app.profile_cursor = 1;
    let mut draft = build_draft_new();
    draft.base_url = InputState::new("https://api.example.com");
    app.config_draft = Some(draft);
    let rows = config_rows(&app);
    assert!(
        !rows.contains(&ConfigRow::Login),
        "the new form hides login once a base url makes it an API account"
    );
}

/// Minted-credential fixture for the login tests.
fn login_creds(refresh: &str) -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

/// Like [`login_creds`] but with a caller-chosen access token, so a test can
/// change the live login's fingerprint without changing its account.
fn creds_ra(refresh: &str, access: &str) -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

/// Write a plain (diverged) `~/.claude/.credentials.json` carrying `creds`.
fn write_live_creds(creds: &crate::profile::ClaudeCredentials) {
    let path = crate::profile::claude_dir()
        .expect("claude dir")
        .join(".credentials.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(&path, serde_json::to_vec(creds).expect("ser live")).expect("write live");
}

/// Force the 1Hz divergence poll to run now, bypassing its interval throttle.
fn force_poll(app: &mut App) {
    app.last_divergence_check = std::time::Instant::now() - std::time::Duration::from_secs(2);
    super::poll_credentials_divergence(app);
}

/// An in-flight login session fixture at the waiting stage.
fn login_session(name: &str, is_new: bool, generation: u64) -> super::LoginSession {
    super::LoginSession {
        name: name.to_string(),
        is_new,
        generation,
        url: None,
        stage: super::LoginStage::WaitingBrowser,
    }
}

#[test]
fn drain_login_events_discards_a_superseded_result() {
    use super::drain_login_events;
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });

    // The user superseded (or canceled) the first login: the live session now
    // carries generation 2, but a worker for generation 1 is still finishing.
    app.login_generation = 2;
    app.login = Some(login_session("ghost", true, 2));
    app.login_result_tx
        .send((1, Ok(login_creds("ref"))))
        .unwrap();

    drain_login_events(&mut app);

    assert!(
        app.config().find("ghost").is_none(),
        "a superseded login result must not create a profile"
    );
    assert!(
        app.login.is_some(),
        "the current (gen 2) session stays live; only the stale result is dropped"
    );
}

#[test]
fn login_result_on_the_new_form_stashes_into_the_draft() {
    use super::{ConfigRow, Modal, build_draft_new, config_rows, drain_login_events};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.profile_cursor = 0; // == profile_count() → the `+ new` form
    let mut draft = build_draft_new();
    draft.name = InputState::new("fresh");
    app.config_draft = Some(draft);
    app.login_generation = 1;
    app.login = Some(login_session("fresh", true, 1));
    app.modals.push(Modal::Login);
    app.login_result_tx
        .send((1, Ok(login_creds("ref"))))
        .unwrap();

    drain_login_events(&mut app);

    assert!(
        app.config().find("fresh").is_none(),
        "capture-then-commit: no profile is persisted until create fires"
    );
    assert!(
        app.config_draft
            .as_ref()
            .is_some_and(|d| d.captured_creds.is_some()),
        "the mint lands in the draft"
    );
    assert!(app.login.is_none(), "the session ends with the result");
    assert!(
        !app.modals.iter().any(|m| matches!(m, Modal::Login)),
        "the progress modal closes with the result"
    );
    let rows = config_rows(&app);
    assert_eq!(
        rows.get(app.config_action_cursor),
        Some(&ConfigRow::Create),
        "the cursor lands on `create account`"
    );
}

#[test]
fn relogin_on_a_stashed_new_form_confirms_before_replacing_the_stash() {
    use super::{ConfigFocus, ConfigRow, ConfirmAction, Modal, build_draft_new, run_config_row};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.profile_cursor = 0; // the `+ new` form
    let mut draft = build_draft_new();
    draft.name = InputState::new("fresh");
    // A mint already captured → the `✓ logged in` done-state row.
    draft.captured_creds = Some(Box::new(login_creds("stashed")));
    app.config_draft = Some(draft);
    app.config_focus = ConfigFocus::Actions;

    run_config_row(&mut app, ConfigRow::Login);

    assert!(
        matches!(
            app.modals.last(),
            Some(Modal::Confirm(s)) if matches!(s.on_confirm, ConfirmAction::RestartLogin(_, true))
        ),
        "⏎ on a stashed new-form login must confirm before dropping the capture",
    );
    assert!(
        app.login.is_none(),
        "no login worker starts until the confirm is accepted",
    );
}

#[test]
fn login_result_with_the_form_closed_is_dropped_with_a_warning() {
    use super::{ToastKind, drain_login_events};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.config_draft = None; // form abandoned during the browser round-trip
    app.login_generation = 1;
    app.login = Some(login_session("fresh", true, 1));
    app.login_result_tx
        .send((1, Ok(login_creds("ref"))))
        .unwrap();

    drain_login_events(&mut app);

    assert!(app.config().find("fresh").is_none());
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Warning && t.body.contains("no longer open")),
        "dropping a real browser round-trip must be surfaced"
    );
}

#[test]
fn commit_new_account_consumes_the_draft_mint() {
    use super::{build_draft_new, commit_new_account};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.profile_cursor = 0;
    let mut draft = build_draft_new();
    draft.name = InputState::new("fresh");
    draft.model = InputState::new("opus");
    draft.captured_creds = Some(Box::new(login_creds("minted")));
    app.config_draft = Some(draft);

    commit_new_account(&mut app);

    let cfg = app.config();
    let profile = cfg
        .find("fresh")
        .expect("create account persists the profile");
    assert_eq!(
        profile.refresh_token(),
        Some("minted"),
        "the draft-held mint is saved with the profile"
    );
    assert_eq!(
        profile.models.default.as_deref(),
        Some("opus"),
        "the model row folds into the same create"
    );
    assert_eq!(
        cfg.state.active_profile.as_deref(),
        Some("fresh"),
        "the first profile links and activates like a capture"
    );
}

#[test]
fn login_stage_events_advance_the_session() {
    use super::{LoginEvent, LoginStage, drain_login_events};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.login_generation = 1;
    app.login = Some(login_session("fresh", true, 1));

    app.login_event_tx
        .send((1, LoginEvent::Url("https://example.test/auth".to_string())))
        .unwrap();
    app.login_event_tx
        .send((1, LoginEvent::Stage(LoginStage::ExchangingCode)))
        .unwrap();
    // A stale generation's stage bump is ignored.
    app.login_event_tx
        .send((7, LoginEvent::Stage(LoginStage::Verifying)))
        .unwrap();

    drain_login_events(&mut app);

    let session = app.login.as_ref().expect("session stays live");
    assert_eq!(session.url.as_deref(), Some("https://example.test/auth"));
    assert_eq!(session.stage, LoginStage::ExchangingCode);
}

#[test]
fn login_modal_esc_collapses_without_canceling() {
    use super::{KeyCode, Modal, handle_key, start_login};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.login_generation = 1;
    app.login = Some(login_session("fresh", true, 1));
    app.modals.push(Modal::Login);

    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert!(app.modals.is_empty(), "esc pops the modal");
    assert!(
        app.login.is_some(),
        "the login keeps running while collapsed"
    );
    assert_eq!(
        app.login_generation, 1,
        "collapsing must not bump the generation"
    );

    // ⏎ on the login row while one is in flight re-expands instead of
    // starting a second login.
    start_login(&mut app, "other".to_string(), false);
    assert!(
        app.modals.iter().any(|m| matches!(m, Modal::Login)),
        "a repeat login request reopens the progress modal"
    );
    assert_eq!(
        app.login.as_ref().map(|s| s.name.as_str()),
        Some("fresh"),
        "the in-flight session is untouched"
    );
    app.modals.clear();

    // Collapsed, top-level q cancels too (symmetric with esc) — it must not
    // arm the 2-step quit or ascend out of a Setup form while a login runs.
    handle_key(&mut app, crate::testutil::key(KeyCode::Char('q')));
    assert!(app.login.is_none(), "top-level q cancels the login");
    assert!(!app.quit, "canceling a login must not quit the app");

    // And esc is the equivalent cancel path.
    app.login_generation = 2;
    app.login = Some(login_session("fresh", true, 2));
    handle_key(&mut app, crate::testutil::key(KeyCode::Esc));
    assert!(app.login.is_none(), "top-level esc cancels the login");
}

#[test]
fn relogin_gate_maps_divergence_defaults() {
    use super::{ConfirmAction, Modal, apply_login};
    use crate::profile::{AppConfig, AppState, DivergenceChoice, Profile};
    let _home = crate::testutil::HomeSandbox::new();

    let profile_with = |refresh: &str| {
        let mut p = Profile::new("work".to_string(), None, None);
        p.credentials = Some(login_creds(refresh));
        p
    };

    // Unset default (ask) → confirm modal, stored creds untouched.
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile_with("old")],
    });
    apply_login(
        &mut app,
        login_session("work", false, 1),
        login_creds("new"),
    );
    assert!(
        matches!(
            app.modals.last(),
            Some(Modal::Confirm(state))
                if matches!(&state.on_confirm, ConfirmAction::CaptureOverwrite(_, name, false) if name == "work")
        ),
        "an unset divergence default must ask before overwriting"
    );
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("old"),
        "stored creds stay until the user confirms"
    );
    // Confirming actually lands the deferred overwrite.
    let Some(Modal::Confirm(state)) = app.modals.pop() else {
        unreachable!("asserted above");
    };
    super::run_confirm_action(&mut app, state.on_confirm);
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("new"),
        "the confirmed re-login replaces the stored creds"
    );

    // NewProfile / Discard defaults also ask — only Overwrite applies silently.
    for choice in [DivergenceChoice::NewProfile, DivergenceChoice::Discard] {
        let mut app = App::new(AppConfig {
            state: AppState {
                default_divergence: Some(choice),
                ..AppState::default()
            },
            profiles: vec![profile_with("old")],
        });
        apply_login(
            &mut app,
            login_session("work", false, 1),
            login_creds("new"),
        );
        assert!(
            matches!(app.modals.last(), Some(Modal::Confirm(_))),
            "{choice:?} must gate the overwrite behind a confirm"
        );
    }

    // Overwrite default → applied immediately, no modal.
    let mut app = App::new(AppConfig {
        state: AppState {
            default_divergence: Some(DivergenceChoice::Overwrite),
            ..AppState::default()
        },
        profiles: vec![profile_with("old")],
    });
    apply_login(
        &mut app,
        login_session("work", false, 1),
        login_creds("new"),
    );
    assert!(
        app.modals.is_empty(),
        "an Overwrite default applies silently"
    );
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("new"),
        "the re-login replaced the stored creds"
    );

    // Credential-less profile: nothing diverges → silent apply even when unset.
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![Profile::new("work".to_string(), None, None)],
    });
    apply_login(
        &mut app,
        login_session("work", false, 1),
        login_creds("new"),
    );
    assert!(
        app.modals.is_empty(),
        "no stored creds means no divergence to gate on"
    );
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("new"),
        "the first login adopts silently"
    );
}

// Issue #20: dismissing the divergence prompt must stop the 1Hz poll from
// re-pushing it every second. The snooze releases only when the live login
// itself changes (a fresh /login or refresh), so a stale account isn't nagged
// forever yet a genuinely new one still surfaces.
#[test]
fn divergence_dismiss_snoozes_until_the_live_login_changes() {
    use super::{Modal, handle_key};
    use crate::profile::{AppConfig, AppState, Profile, save_profile};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("rt-work"));
    save_profile(&work).expect("save work");
    write_live_creds(&creds_ra("rt-live", "at-1"));

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("work".into()),
            ..AppState::default()
        },
        profiles: vec![work],
    });

    force_poll(&mut app);
    assert!(
        matches!(app.modals.last(), Some(Modal::Divergence(_))),
        "a diverged active profile prompts"
    );

    handle_key(&mut app, key(KeyCode::Esc));
    assert!(app.modals.is_empty(), "esc dismisses the prompt");
    assert!(app.divergence_snooze.is_some(), "esc records a snooze");

    force_poll(&mut app);
    assert!(
        app.modals.is_empty(),
        "the same dismissed login must not re-prompt"
    );

    // A fresh login (new access token, same or different account) re-prompts.
    write_live_creds(&creds_ra("rt-live", "at-2"));
    force_poll(&mut app);
    assert!(
        matches!(app.modals.last(), Some(Modal::Divergence(_))),
        "a changed live login re-prompts once"
    );
}

// Issue #20: "save elsewhere" must let the user route the live login into a
// profile OTHER than the active one, so re-logging a second account while the
// wrong profile is active no longer forces two profiles onto one account.
#[test]
fn divergence_picker_saves_the_login_into_a_chosen_profile() {
    use super::{ConfirmAction, Modal, handle_key, run_confirm_action, run_divergence_choice};
    use crate::profile::{AppConfig, AppState, DivergenceChoice, Profile, save_profile};
    use crate::testutil::key;
    use ratatui::crossterm::event::KeyCode;
    let _home = crate::testutil::HomeSandbox::new();

    let mut work = Profile::new("work".to_string(), None, None);
    work.credentials = Some(login_creds("rt-work"));
    save_profile(&work).expect("save work");
    let mut spare = Profile::new("spare".to_string(), None, None);
    spare.credentials = Some(login_creds("rt-spare"));
    save_profile(&spare).expect("save spare");
    // CC re-logged an account; the live file carries a fresh token that matches
    // no stored profile (a re-login mints a new refresh token).
    write_live_creds(&creds_ra("rt-fresh", "at-fresh"));

    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("work".into()),
            ..AppState::default()
        },
        profiles: vec![work, spare],
    });

    // "save elsewhere" opens the picker listing only the non-active profile.
    run_divergence_choice(&mut app, "work", DivergenceChoice::NewProfile);
    let Some(Modal::DivergenceTarget(form)) = app.modals.last() else {
        panic!("expected the target picker, got {:?}", app.modals.last());
    };
    assert_eq!(
        form.targets,
        vec!["spare".to_string()],
        "the active profile is never an overwrite target"
    );

    // Move to "spare" (row 1) and pick it.
    handle_key(&mut app, key(KeyCode::Down));
    handle_key(&mut app, key(KeyCode::Enter));
    let Some(Modal::Confirm(state)) = app.modals.last() else {
        panic!(
            "expected the overwrite confirm, got {:?}",
            app.modals.last()
        );
    };
    assert!(
        matches!(&state.on_confirm, ConfirmAction::AdoptDivergence(_, name) if name == "spare"),
        "the confirm adopts the live login into the chosen profile"
    );

    let Some(Modal::Confirm(state)) = app.modals.pop() else {
        unreachable!("asserted above");
    };
    run_confirm_action(&mut app, state.on_confirm);

    assert_eq!(
        app.config().find("spare").and_then(|p| p.refresh_token()),
        Some("rt-fresh"),
        "the live login landed in the chosen profile"
    );
    assert_eq!(
        app.config().find("work").and_then(|p| p.refresh_token()),
        Some("rt-work"),
        "the previously active profile is untouched"
    );
    assert_eq!(
        app.config().state.active_profile.as_deref(),
        Some("spare"),
        "the chosen profile becomes active so the divergence is resolved"
    );
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

// ── burn-aware switching (issue #8 follow-up b) ─────────────────────────────

#[test]
fn burn_aware_row_is_reachable_by_cursor() {
    let mut app = bare_app();
    app.tab = Tab::Config;
    let pos = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::BurnAware)
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

#[test]
fn burn_aware_space_toggles_and_persists() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::BurnAware)
        .unwrap();
    assert!(!app.config().state.burn_aware_switching, "off by default");

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        app.config().state.burn_aware_switching,
        "space toggles the mode on"
    );

    // Persisted to profiles.toml, not just the in-memory config — reload it
    // fresh, the way a relaunch would pick up the flag.
    let reloaded: crate::profile::AppState = toml::from_str(
        &std::fs::read_to_string(crate::profile::clauth_dir().unwrap().join("profiles.toml"))
            .expect("read profiles.toml"),
    )
    .expect("parse profiles.toml");
    assert!(reloaded.burn_aware_switching, "toggle persists to disk");

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        !app.config().state.burn_aware_switching,
        "space toggles the mode back off"
    );
}

// ── preemptive rotation (rotation coherence #1) ─────────────────────────────

#[test]
fn preemptive_rotation_space_toggles_and_persists() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Config;
    app.global_config_cursor = GLOBAL_CONFIG_ROWS
        .iter()
        .position(|r| *r == GlobalConfigRow::PreemptiveRotation)
        .unwrap();
    assert!(
        !app.config().state.preemptive_rotation,
        "off by default — stock stays strictly lazy"
    );

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        app.config().state.preemptive_rotation,
        "space toggles the mode on"
    );

    // Persisted to profiles.toml — the scheduler reads the flag off the
    // shared config, but a relaunch must pick it up from disk too.
    let reloaded: crate::profile::AppState = toml::from_str(
        &std::fs::read_to_string(crate::profile::clauth_dir().unwrap().join("profiles.toml"))
            .expect("read profiles.toml"),
    )
    .expect("parse profiles.toml");
    assert!(reloaded.preemptive_rotation, "toggle persists to disk");

    super::handle_global_config_key(&mut app, key(KeyCode::Char(' ')));
    assert!(
        !app.config().state.preemptive_rotation,
        "space toggles the mode back off"
    );
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

// The chain has one parking spot: marking a member clears the mark everywhere
// else (radio), so two accounts can never both read `last resort ─●`.
#[test]
fn fallback_last_resort_is_exclusive_across_the_chain() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut b = crate::testutil::blank_profile("b");
    b.last_resort = true;
    let mut app = app_with_unlinked_profiles(vec![crate::testutil::blank_profile("a"), b]);
    app.tab = Tab::Fallback;
    app.fallback_focus = super::FallbackFocus::Detail;
    app.chain_cursor = 0; // member "a"
    app.fallback_detail_cursor = 1; // FALLBACK_ROWS[1] == LastResort

    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));

    assert_eq!(
        app.config().find("a").map(|p| p.last_resort),
        Some(true),
        "space marks the selected member"
    );
    assert_eq!(
        app.config().find("b").map(|p| p.last_resort),
        Some(false),
        "marking one member clears the previous last resort"
    );
    assert!(
        app.toasts.iter().any(|t| t.body.contains("moved from 'b'")),
        "the move away from the old member is surfaced"
    );

    // Turning the mark OFF touches nobody else.
    super::handle_fallback_detail_key(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(app.config().find("a").map(|p| p.last_resort), Some(false));
    assert_eq!(app.config().find("b").map(|p| p.last_resort), Some(false));
}

// ── tokens tab: model filter via the action menu ─────────────────────────────

#[test]
fn tokens_action_menu_sets_and_swaps_the_model_filter() {
    use super::{ActionMenuAction, TokenFilter, build_action_menu, dispatch_action_menu_action};
    use crate::tokens::{ModelTokens, TokenStats};

    let _home = crate::testutil::HomeSandbox::new();
    let mut app = bare_app();
    app.tab = Tab::Tokens;
    // Both models sit above OTHERS_THRESHOLD, so they group individually.
    app.token_stats = Some(TokenStats {
        models: vec![
            ModelTokens {
                model: "claude-opus-4-8".to_string(),
                input: 10_000_000,
                ..Default::default()
            },
            ModelTokens {
                model: "gpt-x".to_string(),
                input: 5_000_000,
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    assert_eq!(super::token_model_count(&app), 2);

    // The menu offers the two inactive lenses plus the page-key mirrors.
    let labels: Vec<&str> = build_action_menu(&app)
        .items
        .iter()
        .map(|i| i.label)
        .collect();
    assert_eq!(
        labels,
        vec![
            "period: daily",
            "period: weekly",
            "period: monthly",
            "show claude models",
            "show other models",
            "toggle cache counting",
            "reload stats"
        ]
    );

    // Narrow to claude models; the cursor re-clamps into the shorter list.
    app.token_model_cursor = 1;
    dispatch_action_menu_action(&mut app, ActionMenuAction::TokensShowClaude);
    assert_eq!(app.token_filter, TokenFilter::Claude);
    assert_eq!(super::token_model_count(&app), 1);
    assert_eq!(
        app.token_model_cursor, 0,
        "cursor clamps into the filtered list"
    );

    // The active lens drops out of the menu; "show all" takes its place.
    let labels: Vec<&str> = build_action_menu(&app)
        .items
        .iter()
        .map(|i| i.label)
        .collect();
    assert!(labels.contains(&"show all models"));
    assert!(!labels.contains(&"show claude models"));
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

// ── AUTH-1 gate on the TUI switch (Incident C, every entry point) ───────────

/// The UI-thread switch shares the quarantine refusal: a flagged target's
/// dead token must never land in the Keychain. Flag-only — no HTTP on the UI
/// thread — so it keys on `is_auth_broken`, which the poller and every
/// refresh site keep current.
#[test]
fn tui_switch_refuses_a_quarantined_target_with_login_hint() {
    use super::ToastKind;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_unlinked_profiles(vec![
        crate::testutil::blank_profile("healthy"),
        crate::testutil::blank_profile("broken"),
    ]);
    app.config().set_auth_broken("broken", true);

    super::perform_switch(&mut app, "broken");

    assert!(
        !app.config().is_active("broken"),
        "a quarantined target must never become active"
    );
    assert!(
        app.toasts
            .iter()
            .any(|t| t.kind == ToastKind::Danger && t.body.contains("clauth login broken")),
        "the refusal names the recovery"
    );
}

#[test]
fn tokens_period_key_cycles_and_clamps_cursor() {
    use super::{KeyCode, Tab, TokenPeriod, TokenView, handle_key};
    use crate::profile::{AppConfig, AppState};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.tab = Tab::Tokens;
    // Two lifetime rows; the daily/scoped lists are empty (no `today`, no
    // per-day models), so cycling must clamp the Models cursor.
    app.token_stats = Some(crate::tokens::TokenStats {
        models: vec![
            crate::tokens::ModelTokens {
                model: "claude-opus-4".into(),
                input: 10,
                output: 5,
                cache_read: 0,
                cache_create: 0,
            },
            crate::tokens::ModelTokens {
                model: "claude-sonnet-4".into(),
                input: 8,
                output: 4,
                cache_read: 0,
                cache_create: 0,
            },
        ],
        ..Default::default()
    });
    app.token_view = TokenView::Models;
    app.token_model_cursor = 1;

    for expected in [
        TokenPeriod::Daily,
        TokenPeriod::Weekly,
        TokenPeriod::Monthly,
        TokenPeriod::Lifetime,
    ] {
        handle_key(&mut app, crate::testutil::key(KeyCode::Char('t')));
        assert_eq!(app.token_period, expected, "t cycles in declared order");
    }
    // The first hop landed on the empty daily list, so the cursor was clamped
    // to 0 and stays there through the full cycle.
    assert_eq!(app.token_model_cursor, 0, "cursor clamps on an empty lens");
}

// ── tokens tab: loading-spinner busy flag ─────────────────────────────────────

/// `tokens_topping_up` drives the tab's loading spinners. Only a seeding `Base`
/// (first paint) or a manual reload lights it; `Loaded`/`Failed` clear it, and a
/// silent periodic `Base` (stats already present) must stay dark.
#[test]
fn tokens_topping_up_tracks_the_load_lifecycle() {
    use super::{drain_tokens_events, reload_token_stats};
    use crate::profile::{AppConfig, AppState};
    use crate::tokens::{TokenStats, TokensEvent};
    let _home = crate::testutil::HomeSandbox::new();

    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    // App::new drops the loader's sender under cfg(test); rewire a live channel
    // so the test can feed the loader's events.
    let (tx, rx) = std::sync::mpsc::channel();
    app.tokens_events = rx;

    assert!(app.token_stats.is_none());
    assert!(!app.tokens_topping_up);

    // First (seeding) Base: paints the cache and marks the top-up in flight.
    tx.send(TokensEvent::Base(Box::<TokenStats>::default()))
        .unwrap();
    drain_tokens_events(&mut app);
    assert!(app.token_stats.is_some(), "seeding Base paints the tab");
    assert!(
        app.tokens_topping_up,
        "a seeding Base lights the loading flag"
    );

    // Sweep progress lands in `tokens_progress` while the top-up runs.
    tx.send(TokensEvent::Progress {
        done: 25,
        total: 380,
    })
    .unwrap();
    drain_tokens_events(&mut app);
    assert_eq!(
        app.tokens_progress,
        Some((25, 380)),
        "Progress stores the sweep counts"
    );

    // Loaded clears both the flag and the counts.
    tx.send(TokensEvent::Loaded(Box::<TokenStats>::default()))
        .unwrap();
    drain_tokens_events(&mut app);
    assert!(!app.tokens_topping_up, "Loaded clears the loading flag");
    assert_eq!(app.tokens_progress, None, "Loaded clears the sweep counts");

    // A silent periodic Base (stats already present) must NOT relight it.
    tx.send(TokensEvent::Base(Box::<TokenStats>::default()))
        .unwrap();
    drain_tokens_events(&mut app);
    assert!(
        !app.tokens_topping_up,
        "a non-seeding periodic Base stays silent"
    );

    // Manual reload lights it (and drops any stale counts); a subsequent
    // Failed clears both.
    app.tokens_progress = Some((1, 2));
    reload_token_stats(&mut app);
    assert!(app.tokens_topping_up, "manual reload lights the flag");
    assert_eq!(
        app.tokens_progress, None,
        "manual reload drops stale sweep counts"
    );
    tx.send(TokensEvent::Failed).unwrap();
    drain_tokens_events(&mut app);
    assert!(!app.tokens_topping_up, "Failed clears the loading flag");
    assert_eq!(app.tokens_progress, None, "Failed clears the sweep counts");
}
