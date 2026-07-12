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

/// `switch_profile` to a name with no profile must bail BEFORE any side
/// effect. Pre-fix the existence check lived in `finish_switch` — LAST in the
/// sequence — so `force_link_profile_credentials` had already torn down the
/// live `.credentials.json` for a ghost target (a profile deleted by
/// `clauth delete` while a queued auto-switch — e.g. a daemon's pending
/// switch — MCP switch, or CLI switch still held its name), destroying the
/// live login even though the switch itself failed.
#[test]
fn switch_to_a_missing_profile_bails_before_touching_the_live_link() {
    let _home = HomeSandbox::new();

    let mut p = Profile::new("keeper".to_string(), None, None);
    p.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "keeper-access".to_string(),
            refresh_token: Some("keeper-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&p).expect("save profile");

    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
    std::fs::write(
        &live_path,
        serde_json::to_vec(p.credentials.as_ref().unwrap()).unwrap(),
    )
    .unwrap();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![p],
    };
    config.state.active_profile = Some("keeper".into());

    let err = switch_profile(&mut config, "ghost").expect_err("ghost must bail");
    assert!(
        err.to_string().contains("not found"),
        "bail names the cause, got: {err}"
    );
    assert!(config.is_active("keeper"), "active unchanged");
    assert!(live_path.exists(), "the live credentials file survives");
    let stored = crate::profile::profile_dir("keeper")
        .unwrap()
        .join("credentials.json");
    assert!(stored.exists(), "keeper's stored credentials survive");
}

/// AUTH-4 parity, TUI side: `auto_switch_if_needed` must leave an auth-broken
/// active even when its (frozen-stale) usage still reads as headroom — the
/// same wedge `scan_auto_switch` had on the daemon side. Pre-fix, the
/// exhaustion gate alone returned `None` here and the TUI parked on the dead
/// account forever.
#[test]
fn auto_switch_if_needed_walks_off_a_broken_active() {
    use crate::fallback::{SwitchAction, auto_switch_if_needed};
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let _home = HomeSandbox::new();

    let mk = |name: &str, access: &str, util: f64, resets_at: i64| {
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
        p.usage = Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: util,
                resets_at: Some(epoch_secs_to_iso(resets_at)),
            }),
            ..Default::default()
        });
        crate::profile::save_profile(&p).expect("save profile");
        p
    };
    // Active "a": broken, last-ever read maxed on a LAPSED window (reads as
    // idle headroom). Target "b": healthy, live window with real headroom.
    let a = mk("a", "a-access", 100.0, now_epoch_secs() - 3600);
    let b = mk("b", "b-access", 10.0, now_epoch_secs() + 3600);

    // Live file = the active account's own captured mirror (macOS shape), so
    // the switch's foreign-file guard sees its own mirror and proceeds.
    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
    std::fs::write(
        &live_path,
        serde_json::to_vec(a.credentials.as_ref().unwrap()).unwrap(),
    )
    .unwrap();

    let mut config = AppConfig {
        state: AppState {
            active_profile: Some("a".into()),
            profiles: vec!["a".into(), "b".into()],
            fallback_chain: vec!["a".into(), "b".into()],
            auth_broken: vec!["a".into()],
            ..AppState::default()
        },
        profiles: vec![a, b],
    };

    let action = auto_switch_if_needed(&mut config, None).expect("auto switch");
    assert_eq!(
        action,
        Some(SwitchAction::To("b".to_string())),
        "a dead active with stale-headroom usage must still be walked away from"
    );
    assert!(config.is_active("b"));
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

// ── set_profile_default_model (`clauth login --model`, the create-form row) ──
// (the ensure_login_profile tests were dropped with the fn — `clauth login` now
//  mints tokens via the browser flow and captures a profile, rather than
//  pre-creating a blank one; `--model` is applied to the captured profile.)

