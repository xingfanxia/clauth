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

/// RAII home sandbox: acquires `HOME_TEST_LOCK` and redirects `home_dir()` into
/// a tempdir for the duration, clearing on drop (even on panic). Required for
/// any test that writes into the per-profile tree, or those paths land in the
/// real `~/.clauth`.
#[cfg(unix)]
struct HomeSandbox {
    // Drop order: tempdir first, then the shared lock.
    _tmp: tempfile::TempDir,
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(unix)]
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

#[cfg(unix)]
impl Drop for HomeSandbox {
    fn drop(&mut self) {
        crate::profile::clear_home_override();
    }
}

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
        fallback_threshold: None,
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

/// The real `write_disk_cache` (usage/fetch.rs) must create usage_cache.json at
/// 0o600 and, when it has to create the per-profile dir, that dir at 0o700.
/// Driven on a FRESH profile name so the dir does not pre-exist.
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

    // Drive the actual production writer (re-exported from `crate::usage`).
    let info = crate::usage::UsageInfo::default();
    crate::usage::write_disk_cache(name, &info);

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
