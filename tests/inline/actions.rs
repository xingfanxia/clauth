#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Per-account custom env editor: collision classification + `edit_profile_env`
//! persistence and the strip-removed-keys-on-active behaviour.

use super::*;
use crate::profile::AppState;
use crate::testutil::HomeSandbox;

fn acct_config() -> AppConfig {
    AppConfig {
        state: AppState::default(),
        profiles: vec![Profile::new("acct".to_string(), None, None)],
    }
}

#[test]
fn classify_env_key_flags_managed_keys() {
    let p = Profile::new("acct".to_string(), None, None);
    assert!(matches!(
        classify_env_key(&p, &[], "ANTHROPIC_BASE_URL"),
        Some(EnvKeyCollision::Managed(_))
    ));
    assert!(matches!(
        classify_env_key(&p, &[], "CLAUDE_CODE_SUBAGENT_MODEL"),
        Some(EnvKeyCollision::Managed(_))
    ));
    assert_eq!(classify_env_key(&p, &[], "ANTHROPIC_CUSTOM_FLAG"), None);
}

#[test]
fn classify_env_key_flags_own_field_by_sorted_index() {
    let mut p = Profile::new("acct".to_string(), None, None);
    p.env.insert("ZED".to_string(), "1".to_string());
    p.env.insert("ALPHA".to_string(), "2".to_string());
    // BTreeMap order: ALPHA(0), ZED(1).
    assert_eq!(
        classify_env_key(&p, &[], "ALPHA"),
        Some(EnvKeyCollision::ProfileField(0))
    );
    assert_eq!(
        classify_env_key(&p, &[], "ZED"),
        Some(EnvKeyCollision::ProfileField(1))
    );
}

#[test]
fn classify_env_key_base_settings_only_for_external_keys() {
    let mut p = Profile::new("acct".to_string(), None, None);
    p.env.insert("OWN".to_string(), "1".to_string());
    let base = vec![
        "OWN".to_string(),
        "EXTERNAL".to_string(),
        "ANTHROPIC_BASE_URL".to_string(),
    ];
    // Managed + own-field checks win before the base check, so only a key that is
    // neither (genuinely external) classifies as BaseSettings.
    assert_eq!(
        classify_env_key(&p, &base, "EXTERNAL"),
        Some(EnvKeyCollision::BaseSettings)
    );
    assert_eq!(
        classify_env_key(&p, &base, "OWN"),
        Some(EnvKeyCollision::ProfileField(0))
    );
    assert!(matches!(
        classify_env_key(&p, &base, "ANTHROPIC_BASE_URL"),
        Some(EnvKeyCollision::Managed(_))
    ));
    assert_eq!(classify_env_key(&p, &base, "FRESH"), None);
}

/// macOS reality: `~/.claude/.credentials.json` is a regular-file Keychain mirror
/// of the ACTIVE account (not clauth's symlink). Switching to another profile must
/// succeed — the live file matches the active profile (already captured), so it is
/// safe to replace even though it legitimately differs from the target. Regression
/// for `Error: refusing to replace .credentials.json — live file differs from
/// profile 'xfx'; resolve divergence first` on every `clauth <name>`.
#[test]
fn switch_replaces_active_account_mirror_without_refusing() {
    let _home = HomeSandbox::new();

    let mk = |name: &str, access: &str| {
        let mut p = Profile::new(name.to_string(), None, None);
        p.credentials = Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some(format!("{access}-refresh")),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        });
        crate::profile::save_profile(&p).expect("save profile");
        p
    };
    let active = mk("cl-ax", "cl-ax-access");
    let target = mk("xfx", "xfx-access");

    // Live file = a plain regular file whose content matches the ACTIVE profile
    // (exactly what Claude Code mirrors from the Keychain on macOS).
    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
    std::fs::write(
        &live_path,
        serde_json::to_vec(active.credentials.as_ref().unwrap()).unwrap(),
    )
    .unwrap();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![active, target],
    };
    config.state.active_profile = Some("cl-ax".into());

    // Must NOT bail — the live file is the active account's captured mirror.
    switch_profile(&mut config, "xfx").expect("switch replaces the active-account mirror");

    assert!(config.is_active("xfx"));
    assert_eq!(
        classify_credentials_link("xfx").expect("classify"),
        LinkState::LinkedTo,
        "after the switch the live path resolves to xfx's stored creds",
    );
}