#[test]
fn set_profile_default_model_persists_to_config_toml() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    set_profile_default_model(&mut config, "acct", "opus").expect("set model");

    assert_eq!(
        config.find("acct").unwrap().models.default.as_deref(),
        Some("opus")
    );
    let toml = std::fs::read_to_string(profile_dir("acct").unwrap().join("config.toml"))
        .expect("config.toml written");
    assert!(toml.contains("opus"), "model persisted to config.toml");
}

#[test]
fn set_profile_default_model_preserves_alias_overrides() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    edit_profile_model(
        &mut config,
        "acct",
        ModelSettings {
            opus: Some("claude-opus-4-8".to_string()),
            ..ModelSettings::default()
        },
    )
    .expect("seed opus alias");

    set_profile_default_model(&mut config, "acct", "sonnet").expect("set default");

    let profile = config.find("acct").unwrap();
    assert_eq!(profile.models.default.as_deref(), Some("sonnet"));
    assert_eq!(
        profile.models.opus.as_deref(),
        Some("claude-opus-4-8"),
        "setting the default must not clobber an existing alias override"
    );
}

#[test]
fn set_profile_default_model_blank_clears_default() {
    let _home = HomeSandbox::new();
    let mut config = acct_config();
    set_profile_default_model(&mut config, "acct", "opus").expect("set model");
    set_profile_default_model(&mut config, "acct", "   ").expect("clear model");
    assert!(
        config.find("acct").unwrap().models.default.is_none(),
        "blank input clears the default, mirroring the Setup tab's ⏎ commit"
    );
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

/// A reauth overwrite replaces the dead credential chain — the whole point of
/// re-logging in — so it must lift the profile's `auth_broken` quarantine,
/// exactly like the fresh-capture path (`capture_into_profile`) does. Left
/// set, the flag keeps the just-relogged account excluded from every chain
/// walk and keeps the "login expired" banner up (observed 2026-07-09: a
/// re-login via the menu bar left the profile quarantined).
#[test]
fn overwrite_captured_profile_clears_auth_broken_quarantine() {
    let _home = HomeSandbox::new();

    let first = Profile::new("first".to_string(), None, None);
    save_profile(&first).expect("save first");
    let target = Profile::new("acme".to_string(), None, None);
    save_profile(&target).expect("save target");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["first".into(), "acme".into()],
            fallback_chain: vec!["first".into(), "acme".into()],
            active_profile: Some("first".into()),
            auth_broken: vec!["acme".into()],
            ..AppState::default()
        },
        profiles: vec![first, target],
    };

    let snapshot = CaptureSnapshot {
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "fresh-access".to_string(),
                refresh_token: Some("fresh-refresh".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        base_url: None,
        api_key: None,
    };
    overwrite_captured_profile(&mut config, "acme", snapshot).expect("overwrite");

    assert!(
        !config.is_auth_broken("acme"),
        "in-memory quarantine must lift with the fresh credentials"
    );
    let persisted = crate::profile::load_config().expect("reload").state;
    assert!(
        !persisted.auth_broken.iter().any(|n| n.as_str() == "acme"),
        "persisted quarantine must lift too"
    );
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

/// Deleting the ACTIVE API-key profile must strip its endpoint + key from the
/// live `~/.claude/settings.json`, not only the (absent) credentials link.
/// Otherwise the deleted account's `ANTHROPIC_AUTH_TOKEN` lingers in plaintext
/// and the next session still routes to the dead endpoint.
#[test]
fn delete_active_api_profile_unwires_settings_endpoint() {
    let _home = HomeSandbox::new();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(
        &mut config,
        "api-acct".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-secret".to_string()),
        None,
    )
    .expect("create api profile");
    // create_blank_profile does not activate; mark it active and wire the live
    // settings.json the way a switch would, then delete it out from under that.
    config.state.active_profile = Some("api-acct".into());
    let profile = config.find("api-acct").expect("profile present").clone();
    crate::claude::apply_profile_to_claude_settings(&profile, &[]).expect("seed settings.json");
    assert_eq!(
        crate::claude::read_claude_endpoint_config()
            .expect("read endpoint")
            .api_key
            .as_deref(),
        Some("sk-secret"),
        "precondition: active api key is wired into settings.json"
    );

    delete_profile(&mut config, "api-acct").expect("delete active api profile");

    let after = crate::claude::read_claude_endpoint_config().expect("read endpoint");
    assert_eq!(
        after.base_url, None,
        "deleted endpoint must not linger in settings.json"
    );
    assert_eq!(
        after.api_key, None,
        "deleted api key must not linger in settings.json"
    );
}

/// Setup-tab "log out" on an ACTIVE API account drops only the api key: the base
/// url stays wired (account keeps its API shell + active status), the live
/// `settings.json` loses `ANTHROPIC_AUTH_TOKEN` but keeps `ANTHROPIC_BASE_URL`,
/// and the stale third-party stats cache is removed.
#[test]
fn clear_profile_api_key_keeps_base_url_and_active_status() {
    let _home = HomeSandbox::new();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    create_blank_profile(
        &mut config,
        "api-acct".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-secret".to_string()),
        None,
    )
    .expect("create api profile");
    config.state.active_profile = Some("api-acct".into());
    let profile = config.find("api-acct").expect("profile present").clone();
    crate::claude::apply_profile_to_claude_settings(&profile, &[]).expect("seed settings.json");
    crate::profile_cache::write_profile_cache(
        "api-acct",
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &"stale",
    );

    clear_profile_api_key(&mut config, "api-acct").expect("clear api key");

    let profile = config.find("api-acct").expect("profile still present");
    assert_eq!(profile.api_key, None, "api key dropped");
    assert_eq!(
        profile.base_url.as_deref(),
        Some("https://api.example.com"),
        "base-url shell preserved"
    );
    assert_eq!(
        config.state.active_profile.as_deref(),
        Some("api-acct"),
        "account stays active (only the key is gone)"
    );

    let after = crate::claude::read_claude_endpoint_config().expect("read endpoint");
    assert_eq!(
        after.base_url.as_deref(),
        Some("https://api.example.com"),
        "live base url kept so the account stays an API shell"
    );
    assert_eq!(after.api_key, None, "live auth token stripped on log out");

    let cache = crate::profile_cache::profile_cache_path(
        "api-acct",
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .unwrap();
    assert!(!cache.exists(), "stale third-party stats cache dropped");
}

/// Blanking an active profile drops its credentials + per-account fetch caches
/// and clears the live link + `active_profile`, while name/env/model survive.
#[test]
fn clear_profile_credentials_blanks_active_profile_keeping_shell() {
    let _home = HomeSandbox::new();

    let mut acct = Profile::new("acct".to_string(), None, None);
    acct.auto_start = true;
    acct.env.insert("FOO".to_string(), "bar".to_string());
    acct.models.opus = Some("claude-opus-4".to_string());
    acct.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    save_profile(&acct).expect("save acct");
    crate::claude::link_profile_credentials("acct").expect("link acct live");

    for file in [
        crate::profile_cache::USAGE_CACHE_FILE,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        crate::throughput::THROUGHPUT_CACHE_FILE,
    ] {
        crate::profile_cache::write_profile_cache("acct", file, &"stale");
    }

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acct".into()],
            fallback_chain: vec!["acct".into()],
            active_profile: Some("acct".into()),
            ..AppState::default()
        },
        profiles: vec![acct],
    };

    clear_profile_credentials(&mut config, "acct").expect("clear credentials");

    let profile = config.find("acct").expect("profile still present");
    assert!(profile.credentials.is_none(), "credentials dropped");
    assert!(profile.auto_start, "shell preserved: auto_start");
    assert_eq!(
        profile.env.get("FOO"),
        Some(&"bar".to_string()),
        "shell preserved: env"
    );
    assert_eq!(
        profile.models.opus.as_deref(),
        Some("claude-opus-4"),
        "shell preserved: model"
    );
    assert!(
        config.state.active_profile.is_none(),
        "active profile deactivated"
    );

    let cred_path = profile_dir("acct").unwrap().join("credentials.json");
    assert!(!cred_path.exists(), "credentials.json removed");

    for file in [
        crate::profile_cache::USAGE_CACHE_FILE,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        crate::throughput::THROUGHPUT_CACHE_FILE,
    ] {
        let path = crate::profile_cache::profile_cache_path("acct", file).unwrap();
        assert!(!path.exists(), "{file} must be dropped");
    }

    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    assert!(
        live_path.symlink_metadata().is_err(),
        "live .credentials.json link cleared on blanking the active profile"
    );
}

