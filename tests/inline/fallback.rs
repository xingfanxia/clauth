//! Behaviour tests for `next_target` — the fallback-chain target picker.
//!
//! Tests stay hermetic: no filesystem I/O, no `switch_profile`. All scenarios
//! construct an in-memory `AppConfig` and assert on `next_target`'s return value.

use super::*;
use crate::profile::{AppConfig, AppState, Profile};
use crate::usage::{UsageInfo, UsageWindow};

fn profile_with_util(name: &str, threshold: Option<f64>, utilization: Option<f64>) -> Profile {
    use std::collections::BTreeMap;
    Profile {
        name: name.to_string(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        fallback_threshold: threshold,
        credentials: None,
        usage: utilization.map(|u| UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: u,
                resets_at: None,
            }),
            ..UsageInfo::default()
        }),
        fetch_status: None,
    }
}

fn config_with_chain(profiles: Vec<Profile>, active: &str) -> AppConfig {
    let names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            active_profile: Some(active.to_string()),
            profiles: names.clone(),
            fallback_chain: names,
            ..AppState::default()
        },
        profiles,
    }
}

// Scenario 1: all members threshold 100, all at 100% — next_target must return None.
// This was the reported loop: A→B→A→… — fix must break it.
#[test]
fn all_maxed_sinks_no_switch() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(100.0), Some(100.0)),
            profile_with_util("b", Some(100.0), Some(100.0)),
        ],
        "a",
    );
    assert_eq!(next_target(&config), None);
}

// Scenario 2 part A: active threshold 95 at 100% switches to B (threshold 100, at 100%) once.
#[test]
fn non_sink_active_migrates_to_sink_once() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(100.0), Some(100.0)),
        ],
        "a",
    );
    assert_eq!(next_target(&config), Some("b".to_string()));
}

// Scenario 2 part B: with B active (threshold 100, at 100%) — next_target returns None.
#[test]
fn sink_active_maxed_stays_put() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(100.0), Some(100.0)),
        ],
        "b",
    );
    assert_eq!(next_target(&config), None);
}

// Scenario 3: active threshold 100 at 100%, member B threshold 95 at 50% — pass 1 finds B.
#[test]
fn sink_active_switches_to_member_with_headroom() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(100.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(50.0)),
        ],
        "a",
    );
    assert_eq!(next_target(&config), Some("b".to_string()));
}

// Scenario 4: active threshold 95 at 100%, member B threshold 95 at 100% — no sink anywhere.
#[test]
fn no_sink_available_returns_none() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    assert_eq!(next_target(&config), None);
}
