//! Regression tests pinning the serde alias that lets clauth 0.2.0 users
//! upgrade without losing their persisted settings: `kick_timer` (per-profile
//! config.toml) was renamed to `auto_start` after 0.2.0. Drop the alias and the
//! test below fails.

use super::*;

#[test]
fn profile_config_reads_kick_timer_as_auto_start() {
    let toml = "kick_timer = true\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse old config");
    assert!(cfg.auto_start);
}

#[test]
fn profile_config_reads_auto_start_directly() {
    let toml = "auto_start = true\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse new config");
    assert!(cfg.auto_start);
}

// Drop `bell_threshold` from `ProfileConfig` and the hand-edited value is
// silently ignored on load (the bug this pins): the field must round-trip.
#[test]
fn profile_config_reads_bell_threshold() {
    let toml = "bell_threshold = 90.0\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse bell config");
    assert_eq!(cfg.bell_threshold, Some(90.0));
}

// `last_resort` (issue #8 follow-up) must default to `false` so every existing
// config.toml written before this field existed keeps loading unchanged.
#[test]
fn profile_config_last_resort_defaults_false() {
    let cfg: ProfileConfig = toml::from_str("").expect("parse empty config");
    assert!(!cfg.last_resort);
}

#[test]
fn profile_config_reads_last_resort_true() {
    let toml = "last_resort = true\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse last_resort config");
    assert!(cfg.last_resort);
}

// `last_resort` must survive a config.toml render→parse round-trip, matching
// the guarantee `model_settings_round_trip_through_config_toml` pins for models.
#[test]
fn last_resort_round_trips_through_config_toml() {
    let mut profile = Profile::new("p".to_string(), None, None);
    profile.last_resort = true;
    let rendered = render_config_toml(&profile);
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert!(parsed.last_resort);
}

// `burn_aware_switching` (issue #8 follow-up b) must default to `false` so
// every existing profiles.toml written before this field existed keeps
// loading unchanged, matching the `last_resort` guarantee above at the
// `AppState` level.
#[test]
fn app_state_burn_aware_switching_defaults_false() {
    let state: AppState = toml::from_str("profiles = []\n").expect("parse state");
    assert!(!state.burn_aware_switching);
}

#[test]
fn app_state_reads_burn_aware_switching_true() {
    let toml = "profiles = []\nburn_aware_switching = true\n";
    let state: AppState = toml::from_str(toml).expect("parse state");
    assert!(state.burn_aware_switching);
}

// On must round-trip explicitly; off (the default) is omitted entirely from
// the rendered profiles.toml, matching `show_pace`/`count_cache`'s treatment
// of their own default-off booleans.
#[test]
fn burn_aware_switching_round_trips_and_is_omitted_when_off() {
    let on = AppState {
        burn_aware_switching: true,
        ..AppState::default()
    };
    let rendered_on = toml::to_string_pretty(&on).expect("render on state");
    assert!(
        rendered_on.contains("burn_aware_switching = true"),
        "on must render explicitly, got:\n{rendered_on}"
    );
    let reparsed: AppState = toml::from_str(&rendered_on).expect("reparse on state");
    assert!(reparsed.burn_aware_switching);

    let off = AppState::default();
    let rendered_off = toml::to_string_pretty(&off).expect("render default state");
    assert!(
        !rendered_off.contains("burn_aware_switching"),
        "off (default) must be omitted, got:\n{rendered_off}"
    );
}

#[test]
fn profile_name_is_serde_transparent() {
    // `ProfileName` must serialize as a bare string so profiles.toml stays
    // byte-identical to the pre-newtype format (a non-transparent newtype
    // would silently migrate every user's state file).
    let toml = r#"active_profile = "work"
profiles = ["work", "play"]
fallback_chain = ["work"]
"#;
    let state: AppState = toml::from_str(toml).expect("parse bare-string state");
    assert_eq!(state.active_profile.as_deref(), Some("work"));
    assert_eq!(state.profiles, ["work", "play"]);
    assert_eq!(state.fallback_chain, ["work"]);

    let rendered = toml::to_string_pretty(&state).expect("render state");
    let reparsed: AppState = toml::from_str(&rendered).expect("reparse");
    assert_eq!(reparsed.active_profile.as_deref(), Some("work"));
    assert_eq!(reparsed.profiles, ["work", "play"]);
    assert_eq!(reparsed.fallback_chain, ["work"]);
    assert!(
        rendered.contains("active_profile = \"work\""),
        "active_profile must render as a bare string, got:\n{rendered}"
    );
    assert!(
        rendered.contains("\"work\"") && rendered.contains("\"play\""),
        "profile names must render as bare strings, got:\n{rendered}"
    );
    assert!(
        !rendered.contains("ProfileName") && !rendered.contains("[profiles."),
        "no newtype wrapper may appear on disk, got:\n{rendered}"
    );

    // Byte-for-byte equality with a String-typed control — no format migration.
    // Field order and serde attrs mirror `AppState` exactly.
    #[derive(serde::Serialize, Default)]
    struct BareState {
        active_profile: Option<String>,
        profiles: Vec<String>,
        fallback_chain: Vec<String>,
        wrap_off: bool,
        refresh_interval_ms: u64,
    }
    let control = BareState {
        active_profile: Some("work".to_string()),
        profiles: vec!["work".to_string(), "play".to_string()],
        fallback_chain: vec!["work".to_string()],
        refresh_interval_ms: 90_000,
        ..Default::default()
    };
    assert_eq!(
        rendered,
        toml::to_string_pretty(&control).expect("render control"),
        "ProfileName AppState must serialize byte-identically to a String one"
    );
}