/// Blanking a NON-active profile must not touch the active link / `active_profile`,
/// and a lingering rotation sidecar must not resurrect the deleted login on the
/// next disk load (`recover_pending_credentials` treats a missing credentials.json
/// as a failed commit and adopts the sidecar).
#[test]
fn clear_profile_credentials_non_active_and_no_sidecar_resurrection() {
    let _home = HomeSandbox::new();

    let creds = || ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "acc".to_string(),
            refresh_token: Some("ref".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };

    let mut acct = Profile::new("acct".to_string(), None, None);
    acct.credentials = Some(creds());
    save_profile(&acct).expect("save acct");
    // A rotation sidecar that never committed — the resurrection vector.
    crate::profile::stage_rotated_credentials("acct", &creds()).expect("stage sidecar");

    let mut other = Profile::new("other".to_string(), None, None);
    other.credentials = Some(creds());
    save_profile(&other).expect("save other");
    crate::claude::link_profile_credentials("other").expect("link other live");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acct".into(), "other".into()],
            fallback_chain: vec!["acct".into(), "other".into()],
            active_profile: Some("other".into()),
            ..AppState::default()
        },
        profiles: vec![acct, other],
    };

    // Persist the profile list so `load_config` below can find both by name.
    crate::profile::save_app_state(&config.state).expect("persist state");

    clear_profile_credentials(&mut config, "acct").expect("clear credentials");

    // The active profile and its live link are untouched — only "acct" changed.
    assert_eq!(
        config.state.active_profile.as_deref(),
        Some("other"),
        "blanking a non-active profile leaves the active one set"
    );
    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    assert!(
        live_path.symlink_metadata().is_ok(),
        "the active profile's live link survives a non-active blank"
    );

    // Reload from disk: the sidecar must be gone, so the login stays deleted.
    let reloaded = crate::profile::load_config().expect("reload config");
    let acct = reloaded.find("acct").expect("acct still present");
    assert!(
        acct.credentials.is_none(),
        "a lingering sidecar must not resurrect the blanked login"
    );
    let cred_path = profile_dir("acct").unwrap().join("credentials.json");
    assert!(
        !cred_path.exists(),
        "credentials.json stays gone after reload (sidecar not adopted)"
    );
}

