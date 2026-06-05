//! Behaviour tests for `rotation_candidates` — the filter that decides which
//! profiles `refresh_all` will attempt to rotate.
//!
//! These tests never touch the network. They assert on the candidate list
//! returned by `rotation_candidates`, which is the only part of `refresh_all`
//! that `force` affects.

use super::*;
use crate::lockorder::RankedMutex;
use crate::profile::{AppState, ClaudeCredentials, OAuthToken, Profile, profile_dir};
use crate::runtime::open_pid_file;
use crate::usage::is_idle;

fn single_profile_config(name: &str, refresh_token: &str) -> AppConfig {
    use std::collections::BTreeMap;
    let profile = Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        fallback_threshold: None,
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "at".to_string(),
                refresh_token: Some(refresh_token.to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push(name.into());
    config
}

/// RAII home sandbox: holds `HOME_TEST_LOCK` and redirects `home_dir()` into a
/// tempdir for the test's duration, clearing on drop (even on panic). Required
/// for any test that creates session dirs, pid files, or rotation locks —
/// including indirectly via `RotationGuard::acquire` — or those paths land in
/// the real `~/.clauth`.
struct HomeSandbox {
    // Drop order: tempdir first, then shared lock.
    _tmp: tempfile::TempDir,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl HomeSandbox {
    fn new() -> Self {
        let guard = crate::profile::HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("create home sandbox");
        crate::profile::set_home_override(tmp.path().to_path_buf());
        Self {
            _tmp: tmp,
            _guard: guard,
        }
    }
}

impl Drop for HomeSandbox {
    fn drop(&mut self) {
        crate::profile::clear_home_override();
    }
}

#[test]
fn no_live_session_included_with_force_false() {
    let config = single_profile_config("test-oauth-no-session-force-false", "rt-abc");
    let candidates = rotation_candidates(&config, false);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].0, "test-oauth-no-session-force-false");
    assert_eq!(candidates[0].1, "rt-abc");
}

#[test]
fn no_live_session_included_with_force_true() {
    let config = single_profile_config("test-oauth-no-session-force-true", "rt-def");
    let candidates = rotation_candidates(&config, true);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].0, "test-oauth-no-session-force-true");
}

