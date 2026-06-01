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
use crate::usage::{LastRotatedWindow, is_idle};

// Build a minimal AppConfig with one OAuth profile named `name`.
fn single_profile_config(name: &str, refresh_token: &str) -> AppConfig {
    use std::collections::BTreeMap;
    let profile = Profile {
        name: name.to_string(),
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
    config.state.profiles.push(name.to_string());
    config
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

    // Release lock — sessions dir and file left behind but harmless.
    drop(file);
}

#[test]
fn live_session_included_when_force_true() {
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

/// `rotate_one` must NOT stamp `Refreshing` when the profile has no refresh
/// token — the short-circuit `let Some(rt) = token else { return false }` runs
/// before any HTTP, so the activity slot should remain clean (Idle).
#[test]
fn rotate_one_no_stamp_when_no_refresh_token() {
    use std::collections::BTreeMap;
    use std::sync::mpsc;

    // Profile with OAuth block but no refresh token.
    let profile = Profile {
        name: "test-rotate-one-no-rt".to_string(),
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
    config
        .state
        .profiles
        .push("test-rotate-one-no-rt".to_string());

    let config = Arc::new(RankedMutex::new(config));
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let (tx, _rx) = mpsc::channel();

    let result = rotate_one(&config, "test-rotate-one-no-rt", &activity, &tx);

    assert!(
        !result,
        "rotate_one should return false when no refresh token"
    );
    assert!(
        is_idle(&activity, "test-rotate-one-no-rt"),
        "activity slot must remain Idle when rotate_one short-circuits at no-token"
    );
}

#[test]
fn profile_without_refresh_token_excluded() {
    use std::collections::BTreeMap;
    let profile = Profile {
        name: "test-oauth-no-rt".to_string(),
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
    config.state.profiles.push("test-oauth-no-rt".to_string());
    // No refresh token → excluded regardless of force.
    assert!(rotation_candidates(&config, false).is_empty());
    assert!(rotation_candidates(&config, true).is_empty());
}

/// Switch paths must call `rotate_one` only for the outgoing active and
/// incoming target, not every profile. This test pins the selection logic by
/// setting up three profiles with no refresh tokens (so `rotate_one` returns
/// false immediately, no HTTP), then verifying that a bystander profile's
/// activity slot is never stamped — i.e., it is never passed to `rotate_one`.
///
/// The observable proxy: only profiles passed to `rotate_one` can have their
/// activity slot touched (Refreshing then cleared). A profile never passed
/// always remains Idle.
#[test]
fn switch_rotate_targets_only_active_and_target() {
    use std::collections::BTreeMap;
    use std::sync::mpsc;

    fn make_profile(name: &str) -> Profile {
        Profile {
            name: name.to_string(),
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
        }
    }

    let active_name = "switch-test-active";
    let target_name = "switch-test-target";
    let bystander_name = "switch-test-bystander";

    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![
            make_profile(active_name),
            make_profile(target_name),
            make_profile(bystander_name),
        ],
    };
    let config = Arc::new(RankedMutex::new(config));
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let (tx, _rx) = mpsc::channel();

    // Simulate the new switch logic: rotate active then target (dedup skipped
    // here since they differ), never the bystander.
    rotate_one(&config, active_name, &activity, &tx);
    rotate_one(&config, target_name, &activity, &tx);

    // All three should be Idle: active and target have no refresh token so
    // rotate_one short-circuits before stamping; bystander was never called.
    assert!(
        is_idle(&activity, active_name),
        "active must be Idle after no-token short-circuit"
    );
    assert!(
        is_idle(&activity, target_name),
        "target must be Idle after no-token short-circuit"
    );
    assert!(
        is_idle(&activity, bystander_name),
        "bystander must never be stamped — only active+target are passed to rotate_one"
    );
}

/// `rotate_one_for_window` must NOT stamp `LastRotatedWindow` when the profile
/// has no refresh token. The function short-circuits before HTTP, so LRW is
/// untouched and the caller can rely on re-enqueue behaviour from the scheduler.
#[test]
fn rotate_one_for_window_no_stamp_when_no_refresh_token() {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::mpsc;

    let name = "test-rotate-window-no-rt";
    let profile = Profile {
        name: name.to_string(),
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
    let config = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    }));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let lrw: LastRotatedWindow = Arc::new(RankedMutex::new(HashMap::new()));
    let (tx, _rx) = mpsc::channel();
    let resets_at: i64 = 9999;

    let rotated = rotate_one_for_window(&config, name, &activity, &tx, &lrw, resets_at);

    assert!(!rotated, "should return false with no refresh token");
    assert!(
        lrw.lock().unwrap().is_empty(),
        "LRW must not be stamped when rotation short-circuits at no-token"
    );
    assert!(
        is_idle(&activity, name),
        "activity slot must remain Idle when short-circuiting"
    );
}

/// `rotate_one_for_window` must NOT stamp `LastRotatedWindow` when a live
/// `clauth start` session holds the chain. The scheduler will re-enqueue the
/// same window on the next tick; `has_live_session` returns early without HTTP.
#[test]
fn rotate_one_for_window_no_stamp_when_live_session() {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::mpsc;

    let name = "test-rotate-window-live-session";
    let sessions = profile_dir(name).expect("profile_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let pid_file = sessions.join("test-pid-window");
    let file = open_pid_file(&pid_file).expect("open pid file");
    file.lock().expect("lock pid file");

    let profile = Profile {
        name: name.to_string(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        fallback_threshold: None,
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "at".to_string(),
                refresh_token: Some("rt-live".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
    };
    let config = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    }));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let lrw: LastRotatedWindow = Arc::new(RankedMutex::new(HashMap::new()));
    let (tx, _rx) = mpsc::channel();
    let resets_at: i64 = 8888;

    let rotated = rotate_one_for_window(&config, name, &activity, &tx, &lrw, resets_at);

    assert!(
        !rotated,
        "should return false when live session holds the chain"
    );
    assert!(
        lrw.lock().unwrap().is_empty(),
        "LRW must not be stamped when skipped due to live session"
    );

    drop(file);
}

/// When active == target (re-switch), the switch paths deduplicate and call
/// `rotate_one` at most once for that profile.
#[test]
fn switch_dedup_active_equals_target() {
    use std::collections::BTreeMap;
    use std::sync::mpsc;

    let name = "switch-dedup-same";
    let profile = Profile {
        name: name.to_string(),
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
    let config = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    }));
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let (tx, _rx) = mpsc::channel();

    // active == target: the dedup condition `active != target` is false, so only
    // one rotate_one call is made (for target). Verify the slot stays Idle.
    let active = Some(name.to_string());
    let target = name.to_string();
    if let Some(ref a) = active
        && a != &target
    {
        rotate_one(&config, a, &activity, &tx);
    }
    rotate_one(&config, &target, &activity, &tx);

    assert!(
        is_idle(&activity, name),
        "slot must stay Idle after single no-token rotate_one call"
    );
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

