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
    let live_path = crate::profile::claude_dir().unwrap().join(".credentials.json");
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