// ── issue #17: stale oauthAccount deleted on every switch path ────────────

fn home_claude_json_path() -> std::path::PathBuf {
    crate::profile::home_dir().unwrap().join(".claude.json")
}

fn write_home_claude_json_with_identity() {
    std::fs::write(
        home_claude_json_path(),
        serde_json::to_vec_pretty(&serde_json::json!({
            "oauthAccount": {"emailAddress": "stale@x"},
            "numStartups": 7,
        }))
        .unwrap(),
    )
    .expect("write home .claude.json");
}

/// `finish_switch` is the shared convergence point for the manual CLI, TUI,
/// MCP `switch`, and fallback switch paths (all four route through
/// `switch_profile`/`switch_profile_reconciled`/`switch_profile_discard`,
/// which call it under the state lock) — asserting on it directly pins the
/// behaviour for all of them at once.
#[test]
fn finish_switch_deletes_stale_oauth_account_block() {
    let _home = HomeSandbox::new();
    write_home_claude_json_with_identity();

    let mut config = acct_config();
    finish_switch(&mut config, "acct").expect("finish_switch");

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home_claude_json_path()).unwrap()).unwrap();
    assert!(
        after.get("oauthAccount").is_none(),
        "the outgoing account's identity block must be gone after a switch"
    );
    assert_eq!(
        after["numStartups"],
        serde_json::json!(7),
        "unrelated keys must survive the switch untouched"
    );
}

