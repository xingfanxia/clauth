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

// ── AUTH-1: pre-install auth gate (`ensure_installable`) ──────────────────────
//
// All offline: the HTTP refresh is injected as a closure, so these pin the
// gate's decision + persistence without touching the network or the real
// Keychain (Incident C guardrail).

/// Epoch-ms already in the past — an expired access token.
fn past_expiry() -> i64 {
    crate::usage::now_ms() as i64 - 60_000
}

/// Epoch-ms an hour ahead — a token with real life left.
fn future_expiry() -> i64 {
    crate::usage::now_ms() as i64 + 3_600_000
}

fn oauth_config(name: &str, refresh_token: Option<&str>, expires_at: Option<i64>) -> AppConfig {
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
                access_token: "at-old".to_string(),
                refresh_token: refresh_token.map(String::from),
                expires_at,
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

fn third_party_config(name: &str) -> AppConfig {
    use std::collections::BTreeMap;
    let profile = Profile {
        name: name.into(),
        base_url: Some("https://api.deepseek.com/anthropic".to_string()),
        api_key: Some("sk-fixture".to_string()),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        bell_threshold: None,
        credentials: None,
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

/// A refresher that must never run — bypass/valid-token paths take no refresh.
fn never_refresh(_rt: &str) -> std::result::Result<TokenResponse, RefreshError> {
    panic!("ensure_installable must not refresh in this scenario");
}

/// Third-party (api-key) profile → gate bypassed, no refresh attempted.
#[test]
fn gate_third_party_bypasses() {
    let _home = HomeSandbox::new();
    let name = "test-gate-third-party";
    let handle = Arc::new(RankedMutex::new(third_party_config(name)));
    assert!(matches!(
        ensure_installable(&handle, name, never_refresh),
        AuthGate::Ready
    ));
}

/// OAuth token with real life left → install as-is, no refresh.
#[test]
fn gate_valid_token_ready_without_refresh() {
    let _home = HomeSandbox::new();
    let name = "test-gate-valid";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-good"),
        Some(future_expiry()),
    )));
    assert!(matches!(
        ensure_installable(&handle, name, never_refresh),
        AuthGate::Ready
    ));
}

/// Expired-but-refreshable → rotated tokens minted, persisted, installed.
#[test]
fn gate_refreshes_expiring_token_and_installs() {
    let _home = HomeSandbox::new();
    let name = "test-gate-refreshable";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-old"),
        Some(past_expiry()),
    )));
    let refresher = |_rt: &str| {
        Ok(TokenResponse {
            access_token: "at-new".to_string(),
            refresh_token: "rt-new".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    assert!(matches!(
        ensure_installable(&handle, name, refresher),
        AuthGate::Refreshed
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    let p = cfg.find(name).expect("profile");
    assert_eq!(
        p.access_token(),
        Some("at-new"),
        "rotated access token stored"
    );
    assert_eq!(
        p.refresh_token(),
        Some("rt-new"),
        "rotated refresh token stored"
    );
    assert!(
        !cfg.is_auth_broken(name),
        "a successful refresh is not broken"
    );
}

/// Refresh rejected as invalid → switch refused + profile quarantined.
#[test]
fn gate_invalid_refresh_marks_broken_and_refuses() {
    let _home = HomeSandbox::new();
    let name = "test-gate-invalid";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-revoked"),
        Some(past_expiry()),
    )));
    let refresher = |_rt: &str| Err(RefreshError::Invalid("HTTP 400: invalid_grant".to_string()));
    assert!(matches!(
        ensure_installable(&handle, name, refresher),
        AuthGate::Broken
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        cfg.is_auth_broken(name),
        "a revoked refresh token quarantines the profile"
    );
}

/// A transient failure refuses the switch but does NOT quarantine the account.
#[test]
fn gate_transient_refresh_does_not_quarantine() {
    let _home = HomeSandbox::new();
    let name = "test-gate-transient";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-ok"),
        Some(past_expiry()),
    )));
    let refresher = |_rt: &str| Err(RefreshError::Transient(anyhow::anyhow!("connection reset")));
    assert!(matches!(
        ensure_installable(&handle, name, refresher),
        AuthGate::Transient(_)
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        !cfg.is_auth_broken(name),
        "a network blip must not quarantine a healthy account"
    );
}