#[test]
fn live_session_excluded_when_force_false() {
    let _home = HomeSandbox::new();
    let name = "test-oauth-live-session-guard";
    let sessions = profile_dir(name).expect("profile_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let pid_file = sessions.join("test-pid");
    let file = open_pid_file(&pid_file).expect("open pid file");
    file.lock().expect("lock pid file");

    let config = single_profile_config(name, "rt-ghi");
    let candidates = rotation_candidates(&config, false);
    assert!(
        candidates.is_empty(),
        "force=false should exclude a profile with a live session"
    );

    drop(file);
}

#[test]
fn live_session_included_when_force_true() {
    let _home = HomeSandbox::new();
    let name = "test-oauth-live-session-force";
    let sessions = profile_dir(name).expect("profile_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let pid_file = sessions.join("test-pid");
    let file = open_pid_file(&pid_file).expect("open pid file");
    file.lock().expect("lock pid file");

    let config = single_profile_config(name, "rt-jkl");
    let candidates = rotation_candidates(&config, true);
    assert_eq!(
        candidates.len(),
        1,
        "force=true should include a profile with a live session"
    );
    assert_eq!(candidates[0].0, name);

    drop(file);
}

#[test]
fn force_true_bypasses_diverged_active_when_no_active_profile() {
    // active_profile is None → active_link_diverged returns false → both force values include the
    // profile. The meaningful contract is `!force && active_link_diverged(config)` (was
    // `active_link_diverged(config)`, ignoring force); tested here without FS side effects.
    let config = single_profile_config("test-oauth-force-diverged", "rt-xyz");
    let force_false = rotation_candidates(&config, false);
    let force_true = rotation_candidates(&config, true);
    assert_eq!(force_false.len(), 1);
    assert_eq!(force_true.len(), 1);
    assert_eq!(force_true[0].0, "test-oauth-force-diverged");
}

/// `rotate_one_inner` must not stamp `Refreshing` when no refresh token —
/// the short-circuit runs before any HTTP, leaving the activity slot Idle.
#[test]
fn rotate_one_no_stamp_when_no_refresh_token() {
    use std::collections::BTreeMap;
    use std::sync::mpsc;

    let _home = HomeSandbox::new();
    let profile = Profile {
        name: "test-rotate-one-no-rt".into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        fallback_threshold: None,
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "at".to_string(),
                refresh_token: None,
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push("test-rotate-one-no-rt".into());

    let config = Arc::new(RankedMutex::new(config));
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let (tx, _rx) = mpsc::channel();

    let result = rotate_one_inner(
        &config,
        "test-rotate-one-no-rt",
        Some(&activity),
        &tx,
        false,
    );

    assert!(
        matches!(result, RotateOutcome::Persisted(false)),
        "rotate_one_inner should return Persisted(false) when no refresh token"
    );
    assert!(
        is_idle(&activity, "test-rotate-one-no-rt"),
        "activity slot must remain Idle when rotation short-circuits at no-token"
    );
}

#[test]
fn profile_without_refresh_token_excluded() {
    use std::collections::BTreeMap;
    let profile = Profile {
        name: "test-oauth-no-rt".into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        fallback_threshold: None,
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "at".to_string(),
                refresh_token: None,
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push("test-oauth-no-rt".into());
    assert!(rotation_candidates(&config, false).is_empty()); // no refresh token → excluded regardless of force
    assert!(rotation_candidates(&config, true).is_empty());
}

/// Per-profile rotation lock: acquiring for `b` must not block while `a` is held.
/// Without this, `refresh_all` workers would serialize behind the slowest profile.
/// `b` is acquired on a separate thread because the ROTATION rank forbids a single
/// thread from re-entering it — exactly the cross-thread guarantee needed.
#[test]
fn rotation_guard_is_independent_across_profiles() {
    use crate::runtime::RotationGuard;
    use std::sync::mpsc;
    use std::time::Duration;

    // HOME_OVERRIDE is process-global, so the worker thread's acquire also resolves into the sandbox.
    let _home = HomeSandbox::new();
    let a = "test-rotation-guard-indep-a";
    let b = "test-rotation-guard-indep-b";
    let held_a = RotationGuard::acquire(a).expect("acquire a");

    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let held_b = RotationGuard::acquire(b).expect("acquire b while a is held"); // distinct lock file → must not block
        tx.send(()).expect("signal acquired");
        drop(held_b);
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("acquiring b must not block on a (per-profile locks are independent)");
    worker.join().expect("join b worker");
    drop(held_a);
}

/// Live session must NOT exclude a windowless profile: CC holds the lock but only
/// opens a window on first message. Kick is access-token-only (safe).
/// Regression: "CC open on background account, usage never started".
#[test]
fn windowless_candidate_even_with_live_session() {
    use std::collections::HashMap;

    let _home = HomeSandbox::new();
    let name = "test-windowless-live-session";

    let sessions = profile_dir(name).expect("profile_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let pid_file = sessions.join("test-pid-live");
    let file = open_pid_file(&pid_file).expect("open pid file");
    file.lock().expect("lock pid file");

    let mut config = single_profile_config(name, "rt-live-session");
    config.profiles[0].auto_start = true;
    let config = Arc::new(RankedMutex::new(config));

    // empty store → no 5h window → windowless; live session must not change this
    let store: crate::usage::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));

    let candidates = windowless_auto_start_candidates(&config, &store);
    assert_eq!(
        candidates,
        vec![name.to_string()],
        "a windowless profile must be re-armed even while a live session holds its lock"
    );

    drop(file);
}

/// A past `resets_at` must not be treated as a live window — profile must be re-armed.
/// Regression: `.is_some()` check treated stale timestamp as live, so the profile was
/// never re-kicked ("auto-start only works for the active account").
#[test]
fn windowless_candidate_when_resets_at_is_in_the_past() {
    use std::collections::HashMap;

    let _home = HomeSandbox::new();
    let name = "test-windowless-expired-window";

    let mut config = single_profile_config(name, "rt-expired");
    config.profiles[0].auto_start = true;
    let config = Arc::new(RankedMutex::new(config));

    let store: crate::usage::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));

    // resets_at in past → still windowless → candidate
    let expired = crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 0.0,
            resets_at: Some("2020-01-01T00:00:00Z".to_string()),
        }),
        ..Default::default()
    };
    store.lock().unwrap().insert(name.to_string(), expired);

    let candidates = windowless_auto_start_candidates(&config, &store);
    assert_eq!(
        candidates,
        vec![name.to_string()],
        "a profile with an expired (past) resets_at must be re-armed"
    );

    // resets_at in future → live window → excluded
    let live = crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 0.0,
            resets_at: Some("2999-01-01T00:00:00Z".to_string()),
        }),
        ..Default::default()
    };
    store.lock().unwrap().insert(name.to_string(), live);

    let candidates = windowless_auto_start_candidates(&config, &store);
    assert!(
        candidates.is_empty(),
        "a profile with a future resets_at still has a live window — no re-arm"
    );
}