#[test]
fn edit_profile_env_persists_to_config_toml() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    let mut env = BTreeMap::new();
    env.insert("FOO".to_string(), "bar".to_string());
    edit_profile_env(&mut config, "acct", env).expect("set env");

    assert_eq!(
        config.find("acct").unwrap().env.get("FOO"),
        Some(&"bar".to_string())
    );
    let toml = std::fs::read_to_string(profile_dir("acct").unwrap().join("config.toml"))
        .expect("config.toml written");
    assert!(
        toml.contains("FOO"),
        "custom env key persisted to config.toml"
    );

    // Clearing the map persists too.
    edit_profile_env(&mut config, "acct", BTreeMap::new()).expect("clear env");
    assert!(config.find("acct").unwrap().env.is_empty());
}

#[test]
fn edit_profile_env_strips_removed_keys_from_live_settings_when_active() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    config.state.active_profile = Some("acct".into());

    let mut env = BTreeMap::new();
    env.insert("KEEP".to_string(), "1".to_string());
    env.insert("DROP".to_string(), "2".to_string());
    edit_profile_env(&mut config, "acct", env).expect("write both");
    let live = crate::claude::claude_settings_env_keys().expect("read settings");
    assert!(live.contains(&"KEEP".to_string()) && live.contains(&"DROP".to_string()));

    // Removing DROP must strip it from the live settings.json, not leak it.
    let mut env2 = BTreeMap::new();
    env2.insert("KEEP".to_string(), "1".to_string());
    edit_profile_env(&mut config, "acct", env2).expect("drop one");
    let live = crate::claude::claude_settings_env_keys().expect("read settings");
    assert!(live.contains(&"KEEP".to_string()));
    assert!(
        !live.contains(&"DROP".to_string()),
        "a removed key is stripped from the live settings on re-apply"
    );
}

// ── ensure_login_profile ──────────────────────────────────────────────────────

#[test]
fn ensure_login_creates_blank_profile() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    assert!(ensure_login_profile(&mut config, "fresh").expect("create"));
    let profile = config.find("fresh").expect("profile added");
    assert!(profile.credentials.is_none(), "created blank");
}

#[test]
fn ensure_login_reuses_existing_profile() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    assert!(!ensure_login_profile(&mut config, "acct").expect("reuse"));
    assert_eq!(config.profiles.len(), 1, "no duplicate created");
}

#[test]
fn ensure_login_rejects_invalid_name() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    assert!(ensure_login_profile(&mut config, ".hidden").is_err());
    assert!(ensure_login_profile(&mut config, "").is_err());
}

#[test]
fn validate_profile_name_accepts_email_rejects_path_chars() {
    for name in [
        "claude@domain.com",
        "user2@domain.com",
        "claude+work@gmail.com",
    ] {
        assert!(
            validate_profile_name(name, &[], None).is_ok(),
            "{name} rejected"
        );
    }
    // path separators / windows-reserved chars stay blocked so the name can't
    // escape its profiles/<name> directory segment.
    for name in ["a/b", "a\\b", "a:b", ".lead", "a b"] {
        assert!(
            validate_profile_name(name, &[], None).is_err(),
            "{name} accepted"
        );
    }
}

// ── capture-name collision overwrite (issue #7) ────────────────────────────