#[cfg(unix)]
use crate::testutil::HomeSandbox;

#[cfg(unix)]
fn oauth_credentials() -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "tok-access".to_string(),
            refresh_token: Some("tok-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

/// credentials.json, its `.pending` rotation sidecar, and the per-profile dir
/// must carry tightened permissions: 0o600 files, 0o700 dir.
#[cfg(unix)]
#[test]
fn credential_and_cache_files_have_restricted_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let name = "perm-test-credentials";
    let creds = oauth_credentials();

    let profile = Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: std::collections::BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        bell_threshold: None,
        credentials: Some(creds.clone()),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    };
    // Goes through ConfigHandle-equivalent path: save_profile takes the state
    // flock (rank-ordered) and writes credentials.json before config.toml.
    save_profile(&profile).expect("save_profile");

    let dir_mode = std::fs::metadata(profile_dir(name).expect("profile_dir"))
        .expect("dir metadata")
        .permissions()
        .mode();
    assert_eq!(
        dir_mode & 0o777,
        0o700,
        "profile dir mode should be 0o700, got {:#o}",
        dir_mode & 0o777,
    );

    let cred_path = profile_subpath(name, "credentials.json").expect("cred path");
    let cred_mode = std::fs::metadata(&cred_path)
        .expect("credentials.json metadata")
        .permissions()
        .mode();
    assert_eq!(
        cred_mode & 0o777,
        0o600,
        "credentials.json mode should be 0o600, got {:#o}",
        cred_mode & 0o777,
    );

    // Stage the rotation sidecar and assert its mode too.
    stage_rotated_credentials(name, &creds).expect("stage_rotated_credentials");
    let pending_path = profile_subpath(name, "credentials.json.pending").expect("pending path");
    let pending_mode = std::fs::metadata(&pending_path)
        .expect("credentials.json.pending metadata")
        .permissions()
        .mode();
    assert_eq!(
        pending_mode & 0o777,
        0o600,
        "credentials.json.pending mode should be 0o600, got {:#o}",
        pending_mode & 0o777,
    );
}

/// The real usage-cache writer (`profile_cache::write_profile_cache`) must
/// create usage_cache.json at 0o600 and, when it has to create the per-profile
/// dir, that dir at 0o700. Driven on a FRESH profile name so the dir does not
/// pre-exist.
#[cfg(unix)]
#[test]
fn usage_cache_write_creates_restricted_file_and_dir() {
    use std::os::unix::fs::PermissionsExt;

    let _home = HomeSandbox::new();
    let name = "perm-test-usage-cache";

    // Fresh profile: its dir must not exist before the cache write.
    let dir = profile_dir(name).expect("profile_dir");
    assert!(
        !dir.exists(),
        "precondition: profile dir must not pre-exist for a fresh profile"
    );

    // Drive the actual production writer.
    let info = crate::usage::UsageInfo::default();
    crate::profile_cache::write_profile_cache(name, crate::profile_cache::USAGE_CACHE_FILE, &info);

    let dir_mode = std::fs::metadata(&dir)
        .expect("freshly-created profile dir metadata")
        .permissions()
        .mode();
    assert_eq!(
        dir_mode & 0o777,
        0o700,
        "freshly-created profile dir mode should be 0o700, got {:#o}",
        dir_mode & 0o777,
    );

    let cache_path = profile_subpath(name, "usage_cache.json").expect("cache path");
    let cache_mode = std::fs::metadata(&cache_path)
        .expect("usage_cache.json metadata")
        .permissions()
        .mode();
    assert_eq!(
        cache_mode & 0o777,
        0o600,
        "usage_cache.json mode should be 0o600, got {:#o}",
        cache_mode & 0o777,
    );
}

#[test]
fn profile_config_reads_models_table() {
    let toml = "[models]\n\
        default = \"opusplan\"\n\
        haiku = \"claude-haiku-4-5\"\n";
    let cfg: ProfileConfig = toml::from_str(toml).expect("parse models table");
    assert_eq!(cfg.models.default.as_deref(), Some("opusplan"));
    assert_eq!(cfg.models.haiku.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(cfg.models.sonnet, None);
}

// Model config must survive a config.toml render→parse round-trip, or
// `maybe_rewrite_config_toml` would either drop a hand-set value or thrash the
// file on every reload.
#[test]
fn model_settings_round_trip_through_config_toml() {
    let mut profile = Profile::new("p".to_string(), None, None);
    profile.models = ModelSettings {
        default: Some("opusplan".to_string()),
        opus: Some("claude-opus-4-8[1m]".to_string()),
        sonnet: None,
        haiku: None,
        subagent: Some("claude-haiku-4-5".to_string()),
    };
    let rendered = render_config_toml(&profile);
    let parsed: ProfileConfig = toml::from_str(&rendered).expect("parse rendered toml");
    assert_eq!(parsed.models, profile.models);
}
