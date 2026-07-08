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
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        bell_threshold: None,
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
        provider: None,
        third_party_usage: None,
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push(name.into());
    config
}

use crate::testutil::HomeSandbox;

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
fn live_session_excluded_even_with_force_true() {
    let _home = HomeSandbox::new();
    let name = "test-oauth-live-session-force";
    let sessions = profile_dir(name).expect("profile_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let pid_file = sessions.join("test-pid");
    let file = open_pid_file(&pid_file).expect("open pid file");
    file.lock().expect("lock pid file");

    let config = single_profile_config(name, "rt-jkl");
    let candidates = rotation_candidates(&config, true);
    assert!(
        candidates.is_empty(),
        "a live session owns its single-use chain — never rotated, even under force"
    );

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
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        bell_threshold: None,
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
        provider: None,
        third_party_usage: None,
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push("test-rotate-one-no-rt".into());

    let config = Arc::new(RankedMutex::new(config));
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let (tx, _rx) = mpsc::channel();

    let result = rotate_one_inner(&config, "test-rotate-one-no-rt", Some(&activity), &tx);

    assert!(
        matches!(result, RotateOutcome::Persisted(false)),
        "rotate_one_inner should return Persisted(false) when no refresh token"
    );
    assert!(
        is_idle(&activity, "test-rotate-one-no-rt"),
        "activity slot must remain Idle when rotation short-circuits at no-token"
    );
}

/// `rotate_one_inner` must never rotate a profile with a live `clauth start`
/// session: its single-use chain is owned by that session, so our stored token
/// is stale and a refresh would 400. It skips silently — `Persisted(false)`,
/// activity Idle, no `OpResult` (the single-rotate caller messages up front).
#[test]
fn rotate_one_inner_skips_live_session() {
    use std::collections::BTreeMap;
    use std::sync::mpsc;

    let _home = HomeSandbox::new();
    let name = "test-rotate-one-live-session";
    let sessions = profile_dir(name).expect("profile_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let pid_file = sessions.join("test-pid");
    let file = open_pid_file(&pid_file).expect("open pid file");
    file.lock().expect("lock pid file");

    let profile = Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        bell_threshold: None,
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
        provider: None,
        third_party_usage: None,
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push(name.into());

    let config = Arc::new(RankedMutex::new(config));
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let (tx, rx) = mpsc::channel();

    let result = rotate_one_inner(&config, name, Some(&activity), &tx);

    assert!(
        matches!(result, RotateOutcome::Persisted(false)),
        "a live session must skip rotation (no stale-token refresh)"
    );
    assert!(
        is_idle(&activity, name),
        "a skipped rotation must leave the activity slot Idle"
    );
    assert!(
        rx.try_recv().is_err(),
        "the silent live-session skip must not emit an OpResult"
    );

    drop(file);
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
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        bell_threshold: None,
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
        provider: None,
        third_party_usage: None,
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

// `auto_start_kick` opens a window on the steady-state fetch leg now, not via a
// candidate scan; its kick/rotation legs hit the network, so the window-lapsed
// gate that decides whether to kick is unit-tested in `scheduler.rs`
// (`window_lapsed`), and the opt-in gate is `Profile::auto_start` threaded into
// `TokenEntry`.

// `live_login_is_foreign` gates the rotation→Keychain mirror (rotation
// coherence, #1): the mirror must still fire when the live `.credentials.json`
// is merely a stale regular-file copy of OUR OWN pre-rotation pair (Claude
// Code's Keychain mirror, one step behind), and must NOT fire over a login
// clauth doesn't own (a real CC re-login into some other account).
#[cfg(target_os = "macos")]
mod keychain_mirror_gate {
    use crate::testutil::HomeSandbox;

    fn creds(access: &str) -> crate::profile::ClaudeCredentials {
        crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some(format!("{access}-refresh")),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }
    }

    fn save_profile_with(name: &str, access: &str) {
        let mut p = crate::profile::Profile::new(name.to_string(), None, None);
        p.credentials = Some(creds(access));
        crate::profile::save_profile(&p).expect("save profile");
    }

    fn write_live_file(access: &str) {
        let live = crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json");
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, serde_json::to_vec(&creds(access)).unwrap()).unwrap();
    }

    #[test]
    fn missing_live_file_is_not_foreign() {
        let _home = HomeSandbox::new();
        save_profile_with("alpha", "new-token");
        assert!(!super::live_login_is_foreign("alpha", "old-token"));
    }

    #[test]
    fn live_file_matching_stored_pair_is_not_foreign() {
        let _home = HomeSandbox::new();
        save_profile_with("alpha", "new-token");
        write_live_file("new-token");
        assert!(!super::live_login_is_foreign("alpha", "old-token"));
    }

    #[test]
    fn stale_mirror_of_own_pre_rotation_pair_is_not_foreign() {
        // The case the gate exists FOR: CC's regular-file mirror still holds the
        // pair this rotation just superseded. Diverged by classification, but it
        // is our own chain one step behind — the Keychain mirror must proceed.
        let _home = HomeSandbox::new();
        save_profile_with("alpha", "new-token");
        write_live_file("old-token");
        assert!(!super::live_login_is_foreign("alpha", "old-token"));
    }

    #[test]
    fn unrelated_live_login_is_foreign() {
        // A real CC re-login into some other account: matches neither the new
        // nor the pre-rotation pair — never overwrite it.
        let _home = HomeSandbox::new();
        save_profile_with("alpha", "new-token");
        write_live_file("someone-elses-token");
        assert!(super::live_login_is_foreign("alpha", "old-token"));
    }
}

// ── try_adopt_live_rotation (rotation coherence, the adopt-don't-race half) ──
//
// The running claude and clauth hold ONE single-use refresh family; when CC
// rotates first, its file mirror (~/.claude/.credentials.json) carries the
// fresher pair. Adopting it — identity-guarded — replaces racing for the
// chain. All offline: identity is injected, the "mirror" is a sandboxed file.
#[cfg(target_os = "macos")]
mod adopt_live_rotation {
    use super::*;
    use crate::lockorder::RankedMutex;
    use crate::testutil::HomeSandbox;
    use std::sync::Arc;

    fn creds_with(access: &str, expires_at: Option<i64>) -> crate::profile::ClaudeCredentials {
        crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some(format!("{access}-refresh")),
                expires_at,
                scopes: None,
                subscription_type: None,
            }),
        }
    }

    fn past_expiry() -> i64 {
        crate::usage::now_ms() as i64 - 60_000
    }

    fn future_expiry() -> i64 {
        crate::usage::now_ms() as i64 + 3_600_000
    }

    /// Active profile persisted to disk (classify reads the file layer) with a
    /// stored pair ("at-old"), plus a DIVERGED live regular file holding the
    /// mirror pair ("at-mirror").
    fn setup(name: &str, stored_expiry: i64, mirror_expiry: i64) -> crate::profile::ConfigHandle {
        let mut p = crate::profile::Profile::new(name.to_string(), None, None);
        p.credentials = Some(creds_with("at-old", Some(stored_expiry)));
        crate::profile::save_profile(&p).expect("save profile");
        let mut config = crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: vec![p],
        };
        config.state.profiles = vec![name.into()];
        config.state.active_profile = Some(name.into());
        let live = crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json");
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(
            &live,
            serde_json::to_vec(&creds_with("at-mirror", Some(mirror_expiry))).unwrap(),
        )
        .unwrap();
        Arc::new(RankedMutex::new(config))
    }

    fn stored_access(handle: &crate::profile::ConfigHandle, name: &str) -> String {
        handle
            .lock()
            .unwrap()
            .find(name)
            .and_then(|p| p.access_token().map(str::to_string))
            .expect("stored access token")
    }

    #[test]
    fn adopts_a_fresher_same_account_pair() {
        let _home = HomeSandbox::new();
        let name = "adopt-ok";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let adopted = try_adopt_live_rotation(&handle, name, &|_| Some("uuid-1".into()));
        // The adopted pair is returned so the caller syncs its TokenList —
        // without it, the next poll runs on the superseded entry.
        assert_eq!(
            adopted,
            Some(("at-mirror".into(), Some("at-mirror-refresh".into())))
        );
        assert_eq!(stored_access(&handle, name), "at-mirror");
        // The identity anchor is cached for future dead-store adopts.
        assert_eq!(
            crate::profile_cache::load_profile_cache::<String>(
                name,
                crate::profile_cache::ACCOUNT_ID_CACHE_FILE
            )
            .as_deref(),
            Some("uuid-1")
        );
    }

    #[test]
    fn refuses_a_live_login_from_a_different_account() {
        let _home = HomeSandbox::new();
        let name = "adopt-foreign";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        // Stored token answers uuid-1; the mirror token answers uuid-2 — a
        // manual CC /login into another account must never be captured.
        let adopted = try_adopt_live_rotation(&handle, name, &|tok| {
            Some(
                if tok == "at-mirror" {
                    "uuid-2"
                } else {
                    "uuid-1"
                }
                .into(),
            )
        });
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    #[test]
    fn refuses_without_an_identity_anchor() {
        let _home = HomeSandbox::new();
        let name = "adopt-anchorless";
        // Stored token already expired → its own uuid can't be fetched, and no
        // cached anchor exists. Identity unprovable ⇒ refuse.
        let handle = setup(name, past_expiry(), future_expiry());
        let adopted = try_adopt_live_rotation(&handle, name, &|tok| {
            (tok == "at-mirror").then(|| "uuid-1".into())
        });
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    #[test]
    fn cached_anchor_allows_adopt_even_with_a_dead_stored_token() {
        let _home = HomeSandbox::new();
        let name = "adopt-cached-anchor";
        let handle = setup(name, past_expiry(), future_expiry());
        crate::profile_cache::write_profile_cache(
            name,
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            &"uuid-1".to_string(),
        );
        let adopted = try_adopt_live_rotation(&handle, name, &|tok| {
            (tok == "at-mirror").then(|| "uuid-1".into())
        });
        assert!(adopted.is_some());
        assert_eq!(stored_access(&handle, name), "at-mirror");
    }

    #[test]
    fn refuses_a_stale_or_equal_mirror() {
        let _home = HomeSandbox::new();
        let name = "adopt-stale";
        // Mirror expiry equals the store's — nothing fresher to adopt.
        let expiry = future_expiry();
        let handle = setup(name, expiry, expiry);
        let adopted = try_adopt_live_rotation(&handle, name, &|_| Some("uuid-1".into()));
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    #[test]
    fn refuses_when_not_the_active_profile() {
        let _home = HomeSandbox::new();
        let name = "adopt-inactive";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        handle.lock().unwrap().state.active_profile = None;
        let adopted = try_adopt_live_rotation(&handle, name, &|_| Some("uuid-1".into()));
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    #[test]
    fn refuses_a_blank_identity() {
        // A present-but-blank uuid is shape drift, not an identity — two
        // blanks matching each other must never prove the tokens are the same
        // account.
        let _home = HomeSandbox::new();
        let name = "adopt-blank-id";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let adopted = try_adopt_live_rotation(&handle, name, &|_| Some("  ".into()));
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }
}