/// `switch_off` (chain-exhausted / manual "turn off") clears live credentials
/// without going through `finish_switch` — a stale identity block is just as
/// wrong once creds are gone, so it needs its own coverage rather than relying
/// on the shared path.
#[test]
fn switch_off_also_deletes_stale_oauth_account_block() {
    let _home = HomeSandbox::new();
    write_home_claude_json_with_identity();

    let profile = Profile::new("acct".to_string(), None, None);
    save_profile(&profile).expect("save profile");
    crate::claude::link_profile_credentials("acct").expect("link acct live");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acct".into()],
            active_profile: Some("acct".into()),
            ..AppState::default()
        },
        profiles: vec![profile],
    };

    switch_off(&mut config).expect("switch_off");

    assert!(config.state.active_profile.is_none());
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home_claude_json_path()).unwrap()).unwrap();
    assert!(
        after.get("oauthAccount").is_none(),
        "no active account remains, so the stale identity block must be gone too"
    );
    assert_eq!(after["numStartups"], serde_json::json!(7));
}

/// `switch_off` on a DIVERGED live file: the foreign `/login` is dropped, never
/// absorbed. `snapshot_active_credentials` skips a diverged file so the profile
/// keeps its stored identity while the live creds are cleared; the divergence
/// flow's consent prompt is what stands between the user and this drop.
#[test]
fn switch_off_on_diverged_file_keeps_profile_snapshot_and_drops_login() {
    let _home = HomeSandbox::new();

    let mut profile = Profile::new("acct".to_string(), None, None);
    profile.credentials = Some(oauth_creds("stored-token"));
    save_profile(&profile).expect("save profile");

    // A plain file with a different token where the symlink should sit = Diverged.
    let live = _home.home().join(".claude").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("fresh-login")).expect("serialize"),
    )
    .expect("write diverged live file");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["acct".into()],
            active_profile: Some("acct".into()),
            ..AppState::default()
        },
        profiles: vec![profile],
    };

    switch_off(&mut config).expect("switch_off");

    assert!(config.state.active_profile.is_none());
    assert!(
        !live.exists(),
        "live creds cleared: the fresh login is gone"
    );
    assert_eq!(
        config.profiles[0]
            .credentials
            .as_ref()
            .and_then(|c| c.access_token()),
        Some("stored-token"),
        "a foreign login must never be absorbed into the profile snapshot"
    );
}

fn oauth_creds(access: &str) -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: access.to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

/// AUTH-1 reauth: `clauth login <existing>` overwrites a quarantined profile's
/// stored tokens through `overwrite_captured_profile` — the documented recovery
/// for a revoked login — and must clear its auth-broken flag so the recovered
/// account rejoins the fallback chain and is a valid switch target again. The
/// active-but-dead account here is the Incident C scenario.
#[test]
fn reauth_overwrite_clears_broken_flag() {
    let _home = HomeSandbox::new();

    let mut stale = Profile::new("xfx".to_string(), None, None);
    stale.credentials = Some(oauth_creds("stale-access"));
    save_profile(&stale).expect("save profile");

    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["xfx".into()],
            active_profile: Some("xfx".into()),
            ..AppState::default()
        },
        profiles: vec![stale],
    };
    config.set_auth_broken("xfx", true);
    assert!(config.is_auth_broken("xfx"), "precondition: quarantined");

    overwrite_captured_profile(
        &mut config,
        "xfx",
        CaptureSnapshot {
            credentials: Some(oauth_creds("fresh-access")),
            base_url: None,
            api_key: None,
        },
    )
    .expect("re-auth overwrite");

    assert_eq!(
        config.find("xfx").and_then(|p| p.access_token()),
        Some("fresh-access"),
        "credentials overwritten by re-auth",
    );
    assert!(
        !config.is_auth_broken("xfx"),
        "auth-broken quarantine cleared by re-auth",
    );
}

