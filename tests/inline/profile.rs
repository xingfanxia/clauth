//! Regression tests pinning the serde aliases that let clauth 0.2.0 users
//! upgrade without losing their persisted settings. Two fields were renamed
//! after 0.2.0:
//!   - `kick_timer` (per-profile config.toml) → `auto_start`
//!   - `last_kick_at` (profiles.toml)         → `last_auto_start_at`
//!
//! Drop one of these aliases and the tests below fail.

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
        #[serde(rename = "last_kick_at")]
        last_auto_start_at: std::collections::HashMap<String, u64>,
        fallback_chain: Vec<String>,
        wrap_off: bool,
    }
    let control = BareState {
        active_profile: Some("work".to_string()),
        profiles: vec!["work".to_string(), "play".to_string()],
        fallback_chain: vec!["work".to_string()],
        ..Default::default()
    };
    assert_eq!(
        rendered,
        toml::to_string_pretty(&control).expect("render control"),
        "ProfileName AppState must serialize byte-identically to a String one"
    );
}

#[test]
fn app_state_reads_last_kick_at_as_last_auto_start_at() {
    let toml = r#"
profiles = ["work"]
[last_kick_at]
work = 1700000000000
"#;
    let state: AppState = toml::from_str(toml).expect("parse old state");
    assert_eq!(state.last_auto_start_at.get("work"), Some(&1700000000000));
}

#[test]
fn app_state_writes_last_auto_start_at_as_last_kick_at_on_disk() {
    // Forward-compat: 0.2.0 must read files written by newer clauth; on-disk name kept via rename.
    let mut state = AppState::default();
    state.last_auto_start_at.insert("work".into(), 42);
    let serialized = toml::to_string(&state).expect("serialize state");
    assert!(
        serialized.contains("[last_kick_at]"),
        "expected serialized AppState to use disk name `last_kick_at`, got:\n{serialized}"
    );
    assert!(!serialized.contains("last_auto_start_at"));
}