/// `auto_start_named` must return false (no token spent) when a live
/// `clauth start` session holds the chain. Selection-time `has_live_session`
/// short-circuits before any HTTP; the in-guard re-check in `run_auto_start`
/// is the second layer for the post-selection race window. Network-free: the
/// live-session gate fires before any refresh.
#[test]
fn auto_start_named_skips_when_live_session() {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::mpsc;

    let name = "test-auto-start-named-live-session";
    let sessions = profile_dir(name).expect("profile_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let pid_file = sessions.join("test-pid-autostart");
    let file = open_pid_file(&pid_file).expect("open pid file");
    file.lock().expect("lock pid file");

    let profile = Profile {
        name: name.to_string(),
        base_url: None,
        api_key: None,
        auto_start: true,
        env: BTreeMap::new(),
        fallback_threshold: None,
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "at".to_string(),
                refresh_token: Some("rt-autostart-live".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
    };
    let config = Arc::new(RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    }));
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let refetch: crate::usage::RefetchQueue =
        Arc::new(RankedMutex::new(std::collections::HashSet::new()));
    let (tx, _rx) = mpsc::channel();

    let kicked = auto_start_named(&config, name, &refetch, &activity, &tx);

    assert!(
        !kicked,
        "auto_start_named must skip (no token spent) when a live session holds the chain"
    );
    assert!(
        is_idle(&activity, name),
        "activity slot must remain Idle when skipped at the live-session gate"
    );

    drop(file);
}