/// Overwriting an existing profile on a capture-name collision must mutate it
/// in place: chain position, env, model/fallback config, and auto_start
/// survive; only credentials/base_url/api_key change; usage_history.jsonl
/// (a persisted log, not a cache) is untouched; the stale per-account fetch
/// caches are dropped since they now describe the wrong credentials.
#[test]
fn overwrite_captured_profile_keeps_config_and_history_swaps_credentials() {
    let _home = HomeSandbox::new();

    // "acme" sits in the MIDDLE of a 3-profile chain — a blind delete+append
    // would move it to the end, so this actually proves position survives an
    // in-place mutation rather than merely proving membership.
    let first = Profile::new("first".to_string(), None, None);
    save_profile(&first).expect("save first");
    let last = Profile::new("last".to_string(), None, None);
    save_profile(&last).expect("save last");

    let mut target = Profile::new("acme".to_string(), None, None);
    target.auto_start = true;
    target.env.insert("FOO".to_string(), "bar".to_string());
    target.fallback_threshold = Some(42.0);
    target.bell_threshold = Some(77.0);
    target.models.opus = Some("claude-opus-4".to_string());
    target.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&target).expect("save target");

    let history_path = profile_dir("acme").unwrap().join("usage_history.jsonl");
    std::fs::write(&history_path, b"{\"ts\":1}\n").expect("seed usage history");

    // Seed the transient fetch-state caches the overwrite must drop.
    for file in [
        crate::profile_cache::USAGE_CACHE_FILE,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        crate::throughput::THROUGHPUT_CACHE_FILE,
    ] {
        crate::profile_cache::write_profile_cache("acme", file, &"stale");
    }

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["first".into(), "acme".into(), "last".into()],
            fallback_chain: vec!["first".into(), "acme".into(), "last".into()],
            active_profile: Some("first".into()),
            ..AppState::default()
        },
        profiles: vec![first, target, last],
    };

    let snapshot = CaptureSnapshot {
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "new-access".to_string(),
                refresh_token: Some("new-refresh".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        base_url: Some("https://api.example.com".to_string()),
        api_key: Some("new-api-key".to_string()),
    };

    overwrite_captured_profile(&mut config, "acme", snapshot).expect("overwrite in place");

    assert_eq!(
        config.profiles.len(),
        3,
        "no duplicate entry from a blind append"
    );
    let acme = config
        .find("acme")
        .expect("profile still present under the same name");
    assert_eq!(
        acme.access_token(),
        Some("new-access"),
        "credentials replaced"
    );
    assert_eq!(
        acme.base_url.as_deref(),
        Some("https://api.example.com"),
        "base_url replaced"
    );
    assert_eq!(
        acme.api_key.as_deref(),
        Some("new-api-key"),
        "api_key replaced"
    );
    assert!(acme.auto_start, "auto_start config preserved");
    assert_eq!(
        acme.env.get("FOO"),
        Some(&"bar".to_string()),
        "env map preserved"
    );
    assert_eq!(
        acme.fallback_threshold,
        Some(42.0),
        "fallback_threshold preserved"
    );
    assert_eq!(acme.bell_threshold, Some(77.0), "bell_threshold preserved");
    assert_eq!(
        acme.models.opus.as_deref(),
        Some("claude-opus-4"),
        "model settings preserved"
    );
    assert!(
        acme.usage.is_none() && acme.fetch_status.is_none() && acme.third_party_usage.is_none(),
        "transient fetch state cleared"
    );

    assert_eq!(
        config.state.fallback_chain,
        vec![
            crate::profile::ProfileName::from("first"),
            crate::profile::ProfileName::from("acme"),
            crate::profile::ProfileName::from("last"),
        ],
        "chain position must survive an in-place overwrite, not delete+append"
    );

    assert_eq!(
        std::fs::read_to_string(&history_path).unwrap(),
        "{\"ts\":1}\n",
        "usage_history.jsonl is the persisted log, not a cache — must survive"
    );

    for file in [
        crate::profile_cache::USAGE_CACHE_FILE,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        crate::throughput::THROUGHPUT_CACHE_FILE,
    ] {
        let path = crate::profile_cache::profile_cache_path("acme", file).unwrap();
        assert!(
            !path.exists(),
            "{file} must be dropped — it describes the old account"
        );
    }
}

/// Overwriting the ACTIVE profile must re-apply to live `~/.claude` state —
/// mirrors `edit_profile_endpoint`'s active-case handling. Without this a
/// running `claude` keeps reading the OLD endpoint/token until the next
/// explicit switch, and dropping OAuth creds on an active profile (a
/// third-party recapture) would leave `.credentials.json` a dangling
/// symlink instead of a clean absence.
#[test]
fn overwrite_captured_profile_reapplies_live_state_when_active() {
    let _home = HomeSandbox::new();

    let mut acme = Profile::new("acme".to_string(), None, None);
    acme.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&acme).expect("save acme");
    crate::claude::link_profile_credentials("acme").expect("link acme live");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acme".into()],
            fallback_chain: vec!["acme".into()],
            active_profile: Some("acme".into()),
            ..AppState::default()
        },
        profiles: vec![acme],
    };

    // Overwrite the active profile with a third-party (no-OAuth) snapshot.
    let snapshot = CaptureSnapshot {
        credentials: None,
        base_url: Some("https://api.example.com".to_string()),
        api_key: Some("new-api-key".to_string()),
    };
    overwrite_captured_profile(&mut config, "acme", snapshot).expect("overwrite active profile");

    let live_endpoint = crate::claude::read_claude_endpoint_config().expect("read live endpoint");
    assert_eq!(
        live_endpoint.base_url.as_deref(),
        Some("https://api.example.com"),
        "live settings.json must pick up the new base_url immediately, not on next switch"
    );
    assert_eq!(
        live_endpoint.api_key.as_deref(),
        Some("new-api-key"),
        "live settings.json must pick up the new api_key immediately, not on next switch"
    );

    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    assert!(
        live_path.symlink_metadata().is_err(),
        "no dangling .credentials.json symlink after credentials go to None while active"
    );
}
