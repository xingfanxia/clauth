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
