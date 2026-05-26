//! Behaviour tests for `next_target` — the fallback-chain target picker.
//!
//! Tests stay hermetic: no filesystem I/O, no `switch_profile`. All scenarios
//! construct an in-memory `AppConfig` and assert on `next_target`'s return value.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::*;
use crate::profile::{AppConfig, AppState, Profile};
use crate::usage::{UsageInfo, UsageStore, UsageWindow};

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

// ── next_auto_switch_target ───────────────────────────────────────────────────
//
// Same scenarios as next_target but routed through the scheduler-side path:
// snapshot the chain out of AppConfig, then read utilization from a UsageStore
// (not Profile.usage). The split exists to avoid the config ↔ store lock
// inversion against App::apply_usage.

fn store_with_utils(pairs: &[(&str, f64)]) -> UsageStore {
    let map: HashMap<String, UsageInfo> = pairs
        .iter()
        .map(|(name, util)| {
            (
                (*name).to_string(),
                UsageInfo {
                    five_hour: Some(UsageWindow {
                        utilization: *util,
                        resets_at: None,
                    }),
                    ..UsageInfo::default()
                },
            )
        })
        .collect();
    Arc::new(Mutex::new(map))
}

#[test]
fn snapshot_chain_captures_thresholds_and_active() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(50.0)),
            profile_with_util("b", Some(100.0), Some(20.0)),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    assert_eq!(snap.active, "a");
    assert_eq!(snap.chain.len(), 2);
    assert_eq!(snap.chain[0].name, "a");
    assert!((snap.chain[0].threshold - 95.0).abs() < f64::EPSILON);
    assert_eq!(snap.chain[1].name, "b");
    assert!((snap.chain[1].threshold - 100.0).abs() < f64::EPSILON);
}

#[test]
fn snapshot_chain_none_when_active_not_in_chain() {
    let mut config = config_with_chain(vec![profile_with_util("a", Some(95.0), Some(50.0))], "a");
    // Active is set but the chain doesn't include it.
    config.state.fallback_chain = vec!["other".into()];
    assert!(snapshot_chain(&config).is_none());
}

#[test]
fn auto_switch_returns_none_when_active_below_threshold() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    // Active at 90% (below 95%) — no switch.
    let store = store_with_utils(&[("a", 90.0), ("b", 10.0)]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

#[test]
fn auto_switch_picks_member_with_headroom() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    // Active maxed, B has headroom.
    let store = store_with_utils(&[("a", 100.0), ("b", 20.0)]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some("b".to_string()),
    );
}

#[test]
fn auto_switch_sink_loop_guard_holds() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(100.0), None),
            profile_with_util("b", Some(100.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    // Both maxed sinks; active is a sink itself → no migration.
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

#[test]
fn auto_switch_non_sink_active_migrates_to_sink_once() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(100.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    // Active threshold 95% (not a sink), B is the sink — one migration.
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some("b".to_string()),
    );
}

#[test]
fn auto_switch_missing_util_is_not_exhausted() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    // Active has no entry in the store → not exhausted → no switch.
    let store = store_with_utils(&[("b", 10.0)]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}
