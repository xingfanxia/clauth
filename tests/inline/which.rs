use super::*;
use std::collections::BTreeMap;

use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile};

fn oauth_profile(name: &str, refresh: &str) -> Profile {
    Profile {
        name: name.to_string(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        fallback_threshold: None,
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: format!("at-{name}"),
                refresh_token: Some(refresh.to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
    }
}

fn endpoint_profile(name: &str) -> Profile {
    Profile {
        name: name.to_string(),
        base_url: Some("https://example.test".to_string()),
        api_key: Some("sk-x".to_string()),
        auto_start: false,
        env: BTreeMap::new(),
        fallback_threshold: None,
        credentials: None,
        usage: None,
        fetch_status: None,
    }
}

fn blank_profile(name: &str) -> Profile {
    Profile {
        name: name.to_string(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        fallback_threshold: None,
        credentials: None,
        usage: None,
        fetch_status: None,
    }
}

fn live_oauth(refresh: Option<&str>) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-live".to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

fn config_with(profiles: Vec<Profile>, active: Option<&str>) -> AppConfig {
    let names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            active_profile: active.map(str::to_string),
            profiles: names,
            ..Default::default()
        },
        profiles,
    }
}

#[test]
fn matches_profile_by_refresh_token() {
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            oauth_profile("personal", "rt-personal"),
        ],
        Some("work"),
    );
    assert_eq!(
        match_by_refresh_token(&config, "rt-personal"),
        Some("personal")
    );
}

#[test]
fn returns_none_when_no_profile_holds_token() {
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    assert_eq!(match_by_refresh_token(&config, "rt-stranger"), None);
}

#[test]
fn ties_break_on_active_profile() {
    // Two profiles holding the same refresh_token (degenerate; e.g. user
    // duplicated a profile directory). The active one wins.
    let config = config_with(
        vec![
            oauth_profile("first", "rt-shared"),
            oauth_profile("second", "rt-shared"),
        ],
        Some("second"),
    );
    assert_eq!(match_by_refresh_token(&config, "rt-shared"), Some("second"));
}

#[test]
fn endpoint_profiles_without_oauth_are_skipped() {
    let config = config_with(
        vec![endpoint_profile("api"), oauth_profile("work", "rt-work")],
        None,
    );
    assert_eq!(match_by_refresh_token(&config, "rt-work"), Some("work"));
}

#[test]
fn attributes_unmatched_login_to_credential_less_active() {
    let config = config_with(
        vec![oauth_profile("work", "rt-work"), blank_profile("new")],
        Some("new"),
    );
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(resolve_profile(&config, &live, false), Some("new"));
}

#[test]
fn token_match_wins_over_credential_less_active() {
    let config = config_with(
        vec![
            oauth_profile("personal", "rt-personal"),
            blank_profile("new"),
        ],
        Some("new"),
    );
    let live = live_oauth(Some("rt-personal"));
    assert_eq!(resolve_profile(&config, &live, false), Some("personal"));
}

#[test]
fn no_attribution_when_active_profile_has_creds() {
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(resolve_profile(&config, &live, false), None);
}

#[test]
fn no_attribution_when_no_active_profile() {
    let config = config_with(vec![blank_profile("new")], None);
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(resolve_profile(&config, &live, false), None);
}

#[test]
fn no_attribution_without_refresh_token() {
    let config = config_with(vec![blank_profile("new")], Some("new"));
    let live = live_oauth(None);
    assert_eq!(resolve_profile(&config, &live, false), None);
}

#[test]
fn no_credential_less_attribution_inside_session() {
    // When CLAUDE_CONFIG_DIR is set the loaded creds belong to the started
    // profile's runtime, not the global active. Suppress the fallback so a
    // credential-less active profile isn't incorrectly credited.
    let config = config_with(
        vec![oauth_profile("work", "rt-work"), blank_profile("active")],
        Some("active"),
    );
    let live = live_oauth(Some("rt-from-runtime"));
    assert_eq!(resolve_profile(&config, &live, true), None);
}

#[test]
fn token_match_still_works_inside_session() {
    // A token-exact match is always valid, even inside a session.
    let config = config_with(
        vec![oauth_profile("work", "rt-work"), blank_profile("active")],
        Some("active"),
    );
    let live = live_oauth(Some("rt-work"));
    assert_eq!(resolve_profile(&config, &live, true), Some("work"));
}
