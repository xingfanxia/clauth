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
    // The `ProfileName` newtype must serialize as a bare string so profiles.toml
    // stays byte-identical to the pre-newtype format. A non-transparent newtype
    // would wrap it (e.g. a single-field table), silently migrating every
    // user's state file. Round-trip a hand-written bare-string state and assert
    // the re-rendered TOML matches what the loader produced.
    let toml = r#"active_profile = "work"
profiles = ["work", "play"]
fallback_chain = ["work"]
"#;
    let state: AppState = toml::from_str(toml).expect("parse bare-string state");
    assert_eq!(state.active_profile.as_deref(), Some("work"));
    assert_eq!(state.profiles, ["work", "play"]);
    assert_eq!(state.fallback_chain, ["work"]);

    // Re-serialize and confirm the name lists are still bare strings (no
    // newtype wrapper appears anywhere in the rendered output).
    let rendered = toml::to_string_pretty(&state).expect("render state");
    let reparsed: AppState = toml::from_str(&rendered).expect("reparse");
    assert_eq!(reparsed.active_profile.as_deref(), Some("work"));
    assert_eq!(reparsed.profiles, ["work", "play"]);
    assert_eq!(reparsed.fallback_chain, ["work"]);
    assert!(
        rendered.contains("active_profile = \"work\""),
        "active_profile must render as a bare string, got:\n{rendered}"
    );
    // A transparent newtype renders each name as a bare quoted string; a
    // non-transparent wrapper would emit a nested table/struct instead.
    assert!(
        rendered.contains("\"work\"") && rendered.contains("\"play\""),
        "profile names must render as bare strings, got:\n{rendered}"
    );
    assert!(
        !rendered.contains("ProfileName") && !rendered.contains("[profiles."),
        "no newtype wrapper may appear on disk, got:\n{rendered}"
    );

    // Byte-for-byte: a `ProfileName`-typed AppState renders identically to a
    // String-typed control with the same data. This is the load-bearing
    // "no format migration" guarantee.
    // Field order and serde attrs mirror `AppState` exactly so the rendered
    // TOML is directly comparable.
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
    // Forward-compat: a 0.2.0 binary must still be able to read profiles.toml
    // written by a newer clauth. We keep the on-disk field name `last_kick_at`
    // via #[serde(rename = "last_kick_at")].
    let mut state = AppState::default();
    state.last_auto_start_at.insert("work".into(), 42);
    let serialized = toml::to_string(&state).expect("serialize state");
    assert!(
        serialized.contains("[last_kick_at]"),
        "expected serialized AppState to use disk name `last_kick_at`, got:\n{serialized}"
    );
    assert!(!serialized.contains("last_auto_start_at"));
}
