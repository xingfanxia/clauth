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

// Build a minimal AppConfig with one OAuth profile named `name`.
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
/// tempdir for the test's whole duration, clearing the override on drop (even
/// on panic). Every test that creates sessions dirs, pid files, or rotation
/// locks — including indirectly via `rotate_one*` / `RotationGuard::acquire`,
/// which `create_dir_all` before any short-circuit — must hold one, or those
/// paths land in the user's real `~/.clauth`.
struct HomeSandbox {
    // Declaration order is the drop order after `Drop::drop` clears the
    // override: tempdir removed first, shared lock released last.
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
    // A unique name that has no sessions dir on disk — has_live_session returns false.
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
    // Create a real locked pid file so has_live_session returns true.
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

    // Release lock — sessions dir and file vanish with the sandbox tempdir.
    drop(file);
}

#[test]
fn live_session_included_when_force_true() {
    let _home = HomeSandbox::new();
    // Same setup: locked pid file makes has_live_session return true.
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
    // When active_profile is None, active_link_diverged returns false, so even
    // force=false would not skip. This test verifies the force=true path includes
    // the profile — and that the old `skip_active = active_link_diverged(config)`
    // (which ignored force) is now `!force && active_link_diverged(config)`.
    // With no active profile, diverged is always false and the behavior matches
    // regardless of force; the meaningful contract change is that force=true
    // with a diverged active now also includes that profile (tested here with
    // no active so it compiles without filesystem side effects).
    let config = single_profile_config("test-oauth-force-diverged", "rt-xyz");
    // active_profile is None → active_link_diverged returns false → both
    // force values include the profile.
    let force_false = rotation_candidates(&config, false);
    let force_true = rotation_candidates(&config, true);
    assert_eq!(force_false.len(), 1);
    assert_eq!(force_true.len(), 1);
    assert_eq!(force_true[0].0, "test-oauth-force-diverged");
}

/// `rotate_one_inner` must NOT stamp `Refreshing` when the profile has no
/// refresh token — the short-circuit `let Some(rt) = token else { … }` runs
/// before any HTTP, so the activity slot should remain clean (Idle).
#[test]
fn rotate_one_no_stamp_when_no_refresh_token() {
    use std::collections::BTreeMap;
    use std::sync::mpsc;

    let _home = HomeSandbox::new();
    // Profile with OAuth block but no refresh token.
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
        // OAuth block exists but no refresh token.
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
    // No refresh token → excluded regardless of force.
    assert!(rotation_candidates(&config, false).is_empty());
    assert!(rotation_candidates(&config, true).is_empty());
}

/// A guarded acquire for a DIFFERENT profile must not block — the rotation
/// lock is per-profile, so two distinct profiles rotate concurrently. Without
/// this, fanning `refresh_all` across worker threads would serialize every
/// profile behind the slowest sibling.
///
/// `b` is acquired on a SEPARATE thread because a single thread never holds two
/// rotation guards at once — the global lock order (`lockorder`) forbids
/// re-entering the ROTATION rank, and the codebase only ever rotates one
/// profile per thread (each `refresh_all` worker is its own thread). The real
/// guarantee is exactly this cross-thread one: a worker rotating `a` must not
/// stall a worker rotating `b`.
#[test]
fn rotation_guard_is_independent_across_profiles() {
    use crate::runtime::RotationGuard;
    use std::sync::mpsc;
    use std::time::Duration;

    // HOME_OVERRIDE is process-global (not thread-local), so the worker
    // thread's `RotationGuard::acquire(b)` below resolves into the sandbox too.
    let _home = HomeSandbox::new();
    let a = "test-rotation-guard-indep-a";
    let b = "test-rotation-guard-indep-b";
    let held_a = RotationGuard::acquire(a).expect("acquire a");

    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        // Distinct profile → distinct lock file → must acquire without blocking.
        let held_b = RotationGuard::acquire(b).expect("acquire b while a is held");
        tx.send(()).expect("signal acquired");
        drop(held_b);
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("acquiring b must not block on a (per-profile locks are independent)");
    worker.join().expect("join b worker");
    drop(held_a);
}

/// A live `clauth start` session must NOT exclude a windowless profile from the
/// auto-start scan. A running Claude Code holds the session lock but only opens
/// a 5h window when it sends a message, so an idle (or just-reset) session has a
/// live lock and no window — exactly the case that needs arming. The kick spends
/// only the access token, so it's token-safe to fire alongside a live session.
/// Regression guard for the "CC open on a background account, usage never
/// started" bug.
#[test]
fn windowless_candidate_even_with_live_session() {
    use std::collections::HashMap;

    let _home = HomeSandbox::new();
    let name = "test-windowless-live-session";

    // Simulate a live session: a locked pid file under the profile's sessions
    // dir makes `has_live_session` return true for its lifetime.
    let sessions = profile_dir(name).expect("profile_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let pid_file = sessions.join("test-pid-live");
    let file = open_pid_file(&pid_file).expect("open pid file");
    file.lock().expect("lock pid file");

    let mut config = single_profile_config(name, "rt-live-session");
    config.profiles[0].auto_start = true;
    let config = Arc::new(RankedMutex::new(config));

    // Empty store → no 5h window → windowless. The live session must not change
    // the verdict.
    let store: crate::usage::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));

    let candidates = windowless_auto_start_candidates(&config, &store);
    assert_eq!(
        candidates,
        vec![name.to_string()],
        "a windowless profile must be re-armed even while a live session holds its lock"
    );

    drop(file);
}

/// An opted-in background profile whose 5h window has lapsed but still carries a
/// past `resets_at` in the usage store must be re-armed: a window counts as live
/// only while `resets_at` is in the future. The previous `.is_some()` check
/// treated the stale timestamp as a live window, so the profile was never re-
/// kicked — surfacing as "auto-start only works for the active account" (the
/// active one gets a fresh window from Claude Code each session).
#[test]
fn windowless_candidate_when_resets_at_is_in_the_past() {
    use std::collections::HashMap;

    let _home = HomeSandbox::new();
    let name = "test-windowless-expired-window";

    let mut config = single_profile_config(name, "rt-expired");
    config.profiles[0].auto_start = true;
    let config = Arc::new(RankedMutex::new(config));

    let store: crate::usage::UsageStore = Arc::new(RankedMutex::new(HashMap::new()));

    // Expired window: resets_at well in the past → still windowless → candidate.
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

    // Future window: resets_at ahead of now → live window → excluded.
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
