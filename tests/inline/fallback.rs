//! Behaviour tests for `next_target` — the fallback-chain target picker.
//!
//! Tests stay hermetic: no filesystem I/O, no `switch_profile`. All scenarios
//! construct an in-memory `AppConfig` and assert on `next_target`'s return value.

use crate::lockorder::RankedMutex;
use std::collections::HashMap;
use std::sync::Arc;

use super::*;
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::usage::{UsageInfo, UsageStore, UsageWindow};

fn profile_with_util(name: &str, threshold: Option<f64>, utilization: Option<f64>) -> Profile {
    use std::collections::BTreeMap;
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        fallback_threshold: threshold,
        bell_threshold: None,
        credentials: None,
        usage: utilization.map(|u| UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: u,
                resets_at: None,
            }),
            ..UsageInfo::default()
        }),
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn config_with_chain(profiles: Vec<Profile>, active: &str) -> AppConfig {
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            active_profile: Some(active.into()),
            profiles: names.clone(),
            fallback_chain: names,
            ..AppState::default()
        },
        profiles,
    }
}

// All sinks exhausted: A→B→A loop must not form; next_target returns None.
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

// Active threshold 95 at 100%; B is the 100% sink — one migration allowed.
#[test]
fn non_sink_active_migrates_to_sink_once() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(100.0), Some(100.0)),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config),
        Some(SwitchAction::To("b".to_string()))
    );
}

// B active as sink (threshold 100 at 100%) — no further migration.
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

// Active sink (100 at 100%), B has headroom (95 at 50%) — migrates to B.
#[test]
fn sink_active_switches_to_member_with_headroom() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(100.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(50.0)),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config),
        Some(SwitchAction::To("b".to_string()))
    );
}

// No sink anywhere (both threshold 95 at 100%) — returns None.
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
// Same scenarios via the scheduler-side path: snapshot the chain from AppConfig,
// read utilization from UsageStore (not Profile.usage). The split avoids the
// config ↔ store lock inversion against App::apply_usage.

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
    Arc::new(RankedMutex::new(map))
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
    // active is set but absent from the chain
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
    let store = store_with_utils(&[("a", 90.0), ("b", 10.0)]); // active at 90% < 95% → no switch
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
    let store = store_with_utils(&[("a", 100.0), ("b", 20.0)]); // active maxed, B has headroom
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
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
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]); // both maxed sinks → no migration
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
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]); // active threshold 95% (not a sink), B is sink → one migration
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
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
    let store = store_with_utils(&[("b", 10.0)]); // active absent from store → not exhausted → no switch
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// ── wrap-off mode ───────────────────────────────────────────────────────────
//
// When no sink exists and the whole chain is exhausted, wrap-off turns off all
// accounts instead of staying put.

// next_target: wrap_off on, no sink, all exhausted → Off.
#[test]
fn wrap_off_switches_off_when_chain_spent() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    config.state.wrap_off = true;
    assert_eq!(next_target(&config), Some(SwitchAction::Off));
}

// next_target: wrap_off on but 100% sink exists → migrate to sink, not Off.
#[test]
fn wrap_off_prefers_sink_over_off() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(100.0), Some(100.0)),
        ],
        "a",
    );
    config.state.wrap_off = true;
    assert_eq!(
        next_target(&config),
        Some(SwitchAction::To("b".to_string()))
    );
}

// next_target: wrap_off on but active still has headroom → no Off.
#[test]
fn wrap_off_skips_off_when_active_has_headroom() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(50.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    config.state.wrap_off = true;
    // a at 50% < 95% → not exhausted → stay
    assert_eq!(next_target(&config), None);
}

// next_target: same spent chain, wrap_off off → legacy None.
#[test]
fn wrap_off_disabled_stays_put() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    assert_eq!(next_target(&config), None);
}

// next_auto_switch_target: scheduler-side wrap-off → Off when chain spent.
#[test]
fn auto_switch_wrap_off_switches_off_when_chain_spent() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.state.wrap_off = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    assert!(snap.wrap_off);
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]); // both over 95% threshold, no sink → Off
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::Off),
    );
}

// ── recovery_target ──────────────────────────────────────────────────────────
//
// After switch-off-all (no active profile), find a chain member whose
// utilization has dropped below its threshold.

#[test]
fn find_recovered_returns_first_member_below_threshold() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 95.0,
        },
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
        },
    ];
    let store = store_with_utils(&[("a", 100.0), ("b", 40.0)]);
    assert_eq!(
        find_recovered_member(&members, &store),
        Some("b".to_string()),
    );
}

#[test]
fn find_recovered_skips_exhausted_members() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 95.0,
        },
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
        },
    ];
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    assert_eq!(find_recovered_member(&members, &store), None);
}

#[test]
fn find_recovered_returns_none_when_no_member_has_data() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 95.0,
        },
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
        },
    ];
    let store = store_with_utils(&[]); // no usage data for any member
    assert_eq!(find_recovered_member(&members, &store), None);
}

#[test]
fn find_recovered_uses_threshold_per_member() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 90.0,
        }, // 95% util ≥ 90 → exhausted
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
        }, // 94% util < 95 → recovered
    ];
    let store = store_with_utils(&[("a", 95.0), ("b", 94.0)]);
    assert_eq!(
        find_recovered_member(&members, &store),
        Some("b".to_string()),
    );
}

// next_auto_switch_target: wrap_off off, spent chain → legacy None.
#[test]
fn auto_switch_wrap_off_disabled_stays_put() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}