/// AUTH-1 switch gate (Incident C): a CLI switch to a target whose OAuth login
/// is dead — expired access token, no refresh token, so unrecoverable without a
/// re-login — is refused with the exact `clauth login <name>` recovery hint
/// instead of installing the dead token into the Keychain. The no-refresh-token
/// path reaches `AuthGate::Broken` with no network call, so the assertion stays
/// hermetic.
#[test]
fn switch_cli_refuses_dead_target_with_login_hint() {
    let _home = HomeSandbox::new();

    let mut dead = Profile::new("dead-acct".to_string(), None, None);
    dead.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "expired".to_string(),
            refresh_token: None,
            expires_at: Some(1), // epoch-ms 1 → long expired
            scopes: None,
            subscription_type: None,
        }),
    });

    let config = AppConfig {
        state: AppState {
            profiles: vec!["dead-acct".into()],
            active_profile: None, // no outgoing profile → no link reconcile before the gate
            ..AppState::default()
        },
        profiles: vec![dead],
    };

    let err = switch_profile_cli(config, "dead-acct").expect_err("a dead target must be refused");
    assert!(
        err.to_string().contains("clauth login dead-acct"),
        "the refusal must name the recovery command, got: {err}",
    );
}

// ── identify_live_login_owner: whose login sits in ~/.claude right now ──────
//
// Token-equality tier only. An account-uuid tier (CC's `~/.claude.json`
// identity record matched against a profile's cached anchor) layers on once
// per-profile identity anchors exist (PR #24) — no anchors upstream yet.

#[cfg(unix)]
mod identify_live_login_owner {
    use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile};
    use crate::testutil::HomeSandbox;

    fn creds(access: &str, refresh: &str) -> ClaudeCredentials {
        ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some(refresh.to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }
    }

    fn config_with(profiles: Vec<(&str, ClaudeCredentials)>) -> AppConfig {
        let profiles: Vec<Profile> = profiles
            .into_iter()
            .map(|(name, c)| {
                let mut p = Profile::new(name.to_string(), None, None);
                p.credentials = Some(c);
                p
            })
            .collect();
        AppConfig {
            state: AppState {
                profiles: profiles.iter().map(|p| p.name.clone()).collect(),
                ..AppState::default()
            },
            profiles,
        }
    }

    fn write_live(c: &ClaudeCredentials) {
        let live = crate::profile::claude_dir()
            .expect("claude dir")
            .join(".credentials.json");
        std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
        std::fs::write(&live, serde_json::to_vec(c).expect("ser")).expect("write");
    }

    /// Exact token equality — the live file IS a profile's stored credential
    /// (stale mirror / half-landed switch).
    #[test]
    fn exact_token_match_identifies_the_owner() {
        let _home = HomeSandbox::new();
        let cfg = config_with(vec![
            ("a", creds("at-a", "rt-a")),
            ("b", creds("at-b", "rt-b")),
        ]);
        write_live(&creds("at-b", "rt-b"));
        assert_eq!(
            crate::actions::identify_live_login_owner(&cfg).as_deref(),
            Some("b")
        );
    }

    /// No token match → unknown; a CC re-login where every token is new (no
    /// anchors yet) and a genuinely foreign account both identify nobody.
    #[test]
    fn a_foreign_login_identifies_nobody() {
        let _home = HomeSandbox::new();
        let cfg = config_with(vec![("a", creds("at-a", "rt-a"))]);
        write_live(&creds("at-foreign", "rt-foreign"));
        assert_eq!(crate::actions::identify_live_login_owner(&cfg), None);
    }
}