/// An expiring OAuth token with no refresh token is unrecoverable → quarantined.
#[test]
fn gate_expiring_without_refresh_token_is_broken() {
    let _home = HomeSandbox::new();
    let name = "test-gate-no-rt";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        None,
        Some(past_expiry()),
    )));
    assert!(matches!(
        ensure_installable(&handle, name, never_refresh),
        AuthGate::Broken
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(cfg.is_auth_broken(name));
}

/// A standing quarantine overrides a still-valid clock: the chain's last
/// refresh terminally failed, so a future `expires_at` proves nothing
/// (server-side revocation outlives the stored clock). The gate must route a
/// flagged profile through the refresher instead of installing the dead token
/// as `Ready` — the hole that let CLI/MCP disagree with the TUI's flag-only
/// refusal. A recovered chain (external re-login) lifts the flag on the way
/// through.
#[test]
fn gate_flagged_profile_refreshes_despite_a_valid_clock() {
    let _home = HomeSandbox::new();
    let name = "test-gate-flagged-recovers";
    let mut config = oauth_config(name, Some("rt-relogin"), Some(future_expiry()));
    config.set_auth_broken(name, true);
    let handle = Arc::new(RankedMutex::new(config));
    let refresher = |_rt: &str| {
        Ok(TokenResponse {
            access_token: "at-recovered".to_string(),
            refresh_token: "rt-recovered".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    assert!(matches!(
        ensure_installable(&handle, name, refresher),
        AuthGate::Refreshed
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        !cfg.is_auth_broken(name),
        "a recovered chain lifts the quarantine on the way through the gate"
    );
    let p = cfg.find(name).unwrap_or_else(|| panic!("profile"));
    assert_eq!(p.access_token(), Some("at-recovered"));
}

/// Same flagged shape with a genuinely dead chain: the gate confirms `Broken`
/// (refusal + login hint), never a silent `Ready` install of the revoked pair.
#[test]
fn gate_flagged_profile_with_a_dead_chain_stays_broken() {
    let _home = HomeSandbox::new();
    let name = "test-gate-flagged-dead";
    let mut config = oauth_config(name, Some("rt-revoked"), Some(future_expiry()));
    config.set_auth_broken(name, true);
    let handle = Arc::new(RankedMutex::new(config));
    let refresher = |_rt: &str| Err(RefreshError::Invalid("HTTP 400: invalid_grant".to_string()));
    assert!(matches!(
        ensure_installable(&handle, name, refresher),
        AuthGate::Broken
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        cfg.is_auth_broken(name),
        "a dead chain keeps the quarantine"
    );
}

/// A 2xx token-endpoint body that fails to deserialize still holds the live
/// access+refresh tokens, so `token_parse_error` must surface only the serde
/// error + HTTP status + body length — never the token values.
#[test]
fn token_parse_error_redacts_the_2xx_body() {
    // Missing `expires_in` → fails to parse into TokenResponse, but the body
    // carries live-looking tokens.
    let body =
        r#"{"access_token":"sk-ant-oat01-SECRETLEAK","refresh_token":"sk-ant-ort01-SECRETLEAK"}"#;
    // Avoid `.expect_err` so `TokenResponse` need not derive `Debug` — a token-
    // bearing struct with a `Debug` impl is its own leak surface.
    let err = match serde_json::from_str::<TokenResponse>(body) {
        Ok(_) => panic!("2xx body without expires_in must fail to parse into TokenResponse"),
        Err(e) => e,
    };
    let msg = super::token_parse_error(err, 200, body.len()).to_string();

    assert!(
        !msg.contains("SECRETLEAK"),
        "no token value substring may appear in the error: {msg}"
    );
    assert!(
        !msg.contains("access_token\":\""),
        "raw body must not be echoed: {msg}"
    );
    assert!(msg.contains("200"), "HTTP status is reported: {msg}");
    assert!(
        msg.contains(&body.len().to_string()),
        "body length is reported: {msg}"
    );
    // Locks the value-free channel: the message reports the failure *position*,
    // never the serde Display `{e}` (which could echo an offending scalar).
    assert!(
        msg.contains("column"),
        "the parse position (not the serde value) is reported: {msg}"
    );
}

// ── refresh_rejection_is_terminal (the 400/401/403 truth table) ─────────────

/// 400/401 are terminal regardless of body (OAuth2 reports a dead refresh
/// token as 400 `invalid_grant`; some proxies answer 401). 403 is terminal
/// ONLY with a confirming `invalid_grant` body — WAF/geo/challenge blocks
/// answer 403 too, and quarantining on one would take a healthy account out
/// of rotation until its next successful refresh.
#[test]
fn refresh_rejection_terminal_truth_table() {
    assert!(refresh_rejection_is_terminal(400, "invalid_grant"));
    assert!(refresh_rejection_is_terminal(
        400,
        "refresh token not found or invalid"
    ));
    assert!(refresh_rejection_is_terminal(401, "unauthorized"));
    assert!(refresh_rejection_is_terminal(
        403,
        r#"{"error":"invalid_grant"}"#
    ));
    assert!(!refresh_rejection_is_terminal(
        403,
        "<html>Access denied by security policy</html>"
    ));
    assert!(!refresh_rejection_is_terminal(429, "rate limited"));
    assert!(!refresh_rejection_is_terminal(500, "internal error"));
}

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
// NOT macOS-gated: only the production call site's `keychain_live()` gate is
// platform-specific — the identity gate and the expiry-monotonicity re-check
// must hold (and run in CI) on every OS.
mod adopt_live_rotation {
    use super::*;
    use crate::lockorder::RankedMutex;
    use crate::testutil::HomeSandbox;
    use std::sync::Arc;

    /// The per-profile rotation lock `try_adopt_live_rotation` demands proof
    /// of (production callers hold it across the whole rotation leg).
    fn guard(name: &str) -> crate::runtime::RotationGuard {
        crate::runtime::RotationGuard::acquire(name).expect("rotation guard")
    }

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
        let adopted =
            try_adopt_live_rotation(&handle, name, &guard(name), &|_| Some("uuid-1".into()));
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
        let adopted = try_adopt_live_rotation(&handle, name, &guard(name), &|tok| {
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
        let adopted = try_adopt_live_rotation(&handle, name, &guard(name), &|tok| {
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
        let adopted = try_adopt_live_rotation(&handle, name, &guard(name), &|tok| {
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
        let adopted =
            try_adopt_live_rotation(&handle, name, &guard(name), &|_| Some("uuid-1".into()));
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    #[test]
    fn refuses_when_not_the_active_profile() {
        let _home = HomeSandbox::new();
        let name = "adopt-inactive";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        handle.lock().unwrap().state.active_profile = None;
        let adopted =
            try_adopt_live_rotation(&handle, name, &guard(name), &|_| Some("uuid-1".into()));
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
        let adopted = try_adopt_live_rotation(&handle, name, &guard(name), &|_| Some("  ".into()));
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    /// An adopted pair proves the chain is alive, so a standing `auth_broken`
    /// is stale — same lift as the scheduler's `carry_external_rotation`.
    /// Without it, an active recovered by a CC-side re-login stays excluded
    /// from the fallback walk and refused as a switch target until a manual
    /// `clauth login` (the cross-PR seam the adopt PR deferred to the rebase).
    #[test]
    fn adopting_a_live_rotation_lifts_a_stale_quarantine() {
        let _home = HomeSandbox::new();
        let name = "adopt-quarantined";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        handle.lock().unwrap().set_auth_broken(name, true);
        let adopted =
            try_adopt_live_rotation(&handle, name, &guard(name), &|_| Some("uuid-1".into()));
        assert!(adopted.is_some(), "the fresher same-account pair adopts");
        assert!(
            !handle.lock().unwrap().is_auth_broken(name),
            "an adopted (alive) chain lifts a stale quarantine"
        );
    }
}

// ── post-guard re-read (the pre-RotationGuard token-snapshot race) ────────────
//
// Between the guard-less pre-check and RotationGuard acquisition a sibling
// rotation can spend the single-use refresh token and persist a new pair;
// refreshing from a pre-guard snapshot would 400 and wrongly quarantine a
// healthy login. `gate_under_guard` therefore takes NO token arguments — its
// decisions can only come from state read under the guard. These pin that
// boundary directly.

/// The rotation lock the guard leg demands proof of (production callers hold
/// it across the whole refresh window).
fn gate_guard(name: &str) -> crate::runtime::RotationGuard {
    crate::runtime::RotationGuard::acquire(name).expect("rotation guard")
}

/// Persist a peer's rotation to the on-disk profile store — the state a
/// cross-process rotation leaves behind for `adopt_disk_rotation` to find.
fn save_disk_profile(name: &str, refresh: &str, expires_at: Option<i64>) {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-disk".to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at,
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&p).expect("save disk profile");
}

/// Stored pair already fresh when the guard leg runs (the sibling-refreshed
/// interleave) → Ready, and the old chain is NOT double-spent (the refresher
/// panics if called).
#[test]
fn gate_under_guard_installs_a_sibling_refreshed_pair_as_is() {
    let _home = HomeSandbox::new();
    let name = "test-gate-sibling-refreshed";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-fresh"),
        Some(future_expiry()),
    )));
    assert!(matches!(
        gate_under_guard(&handle, name, never_refresh, &gate_guard(name)),
        AuthGate::Ready
    ));
}

/// Still expiring under the guard → the refresher is fed the CURRENTLY stored
/// refresh token, never a caller-supplied snapshot.
#[test]
fn gate_under_guard_spends_the_currently_stored_refresh_token() {
    let _home = HomeSandbox::new();
    let name = "test-gate-current-rt";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-current"),
        Some(past_expiry()),
    )));
    let refresher = |rt: &str| {
        assert_eq!(
            rt, "rt-current",
            "must spend the token read under the guard"
        );
        Ok(TokenResponse {
            access_token: "at-new".to_string(),
            refresh_token: "rt-next".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    assert!(matches!(
        gate_under_guard(&handle, name, refresher, &gate_guard(name)),
        AuthGate::Refreshed
    ));
}

/// A cross-process peer rotated and persisted while this process held a stale
/// in-memory config snapshot (the CLI and MCP load config from disk once and
/// never reload): under the guard the DISK pair is authoritative. A live disk
/// pair installs as-is — the stale in-memory token is never spent (the
/// refresher panics if called) — and the handle carries the adopted pair.
#[test]
fn gate_under_guard_adopts_a_cross_process_rotation_from_disk() {
    let _home = HomeSandbox::new();
    let name = "test-gate-disk-adopt";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-stale"),
        Some(past_expiry()),
    )));
    save_disk_profile(name, "rt-peer", Some(future_expiry()));
    assert!(matches!(
        gate_under_guard(&handle, name, never_refresh, &gate_guard(name)),
        AuthGate::Ready
    ));
    assert_eq!(
        handle.lock().unwrap().find(name).unwrap().refresh_token(),
        Some("rt-peer"),
        "the adopted disk pair must replace the stale in-memory snapshot"
    );
}

/// Peer-rotated pair that is ITSELF already expiring again: the refresher must
/// be fed the disk refresh token — spending the stale in-memory one would 400
/// (already spent by the peer) and wrongly quarantine a healthy login.
#[test]
fn gate_under_guard_spends_the_disk_pair_after_an_external_rotation() {
    let _home = HomeSandbox::new();
    let name = "test-gate-disk-spend";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-stale"),
        Some(past_expiry()),
    )));
    save_disk_profile(name, "rt-peer", Some(past_expiry()));
    let refresher = |rt: &str| {
        assert_eq!(rt, "rt-peer", "must spend the disk pair, not the snapshot");
        Ok(TokenResponse {
            access_token: "at-new".to_string(),
            refresh_token: "rt-next".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    assert!(matches!(
        gate_under_guard(&handle, name, refresher, &gate_guard(name)),
        AuthGate::Refreshed
    ));
}

/// A differing disk pair proves the chain is alive, so a standing in-memory
/// quarantine is stale and lifts (same rationale as the scheduler's
/// `carry_external_rotation`): the gate proceeds from the adopted pair
/// instead of refusing a recovered login.
#[test]
fn gate_under_guard_disk_adoption_lifts_a_stale_quarantine() {
    let _home = HomeSandbox::new();
    let name = "test-gate-disk-quarantine";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-stale"),
        Some(future_expiry()),
    )));
    handle.lock().unwrap().set_auth_broken(name, true);
    save_disk_profile(name, "rt-peer", Some(future_expiry()));
    assert!(matches!(
        gate_under_guard(&handle, name, never_refresh, &gate_guard(name)),
        AuthGate::Ready
    ));
    assert!(
        !handle.lock().unwrap().is_auth_broken(name),
        "an adopted (alive) chain lifts a stale quarantine"
    );
}
